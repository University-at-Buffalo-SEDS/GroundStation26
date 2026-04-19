/// Public trigger type used by both real and dummy implementations.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub enum Trigger {
    RisingEdge,
    FallingEdge,
    Both,
}

#[cfg(feature = "raspberry_pi")]
mod real {
    use super::Trigger;
    use rppal::gpio::{Gpio, InputPin, OutputPin, Trigger as PiTrigger};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    pub struct GpioPins {
        input_pins: Arc<Mutex<HashMap<u8, InputPin>>>,
        output_pins: Arc<Mutex<HashMap<u8, OutputPin>>>,
        gpio: Gpio,
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
                        gpio: Gpio::new().expect("Failed to initialize GPIO"),
                    })
                })
                .clone()
        }

        pub fn setup_input_pin(&self, pin_number: u8) -> Result<(), Box<dyn std::error::Error>> {
            let pin = self.gpio.get(pin_number)?.into_input();
            self.input_pins
                .lock()
                .expect("failed to get lock")
                .insert(pin_number, pin);
            Ok(())
        }

        pub fn setup_output_pin(&self, pin_number: u8) -> Result<(), Box<dyn std::error::Error>> {
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

            let pi_trigger = Self::to_pi_trigger(trigger);

            pin.set_async_interrupt(
                pi_trigger,
                Some(debounce),
                move |event: rppal::gpio::Event| {
                    let level = event.trigger;
                    callback(level == rppal::gpio::Trigger::RisingEdge);
                },
            )?;

            Ok(())
        }
    }

    // Re-export so external code can just use `GpioPins` regardless of cfg.
    pub use GpioPins as GpioPinsReal;
}
#[cfg(not(feature = "raspberry_pi"))]
mod dummy {
    use super::Trigger;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    #[derive(Clone)]
    pub struct GpioPins {
        input_pins: Arc<Mutex<HashMap<u8, bool>>>,
        output_pins: Arc<Mutex<HashMap<u8, bool>>>,
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
            // No-op in dummy implementation
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
