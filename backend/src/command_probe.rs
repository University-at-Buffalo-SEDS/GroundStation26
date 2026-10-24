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

/// Only simulator startup validation uses this barrier. Production commands do
/// not acquire it or wait for a simulator task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupState {
    Pending,
    Passed,
    Failed,
}
pub(crate) struct StartupAck(std::sync::atomic::AtomicU8);
impl StartupAck {
    pub(crate) fn new() -> Self {
        Self(std::sync::atomic::AtomicU8::new(0))
    }
    pub(crate) fn state(&self) -> StartupState {
        match self.0.load(std::sync::atomic::Ordering::Acquire) {
            0 => StartupState::Pending,
            1 => StartupState::Passed,
            _ => StartupState::Failed,
        }
    }
    pub(crate) fn fail(&self) {
        let _ = self.0.compare_exchange(
            0,
            2,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }
    pub(crate) fn begin(self: &std::sync::Arc<Self>) -> StartupRun {
        StartupRun {
            barrier: self.clone(),
            finished: false,
        }
    }
}
pub(crate) fn startup_state(barrier: Option<&StartupAck>) -> StartupState {
    barrier.map_or(StartupState::Passed, StartupAck::state)
}
pub(crate) struct StartupRun {
    barrier: std::sync::Arc<StartupAck>,
    finished: bool,
}
impl StartupRun {
    /// Call only after the existing actual boot ACK and latency checks succeed.
    pub(crate) fn pass(&mut self) -> bool {
        self.finished = true;
        self.barrier
            .0
            .compare_exchange(
                0,
                1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
}
impl Drop for StartupRun {
    fn drop(&mut self) {
        if !self.finished {
            self.barrier.fail();
        }
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
    #[test]
    fn startup_ack_prevents_conflicting_soak_command_and_preserves_captured_rounds() {
        let samples = [1, 2, 4, 5, 6, 7, 8, 9, 10, 11];
        let startup_sample = 8;
        let barrier = std::sync::Arc::new(StartupAck::new());
        let mut run = barrier.begin();
        let mut probe = CommandProbe::default();
        let before = probe.generation(0, true);
        // Discovery and queuing the boot Open are not an acknowledgement. The
        // soak Close must remain undispatched until the actual startup ACK.
        assert_eq!(startup_state(Some(&barrier)), StartupState::Pending);
        assert_eq!(probe.generation(0, true), before);
        probe.observe("VB", &[0, 1]);
        assert!(probe.generation(0, true) > before);
        assert!(run.pass());
        assert_eq!(startup_state(Some(&barrier)), StartupState::Passed);
        // Waiting may cross a sample boundary; retain the originally captured
        // schedule, including round7/sample8 rather than silently skipping it.
        assert_eq!(
            remaining_rounds(&samples, startup_sample),
            vec![(6, 8), (7, 9), (8, 10), (9, 11)]
        );
        assert_eq!(validation_commands(false)[0].3, 3);
    }
    #[test]
    fn startup_failure_cancellation_and_late_success_never_release_soak() {
        let barrier = std::sync::Arc::new(StartupAck::new());
        {
            let _run = barrier.begin();
        }
        assert_eq!(startup_state(Some(&barrier)), StartupState::Failed);
        let mut late = barrier.begin();
        assert!(!late.pass());
        assert_eq!(barrier.state(), StartupState::Failed);
        let barrier = std::sync::Arc::new(StartupAck::new());
        let mut run = barrier.begin();
        barrier.fail();
        assert!(!run.pass());
        assert_eq!(barrier.state(), StartupState::Failed);
        assert_eq!(startup_state(None), StartupState::Passed);
    }
}
