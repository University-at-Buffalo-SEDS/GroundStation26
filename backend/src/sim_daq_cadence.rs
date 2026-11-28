//! Optional telemetry-return validation for the two DAQ load-cell wire formats.
//! The caller requires explicit GS_SIM_VALIDATE_TELEMETRY_RETURN; this module
//! does not select firmware_sim or change any operational clock policy.
//! Acquisition, publication, derived rows and cadence thresholds stay unchanged.
use crate::{telemetry_schema, types::Board};
use sedsnet::{config::DataType, packet::Packet};

fn loadcell_value(ty: DataType, payload: &[u8]) -> Option<f32> {
    let length = if ty == telemetry_schema::data_type("KG1000") {
        4
    } else if ty == telemetry_schema::data_type("DAQ_LOADCELL_CHAMBER") {
        12
    } else {
        return None;
    };
    if payload.len() != length {
        return None;
    }
    // Production chamber_pressure_rows preserves a finite load-cell value even
    // when the independent pressure/voltage channels are unavailable.
    let value = f32::from_le_bytes(payload[..4].try_into().ok()?);
    value.is_finite().then_some(value)
}

pub(super) fn captured_loadcell(board: Option<Board>, packet: &Packet) -> Option<(f32, u64)> {
    if board != Some(Board::DaqBoard) {
        return None;
    }
    loadcell_value(packet.data_type(), packet.payload()).map(|value| (value, packet.timestamp()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DaqLoadcellCadence, StreamRateProbe};
    use std::sync::Arc;

    fn packet(name: &str, values: &[f32], timestamp: u64) -> Packet {
        let payload: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        Packet::new(
            telemetry_schema::data_type(name),
            &[telemetry_schema::endpoint("GROUND_STATION")],
            "DAQ",
            timestamp,
            Arc::from(payload),
        )
        .unwrap()
    }
    fn observe(probe: &mut StreamRateProbe, board: Option<Board>, packet: &Packet) -> bool {
        let cadence = DaqLoadcellCadence::parse(Some("40")).unwrap();
        assert_eq!(
            (cadence.minimum_interval_ms, cadence.maximum_interval_ms),
            (12, 37)
        );
        captured_loadcell(board, packet).is_some_and(|(_, timestamp)| {
            probe.observe(
                timestamp,
                cadence.minimum_interval_ms,
                cadence.maximum_interval_ms,
                5,
            )
        })
    }

    #[test]
    fn actual_scalar_and_combined_packets_keep_loadcell_source_clock_and_identity() {
        for (name, values) in [
            ("KG1000", vec![-0.125]),
            ("DAQ_LOADCELL_CHAMBER", vec![-0.125, 1000.0, 5.0]),
        ] {
            let p = packet(name, &values, 987_654);
            let id = p.packet_id();
            let bytes = p.payload().to_vec();
            assert_eq!(
                captured_loadcell(Some(Board::DaqBoard), &p),
                Some((-0.125, 987_654))
            );
            assert_eq!(p.packet_id(), id);
            assert_eq!(p.payload(), bytes);
            assert_eq!(captured_loadcell(Some(Board::PowerBoard), &p), None);
            assert_eq!(captured_loadcell(None, &p), None);
        }
    }

    #[test]
    fn malformed_lengths_unrelated_types_and_nonfinite_loadcells_are_not_samples() {
        let scalar = telemetry_schema::data_type("KG1000");
        let combined = telemetry_schema::data_type("DAQ_LOADCELL_CHAMBER");
        for length in [0, 1, 3, 5, 8, 12, 16] {
            assert_eq!(loadcell_value(scalar, &vec![0; length]), None);
        }
        for length in [0, 1, 4, 8, 11, 13, 16] {
            assert_eq!(loadcell_value(combined, &vec![0; length]), None);
        }
        assert_eq!(
            loadcell_value(telemetry_schema::data_type("KG50"), &[0; 4]),
            None
        );
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(loadcell_value(scalar, &value.to_le_bytes()), None);
            let bytes: Vec<u8> = [value, 0.0, 0.0]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect();
            assert_eq!(loadcell_value(combined, &bytes), None);
        }
    }

    #[test]
    fn invalid_pressure_does_not_discard_valid_loadcell_matching_production_rows() {
        let p = packet("DAQ_LOADCELL_CHAMBER", &[4.25, f32::NAN, f32::INFINITY], 25);
        assert_eq!(
            captured_loadcell(Some(Board::DaqBoard), &p),
            Some((4.25, 25))
        );
    }

    #[test]
    fn six_mixed_wire_samples_satisfy_original_five_interval_40_hz_gate_once() {
        let mut probe = StreamRateProbe::default();
        let mut ids = std::collections::HashSet::new();
        for index in 0..6 {
            let p = if index % 2 == 0 {
                packet("KG1000", &[index as f32], index * 25)
            } else {
                packet(
                    "DAQ_LOADCELL_CHAMBER",
                    &[index as f32, 500.0, 2.5],
                    index * 25,
                )
            };
            assert!(ids.insert(p.packet_id()));
            assert_eq!(observe(&mut probe, Some(Board::DaqBoard), &p), index == 5);
            assert_eq!(probe.count, index as usize + 1);
        }
        // Switching wire format at the same source clock is not a new sample.
        let duplicate = packet("DAQ_LOADCELL_CHAMBER", &[5.0, 0.0, 0.0], 125);
        assert!(!observe(&mut probe, Some(Board::DaqBoard), &duplicate));
        assert_eq!(probe.count, 6);
    }

    #[test]
    fn nonfinite_wrong_source_and_malformed_samples_cannot_complete_gate() {
        let mut probe = StreamRateProbe::default();
        for index in 0..5 {
            assert!(!observe(
                &mut probe,
                Some(Board::DaqBoard),
                &packet("KG1000", &[1.0], index * 25)
            ));
        }
        assert_eq!(probe.count, 5);
        let bad = packet("DAQ_LOADCELL_CHAMBER", &[f32::NAN, 1.0, 1.0], 125);
        assert!(!observe(&mut probe, Some(Board::DaqBoard), &bad));
        assert!(!observe(
            &mut probe,
            Some(Board::PowerBoard),
            &packet("KG1000", &[1.0], 125)
        ));
        assert_eq!(
            loadcell_value(telemetry_schema::data_type("DAQ_LOADCELL_CHAMBER"), &[0; 4]),
            None
        );
        assert_eq!(probe.count, 5);
        assert!(observe(
            &mut probe,
            Some(Board::DaqBoard),
            &packet("DAQ_LOADCELL_CHAMBER", &[1.0, 0.0, 0.0], 125)
        ));
    }

    #[test]
    fn original_12_to_37_ms_limits_and_required_five_intervals_are_unchanged() {
        for period in [11, 12, 25, 37, 38] {
            let mut probe = StreamRateProbe::default();
            for index in 0..6 {
                let p = packet("DAQ_LOADCELL_CHAMBER", &[1.0, 0.0, 0.0], index * period);
                assert_eq!(
                    observe(&mut probe, Some(Board::DaqBoard), &p),
                    index == 5 && (12..=37).contains(&period)
                );
            }
        }
    }
}
