//! Proposed pure software tank plant; independent of GSE controller targets.
//! Defaults are provisional training parameters, not measured hardware fits.
//! Nothing here admits a command, acknowledges it, changes a flight state,
//! grants qualification or opens a physical transport.
#[derive(Clone, Copy, Debug)]
pub(super) struct Parameters {
    pub nitrous_supply_psi: f64,
    pub nitrogen_supply_psi: f64,
    pub tank_volume_m3: f64,
    pub liquid_density_kg_m3: f64,
    pub max_liquid_kg: f64,
    pub liquid_fill_kg_s: f64,
    pub nitrogen_rise_psi_s: f64,
    pub vapor_response_psi_s: f64,
    pub dump_liquid_kg_s: f64,
    pub pilot_liquid_kg_s: f64,
    pub dump_pressure_psi_s: f64,
    pub vent_pressure_psi_s: f64,
    pub sealed_leak_psi_s: f64,
}
impl Default for Parameters {
    fn default() -> Self {
        Self {
            nitrous_supply_psi: 745.0,
            nitrogen_supply_psi: 1000.0,
            tank_volume_m3: 0.018,
            liquid_density_kg_m3: 750.0,
            max_liquid_kg: 12.0,
            liquid_fill_kg_s: 8.0 / 18.0,
            nitrogen_rise_psi_s: 20.0,
            vapor_response_psi_s: 26.0,
            dump_liquid_kg_s: 0.55,
            pilot_liquid_kg_s: 1.0,
            dump_pressure_psi_s: 80.0,
            vent_pressure_psi_s: 0.25,
            sealed_leak_psi_s: 0.0,
        }
    }
}
impl Parameters {
    pub fn validate(&self) -> bool {
        let positive = [
            self.nitrous_supply_psi,
            self.nitrogen_supply_psi,
            self.tank_volume_m3,
            self.liquid_density_kg_m3,
            self.max_liquid_kg,
            self.liquid_fill_kg_s,
            self.nitrogen_rise_psi_s,
            self.vapor_response_psi_s,
            self.dump_liquid_kg_s,
            self.pilot_liquid_kg_s,
            self.dump_pressure_psi_s,
            self.vent_pressure_psi_s,
        ];
        positive.iter().all(|v| v.is_finite() && *v > 0.0)
            && self.sealed_leak_psi_s.is_finite()
            && self.sealed_leak_psi_s >= 0.0
            && self.max_liquid_kg / self.liquid_density_kg_m3 < self.tank_volume_m3 * 0.95
    }
}
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Valves {
    pub nitrogen: bool,
    pub nitrous: bool,
    pub vent: bool,
    pub dump: bool,
    pub pilot: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StepError {
    InvalidParameters,
    ClockBackwards,
    ClockGap,
}
#[derive(Clone, Debug)]
pub(super) struct Plant {
    pub liquid_kg: f64,
    pub nitrogen_partial_psi: f64,
    pub vapor_partial_psi: f64,
    last_ms: Option<u64>,
}
impl Plant {
    pub fn empty() -> Self {
        Self {
            liquid_kg: 0.0,
            nitrogen_partial_psi: 0.0,
            vapor_partial_psi: 0.0,
            last_ms: None,
        }
    }
    pub fn pressure_psi(&self) -> f64 {
        self.nitrogen_partial_psi + self.vapor_partial_psi
    }
    pub fn mass_kg(&self, p: Parameters) -> f64 {
        let headspace = (p.tank_volume_m3 - self.liquid_kg / p.liquid_density_kg_m3)
            .max(p.tank_volume_m3 * 0.05);
        // Ideal-gas N2 inventory at24C; liquid and gas are never overwritten
        // by the user's fill/nitrogen target. Geometry remains provisional.
        self.liquid_kg + self.nitrogen_partial_psi * 6894.757293 * headspace / (296.8 * 297.15)
    }
    pub fn step(&mut self, now: u64, v: Valves, p: Parameters) -> Result<(), StepError> {
        if !p.validate() {
            return Err(StepError::InvalidParameters);
        }
        let Some(before) = self.last_ms else {
            self.last_ms = Some(now);
            return Ok(());
        };
        if now < before {
            return Err(StepError::ClockBackwards);
        }
        let mut remaining = now - before;
        if remaining > 10_000 {
            // Do not invent flow during an unobserved interval. Reject this
            // observation and anchor a new integration epoch; the next normal
            // interval may resume from the last known inventory. Caller exposes
            // this discontinuity, never a fresh sample for the missing interval.
            self.last_ms = Some(now);
            return Err(StepError::ClockGap);
        }
        // Equal-clock emissions integrate ZERO time. Different sensor counts
        // or UI subscribers therefore cannot change the trajectory.
        while remaining > 0 {
            let elapsed = remaining.min(20);
            self.integrate(elapsed as f64 / 1000.0, v, p);
            remaining -= elapsed;
        }
        self.last_ms = Some(now);
        Ok(())
    }
    fn integrate(&mut self, dt: f64, v: Valves, p: Parameters) {
        let old_headspace = (p.tank_volume_m3 - self.liquid_kg / p.liquid_density_kg_m3)
            .max(p.tank_volume_m3 * 0.05);
        if v.nitrous && !v.dump && self.pressure_psi() < p.nitrous_supply_psi {
            self.liquid_kg = (self.liquid_kg + p.liquid_fill_kg_s * dt).min(p.max_liquid_kg);
        }
        let loss = if v.dump { p.dump_liquid_kg_s } else { 0.0 }
            + if v.pilot { p.pilot_liquid_kg_s } else { 0.0 };
        self.liquid_kg = (self.liquid_kg - loss * dt).max(0.0);
        let headspace = (p.tank_volume_m3 - self.liquid_kg / p.liquid_density_kg_m3)
            .max(p.tank_volume_m3 * 0.05);
        self.nitrogen_partial_psi *= old_headspace / headspace;
        let vapor_target = p.nitrous_supply_psi * (self.liquid_kg / 0.5).clamp(0.0, 1.0);
        self.vapor_partial_psi += (vapor_target - self.vapor_partial_psi)
            .clamp(-p.vapor_response_psi_s * dt, p.vapor_response_psi_s * dt);
        if v.nitrogen && !v.dump {
            let room = (p.nitrogen_supply_psi - self.pressure_psi()).max(0.0);
            self.nitrogen_partial_psi += (p.nitrogen_rise_psi_s * dt).min(room);
        }
        let bleed = (if v.dump {
            p.dump_pressure_psi_s
        } else if v.vent {
            p.vent_pressure_psi_s
        } else {
            p.sealed_leak_psi_s
        }) * dt;
        let pressure = self.pressure_psi();
        if pressure > 0.0 {
            let retained = (pressure - bleed).max(0.0) / pressure;
            self.nitrogen_partial_psi *= retained;
            self.vapor_partial_psi *= retained;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nitrogen_adds_inventory_without_erasing_existing_liquid() {
        let p = Parameters::default();
        let mut plant = Plant::empty();
        plant.liquid_kg = 8.0;
        plant.vapor_partial_psi = 745.0;
        plant.step(1000, Valves::default(), p).unwrap();
        let before = plant.mass_kg(p);
        plant
            .step(
                2000,
                Valves {
                    nitrogen: true,
                    ..Default::default()
                },
                p,
            )
            .unwrap();
        assert_eq!(plant.liquid_kg, 8.0);
        assert!(plant.mass_kg(p) > before);
        assert!(plant.pressure_psi() > 745.0);
    }
    #[test]
    fn repeated_equal_clock_is_physically_inert() {
        let p = Parameters::default();
        let mut a = Plant::empty();
        let v = Valves {
            nitrogen: true,
            nitrous: true,
            ..Default::default()
        };
        a.step(1000, v, p).unwrap();
        let before = (a.mass_kg(p), a.pressure_psi());
        for _ in 0..100 {
            a.step(1000, v, p).unwrap();
        }
        assert_eq!((a.mass_kg(p), a.pressure_psi()), before);
    }
    #[test]
    fn frozen_closed_valve_cannot_fill_and_dump_lowers_pressure() {
        let p = Parameters::default();
        let mut a = Plant::empty();
        a.step(1000, Valves::default(), p).unwrap();
        a.step(2000, Valves::default(), p).unwrap();
        assert_eq!((a.mass_kg(p), a.pressure_psi()), (0.0, 0.0));
        a.nitrogen_partial_psi = 500.0;
        let before = a.pressure_psi();
        a.step(
            3000,
            Valves {
                dump: true,
                ..Default::default()
            },
            p,
        )
        .unwrap();
        assert!(a.pressure_psi() < before);
    }
    #[test]
    fn actual_vent_only_releases_nitrogen_inventory() {
        let p = Parameters::default();
        let mut a = Plant::empty();
        a.nitrogen_partial_psi = 500.0;
        a.step(1000, Valves::default(), p).unwrap();
        let before = a.mass_kg(p);
        a.step(
            2000,
            Valves {
                vent: true,
                ..Default::default()
            },
            p,
        )
        .unwrap();
        assert!(a.pressure_psi() < 500.0);
        assert!(a.mass_kg(p) < before);
        assert_eq!(a.liquid_kg, 0.0);
    }
    #[test]
    fn above_nitrous_supply_a_stuck_open_inlet_cannot_add_liquid() {
        let p = Parameters::default();
        let mut a = Plant::empty();
        a.liquid_kg = 8.0;
        a.nitrogen_partial_psi = 900.0;
        a.step(1000, Valves::default(), p).unwrap();
        a.step(
            2000,
            Valves {
                nitrous: true,
                ..Default::default()
            },
            p,
        )
        .unwrap();
        assert_eq!(a.liquid_kg, 8.0);
        assert!(a.pressure_psi() > p.nitrous_supply_psi);
    }
    #[test]
    fn finite_supply_cannot_satisfy_an_unreachable_pressure_goal() {
        let p = Parameters {
            nitrogen_supply_psi: 100.0,
            ..Parameters::default()
        };
        let mut a = Plant::empty();
        let v = Valves {
            nitrogen: true,
            ..Default::default()
        };
        for now in (1000..=21000).step_by(1000) {
            a.step(now, v, p).unwrap();
        }
        assert!(a.pressure_psi() <= 100.0);
        assert!(
            a.pressure_psi() < 500.0,
            "a controller goal is not a plant override"
        );
    }
    #[test]
    fn backwards_or_excessive_clock_gap_cannot_synthesize_data() {
        let p = Parameters::default();
        let mut a = Plant::empty();
        a.step(1000, Valves::default(), p).unwrap();
        assert_eq!(
            a.step(999, Valves::default(), p),
            Err(StepError::ClockBackwards)
        );
        assert_eq!(
            a.step(12000, Valves::default(), p),
            Err(StepError::ClockGap)
        );
        assert_eq!((a.liquid_kg, a.pressure_psi()), (0.0, 0.0));
        a.step(13000, Valves::default(), p).unwrap();
        assert_eq!((a.liquid_kg, a.pressure_psi()), (0.0, 0.0));
    }
}
