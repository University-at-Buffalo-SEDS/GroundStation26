//! Optional recovery radio with bounded reconnects and independent RX/TX handles.
use crate::comms::{CommsDevice, RadioWindowUpdate, open_link};
use crate::comms_config::CommsLinkConfig;
use sedsnet::{
    TelemetryError, TelemetryResult,
    packet::Packet,
    router::{Router, RouterSideId},
};
use std::{
    error::Error,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
type Device = Arc<Mutex<Box<dyn CommsDevice>>>;
type Opener = Box<dyn Fn(&CommsLinkConfig) -> anyhow::Result<Box<dyn CommsDevice>> + Send>;
struct Connection {
    config: CommsLinkConfig,
    opener: Opener,
    generation: u64,
    retry_at: Instant,
    rx: Option<Device>,
    tx: Option<Device>,
    failures: u64,
}
pub struct RecoveryRadio {
    shared: Arc<Mutex<Connection>>,
    tx: bool,
    side: Option<RouterSideId>,
}
impl RecoveryRadio {
    pub fn new(config: CommsLinkConfig) -> Self {
        Self {
            shared: Arc::new(Mutex::new(Connection {
                config,
                opener: Box::new(open_link),
                generation: 0,
                retry_at: Instant::now(),
                rx: None,
                tx: None,
                failures: 0,
            })),
            tx: false,
            side: None,
        }
    }
    fn connect(&self) -> Option<(u64, Device)> {
        let mut state = self.shared.lock().unwrap();
        if state.rx.is_none() && Instant::now() >= state.retry_at {
            state.retry_at = Instant::now() + Duration::from_secs(1);
            match (state.opener)(&state.config).and_then(|mut rx| {
                rx.enable_radio_capacity_control();
                let tx = rx
                    .try_clone_tx()?
                    .ok_or_else(|| anyhow::anyhow!("RRLS requires independent UART transmit"))?;
                Ok((rx, tx))
            }) {
                Ok((rx, tx)) => {
                    state.generation += 1;
                    state.rx = Some(Arc::new(Mutex::new(rx)));
                    state.tx = Some(Arc::new(Mutex::new(tx)));
                    log::info!(
                        "RRLS radio connected: {}",
                        crate::comms::link_description(&state.config)
                    );
                }
                Err(error) => {
                    state.failures += 1;
                    if state.failures == 1 || state.failures % 30 == 0 {
                        log::warn!(
                            "RRLS radio {} unavailable; retrying once per second: {error:#}",
                            crate::comms::link_description(&state.config)
                        );
                    }
                }
            }
        }
        let device = if self.tx { &state.tx } else { &state.rx };
        device.clone().map(|device| (state.generation, device))
    }
    fn disconnected(&self, generation: u64) {
        let mut state = self.shared.lock().unwrap();
        if state.generation == generation {
            // Endpoints do not retain idle clones of failed UART handles.
            state.rx = None;
            state.tx = None;
            state.retry_at = Instant::now() + Duration::from_secs(1);
        }
    }
    fn with_device<T, E>(
        &self,
        offline: E,
        retry: impl FnOnce(&E) -> bool,
        op: impl FnOnce(&mut dyn CommsDevice) -> Result<T, E>,
    ) -> Result<T, E> {
        let Some((generation, device)) = self.connect() else {
            std::thread::sleep(Duration::from_millis(10));
            return Err(offline);
        };
        let result = {
            let mut device = device.lock().unwrap();
            if let Some(side) = self.side {
                device.set_side_id(side);
            }
            op(&mut **device)
        };
        drop(device);
        if result.as_ref().err().is_some_and(retry) {
            self.disconnected(generation);
        }
        result
    }
}
fn io_failure(error: &TelemetryError) -> bool {
    matches!(
        error,
        TelemetryError::Io(_) | TelemetryError::GenericError(_)
    )
}
impl CommsDevice for RecoveryRadio {
    fn service_transmit(&mut self) -> TelemetryResult<()> {
        self.with_device(
            TelemetryError::Io("RRLS radio disconnected"),
            io_failure,
            |device| device.service_transmit(),
        )
    }
    fn transmit_ready(&self) -> bool {
        let device = {
            let state = self.shared.lock().unwrap();
            if self.tx {
                state.tx.clone()
            } else {
                state.rx.clone()
            }
        };
        device.is_none_or(|device| device.lock().unwrap().transmit_ready())
    }

    fn try_clone_tx(&self) -> std::io::Result<Option<Box<dyn CommsDevice>>> {
        Ok(Some(Box::new(Self {
            shared: self.shared.clone(),
            tx: true,
            side: self.side,
        })))
    }
    fn set_side_id(&mut self, side: RouterSideId) {
        self.side = Some(side);
    }
    fn recv_packet(
        &mut self,
        router: &Router,
        tap: &mut dyn FnMut(&Packet),
    ) -> TelemetryResult<()> {
        self.with_device(
            TelemetryError::Io("RRLS radio disconnected"),
            io_failure,
            |device| device.recv_packet(router, tap),
        )
    }
    fn recv_serialized_packets(&mut self, sink: &mut dyn FnMut(Vec<u8>)) -> TelemetryResult<()> {
        self.recv_serialized_packets_with_budget(sink, Duration::from_millis(50), usize::MAX)
    }
    fn recv_serialized_packets_with_timeout(
        &mut self,
        sink: &mut dyn FnMut(Vec<u8>),
        timeout: Duration,
    ) -> TelemetryResult<()> {
        self.recv_serialized_packets_with_budget(sink, timeout, usize::MAX)
    }
    fn recv_serialized_packets_with_budget(
        &mut self,
        sink: &mut dyn FnMut(Vec<u8>),
        timeout: Duration,
        max: usize,
    ) -> TelemetryResult<()> {
        if self.connect().is_none() {
            std::thread::sleep(Duration::from_millis(10));
            return Err(TelemetryError::Io("RRLS radio disconnected"));
        }
        self.with_device(
            TelemetryError::Io("RRLS radio disconnected"),
            io_failure,
            |device| device.recv_serialized_packets_with_budget(sink, timeout, max),
        )
    }
    fn send_data(&mut self, payload: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.send_data_with_priority(payload, 0)
    }
    fn send_data_with_priority(
        &mut self,
        payload: &[u8],
        priority: u8,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.with_device(
            "RRLS radio disconnected".into(),
            |error| {
                !error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    )
                })
            },
            |device| device.send_data_with_priority(payload, priority),
        )
    }
    fn send_radio_scheduler_status(
        &mut self,
        seq: u8,
        more: bool,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.with_device(
            "RRLS radio disconnected".into(),
            |_| true,
            |device| device.send_radio_scheduler_status(seq, more),
        )
    }
    fn take_radio_window_update(&mut self) -> Option<RadioWindowUpdate> {
        self.with_device(
            (),
            |_| false,
            |device| Ok(device.take_radio_window_update()),
        )
        .ok()
        .flatten()
    }
    fn receive_budget_exhausted(&self) -> bool {
        let device = {
            let state = self.shared.lock().unwrap();
            if self.tx {
                state.tx.clone()
            } else {
                state.rx.clone()
            }
        };
        device.is_some_and(|device| device.lock().unwrap().receive_budget_exhausted())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::comms_config::{SerialLinkConfig, SerialProtocol};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    fn missing() -> RecoveryRadio {
        RecoveryRadio::new(CommsLinkConfig::Serial {
            serial: SerialLinkConfig {
                port: "/missing-rrls-radio-for-test".into(),
                baud_rate: 115200,
                protocol: SerialProtocol::RawUart,
            },
        })
    }
    #[test]
    fn absent_radio_is_bounded_and_does_not_panic_or_fake_success() {
        let mut radio = missing();
        let started = Instant::now();
        let mut tx = radio.try_clone_tx().unwrap().unwrap();
        for _ in 0..100 {
            assert!(
                radio
                    .recv_serialized_packets_with_budget(
                        &mut |_| panic!("no fake data"),
                        Duration::ZERO,
                        1
                    )
                    .is_err()
            );
            assert!(tx.send_data(&[1]).is_err());
        }
        let attempts = radio.shared.lock().unwrap().failures;
        assert!(
            attempts >= 1 && attempts <= 1 + started.elapsed().as_secs(),
            "retry storm: {attempts}"
        );
    }
    struct TestRadio {
        online: Arc<AtomicBool>,
        ready: Arc<AtomicBool>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        alive: Arc<AtomicUsize>,
    }
    impl TestRadio {
        fn copy(&self) -> Self {
            self.alive.fetch_add(1, Ordering::Relaxed);
            Self {
                online: self.online.clone(),
                ready: self.ready.clone(),
                sent: self.sent.clone(),
                alive: self.alive.clone(),
            }
        }
    }
    impl Drop for TestRadio {
        fn drop(&mut self) {
            self.alive.fetch_sub(1, Ordering::Relaxed);
        }
    }
    impl CommsDevice for TestRadio {
        fn transmit_ready(&self) -> bool {
            self.ready.load(Ordering::Relaxed)
        }
        fn try_clone_tx(&self) -> std::io::Result<Option<Box<dyn CommsDevice>>> {
            Ok(Some(Box::new(self.copy())))
        }
        fn set_side_id(&mut self, _: RouterSideId) {}
        fn recv_packet(&mut self, _: &Router, _: &mut dyn FnMut(&Packet)) -> TelemetryResult<()> {
            Ok(())
        }
        fn send_data(&mut self, data: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
            if !self.online.load(Ordering::Relaxed) {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into());
            }
            if !self.ready.load(Ordering::Relaxed) {
                return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock).into());
            }
            self.sent.lock().unwrap().push(data.to_vec());
            Ok(())
        }
        fn recv_serialized_packets_with_budget(
            &mut self,
            sink: &mut dyn FnMut(Vec<u8>),
            _: Duration,
            _: usize,
        ) -> TelemetryResult<()> {
            sink(vec![9]);
            Ok(())
        }
    }
    #[test]
    fn attach_receive_transmit_and_reconnect_release_idle_handles() {
        let mut radio = missing();
        let mut tx = radio.try_clone_tx().unwrap().unwrap();
        let online = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let alive = Arc::new(AtomicUsize::new(0));
        let (o, s, a) = (online.clone(), sent.clone(), alive.clone());
        radio.shared.lock().unwrap().opener = Box::new(move |_| {
            if !o.load(Ordering::Relaxed) {
                anyhow::bail!("radio absent")
            }
            a.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(TestRadio {
                online: o.clone(),
                ready: Arc::new(AtomicBool::new(true)),
                sent: s.clone(),
                alive: a.clone(),
            }))
        });
        assert!(radio.connect().is_none());
        online.store(true, Ordering::Relaxed);
        radio.shared.lock().unwrap().retry_at = Instant::now();
        tx.send_data(&[1, 2]).unwrap();
        let mut received = vec![];
        radio
            .recv_serialized_packets_with_budget(&mut |p| received.push(p), Duration::ZERO, 1)
            .unwrap();
        assert_eq!(received, vec![vec![9]]);
        assert_eq!(*sent.lock().unwrap(), vec![vec![1, 2]]);
        {
            let state = radio.shared.lock().unwrap();
            assert!(!Arc::ptr_eq(
                state.rx.as_ref().unwrap(),
                state.tx.as_ref().unwrap()
            ));
        }
        assert_eq!(alive.load(Ordering::Relaxed), 2);
        online.store(false, Ordering::Relaxed);
        assert!(tx.send_data(&[3]).is_err());
        assert_eq!(
            alive.load(Ordering::Relaxed),
            0,
            "even an idle RX endpoint releases failed UART handles"
        );
        online.store(true, Ordering::Relaxed);
        radio.shared.lock().unwrap().retry_at = Instant::now();
        tx.send_data(&[4, 5]).unwrap();
        assert_eq!(*sent.lock().unwrap(), vec![vec![1, 2], vec![4, 5]]);
        let old = radio.shared.lock().unwrap().generation - 1;
        radio.disconnected(old);
        assert!(
            radio.connect().is_some(),
            "late failure must not close new session"
        );
    }
    #[test]
    fn reconnecting_writer_preserves_trusted_priority_metadata() {
        struct PrioritySpy(Arc<Mutex<Vec<(u8, Vec<u8>)>>>);
        impl CommsDevice for PrioritySpy {
            fn try_clone_tx(&self) -> std::io::Result<Option<Box<dyn CommsDevice>>> {
                Ok(Some(Box::new(Self(self.0.clone()))))
            }
            fn set_side_id(&mut self, _: RouterSideId) {}
            fn recv_packet(
                &mut self,
                _: &Router,
                _: &mut dyn FnMut(&Packet),
            ) -> TelemetryResult<()> {
                Ok(())
            }
            fn send_data(&mut self, _: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
                panic!("recovery wrapper discarded trusted priority")
            }
            fn send_data_with_priority(
                &mut self,
                bytes: &[u8],
                priority: u8,
            ) -> Result<(), Box<dyn Error + Send + Sync>> {
                self.0.lock().unwrap().push((priority, bytes.to_vec()));
                Ok(())
            }
        }
        let radio = missing();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sent = observed.clone();
        radio.shared.lock().unwrap().opener =
            Box::new(move |_| Ok(Box::new(PrioritySpy(sent.clone()))));
        let mut writer = radio.try_clone_tx().unwrap().unwrap();
        writer.send_data_with_priority(&[1, 2, 3], 200).unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![(200, vec![1, 2, 3])]);
    }
    #[test]
    fn uart_backpressure_keeps_recovery_radio_connected_and_can_resume() {
        let radio = missing();
        let ready = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let alive = Arc::new(AtomicUsize::new(0));
        let (r, s, a) = (ready.clone(), sent.clone(), alive.clone());
        radio.shared.lock().unwrap().opener = Box::new(move |_| {
            a.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(TestRadio {
                online: Arc::new(AtomicBool::new(true)),
                ready: r.clone(),
                sent: s.clone(),
                alive: a.clone(),
            }))
        });
        assert!(radio.connect().is_some());
        let mut tx = radio.try_clone_tx().unwrap().unwrap();
        assert!(!tx.transmit_ready());
        assert!(tx.send_data(&[7]).is_err());
        assert_eq!(alive.load(Ordering::Relaxed), 2);
        assert_eq!(radio.shared.lock().unwrap().generation, 1);
        assert!(sent.lock().unwrap().is_empty());
        ready.store(true, Ordering::Relaxed);
        assert!(tx.transmit_ready());
        tx.send_data(&[7]).unwrap();
        assert_eq!(*sent.lock().unwrap(), vec![vec![7]]);
        assert_eq!(radio.shared.lock().unwrap().generation, 1);
    }

    // macOS pseudo terminals reject UART hardware setup with ENOTTY. The
    // transport-independent test above exercises the same lifecycle on macOS.
    #[cfg(target_os = "linux")]
    #[test]
    fn virtual_uart_attached_after_startup_restores_tx() {
        use serialport::SerialPort;
        use std::io::Read;
        let root = crate::recording_export::tests::TestDirectory::new();
        let path = root.0.join("radio");
        let radio = missing();
        if let CommsLinkConfig::Serial { serial } = &mut radio.shared.lock().unwrap().config {
            serial.port = path.to_string_lossy().into_owned();
        }
        let mut tx = radio.try_clone_tx().unwrap().unwrap();
        assert!(radio.connect().is_none());
        let (mut peer, port) = serialport::TTYPort::pair().unwrap();
        peer.set_timeout(Duration::from_millis(500)).unwrap();
        std::os::unix::fs::symlink(port.name().unwrap(), &path).unwrap();
        drop(port);
        radio.shared.lock().unwrap().retry_at = Instant::now();
        assert!(radio.connect().is_some());
        tx.send_data(&[1, 2, 3]).unwrap();
        let mut frame = [0; 7];
        peer.read_exact(&mut frame).unwrap();
        assert_eq!(&frame[4..], &[1, 2, 3]);
    }
}
