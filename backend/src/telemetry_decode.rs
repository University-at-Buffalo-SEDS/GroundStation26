use sedsprintf_rs_2026::config::{get_message_data_type};
use sedsprintf_rs_2026::{get_needed_message_size, MessageDataType};
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;

pub fn decode_f32_values(pkt: &TelemetryPacket) -> Option<Vec<f32>> {
    if get_message_data_type(pkt.data_type()) != MessageDataType::Float32 {
        return None;
    }

    let count = get_needed_message_size(pkt.data_type()) / size_of::<f32>();
    if count == 0 {
        return None;
    }
    

    let bytes = pkt.payload();
    if bytes.len() != count * 4 {
        // Size mismatch – schema vs payload. Bail out.
        return None;
    }

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let offset = i * 4;
        let chunk: [u8; 4] = bytes[offset..offset + 4].try_into().ok()?;
        out.push(f32::from_le_bytes(chunk));
    }
    Some(out)
}
