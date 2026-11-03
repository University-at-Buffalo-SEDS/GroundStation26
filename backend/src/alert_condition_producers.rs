//! Bind the initial condition producers to existing decoded observations.
//! Threshold values and warning eligibility are supplied by their current owners.
use crate::alert_conditions::{ConditionState, Key};
#[cfg(any(test, not(any(feature = "hitl_mode", feature = "test_fire_mode"))))]
use crate::alert_conditions::BoardKey;
use sedsnet::{config::DataType, packet::Packet};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    pub key: Key,
    pub state: ConditionState,
    pub message: String,
}
/// The caller supplies the already-established liveness/policy decision; this
/// helper neither changes timeouts nor selects an abort or launch policy.
#[cfg(any(test, not(any(feature = "hitl_mode", feature = "test_fire_mode"))))]
pub fn board_liveness(
    board: BoardKey,
    affirmative_online: bool,
    eligible_offline: bool,
) -> Option<Evidence> {
    if affirmative_online {
        Some(Evidence {
            key: Key::BoardOffline(board),
            state: ConditionState::Resolved,
            message: format!("{} communications recovered", board.sender()),
        })
    } else if eligible_offline {
        Some(Evidence {
            key: Key::BoardOffline(board),
            state: ConditionState::Active,
            message: format!("{} communications offline", board.sender()),
        })
    } else {
        None
    }
}
/// A successful Router::tx only proves enqueue admission. It does not prove a
/// physical radio transmission, remote heartbeat receipt, or link recovery.
pub fn heartbeat_enqueue(accepted: bool) -> Evidence {
    Evidence {
        key: Key::StationHeartbeatEnqueue,
        state: if accepted {
            ConditionState::Resolved
        } else {
            ConditionState::Active
        },
        message: if accepted {
            "Station heartbeat enqueue recovered"
        } else {
            "Station heartbeat enqueue failed"
        }
        .into(),
    }
}
/// Gate recovery using the original decoded sender/type/value/source clock.
/// Unknown clocks may raise a low-voltage condition, but cannot prove recovery.
pub fn gateway_voltage(
    packet: &Packet,
    received_network_ms: i64,
    previous: Option<ConditionState>,
    low: f32,
    recover: f32,
) -> Option<Evidence> {
    if packet.data_type() != DataType::named("BATTERY_VOLTAGE")
        || !matches!(packet.sender(), "GB" | "GW")
    {
        return None;
    }
    let values = packet.data_as_f32().ok()?;
    let voltage = *values.first()?;
    if !voltage.is_finite() || !low.is_finite() || !recover.is_finite() || recover <= low {
        return None;
    }
    let source = i64::try_from(packet.timestamp()).ok()?;
    let clock_known = source >= 1_577_836_800_000 && received_network_ms >= 1_577_836_800_000;
    let fresh = clock_known
        && received_network_ms
            .checked_sub(source)
            .is_some_and(|age| (-100..=2000).contains(&age));
    if clock_known && !fresh {
        return None;
    }
    let state = if voltage <= low {
        ConditionState::Active
    } else if !clock_known {
        ConditionState::Unknown
    } else if voltage > recover {
        ConditionState::Resolved
    } else {
        previous.unwrap_or(ConditionState::Resolved)
    };
    Some(Evidence {
        key: Key::GatewayLowVoltage,
        state,
        message: match state {
            ConditionState::Active => "Fill system battery voltage low",
            ConditionState::Resolved => "Fill system battery voltage recovered",
            ConditionState::Unknown => {
                "Fill system battery voltage condition requires fresh source-time evidence"
            }
        }
        .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sedsnet::{config::DataEndpoint, wire_format};
    fn packet(sender: &str, kind: &str, source: u64, value: f32) -> Packet {
        sedsnet::config::register_schema_json_bytes(include_bytes!(
            "../config/telemetry_config.json"
        ))
        .unwrap();
        let original = Packet::new(
            DataType::named(kind),
            &[DataEndpoint::named("GROUND_STATION")],
            sender,
            source,
            std::sync::Arc::from(value.to_le_bytes()),
        )
        .unwrap();
        wire_format::unpack_packet(&wire_format::pack_packet(&original)).unwrap()
    }
    #[test]
    fn real_sdk_crc_decode_and_sender_type_bindings_prevent_foreign_or_stale_clear() {
        let at = 1_793_000_000_000;
        let low = packet("GB", "BATTERY_VOLTAGE", at, 12.9);
        assert_eq!(
            gateway_voltage(&low, at as i64, None, 13., 15.)
                .unwrap()
                .state,
            ConditionState::Active
        );
        for wrong in [
            packet("FC", "BATTERY_VOLTAGE", at, 16.),
            packet("GB", "FUEL_FLOW", at, 16.),
            packet("GB", "BATTERY_VOLTAGE", at - 2001, 16.),
        ] {
            assert!(
                gateway_voltage(&wrong, at as i64, Some(ConditionState::Active), 13., 15.)
                    .is_none()
            );
        }
        let fresh = packet("GW", "BATTERY_VOLTAGE", at, 15.1);
        assert_eq!(
            gateway_voltage(&fresh, at as i64, Some(ConditionState::Active), 13., 15.)
                .unwrap()
                .state,
            ConditionState::Resolved
        );
        let midpoint = packet("GB", "BATTERY_VOLTAGE", at, 14.);
        assert_eq!(
            gateway_voltage(&midpoint, at as i64, Some(ConditionState::Active), 13., 15.)
                .unwrap()
                .state,
            ConditionState::Active
        );
        assert_eq!(
            gateway_voltage(
                &midpoint,
                at as i64,
                Some(ConditionState::Unknown),
                13.,
                15.
            )
            .unwrap()
            .state,
            ConditionState::Unknown
        );
        let unknown = packet("GB", "BATTERY_VOLTAGE", 1000, 16.);
        assert_eq!(
            gateway_voltage(&unknown, at as i64, Some(ConditionState::Active), 13., 15.)
                .unwrap()
                .state,
            ConditionState::Unknown
        );
    }
    #[test]
    fn board_policy_is_supplied_and_heartbeat_recovery_claims_only_enqueue() {
        assert!(board_liveness(BoardKey::Daq, false, false).is_none());
        assert_eq!(
            board_liveness(BoardKey::Daq, false, true).unwrap().state,
            ConditionState::Active
        );
        assert_eq!(
            board_liveness(BoardKey::Daq, true, false).unwrap().state,
            ConditionState::Resolved
        );
        assert_eq!(heartbeat_enqueue(false).state, ConditionState::Active);
        assert_eq!(
            heartbeat_enqueue(true).message,
            "Station heartbeat enqueue recovered"
        );
    }
}
