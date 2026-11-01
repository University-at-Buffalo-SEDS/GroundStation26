//! Application-owned actuator settings. These IDs and layouts are not SDK protocol.
pub const REQUEST_BYTES: usize = 32;
pub const STATUS_BYTES: usize = 40;
pub const TIMED_MOVE_MS: u32 = 45_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    Stepper = 0,
    HBridge = 1,
    Auxiliary = 2,
}
impl Mode {
    pub fn decode(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Stepper),
            1 => Some(Self::HBridge),
            2 => Some(Self::Auxiliary),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Stepper => "stepper",
            Self::HBridge => "h_bridge",
            Self::Auxiliary => "auxiliary",
        }
    }
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "stepper" => Some(Self::Stepper),
            "h_bridge" => Some(Self::HBridge),
            "auxiliary" => Some(Self::Auxiliary),
            _ => None,
        }
    }
    pub fn supported(self, capabilities: u8) -> bool {
        self != Self::HBridge && capabilities & (1 << self as u8) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub mode: Option<Mode>, // None is read-only QUERY.
    pub nonce: u64,
    pub generation: u64,
    pub epoch: u32,
    pub issuer: u64,
}
impl Request {
    pub fn encode(self) -> Option<[u8; REQUEST_BYTES]> {
        if self.nonce == 0 || self.issuer == 0 {
            return None;
        }
        if self.mode.is_some()
            && (matches!(self.generation, 0 | u64::MAX) || matches!(self.epoch, 0 | u32::MAX))
        {
            return None;
        }
        let mut bytes = [0; REQUEST_BYTES];
        bytes[0] = 1;
        if let Some(mode) = self.mode {
            bytes[1] = 1;
            bytes[2] = mode as u8;
        }
        bytes[4..12].copy_from_slice(&self.nonce.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.generation.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.epoch.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.issuer.to_le_bytes());
        Some(bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub result: u8,
    pub mode: Option<Mode>,
    pub capabilities: u8,
    pub nonce: u64,
    pub generation: u64,
    pub epoch: u32,
    pub issuer: u64,
    pub active_action: u8,
    pub busy: bool,
    pub remaining_ms: u32,
}
impl Status {
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != STATUS_BYTES
            || bytes[0] != 1
            || bytes[1] > 8
            || (Mode::decode(bytes[2]).is_none() && bytes[2] != 255)
            || bytes[3] & !0x0f != 0
            || bytes[32] > 2
            || bytes[33] > 1
            || bytes[34..36] != [0, 0]
        {
            return None;
        }
        let nonce = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
        let issuer = u64::from_le_bytes(bytes[24..32].try_into().ok()?);
        let remaining_ms = u32::from_le_bytes(bytes[36..40].try_into().ok()?);
        let mode = Mode::decode(bytes[2]);
        if nonce == 0
            || issuer == 0
            || remaining_ms > TIMED_MOVE_MS
            || (bytes[33] == 0 && (bytes[32] != 0 || remaining_ms != 0))
            || (bytes[32] == 0 && remaining_ms != 0)
            || (mode == Some(Mode::Stepper) && remaining_ms != 0)
            || (mode == Some(Mode::Auxiliary) && bytes[32] == 2)
        {
            return None;
        }
        Some(Self {
            result: bytes[1],
            mode,
            capabilities: bytes[3],
            nonce,
            generation: u64::from_le_bytes(bytes[12..20].try_into().ok()?),
            epoch: u32::from_le_bytes(bytes[20..24].try_into().ok()?),
            issuer,
            active_action: bytes[32],
            busy: bytes[33] == 1,
            remaining_ms,
        })
    }
    pub fn result_name(self) -> &'static str {
        [
            "ok",
            "pending",
            "unsupported",
            "invalid",
            "stale",
            "busy",
            "io",
            "conflict",
            "denied",
        ][self.result as usize]
    }
    pub fn ready(self) -> bool {
        self.result == 0
            && self
                .mode
                .is_some_and(|mode| mode.supported(self.capabilities))
            && !matches!(self.generation, 0 | u64::MAX)
            && !matches!(self.epoch, 0 | u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response() -> [u8; STATUS_BYTES] {
        let mut bytes = [0; STATUS_BYTES];
        bytes[0] = 1;
        bytes[2] = 2;
        bytes[3] = 5;
        bytes[4..12].copy_from_slice(&9u64.to_le_bytes());
        bytes[12..20].copy_from_slice(&7u64.to_le_bytes());
        bytes[20..24].copy_from_slice(&3u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&11u64.to_le_bytes());
        bytes
    }
    #[test]
    fn request_uses_exact_board_contract_and_read_only_query() {
        let request = Request {
            mode: Some(Mode::Auxiliary),
            nonce: 9,
            generation: 7,
            epoch: 3,
            issuer: 11,
        };
        let bytes = request.encode().unwrap();
        assert_eq!(&bytes[..4], &[1, 1, 2, 0]);
        assert_eq!(u64::from_le_bytes(bytes[12..20].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(bytes[20..24].try_into().unwrap()), 3);
        assert_eq!(u64::from_le_bytes(bytes[24..32].try_into().unwrap()), 11);
        let query = Request {
            mode: None,
            generation: 0,
            epoch: 0,
            ..request
        }
        .encode()
        .unwrap();
        assert_eq!(&query[..4], &[1, 0, 0, 0]);
        for generation in [0, u64::MAX] {
            assert!(
                Request {
                    generation,
                    ..request
                }
                .encode()
                .is_none()
            );
        }
        for epoch in [0, u32::MAX] {
            assert!(Request { epoch, ..request }.encode().is_none());
        }
    }
    #[test]
    fn current_board_cannot_advertise_bridge_as_available() {
        let status = Status::decode(&response()).unwrap();
        assert!(status.ready());
        assert_eq!(status.result_name(), "ok");
        assert!(Mode::Auxiliary.supported(status.capabilities));
        assert!(!Mode::HBridge.supported(status.capabilities));
        for mode in [Mode::Stepper, Mode::HBridge, Mode::Auxiliary] {
            assert_eq!(Mode::from_name(mode.name()), Some(mode));
        }
    }
    #[test]
    fn reports_distinguish_running_timer_from_completion() {
        let mut bytes = response();
        bytes[32] = 1;
        bytes[33] = 1;
        bytes[36..40].copy_from_slice(&45_000u32.to_le_bytes());
        let status = Status::decode(&bytes).unwrap();
        assert!(status.busy);
        assert_eq!(status.remaining_ms, 45_000);
        bytes[33] = 0;
        assert!(Status::decode(&bytes).is_none());
        bytes[32] = 0;
        bytes[36..40].fill(0);
        assert!(!Status::decode(&bytes).unwrap().busy);
    }
    #[test]
    fn malformed_or_inconsistent_payload_is_not_a_confirmation() {
        let good = response();
        for (at, value) in [
            (0, 0),
            (1, 9),
            (2, 4),
            (3, 128),
            (32, 3),
            (33, 2),
            (34, 1),
            (35, 1),
        ] {
            let mut bytes = good;
            bytes[at] = value;
            assert!(Status::decode(&bytes).is_none(), "offset {at}");
        }
        for at in [4..12, 24..32] {
            let mut bytes = good;
            bytes[at].fill(0);
            assert!(Status::decode(&bytes).is_none());
        }
        assert!(Status::decode(&good[..39]).is_none());
        let mut bytes = good.to_vec();
        bytes.push(0);
        assert!(Status::decode(&bytes).is_none());
        let mut unknown = good;
        unknown[2] = 255;
        assert!(!Status::decode(&unknown).unwrap().ready());
    }
    #[test]
    fn actual_c_strict_idle_auxiliary_goldens_match_exact_application_bytes() {
        fn bytes(value: &str) -> Vec<u8> {
            value
                .as_bytes()
                .chunks_exact(2)
                .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
                .collect()
        }
        let expected = bytes("0101020002000000000000002a00000000000000010000004d00000000000000");
        let request = Request {
            mode: Some(Mode::Auxiliary),
            nonce: 2,
            generation: 42,
            epoch: 1,
            issuer: 77,
        }
        .encode()
        .unwrap();
        assert_eq!(request.as_slice(), expected);
        let reply = bytes(
            "0100020502000000000000002a00000000000000010000004d000000000000000000000000000000",
        );
        let status = Status::decode(&reply).unwrap();
        assert_eq!(
            (
                status.mode,
                status.nonce,
                status.generation,
                status.epoch,
                status.issuer,
                status.capabilities,
                status.busy,
                status.remaining_ms
            ),
            (Some(Mode::Auxiliary), 2, 42, 1, 77, 5, false, 0)
        );
        assert!(
            !Mode::HBridge.supported(15),
            "physical prohibition overrides advertised bits"
        );
    }
}
