//! Bounded local WebSocket crew voice, with an opt-in RRLS Codec2 radio bridge.
//! Camera/video and voice recording are outside the radio voice path.
use crate::{auth::Permission, state::AppState};
#[path = "voice/pairing.rs"]
mod pairing;
#[path = "voice/program.rs"]
mod program;
use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::{Html, IntoResponse},
    routing::get,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};

const FRAME_BYTES: usize = 640; // mono signed PCM16 little endian, 16 kHz, 20 ms
const FRAME_SAMPLES: usize = FRAME_BYTES / 2;
const MAX_PEERS: usize = 12;
/// Largest Opus packet accepted for one 20 ms frame (24 kbit/s needs ~60 B).
const MAX_OPUS_BYTES: usize = 400;
/// Opus bit rate the station encodes PCM speakers at for Opus listeners.
const OPUS_BITRATE: i32 = 24_000;
/// Sender id of audio received over the RRLS radio bridge.
const RADIO_SPEAKER: u32 = 0xffff_fffe;

/// Audio encoding of one participant's binary frames, chosen in the `auth`
/// message. Browsers use PCM16 (640 B per 20 ms); the watch apps use Opus
/// (one packet per 20 ms, 16 kHz mono) to save radio time and battery. The
/// station decodes Opus to PCM16 for the shared pipeline (relay to PCM
/// listeners, audience program, radio bridge) and sends each listener audio
/// in its own codec, so both kinds share one channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Codec {
    #[default]
    Pcm16,
    Opus,
}

fn opus_encoder() -> Option<opus::Encoder> {
    let mut encoder =
        opus::Encoder::new(16_000, opus::Channels::Mono, opus::Application::Voip).ok()?;
    encoder.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE)).ok()?;
    Some(encoder)
}

/// One 20 ms PCM16 frame to an Opus packet; the encoder is created on first use.
fn opus_encode(encoder: &mut Option<opus::Encoder>, pcm: &[u8]) -> Option<Vec<u8>> {
    if encoder.is_none() {
        *encoder = opus_encoder();
    }
    let samples: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    let mut out = [0u8; MAX_OPUS_BYTES];
    let n = encoder.as_mut()?.encode(&samples, &mut out).ok()?;
    Some(out[..n].to_vec())
}

/// One Opus packet to exactly one 20 ms PCM16 frame; anything else is refused.
fn opus_decode(decoder: &mut opus::Decoder, packet: &[u8]) -> Option<Vec<u8>> {
    if packet.is_empty() || packet.len() > MAX_OPUS_BYTES {
        return None;
    }
    let mut samples = [0i16; FRAME_SAMPLES];
    let n = decoder.decode(packet, &mut samples, false).ok()?;
    (n == FRAME_SAMPLES).then(|| samples.iter().flat_map(|s| s.to_le_bytes()).collect())
}

fn framed(speaker: u32, payload: &[u8]) -> Message {
    let mut packet = Vec::with_capacity(4 + payload.len());
    packet.extend_from_slice(&speaker.to_le_bytes());
    packet.extend_from_slice(payload);
    Message::Binary(packet.into())
}

struct AudioFrame {
    sequence: u64,
    speaker: u32,
    created: std::time::Instant,
    pcm: Vec<u8>,
}
#[derive(Default)]
struct BroadcastHistory {
    enabled: bool,
    commentary_enabled: bool,
    generation: u64,
    next: u64,
    frames: VecDeque<AudioFrame>,
}
#[derive(Default)]
pub(crate) struct VoiceHub {
    history: Mutex<BroadcastHistory>,
}

impl VoiceHub {
    pub(crate) async fn set_mix(&self, enabled: bool, commentary: bool) {
        let mut history = self.history.lock().await;
        if history.enabled != enabled || history.commentary_enabled != commentary {
            history.enabled = enabled;
            history.commentary_enabled = commentary;
            history.generation += 1;
            history.frames.clear();
        }
    }
    async fn enabled(&self) -> bool {
        self.history.lock().await.enabled
    }
    async fn commentary_enabled(&self) -> bool {
        self.history.lock().await.commentary_enabled
    }
    async fn capture(&self, speaker: u32, pcm: &[u8]) {
        self.capture_source(speaker, pcm, false).await;
    }
    async fn capture_program(&self, speaker: u32, pcm: &[u8]) {
        self.capture_source(speaker, pcm, true).await;
    }
    async fn capture_source(&self, speaker: u32, pcm: &[u8], commentary: bool) {
        let mut history = self.history.lock().await;
        if if commentary {
            !history.commentary_enabled
        } else {
            !history.enabled
        } {
            return;
        }
        history.next += 1;
        let sequence = history.next;
        history.frames.push_back(AudioFrame {
            sequence,
            speaker,
            created: std::time::Instant::now(),
            pcm: pcm.to_vec(),
        });
        while history.frames.len() > 45_000
            || history
                .frames
                .front()
                .is_some_and(|f| f.created.elapsed() > Duration::from_secs(70))
        {
            history.frames.pop_front();
        }
    }
    /// Monotonic server-side release gate. A viewer cannot request a shorter delay.
    pub(crate) async fn audience_batch(&self, after: u64, delay_seconds: u32) -> serde_json::Value {
        let mut history = self.history.lock().await;
        while history
            .frames
            .front()
            .is_some_and(|f| f.created.elapsed() > Duration::from_secs(70))
        {
            history.frames.pop_front();
        }
        let delay = Duration::from_millis(u64::from(delay_seconds) * 1000 + 2500);
        let mut cursor = after.min(history.next);
        let mut frames = Vec::new();
        if history.enabled || history.commentary_enabled {
            let start = history
                .frames
                .partition_point(|frame| frame.sequence <= cursor);
            for frame in history.frames.iter().skip(start) {
                let age = frame.created.elapsed();
                if age < delay {
                    break;
                }
                cursor = frame.sequence;
                if age <= delay + Duration::from_millis(250) {
                    frames.push(serde_json::json!({"id":frame.speaker,"pcm":base64::engine::general_purpose::STANDARD.encode(&frame.pcm)}));
                }
            }
        }
        serde_json::json!({"enabled":history.enabled||history.commentary_enabled,"generation":history.generation,"cursor":cursor,"frames":frames})
    }
}

struct Peer {
    name: String,
    transmitting: bool,
    radio_enabled: bool,
    codec: Codec,
    /// Echo-avoidance group (`auth::voice_device_group`): a phone session and
    /// the watch it linked share one.
    group: String,
    /// Joined with a watch token.
    watch: bool,
    /// Watch floor claim (see `pairing`); `None` when yielded or not a watch.
    active_since: Option<u64>,
    /// Paused because a watch of the same group holds the floor.
    held: bool,
    tx: mpsc::Sender<Message>,
}

impl Peer {
    fn member(&self, id: u32) -> pairing::Member<'_> {
        pairing::Member {
            id,
            group: &self.group,
            watch: self.watch,
            active_since: self.active_since,
        }
    }
}

struct VoiceState {
    app: Arc<AppState>,
    hub: Arc<VoiceHub>,
    peers: Mutex<BTreeMap<u32, Peer>>,
    next: AtomicU32,
    claims: AtomicU64,
    program_slots: Arc<tokio::sync::Semaphore>,
}

/// Sends one 20 ms frame of `speaker` to every listener it may reach (see
/// `pairing`), in each listener's codec. `speaker_opus` is the speaker's own
/// Opus packet when it sent one; PCM speakers are encoded once per frame
/// with `encoder` and only when some listener uses Opus.
fn fan_out(
    peers: &BTreeMap<u32, Peer>,
    speaker: &pairing::Member<'_>,
    pcm: &[u8],
    speaker_opus: Option<&[u8]>,
    encoder: &mut Option<opus::Encoder>,
) {
    let held: std::collections::BTreeSet<u32> =
        peers.iter().filter(|(_, p)| p.held).map(|(id, _)| *id).collect();
    let mut pcm_packet: Option<Message> = None;
    let mut opus_packet: Option<Option<Message>> = None;
    for (id, peer) in peers {
        if !pairing::delivers(speaker, &peer.member(*id), &held) {
            continue;
        }
        let packet = match peer.codec {
            Codec::Pcm16 => Some(
                pcm_packet
                    .get_or_insert_with(|| framed(speaker.id, pcm))
                    .clone(),
            ),
            Codec::Opus => opus_packet
                .get_or_insert_with(|| match speaker_opus {
                    Some(packet) => Some(framed(speaker.id, packet)),
                    None => opus_encode(encoder, pcm).map(|p| framed(speaker.id, &p)),
                })
                .clone(),
        };
        if let Some(packet) = packet {
            let _ = peer.tx.try_send(packet);
        }
    }
}

/// Recomputes who is held (`pairing::held`), stops a newly held peer's
/// microphone, tells the peers whose state changed, and sends the roster.
fn rebalance(peers: &mut BTreeMap<u32, Peer>) {
    let held = {
        let members: Vec<_> = peers.iter().map(|(id, p)| p.member(*id)).collect();
        pairing::held(&members)
    };
    for (id, peer) in peers.iter_mut() {
        let now_held = held.contains(id);
        if now_held == peer.held {
            continue;
        }
        peer.held = now_held;
        if now_held {
            peer.transmitting = false;
            peer.radio_enabled = false;
            crate::radio_voice::release(*id);
        }
        let _ = peer.tx.try_send(text(serde_json::json!({
            "type": "handoff",
            "held": now_held,
            "by": if now_held { "watch" } else { "" },
        })));
    }
    roster(peers);
}

pub fn routes(app: Arc<AppState>, hub: Arc<VoiceHub>) -> Router<Arc<AppState>> {
    let voice_state = Arc::new(VoiceState {
        app,
        hub,
        peers: Mutex::new(BTreeMap::new()),
        next: AtomicU32::new(1),
        claims: AtomicU64::new(1),
        program_slots: Arc::new(tokio::sync::Semaphore::new(4)),
    });
    if crate::rrls::voice_status().0 {
        let weak = Arc::downgrade(&voice_state);
        let mut audio = crate::radio_voice::subscribe();
        tokio::spawn(async move {
            let mut encoder = None;
            let radio = pairing::Member {
                id: RADIO_SPEAKER,
                group: "radio",
                watch: false,
                active_since: None,
            };
            loop {
                let frame = match audio.recv().await {
                    Ok(frame) => frame,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                if frame.created.elapsed() > Duration::from_millis(180) {
                    continue;
                }
                let Some(state) = weak.upgrade() else {
                    break;
                };
                // A radio packet holds 320 ms. Browsers keep under 200 ms
                // queued per speaker, so release it at real time after a
                // 60 ms head start instead of in one burst.
                for (index, pcm) in frame.pcm.as_chunks::<320>().0.iter().enumerate() {
                    if index >= 3 {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                    fan_out(&*state.peers.lock().await, &radio, &bytes, None, &mut encoder);
                    state.hub.capture(RADIO_SPEAKER, &bytes).await;
                }
            }
        });
    }
    Router::new()
        .route("/api/voice/ws", get(upgrade))
        .route("/api/voice/status", get(capabilities))
        .route("/api/program/commentary/ws", get(program::upgrade))
        .route(
            "/studio-audio",
            get(|| async { Html(gs_browser_assets::STUDIO_AUDIO_HTML) }),
        )
        .route(
            "/assets/studio-audio.js",
            get(|| async {
                (
                    [("Content-Type", "text/javascript")],
                    gs_browser_assets::STUDIO_AUDIO_JS,
                )
            }),
        )
        .with_state(voice_state)
        .route(
            "/radio",
            get(|| async { Html(gs_browser_assets::VOICE_HTML) }),
        )
        .route(
            "/assets/voice.js",
            get(|| async {
                (
                    [("Content-Type", "text/javascript")],
                    gs_browser_assets::VOICE_JS,
                )
            }),
        )
        .route(
            "/assets/voice-worklet.js",
            get(|| async {
                (
                    [("Content-Type", "text/javascript")],
                    gs_browser_assets::VOICE_WORKLET_JS,
                )
            }),
        )
        .route(
            "/assets/media-login.js",
            get(|| async {
                (
                    [("Content-Type", "text/javascript")],
                    gs_browser_assets::MEDIA_LOGIN_JS,
                )
            }),
        )
}

async fn capabilities(
    State(state): State<Arc<VoiceState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let mut response = match crate::web::authorize_headers(&state.app, &headers, Permission::ViewData).await {
        Ok(p) => axum::Json(serde_json::json!({"can_transmit":!p.anonymous && p.permissions.allows(Permission::VoiceTransmit),"can_listen":!p.anonymous&&p.permissions.allows(Permission::VoiceTransmit),"can_commentate":!p.anonymous&&p.can_manage_stream(),"commentary_enabled":state.hub.commentary_enabled().await,"broadcast_enabled":state.hub.enabled().await,"radio":crate::radio_voice::status()})).into_response(),
        Err(response) => response,
    };
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

async fn upgrade(State(state): State<Arc<VoiceState>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.max_message_size(2048)
        .max_frame_size(2048)
        .on_upgrade(move |socket| connection(state, socket))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Control {
    Auth {
        token: String,
        /// Binary frame encoding; PCM16 when absent (browsers).
        #[serde(default)]
        codec: Codec,
    },
    Transmitting {
        enabled: bool,
    },
    Radio {
        enabled: bool,
    },
    /// A watch claims (`true`) or yields (`false`) its group's floor; the
    /// paired phone is held while a watch holds it. Ignored for browsers.
    Active {
        enabled: bool,
    },
    Ping,
}

fn text(value: serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}
fn roster(peers: &BTreeMap<u32, Peer>) {
    let message = text(
        serde_json::json!({"type":"roster", "peers":peers.iter().map(|(id,p)|
        serde_json::json!({"id":id,"name":p.name,"transmitting":p.transmitting,"device":if p.watch {"watch"} else {"web"}})).collect::<Vec<_>>() }),
    );
    for peer in peers.values() {
        let _ = peer.tx.try_send(message.clone());
    }
}

async fn reject(socket: &mut WebSocket, reason: &str) {
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        socket.send(text(serde_json::json!({"type":"error","message":reason}))),
    )
    .await;
}

async fn connection(state: Arc<VoiceState>, mut socket: WebSocket) {
    // Authenticate in the first frame: login tokens never appear in URLs or proxy logs.
    let (token, codec) = match tokio::time::timeout(Duration::from_secs(5), socket.recv()).await {
        Ok(Some(Ok(Message::Text(raw)))) => match serde_json::from_str::<Control>(&raw) {
            Ok(Control::Auth { token, codec }) if !token.is_empty() => (token, codec),
            _ => {
                reject(&mut socket, "Sign in before joining voice").await;
                return;
            }
        },
        _ => return,
    };
    let principal = match state
        .app
        .auth
        .authorize_token(&state.app.auth_db, Some(&token), Permission::ViewData)
        .await
    {
        Ok(p)
            if !p.anonymous
                && p.permissions.allows(Permission::VoiceTransmit) =>
        {
            p
        }
        _ => {
            reject(
                &mut socket,
                "A signed-in account with the voice_transmit permission is required",
            )
            .await;
            return;
        }
    };
    let mut can_transmit = principal.permissions.allows(Permission::VoiceTransmit);
    let watch = crate::auth::is_watch_token(&token);
    let group = crate::auth::voice_device_group(&state.app.auth_db, &token).await;
    let mut decoder = match codec {
        Codec::Opus => match opus::Decoder::new(16_000, opus::Channels::Mono) {
            Ok(decoder) => Some(decoder),
            Err(_) => {
                reject(&mut socket, "Opus voice is unavailable on this station").await;
                return;
            }
        },
        Codec::Pcm16 => None,
    };
    // This speaker's frames encoded once for every Opus listener.
    let mut encoder: Option<opus::Encoder> = None;
    let id = state.next.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::channel(16); // bounded to about 320 ms of audio per listener
    {
        let mut peers = state.peers.lock().await;
        if peers.len() >= MAX_PEERS {
            drop(peers);
            reject(&mut socket, "Voice channel is full (12 participants)").await;
            return;
        }
        let _ = tx.try_send(text(serde_json::json!({
            "type":"joined","id":id,"sample_rate":16000,"codec":codec,
            "frame_samples":FRAME_SAMPLES,"device":if watch {"watch"} else {"web"}
        })));
        peers.insert(
            id,
            Peer {
                name: principal.username.unwrap_or_else(|| "Crew".into()),
                transmitting: false,
                radio_enabled: false,
                codec,
                group,
                watch,
                // A watch takes its group's floor when it joins.
                active_since: watch.then(|| state.claims.fetch_add(1, Ordering::Relaxed)),
                held: false,
                tx,
            },
        );
        rebalance(&mut peers);
    }
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let mut last_message = tokio::time::Instant::now();
    let mut budget = RateLimit::new();
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break; };
                last_message = tokio::time::Instant::now();
                match message {
                    Message::Binary(bytes) => {
                        if !budget.accept() { break; }
                        let (pcm, own_opus) = match decoder.as_mut() {
                            None if bytes.len() == FRAME_BYTES => (bytes.to_vec(), None),
                            None => break,
                            Some(decoder) => match opus_decode(decoder, &bytes) {
                                Some(pcm) => (pcm, Some(&bytes[..])),
                                None => break,
                            },
                        };
                        let peers = state.peers.lock().await;
                        let Some(me) = peers.get(&id) else { break; };
                        if !can_transmit || !me.transmitting || me.held { continue; }
                        let radio = me.radio_enabled;
                        fan_out(&peers, &me.member(id), &pcm, own_opus, &mut encoder);
                        drop(peers);
                        state.hub.capture(id, &pcm).await;
                        if radio {crate::radio_voice::submit(id,&pcm);}
                    }
                    Message::Text(raw) => {
                        if !budget.accept() { break; }
                        match serde_json::from_str::<Control>(&raw) {
                            Ok(Control::Transmitting { enabled }) => {
                                let mut peers = state.peers.lock().await;
                                if let Some(peer) = peers.get_mut(&id) {
                                    peer.transmitting = enabled && can_transmit && !peer.held;
                                    if enabled && peer.held {
                                        let _ = peer.tx.try_send(text(serde_json::json!({"type":"handoff","held":true,"by":"watch"})));
                                    }
                                }
                                roster(&peers);
                                drop(peers);
                                if !enabled {crate::radio_voice::release(id);}
                            }
                            Ok(Control::Active { enabled }) => {
                                let mut peers = state.peers.lock().await;
                                if let Some(peer) = peers.get_mut(&id).filter(|p| p.watch) {
                                    peer.active_since = enabled.then(|| state.claims.fetch_add(1, Ordering::Relaxed));
                                }
                                rebalance(&mut peers);
                            }
                            Ok(Control::Radio{enabled})=>{
                                let mut peers=state.peers.lock().await;
                                let allowed=enabled && can_transmit && crate::rrls::voice_status().1 && !peers.get(&id).is_some_and(|p|p.held);
                                if let Some(peer)=peers.get_mut(&id) {
                                    peer.radio_enabled=allowed;
                                    let _=peer.tx.try_send(text(serde_json::json!({"type":"radio_state","enabled":allowed})));
                                }
                                drop(peers);
                                if !allowed {crate::radio_voice::release(id);}
                            }
                            Ok(Control::Ping) => {},
                            _ => break,
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {},
                }
            }
            outgoing = rx.recv() => {
                let Some(message) = outgoing else { break; };
                if !matches!(tokio::time::timeout(Duration::from_secs(2), socket.send(message)).await, Ok(Ok(()))) { break; }
            }
            _ = interval.tick() => {
                if last_message.elapsed() > Duration::from_secs(40) { break; }
                let current=match state.app.auth.authorize_token(&state.app.auth_db,Some(&token),Permission::ViewData).await {Ok(p) if !p.anonymous && p.permissions.allows(Permission::VoiceTransmit)=>p,_=>break};
                can_transmit=current.permissions.allows(Permission::VoiceTransmit);
                if !can_transmit {if let Some(peer)=state.peers.lock().await.get_mut(&id){peer.transmitting=false;peer.radio_enabled=false;}crate::radio_voice::release(id);}
                roster(&*state.peers.lock().await);
                let radio=crate::radio_voice::status();
                if radio["available"].as_bool()!=Some(true) {
                    if let Some(peer)=state.peers.lock().await.get_mut(&id) {peer.radio_enabled=false;}
                    crate::radio_voice::release(id);
                }
                if !matches!(tokio::time::timeout(Duration::from_secs(2),socket.send(text(serde_json::json!({"type":"radio_link","radio":radio})))).await,Ok(Ok(()))) {break;}
                let enabled = state.hub.enabled().await;
                if !matches!(tokio::time::timeout(Duration::from_secs(2), socket.send(text(serde_json::json!({"type":"broadcast_state","enabled":enabled})))).await, Ok(Ok(()))) { break; }
            }
        }
    }
    crate::radio_voice::release(id);
    let mut peers = state.peers.lock().await;
    peers.remove(&id);
    // Leaving releases a phone this watch held.
    rebalance(&mut peers);
}

struct RateLimit {
    start: std::time::Instant,
    count: usize,
}
impl RateLimit {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            count: 0,
        }
    }
    fn accept(&mut self) -> bool {
        if self.start.elapsed() >= Duration::from_secs(1) {
            self.start = std::time::Instant::now();
            self.count = 0;
        }
        self.count += 1;
        self.count <= 100
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn audience_audio_is_delayed_and_disable_discards_private_history() {
        let hub = VoiceHub::default();
        hub.capture(1, &[0; FRAME_BYTES]).await;
        assert!(hub.history.lock().await.frames.is_empty());
        hub.set_mix(true, false).await;
        hub.capture(1, &[0; FRAME_BYTES]).await;
        assert_eq!(
            hub.audience_batch(0, 3).await["frames"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        hub.history.lock().await.frames[0].created =
            std::time::Instant::now() - Duration::from_millis(5510);
        let batch = hub.audience_batch(0, 3).await;
        assert_eq!(batch["frames"].as_array().unwrap().len(), 1);
        assert_eq!(
            hub.audience_batch(batch["cursor"].as_u64().unwrap(), 3)
                .await["frames"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        hub.set_mix(false, false).await;
        assert!(hub.history.lock().await.frames.is_empty());
        hub.capture(1, &[0; FRAME_BYTES]).await;
        hub.set_mix(true, false).await;
        assert_eq!(
            hub.audience_batch(0, 3).await["frames"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn voice_status_uses_current_admin_role_without_explicit_voice_grant() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        let app = export_state(&root.0, true).await;
        let save = |role: &str, disabled: bool| {
            std::fs::write(root.0.join("auth.json"), serde_json::json!({
                "anonymous":{"view_data":true},
                "users":[{"username":"administrator","roles":[role],"password":{"salt_b64":"","hash_b64":""},
                    "permissions":{"view_data":false,"send_commands":false,"voice_transmit":false},"disabled":disabled}]
            }).to_string()).unwrap();
        };
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('voice-admin-test','administrator','session',0,0,0,9999999999999)")
            .execute(&app.auth_db).await.unwrap();
        let state = Arc::new(VoiceState {
            app,
            hub: Arc::new(VoiceHub::default()),
            peers: Mutex::new(BTreeMap::new()),
            next: AtomicU32::new(1),
            claims: AtomicU64::new(1),
            program_slots: Arc::new(tokio::sync::Semaphore::new(4)),
        });
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer voice-admin-test".parse().unwrap(),
        );
        save("admin", false);
        let response = capabilities(State(state.clone()), headers.clone()).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers()[axum::http::header::CACHE_CONTROL],
            "no-store"
        );
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["can_transmit"], true);
        // Revocation is effective without replacing or reissuing the session.
        save("stream_admin", false);
        assert_eq!(
            capabilities(State(state.clone()), headers.clone())
                .await
                .status(),
            axum::http::StatusCode::FORBIDDEN
        );
        save("admin", true);
        assert_eq!(
            capabilities(State(state), headers).await.status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn voice_requires_explicit_current_permission_even_for_operators_and_stream_admins() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        let state = export_state(&root.0, true).await;
        let save = |allowed: bool, disabled: bool, view: bool| {
            std::fs::write(root.0.join("auth.json"), serde_json::json!({
                "anonymous":{"view_data":true,"voice_transmit":true},
                "users":[{"username":"speaker","roles":["stream_admin"],"password":{"salt_b64":"","hash_b64":""},
                    "permissions":{"view_data":view,"send_commands":false,"voice_transmit":allowed},"disabled":disabled}]
            }).to_string()).unwrap();
        };
        save(false, false, true);
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('voice-test','speaker','session',1,1,0,9999999999999)")
            .execute(&state.auth_db).await.unwrap();
        assert!(
            state
                .auth
                .authorize_token(
                    &state.auth_db,
                    Some("voice-test"),
                    Permission::VoiceTransmit
                )
                .await
                .is_err()
        );
        save(true, false, true);
        assert!(
            state
                .auth
                .authorize_token(
                    &state.auth_db,
                    Some("voice-test"),
                    Permission::VoiceTransmit
                )
                .await
                .is_ok()
        );
        assert!(
            state
                .auth
                .authorize_token(&state.auth_db, None, Permission::VoiceTransmit)
                .await
                .is_err()
        );
        for settings in [
            (false, false, true),
            (true, true, true),
            (true, false, false),
        ] {
            save(settings.0, settings.1, settings.2);
            assert!(
                state
                    .auth
                    .authorize_token(
                        &state.auth_db,
                        Some("voice-test"),
                        Permission::VoiceTransmit
                    )
                    .await
                    .is_err()
            );
        }
    }
    #[test]
    fn audio_and_control_are_bounded() {
        let mut limiter = RateLimit::new();
        for _ in 0..100 {
            assert!(limiter.accept());
        }
        assert!(!limiter.accept());
        assert!(
            serde_json::from_str::<Control>(r#"{"type":"transmitting","enabled":true,"id":3}"#)
                .is_err()
        );
    }
    // --- Watch voice: Opus bridge, pairing, scope (fake watch client) ---

    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

    fn sine_frame(index: usize, hz: f32) -> Vec<i16> {
        (0..FRAME_SAMPLES)
            .map(|i| {
                let t = (index * FRAME_SAMPLES + i) as f32 / 16_000.0;
                ((t * hz * std::f32::consts::TAU).sin() * 9_000.0) as i16
            })
            .collect()
    }
    fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }
    fn rms(pcm: &[u8]) -> f64 {
        let samples: Vec<f64> = pcm
            .chunks_exact(2)
            .map(|b| f64::from(i16::from_le_bytes([b[0], b[1]])))
            .collect();
        (samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64).sqrt()
    }

    struct Station {
        _root: crate::recording_export::tests::TestDirectory,
        app: Arc<AppState>,
        hub: Arc<VoiceHub>,
        address: std::net::SocketAddr,
    }

    async fn station() -> Station {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        let app = export_state(&root.0, true).await;
        std::fs::write(
            root.0.join("auth.json"),
            serde_json::json!({"users":[
                {"username":"ada","roles":[],"password":{"salt_b64":"","hash_b64":""},
                 "permissions":{"view_data":true,"voice_transmit":true}},
                {"username":"bob","roles":[],"password":{"salt_b64":"","hash_b64":""},
                 "permissions":{"view_data":true,"voice_transmit":true}},
                {"username":"carl","roles":[],"password":{"salt_b64":"","hash_b64":""},
                 "permissions":{"view_data":true,"send_commands":true}}
            ]})
            .to_string(),
        )
        .unwrap();
        for (token, name) in [("ada-phone", "ada"), ("bob-laptop", "bob"), ("carl-phone", "carl")] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,1,0,9999999999999)")
                .bind(token).bind(name).execute(&app.auth_db).await.unwrap();
        }
        let hub = Arc::new(VoiceHub::default());
        let router = routes(app.clone(), hub.clone())
            .merge(crate::watch::routes())
            .layer(axum::middleware::from_fn(crate::watch::scope_watch_tokens))
            .with_state(app.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Station { _root: root, app, hub, address }
    }

    async fn watch_token(station: &Station, parent: &str, device: &str, voice: bool) -> crate::auth::WatchTokenGrant {
        let principal = station
            .app
            .auth
            .authorize_token(&station.app.auth_db, Some(parent), Permission::ViewData)
            .await
            .unwrap();
        station
            .app
            .auth
            .issue_watch_token(&station.app.auth_db, parent, &principal, device, &[], voice, |_| false)
            .await
            .unwrap()
    }

    async fn connect(station: &Station, path: &str) -> Ws {
        tokio_tungstenite::connect_async(format!("ws://{}{path}", station.address))
            .await
            .unwrap()
            .0
    }

    async fn send_json(ws: &mut Ws, value: serde_json::Value) {
        ws.send(WsMessage::Text(value.to_string().into())).await.unwrap();
    }

    /// Next text message of `kind`, skipping everything else.
    async fn next_text(ws: &mut Ws, kind: &str) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match ws.next().await {
                    Some(Ok(WsMessage::Text(raw))) => {
                        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
                        if value["type"] == kind {
                            return value;
                        }
                    }
                    Some(Ok(_)) => {}
                    other => panic!("socket ended waiting for {kind}: {other:?}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {kind} message"))
    }

    /// Binary frames received within `window`, as (speaker, payload).
    async fn frames_within(ws: &mut Ws, window: Duration) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, ws.next()).await {
            if let WsMessage::Binary(bytes) = message {
                out.push((u32::from_le_bytes(bytes[..4].try_into().unwrap()), bytes[4..].to_vec()));
            }
        }
        out
    }

    async fn join(station: &Station, token: &str, codec: &str) -> (Ws, u32) {
        let mut ws = connect(station, "/api/voice/ws").await;
        send_json(&mut ws, serde_json::json!({"type":"auth","token":token,"codec":codec})).await;
        let joined = next_text(&mut ws, "joined").await;
        assert_eq!(joined["codec"], codec);
        (ws, joined["id"].as_u64().unwrap() as u32)
    }

    async fn rejection(station: &Station, path: &str, token: &str) -> String {
        let mut ws = connect(station, path).await;
        send_json(&mut ws, serde_json::json!({"type":"auth","token":token})).await;
        next_text(&mut ws, "error").await["message"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn watch_opus_and_browser_pcm_share_one_channel_and_reach_the_program_pipeline() {
        let station = station().await;
        station.hub.set_mix(true, false).await;
        let watch = watch_token(&station, "ada-phone", "ada-watch", true).await;
        assert!(watch.voice);
        let (mut bob, bob_id) = join(&station, "bob-laptop", "pcm16").await;
        let (mut wrist, wrist_id) = join(&station, &watch.token, "opus").await;
        loop {
            let roster = next_text(&mut bob, "roster").await;
            let peers = roster["peers"].as_array().unwrap();
            if let Some(p) = peers.iter().find(|p| p["id"] == wrist_id) {
                assert_eq!(p["device"], "watch");
                assert!(peers.iter().any(|p| p["id"] == bob_id && p["device"] == "web"));
                break;
            }
        }

        // Fake watch: Opus 16 kHz mono, one packet per 20 ms frame.
        let mut encoder = opus::Encoder::new(16_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        send_json(&mut wrist, serde_json::json!({"type":"transmitting","enabled":true})).await;
        next_text(&mut bob, "roster").await;
        for i in 0..20 {
            let mut packet = [0u8; MAX_OPUS_BYTES];
            let n = encoder.encode(&sine_frame(i, 440.0), &mut packet).unwrap();
            wrist.send(WsMessage::Binary(packet[..n].to_vec().into())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // The browser gets the decoded audio as PCM16 under the watch's id.
        let heard = frames_within(&mut bob, Duration::from_millis(600)).await;
        assert!(heard.len() >= 10, "{} frames", heard.len());
        assert!(heard.iter().all(|(id, pcm)| *id == wrist_id && pcm.len() == FRAME_BYTES));
        assert!(rms(&heard.last().unwrap().1) > 1_000.0, "decoded speech is not silence");
        // Watch audio enters the same capture pipeline as browser audio.
        assert!(station.hub.history.lock().await.frames.iter().any(|f| f.speaker == wrist_id));

        // The browser talks; the watch receives Opus it can decode.
        send_json(&mut bob, serde_json::json!({"type":"transmitting","enabled":true})).await;
        for i in 0..20 {
            bob.send(WsMessage::Binary(pcm_bytes(&sine_frame(i, 300.0)).into())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let heard = frames_within(&mut wrist, Duration::from_millis(600)).await;
        assert!(heard.len() >= 10, "{} frames", heard.len());
        let mut decoder = opus::Decoder::new(16_000, opus::Channels::Mono).unwrap();
        let mut last = Vec::new();
        for (id, packet) in &heard {
            assert_eq!(*id, bob_id);
            assert!(packet.len() < 200, "Opus, not PCM ({} B)", packet.len());
            let mut out = [0i16; FRAME_SAMPLES];
            assert_eq!(decoder.decode(packet, &mut out, false).unwrap(), FRAME_SAMPLES);
            last = pcm_bytes(&out);
        }
        assert!(rms(&last) > 1_000.0);
    }

    #[tokio::test]
    async fn a_phone_and_its_watch_never_hear_each_other_and_the_phone_pauses_while_the_watch_has_voice() {
        let station = station().await;
        let watch = watch_token(&station, "ada-phone", "ada-watch", true).await;
        let (mut phone, phone_id) = join(&station, "ada-phone", "pcm16").await;
        let (mut bob, bob_id) = join(&station, "bob-laptop", "pcm16").await;
        let (mut wrist, wrist_id) = join(&station, &watch.token, "opus").await;
        // The watch joining holds the phone.
        let handoff = next_text(&mut phone, "handoff").await;
        assert_eq!(handoff["held"], true);
        assert_eq!(handoff["by"], "watch");

        // Bob talks: the watch hears him, the held phone plays nothing.
        send_json(&mut bob, serde_json::json!({"type":"transmitting","enabled":true})).await;
        for i in 0..8 {
            bob.send(WsMessage::Binary(pcm_bytes(&sine_frame(i, 300.0)).into())).await.unwrap();
        }
        assert!(frames_within(&mut wrist, Duration::from_millis(400)).await.iter().any(|(id, _)| *id == bob_id));
        assert!(frames_within(&mut phone, Duration::from_millis(200)).await.is_empty());
        send_json(&mut bob, serde_json::json!({"type":"transmitting","enabled":false})).await;

        // The held phone cannot transmit; it is reminded why.
        send_json(&mut phone, serde_json::json!({"type":"transmitting","enabled":true})).await;
        assert_eq!(next_text(&mut phone, "handoff").await["held"], true);
        for i in 0..8 {
            phone.send(WsMessage::Binary(pcm_bytes(&sine_frame(i, 500.0)).into())).await.unwrap();
        }
        assert!(!frames_within(&mut bob, Duration::from_millis(300)).await.iter().any(|(id, _)| *id == phone_id));

        // The watch yields: the phone resumes listening, but the pair still
        // never hear each other.
        send_json(&mut wrist, serde_json::json!({"type":"active","enabled":false})).await;
        assert_eq!(next_text(&mut phone, "handoff").await["held"], false);
        let mut encoder = opus::Encoder::new(16_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        send_json(&mut wrist, serde_json::json!({"type":"transmitting","enabled":true})).await;
        for i in 0..8 {
            let mut packet = [0u8; MAX_OPUS_BYTES];
            let n = encoder.encode(&sine_frame(i, 440.0), &mut packet).unwrap();
            wrist.send(WsMessage::Binary(packet[..n].to_vec().into())).await.unwrap();
        }
        assert!(frames_within(&mut bob, Duration::from_millis(400)).await.iter().any(|(id, _)| *id == wrist_id));
        assert!(!frames_within(&mut phone, Duration::from_millis(200)).await.iter().any(|(id, _)| *id == wrist_id));

        // Claiming again holds the phone again; leaving releases it.
        send_json(&mut wrist, serde_json::json!({"type":"active","enabled":true})).await;
        assert_eq!(next_text(&mut phone, "handoff").await["held"], true);
        wrist.close(None).await.unwrap();
        assert_eq!(next_text(&mut phone, "handoff").await["held"], false);
    }

    #[tokio::test]
    async fn watch_voice_needs_the_grant_and_the_account_permission() {
        let station = station().await;
        // A watch token issued without voice cannot join.
        let quiet = watch_token(&station, "ada-phone", "ada-watch", false).await;
        assert!(!quiet.voice);
        assert!(rejection(&station, "/api/voice/ws", &quiet.token).await.contains("voice_transmit"));
        // An account without voice permission never gets the grant.
        let carl = watch_token(&station, "carl-phone", "carl-watch", true).await;
        assert!(!carl.voice);
        assert!(rejection(&station, "/api/voice/ws", &carl.token).await.contains("voice_transmit"));
        // A voice watch token never reaches the audience commentary socket.
        let voice = watch_token(&station, "ada-phone", "ada-watch-2", true).await;
        assert!(rejection(&station, "/api/program/commentary/ws", &voice.token).await.contains("Sign in"));
        // Malformed Opus disconnects the sender.
        let (mut wrist, _) = join(&station, &voice.token, "opus").await;
        send_json(&mut wrist, serde_json::json!({"type":"transmitting","enabled":true})).await;
        wrist.send(WsMessage::Binary(vec![0u8; MAX_OPUS_BYTES + 1].into())).await.unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match wrist.next().await {
                    None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => return true,
                    _ => {}
                }
            }
        })
        .await;
        assert_eq!(closed, Ok(true));
        // Revoking the account's voice permission ends the watch's voice.
        let (mut wrist, _) = join(&station, &voice.token, "opus").await;
        std::fs::write(
            station._root.0.join("auth.json"),
            serde_json::json!({"users":[{"username":"ada","roles":[],"password":{"salt_b64":"","hash_b64":""},
                "permissions":{"view_data":true,"voice_transmit":false}}]})
            .to_string(),
        )
        .unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match wrist.next().await {
                    None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => return true,
                    _ => {}
                }
            }
        })
        .await;
        assert_eq!(ended, Ok(true));
        assert!(
            station
                .app
                .auth
                .authorize_token(&station.app.auth_db, Some(&voice.token), Permission::VoiceTransmit)
                .await
                .is_err()
        );
    }

    #[test]
    fn opus_framing_round_trips_and_refuses_bad_packets() {
        let mut encoder = None;
        let pcm = pcm_bytes(&sine_frame(0, 440.0));
        let packet = opus_encode(&mut encoder, &pcm).unwrap();
        assert!(!packet.is_empty() && packet.len() <= MAX_OPUS_BYTES);
        let mut decoder = opus::Decoder::new(16_000, opus::Channels::Mono).unwrap();
        assert_eq!(opus_decode(&mut decoder, &packet).unwrap().len(), FRAME_BYTES);
        assert!(opus_decode(&mut decoder, &[]).is_none());
        assert!(opus_decode(&mut decoder, &[0u8; MAX_OPUS_BYTES + 1]).is_none());
        // A 40 ms packet is not one 20 ms frame.
        let mut long = opus::Encoder::new(16_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let samples: Vec<i16> = [sine_frame(0, 440.0), sine_frame(1, 440.0)].concat();
        let mut out = [0u8; MAX_OPUS_BYTES];
        let n = long.encode(&samples, &mut out).unwrap();
        assert!(opus_decode(&mut decoder, &out[..n]).is_none());
        // Server frames: four-byte little-endian sender id, then the payload.
        let Message::Binary(bytes) = framed(0x0102_0304, &[9, 8]) else { panic!() };
        assert_eq!(&bytes[..], &[4, 3, 2, 1, 9, 8]);
        // `codec` is optional in `auth`; browsers keep sending PCM16.
        assert!(matches!(
            serde_json::from_str::<Control>(r#"{"type":"auth","token":"t"}"#),
            Ok(Control::Auth { codec: Codec::Pcm16, .. })
        ));
        assert!(matches!(
            serde_json::from_str::<Control>(r#"{"type":"auth","token":"t","codec":"opus"}"#),
            Ok(Control::Auth { codec: Codec::Opus, .. })
        ));
        assert!(serde_json::from_str::<Control>(r#"{"type":"auth","token":"t","codec":"mp3"}"#).is_err());
        assert!(matches!(
            serde_json::from_str::<Control>(r#"{"type":"active","enabled":false}"#),
            Ok(Control::Active { enabled: false })
        ));
    }

    #[test]
    fn slow_listener_queue_is_bounded() {
        let (tx, _rx) = mpsc::channel(16);
        for _ in 0..16 {
            assert!(
                tx.try_send(Message::Binary(vec![0; FRAME_BYTES].into()))
                    .is_ok()
            );
        }
        assert!(
            tx.try_send(Message::Binary(vec![0; FRAME_BYTES].into()))
                .is_err()
        );
    }
}
