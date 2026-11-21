//! Pairing a primary GroundStation and an RRLS from the UI over their radio link.
//!
//! The result is the same pair of installation bundles `tools/pair_stations.py`
//! writes: one shared 32-byte Ed25519 seed and installation id, a
//! `pairing.json` per station naming the other as its single peer.
//!
//! The RRLS asks and the primary answers, on the existing settings channel.
//! Both sides run an ephemeral X25519 agreement. The RRLS commits to its key
//! before it sees the primary's, so a relay in the middle cannot search for a
//! matching code: it gets one guess in a million. Each operator compares the
//! six-digit code from the shared transcript and confirms it locally. The seed
//! travels encrypted under the agreed key. Nothing is written until both
//! operators confirm: the primary saves only after its own admin confirmed and
//! the RRLS proved, with a signature from the new identity, that its admin did
//! too; the RRLS saves only after the primary's signed reply.
use ring::{aead, agreement, digest, hkdf, rand::SecureRandom, signature, signature::KeyPair};

pub(crate) const MAGIC: &[u8; 3] = b"SP\x01";
const HELLO: u8 = 1;
const KEY: u8 = 2;
const REVEAL: u8 = 3;
const BUNDLE: u8 = 4;
const CONFIRM: u8 = 5;
const DONE: u8 = 6;
const ABORT: u8 = 7;
pub(crate) const MAX_URL: usize = 128;
/// An open pairing window lasts this long from Start.
pub(crate) const WINDOW_MS: u64 = 300_000;
/// The RRLS repeats its current request this often until answered.
pub(crate) const RESEND_MS: u64 = 2_000;
const INSTALLATION_LEN: usize = 36;
const BUNDLE_PLAIN: usize = 32 + INSTALLATION_LEN;
const TAG: usize = 16;

pub(crate) fn recognizes(wire: &[u8]) -> bool {
    wire.len() > MAGIC.len() && wire.starts_with(MAGIC)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Primary,
    Rrls,
}
impl Side {
    pub(crate) fn station_id(self) -> &'static str {
        match self {
            Side::Primary => "primary",
            Side::Rrls => "rrls",
        }
    }
    pub(crate) fn peer(self) -> Side {
        match self {
            Side::Primary => Side::Rrls,
            Side::Rrls => Side::Primary,
        }
    }
}

/// Normalise an HTTP(S) origin the same way `pair_stations.py` does.
pub(crate) fn origin(value: &str) -> Result<String, String> {
    let bad = || "use an HTTP(S) origin, without credentials, path or query".to_string();
    let value = value.trim();
    let (scheme, rest) = value.split_once("://").ok_or_else(bad)?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return Err(bad()),
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty()
        || authority.contains(['/', '?', '#', '@', ' ', '\\'])
        || authority.chars().any(|c| c.is_control())
    {
        return Err(bad());
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (host, after) = inner.split_once(']').ok_or_else(bad)?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or_else(bad)?),
        };
        (format!("[{}]", host.to_ascii_lowercase()), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_ascii_lowercase(), Some(port)),
            None => (authority.to_ascii_lowercase(), None),
        }
    };
    if host.is_empty() || host == "[]" || host.contains(':') && !host.starts_with('[') {
        return Err(bad());
    }
    let port = match port {
        None => default_port,
        Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(bad)?,
    };
    let out = if port == default_port {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{port}")
    };
    if out.len() > MAX_URL {
        return Err("origin is too long".into());
    }
    Ok(out)
}
/// This station's web origin scheme/port, from GS_HTTPS_BIND_ADDRESS (443
/// default). With HTTPS off the plain HTTP server on 3000 is the reachable one.
fn local_scheme_port() -> (&'static str, u16) {
    match std::env::var("GS_HTTPS_BIND_ADDRESS").ok().as_deref() {
        Some("off") => ("http", 3000),
        Some(addr) => (
            "https",
            addr.parse::<std::net::SocketAddr>().map_or(443, |a| a.port()),
        ),
        None => ("https", 443),
    }
}
/// The URL this station advertises in the exchange when the operator gives
/// none: GS_STATION_PUBLIC_URL if set, otherwise the LAN address of the
/// interface that routes off-host, with this station's web port. Empty when
/// neither is known: pairing then completes without a URL, and the peer is
/// found later through LAN station discovery. URLs are hints for reaching
/// the peer; they are bound into the transcript and the confirmed code, and
/// never grant trust on their own.
pub(crate) fn advertised_origin() -> String {
    if let Some(url) = std::env::var("GS_STATION_PUBLIC_URL")
        .ok()
        .and_then(|url| origin(&url).ok())
    {
        return url;
    }
    // No packet is sent: connect() on UDP only selects the outgoing interface.
    let ip = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("192.0.2.1:9")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip())
        .ok()
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified());
    ip.map_or_else(String::new, |ip| lan_origin(ip, local_scheme_port()))
}
fn lan_origin(ip: std::net::IpAddr, (scheme, port): (&str, u16)) -> String {
    let host = match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
    };
    origin(&format!("{scheme}://{host}:{port}")).unwrap_or_default()
}
pub(crate) fn scheme_port(origin: &str) -> (String, u16) {
    if origin.is_empty() {
        let (scheme, port) = local_scheme_port();
        return (scheme.into(), port);
    }
    let (scheme, rest) = origin.split_once("://").unwrap_or(("https", origin));
    let default = if scheme == "http" { 80 } else { 443 };
    let port = match rest.strip_prefix('[') {
        Some(inner) => inner.split_once("]:").map(|(_, p)| p),
        None => rest.rsplit_once(':').map(|(_, p)| p),
    };
    (
        scheme.into(),
        port.and_then(|p| p.parse().ok()).unwrap_or(default),
    )
}

/// The persisted outcome, in the `pair_stations.py` format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bundle {
    pub side: Side,
    pub seed: [u8; 32],
    pub installation_id: String,
    pub own_url: String,
    pub peer_url: String,
}
impl Bundle {
    pub(crate) fn pairing_json(&self) -> serde_json::Value {
        let (scheme, port) = scheme_port(&self.own_url);
        let peer = self.side.peer().station_id();
        serde_json::json!({
            "version": 1,
            "installation_id": self.installation_id,
            "station_id": self.side.station_id(),
            "signing_seed_file": "identity.seed",
            "scheme": scheme,
            "port": port,
            // Peer URLs are optional hints; LAN discovery finds the peer
            // when none was exchanged.
            "peers": [{"id": peer, "role": peer,
                "urls": if self.peer_url.is_empty() { Vec::new() } else { vec![self.peer_url.clone()] }}],
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    /// Window open, no code yet.
    Waiting,
    /// A code is shown; the local admin has not confirmed it.
    Compare,
    /// The local admin confirmed; waiting for the other station.
    AwaitingPeer,
    /// The bundle was saved on this station.
    Saved,
    Failed,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct View {
    pub phase: Phase,
    pub code: Option<String>,
    pub expires_in_ms: u64,
    pub local_confirmed: bool,
    pub peer_confirmed: bool,
    pub peer_url: Option<String>,
    pub message: String,
}

#[derive(Clone, Copy)]
struct Derived {
    transcript: [u8; 32],
    key: [u8; 32],
    code: u32,
}
fn derive(shared: &[u8], transcript: [u8; 32]) -> Derived {
    struct Len(usize);
    impl hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    let mut okm = [0u8; 36];
    hkdf::Salt::new(hkdf::HKDF_SHA256, &transcript)
        .extract(shared)
        .expand(&[b"GS26PAIR1"], Len(okm.len()))
        .expect("fixed HKDF length")
        .fill(&mut okm)
        .expect("fixed HKDF length");
    let mut key = [0u8; 32];
    key.copy_from_slice(&okm[..32]);
    Derived {
        transcript,
        key,
        code: u32::from_le_bytes(okm[32..36].try_into().unwrap()) % 1_000_000,
    }
}
fn commitment(public: &[u8; 32], url: &str) -> [u8; 32] {
    let mut c = digest::Context::new(&digest::SHA256);
    c.update(b"GS26PAIR1C");
    c.update(public);
    c.update(url.as_bytes());
    c.finish().as_ref().try_into().unwrap()
}
fn transcript(
    nonce: &[u8; 16],
    commit: &[u8; 32],
    primary: &[u8; 32],
    primary_url: &str,
    rrls: &[u8; 32],
    rrls_url: &str,
) -> [u8; 32] {
    let mut c = digest::Context::new(&digest::SHA256);
    c.update(b"GS26PAIR1T");
    c.update(nonce);
    c.update(commit);
    c.update(primary);
    c.update(&[primary_url.len() as u8]);
    c.update(primary_url.as_bytes());
    c.update(rrls);
    c.update(&[rrls_url.len() as u8]);
    c.update(rrls_url.as_bytes());
    c.finish().as_ref().try_into().unwrap()
}
fn seal(key: &[u8; 32], aad: &[u8; 32], plain: &[u8]) -> Vec<u8> {
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::CHACHA20_POLY1305, key).expect("fixed key length"),
    );
    let mut out = plain.to_vec();
    // Each key is derived from a fresh agreement and seals exactly one message.
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key([0; 12]),
        aead::Aad::from(aad),
        &mut out,
    )
    .expect("bounded plaintext");
    out
}
fn open(key: &[u8; 32], aad: &[u8; 32], sealed: &[u8]) -> Option<Vec<u8>> {
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::CHACHA20_POLY1305, key).ok()?);
    let mut buf = sealed.to_vec();
    let plain = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key([0; 12]),
            aead::Aad::from(aad),
            &mut buf,
        )
        .ok()?;
    Some(plain.to_vec())
}
fn sign(seed: &[u8; 32], domain: &[u8], transcript: &[u8; 32]) -> [u8; 64] {
    let key = signature::Ed25519KeyPair::from_seed_unchecked(seed).expect("32-byte seed");
    let mut message = domain.to_vec();
    message.extend_from_slice(transcript);
    key.sign(&message).as_ref().try_into().unwrap()
}
fn verify(seed: &[u8; 32], domain: &[u8], transcript: &[u8; 32], sig: &[u8]) -> bool {
    let key = signature::Ed25519KeyPair::from_seed_unchecked(seed).expect("32-byte seed");
    let mut message = domain.to_vec();
    message.extend_from_slice(transcript);
    signature::UnparsedPublicKey::new(&signature::ED25519, key.public_key().as_ref())
        .verify(&message, sig)
        .is_ok()
}
fn installation_id(bytes: [u8; 16]) -> String {
    let mut b = bytes;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}
fn frame(kind: u8, nonce: &[u8; 16], body: &[&[u8]]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.push(kind);
    out.extend_from_slice(nonce);
    for part in body {
        out.extend_from_slice(part);
    }
    out
}
fn url_field(url: &str) -> Vec<u8> {
    let mut out = vec![url.len() as u8];
    out.extend_from_slice(url.as_bytes());
    out
}
struct Parsed<'a> {
    kind: u8,
    nonce: [u8; 16],
    body: &'a [u8],
}
fn parse(wire: &[u8]) -> Option<Parsed<'_>> {
    if !recognizes(wire) || wire.len() < MAGIC.len() + 1 + 16 || wire.len() > 512 {
        return None;
    }
    Some(Parsed {
        kind: wire[3],
        nonce: wire[4..20].try_into().ok()?,
        body: &wire[20..],
    })
}
fn key_and_url(body: &[u8]) -> Option<([u8; 32], String)> {
    let key: [u8; 32] = body.get(..32)?.try_into().ok()?;
    let len = usize::from(*body.get(32)?);
    let url = std::str::from_utf8(body.get(33..33 + len)?).ok()?;
    // An empty URL is allowed: the station did not know a reachable address.
    (body.len() == 33 + len && (url.is_empty() || origin(url).ok()? == url))
        .then(|| (key, url.to_string()))
}

struct Session {
    nonce: [u8; 16],
    expires: u64,
    own_url: String,
    local_confirmed: bool,
    peer_confirmed: bool,
    phase: Phase,
    message: String,
    // RRLS: key committed to before the primary's key is known.
    ephemeral: Option<agreement::EphemeralPrivateKey>,
    own_public: [u8; 32],
    commit: [u8; 32],
    derived: Option<Derived>,
    /// RRLS: keys agreed after KEY, held back until BUNDLE proves the primary has them.
    pending: Option<Derived>,
    bundle: Option<Bundle>,
    peer_url: Option<String>,
    /// RRLS: current request and when it was last queued.
    request: Option<Vec<u8>>,
    last_sent: Option<u64>,
    /// Primary: cached answer to the last request, for retried requests.
    reply: Option<(u8, Vec<u8>)>,
}

/// What the worker must do after a step.
#[derive(Default, Debug)]
pub(crate) struct Effects {
    pub send: Vec<Vec<u8>>,
    pub save: Option<Bundle>,
}

pub(crate) struct Exchange {
    side: Side,
    session: Option<Session>,
    rng: ring::rand::SystemRandom,
}
impl Exchange {
    pub(crate) fn new(side: Side) -> Self {
        Self {
            side,
            session: None,
            rng: ring::rand::SystemRandom::new(),
        }
    }
    /// Still exchanging frames. A primary that has saved keeps answering
    /// CONFIRM retries until the window closes, so a lost DONE is repeated.
    pub(crate) fn active(&self, now: u64) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| now < s.expires && s.phase != Phase::Failed)
    }
    fn undecided(&self, now: u64) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| now < s.expires && !matches!(s.phase, Phase::Saved | Phase::Failed))
    }
    pub(crate) fn view(&self, now: u64) -> Option<View> {
        let s = self.session.as_ref()?;
        let (phase, message) =
            if now >= s.expires && !matches!(s.phase, Phase::Saved | Phase::Failed) {
                (
                    Phase::Failed,
                    "Pairing window expired; nothing was saved on this station.".to_string(),
                )
            } else {
                (s.phase, s.message.clone())
            };
        Some(View {
            phase,
            code: s
                .derived
                .as_ref()
                .filter(|_| !matches!(phase, Phase::Failed))
                .map(|d| format!("{:03} {:03}", d.code / 1000, d.code % 1000)),
            expires_in_ms: s.expires.saturating_sub(now),
            local_confirmed: s.local_confirmed,
            peer_confirmed: s.peer_confirmed,
            peer_url: s.peer_url.clone().filter(|url| !url.is_empty()),
            message,
        })
    }
    fn random<const N: usize>(&self) -> [u8; N] {
        let mut out = [0u8; N];
        self.rng.fill(&mut out).expect("system randomness");
        out
    }
    /// Open a window. On the RRLS this also builds the first request.
    /// `own_url` is optional: without one the station advertises its own
    /// address (see `advertised_origin`), possibly none at all.
    pub(crate) fn start(&mut self, own_url: Option<&str>, now: u64) -> Result<(), String> {
        let own_url = match own_url.map(str::trim).filter(|url| !url.is_empty()) {
            Some(url) => origin(url)?,
            None => advertised_origin(),
        };
        self.start_origin(own_url, now)
    }
    /// Open a window advertising `own_url`, which may be empty.
    fn start_origin(&mut self, own_url: String, now: u64) -> Result<(), String> {
        if self.undecided(now) {
            return Err("A pairing window is already open; cancel it first.".into());
        }
        let nonce = self.random::<16>();
        let mut session = Session {
            nonce,
            expires: now.saturating_add(WINDOW_MS),
            own_url,
            local_confirmed: false,
            peer_confirmed: false,
            phase: Phase::Waiting,
            message: match self.side {
                Side::Primary => "Waiting for the RRLS. Start pairing on the RRLS too.".into(),
                Side::Rrls => {
                    "Asking the primary over the radio. Start pairing on the primary too.".into()
                }
            },
            ephemeral: None,
            own_public: [0; 32],
            commit: [0; 32],
            derived: None,
            pending: None,
            bundle: None,
            peer_url: None,
            request: None,
            last_sent: None,
            reply: None,
        };
        if self.side == Side::Rrls {
            let key = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &self.rng)
                .map_err(|_| "randomness unavailable".to_string())?;
            session.own_public = key
                .compute_public_key()
                .map_err(|_| "key generation failed".to_string())?
                .as_ref()
                .try_into()
                .unwrap();
            session.commit = commitment(&session.own_public, &session.own_url);
            session.ephemeral = Some(key);
            session.request = Some(frame(HELLO, &nonce, &[&session.commit]));
        }
        self.session = Some(session);
        Ok(())
    }
    pub(crate) fn cancel(&mut self) -> Effects {
        let mut effects = Effects::default();
        if let Some(s) = self.session.take()
            && s.phase != Phase::Saved
            && s.derived.is_some()
        {
            effects.send.push(frame(ABORT, &s.nonce, &[]));
        }
        effects
    }
    /// The local admin confirms the code they compared.
    pub(crate) fn confirm(&mut self, code: &str, now: u64) -> Result<Effects, String> {
        let active = self.undecided(now);
        let side = self.side;
        let s = self
            .session
            .as_mut()
            .filter(|_| active)
            .ok_or_else(|| "No pairing window is open.".to_string())?;
        let expected = s
            .derived
            .as_ref()
            .ok_or_else(|| "No code to confirm yet.".to_string())?
            .code;
        let typed: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
        if typed.len() != 6 || typed.parse::<u32>().ok() != Some(expected) {
            return Err("The code does not match the one shown on this station.".into());
        }
        s.local_confirmed = true;
        s.phase = Phase::AwaitingPeer;
        let mut effects = Effects::default();
        match side {
            Side::Rrls => {
                let bundle = s
                    .bundle
                    .as_ref()
                    .expect("RRLS has a code only with its bundle");
                let d = s.derived.as_ref().unwrap();
                let sig = sign(&bundle.seed, b"GS26PAIR1-confirm", &d.transcript);
                s.request = Some(frame(CONFIRM, &s.nonce, &[&sig]));
                s.last_sent = None;
                s.message = "Waiting for the primary to confirm the same code.".into();
            }
            Side::Primary => {
                if s.peer_confirmed {
                    s.phase = Phase::Saved;
                    s.message = "Paired. The RRLS saves its copy on its next retry.".into();
                    effects.save = s.bundle.clone();
                } else {
                    s.message = "Waiting for the RRLS admin to confirm the same code.".into();
                }
            }
        }
        Ok(effects)
    }
    /// A save failed after a confirmed exchange.
    pub(crate) fn save_failed(&mut self, error: &str) {
        if let Some(s) = self.session.as_mut() {
            s.phase = Phase::Failed;
            s.message = format!("Could not save the pairing on this station: {error}");
            s.reply = None;
            s.request = None;
        }
    }
    /// Periodic work: the RRLS (re)sends its current request.
    pub(crate) fn poll(&mut self, now: u64, link: bool) -> Effects {
        let mut effects = Effects::default();
        let active = self.active(now);
        if let Some(s) = self.session.as_mut()
            && active
            && link
            && let Some(request) = &s.request
            && s.last_sent
                .is_none_or(|last| now.saturating_sub(last) >= RESEND_MS)
        {
            effects.send.push(request.clone());
            s.last_sent = Some(now);
        }
        effects
    }
    pub(crate) fn receive(&mut self, wire: &[u8], now: u64) -> Effects {
        let Some(m) = parse(wire) else {
            return Effects::default();
        };
        if !self.active(now) {
            return Effects::default();
        }
        match self.side {
            Side::Primary => self.primary_receive(m),
            Side::Rrls => self.rrls_receive(m),
        }
    }
    fn primary_receive(&mut self, m: Parsed<'_>) -> Effects {
        let mut effects = Effects::default();
        let rng = &self.rng;
        let s = self.session.as_mut().unwrap();
        if s.phase == Phase::Waiting && m.kind == HELLO && s.reply.is_none() {
            // The first RRLS to ask owns this window.
            let Ok(commit) = <[u8; 32]>::try_from(m.body) else {
                return effects;
            };
            s.nonce = m.nonce;
            s.commit = commit;
            let key = match agreement::EphemeralPrivateKey::generate(&agreement::X25519, rng) {
                Ok(key) => key,
                Err(_) => return effects,
            };
            let Ok(public) = key.compute_public_key() else {
                return effects;
            };
            s.own_public = public.as_ref().try_into().unwrap();
            s.ephemeral = Some(key);
            let reply = frame(KEY, &s.nonce, &[&s.own_public, &url_field(&s.own_url)]);
            s.reply = Some((HELLO, reply.clone()));
            effects.send.push(reply);
            return effects;
        }
        if m.nonce != s.nonce || s.reply.is_none() {
            return effects;
        }
        if m.kind == ABORT {
            if s.phase != Phase::Saved {
                s.phase = Phase::Failed;
                s.message = "The RRLS cancelled pairing; nothing was saved on this station.".into();
                s.reply = None;
            }
            return effects;
        }
        // A retried request gets the same answer.
        if let Some((kind, reply)) = &s.reply
            && *kind == m.kind
            && m.kind != CONFIRM
        {
            effects.send.push(reply.clone());
            return effects;
        }
        match m.kind {
            REVEAL if s.derived.is_none() => {
                let Some((rrls_public, rrls_url)) = key_and_url(m.body) else {
                    return effects;
                };
                if commitment(&rrls_public, &rrls_url) != s.commit {
                    s.phase = Phase::Failed;
                    s.message =
                        "The RRLS key did not match its commitment; nothing was saved.".into();
                    s.reply = None;
                    return effects;
                }
                let t = transcript(
                    &s.nonce,
                    &s.commit,
                    &s.own_public,
                    &s.own_url,
                    &rrls_public,
                    &rrls_url,
                );
                let Some(key) = s.ephemeral.take() else {
                    return effects;
                };
                let Ok(d) = agreement::agree_ephemeral(
                    key,
                    &agreement::UnparsedPublicKey::new(&agreement::X25519, rrls_public),
                    |shared| derive(shared, t),
                ) else {
                    s.phase = Phase::Failed;
                    s.message = "Key agreement failed; nothing was saved.".into();
                    s.reply = None;
                    return effects;
                };
                let mut seed = [0u8; 32];
                let mut id = [0u8; 16];
                if rng.fill(&mut seed).is_err() || rng.fill(&mut id).is_err() {
                    return effects;
                }
                let bundle = Bundle {
                    side: Side::Primary,
                    seed,
                    installation_id: installation_id(id),
                    own_url: s.own_url.clone(),
                    peer_url: rrls_url.clone(),
                };
                let mut plain = seed.to_vec();
                plain.extend_from_slice(bundle.installation_id.as_bytes());
                let reply = frame(BUNDLE, &s.nonce, &[&seal(&d.key, &d.transcript, &plain)]);
                s.derived = Some(d);
                s.bundle = Some(bundle);
                s.peer_url = Some(rrls_url);
                s.phase = Phase::Compare;
                s.message = "Compare this code with the RRLS screen, then confirm it here.".into();
                s.reply = Some((REVEAL, reply.clone()));
                effects.send.push(reply);
            }
            CONFIRM => {
                let (Some(d), Some(bundle)) = (&s.derived, &s.bundle) else {
                    return effects;
                };
                if !verify(&bundle.seed, b"GS26PAIR1-confirm", &d.transcript, m.body) {
                    return effects;
                }
                s.peer_confirmed = true;
                if s.local_confirmed {
                    let done = frame(
                        DONE,
                        &s.nonce,
                        &[&sign(&bundle.seed, b"GS26PAIR1-done", &d.transcript)],
                    );
                    if s.phase != Phase::Saved {
                        s.phase = Phase::Saved;
                        s.message = "Paired. Restart this station's service to activate it.".into();
                        effects.save = Some(bundle.clone());
                    }
                    effects.send.push(done);
                } else if s.phase == Phase::Compare {
                    s.message =
                        "The RRLS admin confirmed. Compare the code and confirm it here.".into();
                }
            }
            _ => {}
        }
        effects
    }
    fn rrls_receive(&mut self, m: Parsed<'_>) -> Effects {
        let mut effects = Effects::default();
        let s = self.session.as_mut().unwrap();
        if m.nonce != s.nonce {
            return effects;
        }
        match m.kind {
            ABORT if s.phase != Phase::Saved => {
                s.phase = Phase::Failed;
                s.message =
                    "The primary cancelled pairing; nothing was saved on this station.".into();
                s.request = None;
            }
            KEY if s.derived.is_none() && s.peer_url.is_none() => {
                let Some((primary_public, primary_url)) = key_and_url(m.body) else {
                    return effects;
                };
                let t = transcript(
                    &s.nonce,
                    &s.commit,
                    &primary_public,
                    &primary_url,
                    &s.own_public,
                    &s.own_url,
                );
                let Some(key) = s.ephemeral.take() else {
                    return effects;
                };
                let Ok(d) = agreement::agree_ephemeral(
                    key,
                    &agreement::UnparsedPublicKey::new(&agreement::X25519, primary_public),
                    |shared| derive(shared, t),
                ) else {
                    s.phase = Phase::Failed;
                    s.message = "Key agreement failed; nothing was saved.".into();
                    s.request = None;
                    return effects;
                };
                // Reveal only now that the primary's key is fixed.
                let reveal = frame(REVEAL, &s.nonce, &[&s.own_public, &url_field(&s.own_url)]);
                s.peer_url = Some(primary_url);
                s.request = Some(reveal.clone());
                s.last_sent = None;
                // The code stays hidden until the primary's sealed bundle proves
                // it computed the same keys.
                s.message = "Primary answered; exchanging keys.".into();
                s.pending = Some(d);
            }
            BUNDLE if s.derived.is_none() => {
                let Some(Derived {
                    transcript: t,
                    key,
                    code,
                }) = s.pending
                else {
                    return effects;
                };
                if m.body.len() != BUNDLE_PLAIN + TAG {
                    return effects;
                }
                let Some(plain) = open(&key, &t, m.body) else {
                    return effects;
                };
                let Ok(id) = std::str::from_utf8(&plain[32..]) else {
                    return effects;
                };
                if id.len() != INSTALLATION_LEN
                    || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                {
                    return effects;
                }
                s.bundle = Some(Bundle {
                    side: Side::Rrls,
                    seed: plain[..32].try_into().unwrap(),
                    installation_id: id.into(),
                    own_url: s.own_url.clone(),
                    peer_url: s.peer_url.clone().unwrap_or_default(),
                });
                s.derived = Some(Derived {
                    transcript: t,
                    key,
                    code,
                });
                s.pending = None;
                s.request = None;
                s.phase = Phase::Compare;
                s.message =
                    "Compare this code with the primary screen, then confirm it here.".into();
            }
            DONE if s.local_confirmed && s.phase == Phase::AwaitingPeer => {
                let (Some(d), Some(bundle)) = (&s.derived, &s.bundle) else {
                    return effects;
                };
                if verify(&bundle.seed, b"GS26PAIR1-done", &d.transcript, m.body) {
                    s.peer_confirmed = true;
                    s.phase = Phase::Saved;
                    s.request = None;
                    s.message = "Paired. Restart this station's service to activate it.".into();
                    effects.save = Some(bundle.clone());
                }
            }
            _ => {}
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pump(from: &mut Exchange, to: &mut Exchange, sent: Vec<Vec<u8>>, now: u64) -> Effects {
        let mut out = Effects::default();
        for wire in sent {
            let e = to.receive(&wire, now);
            out.send.extend(e.send);
            if e.save.is_some() {
                out.save = e.save;
            }
        }
        let _ = from;
        out
    }

    fn run_to_codes(p: &mut Exchange, r: &mut Exchange) {
        p.start(Some("https://GroundStation.local:443/"), 0).unwrap();
        r.start(Some("http://rrls.local:3000"), 0).unwrap();
        let hello = r.poll(0, true).send;
        assert_eq!(hello.len(), 1);
        let key = pump(r, p, hello, 1).send;
        let reveal = pump(p, r, key, 2).send;
        assert!(
            reveal.is_empty(),
            "RRLS reveals on its next request, not as a reply"
        );
        let reveal = r.poll(3, true).send;
        let bundle = pump(r, p, reveal, 4).send;
        assert!(pump(p, r, bundle, 5).send.is_empty());
    }

    #[test]
    fn pairing_completes_with_no_url_on_either_side() {
        let (mut p, mut r) = (Exchange::new(Side::Primary), Exchange::new(Side::Rrls));
        p.start_origin(String::new(), 0).unwrap();
        r.start_origin(String::new(), 0).unwrap();
        let hello = r.poll(0, true).send;
        let key = pump(&mut r, &mut p, hello, 1).send;
        assert!(pump(&mut p, &mut r, key, 2).send.is_empty());
        let reveal = r.poll(3, true).send;
        let bundle = pump(&mut r, &mut p, reveal, 4).send;
        assert!(pump(&mut p, &mut r, bundle, 5).send.is_empty());
        let (pv, rv) = (p.view(6).unwrap(), r.view(6).unwrap());
        assert_eq!(pv.phase, Phase::Compare);
        assert_eq!(pv.code, rv.code);
        assert_eq!((pv.peer_url, rv.peer_url), (None, None));
        let code = pv.code.unwrap();
        r.confirm(&code, 7).unwrap();
        let confirm = r.poll(8, true).send;
        pump(&mut r, &mut p, confirm, 9);
        let saved = p.confirm(&code, 10).unwrap().save.unwrap();
        let json = saved.pairing_json();
        assert_eq!(json["peers"][0]["urls"], serde_json::json!([]));
        assert!(matches!(json["scheme"].as_str(), Some("https" | "http")));
        assert!(json["port"].as_u64().is_some_and(|p| p > 0));
    }

    #[test]
    fn a_station_without_a_url_still_learns_the_peer_url_bound_in_the_code() {
        let (mut p, mut r) = (Exchange::new(Side::Primary), Exchange::new(Side::Rrls));
        p.start_origin("https://192.168.3.7".into(), 0).unwrap();
        r.start_origin(String::new(), 0).unwrap();
        let hello = r.poll(0, true).send;
        let key = pump(&mut r, &mut p, hello, 1).send;
        pump(&mut p, &mut r, key, 2);
        let reveal = r.poll(3, true).send;
        let bundle = pump(&mut r, &mut p, reveal, 4).send;
        pump(&mut p, &mut r, bundle, 5);
        assert_eq!(r.view(6).unwrap().peer_url.as_deref(), Some("https://192.168.3.7"));
        assert_eq!(p.view(6).unwrap().peer_url, None);
        assert_eq!(p.view(6).unwrap().code, r.view(6).unwrap().code);
    }

    #[test]
    fn advertised_lan_origins_are_valid_https_origins() {
        assert_eq!(lan_origin("192.168.3.7".parse().unwrap(), ("https", 443)), "https://192.168.3.7");
        assert_eq!(lan_origin("10.0.0.2".parse().unwrap(), ("http", 3000)), "http://10.0.0.2:3000");
        assert_eq!(lan_origin("fe80::1".parse().unwrap(), ("https", 8443)), "https://[fe80::1]:8443");
        let advertised = advertised_origin();
        assert!(advertised.is_empty() || origin(&advertised).as_deref() == Ok(advertised.as_str()));
        assert!(key_and_url(&[&[1u8; 32][..], &[0]].concat()).is_some(), "empty URL field");
        assert!(key_and_url(&[&[1u8; 32][..], &[3], b"bad"].concat()).is_none());
    }

    #[test]
    fn both_sides_show_the_same_code_and_save_matching_bundles_only_after_both_confirm() {
        let (mut p, mut r) = (Exchange::new(Side::Primary), Exchange::new(Side::Rrls));
        run_to_codes(&mut p, &mut r);
        let (pv, rv) = (p.view(6).unwrap(), r.view(6).unwrap());
        assert_eq!(pv.phase, Phase::Compare);
        assert_eq!(rv.phase, Phase::Compare);
        assert!(pv.code.is_some());
        assert_eq!(pv.code, rv.code);
        assert_eq!(pv.peer_url.as_deref(), Some("http://rrls.local:3000"));
        assert_eq!(rv.peer_url.as_deref(), Some("https://groundstation.local"));
        let code = pv.code.unwrap();
        assert!(r.confirm("000 000x", 7).is_err() || code == "000 000");
        // RRLS confirms first: the primary must not save until its own admin confirms.
        assert!(r.confirm(&code, 7).unwrap().save.is_none());
        let confirm = r.poll(8, true).send;
        let e = pump(&mut r, &mut p, confirm, 9);
        assert!(e.save.is_none() && e.send.is_empty());
        assert!(p.view(9).unwrap().peer_confirmed);
        let saved_primary = p.confirm(&code, 10).unwrap().save.unwrap();
        // The RRLS retries CONFIRM and receives DONE.
        let confirm = r.poll(8 + RESEND_MS, true).send;
        let done = pump(&mut r, &mut p, confirm, 8 + RESEND_MS).send;
        assert_eq!(done.len(), 1);
        let saved_rrls = pump(&mut p, &mut r, done, 9 + RESEND_MS).save.unwrap();
        assert_eq!(saved_primary.seed, saved_rrls.seed);
        assert_eq!(saved_primary.installation_id, saved_rrls.installation_id);
        let a = saved_primary.pairing_json();
        let b = saved_rrls.pairing_json();
        assert_eq!(a["station_id"], "primary");
        assert_eq!(b["station_id"], "rrls");
        assert_eq!(a["peers"][0]["urls"][0], "http://rrls.local:3000");
        assert_eq!(b["peers"][0]["urls"][0], "https://groundstation.local");
        assert_eq!(a["scheme"], "https");
        assert_eq!(a["port"], 443);
        assert_eq!(b["scheme"], "http");
        assert_eq!(b["port"], 3000);
        assert_eq!(r.view(20).unwrap().phase, Phase::Saved);
        assert!(
            r.poll(30_000, true).send.is_empty(),
            "no traffic after saving"
        );
    }

    #[test]
    fn primary_confirm_first_saves_when_the_rrls_proof_arrives() {
        let (mut p, mut r) = (Exchange::new(Side::Primary), Exchange::new(Side::Rrls));
        run_to_codes(&mut p, &mut r);
        let code = p.view(6).unwrap().code.unwrap();
        assert!(p.confirm(&code, 6).unwrap().save.is_none());
        assert_eq!(p.view(6).unwrap().phase, Phase::AwaitingPeer);
        r.confirm(&code, 7).unwrap();
        let confirm = r.poll(7, true).send;
        let e = pump(&mut r, &mut p, confirm, 8);
        assert!(e.save.is_some());
        assert!(pump(&mut p, &mut r, e.send, 9).save.is_some());
    }

    #[test]
    fn wrong_code_cancel_and_expiry_save_nothing() {
        let (mut p, mut r) = (Exchange::new(Side::Primary), Exchange::new(Side::Rrls));
        run_to_codes(&mut p, &mut r);
        let code = p.view(6).unwrap().code.unwrap();
        let wrong = if code == "123 456" {
            "654 321"
        } else {
            "123 456"
        };
        assert!(p.confirm(wrong, 6).is_err());
        assert!(!p.view(6).unwrap().local_confirmed);
        // Cancel on the primary tells the RRLS, which then never saves.
        let abort = p.cancel().send;
        assert_eq!(abort.len(), 1);
        pump(&mut p, &mut r, abort, 7);
        assert_eq!(r.view(7).unwrap().phase, Phase::Failed);
        assert!(r.confirm(&code, 8).is_err());
        // Expiry.
        let (mut p, mut r) = (Exchange::new(Side::Primary), Exchange::new(Side::Rrls));
        run_to_codes(&mut p, &mut r);
        let code = p.view(6).unwrap().code.unwrap();
        assert_eq!(p.view(WINDOW_MS).unwrap().phase, Phase::Failed);
        assert!(p.confirm(&code, WINDOW_MS).is_err());
        assert!(r.poll(WINDOW_MS, true).send.is_empty());
    }

    #[test]
    fn a_relay_cannot_substitute_keys_or_forge_confirmation() {
        // The RRLS's reveal must match its earlier commitment.
        let (mut p, mut r) = (Exchange::new(Side::Primary), Exchange::new(Side::Rrls));
        p.start(Some("https://gs.local"), 0).unwrap();
        r.start(Some("https://rrls.local"), 0).unwrap();
        let hello = r.poll(0, true).send;
        let key = pump(&mut r, &mut p, hello, 1).send;
        pump(&mut p, &mut r, key, 2);
        let mut reveal = r.poll(3, true).send.remove(0);
        reveal[20] ^= 1; // substitute the RRLS public key
        assert!(pump(&mut r, &mut p, vec![reveal], 4).send.is_empty());
        assert_eq!(p.view(4).unwrap().phase, Phase::Failed);
        assert!(p.view(4).unwrap().code.is_none());

        // A tampered bundle never yields a code on the RRLS.
        let (mut p, mut r) = (Exchange::new(Side::Primary), Exchange::new(Side::Rrls));
        p.start(Some("https://gs.local"), 0).unwrap();
        r.start(Some("https://rrls.local"), 0).unwrap();
        let hello = r.poll(0, true).send;
        let key = pump(&mut r, &mut p, hello, 1).send;
        pump(&mut p, &mut r, key, 2);
        let reveal = r.poll(3, true).send;
        let mut bundle = pump(&mut r, &mut p, reveal, 4).send.remove(0);
        *bundle.last_mut().unwrap() ^= 1;
        pump(&mut p, &mut r, vec![bundle], 5);
        assert!(r.view(5).unwrap().code.is_none());

        // An unsigned CONFIRM cannot make the primary save.
        let (mut p, mut r) = (Exchange::new(Side::Primary), Exchange::new(Side::Rrls));
        run_to_codes(&mut p, &mut r);
        let code = p.view(6).unwrap().code.unwrap();
        p.confirm(&code, 6).unwrap();
        let nonce = p.session.as_ref().unwrap().nonce;
        let forged = frame(CONFIRM, &nonce, &[&[0u8; 64]]);
        assert!(p.receive(&forged, 7).save.is_none());
        // A second RRLS cannot take over an owned window.
        let mut other = Exchange::new(Side::Rrls);
        other.start(Some("https://other.local"), 0).unwrap();
        let hello = other.poll(0, true).send;
        assert!(pump(&mut other, &mut p, hello, 8).send.is_empty());
    }

    #[test]
    fn closed_window_and_garbage_are_ignored() {
        let mut p = Exchange::new(Side::Primary);
        let mut r = Exchange::new(Side::Rrls);
        r.start(Some("https://rrls.local"), 0).unwrap();
        let hello = r.poll(0, true).send;
        assert!(
            pump(&mut r, &mut p, hello.clone(), 1).send.is_empty(),
            "no window on the primary"
        );
        assert!(
            r.poll(1, false).send.is_empty(),
            "no radio link, no traffic"
        );
        assert!(
            r.poll(1, true).send.is_empty(),
            "resend waits for the interval"
        );
        assert_eq!(r.poll(RESEND_MS, true).send, hello);
        p.start(Some("https://gs.local"), 0).unwrap();
        for junk in [&b"SP\x01"[..], b"SP\x01\x01short", &[0u8; 600]] {
            assert!(p.receive(junk, 1).send.is_empty());
        }
    }

    #[test]
    fn origins_match_the_provisioning_tool() {
        assert_eq!(
            origin("https://GroundStation.local:443/").unwrap(),
            "https://groundstation.local"
        );
        assert_eq!(
            origin("http://RRLS.local:3000").unwrap(),
            "http://rrls.local:3000"
        );
        assert_eq!(
            origin("http://[FE80::1]:8080").unwrap(),
            "http://[fe80::1]:8080"
        );
        for bad in [
            "http://host:invalid",
            "ftp://host",
            "https://user:secret@host",
            "https://host/api",
            "https://host?token=secret",
            "https://host/#fragment",
            "https://",
            "https://host:0",
        ] {
            assert!(origin(bad).is_err(), "{bad}");
        }
        assert_eq!(scheme_port("http://rrls.local:3000"), ("http".into(), 3000));
        assert_eq!(scheme_port("https://gs.local"), ("https".into(), 443));
        assert_eq!(scheme_port("http://[fe80::1]"), ("http".into(), 80));
        assert_eq!(scheme_port("http://[fe80::1]:81"), ("http".into(), 81));
        let id = installation_id([0xab; 16]);
        assert_eq!(id.len(), INSTALLATION_LEN);
        assert_eq!(&id[14..15], "4");
    }
}
