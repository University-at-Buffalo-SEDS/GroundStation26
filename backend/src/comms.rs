use crate::comms_config::{
    CanLinkConfig, CommsLinkConfig, I2cLinkConfig, SerialLinkConfig, SerialProtocol, SpiLinkConfig,
};
#[cfg(feature = "testing")]
use crate::dummy_packets::get_dummy_packet;
use anyhow::Context;
use sedsprintf_rs_2026::{
    TelemetryError, TelemetryResult,
    router::{Router, RouterSideId},
    serialize,
};
use serialport::SerialPort;
use std::error::Error;
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::fs::File;
#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::mem::size_of;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::time::SystemTime;
use std::time::{Duration, Instant};

pub const ROCKET_COMMS_PORT: &str = "/dev/ttyUSB1";
pub const UMBILICAL_COMMS_PORT: &str = "/dev/ttyUSB2";
pub const COMMS_BAUD_RATE: usize = 57_600;
pub const MAX_PACKET_SIZE: usize = 256;
const STREAM_PACKET_MAX_SIZE: usize = 4_096;
const RAW_UART_FRAME_SYNC_0: u8 = 0xA5;
const RAW_UART_FRAME_SYNC_1: u8 = 0x5A;
const RAW_UART_COMMAND_SYNC_0: u8 = 0xA6;
const RAW_UART_COMMAND_SYNC_1: u8 = 0x5B;
const RAW_UART_ASCII_SYNC_0: u8 = 0xA7;
const RAW_UART_ASCII_SYNC_1: u8 = 0x7A;
const RAW_UART_FRAME_HEADER_SIZE: usize = 4;
const RAW_UART_MAX_FRAME_BYTES: usize = 4_096;
const RAW_UART_DEBUG_PREVIEW_BYTES: usize = 48;
#[cfg(target_os = "linux")]
const I2C_PACKET_MAX_BYTES: usize = 4_096;
#[cfg(target_os = "linux")]
const I2C_FRAME_PAYLOAD_MAX_BYTES: usize = I2C_PACKET_MAX_BYTES - RAW_UART_FRAME_HEADER_SIZE;
const UART_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const UART_NORMAL_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const I2C_DEBUG_PREVIEW_BYTES: usize = 48;
#[cfg(target_os = "linux")]
const I2C_SLOT_SIZE: usize = 32;
#[cfg(target_os = "linux")]
const I2C_SLOT_HEADER_SIZE: usize = 18;
#[cfg(target_os = "linux")]
const I2C_SLOT_PAYLOAD_SIZE: usize = I2C_SLOT_SIZE - I2C_SLOT_HEADER_SIZE;
#[cfg(target_os = "linux")]
const I2C_SLOT_MAGIC_0: u8 = 0x49;
#[cfg(target_os = "linux")]
const I2C_SLOT_MAGIC_1: u8 = 0x32;
#[cfg(target_os = "linux")]
const I2C_SLOT_VERSION: u8 = 1;
#[cfg(target_os = "linux")]
const I2C_KIND_IDLE: u8 = 0;
#[cfg(target_os = "linux")]
const I2C_KIND_DATA: u8 = 1;
#[cfg(target_os = "linux")]
const I2C_KIND_COMMAND: u8 = 2;
#[cfg(target_os = "linux")]
const I2C_KIND_ERROR: u8 = 127;
#[cfg(target_os = "linux")]
const I2C_FLAG_START: u8 = 0x01;
#[cfg(target_os = "linux")]
const I2C_FLAG_END: u8 = 0x02;
#[cfg(target_os = "linux")]
const I2C_PARTIAL_PACKET_TIMEOUT: Duration = Duration::from_millis(50);

#[cfg(feature = "testing")]
const DUMMY_ROCKET_TIMESYNC_SOURCES: &[&str] = &["RF", "FC", "PB"];
#[cfg(feature = "testing")]
const DUMMY_UMBILICAL_TIMESYNC_SOURCES: &[&str] = &["GW", "VB", "AB", "DAQ"];

// ======================================================================
//  Comms Device Trait
// ======================================================================
pub trait CommsDevice: Send {
    fn recv_packet(&mut self, router: &Router) -> TelemetryResult<()>;
    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>>;
    fn set_side_id(&mut self, side_id: RouterSideId);
}

pub fn link_description(cfg: &CommsLinkConfig) -> String {
    match cfg {
        CommsLinkConfig::Serial { serial } => serial_description("serial", serial),
        CommsLinkConfig::RaspberryPiGpioUart { serial } => {
            serial_description("raspberry_pi_gpio_uart", serial)
        }
        CommsLinkConfig::CustomSerial { serial } => serial_description("custom_serial", serial),
        CommsLinkConfig::Spi { spi } => spi_description(spi),
        CommsLinkConfig::Can { can } => can_description(can),
        CommsLinkConfig::I2c { i2c } => i2c_description(i2c),
    }
}

pub fn open_link(cfg: &CommsLinkConfig) -> anyhow::Result<Box<dyn CommsDevice>> {
    match cfg {
        CommsLinkConfig::Serial { serial }
        | CommsLinkConfig::RaspberryPiGpioUart { serial }
        | CommsLinkConfig::CustomSerial { serial } => Ok(Box::new(UartComms::open(serial)?)),
        CommsLinkConfig::Spi { spi } => Ok(Box::new(SpiComms::open(spi)?)),
        CommsLinkConfig::Can { can } => Ok(Box::new(CanComms::open(can)?)),
        CommsLinkConfig::I2c { i2c } => Ok(Box::new(I2cComms::open(i2c)?)),
    }
}

pub fn startup_failure_hint(cfg: &CommsLinkConfig) -> String {
    match cfg {
        CommsLinkConfig::Serial { serial } | CommsLinkConfig::CustomSerial { serial } => {
            format!(
                "Check that {} exists, is the correct serial device, and is not already in use. Prefer a stable /dev/serial/by-id path when available.",
                serial.port
            )
        }
        CommsLinkConfig::RaspberryPiGpioUart { serial } => format!(
            "Check that {} exists and that the Raspberry Pi UART is enabled. On Raspberry Pi OS or Ubuntu on Pi: set enable_uart=1, disable the serial login console/getty, reboot, then retry. This UART path is generic Linux serial access; it does not require rppal.",
            serial.port
        ),
        CommsLinkConfig::Spi { spi } => format!(
            "Check that {} exists and that SPI is enabled. On Raspberry Pi OS or Ubuntu on Pi: enable SPI in the boot config so /dev/spidev* appears, then confirm mode {} and {} Hz match the attached device.",
            spi.port, spi.spi_mode, spi.spi_speed_hz
        ),
        CommsLinkConfig::Can { can } => format!(
            "Check that CAN interface {} exists and is up. Example: `sudo ip link set {} type can bitrate 500000` then `sudo ip link set {} up`. Confirm the remote device uses tx_id=0x{:x} / rx_id=0x{:x}.",
            can.port, can.port, can.port, can.can_tx_id, can.can_rx_id
        ),
        CommsLinkConfig::I2c { i2c } => format!(
            "Check that /dev/i2c-{} exists, I2C is enabled on the Pi, the remote Pico is at address 0x{:02x}, and SDA/SCL plus pull-ups are correct.",
            i2c.bus, i2c.addr
        ),
    }
}

fn serial_description(name: &str, serial: &SerialLinkConfig) -> String {
    format!(
        "interface={name} port={} baud_rate={} protocol={:?}",
        serial.port, serial.baud_rate, serial.protocol
    )
}

fn spi_description(spi: &SpiLinkConfig) -> String {
    format!(
        "interface=spi port={} speed_hz={} mode={} bits_per_word={}",
        spi.port, spi.spi_speed_hz, spi.spi_mode, spi.spi_bits_per_word
    )
}

fn can_description(can: &CanLinkConfig) -> String {
    format!(
        "interface=can ifname={} tx_id=0x{:x} rx_id=0x{:x}",
        can.port, can.can_tx_id, can.can_rx_id
    )
}

fn i2c_description(i2c: &I2cLinkConfig) -> String {
    format!(
        "interface=i2c bus={} addr=0x{:02x} chunk_delay_ms={} initial_wait_ms={}",
        i2c.bus, i2c.addr, i2c.chunk_delay_ms, i2c.initial_wait_ms
    )
}

// ======================================================================
//  Real Comms Implementation
// ======================================================================
pub struct UartComms {
    inner: Box<dyn SerialPort>,
    side_id: Option<RouterSideId>,
    rx_buf: Vec<u8>,
    protocol: SerialProtocol,
    slow_start_deadline: Option<Instant>,
}

impl UartComms {
    pub fn open(cfg: &SerialLinkConfig) -> anyhow::Result<Self> {
        let inner = serialport::new(&cfg.port, cfg.baud_rate as u32)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None)
            .timeout(UART_STARTUP_TIMEOUT)
            .open()
            .context("failed to configure serial port")?;
        Ok(Self {
            inner,
            side_id: None,
            rx_buf: Vec::with_capacity(STREAM_PACKET_MAX_SIZE),
            protocol: cfg.protocol.clone(),
            slow_start_deadline: Some(Instant::now() + UART_STARTUP_TIMEOUT),
        })
    }

    fn update_uart_timeout_mode(&mut self) -> std::io::Result<()> {
        if self
            .slow_start_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.inner.set_timeout(UART_NORMAL_TIMEOUT)?;
            self.slow_start_deadline = None;
        }
        Ok(())
    }

    fn fill_rx_buf(&mut self) -> std::io::Result<()> {
        self.update_uart_timeout_mode()?;
        let mut scratch = [0u8; 512];
        match self.inner.read(&mut scratch) {
            Ok(0) => {
                maybe_log_raw_uart_read_outcome(
                    "read returned 0 bytes",
                    self.rx_buf.len(),
                    None,
                    &self.protocol,
                );
                Ok(())
            }
            Ok(n) => {
                if self.slow_start_deadline.take().is_some() {
                    self.inner.set_timeout(UART_NORMAL_TIMEOUT)?;
                }
                maybe_log_raw_uart_read_outcome(
                    "read returned bytes",
                    self.rx_buf.len(),
                    Some(n),
                    &self.protocol,
                );
                self.rx_buf.extend_from_slice(&scratch[..n]);
                maybe_log_raw_uart_rx(&scratch[..n], &self.protocol);
                Ok(())
            }
            Err(err) if is_idle_serial_timeout(&err) => {
                maybe_log_raw_uart_read_outcome(
                    "read timeout/wouldblock",
                    self.rx_buf.len(),
                    None,
                    &self.protocol,
                );
                Ok(())
            }
            Err(err) => {
                maybe_log_raw_uart_read_error(&err, self.rx_buf.len(), &self.protocol);
                Err(err)
            }
        }
    }

    fn try_take_framed_packet(&mut self) -> TelemetryResult<Option<Vec<u8>>> {
        if self.rx_buf.len() < 2 {
            return Ok(None);
        }

        let frame_len = u16::from_le_bytes([self.rx_buf[0], self.rx_buf[1]]) as usize;
        if frame_len == 0 || frame_len > MAX_PACKET_SIZE {
            self.rx_buf.clear();
            return Err(TelemetryError::HandlerError(
                "invalid frame length from comms",
            ));
        }

        let total_len = 2 + frame_len;
        if self.rx_buf.len() < total_len {
            return Ok(None);
        }

        let payload = self.rx_buf[2..total_len].to_vec();
        self.rx_buf.drain(..total_len);
        Ok(Some(payload))
    }

    fn try_take_raw_uart_packet(&mut self) -> TelemetryResult<Option<Vec<u8>>> {
        take_raw_uart_framed_payload(&mut self.rx_buf)
    }

    fn try_take_packet(&mut self) -> TelemetryResult<Option<Vec<u8>>> {
        match self.protocol {
            SerialProtocol::PacketFramed => self.try_take_framed_packet(),
            SerialProtocol::RawUart => self.try_take_raw_uart_packet(),
        }
    }

    fn handle_raw_uart_router_reject(&mut self, payload: &[u8]) {
        maybe_log_raw_uart_router_error(payload, &self.protocol);

        // If a corrupted payload swallowed the next frame start, salvage from the
        // embedded sync marker rather than dropping the entire burst.
        let mut recovered = payload
            .windows(2)
            .enumerate()
            .skip(1)
            .find_map(|(idx, pair)| {
                (pair == [RAW_UART_FRAME_SYNC_0, RAW_UART_FRAME_SYNC_1]).then_some(idx)
            })
            .map(|idx| payload[idx..].to_vec())
            .unwrap_or_default();

        if !self.rx_buf.is_empty() {
            recovered.extend_from_slice(&self.rx_buf);
        }
        self.rx_buf = recovered;
        maybe_log_raw_uart_buffer_state(
            &self.rx_buf,
            "resynced after router reject",
            &self.protocol,
        );
    }

    fn process_buffered_packets(
        &mut self,
        router: &Router,
        side_id: RouterSideId,
    ) -> TelemetryResult<bool> {
        let mut processed_any = false;

        loop {
            let payload: Vec<u8> = match self.try_take_packet()? {
                Some(payload) => payload,
                None => break,
            };
            processed_any = true;
            maybe_log_raw_uart_decoded(&payload, &self.protocol);
            maybe_log_raw_uart_router_queue_before(&payload, &self.protocol);
            match router.rx_serialized_queue_from_side(&payload, side_id) {
                Ok(()) => {
                    maybe_log_raw_uart_router_queue_after(&payload, &self.protocol);
                }
                Err(err) => {
                    if matches!(self.protocol, SerialProtocol::RawUart) {
                        let _ = err;
                        self.handle_raw_uart_router_reject(&payload);
                        continue;
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        Ok(processed_any)
    }
}

fn is_idle_serial_timeout(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

fn raw_uart_debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GS_RAW_UART_DEBUG").ok().as_deref() == Some("1"))
}

fn unix_now_ms() -> u64 {
    match SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(_) => 0,
    }
}

fn build_raw_uart_frame(payload: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
    build_link_frame(RAW_UART_FRAME_SYNC_0, RAW_UART_FRAME_SYNC_1, payload)
}

#[cfg(target_os = "linux")]
fn build_i2c_data_frame(payload: &[u8]) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
    if payload.len() > I2C_FRAME_PAYLOAD_MAX_BYTES {
        return Err(format!(
            "packet too large to send over i2c framed link: {} > {} bytes",
            payload.len(),
            I2C_FRAME_PAYLOAD_MAX_BYTES
        )
        .into());
    }
    build_link_frame(RAW_UART_FRAME_SYNC_0, RAW_UART_FRAME_SYNC_1, payload)
}

fn build_link_frame(
    sync_0: u8,
    sync_1: u8,
    payload: &[u8],
) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
    let len = payload.len();
    if len == 0 || len > STREAM_PACKET_MAX_SIZE {
        return Err(format!("packet too large to send over framed link: {len} bytes").into());
    }

    let mut framed = Vec::with_capacity(RAW_UART_FRAME_HEADER_SIZE + len);
    framed.push(sync_0);
    framed.push(sync_1);
    framed.extend_from_slice(&(len as u16).to_le_bytes());
    framed.extend_from_slice(payload);
    Ok(framed)
}

fn parse_link_frame(payload: &[u8]) -> Option<((u8, u8), &[u8])> {
    if payload.len() < RAW_UART_FRAME_HEADER_SIZE {
        return None;
    }
    let header = match (payload[0], payload[1]) {
        (RAW_UART_FRAME_SYNC_0, RAW_UART_FRAME_SYNC_1)
        | (RAW_UART_FRAME_SYNC_1, RAW_UART_FRAME_SYNC_0) => {
            (RAW_UART_FRAME_SYNC_0, RAW_UART_FRAME_SYNC_1)
        }
        (RAW_UART_COMMAND_SYNC_0, RAW_UART_COMMAND_SYNC_1)
        | (RAW_UART_COMMAND_SYNC_1, RAW_UART_COMMAND_SYNC_0) => {
            (RAW_UART_COMMAND_SYNC_0, RAW_UART_COMMAND_SYNC_1)
        }
        (RAW_UART_ASCII_SYNC_0, RAW_UART_ASCII_SYNC_1) => {
            (RAW_UART_ASCII_SYNC_0, RAW_UART_ASCII_SYNC_1)
        }
        _ => return None,
    };
    let len = u16::from_le_bytes([payload[2], payload[3]]) as usize;
    if payload.len() < RAW_UART_FRAME_HEADER_SIZE + len {
        return None;
    }
    Some((
        header,
        &payload[RAW_UART_FRAME_HEADER_SIZE..RAW_UART_FRAME_HEADER_SIZE + len],
    ))
}

fn take_raw_uart_framed_payload(rx_buf: &mut Vec<u8>) -> TelemetryResult<Option<Vec<u8>>> {
    if rx_buf.len() > RAW_UART_MAX_FRAME_BYTES {
        let drop_len = rx_buf.len() - RAW_UART_MAX_FRAME_BYTES;
        maybe_log_raw_uart_parse_issue(
            "dropping oversized raw UART buffer",
            &rx_buf[..rx_buf.len().min(RAW_UART_DEBUG_PREVIEW_BYTES)],
        );
        rx_buf.drain(..drop_len);
    }

    let sync_pos = rx_buf.windows(2).position(|pair| {
        pair == [RAW_UART_FRAME_SYNC_0, RAW_UART_FRAME_SYNC_1]
            || pair == [RAW_UART_COMMAND_SYNC_0, RAW_UART_COMMAND_SYNC_1]
            || pair == [RAW_UART_ASCII_SYNC_0, RAW_UART_ASCII_SYNC_1]
    });
    match sync_pos {
        Some(0) => {}
        Some(pos) => {
            maybe_log_raw_uart_parse_issue(
                "dropping bytes before raw UART frame sync",
                &rx_buf[..pos.min(RAW_UART_DEBUG_PREVIEW_BYTES)],
            );
            rx_buf.drain(..pos);
        }
        None => {
            if !rx_buf.is_empty() {
                maybe_log_raw_uart_parse_issue(
                    "raw UART waiting for frame sync",
                    &rx_buf[..rx_buf.len().min(RAW_UART_DEBUG_PREVIEW_BYTES)],
                );
                let keep = usize::from(matches!(
                    rx_buf.last().copied(),
                    Some(RAW_UART_FRAME_SYNC_0 | RAW_UART_COMMAND_SYNC_0 | RAW_UART_ASCII_SYNC_0)
                ));
                let drop_len = rx_buf.len().saturating_sub(keep);
                if drop_len > 0 {
                    rx_buf.drain(..drop_len);
                }
            }
            return Ok(None);
        }
    }

    if rx_buf.len() < RAW_UART_FRAME_HEADER_SIZE {
        return Ok(None);
    }

    let frame_len = u16::from_le_bytes([rx_buf[2], rx_buf[3]]) as usize;
    if frame_len == 0 {
        maybe_log_raw_uart_parse_issue(
            "ignoring empty raw UART frame",
            &rx_buf[..RAW_UART_FRAME_HEADER_SIZE],
        );
        rx_buf.drain(..RAW_UART_FRAME_HEADER_SIZE);
        return Ok(None);
    }

    if frame_len > STREAM_PACKET_MAX_SIZE {
        maybe_log_raw_uart_parse_issue(
            &format!("invalid raw UART frame length: {frame_len}"),
            &rx_buf[..rx_buf.len().min(RAW_UART_DEBUG_PREVIEW_BYTES)],
        );
        rx_buf.drain(..1);
        return Ok(None);
    }

    let total_len = RAW_UART_FRAME_HEADER_SIZE + frame_len;
    if rx_buf.len() < total_len {
        return Ok(None);
    }

    let payload = rx_buf[RAW_UART_FRAME_HEADER_SIZE..total_len].to_vec();
    rx_buf.drain(..total_len);
    Ok(Some(payload))
}

#[cfg(target_os = "linux")]
fn i2c_debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GS_I2C_DEBUG").ok().as_deref() == Some("1"))
}

fn maybe_log_raw_uart_rx(bytes: &[u8], protocol: &SerialProtocol) {
    if !raw_uart_debug_enabled() || !matches!(protocol, SerialProtocol::RawUart) {
        return;
    }
    eprintln!(
        "raw_uart rx {} bytes: {}",
        bytes.len(),
        hex_preview(bytes, RAW_UART_DEBUG_PREVIEW_BYTES)
    );
}

fn maybe_log_raw_uart_read_outcome(
    context: &str,
    buffered_len: usize,
    read_len: Option<usize>,
    protocol: &SerialProtocol,
) {
    if !raw_uart_debug_enabled() || !matches!(protocol, SerialProtocol::RawUart) {
        return;
    }
    static LAST_LOG_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now_ms = unix_now_ms();
    let last = LAST_LOG_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 200 {
        return;
    }
    LAST_LOG_MS.store(now_ms, std::sync::atomic::Ordering::Relaxed);
    match read_len {
        Some(n) => eprintln!("{context}: n={n} buffered={buffered_len}"),
        None => eprintln!("{context}: buffered={buffered_len}"),
    }
}

fn maybe_log_raw_uart_read_error(
    err: &std::io::Error,
    buffered_len: usize,
    protocol: &SerialProtocol,
) {
    if !raw_uart_debug_enabled() || !matches!(protocol, SerialProtocol::RawUart) {
        return;
    }
    eprintln!("raw_uart read error: {err}; buffered={buffered_len}");
}

fn maybe_log_raw_uart_parse_issue(context: &str, bytes: &[u8]) {
    if !raw_uart_debug_enabled() {
        return;
    }
    static LAST_LOG_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now_ms = unix_now_ms();
    let last = LAST_LOG_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 500 {
        return;
    }
    LAST_LOG_MS.store(now_ms, std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "{context}; buffered {} bytes: {}",
        bytes.len(),
        hex_preview(bytes, RAW_UART_DEBUG_PREVIEW_BYTES)
    );
}

fn maybe_log_raw_uart_router_error(payload: &[u8], protocol: &SerialProtocol) {
    if !raw_uart_debug_enabled() || !matches!(protocol, SerialProtocol::RawUart) {
        return;
    }
    eprintln!(
        "raw_uart router rejected {} bytes; payload: {}",
        payload.len(),
        hex_preview(payload, RAW_UART_DEBUG_PREVIEW_BYTES)
    );
}

fn maybe_log_raw_uart_buffer_state(rx_buf: &[u8], context: &str, protocol: &SerialProtocol) {
    if !raw_uart_debug_enabled() || !matches!(protocol, SerialProtocol::RawUart) {
        return;
    }
    static LAST_LOG_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now_ms = unix_now_ms();
    let last = LAST_LOG_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 1_000 {
        return;
    }
    LAST_LOG_MS.store(now_ms, std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "raw_uart {context}: buffered={} preview={}",
        rx_buf.len(),
        hex_preview(rx_buf, RAW_UART_DEBUG_PREVIEW_BYTES)
    );
}

fn maybe_log_raw_uart_decoded(payload: &[u8], protocol: &SerialProtocol) {
    if !raw_uart_debug_enabled() || !matches!(protocol, SerialProtocol::RawUart) {
        return;
    }
    eprintln!(
        "raw_uart decoded {} bytes: {}",
        payload.len(),
        hex_preview(payload, RAW_UART_DEBUG_PREVIEW_BYTES)
    );
}

fn maybe_log_raw_uart_router_queue_before(payload: &[u8], protocol: &SerialProtocol) {
    if !raw_uart_debug_enabled() || !matches!(protocol, SerialProtocol::RawUart) {
        return;
    }
    eprintln!(
        "raw_uart queueing {} bytes to router: {}",
        payload.len(),
        hex_preview(payload, RAW_UART_DEBUG_PREVIEW_BYTES)
    );
}

fn maybe_log_raw_uart_router_queue_after(payload: &[u8], protocol: &SerialProtocol) {
    if !raw_uart_debug_enabled() || !matches!(protocol, SerialProtocol::RawUart) {
        return;
    }
    eprintln!("raw_uart router accepted {} bytes", payload.len());
}

#[cfg(target_os = "linux")]
fn maybe_log_i2c_frame(context: &str, bytes: &[u8]) {
    if !i2c_debug_enabled() {
        return;
    }
    eprintln!(
        "{context}: {} bytes: {}",
        bytes.len(),
        hex_preview(bytes, I2C_DEBUG_PREVIEW_BYTES)
    );
}

#[cfg(target_os = "linux")]
fn maybe_log_i2c_decoded(payload: &[u8]) {
    if !i2c_debug_enabled() {
        return;
    }
    match serialize::peek_frame_info(payload) {
        Ok(_) => {
            eprintln!(
                "i2c decoded {} bytes: {}",
                payload.len(),
                hex_preview(payload, I2C_DEBUG_PREVIEW_BYTES)
            );
        }
        Err(_) => {
            eprintln!(
                "i2c decoded {} bytes but payload is not a valid serialized packet: {}",
                payload.len(),
                hex_preview(payload, I2C_DEBUG_PREVIEW_BYTES)
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn log_i2c_router_decode_error(context: &str, payload: &[u8], err: &dyn Error) {
    eprintln!(
        "i2c router decode error ({context}): bytes={} err={err} data={}",
        payload.len(),
        hex_preview(payload, I2C_DEBUG_PREVIEW_BYTES)
    );
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct I2cMailboxSlot {
    kind: u8,
    flags: u8,
    transfer_id: u16,
    offset: u32,
    total_len: u32,
    data: Vec<u8>,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct I2cRxAssembly {
    kind: u8,
    transfer_id: u16,
    total_len: usize,
    next_offset: usize,
    payload: Vec<u8>,
    started_at: Instant,
}

#[cfg(target_os = "linux")]
impl I2cRxAssembly {
    fn new(slot: &I2cMailboxSlot) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if slot.flags & I2C_FLAG_START == 0 {
            return Err(std::io::Error::other("i2c transfer must start with START flag").into());
        }
        if slot.offset != 0 {
            return Err(std::io::Error::other("i2c transfer start must have offset 0").into());
        }
        let total_len = slot.total_len as usize;
        if total_len > STREAM_PACKET_MAX_SIZE {
            return Err(std::io::Error::other("i2c transfer exceeds max packet size").into());
        }
        if total_len < slot.data.len() {
            return Err(
                std::io::Error::other("i2c transfer total length smaller than first slot").into(),
            );
        }
        let mut payload = Vec::with_capacity(total_len);
        payload.extend_from_slice(&slot.data);
        Ok(Self {
            kind: slot.kind,
            transfer_id: slot.transfer_id,
            total_len,
            next_offset: slot.data.len(),
            payload,
            started_at: Instant::now(),
        })
    }

    fn push(
        &mut self,
        slot: &I2cMailboxSlot,
    ) -> Result<Option<Vec<u8>>, Box<dyn Error + Send + Sync>> {
        if slot.kind != self.kind {
            return Err(std::io::Error::other("i2c transfer kind changed mid-stream").into());
        }
        if slot.transfer_id != self.transfer_id {
            return Err(std::io::Error::other("i2c transfer id changed mid-stream").into());
        }
        if slot.offset as usize != self.next_offset {
            return Err(std::io::Error::other(format!(
                "i2c transfer offset mismatch: expected {} got {}",
                self.next_offset, slot.offset
            ))
            .into());
        }
        if self.payload.len() + slot.data.len() > self.total_len {
            return Err(
                std::io::Error::other("i2c transfer exceeded declared total length").into(),
            );
        }
        if slot.offset != 0 {
            self.payload.extend_from_slice(&slot.data);
            self.next_offset += slot.data.len();
        }
        if slot.flags & I2C_FLAG_END != 0 {
            if self.payload.len() != self.total_len {
                return Err(std::io::Error::other(format!(
                    "i2c transfer ended early: expected {} bytes got {}",
                    self.total_len,
                    self.payload.len()
                ))
                .into());
            }
            return Ok(Some(std::mem::take(&mut self.payload)));
        }
        Ok(None)
    }
}

#[cfg(target_os = "linux")]
fn encode_i2c_slot(
    kind: u8,
    flags: u8,
    transfer_id: u16,
    offset: u32,
    total_len: u32,
    data: &[u8],
) -> [u8; I2C_SLOT_SIZE] {
    let mut raw = [0u8; I2C_SLOT_SIZE];
    let data_len = data.len().min(I2C_SLOT_PAYLOAD_SIZE);
    raw[0] = I2C_SLOT_MAGIC_0;
    raw[1] = I2C_SLOT_MAGIC_1;
    raw[2] = I2C_SLOT_VERSION;
    raw[3] = kind;
    raw[4] = flags;
    raw[5] = 0;
    raw[6..10].copy_from_slice(&offset.to_le_bytes());
    raw[10..14].copy_from_slice(&total_len.to_le_bytes());
    raw[14..16].copy_from_slice(&(data_len as u16).to_le_bytes());
    raw[16..18].copy_from_slice(&transfer_id.to_le_bytes());
    raw[I2C_SLOT_HEADER_SIZE..I2C_SLOT_HEADER_SIZE + data_len].copy_from_slice(&data[..data_len]);
    raw
}

#[cfg(target_os = "linux")]
fn decode_i2c_slot(
    raw: &[u8; I2C_SLOT_SIZE],
) -> Result<Option<I2cMailboxSlot>, Box<dyn Error + Send + Sync>> {
    let all_zero = raw.iter().all(|byte| *byte == 0x00);
    let all_ff = raw.iter().all(|byte| *byte == 0xFF);
    if all_zero || all_ff {
        return Ok(None);
    }
    if raw[0] != I2C_SLOT_MAGIC_0 || raw[1] != I2C_SLOT_MAGIC_1 {
        return Err(std::io::Error::other(format!(
            "invalid i2c slot magic: {:02x} {:02x}",
            raw[0], raw[1]
        ))
        .into());
    }
    if raw[2] != I2C_SLOT_VERSION {
        return Err(
            std::io::Error::other(format!("unsupported i2c slot version: {}", raw[2])).into(),
        );
    }
    let kind = raw[3];
    if kind == I2C_KIND_IDLE {
        return Ok(None);
    }
    let flags = raw[4];
    let offset = u32::from_le_bytes(raw[6..10].try_into().unwrap());
    let total_len = u32::from_le_bytes(raw[10..14].try_into().unwrap());
    let data_len = u16::from_le_bytes(raw[14..16].try_into().unwrap()) as usize;
    let transfer_id = u16::from_le_bytes(raw[16..18].try_into().unwrap());
    if data_len > I2C_SLOT_PAYLOAD_SIZE {
        return Err(
            std::io::Error::other(format!("invalid i2c slot payload length: {data_len}")).into(),
        );
    }
    let data = raw[I2C_SLOT_HEADER_SIZE..I2C_SLOT_HEADER_SIZE + data_len].to_vec();
    Ok(Some(I2cMailboxSlot {
        kind,
        flags,
        transfer_id,
        offset,
        total_len,
        data,
    }))
}

fn hex_preview(bytes: &[u8], limit: usize) -> String {
    let preview = bytes
        .iter()
        .take(limit)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    if bytes.len() > limit {
        format!("{preview} ...")
    } else {
        preview
    }
}

impl CommsDevice for UartComms {
    /// Blocking receive of one Packet
    fn recv_packet(&mut self, router: &Router) -> TelemetryResult<()> {
        let side_id = self
            .side_id
            .ok_or(TelemetryError::HandlerError("comms side id not set"))?;

        if self.rx_buf.len() < 2
            && let Err(err) = self.fill_rx_buf()
        {
            return Err(err.into());
        }
        if self.process_buffered_packets(router, side_id)? {
            return Ok(());
        }

        if let Err(err) = self.fill_rx_buf() {
            return Err(err.into());
        }

        if self.process_buffered_packets(router, side_id)? {
            return Ok(());
        }

        maybe_log_raw_uart_buffer_state(&self.rx_buf, "waiting for complete frame", &self.protocol);

        Ok(())
    }

    /// Blocking send of serialized bytes.
    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        let len = payload.len();

        if len == 0 || len > u16::MAX as usize {
            return Err(format!("packet too large to send over comms: {len} bytes").into());
        }

        match self.protocol {
            SerialProtocol::PacketFramed => {
                let len_bytes = (len as u16).to_le_bytes();
                self.inner.write_all(&len_bytes)?;
                self.inner.write_all(payload)?;
            }
            SerialProtocol::RawUart => {
                self.inner.write_all(&build_raw_uart_frame(payload)?)?;
            }
        }
        self.inner.flush()?;
        Ok(())
    }

    fn set_side_id(&mut self, side_id: RouterSideId) {
        self.side_id = Some(side_id);
    }
}

// ======================================================================
// I2C Comms Implementation
// ======================================================================

// This Linux backend speaks the Pico slot-based I2C protocol directly:
// one self-describing 32-byte slot per transfer, not the legacy fake 258-byte
// framing model used by older host experiments.

pub struct I2cComms {
    #[cfg(target_os = "linux")]
    inner: File,
    side_id: Option<RouterSideId>,
    #[cfg(target_os = "linux")]
    addr: u16,
    #[cfg(target_os = "linux")]
    chunk_delay: Duration,
    #[cfg(target_os = "linux")]
    initial_wait: Duration,
    #[cfg(target_os = "linux")]
    tx_transfer_id: u16,
    #[cfg(target_os = "linux")]
    rx_assembly: Option<I2cRxAssembly>,
    #[cfg(target_os = "linux")]
    rx_payload_buf: Vec<u8>,
}

impl I2cComms {
    pub fn open(cfg: &I2cLinkConfig) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let path = format!("/dev/i2c-{}", cfg.bus);
            let inner = OpenOptions::new().read(true).write(true).open(&path)?;
            Ok(Self {
                inner,
                side_id: None,
                addr: cfg.addr,
                chunk_delay: Duration::from_millis(cfg.chunk_delay_ms),
                initial_wait: Duration::from_millis(cfg.initial_wait_ms),
                tx_transfer_id: 1,
                rx_assembly: None,
                rx_payload_buf: Vec::with_capacity(STREAM_PACKET_MAX_SIZE),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = cfg;
            anyhow::bail!("I2C comms support is only implemented on Linux")
        }
    }

    #[cfg(target_os = "linux")]
    fn read_slot(&mut self) -> Result<Option<I2cMailboxSlot>, Box<dyn Error + Send + Sync>> {
        let mut raw = [0u8; I2C_SLOT_SIZE];
        match self.transfer_read(&mut raw) {
            Ok(()) => {
                maybe_log_i2c_frame("i2c rx slot", &raw);
                decode_i2c_slot(&raw)
            }
            Err(err) if is_i2c_idle_read_error(err.as_ref()) => Ok(None),
            Err(err) => Err(err),
        }
    }

    #[cfg(target_os = "linux")]
    fn next_transfer_id(&mut self) -> u16 {
        let current = self.tx_transfer_id;
        self.tx_transfer_id = self.tx_transfer_id.wrapping_add(1).max(1);
        current
    }

    #[cfg(target_os = "linux")]
    fn write_payload(
        &mut self,
        kind: u8,
        payload: &[u8],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let total_len = payload.len();
        if total_len > STREAM_PACKET_MAX_SIZE {
            return Err(format!("packet too large to send over i2c: {total_len} bytes").into());
        }
        let transfer_id = self.next_transfer_id();
        let mut offset = 0usize;

        if total_len == 0 {
            let slot = encode_i2c_slot(kind, I2C_FLAG_START | I2C_FLAG_END, transfer_id, 0, 0, &[]);
            maybe_log_i2c_frame("i2c tx slot", &slot);
            self.transfer_write(&slot)?;
            return Ok(());
        }

        while offset < total_len {
            let end = (offset + I2C_SLOT_PAYLOAD_SIZE).min(total_len);
            let mut flags = 0u8;
            if offset == 0 {
                flags |= I2C_FLAG_START;
            }
            if end >= total_len {
                flags |= I2C_FLAG_END;
            }
            let slot = encode_i2c_slot(
                kind,
                flags,
                transfer_id,
                offset as u32,
                total_len as u32,
                &payload[offset..end],
            );
            maybe_log_i2c_frame("i2c tx slot", &slot);
            self.transfer_write(&slot)?;
            offset = end;
            if offset < total_len {
                std::thread::sleep(self.chunk_delay);
            }
        }
        if !self.initial_wait.is_zero() {
            std::thread::sleep(self.initial_wait);
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn ingest_rx_slot(
        &mut self,
        slot: I2cMailboxSlot,
    ) -> Result<Option<(u8, Vec<u8>)>, Box<dyn Error + Send + Sync>> {
        if slot.kind == I2C_KIND_IDLE {
            return Ok(None);
        }

        if self.rx_assembly.as_ref().is_some_and(|assembly| {
            Instant::now().duration_since(assembly.started_at) >= I2C_PARTIAL_PACKET_TIMEOUT
        }) {
            self.rx_assembly = None;
        }

        if slot.flags & I2C_FLAG_START != 0 {
            let assembly = I2cRxAssembly::new(&slot)?;
            if slot.flags & I2C_FLAG_END != 0 {
                return Ok(Some((slot.kind, assembly.payload)));
            }
            self.rx_assembly = Some(assembly);
            return Ok(None);
        }

        let assembly = self
            .rx_assembly
            .as_mut()
            .ok_or_else(|| std::io::Error::other("i2c slot arrived without an active transfer"))?;
        let completed = match assembly.push(&slot) {
            Ok(completed) => completed,
            Err(err) => {
                self.rx_assembly = None;
                return Err(err);
            }
        };
        if completed.is_some() {
            self.rx_assembly = None;
        }
        Ok(completed.map(|payload| (slot.kind, payload)))
    }

    #[cfg(target_os = "linux")]
    fn try_take_buffered_packet(&mut self) -> TelemetryResult<Option<Vec<u8>>> {
        let scan_len = self.rx_payload_buf.len().min(RAW_UART_MAX_FRAME_BYTES);
        for start in 0..scan_len {
            for end in (start + 1)..=scan_len {
                let candidate = &self.rx_payload_buf[start..end];
                if serialize::peek_frame_info(candidate).is_ok() {
                    let payload = candidate.to_vec();
                    self.rx_payload_buf.drain(..end);
                    return Ok(Some(payload));
                }
            }
        }

        if self.rx_payload_buf.len() > RAW_UART_MAX_FRAME_BYTES {
            let drop_len = self.rx_payload_buf.len() - RAW_UART_MAX_FRAME_BYTES;
            self.rx_payload_buf.drain(..drop_len);
        }

        Ok(None)
    }

    #[cfg(target_os = "linux")]
    fn transfer_write(&mut self, data: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut msg = I2cMsg {
            addr: self.addr,
            flags: 0,
            len: data.len() as u16,
            buf: data.as_ptr() as *mut u8,
        };
        self.transfer_ioctl(&mut msg)
    }

    #[cfg(target_os = "linux")]
    fn transfer_read(&mut self, data: &mut [u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut msg = I2cMsg {
            addr: self.addr,
            flags: I2C_M_RD,
            len: data.len() as u16,
            buf: data.as_mut_ptr(),
        };
        self.transfer_ioctl(&mut msg)
    }

    #[cfg(target_os = "linux")]
    fn transfer_ioctl(&mut self, msg: &mut I2cMsg) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut ioctl_data = I2cRdwrIoctlData {
            msgs: msg as *mut _,
            nmsgs: 1,
        };

        let rc = unsafe { libc::ioctl(self.inner.as_raw_fd(), I2C_RDWR as _, &mut ioctl_data) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

impl CommsDevice for I2cComms {
    fn recv_packet(&mut self, router: &Router) -> TelemetryResult<()> {
        let side_id = self
            .side_id
            .ok_or(TelemetryError::HandlerError("comms side id not set"))?;

        #[cfg(target_os = "linux")]
        {
            for _ in 0..32 {
                match self.read_slot() {
                    Ok(Some(slot)) => {
                        let assembled = match self.ingest_rx_slot(slot) {
                            Ok(assembled) => assembled,
                            Err(err) => {
                                eprintln!("i2c receive failed: {err}");
                                continue;
                            }
                        };
                        if let Some((kind, payload)) = assembled {
                            maybe_log_i2c_decoded(&payload);
                            if kind != I2C_KIND_DATA {
                                if kind == I2C_KIND_ERROR {
                                    let msg = String::from_utf8_lossy(&payload);
                                    if msg != "error invalid i2c slot" || i2c_debug_enabled() {
                                        eprintln!("i2c error packet: {msg}");
                                    }
                                } else if kind == I2C_KIND_COMMAND {
                                    eprintln!("i2c command packet ignored on telemetry path");
                                } else {
                                    eprintln!("i2c non-data packet kind {kind} ignored");
                                }
                                continue;
                            }
                            let Some((header, payload)) = parse_link_frame(&payload) else {
                                log_i2c_router_decode_error(
                                    "data packet missing Pico link frame header",
                                    &payload,
                                    &std::io::Error::other("missing link frame header"),
                                );
                                continue;
                            };
                            if header != (RAW_UART_FRAME_SYNC_0, RAW_UART_FRAME_SYNC_1) {
                                if header == (RAW_UART_COMMAND_SYNC_0, RAW_UART_COMMAND_SYNC_1) {
                                    eprintln!(
                                        "i2c command response ignored on telemetry path: {}",
                                        String::from_utf8_lossy(payload)
                                    );
                                } else if header == (RAW_UART_ASCII_SYNC_0, RAW_UART_ASCII_SYNC_1) {
                                    eprintln!(
                                        "i2c raw ascii response ignored on telemetry path: {}",
                                        String::from_utf8_lossy(payload)
                                    );
                                }
                                continue;
                            }
                            self.rx_payload_buf.extend_from_slice(payload);
                            while let Some(packet) = self.try_take_buffered_packet()? {
                                match router.rx_serialized_queue_from_side(&packet, side_id) {
                                    Ok(()) => {}
                                    Err(err) => {
                                        log_i2c_router_decode_error(
                                            "router rejected serialized packet",
                                            &packet,
                                            &err,
                                        );
                                        eprintln!("i2c router reject: {err}");
                                        continue;
                                    }
                                }
                            }
                            if !self.rx_payload_buf.is_empty() {
                                match serialize::peek_frame_info(&self.rx_payload_buf) {
                                    Ok(_) => {}
                                    Err(err) => {
                                        log_i2c_router_decode_error(
                                            "buffered payload did not form a serialized packet",
                                            &self.rx_payload_buf,
                                            &err,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => continue,
                    Err(err) => {
                        eprintln!("i2c receive failed: {err}");
                        continue;
                    }
                }
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = router;
            let _ = side_id;
            Err(TelemetryError::HandlerError(
                "i2c comms support is only implemented on Linux",
            ))
        }
    }

    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        #[cfg(target_os = "linux")]
        {
            let framed = build_i2c_data_frame(payload)?;
            self.write_payload(I2C_KIND_DATA, &framed)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = payload;
            Err("i2c comms support is only implemented on Linux".into())
        }
    }

    fn set_side_id(&mut self, side_id: RouterSideId) {
        self.side_id = Some(side_id);
    }
}

// ======================================================================
// SPI Comms Implementation
// ======================================================================

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct SpiComms {
    inner: File,
    side_id: Option<RouterSideId>,
    speed_hz: u32,
    bits_per_word: u8,
}

impl SpiComms {
    pub fn open(cfg: &SpiLinkConfig) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let inner = OpenOptions::new().read(true).write(true).open(&cfg.port)?;
            let fd = inner.as_raw_fd();

            spi_ioctl_write(fd, SPI_IOC_WR_MODE, &cfg.spi_mode)
                .context("failed to set SPI mode")?;
            spi_ioctl_write(fd, SPI_IOC_WR_BITS_PER_WORD, &cfg.spi_bits_per_word)
                .context("failed to set SPI bits_per_word")?;
            spi_ioctl_write(fd, SPI_IOC_WR_MAX_SPEED_HZ, &cfg.spi_speed_hz)
                .context("failed to set SPI speed")?;

            Ok(Self {
                inner,
                side_id: None,
                speed_hz: cfg.spi_speed_hz,
                bits_per_word: cfg.spi_bits_per_word,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = cfg;
            anyhow::bail!("SPI comms support is only implemented on Linux")
        }
    }

    #[cfg(target_os = "linux")]
    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> anyhow::Result<()> {
        if tx.len() != rx.len() {
            anyhow::bail!("spi transfer tx/rx length mismatch");
        }
        let transfer = SpiIocTransfer {
            tx_buf: tx.as_ptr() as u64,
            rx_buf: rx.as_mut_ptr() as u64,
            len: tx.len() as u32,
            speed_hz: self.speed_hz,
            delay_usecs: 0,
            bits_per_word: self.bits_per_word,
            cs_change: 0,
            tx_nbits: 0,
            rx_nbits: 0,
            word_delay_usecs: 0,
            pad: 0,
        };
        let req = spi_ioc_message_request(1);
        let rc = unsafe { libc::ioctl(self.inner.as_raw_fd(), req as _, &transfer) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).context("spi ioctl transfer failed");
        }
        Ok(())
    }
}

impl CommsDevice for SpiComms {
    fn recv_packet(&mut self, router: &Router) -> TelemetryResult<()> {
        let side_id = self
            .side_id
            .ok_or(TelemetryError::HandlerError("comms side id not set"))?;

        #[cfg(target_os = "linux")]
        {
            let mut tx = vec![0u8; MAX_PACKET_SIZE + RAW_UART_FRAME_HEADER_SIZE];
            let mut rx = vec![0u8; MAX_PACKET_SIZE + RAW_UART_FRAME_HEADER_SIZE];
            self.transfer(&tx, &mut rx)
                .map_err(|_| TelemetryError::HandlerError("spi transfer failed"))?;
            tx.clear();

            let Some(((RAW_UART_FRAME_SYNC_0, RAW_UART_FRAME_SYNC_1), payload)) =
                parse_link_frame(&rx)
            else {
                return Ok(());
            };
            if payload.is_empty() {
                return Ok(());
            }
            return router.rx_serialized_queue_from_side(payload, side_id);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = router;
            let _ = side_id;
            Err(TelemetryError::HandlerError(
                "spi comms support is only implemented on Linux",
            ))
        }
    }

    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        if payload.is_empty() || payload.len() > MAX_PACKET_SIZE {
            return Err(
                format!("packet too large to send over spi: {} bytes", payload.len()).into(),
            );
        }

        #[cfg(target_os = "linux")]
        {
            let tx = build_raw_uart_frame(payload)?;
            let mut rx = vec![0u8; tx.len()];
            self.transfer(&tx, &mut rx)?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = payload;
            Err("spi comms support is only implemented on Linux".into())
        }
    }

    fn set_side_id(&mut self, side_id: RouterSideId) {
        self.side_id = Some(side_id);
    }
}

// ======================================================================
// CAN Comms Implementation
// ======================================================================

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct CanComms {
    inner: File,
    side_id: Option<RouterSideId>,
    tx_id: u32,
    rx_id: u32,
    rx_seq: Option<u8>,
    rx_expected_chunks: u8,
    rx_received_chunks: u8,
    rx_buf: Vec<u8>,
    tx_seq: u8,
}

impl CanComms {
    pub fn open(cfg: &CanLinkConfig) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let fd = open_can_socket(&cfg.port)?;
            let inner = unsafe { File::from_raw_fd(fd) };
            Ok(Self {
                inner,
                side_id: None,
                tx_id: cfg.can_tx_id,
                rx_id: cfg.can_rx_id,
                rx_seq: None,
                rx_expected_chunks: 0,
                rx_received_chunks: 0,
                rx_buf: Vec::new(),
                tx_seq: 0,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = cfg;
            anyhow::bail!("CAN comms support is only implemented on Linux")
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn reset_rx(&mut self) {
        self.rx_seq = None;
        self.rx_expected_chunks = 0;
        self.rx_received_chunks = 0;
        self.rx_buf.clear();
    }
}

impl CommsDevice for CanComms {
    fn recv_packet(&mut self, router: &Router) -> TelemetryResult<()> {
        let side_id = self
            .side_id
            .ok_or(TelemetryError::HandlerError("comms side id not set"))?;

        #[cfg(target_os = "linux")]
        {
            let frame = match can_read_frame(self.inner.as_raw_fd()) {
                Ok(Some(frame)) => frame,
                Ok(None) => return Ok(()),
                Err(_) => return Err(TelemetryError::HandlerError("can receive failed")),
            };

            if (frame.can_id & CAN_EFF_FLAG) != 0 || (frame.can_id & CAN_ERR_FLAG) != 0 {
                return Ok(());
            }
            if frame.can_id != self.rx_id {
                return Ok(());
            }
            if frame.can_dlc < 4 {
                self.reset_rx();
                return Err(TelemetryError::HandlerError("invalid can fragment header"));
            }

            let seq = frame.data[0];
            let chunk_idx = frame.data[1];
            let total_chunks = frame.data[2];
            let chunk_len = frame.data[3] as usize;

            if chunk_len > 4 || 4 + chunk_len > frame.can_dlc as usize || total_chunks == 0 {
                self.reset_rx();
                return Err(TelemetryError::HandlerError("invalid can fragment size"));
            }

            if chunk_idx == 0 {
                self.rx_seq = Some(seq);
                self.rx_expected_chunks = total_chunks;
                self.rx_received_chunks = 0;
                self.rx_buf.clear();
            }

            if self.rx_seq != Some(seq)
                || self.rx_expected_chunks != total_chunks
                || chunk_idx != self.rx_received_chunks
            {
                self.reset_rx();
                return Err(TelemetryError::HandlerError("out-of-order can fragments"));
            }

            self.rx_buf.extend_from_slice(&frame.data[4..4 + chunk_len]);
            if self.rx_buf.len() > MAX_PACKET_SIZE {
                self.reset_rx();
                return Err(TelemetryError::HandlerError("can packet exceeds max size"));
            }

            self.rx_received_chunks = self.rx_received_chunks.saturating_add(1);
            if self.rx_received_chunks < self.rx_expected_chunks {
                return Ok(());
            }

            let packet = std::mem::take(&mut self.rx_buf);
            self.reset_rx();
            return router.rx_serialized_queue_from_side(&packet, side_id);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = router;
            let _ = side_id;
            Err(TelemetryError::HandlerError(
                "can comms support is only implemented on Linux",
            ))
        }
    }

    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        if payload.is_empty() || payload.len() > MAX_PACKET_SIZE {
            return Err(
                format!("packet too large to send over can: {} bytes", payload.len()).into(),
            );
        }

        #[cfg(target_os = "linux")]
        {
            let total_chunks = payload.len().div_ceil(4) as u8;
            let seq = self.tx_seq;
            self.tx_seq = self.tx_seq.wrapping_add(1);

            for (chunk_idx, chunk) in payload.chunks(4).enumerate() {
                let mut frame = CanFrame::default();
                frame.can_id = self.tx_id;
                frame.can_dlc = (4 + chunk.len()) as u8;
                frame.data[0] = seq;
                frame.data[1] = chunk_idx as u8;
                frame.data[2] = total_chunks;
                frame.data[3] = chunk.len() as u8;
                frame.data[4..4 + chunk.len()].copy_from_slice(chunk);
                can_write_frame(self.inner.as_raw_fd(), &frame)?;
            }

            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = payload;
            Err("can comms support is only implemented on Linux".into())
        }
    }

    fn set_side_id(&mut self, side_id: RouterSideId) {
        self.side_id = Some(side_id);
    }
}

// ======================================================================
//  Dummy Comms (fallback when hardware missing)
// ======================================================================
#[cfg(any(feature = "testing", feature = "hitl_mode", feature = "test_fire_mode"))]
#[derive(Debug)]
pub struct DummyComms {
    name: &'static str,
    side_id: Option<RouterSideId>,
    #[cfg(feature = "testing")]
    discovery_next_announce_ms: u64,
    #[cfg(feature = "testing")]
    pending_rx: std::collections::VecDeque<sedsprintf_rs_2026::packet::Packet>,
}

#[cfg(any(feature = "testing", feature = "hitl_mode", feature = "test_fire_mode"))]
impl DummyComms {
    pub fn new(name: &'static str) -> Self {
        DummyComms {
            name,
            side_id: None,
            #[cfg(feature = "testing")]
            discovery_next_announce_ms: 0,
            #[cfg(feature = "testing")]
            pending_rx: std::collections::VecDeque::new(),
        }
    }

    #[cfg(feature = "testing")]
    fn simulated_discovery_sender(&self) -> &'static str {
        match self.name {
            "Rocket Comms" => crate::types::Board::RFBoard.sender_id(),
            "Umbilical Comms" => crate::types::Board::GatewayBoard.sender_id(),
            _ => crate::types::Board::GroundStation.sender_id(),
        }
    }

    #[cfg(feature = "testing")]
    fn simulated_discovery_endpoints(&self) -> &'static [sedsprintf_rs_2026::config::DataEndpoint] {
        use sedsprintf_rs_2026::config::DataEndpoint;

        match self.name {
            "Rocket Comms" => &[
                DataEndpoint::FlightController,
                DataEndpoint::FlightState,
                DataEndpoint::SdCard,
            ],
            "Umbilical Comms" => &[
                DataEndpoint::ValveBoard,
                DataEndpoint::ActuatorBoard,
                DataEndpoint::Abort,
            ],
            _ => &[],
        }
    }

    #[cfg(feature = "testing")]
    fn simulated_timesync_sources(&self) -> &'static [&'static str] {
        match self.name {
            "Rocket Comms" => DUMMY_ROCKET_TIMESYNC_SOURCES,
            "Umbilical Comms" => DUMMY_UMBILICAL_TIMESYNC_SOURCES,
            _ => &[],
        }
    }

    #[cfg(feature = "testing")]
    fn maybe_queue_discovery(&mut self) -> TelemetryResult<()> {
        let now_ms = crate::telemetry_task::get_current_timestamp_ms();
        if now_ms < self.discovery_next_announce_ms {
            return Ok(());
        }

        let sender = self.simulated_discovery_sender();
        let endpoints = self.simulated_discovery_endpoints();
        if !endpoints.is_empty() {
            self.pending_rx
                .push_back(sedsprintf_rs_2026::discovery::build_discovery_announce(
                    sender, now_ms, endpoints,
                )?);
        }
        let timesync_sources = self.simulated_timesync_sources();
        if !timesync_sources.is_empty() {
            self.pending_rx.push_back(
                sedsprintf_rs_2026::discovery::build_discovery_timesync_sources(
                    sender,
                    now_ms,
                    timesync_sources,
                )?,
            );
        }

        self.discovery_next_announce_ms =
            now_ms.saturating_add(sedsprintf_rs_2026::discovery::DISCOVERY_SLOW_INTERVAL_MS);
        Ok(())
    }
}

#[cfg(any(feature = "testing", feature = "hitl_mode", feature = "test_fire_mode"))]
impl CommsDevice for DummyComms {
    fn recv_packet(&mut self, _router: &Router) -> TelemetryResult<()> {
        #[cfg(feature = "testing")]
        {
            let side_id = self
                .side_id
                .ok_or(TelemetryError::HandlerError("comms side id not set"))?;
            self.maybe_queue_discovery()?;
            if let Some(pkt) = self.pending_rx.pop_front() {
                return _router.rx_queue_from_side(pkt, side_id);
            }
            let pkt = get_dummy_packet()?;
            _router.rx_queue_from_side(pkt, side_id)
        }

        #[cfg(not(feature = "testing"))]
        {
            let _ = _router;
            // In hitl_mode, dummy comms links are used only as disconnected-link placeholders.
            Ok(())
        }
    }

    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        use sedsprintf_rs_2026::config::DataType;
        use sedsprintf_rs_2026::serialize::peek_envelope;

        if peek_envelope(payload).unwrap().ty == DataType::Heartbeat {
            return Ok(());
        }
        static LAST_LOG_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static LOG_INTERVAL_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        static LOG_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

        let enabled = *LOG_ENABLED
            .get_or_init(|| std::env::var("GS_DUMMY_RADIO_LOG").ok().as_deref() != Some("0"));
        if enabled {
            let interval_ms = *LOG_INTERVAL_MS.get_or_init(|| {
                std::env::var("GS_DUMMY_RADIO_LOG_INTERVAL_MS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(60_000)
                    .clamp(1_000, 3_600_000)
            });
            let now_ms = crate::telemetry_task::get_current_timestamp_ms();
            let prev = LAST_LOG_MS.load(std::sync::atomic::Ordering::Relaxed);
            if now_ms.saturating_sub(prev) >= interval_ms {
                LAST_LOG_MS.store(now_ms, std::sync::atomic::Ordering::Relaxed);
                println!(
                    "DummyComms: dropping {} bytes of outgoing telemetry send from {}",
                    payload.len(),
                    self.name
                );
            }
        }
        Ok(())
    }

    fn set_side_id(&mut self, side_id: RouterSideId) {
        self.side_id = Some(side_id);
    }
}

#[cfg(target_os = "linux")]
const SPI_IOC_MAGIC: u8 = b'k';
#[cfg(target_os = "linux")]
const I2C_M_RD: u16 = 0x0001;
#[cfg(target_os = "linux")]
const I2C_RDWR: libc::c_ulong = 0x0707;
#[cfg(target_os = "linux")]
const SPI_IOC_WR_MODE: libc::c_ulong = ioc_write::<u8>(SPI_IOC_MAGIC, 1);
#[cfg(target_os = "linux")]
const SPI_IOC_WR_BITS_PER_WORD: libc::c_ulong = ioc_write::<u8>(SPI_IOC_MAGIC, 3);
#[cfg(target_os = "linux")]
const SPI_IOC_WR_MAX_SPEED_HZ: libc::c_ulong = ioc_write::<u32>(SPI_IOC_MAGIC, 4);

#[cfg(target_os = "linux")]
const IOC_NRBITS: u32 = 8;
#[cfg(target_os = "linux")]
const IOC_TYPEBITS: u32 = 8;
#[cfg(target_os = "linux")]
const IOC_SIZEBITS: u32 = 14;
#[cfg(target_os = "linux")]
const IOC_NRSHIFT: u32 = 0;
#[cfg(target_os = "linux")]
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
#[cfg(target_os = "linux")]
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
#[cfg(target_os = "linux")]
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
#[cfg(target_os = "linux")]
const IOC_WRITE: u32 = 1;

#[cfg(target_os = "linux")]
const CAN_EFF_FLAG: u32 = 0x8000_0000;
#[cfg(target_os = "linux")]
const CAN_ERR_FLAG: u32 = 0x2000_0000;
#[cfg(target_os = "linux")]
const PF_CAN_VALUE: libc::c_int = 29;
#[cfg(target_os = "linux")]
const AF_CAN_VALUE: libc::sa_family_t = 29;
#[cfg(target_os = "linux")]
const CAN_RAW_PROTOCOL: libc::c_int = 1;

#[cfg(target_os = "linux")]
#[repr(C)]
struct SpiIocTransfer {
    tx_buf: u64,
    rx_buf: u64,
    len: u32,
    speed_hz: u32,
    delay_usecs: u16,
    bits_per_word: u8,
    cs_change: u8,
    tx_nbits: u8,
    rx_nbits: u8,
    word_delay_usecs: u8,
    pad: u8,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrCan {
    can_family: libc::sa_family_t,
    can_ifindex: libc::c_int,
    addr: [u8; 8],
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct I2cMsg {
    addr: u16,
    flags: u16,
    len: u16,
    buf: *mut u8,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct I2cRdwrIoctlData {
    msgs: *mut I2cMsg,
    nmsgs: u32,
}

#[cfg(target_os = "linux")]
fn is_i2c_idle_read_error(err: &(dyn Error + 'static)) -> bool {
    err.downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
        .is_some_and(|code| {
            code == libc::ETIMEDOUT || code == libc::EREMOTEIO || code == libc::ENXIO
        })
}

#[cfg(target_os = "linux")]
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_i2c_slot_roundtrip() {
        let raw = encode_i2c_slot(I2C_KIND_DATA, I2C_FLAG_START, 7, 0, 99, b"hello");
        let decoded = decode_i2c_slot(&raw).unwrap().unwrap();
        assert_eq!(decoded.kind, I2C_KIND_DATA);
        assert_eq!(decoded.flags, I2C_FLAG_START);
        assert_eq!(decoded.transfer_id, 7);
        assert_eq!(decoded.offset, 0);
        assert_eq!(decoded.total_len, 99);
        assert_eq!(decoded.data, b"hello");
    }

    #[test]
    fn i2c_rx_assembly_reassembles_multislot_transfer() {
        let first = decode_i2c_slot(&encode_i2c_slot(
            I2C_KIND_DATA,
            I2C_FLAG_START,
            11,
            0,
            20,
            b"abcdefghijklmn",
        ))
        .unwrap()
        .unwrap();
        let second = decode_i2c_slot(&encode_i2c_slot(
            I2C_KIND_DATA,
            I2C_FLAG_END,
            11,
            14,
            20,
            b"opqrst",
        ))
        .unwrap()
        .unwrap();

        let mut assembly = I2cRxAssembly::new(&first).unwrap();
        let payload = assembly.push(&second).unwrap().unwrap();
        assert_eq!(payload, b"abcdefghijklmnopqrst");
    }

    #[test]
    fn timed_out_i2c_read_is_treated_as_idle() {
        let err = std::io::Error::from_raw_os_error(libc::ETIMEDOUT);
        assert!(is_i2c_idle_read_error(&err));
    }
}

#[cfg(test)]
mod raw_uart_tests {
    use super::*;

    #[test]
    fn raw_uart_frame_roundtrip() {
        let payload = vec![1, 2, 3, 4, 5];
        let mut framed = build_raw_uart_frame(&payload).unwrap();
        let decoded = take_raw_uart_framed_payload(&mut framed).unwrap().unwrap();
        assert_eq!(decoded, payload);
        assert!(framed.is_empty());
    }

    #[test]
    fn raw_uart_frame_resyncs_after_garbage() {
        let payload = vec![9, 8, 7];
        let mut framed = vec![0x00, 0x11, 0x22];
        framed.extend_from_slice(&build_raw_uart_frame(&payload).unwrap());
        let decoded = take_raw_uart_framed_payload(&mut framed).unwrap().unwrap();
        assert_eq!(decoded, payload);
        assert!(framed.is_empty());
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CanFrame {
    can_id: u32,
    can_dlc: u8,
    __pad: u8,
    __res0: u8,
    __res1: u8,
    data: [u8; 8],
}

#[cfg(target_os = "linux")]
const fn ioc_write<T>(kind: u8, nr: u8) -> libc::c_ulong {
    ((IOC_WRITE << IOC_DIRSHIFT)
        | ((kind as u32) << IOC_TYPESHIFT)
        | ((nr as u32) << IOC_NRSHIFT)
        | ((size_of::<T>() as u32) << IOC_SIZESHIFT)) as libc::c_ulong
}

#[cfg(target_os = "linux")]
const fn spi_ioc_message_request(count: usize) -> libc::c_ulong {
    ((IOC_WRITE << IOC_DIRSHIFT)
        | ((SPI_IOC_MAGIC as u32) << IOC_TYPESHIFT)
        | ((0u32) << IOC_NRSHIFT)
        | (((count * size_of::<SpiIocTransfer>()) as u32) << IOC_SIZESHIFT)) as libc::c_ulong
}

#[cfg(target_os = "linux")]
fn spi_ioctl_write<T>(fd: RawFd, request: libc::c_ulong, value: &T) -> std::io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, request as _, value) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_can_socket(ifname: &str) -> anyhow::Result<RawFd> {
    let fd = unsafe { libc::socket(PF_CAN_VALUE, libc::SOCK_RAW, CAN_RAW_PROTOCOL) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("socket(PF_CAN) failed");
    }

    let timeout = libc::timeval {
        tv_sec: 0,
        tv_usec: 200_000,
    };
    let timeout_rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &timeout as *const _ as *const _,
            size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if timeout_rc < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err).context("setsockopt(SO_RCVTIMEO) failed");
    }

    let ifname_c = CString::new(ifname).context("CAN interface contains NUL byte")?;
    let ifindex = unsafe { libc::if_nametoindex(ifname_c.as_ptr()) };
    if ifindex == 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err).context(format!("unknown CAN interface {ifname}"));
    }

    let addr = SockAddrCan {
        can_family: AF_CAN_VALUE,
        can_ifindex: ifindex as libc::c_int,
        addr: [0u8; 8],
    };
    let rc = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            size_of::<SockAddrCan>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err).context(format!("bind CAN interface {ifname} failed"));
    }

    Ok(fd)
}

#[cfg(target_os = "linux")]
fn can_read_frame(fd: RawFd) -> std::io::Result<Option<CanFrame>> {
    let mut frame = CanFrame::default();
    let rc = unsafe {
        libc::read(
            fd,
            &mut frame as *mut _ as *mut libc::c_void,
            size_of::<CanFrame>(),
        )
    };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        return match err.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Ok(None),
            _ => Err(err),
        };
    }
    if rc == 0 {
        return Ok(None);
    }
    if rc as usize != size_of::<CanFrame>() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "short CAN frame read",
        ));
    }
    Ok(Some(frame))
}

#[cfg(target_os = "linux")]
fn can_write_frame(fd: RawFd, frame: &CanFrame) -> std::io::Result<()> {
    let rc = unsafe {
        libc::write(
            fd,
            frame as *const _ as *const libc::c_void,
            size_of::<CanFrame>(),
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if rc as usize != size_of::<CanFrame>() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "short CAN frame write",
        ));
    }
    Ok(())
}
