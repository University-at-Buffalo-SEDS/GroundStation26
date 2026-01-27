#[cfg(feature = "testing")]
use crate::dummy_packets::get_dummy_packet;
use anyhow::Context;
use sedsprintf_rs_2026::router::{LinkId, Router};
use sedsprintf_rs_2026::{TelemetryError, TelemetryResult};
use serial::{SerialPort, SystemPort};
use std::error::Error;
use std::io::{Read, Write};
use std::time::Duration;

pub const ROCKET_RADIO_PORT: &str = "/dev/ttyUSB1";
pub const UMBILICAL_RADIO_PORT: &str = "/dev/ttyUSB2";
pub const RADIO_BAUD_RATE: usize = 57_600;
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
    id: LinkId
}

impl Radio {
    pub fn open(path: &str, baud: usize, id: LinkId) -> anyhow::Result<Self> {
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
        Ok(Self { inner, id})
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

        router.rx_serialized_queue_from(&payload, self.id)
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
#[cfg(feature = "testing")]
#[derive(Debug)]
pub struct DummyRadio {
    name: &'static str,
    sender: Option<&'static str>,
    id: LinkId,
}

#[cfg(feature = "testing")]

impl DummyRadio {
    pub fn new(name: &'static str, sender: Option<&'static str>, id: LinkId) -> Self {
        DummyRadio { name, sender, id }
    }
}

#[cfg(feature = "testing")]
impl RadioDevice for DummyRadio {
    fn recv_packet(&mut self, _router: &Router) -> TelemetryResult<()> {
        let pkt = get_dummy_packet()?; //self.sender)?;
        return _router.rx_queue_from(pkt, self.id);

        // No incoming packets in dummy mode
    }

    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        println!(
            "DummyRadio: dropping {} bytes of outgoing telemetry send from {}",
            payload.len(),
            self.name
        );
        Ok(())
    }
}
