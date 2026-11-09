/// Public trigger type used by both real and dummy implementations.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub enum Trigger {
    RisingEdge,
    FallingEdge,
    Both,
}

/// Internal pull resistor requested for an input pad.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputBias {
    Off,
    PullDown,
    PullUp,
}

impl InputBias {
    pub fn label(self) -> &'static str {
        match self {
            InputBias::Off => "no pull",
            InputBias::PullDown => "pulldown",
            InputBias::PullUp => "pullup",
        }
    }
}

/// Bias read back from the pad, as opposed to the bias that was requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BiasReadback {
    /// The pad register reports this bias. `source` names where it was read.
    Applied {
        bias: InputBias,
        source: &'static str,
    },
    /// This platform cannot read the pad bias back.
    #[cfg_attr(not(feature = "raspberry_pi"), allow(dead_code))]
    Unverifiable(&'static str),
}

#[cfg(feature = "raspberry_pi")]
const PI_5_UART_GPIO_PINS: &[u8] = &[14, 15];

/// Read-only view of the Raspberry Pi 5 RP1 PADS_BANK0 registers, used only to
/// report the bias the pad really has. All writes still go through rppal.
#[cfg(feature = "raspberry_pi")]
mod rp1_pads {
    use super::InputBias;
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    const PATH: &str = "/dev/gpiomem0";
    // Same window rppal maps: IO_BANK0-2, SYS_RIO0-2, PADS_BANK0-2.
    const MAP_LEN: usize = 0x30000;
    const PADS_BANK0_OFFSET: usize = 0x20000;
    const PADS_GPIO: usize = 0x04;
    const PADS_STRIDE: usize = 0x04;
    const PDE: u32 = 0x04;
    const PUE: u32 = 0x08;
    const BANK0_PINS: u8 = 28;

    pub(super) struct Rp1Pads {
        ptr: *const u32,
    }

    // SAFETY: the mapping is read-only, never unmapped while the process runs
    // (the owner is a process-wide singleton) and only read with volatile loads.
    unsafe impl Send for Rp1Pads {}
    unsafe impl Sync for Rp1Pads {}

    impl Rp1Pads {
        /// `Ok(None)` when this is not an RP1 board.
        pub(super) fn open() -> std::io::Result<Option<Rp1Pads>> {
            if !std::path::Path::new(PATH).exists() {
                return Ok(None);
            }
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_SYNC)
                .open(PATH)?;
            // SAFETY: a fresh shared read-only mapping of the gpiomem window.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    MAP_LEN,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Some(Rp1Pads {
                ptr: ptr as *const u32,
            }))
        }

        pub(super) fn read_bias(&self, pin: u8) -> Result<(InputBias, u32), String> {
            if pin >= BANK0_PINS {
                return Err(format!("GPIO {pin} is outside RP1 bank 0"));
            }
            let offset = (PADS_BANK0_OFFSET + PADS_GPIO + pin as usize * PADS_STRIDE) / 4;
            // SAFETY: offset is inside MAP_LEN for bank-0 pins.
            let raw = unsafe { std::ptr::read_volatile(self.ptr.add(offset)) };
            if raw == u32::MAX {
                return Err(format!(
                    "RP1 PADS_BANK0 GPIO {pin} read 0xFFFFFFFF; RP1 is not answering register reads"
                ));
            }
            match (raw & PUE != 0, raw & PDE != 0) {
                (false, false) => Ok((InputBias::Off, raw)),
                (false, true) => Ok((InputBias::PullDown, raw)),
                (true, false) => Ok((InputBias::PullUp, raw)),
                (true, true) => Err(format!(
                    "RP1 PADS_BANK0 GPIO {pin} = {raw:#04x} has both pullup and pulldown set"
                )),
            }
        }
    }
}

#[cfg(feature = "raspberry_pi")]
fn ensure_regular_gpio_pin_allowed(pin_number: u8) -> Result<(), Box<dyn std::error::Error>> {
    if PI_5_UART_GPIO_PINS.contains(&pin_number) {
        return Err(format!(
            "GPIO pin {pin_number} is reserved for the Raspberry Pi 5 UART and cannot be used as regular GPIO"
        )
        .into());
    }

    Ok(())
}

#[cfg(feature = "raspberry_pi")]
mod real {
    use super::rp1_pads::Rp1Pads;
    use super::{BiasReadback, InputBias, Trigger, ensure_regular_gpio_pin_allowed};
    use rppal::gpio::{Bias, Gpio, InputPin, OutputPin, Trigger as PiTrigger};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    type SharedCallback = Arc<Mutex<Box<dyn Fn(bool) + Send>>>;

    /// What is needed to install an edge callback again after the line is re-opened.
    #[derive(Clone)]
    struct CallbackSpec {
        trigger: Trigger,
        debounce: Duration,
        callback: SharedCallback,
    }

    pub struct GpioPins {
        input_pins: Arc<Mutex<HashMap<u8, InputPin>>>,
        output_pins: Arc<Mutex<HashMap<u8, OutputPin>>>,
        /// Bias explicitly requested per input pin (absent = never requested).
        requested_bias: Mutex<HashMap<u8, InputBias>>,
        callbacks: Mutex<HashMap<u8, CallbackSpec>>,
        pads: OnceLock<Option<Rp1Pads>>,
        gpio: Gpio,
    }

    /// Convert straight to an input with the requested pull, so the pad never
    /// passes through no-pull on the way.
    fn input_with_bias(pin: rppal::gpio::Pin, bias: InputBias) -> InputPin {
        match bias {
            InputBias::Off => pin.into_input(),
            InputBias::PullDown => pin.into_input_pulldown(),
            InputBias::PullUp => pin.into_input_pullup(),
        }
    }

    fn to_pi_bias(bias: InputBias) -> Bias {
        match bias {
            InputBias::Off => Bias::Off,
            InputBias::PullDown => Bias::PullDown,
            InputBias::PullUp => Bias::PullUp,
        }
    }

    #[allow(dead_code)]
    impl GpioPins {
        const LED_PWM_FREQUENCY_HZ: f64 = 120.0;

        /// Global singleton instance.
        pub fn new() -> Arc<GpioPins> {
            static INSTANCE: OnceLock<Arc<GpioPins>> = OnceLock::new();
            INSTANCE
                .get_or_init(|| {
                    Arc::new(GpioPins {
                        input_pins: Arc::new(Mutex::new(HashMap::new())),
                        output_pins: Arc::new(Mutex::new(HashMap::new())),
                        requested_bias: Mutex::new(HashMap::new()),
                        callbacks: Mutex::new(HashMap::new()),
                        pads: OnceLock::new(),
                        gpio: Gpio::new().expect("Failed to initialize GPIO"),
                    })
                })
                .clone()
        }

        pub fn setup_input_pin(&self, pin_number: u8) -> Result<(), Box<dyn std::error::Error>> {
            ensure_regular_gpio_pin_allowed(pin_number)?;
            if self
                .input_pins
                .lock()
                .expect("failed to get lock")
                .contains_key(&pin_number)
            {
                return Ok(());
            }
            let pin = self.gpio.get(pin_number)?.into_input();
            self.input_pins
                .lock()
                .expect("failed to get lock")
                .insert(pin_number, pin);
            Ok(())
        }

        /// Configure an input and write the requested pull explicitly. If the
        /// pin is already configured the bias is written again rather than
        /// assumed.
        pub fn setup_input_with_bias(
            &self,
            pin_number: u8,
            bias: InputBias,
        ) -> Result<(), Box<dyn std::error::Error>> {
            ensure_regular_gpio_pin_allowed(pin_number)?;
            self.requested_bias
                .lock()
                .expect("failed to get lock")
                .insert(pin_number, bias);
            let mut pins = self.input_pins.lock().expect("failed to get lock");
            if let Some(pin) = pins.get_mut(&pin_number) {
                pin.set_bias(to_pi_bias(bias));
                return Ok(());
            }
            let pin = input_with_bias(self.gpio.get(pin_number)?, bias);
            pins.insert(pin_number, pin);
            Ok(())
        }

        /// Write the requested bias to the pad again.
        pub fn reassert_input_bias(
            &self,
            pin_number: u8,
        ) -> Result<InputBias, Box<dyn std::error::Error>> {
            let bias = self.requested_bias_for(pin_number)?;
            let mut pins = self.input_pins.lock().expect("failed to get lock");
            let pin = pins
                .get_mut(&pin_number)
                .ok_or_else(|| format!("Input pin {pin_number} not configured"))?;
            pin.set_bias(to_pi_bias(bias));
            Ok(bias)
        }

        /// Release and request the input again (including its edge callback),
        /// then write the requested bias after the kernel line request.
        pub fn reopen_input(
            &self,
            pin_number: u8,
        ) -> Result<InputBias, Box<dyn std::error::Error>> {
            let bias = self.requested_bias_for(pin_number)?;
            let mut pins = self.input_pins.lock().expect("failed to get lock");
            if let Some(mut old) = pins.remove(&pin_number) {
                // Do not drop the pad to no-pull while it is being re-requested.
                old.set_reset_on_drop(false);
                drop(old);
            }
            let mut pin = input_with_bias(self.gpio.get(pin_number)?, bias);
            let spec = self
                .callbacks
                .lock()
                .expect("failed to get lock")
                .get(&pin_number)
                .cloned();
            if let Some(spec) = spec {
                Self::install_interrupt(&mut pin, &spec)?;
                pin.set_bias(to_pi_bias(bias));
            }
            pins.insert(pin_number, pin);
            Ok(bias)
        }

        /// Bias the pad really has, read from the RP1 pad register.
        pub fn read_input_bias(
            &self,
            pin_number: u8,
        ) -> Result<BiasReadback, Box<dyn std::error::Error>> {
            let pads = self.pads.get_or_init(|| match Rp1Pads::open() {
                Ok(pads) => pads,
                Err(error) => {
                    log::warn!("GPIO pad bias readback disabled: cannot map /dev/gpiomem0 read-only: {error}");
                    None
                }
            });
            match pads {
                Some(pads) => {
                    let (bias, _raw) = pads.read_bias(pin_number)?;
                    Ok(BiasReadback::Applied {
                        bias,
                        source: "RP1 PADS_BANK0 register",
                    })
                }
                None => Ok(BiasReadback::Unverifiable(
                    "pad bias readback needs the Raspberry Pi 5 RP1 /dev/gpiomem0 window",
                )),
            }
        }

        fn requested_bias_for(
            &self,
            pin_number: u8,
        ) -> Result<InputBias, Box<dyn std::error::Error>> {
            self.requested_bias
                .lock()
                .expect("failed to get lock")
                .get(&pin_number)
                .copied()
                .ok_or_else(|| format!("Input pin {pin_number} has no requested bias").into())
        }

        fn install_interrupt(
            pin: &mut InputPin,
            spec: &CallbackSpec,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let callback = spec.callback.clone();
            pin.set_async_interrupt(
                Self::to_pi_trigger(spec.trigger),
                Some(spec.debounce),
                move |event: rppal::gpio::Event| {
                    let level = event.trigger;
                    let callback = callback.lock().unwrap_or_else(|e| e.into_inner());
                    callback(level == rppal::gpio::Trigger::RisingEdge);
                },
            )?;
            Ok(())
        }

        pub fn setup_output_pin(&self, pin_number: u8) -> Result<(), Box<dyn std::error::Error>> {
            ensure_regular_gpio_pin_allowed(pin_number)?;
            let pin = self.gpio.get(pin_number)?.into_output();
            self.output_pins
                .lock()
                .expect("failed to get lock")
                .insert(pin_number, pin);
            Ok(())
        }

        pub fn setup_led_pin(&self, pin_number: u8) -> Result<(), Box<dyn std::error::Error>> {
            self.setup_output_pin(pin_number)
        }

        pub fn read_input_pin(&self, pin_number: u8) -> Result<bool, Box<dyn std::error::Error>> {
            let input_pins = self.input_pins.lock().expect("failed to get lock");
            if let Some(pin) = input_pins.get(&pin_number) {
                Ok(pin.is_high())
            } else {
                Err(format!("Input pin {} not configured", pin_number).into())
            }
        }

        pub fn write_output_pin(
            &self,
            pin_number: u8,
            value: bool,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let mut output_pins = self.output_pins.lock().expect("failed to get lock");
            if let Some(pin) = output_pins.get_mut(&pin_number) {
                pin.clear_pwm()?;
                if value {
                    pin.set_high();
                } else {
                    pin.set_low();
                }
                Ok(())
            } else {
                Err(format!("Output pin {} not configured", pin_number).into())
            }
        }

        pub fn write_led_brightness(
            &self,
            pin_number: u8,
            brightness: f64,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let mut output_pins = self.output_pins.lock().expect("failed to get lock");
            if let Some(pin) = output_pins.get_mut(&pin_number) {
                let duty_cycle = brightness.clamp(0.0, 1.0);
                if duty_cycle <= 0.0 {
                    pin.clear_pwm()?;
                    pin.set_low();
                } else if duty_cycle >= 1.0 {
                    pin.clear_pwm()?;
                    pin.set_high();
                } else {
                    pin.set_pwm_frequency(Self::LED_PWM_FREQUENCY_HZ, duty_cycle)?;
                }
                Ok(())
            } else {
                Err(format!("LED pin {} not configured", pin_number).into())
            }
        }

        pub fn reset_outputs_low(&self) -> Result<(), Box<dyn std::error::Error>> {
            let mut output_pins = self.output_pins.lock().expect("failed to get lock");
            for pin in output_pins.values_mut() {
                pin.clear_pwm()?;
                pin.set_low();
            }
            Ok(())
        }

        fn to_pi_trigger(trigger: Trigger) -> PiTrigger {
            match trigger {
                Trigger::RisingEdge => PiTrigger::RisingEdge,
                Trigger::FallingEdge => PiTrigger::FallingEdge,
                Trigger::Both => PiTrigger::Both,
            }
        }

        pub fn setup_callback_input_pin<F>(
            &self,
            pin_number: u8,
            trigger: Trigger,
            debounce: Duration,
            callback: F,
        ) -> Result<(), Box<dyn std::error::Error>>
        where
            F: Fn(bool) + Send + 'static,
        {
            let mut pins = self
                .input_pins
                .lock()
                .map_err(|_| "failed to lock input_pins")?;

            let pin = pins
                .get_mut(&pin_number)
                .ok_or_else(|| format!("input pin {} not configured", pin_number))?;

            let spec = CallbackSpec {
                trigger,
                debounce,
                callback: Arc::new(Mutex::new(Box::new(callback))),
            };
            Self::install_interrupt(pin, &spec)?;
            // The kernel line request for edge events carries no bias flag, so
            // write the requested pull again after it.
            if let Some(bias) = self
                .requested_bias
                .lock()
                .expect("failed to get lock")
                .get(&pin_number)
                .copied()
            {
                pin.set_bias(to_pi_bias(bias));
            }
            self.callbacks
                .lock()
                .expect("failed to get lock")
                .insert(pin_number, spec);

            Ok(())
        }
    }

    // Re-export so external code can just use `GpioPins` regardless of cfg.
    pub use GpioPins as GpioPinsReal;
}
#[cfg(not(feature = "raspberry_pi"))]
mod dummy {
    use super::{BiasReadback, InputBias, Trigger};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    #[cfg(test)]
    type CallbackMap = Arc<Mutex<HashMap<u8, Arc<Mutex<Box<dyn Fn(bool) + Send>>>>>>;

    /// Simulated pad: requested bias, the bias the "hardware" holds, and
    /// counters that let tests see each explicit bias write and re-request.
    #[derive(Clone, Copy, Debug, Default)]
    struct SimPad {
        requested: Option<InputBias>,
        applied: Option<InputBias>,
        bias_writes: u32,
        reopens: u32,
    }

    #[derive(Clone)]
    pub struct GpioPins {
        input_pins: Arc<Mutex<HashMap<u8, bool>>>,
        output_pins: Arc<Mutex<HashMap<u8, bool>>>,
        pads: Arc<Mutex<HashMap<u8, SimPad>>>,
        #[cfg(test)]
        callbacks: CallbackMap,
    }
    #[allow(dead_code)]
    impl GpioPins {
        /// Global singleton instance (dummy).
        pub fn new() -> Arc<GpioPins> {
            static INSTANCE: OnceLock<Arc<GpioPins>> = OnceLock::new();
            INSTANCE
                .get_or_init(|| {
                    Arc::new(GpioPins {
                        input_pins: Arc::new(Mutex::new(HashMap::new())),
                        output_pins: Arc::new(Mutex::new(HashMap::new())),
                        pads: Arc::new(Mutex::new(HashMap::new())),
                        #[cfg(test)]
                        callbacks: Arc::new(Mutex::new(HashMap::new())),
                    })
                })
                .clone()
        }

        pub fn setup_input_pin(&self, pin_number: u8) -> Result<(), Box<dyn std::error::Error>> {
            self.input_pins
                .lock()
                .expect("failed to get lock")
                .insert(pin_number, false);
            gs_debug_println!(
                "Input pin {} is configured (no gpio is actually attached)",
                pin_number
            );
            Ok(())
        }

        pub fn setup_input_with_bias(
            &self,
            pin_number: u8,
            bias: InputBias,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.input_pins
                .lock()
                .expect("failed to get lock")
                .entry(pin_number)
                .or_insert(false);
            let mut pads = self.pads.lock().expect("failed to get lock");
            let pad = pads.entry(pin_number).or_default();
            pad.requested = Some(bias);
            pad.applied = Some(bias);
            pad.bias_writes += 1;
            Ok(())
        }

        pub fn reassert_input_bias(
            &self,
            pin_number: u8,
        ) -> Result<InputBias, Box<dyn std::error::Error>> {
            let mut pads = self.pads.lock().expect("failed to get lock");
            let pad = pads
                .get_mut(&pin_number)
                .and_then(|pad| pad.requested.map(|bias| (pad, bias)));
            let Some((pad, bias)) = pad else {
                return Err(format!("Input pin {pin_number} has no requested bias").into());
            };
            pad.applied = Some(bias);
            pad.bias_writes += 1;
            Ok(bias)
        }

        pub fn reopen_input(
            &self,
            pin_number: u8,
        ) -> Result<InputBias, Box<dyn std::error::Error>> {
            {
                let mut pads = self.pads.lock().expect("failed to get lock");
                let pad = pads
                    .get_mut(&pin_number)
                    .ok_or_else(|| format!("Input pin {pin_number} has no requested bias"))?;
                pad.reopens += 1;
            }
            self.input_pins
                .lock()
                .expect("failed to get lock")
                .entry(pin_number)
                .or_insert(false);
            self.reassert_input_bias(pin_number)
        }

        pub fn read_input_bias(
            &self,
            pin_number: u8,
        ) -> Result<BiasReadback, Box<dyn std::error::Error>> {
            let pads = self.pads.lock().expect("failed to get lock");
            match pads.get(&pin_number).and_then(|pad| pad.applied) {
                Some(bias) => Ok(BiasReadback::Applied {
                    bias,
                    source: "simulated pad (no GPIO hardware)",
                }),
                None => Err(format!("Input pin {pin_number} not configured").into()),
            }
        }

        /// A private instance so tests do not share pads with the singleton.
        #[cfg(test)]
        pub(crate) fn new_isolated() -> Arc<GpioPins> {
            Arc::new(GpioPins {
                input_pins: Arc::new(Mutex::new(HashMap::new())),
                output_pins: Arc::new(Mutex::new(HashMap::new())),
                pads: Arc::new(Mutex::new(HashMap::new())),
                callbacks: Arc::new(Mutex::new(HashMap::new())),
            })
        }
        /// Simulate the pad losing its configured pull (for example an RP1 reset).
        #[cfg(test)]
        pub(crate) fn set_applied_bias_for_test(&self, pin: u8, bias: InputBias) {
            self.pads.lock().unwrap().entry(pin).or_default().applied = Some(bias);
        }
        #[cfg(test)]
        pub(crate) fn bias_counters_for_test(&self, pin: u8) -> (Option<InputBias>, u32, u32) {
            let pad = self
                .pads
                .lock()
                .unwrap()
                .get(&pin)
                .copied()
                .unwrap_or_default();
            (pad.requested, pad.bias_writes, pad.reopens)
        }

        pub fn setup_output_pin(&self, pin_number: u8) -> Result<(), Box<dyn std::error::Error>> {
            self.output_pins
                .lock()
                .expect("failed to get lock")
                .insert(pin_number, false);
            gs_debug_println!(
                "Output pin {} is configured (no gpio is actually attached)",
                pin_number
            );
            Ok(())
        }

        pub fn setup_led_pin(&self, pin_number: u8) -> Result<(), Box<dyn std::error::Error>> {
            self.setup_output_pin(pin_number)
        }

        pub fn read_input_pin(&self, pin_number: u8) -> Result<bool, Box<dyn std::error::Error>> {
            let input_pins = self.input_pins.lock().expect("failed to get lock");
            if let Some(pin) = input_pins.get(&pin_number) {
                gs_debug_println!("Reading input pin {} (dummy value: {})", pin_number, pin);
                Ok(*pin)
            } else {
                Err(format!("Input pin {} not configured", pin_number).into())
            }
        }

        pub fn write_output_pin(
            &self,
            pin_number: u8,
            value: bool,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let mut output_pins = self.output_pins.lock().expect("failed to get lock");
            if let Some(pin) = output_pins.get_mut(&pin_number) {
                *pin = value;
                Ok(())
            } else {
                Err(format!("Output pin {} not configured", pin_number).into())
            }
        }

        pub fn write_led_brightness(
            &self,
            pin_number: u8,
            brightness: f64,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.write_output_pin(pin_number, brightness > 0.0)
        }

        pub fn reset_outputs_low(&self) -> Result<(), Box<dyn std::error::Error>> {
            let mut output_pins = self.output_pins.lock().expect("failed to get lock");
            for pin in output_pins.values_mut() {
                *pin = false;
            }
            Ok(())
        }

        #[cfg(test)]
        pub(crate) fn trigger_input_for_test(&self, pin: u8, high: bool) {
            self.input_pins.lock().unwrap().insert(pin, high);
            let callback = self
                .callbacks
                .lock()
                .unwrap()
                .get(&pin)
                .cloned()
                .expect("actual callback installed");
            callback.lock().unwrap()(high);
        }
        #[cfg(test)]
        pub(crate) fn set_input_for_test(&self, pin: u8, high: bool) {
            self.input_pins.lock().unwrap().insert(pin, high);
        }

        pub fn setup_callback_input_pin<F>(
            &self,
            _pin_number: u8,
            _trigger: Trigger,
            _debounce: Duration,
            _callback: F,
        ) -> Result<(), Box<dyn std::error::Error>>
        where
            F: Fn(bool) + Send + 'static,
        {
            #[cfg(test)]
            self.callbacks
                .lock()
                .unwrap()
                .insert(_pin_number, Arc::new(Mutex::new(Box::new(_callback))));
            // Production dummy mode continues to have no physical callbacks.
            gs_debug_println!(
                "Setting up callback on pin {} (dummy, no actual effect)",
                _pin_number
            );
            Ok(())
        }
    }
}

#[cfg(feature = "raspberry_pi")]
pub use real::GpioPinsReal as GpioPins;
#[cfg(not(feature = "raspberry_pi"))]
pub type GpioPins = dummy::GpioPins;
