//! Actual accepted-start provenance. Separate from the legacy Static40 setting
//! status and from the host T-clock. No packet ACK proves physical position.
use super::wire::{Mode, Request, Status};

pub(super) const TYPE: &str = "AB_FILL_MOTION_STATUS_V2";
pub(super) const BYTES: usize = 56;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MotionStatus {
    pub(super) setting: Status,
    pub(super) lifecycle: u8,
    pub(super) accepted_action: u8,
    pub(super) original_utc_ms: u64,
    pub(super) minimum_delay_remaining_ms: u32,
    pub(super) origin_epoch: u32,
}
impl MotionStatus {
    pub(super) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != BYTES || bytes[0] != 2 || bytes[34] > 4 || bytes[35] > 2 {
            return None;
        }
        let mut legacy = [0; 40];
        legacy.copy_from_slice(&bytes[..40]);
        legacy[0] = 1;
        legacy[34..36].fill(0);
        let setting = Status::decode(&legacy)?;
        let value = Self {
            setting,
            lifecycle: bytes[34],
            accepted_action: bytes[35],
            original_utc_ms: u64::from_le_bytes(bytes[40..48].try_into().ok()?),
            minimum_delay_remaining_ms: u32::from_le_bytes(bytes[48..52].try_into().ok()?),
            origin_epoch: u32::from_le_bytes(bytes[52..56].try_into().ok()?),
        };
        if value.minimum_delay_remaining_ms > 45_000
            || (value.lifecycle == 1
                && (!setting.busy || setting.active_action != value.accepted_action))
            || (value.lifecycle == 2 && (setting.busy || setting.active_action != 0))
        {
            return None;
        }
        Some(value)
    }
    pub(super) fn completed_retract(self) -> bool {
        self.setting.ready()
            && matches!(self.setting.mode, Some(Mode::Stepper | Mode::Auxiliary))
            && self.lifecycle == 2
            && self.accepted_action == 1
            && !self.setting.busy
            && self.setting.active_action == 0
            && self.setting.remaining_ms == 0
            && self.minimum_delay_remaining_ms == 0
            && !matches!(self.original_utc_ms, 0 | u64::MAX)
            && !matches!(self.origin_epoch, 0 | u32::MAX)
            && self.origin_epoch == self.setting.epoch
    }
}
pub(super) fn query(nonce: u64, issuer: u64) -> Option<[u8; 32]> {
    let mut bytes = Request {
        mode: None,
        nonce,
        generation: 0,
        epoch: 0,
        issuer,
    }
    .encode()?;
    bytes[0] = 2;
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn completed() -> Vec<u8> {
        let mut bytes = vec![0; BYTES];
        bytes[0] = 2;
        bytes[2] = 2;
        bytes[3] = 5;
        bytes[4..12].copy_from_slice(&9u64.to_le_bytes());
        bytes[12..20].copy_from_slice(&42u64.to_le_bytes());
        bytes[20..24].copy_from_slice(&8u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&77u64.to_le_bytes());
        bytes[34] = 2;
        bytes[35] = 1;
        bytes[40..48].copy_from_slice(&10_000u64.to_le_bytes());
        bytes[52..56].copy_from_slice(&8u32.to_le_bytes());
        bytes
    }
    #[test]
    fn motion_v2_completed_minimum_delay_requires_exact_separate_type_body_and_identity() {
        let bytes = completed();
        assert!(MotionStatus::decode(&bytes).unwrap().completed_retract());
        for length in [0, 40, 48, 52, 55] {
            assert!(MotionStatus::decode(&bytes[..length]).is_none());
        }
        let mut padded = vec![0; 56];
        padded[..40].copy_from_slice(&bytes[..40]);
        assert!(!MotionStatus::decode(&padded).unwrap().completed_retract());
        for (at, value) in [
            (0, 1),
            (34, 0),
            (34, 1),
            (34, 3),
            (34, 4),
            (35, 0),
            (35, 2),
            (2, 1),
            (2, 255),
            (1, 6),
            (48, 1),
            (52, 9),
        ] {
            let mut bad = bytes.clone();
            bad[at] = value;
            assert!(
                !MotionStatus::decode(&bad).is_some_and(|s| s.completed_retract()),
                "offset{at} value{value}"
            );
        }
        for value in [0, u64::MAX] {
            let mut bad = bytes.clone();
            bad[40..48].copy_from_slice(&value.to_le_bytes());
            assert!(!MotionStatus::decode(&bad).unwrap().completed_retract());
        }
    }
    #[test]
    fn motion_v2_matches_actual_c_codec_golden_and_registered_static56_identity() {
        crate::telemetry_schema::initialize().unwrap();
        assert_eq!(crate::telemetry_schema::data_type(TYPE).0, 199044760);
        let text = "020002051112131415161718a8a7a6a5a4a3a2a1785634128182838485868788000002010000000008070605040302010000000078563412";
        let bytes: Vec<u8> = (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect();
        let status = MotionStatus::decode(&bytes).unwrap();
        assert!(status.completed_retract());
        assert_eq!(status.setting.nonce, 0x1817161514131211);
        assert_eq!(status.setting.generation, 0xa1a2a3a4a5a6a7a8);
        assert_eq!(status.setting.issuer, 0x8887868584838281);
        assert_eq!(status.original_utc_ms, 0x0102030405060708);
        assert_eq!(status.origin_epoch, 0x12345678);
        let schema: serde_json::Value =
            serde_json::from_slice(include_bytes!("../../config/telemetry_config.json")).unwrap();
        let entry = schema["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == TYPE)
            .unwrap();
        assert_eq!(entry["element"]["kind"], "Static");
        assert_eq!(entry["element"]["count"], 56);
        let legacy = schema["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "AB_FILL_DRIVE_STATUS")
            .unwrap();
        assert_eq!(legacy["element"]["count"], 40);
    }
    #[test]
    fn motion_v2_query_is_readonly_and_keeps_legacy_request_encoding() {
        let old = Request {
            mode: None,
            nonce: 9,
            generation: 0,
            epoch: 0,
            issuer: 77,
        }
        .encode()
        .unwrap();
        let new = query(9, 77).unwrap();
        assert_eq!(old[0], 1);
        assert_eq!(new[0], 2);
        assert_eq!(&old[1..], &new[1..]);
        assert_eq!(&new[1..4], &[0, 0, 0]);
        assert!(query(0, 77).is_none());
        assert!(query(9, 0).is_none());
    }
}
