use crate::comms_config::{
    CanLinkConfig, CommsLinkConfig, I2cLinkConfig, SerialLinkConfig, SpiLinkConfig,
};
#[cfg(feature = "testing")]
use crate::dummy_packets::get_dummy_packet;
use anyhow::Context;
use sedsprintf_rs_2026::{
    TelemetryError, TelemetryResult,
    router::{Router, RouterSideId},
};
use serial::{SerialPort, SystemPort};
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
use std::time::Duration;

pub const ROCKET_COMMS_PORT: &str = "/dev/ttyUSB1";
pub const UMBILICAL_COMMS_PORT: &str = "/dev/ttyUSB2";
pub const COMMS_BAUD_RATE: usize = 57_600;
pub const MAX_PACKET_SIZE: usize = 256;
#[cfg(target_os = "linux")]
const I2C_FRAME_SIZE: usize = 258;
#[cfg(target_os = "linux")]
const I2C_CHUNK_SIZE: usize = 32;
#[cfg(target_os = "linux")]
const I2C_REQ_DATA_MAGIC: u8 = 0xA5;
#[cfg(target_os = "linux")]
const I2C_RESP_DATA_MAGIC: u8 = 0x5A;

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
        CommsLinkConfig::UsbSerial { serial } => serial_description("usb_serial", serial),
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
        CommsLinkConfig::UsbSerial { serial }
        | CommsLinkConfig::RaspberryPiGpioUart { serial }
        | CommsLinkConfig::CustomSerial { serial } => {
            Ok(Box::new(UartComms::open(&serial.port, serial.baud_rate)?))
        }
        CommsLinkConfig::Spi { spi } => Ok(Box::new(SpiComms::open(spi)?)),
        CommsLinkConfig::Can { can } => Ok(Box::new(CanComms::open(can)?)),
        CommsLinkConfig::I2c { i2c } => Ok(Box::new(I2cComms::open(i2c)?)),
    }
}

pub fn startup_failure_hint(cfg: &CommsLinkConfig) -> String {
    match cfg {
        CommsLinkConfig::UsbSerial { serial } | CommsLinkConfig::CustomSerial { serial } => {
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
        "interface={name} port={} baud_rate={}",
        serial.port, serial.baud_rate
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
//  Real Radio Implementation
// ======================================================================
pub struct UartComms {
    inner: SystemPort,
    side_id: Option<RouterSideId>,
}

impl UartComms {
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
        Ok(Self {
            inner,
            side_id: None,
        })
    }
}

impl CommsDevice for UartComms {
    /// Blocking receive of one Packet
    fn recv_packet(&mut self, router: &Router) -> TelemetryResult<()> {
        let side_id = self
            .side_id
            .ok_or(TelemetryError::HandlerError("radio side id not set"))?;

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

        router.rx_serialized_queue_from_side(&payload, side_id)
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

    fn set_side_id(&mut self, side_id: RouterSideId) {
        self.side_id = Some(side_id);
    }
}

// ======================================================================
// I2C Radio Implementation
// ======================================================================

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
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = cfg;
            anyhow::bail!("I2C radio support is only implemented on Linux")
        }
    }

    #[cfg(target_os = "linux")]
    fn read_frame(&mut self) -> Result<Option<Vec<u8>>, Box<dyn Error + Send + Sync>> {
        let mut raw = [0u8; I2C_FRAME_SIZE];
        let mut offset = 0usize;
        while offset < I2C_FRAME_SIZE {
            let read_len = (I2C_FRAME_SIZE - offset).min(I2C_CHUNK_SIZE);
            self.transfer_read(&mut raw[offset..offset + read_len])?;
            offset += read_len;
            if offset < I2C_FRAME_SIZE {
                std::thread::sleep(self.chunk_delay);
            }
        }
        parse_i2c_response(&raw)
    }

    #[cfg(target_os = "linux")]
    fn write_frame(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        let payload = &payload[..payload.len().min(MAX_PACKET_SIZE)];
        let mut frame = Vec::with_capacity(payload.len() + 2);
        frame.push(I2C_REQ_DATA_MAGIC);
        frame.push(payload.len() as u8);
        frame.extend_from_slice(payload);

        for (idx, chunk) in frame.chunks(I2C_CHUNK_SIZE).enumerate() {
            self.transfer_write(chunk)?;
            if idx + 1 < frame.len().div_ceil(I2C_CHUNK_SIZE) {
                std::thread::sleep(self.chunk_delay);
            }
        }
        Ok(())
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
            .ok_or(TelemetryError::HandlerError("radio side id not set"))?;

        #[cfg(target_os = "linux")]
        {
            match self.read_frame() {
                Ok(Some(payload)) => router.rx_serialized_queue_from_side(&payload, side_id),
                Ok(None) => Ok(()),
                Err(_) => Err(TelemetryError::HandlerError("i2c receive failed")),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = router;
            let _ = side_id;
            Err(TelemetryError::HandlerError(
                "i2c radio support is only implemented on Linux",
            ))
        }
    }

    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        if payload.is_empty() || payload.len() > MAX_PACKET_SIZE {
            return Err(
                format!("packet too large to send over i2c: {} bytes", payload.len()).into(),
            );
        }

        #[cfg(target_os = "linux")]
        {
            self.write_frame(payload)?;
            if !self.initial_wait.is_zero() {
                std::thread::sleep(self.initial_wait);
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = payload;
            Err("i2c radio support is only implemented on Linux".into())
        }
    }

    fn set_side_id(&mut self, side_id: RouterSideId) {
        self.side_id = Some(side_id);
    }
}

// ======================================================================
// SPI Radio Implementation
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
            anyhow::bail!("SPI radio support is only implemented on Linux")
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
            .ok_or(TelemetryError::HandlerError("radio side id not set"))?;

        #[cfg(target_os = "linux")]
        {
            let mut tx = vec![0u8; MAX_PACKET_SIZE + 2];
            let mut rx = vec![0u8; MAX_PACKET_SIZE + 2];
            self.transfer(&tx, &mut rx)
                .map_err(|_| TelemetryError::HandlerError("spi transfer failed"))?;
            tx.clear();

            let frame_len = u16::from_le_bytes([rx[0], rx[1]]) as usize;
            if frame_len == 0 {
                return Ok(());
            }
            if frame_len > MAX_PACKET_SIZE {
                return Err(TelemetryError::HandlerError(
                    "invalid frame length from spi",
                ));
            }

            let payload = &rx[2..2 + frame_len];
            return router.rx_serialized_queue_from_side(payload, side_id);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = router;
            let _ = side_id;
            Err(TelemetryError::HandlerError(
                "spi radio support is only implemented on Linux",
            ))
        }
    }

    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        if payload.is_empty() || payload.len() > u16::MAX as usize {
            return Err(
                format!("packet too large to send over spi: {} bytes", payload.len()).into(),
            );
        }

        #[cfg(target_os = "linux")]
        {
            let mut tx = Vec::with_capacity(payload.len() + 2);
            tx.extend_from_slice(&(payload.len() as u16).to_le_bytes());
            tx.extend_from_slice(payload);
            let mut rx = vec![0u8; tx.len()];
            self.transfer(&tx, &mut rx)?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = payload;
            Err("spi radio support is only implemented on Linux".into())
        }
    }

    fn set_side_id(&mut self, side_id: RouterSideId) {
        self.side_id = Some(side_id);
    }
}

// ======================================================================
// CAN Radio Implementation
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
            anyhow::bail!("CAN radio support is only implemented on Linux")
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
            .ok_or(TelemetryError::HandlerError("radio side id not set"))?;

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
                "can radio support is only implemented on Linux",
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
            Err("can radio support is only implemented on Linux".into())
        }
    }

    fn set_side_id(&mut self, side_id: RouterSideId) {
        self.side_id = Some(side_id);
    }
}

// ======================================================================
//  Dummy Radio (fallback when hardware missing)
// ======================================================================
#[cfg(any(feature = "testing", feature = "hitl_mode"))]
#[derive(Debug)]
pub struct DummyComms {
    name: &'static str,
    side_id: Option<RouterSideId>,
}

#[cfg(any(feature = "testing", feature = "hitl_mode"))]

impl DummyComms {
    pub fn new(name: &'static str) -> Self {
        DummyComms {
            name,
            side_id: None,
        }
    }
}

#[cfg(any(feature = "testing", feature = "hitl_mode"))]
impl CommsDevice for DummyComms {
    fn recv_packet(&mut self, _router: &Router) -> TelemetryResult<()> {
        #[cfg(feature = "testing")]
        {
            let side_id = self
                .side_id
                .ok_or(TelemetryError::HandlerError("radio side id not set"))?;
            let pkt = get_dummy_packet()?;
            return _router.rx_queue_from_side(pkt, side_id);
        }

        #[cfg(not(feature = "testing"))]
        {
            let _ = _router;
            // In hitl_mode, dummy radios are used only as disconnected-link placeholders.
            return Ok(());
        }

        #[allow(unreachable_code)]
        Ok(())
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
fn parse_i2c_response(
    raw: &[u8; I2C_FRAME_SIZE],
) -> Result<Option<Vec<u8>>, Box<dyn Error + Send + Sync>> {
    let magic = raw[0];
    let len = raw[1] as usize;

    // Treat all-0xff and all-zero idle reads as "no frame available yet" to match the
    // permissive Python polling behavior used by the Pico groundstation tools.
    let all_ff = raw.iter().all(|byte| *byte == 0xFF);
    let all_zero = raw.iter().all(|byte| *byte == 0x00);
    if all_ff || all_zero {
        return Ok(None);
    }

    if magic != I2C_RESP_DATA_MAGIC || len > MAX_PACKET_SIZE {
        return Ok(None);
    }

    if len == 0 {
        return Ok(None);
    }

    Ok(Some(raw[2..2 + len].to_vec()))
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
