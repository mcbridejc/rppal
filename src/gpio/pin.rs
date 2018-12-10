use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::thread::sleep;

use crate::gpio::{Result, Mode, Level, Trigger, PullUpDown, GPIO_OFFSET_GPLEV, GPIO_OFFSET_GPFSEL, GPIO_OFFSET_GPPUDCLK, GPIO_OFFSET_GPPUD, GPIO_OFFSET_GPCLR, GPIO_OFFSET_GPSET, mem::GpioMem, interrupt::{AsyncInterrupt, EventLoop}};

#[derive(Debug)]
pub struct Pin {
    pin: u8,
    event_loop: Arc<Mutex<EventLoop>>,
    gpio_mem: Arc<GpioMem>,
    gpio_cdev: Arc<Mutex<File>>,
}

impl Pin {
    pub(crate) fn new(pin: u8, event_loop: Arc<Mutex<EventLoop>>, gpio_mem: Arc<GpioMem>, gpio_cdev: Arc<Mutex<File>>) -> Pin {
        Pin { pin, event_loop, gpio_mem, gpio_cdev }
    }

    pub fn as_input(&mut self) -> InputPin {
        InputPin::new(self)
    }

    pub fn as_output(&mut self) -> OutputPin {
        OutputPin::new(self, Mode::Output)
    }

    pub fn as_output_with_mode(&mut self, mode: Mode) -> OutputPin {
        OutputPin::new(self, mode)
    }

    pub(crate) fn set_mode(&mut self, mode: Mode) {
        let reg_addr: usize = GPIO_OFFSET_GPFSEL + (self.pin / 10) as usize;

        let reg_value = (*self.gpio_mem).read(reg_addr);
        (*self.gpio_mem).write(
            reg_addr,
            (reg_value & !(0b111 << ((self.pin % 10) * 3)))
                | ((mode as u32 & 0b111) << ((self.pin % 10) * 3)),
        );

    }

    /// Returns the current GPIO pin mode.
    pub fn mode(&self) -> Mode {
        let reg_addr: usize = GPIO_OFFSET_GPFSEL + (self.pin / 10) as usize;
        let reg_value = (*self.gpio_mem).read(reg_addr);
        let mode_value = ((reg_value >> ((self.pin % 10) * 3)) & 0b111) as u8;

        mode_value.into()
    }

    /// Configures the built-in GPIO pull-up/pull-down resistors.
    pub fn set_pullupdown(&self, pud: PullUpDown) -> Result<()> {
        let gpio_mem = &*self.gpio_mem;

        // Set the control signal in GPPUD, while leaving the other 30
        // bits unchanged.
        let reg_value = gpio_mem.read(GPIO_OFFSET_GPPUD);
        gpio_mem.write(
            GPIO_OFFSET_GPPUD,
            (reg_value & !0b11) | ((pud as u32) & 0b11),
        );

        // Set-up time for the control signal.
        sleep(Duration::new(0, 20000)); // >= 20µs

        // Select the first GPPUDCLK register for the first 32 pins, and
        // the second register for the remaining pins.
        let reg_addr: usize = GPIO_OFFSET_GPPUDCLK + (self.pin / 32) as usize;

        // Clock the control signal into the selected pin.
        gpio_mem.write(reg_addr, 1 << (self.pin % 32));

        // Hold time for the control signal.
        sleep(Duration::new(0, 20000)); // >= 20µs

        // Remove the control signal and clock.
        let reg_value = gpio_mem.read(GPIO_OFFSET_GPPUD);
        gpio_mem.write(GPIO_OFFSET_GPPUD, reg_value & !0b11);
        gpio_mem.write(reg_addr, 0 << (self.pin % 32));

        Ok(())
    }
}

#[derive(Debug)]
pub struct InputPin<'a> {
    pin: &'a mut Pin,
    prev_mode: Option<Mode>,
    async_interrupt: Option<AsyncInterrupt>,
    clear_on_drop: bool,
}

impl<'a> InputPin<'a> {
    pub(crate) fn new(pin: &'a mut Pin) -> InputPin<'a> {
        let prev_mode = pin.mode();

        let prev_mode = if prev_mode == Mode::Input {
            None
        } else {
            pin.set_mode(Mode::Input);
            Some(prev_mode)
        };

        InputPin { pin, prev_mode, async_interrupt: None, clear_on_drop: true }
    }

    /// Returns the value of `clear_on_drop`.
    pub fn clear_on_drop(&self) -> bool {
        self.clear_on_drop
    }

    /// When enabled, resets all pins to their original state when `Gpio` goes out of scope.
    ///
    /// Drop methods aren't called when a program is abnormally terminated,
    /// for instance when a user presses Ctrl-C, and the SIGINT signal isn't
    /// caught. You'll either have to catch those using crates such as
    /// [`simple_signal`], or manually call [`cleanup`].
    ///
    /// By default, `clear_on_drop` is set to `true`.
    ///
    /// [`simple_signal`]: https://crates.io/crates/simple-signal
    /// [`cleanup`]: #method.cleanup
    pub fn set_clear_on_drop(&mut self, clear_on_drop: bool) {
        self.clear_on_drop = clear_on_drop;
    }

    pub fn read(&self) -> Level {
        let reg_addr: usize = GPIO_OFFSET_GPLEV + (self.pin.pin / 32) as usize;
        let reg_value = (*self.pin.gpio_mem).read(reg_addr);

        if (reg_value & (1 << (self.pin.pin % 32))) > 0 {
            Level::High
        } else {
            Level::Low
        }
    }

    /// Configures a synchronous interrupt trigger.
    ///
    /// After configuring a synchronous interrupt trigger, you can use
    /// [`poll_interrupt`] to wait for a trigger event.
    ///
    /// `set_interrupt` will remove any previously configured
    /// (a)synchronous interrupt triggers for the same pin.
    ///
    /// [`poll_interrupt`]: #method.poll_interrupt
    pub fn set_interrupt(&mut self, trigger: Trigger) -> Result<()> {
        self.clear_async_interrupt()?;

        // Each pin can only be configured for a single trigger type
        (*self.pin.event_loop.lock().unwrap()).set_interrupt(self.pin.pin, trigger)
    }

    /// Removes a previously configured synchronous interrupt trigger.
    pub fn clear_interrupt(&mut self) -> Result<()> {
        (*self.pin.event_loop.lock().unwrap()).clear_interrupt(self.pin.pin)
    }

    /// Blocks until an interrupt is triggered on the specified pin, or a timeout occurs.
    ///
    /// `poll_interrupt` only works for pins that have been configured for synchronous interrupts using
    /// [`set_interrupt`]. Asynchronous interrupt triggers are automatically polled on a separate thread.
    ///
    /// Setting `reset` to `false` causes `poll_interrupt` to return immediately if the interrupt
    /// has been triggered since the previous call to [`set_interrupt`] or `poll_interrupt`.
    /// Setting `reset` to `true` clears any cached trigger events for the pin.
    ///
    /// The `timeout` duration indicates how long the call to `poll_interrupt` will block while waiting
    /// for interrupt trigger events, after which an `Ok(None))` is returned.
    /// `timeout` can be set to `None` to wait indefinitely.
    ///
    /// [`set_interrupt`]: #method.set_interrupt
    pub fn poll_interrupt(&mut self, reset: bool, timeout: Option<Duration>) -> Result<Option<Level>> {
        let opt = (*self.pin.event_loop.lock().unwrap()).poll(&[self.pin.pin], reset, timeout)?;

        if let Some(trigger) = opt {
            Ok(Some(trigger.1))
        } else {
            Ok(None)
        }
    }

    /// Configures an asynchronous interrupt trigger, which will execute the callback on a
    /// separate thread when the interrupt is triggered.
    ///
    /// The callback closure or function pointer is called with a single [`Level`] argument.
    ///
    /// `set_async_interrupt` will remove any previously configured
    /// (a)synchronous interrupt triggers for the same pin.
    ///
    /// [`Level`]: enum.Level.html
    pub fn set_async_interrupt<C>(&mut self, trigger: Trigger, callback: C) -> Result<()>
    where
        C: FnMut(Level) + Send + 'static,
    {
        self.clear_interrupt()?;
        self.clear_async_interrupt()?;

        self.async_interrupt = Some(AsyncInterrupt::new(
            (*self.pin.gpio_cdev.lock().unwrap()).as_raw_fd(),
            self.pin.pin,
            trigger,
            callback,
        )?);

        Ok(())
    }

    pub(crate) fn clear_async_interrupt(&mut self) -> Result<()> {
        if let Some(mut interrupt) = self.async_interrupt.take() {
            interrupt.stop()?;
        }

        Ok(())
    }
}

impl<'a> Drop for InputPin<'a> {
    fn drop(&mut self) {
        let _ = self.clear_async_interrupt();

        if self.clear_on_drop == false {
          return
        }

        if let Some(prev_mode) = self.prev_mode {
            self.pin.set_mode(prev_mode)
        }
    }
}

#[derive(Debug)]
pub struct OutputPin<'a> {
    pin: &'a mut Pin,
    mode: Mode,
    prev_mode: Option<Mode>,
    clear_on_drop: bool,
}

impl<'a> OutputPin<'a> {
    pub(crate) fn new(pin: &'a mut Pin, mode: Mode) -> OutputPin<'a> {
        let prev_mode = pin.mode();

        let prev_mode = if prev_mode == mode {
            None
        } else {
            pin.set_mode(mode);
            Some(prev_mode)
        };

        OutputPin { pin, mode, prev_mode, clear_on_drop: true }
    }

    /// Returns the value of `clear_on_drop`.
    pub fn clear_on_drop(&self) -> bool {
        self.clear_on_drop
    }

    /// When enabled, resets all pins to their original state when `Gpio` goes out of scope.
    ///
    /// Drop methods aren't called when a program is abnormally terminated,
    /// for instance when a user presses Ctrl-C, and the SIGINT signal isn't
    /// caught. You'll either have to catch those using crates such as
    /// [`simple_signal`], or manually call [`cleanup`].
    ///
    /// By default, `clear_on_drop` is set to `true`.
    ///
    /// [`simple_signal`]: https://crates.io/crates/simple-signal
    /// [`cleanup`]: #method.cleanup
    pub fn set_clear_on_drop(&mut self, clear_on_drop: bool) {
        self.clear_on_drop = clear_on_drop;
    }

    pub fn set_low(&mut self) {
        self.write(Level::Low)
    }

    pub fn set_high(&mut self) {
        self.write(Level::High)
    }

    pub fn write(&mut self, level: Level) {
        let reg_addr: usize = match level {
            Level::Low => GPIO_OFFSET_GPCLR + (self.pin.pin / 32) as usize,
            Level::High => GPIO_OFFSET_GPSET + (self.pin.pin / 32) as usize,
        };

        (*self.pin.gpio_mem).write(reg_addr, 1 << (self.pin.pin % 32));
    }
}

impl<'a> Drop for OutputPin<'a> {
  fn drop(&mut self) {
    if self.clear_on_drop == false {
      return
    }

    if let Some(prev_mode) = self.prev_mode {
      self.pin.set_mode(prev_mode)
    }
  }
}
