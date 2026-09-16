//! Bounded application-status evidence for simulator command validation.
//! Transport acceptance and periodic reports of another valve are not ACKs.
pub fn remaining_rounds(samples: &[u32], startup_sample: u32) -> Vec<(usize, u32)> {
    samples
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, sample)| *sample >= startup_sample)
        .collect()
}

pub fn validation_commands(open: bool) -> [(usize, &'static str, &'static str, u8); 2] {
    [
        (0, "VB", "VALVE_COMMAND", if open { 0 } else { 3 }),
        (1, "AB", "ACTUATOR_COMMAND", if open { 10 } else { 13 }),
    ]
}

#[derive(Default)]
pub struct CommandProbe {
    generations: [[u64; 2]; 2],
}

impl CommandProbe {
    pub fn observe(&mut self, sender: &str, payload: &[u8]) {
        let slot = match (sender, payload.first()) {
            ("VB", Some(0)) => 0,
            ("AB", Some(10)) => 1,
            _ => return,
        };
        if payload.len() == 2 && payload[1] <= 1 {
            self.generations[slot][payload[1] as usize] += 1;
        }
    }

    pub fn generation(&self, slot: usize, open: bool) -> u64 {
        self.generations[slot][usize::from(open)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_preserves_global_round_number_and_does_not_replay_commands() {
        let samples = [1, 2, 4, 5, 6, 7, 8, 9, 10, 11];
        assert_eq!(
            remaining_rounds(&samples, 8),
            vec![(6, 8), (7, 9), (8, 10), (9, 11)]
        );
        assert_eq!(remaining_rounds(&samples, 0).len(), 10);
        assert!(remaining_rounds(&samples, 12).is_empty());
    }

    #[test]
    fn validation_commands_use_one_byte_and_alternate_both_boards() {
        use sedsnet::router::LeBytes;
        fn width<T: LeBytes>(_: T) -> usize {
            T::WIDTH
        }
        for open in [false, true, false] {
            let commands = validation_commands(open);
            assert_eq!(commands[0].3, if open { 0 } else { 3 });
            assert_eq!(commands[1].3, if open { 10 } else { 13 });
            for (_, _, _, command) in commands {
                assert_eq!(width(command), 1);
            }
        }
    }

    #[test]
    fn ignores_wrong_sender_state_valve_and_malformed_payloads() {
        let mut p = CommandProbe::default();
        for (sender, bytes) in [
            ("GB", vec![0, 1]),
            ("AB", vec![0, 1]),
            ("VB", vec![2, 1]),
            ("VB", vec![0, 2]),
            ("VB", vec![0, 1, 0]),
        ] {
            p.observe(sender, &bytes);
        }
        assert_eq!(p.generation(0, true), 0);
        p.observe("VB", &[0, 0]);
        assert_eq!(p.generation(0, true), 0);
        p.observe("VB", &[0, 1]);
        let before = p.generation(0, true);
        assert_eq!(before, 1);
        assert!(
            p.generation(0, true) <= before,
            "cached receipt is not a fresh response"
        );
        p.observe("AB", &[10, 1]);
        assert_eq!(p.generation(1, true), 1);
        assert_eq!(p.generation(0, true), before);
    }
}
