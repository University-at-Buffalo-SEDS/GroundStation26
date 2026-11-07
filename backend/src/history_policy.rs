//! Approved policy contract only; no default or runtime activation here.
pub const PROSPECTIVE_ALL_BOARDS: u8 = 0x7f;
// Runtime ordering: RF, FC, PB, Actuator, Valve, Gateway, DAQ.
pub const TEST_FIRE_FILL_BOARDS: u8 = 0x78;

#[derive(Clone, Copy)]
pub enum LaunchScope {
    Flight,
    TestFire,
}

/// Missing/uncertain history policy is distinct from observed actual Abort.
/// The HITL flag is a dedicated board-proof policy, never a physical-pin toggle.
pub fn uncertain_launch_blockers(
    unknown: u8,
    scope: LaunchScope,
    hitl: bool,
    hitl_board_proof_enforced: bool,
) -> u8 {
    if hitl && !hitl_board_proof_enforced {
        return 0;
    }
    unknown
        & match scope {
            LaunchScope::Flight => PROSPECTIVE_ALL_BOARDS,
            LaunchScope::TestFire => TEST_FIRE_FILL_BOARDS,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_mode_combinations_preserve_exact_approved_scope() {
        for unknown in 0..=PROSPECTIVE_ALL_BOARDS {
            for hitl in [false, true] {
                for enforce in [false, true] {
                    let optional = hitl && !enforce;
                    assert_eq!(
                        uncertain_launch_blockers(unknown, LaunchScope::Flight, hitl, enforce),
                        if optional { 0 } else { unknown }
                    );
                    assert_eq!(
                        uncertain_launch_blockers(unknown, LaunchScope::TestFire, hitl, enforce),
                        if optional {
                            0
                        } else {
                            unknown & TEST_FIRE_FILL_BOARDS
                        }
                    );
                }
            }
        }
    }
    #[test]
    fn test_fire_keeps_daq_and_excludes_av_without_link_group_assumptions() {
        assert_eq!(
            uncertain_launch_blockers(0x7f, LaunchScope::TestFire, false, false),
            0x78
        );
        assert_eq!(
            uncertain_launch_blockers(0x07, LaunchScope::TestFire, true, true),
            0
        );
        assert_eq!(
            uncertain_launch_blockers(0x40, LaunchScope::TestFire, true, true),
            0x40
        );
    }
}
