use crate::dummy_packets::get_dummy_packet;
use anyhow::Context;
use sedsprintf_rs_2026::router::Router;
use sedsprintf_rs_2026::{TelemetryError, TelemetryResult};
use serial::{SerialPort, SystemPort};
use std::error::Error;
use std::io::{Read, Write};
use std::time::Duration;

pub const RADIO_PORT: &str = "/dev/ttyUSB1";
pub const RADIO_BAUDRATE: usize = 57_600;
pub const MAX_PACKET_SIZE: usize = 256;

// ======================================================================
//  Radio Device Trait
// ======================================================================
pub trait RadioDevice: Send {
    fn recv_packet(&mut self, router: &Router) -> TelemetryResult<()>;
    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>>;
}

// ======================================================================
//  Real Radio Implementation
// ======================================================================
pub struct Radio {
    inner: SystemPort,
}

impl Radio {
    pub fn open(path: &str, baud: usize) -> anyhow::Result<Self> {
        let mut inner = serial::open(path)?;
        inner
            .reconfigure(&|settings| {
                settings.set_baud_rate(serial::BaudRate::from_speed(baud))?;
                settings.set_char_size(serial::CharSize::Bits8);
                settings.set_parity(serial::Parity::ParityNone);
                settings.set_stop_bits(serial::StopBits::Stop1);
                settings.set_flow_control(serial::FlowControl::FlowNone);
                Ok(())
            })
            .context("failed to configure serial port")?;
        inner.set_timeout(Duration::from_millis(200))?;
        Ok(Self { inner })
    }
}

impl RadioDevice for Radio {
    /// Blocking receive of one TelemetryPacket
    fn recv_packet(&mut self, router: &Router) -> TelemetryResult<()> {
        // read length prefix
        let mut len_buf = [0u8; 2];
        self.inner.read_exact(&mut len_buf)?;
        let frame_len = u16::from_le_bytes(len_buf) as usize;

        if frame_len == 0 || frame_len > MAX_PACKET_SIZE {
            return Err(TelemetryError::HandlerError(
                "invalid frame length from radio",
            ));
        }

        // read payload
        let mut payload = vec![0u8; frame_len];
        self.inner.read_exact(&mut payload)?;

        router.rx_serialized_packet_to_queue(&*payload)
    }

    /// Blocking send of serialized bytes (length-prefixed).
    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        let len = payload.len();

        if len == 0 || len > u16::MAX as usize {
            return Err(format!("packet too large to send over radio: {len} bytes").into());
        }

        let len_bytes = (len as u16).to_le_bytes();

        self.inner.write_all(&len_bytes)?;
        self.inner.write_all(payload)?;
        self.inner.flush()?;
        Ok(())
    }
}

// ======================================================================
//  Dummy Radio (fallback when hardware missing)
// ======================================================================
#[derive(Debug, Default)]
pub struct DummyRadio;

impl DummyRadio {
    pub fn new() -> Self {
        DummyRadio 
    }
}

impl RadioDevice for DummyRadio {
    fn recv_packet(&mut self, router: &Router) -> TelemetryResult<()> {
        let pkt = get_dummy_packet()?;

        // No incoming packets in dummy mode
        router.rx_packet_to_queue(pkt)
    }

    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        tracing::warn!(
            "DummyRadio: dropping {} bytes of outgoing telemetry (no radio connected)",
            payload.len()
        );
        Ok(())
    }
}
