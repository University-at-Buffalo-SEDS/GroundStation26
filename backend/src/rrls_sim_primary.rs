//! RRLS simulation: a simulated paired primary on a local pseudo-terminal.
//!
//! Selected by the station mode switch (the supervisor sets
//! `GS_RRLS_SIMULATED_PRIMARY=1`). The RRLS radio stack is unchanged: it opens
//! the slave end of a pseudo-terminal as its radio serial port. The master end
//! is a simulated primary: a primary-role recovery Mesh (HEALTH), the signed
//! station-mode responder, the command-proxy responder (an all-unknown idle
//! observation; every forwarded command is refused as Unavailable) and a small
//! telemetry stream. Bytes never leave this process: no radio, serial device or
//! network output is opened. A fresh simulated installation pairing and
//! redirected state paths keep every saved trust, mode and configuration file
//! inside `GS_RRLS_SIM_ROOT`, so the real primary pairing is never used.
// The simulated primary runs only where a pseudo-terminal radio exists (Linux).
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]
use crate::comms_config::{CommsLinkConfig, CommsLinksConfig, SerialLinkConfig, SerialProtocol};
use crate::rrls::{Mesh, Outgoing, Role};
use crate::station_pairing::Pairing;
use anyhow::{Context, ensure};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

pub(crate) const ENV: &str = "GS_RRLS_SIMULATED_PRIMARY";
pub(crate) const ROOT_ENV: &str = "GS_RRLS_SIM_ROOT";
pub(crate) const PRIMARY_ID: &str = "simulated-primary";
pub(crate) const RRLS_ID: &str = "simulated-rrls";
/// Every path an RRLS writes paired or primary-derived state to.
pub(crate) const ISOLATED: &[(&str, &str)] = &[
    ("GS_STATION_PAIRING_FILE", "rrls/pairing.json"),
    ("GS_STATION_PAIRING_DIR", "managed-pairing"),
    ("GS_STATION_MODE_FILE", "station-mode.json"),
    ("GS_RRLS_CONFIGURATION_FILE", "retained-configuration.json"),
    ("GS_MISSION_SETTINGS_PATH", "mission-settings.json"),
    ("GS_LAUNCH_CLOCK_STATE", "launch_clock.json"),
    ("GS_BOARD_ABORT_HISTORY_DIR", "board_abort_history"),
];
const TELEMETRY_PERIOD_MS: u64 = 1_000;
const MAX_PENDING_PROXY_REPLIES: usize = 8;
const SYNC_DATA: [u8; 2] = [0xA5, 0x5A];
const SYNC_COMMAND: [u8; 2] = [0xA6, 0x5B];
const SYNC_ASCII: [u8; 2] = [0xA7, 0x7A];
const MAX_FRAME: usize = 4_096;

pub(crate) fn requested() -> bool {
    std::env::var(ENV).as_deref() == Ok("1")
}
pub(crate) fn enabled() -> bool {
    requested() && crate::rrls::is_rrls()
}
fn root() -> anyhow::Result<PathBuf> {
    let root = PathBuf::from(
        std::env::var_os(ROOT_ENV).ok_or_else(|| anyhow::anyhow!("{ROOT_ENV} is required"))?,
    );
    ensure!(root.is_absolute(), "{ROOT_ENV} must be absolute");
    Ok(root)
}

/// Must run before logging, pairing, configuration or radio setup.
pub(crate) fn validate_startup() -> anyhow::Result<()> {
    if !requested() {
        return Ok(());
    }
    ensure!(
        crate::rrls::is_rrls(),
        "The simulated primary is an RRLS mode; set GS_RRLS_ROLE=rrls"
    );
    ensure!(
        !crate::flight_sim_runtime::enabled(),
        "FlightSim and the RRLS simulated primary are exclusive"
    );
    ensure!(
        std::env::var_os("GS_RRLS_RADIO_PORT").is_none(),
        "GS_RRLS_RADIO_PORT would be saved into the real RRLS radio settings; unset it for simulation"
    );
    ensure!(
        !crate::flight_sim::sim_mode_enabled(),
        "Set GS_SIM_MODE=0: the testing model would bypass the simulated radio link"
    );
    let root = root()?;
    validate_isolation(&root, |key| std::env::var_os(key).map(PathBuf::from))?;
    std::fs::create_dir_all(root.join("managed-pairing"))?;
    // The abort history creates its own directory and refuses a pre-made one.
    if !root.join("rrls/pairing.json").exists() || !root.join("primary/pairing.json").exists() {
        write_pairing(&root)?;
    }
    Ok(())
}
fn validate_isolation(
    root: &Path,
    get: impl Fn(&str) -> Option<PathBuf>,
) -> anyhow::Result<()> {
    for (key, relative) in ISOLATED {
        ensure!(
            get(key).as_deref() == Some(root.join(relative).as_path()),
            "RRLS simulation requires {key}={}",
            root.join(relative).display()
        );
    }
    Ok(())
}

/// A fresh simulated installation: one shared signing seed, two identities.
pub(crate) fn write_pairing(root: &Path) -> anyhow::Result<()> {
    let installation = format!("simulated-{:032x}", rand::random::<u128>());
    let seed = rand::random::<[u8; 32]>();
    for (dir, id, peer, peer_role) in [
        ("rrls", RRLS_ID, PRIMARY_ID, "primary"),
        ("primary", PRIMARY_ID, RRLS_ID, "rrls"),
    ] {
        let dir = root.join(dir);
        std::fs::create_dir_all(&dir)?;
        write_private(&dir.join("signing.seed"), &seed)?;
        let config = serde_json::json!({
            "version": 1,
            "installation_id": installation,
            "station_id": id,
            "signing_seed_file": "signing.seed",
            "peers": [{"id": peer, "role": peer_role, "urls": []}],
        });
        write_private(&dir.join("pairing.json"), &serde_json::to_vec_pretty(&config)?)?;
    }
    Ok(())
}
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// The simulated primary's protocol state. Pure: the caller moves bytes.
pub(crate) struct SimulatedPrimary {
    mesh: Mesh,
    pairing: Pairing,
    mode: crate::station_mode::SimulatedPrimaryMode,
    proxy: crate::command_proxy::simulated_primary::SimulatedPrimaryProxy,
    pending_mode_reply: Option<Vec<u8>>,
    pending_proxy: VecDeque<Vec<u8>>,
    next_telemetry: u64,
}
impl SimulatedPrimary {
    pub(crate) fn new(pairing: Pairing, generation_path: PathBuf) -> anyhow::Result<Self> {
        ensure!(
            pairing.public.station_id == PRIMARY_ID
                && pairing.public.installation_id.starts_with("simulated-"),
            "The simulated primary only uses a simulated installation pairing"
        );
        Ok(Self {
            mesh: Mesh::new(Role::Primary, rand::random()),
            mode: crate::station_mode::SimulatedPrimaryMode::new(&pairing, generation_path)?,
            pairing,
            proxy: crate::command_proxy::simulated_primary::SimulatedPrimaryProxy::new(),
            pending_mode_reply: None,
            pending_proxy: VecDeque::new(),
            next_telemetry: 0,
        })
    }
    #[cfg(test)]
    pub(crate) fn refused_commands(&self) -> u64 {
        self.proxy.refused_commands
    }
    /// Consume frames heard from the RRLS and return frames to deliver to it.
    pub(crate) fn step(
        &mut self,
        now: u64,
        inbound: impl IntoIterator<Item = (bool, Vec<u8>)>,
    ) -> Vec<Outgoing> {
        let mut out = Vec::new();
        for (command, bytes) in inbound {
            // Locally delivered RRLS data would be executed by a real primary.
            // The simulation drops it: nothing here can act on a command.
            let _ = self.mesh.incoming(command, &bytes, now);
        }
        self.mesh.tick(now);
        for _ in 0..4 {
            let Some(wire) = self.mesh.take_operating_mode() else {
                break;
            };
            if let Some(reply) = self.mode.receive(&self.pairing, &wire, now) {
                self.pending_mode_reply = Some(reply);
            }
        }
        if let Some(reply) = self.pending_mode_reply.take() {
            if self.mesh.queue_operating_mode_reply(&reply, now) {
                self.mesh.note_verified_peer(now);
            } else {
                self.pending_mode_reply = Some(reply);
            }
        }
        let transport = self.mesh.command_proxy_transport(now);
        self.proxy.connect(transport);
        while let Some(frame) = self.mesh.take_command_proxy() {
            if let Some(reply) =
                self.proxy
                    .receive(&self.pairing, transport, crate::station_mode::local_mode(), &frame)
                && self.pending_proxy.len() < MAX_PENDING_PROXY_REPLIES
            {
                self.pending_proxy.push_back(reply);
            }
        }
        while let Some(reply) = self.pending_proxy.front() {
            if !self.mesh.queue_command_proxy(reply, now) {
                break;
            }
            self.pending_proxy.pop_front();
        }
        // Settings, clock and pairing exchanges are not simulated. Drain them
        // so the bounded inbox cannot hold back the simulated replies.
        while self.mesh.take_settings().is_some() {}
        while self.mesh.take_launch_clock().is_some() {}
        while self.mesh.take_station_pairing().is_some() {}
        if now >= self.next_telemetry {
            self.next_telemetry = now.saturating_add(TELEMETRY_PERIOD_MS);
            for wire in telemetry() {
                if let Some(frame) = self.mesh.local_tx(false, &wire, now) {
                    out.push(frame);
                }
            }
        }
        while let Some(frame) = pop(&mut self.mesh, now) {
            out.push(frame);
        }
        out
    }
}
/// One queued frame, fully sent: a pseudo-terminal has no air budget.
fn pop(mesh: &mut Mesh, now: u64) -> Option<Outgoing> {
    let transmission = mesh.begin_bounded_transmission(now, 0, false, 1)?;
    mesh.finish_bounded_transmission(transmission, true)
        .map(|(first, _)| first)
}

/// A small idle-pad picture: Idle flight state, pad GPS and battery voltage.
fn telemetry() -> Vec<Vec<u8>> {
    use crate::telemetry_schema::{data_type, endpoint};
    use crate::types::Board;
    let now = crate::telemetry_task::get_current_timestamp_ms();
    let floats = |values: &[f32]| -> std::sync::Arc<[u8]> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>().into()
    };
    [
        (
            "FLIGHT_STATE",
            "FLIGHT_STATE",
            Board::GroundStation.sender_id(),
            std::sync::Arc::from(vec![crate::types::FlightState::Idle as u8]),
        ),
        (
            "GPS_DATA",
            "GROUND_STATION",
            Board::FlightComputer.sender_id(),
            floats(&[31.7619, -106.485, 1200.0]),
        ),
        (
            "BATTERY_VOLTAGE",
            "GROUND_STATION",
            Board::PowerBoard.sender_id(),
            floats(&[7.9]),
        ),
    ]
    .into_iter()
    .filter_map(|(ty, to, sender, payload)| {
        sedsnet::packet::Packet::new(data_type(ty), &[endpoint(to)], sender, now, payload)
            .ok()
            .map(|packet| sedsnet::wire_format::pack_packet(&packet).to_vec())
    })
    .collect()
}

fn encode_frame(frame: &Outgoing) -> Option<Vec<u8>> {
    let len = frame.bytes.len();
    if len == 0 || len > MAX_FRAME {
        return None;
    }
    let mut out = Vec::with_capacity(len + 4);
    out.extend_from_slice(if frame.command { &SYNC_COMMAND } else { &SYNC_DATA });
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&frame.bytes);
    Some(out)
}
/// Raw-UART frames written by the RRLS; resynchronizes past garbage.
fn take_frame(buffer: &mut Vec<u8>) -> Option<(bool, Vec<u8>)> {
    loop {
        let start = buffer
            .windows(2)
            .position(|w| w == SYNC_DATA || w == SYNC_COMMAND || w == SYNC_ASCII);
        match start {
            None => {
                let keep = usize::from(buffer.last().is_some_and(|b| [0xA5, 0xA6, 0xA7].contains(b)));
                buffer.drain(..buffer.len() - keep);
                return None;
            }
            Some(offset) => {
                buffer.drain(..offset);
            }
        }
        if buffer.len() < 4 {
            return None;
        }
        let len = usize::from(u16::from_le_bytes([buffer[2], buffer[3]]));
        if len == 0 || len > MAX_FRAME {
            buffer.drain(..1);
            continue;
        }
        if buffer.len() < 4 + len {
            return None;
        }
        let kind = [buffer[0], buffer[1]];
        let payload = buffer[4..4 + len].to_vec();
        buffer.drain(..4 + len);
        if kind == SYNC_ASCII {
            continue;
        }
        return Some((kind == SYNC_COMMAND, payload));
    }
}

#[cfg(target_os = "linux")]
mod pty {
    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd};
    pub(super) struct Pty {
        pub master: File,
        /// Held open so the master never reports EIO while the RRLS reconnects.
        pub _slave: File,
        pub path: String,
    }
    pub(super) fn open() -> std::io::Result<Pty> {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty writes two new descriptors on success; null name,
        // termios and winsize pointers are permitted.
        let status = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both descriptors were just created and are owned here.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        // SAFETY: termios is plain data; tcgetattr fills it before use.
        unsafe {
            let mut termios = std::mem::zeroed::<libc::termios>();
            if libc::tcgetattr(slave.as_raw_fd(), &mut termios) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::cfmakeraw(&mut termios);
            if libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &termios) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            if flags < 0
                || libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))?
            .to_string_lossy()
            .into_owned();
        Ok(Pty {
            master,
            _slave: slave,
            path,
        })
    }
}

/// Byte pump between the pseudo-terminal master and the simulated primary.
#[cfg(target_os = "linux")]
fn run(mut master: std::fs::File, mut primary: SimulatedPrimary) {
    use std::io::{ErrorKind, Read, Write};
    let started = std::time::Instant::now();
    let mut buffer = Vec::new();
    let mut scratch = [0u8; 2048];
    loop {
        loop {
            match master.read(&mut scratch) {
                Ok(0) => break,
                Ok(n) => buffer.extend_from_slice(&scratch[..n]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => {
                    log::warn!("Simulated primary link read failed: {error}");
                    break;
                }
            }
            if buffer.len() > 4 * MAX_FRAME {
                let excess = buffer.len() - 4 * MAX_FRAME;
                buffer.drain(..excess);
            }
        }
        let mut inbound = Vec::new();
        while let Some(frame) = take_frame(&mut buffer) {
            inbound.push(frame);
        }
        let now = started.elapsed().as_millis() as u64;
        for frame in primary.step(now, inbound) {
            let Some(bytes) = encode_frame(&frame) else {
                continue;
            };
            let mut written = 0;
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
            while written < bytes.len() && std::time::Instant::now() < deadline {
                match master.write(&bytes[written..]) {
                    Ok(n) => written += n,
                    Err(error)
                        if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => {
                        log::warn!("Simulated primary link write failed: {error}");
                        break;
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Opens the pseudo-terminal, starts the simulated primary and returns the
/// RRLS link configuration pointing at it. Called only when [`enabled`].
pub(crate) fn comms_links() -> anyhow::Result<CommsLinksConfig> {
    #[cfg(target_os = "linux")]
    {
        let root = root()?;
        let pairing = Pairing::from_file(&root.join("primary/pairing.json"))
            .context("simulated primary pairing")?;
        let primary = SimulatedPrimary::new(pairing, root.join("primary/mode-generation.json"))?;
        let pty = pty::open().context("simulated primary pseudo-terminal")?;
        let port = pty.path.clone();
        let pty::Pty { master, _slave, .. } = pty;
        std::thread::Builder::new()
            .name("rrls-simulated-primary".into())
            .spawn(move || {
                let _slave = _slave;
                run(master, primary)
            })?;
        log::warn!(
            "RRLS SIMULATION: radio replaced by a simulated paired primary on {port}; no radio output, state isolated in {}",
            root.display()
        );
        let serial = |port: String| SerialLinkConfig {
            port,
            hardware_flow_control: false,
            rfd_profile: None,
            baud_rate: 115_200,
            protocol: SerialProtocol::RawUart,
        };
        Ok(CommsLinksConfig {
            version: CommsLinksConfig::default().version,
            av_bay: CommsLinkConfig::Serial { serial: serial(port) },
            fill_box: CommsLinkConfig::Serial {
                serial: serial("unused".into()),
            },
        })
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("The RRLS simulated primary requires Linux pseudo-terminals")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("gs-rrls-sim-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    fn primary(root: &Path) -> SimulatedPrimary {
        write_pairing(root).unwrap();
        SimulatedPrimary::new(
            Pairing::from_file(&root.join("primary/pairing.json")).unwrap(),
            root.join("primary/mode-generation.json"),
        )
        .unwrap()
    }
    pub(crate) fn rrls_pairing(root: &Path) -> Pairing {
        Pairing::from_file(&root.join("rrls/pairing.json")).unwrap()
    }
    /// Deliver every simulated-primary frame to an RRLS mesh, returning its
    /// locally delivered data frames.
    fn deliver(rr: &mut Mesh, frames: Vec<Outgoing>, now: u64) -> Vec<Vec<u8>> {
        frames
            .into_iter()
            .filter_map(|frame| rr.incoming(frame.command, &frame.bytes, now))
            .filter(|frame| !frame.command)
            .map(|frame| frame.bytes)
            .collect()
    }
    fn rrls_frames(rr: &mut Mesh, now: u64) -> Vec<(bool, Vec<u8>)> {
        rr.tick(now);
        std::iter::from_fn(|| rr.pop(now))
            .map(|frame| (frame.command, frame.bytes))
            .collect()
    }

    #[test]
    fn isolation_requires_every_state_path_inside_the_simulation_root() {
        let root = PathBuf::from("/sim/root");
        let all = |key: &str| {
            ISOLATED
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, relative)| root.join(relative))
        };
        validate_isolation(&root, all).unwrap();
        for (missing, _) in ISOLATED {
            let partial = |key: &str| if key == *missing { None } else { all(key) };
            assert!(validate_isolation(&root, partial).is_err(), "{missing}");
            let real = |key: &str| {
                if key == *missing {
                    Some(PathBuf::from("data/real.json"))
                } else {
                    all(key)
                }
            };
            assert!(validate_isolation(&root, real).is_err(), "{missing}");
        }
    }

    #[test]
    fn generated_pairing_is_a_fresh_simulated_installation() {
        let root = root();
        write_pairing(&root).unwrap();
        let rr = rrls_pairing(&root);
        let gs = Pairing::from_file(&root.join("primary/pairing.json")).unwrap();
        assert!(rr.public.installation_id.starts_with("simulated-"));
        assert_eq!(rr.public.installation_id, gs.public.installation_id);
        assert_eq!(rr.public.station_id, RRLS_ID);
        assert_eq!(rr.public.peers[0].id, PRIMARY_ID);
        assert_eq!(rr.public.peers[0].role, "primary");
        assert_eq!(gs.public.peers[0].role, "rrls");
        write_pairing(&root).unwrap();
        assert_ne!(
            rrls_pairing(&root).public.installation_id,
            rr.public.installation_id,
            "every simulation start is a new installation"
        );
        // A real pairing can never drive the simulated primary.
        assert!(SimulatedPrimary::new(rr, root.join("g")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frames_roundtrip_and_resynchronize_past_garbage() {
        let command = Outgoing {
            command: true,
            bytes: b"RRhealth".to_vec(),
        };
        let data = Outgoing {
            command: false,
            bytes: vec![1, 2, 3],
        };
        let mut buffer = vec![0x00, 0xA5, 0x13];
        buffer.extend(encode_frame(&command).unwrap());
        buffer.extend([0xA7, 0x7A, 2, 0, b'o', b'k']);
        buffer.extend(encode_frame(&data).unwrap());
        let tail = encode_frame(&command).unwrap();
        buffer.extend(&tail[..5]);
        assert_eq!(take_frame(&mut buffer), Some((true, b"RRhealth".to_vec())));
        assert_eq!(take_frame(&mut buffer), Some((false, vec![1, 2, 3])));
        assert_eq!(take_frame(&mut buffer), None);
        buffer.extend(&tail[5..]);
        assert_eq!(take_frame(&mut buffer), Some((true, b"RRhealth".to_vec())));
        assert!(encode_frame(&Outgoing { command: true, bytes: vec![] }).is_none());
    }

    #[test]
    fn simulated_primary_gives_rrls_health_session_and_telemetry() {
        crate::telemetry_schema::initialize().unwrap();
        let root = root();
        let mut gs = primary(&root);
        let mut rr = Mesh::new(Role::Rrls, 9);
        assert!(rr.operating_mode_session(0).is_none());
        let data = deliver(&mut rr, gs.step(0, []), 0);
        assert!(rr.operating_mode_session(0).is_some(), "HEALTH opens the primary session");
        assert!(rr.settings_available(0));
        let types: Vec<_> = data
            .iter()
            .filter_map(|wire| sedsnet::wire_format::unpack_packet(wire).ok())
            .map(|packet| packet.data_type())
            .collect();
        assert!(types.contains(&crate::telemetry_schema::data_type("FLIGHT_STATE")));
        assert!(types.contains(&crate::telemetry_schema::data_type("GPS_DATA")));
        // Telemetry is periodic, not per step.
        assert!(deliver(&mut rr, gs.step(10, []), 10).is_empty());
        assert!(!deliver(&mut rr, gs.step(1_000, []), 1_000).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rrls_command_proxy_gets_observation_and_every_command_is_refused() {
        let root = root();
        let mut gs = primary(&root);
        let rr_pairing = rrls_pairing(&root);
        let mut rr = Mesh::new(Role::Rrls, 11);
        let mut now = 0;
        deliver(&mut rr, gs.step(now, []), now);
        let mut proxy = crate::command_proxy::simulated_primary::tests::RrlsProxy::new();
        let mut capability = None;
        let mut outcome = None;
        for _ in 0..200 {
            now += 20;
            if let Some(transport) = rr.command_proxy_transport(now)
                && let Some(query) = proxy.next_frame(&rr_pairing, transport)
            {
                assert!(rr.queue_command_proxy(&query, now));
            }
            let up = rrls_frames(&mut rr, now);
            deliver(&mut rr, gs.step(now, up), now);
            while let Some(frame) = rr.take_command_proxy() {
                let transport = rr.command_proxy_transport(now).unwrap();
                match proxy.receive(&rr_pairing, transport, &frame) {
                    Some(Ok(observation)) => capability = Some(observation),
                    Some(Err(result)) => outcome = Some(result),
                    None => {}
                }
            }
            if outcome.is_some() {
                break;
            }
        }
        assert_eq!(
            capability.as_deref(),
            Some(crate::primary_mirror::simulated_idle_observation().as_slice())
        );
        assert_eq!(outcome.as_deref(), Some("Unavailable"));
        assert_eq!(gs.refused_commands(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn simulated_primary_runs_over_a_pseudo_terminal_without_radio() {
        use std::io::{Read, Write};
        let root = root();
        let gs = primary(&root);
        let pty = pty::open().unwrap();
        assert!(pty.path.starts_with("/dev/pts/"), "{}", pty.path);
        let pty::Pty { master, _slave, path } = pty;
        std::thread::spawn(move || {
            let _slave = _slave;
            run(master, gs)
        });
        let mut port = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut rr = Mesh::new(Role::Rrls, 3);
        let mut buffer = Vec::new();
        let mut scratch = [0u8; 1024];
        let started = std::time::Instant::now();
        // The pseudo-terminal is blocking for this reader; HEALTH arrives
        // every tick, so each read returns promptly.
        while rr.operating_mode_session(started.elapsed().as_millis() as u64).is_none() {
            assert!(started.elapsed() < std::time::Duration::from_secs(10));
            let n = port.read(&mut scratch).unwrap();
            buffer.extend_from_slice(&scratch[..n]);
            let now = started.elapsed().as_millis() as u64;
            while let Some((command, bytes)) = take_frame(&mut buffer) {
                rr.incoming(command, &bytes, now);
            }
        }
        // The RRLS can answer over the same link.
        let now = started.elapsed().as_millis() as u64;
        for (command, bytes) in rrls_frames(&mut rr, now) {
            let frame = encode_frame(&Outgoing { command, bytes }).unwrap();
            port.write_all(&frame).unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
