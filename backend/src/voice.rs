//! Browser voice only: bounded PCM relay over the existing authenticated WebSocket link.
//! No radio hardware, external signaling service, or voice recording.
use crate::{auth::Permission, state::AppState};
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
use serde::Deserialize;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};

const FRAME_BYTES: usize = 640; // mono signed PCM16 little endian, 16 kHz, 20 ms
const MAX_PEERS: usize = 12;

struct AudioFrame {
    sequence: u64,
    speaker: u32,
    created: std::time::Instant,
    pcm: Vec<u8>,
}
#[derive(Default)]
struct BroadcastHistory {
    enabled: bool,
    generation: u64,
    next: u64,
    frames: VecDeque<AudioFrame>,
}
#[derive(Default)]
pub(crate) struct VoiceHub {
    history: Mutex<BroadcastHistory>,
}

impl VoiceHub {
    pub(crate) async fn set_broadcast(&self, enabled: bool) {
        let mut history = self.history.lock().await;
        if history.enabled != enabled {
            history.enabled = enabled;
            history.generation += 1;
            history.frames.clear();
        }
    }
    async fn enabled(&self) -> bool {
        self.history.lock().await.enabled
    }
    async fn capture(&self, speaker: u32, pcm: &[u8]) {
        let mut history = self.history.lock().await;
        if !history.enabled {
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
        if history.enabled {
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
        serde_json::json!({"enabled":history.enabled,"generation":history.generation,"cursor":cursor,"frames":frames})
    }
}

struct Peer {
    name: String,
    transmitting: bool,
    tx: mpsc::Sender<Message>,
}
struct VoiceState {
    app: Arc<AppState>,
    hub: Arc<VoiceHub>,
    peers: Mutex<BTreeMap<u32, Peer>>,
    next: AtomicU32,
}

pub fn routes(app: Arc<AppState>, hub: Arc<VoiceHub>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/voice/ws", get(upgrade))
        .route("/api/voice/status", get(capabilities))
        .with_state(Arc::new(VoiceState {
            app,
            hub,
            peers: Mutex::new(BTreeMap::new()),
            next: AtomicU32::new(1),
        }))
        .route("/radio", get(|| async { Html(include_str!("voice.html")) }))
        .route(
            "/assets/voice.js",
            get(|| async {
                (
                    [("Content-Type", "text/javascript")],
                    include_str!("voice.js"),
                )
            }),
        )
        .route(
            "/assets/voice-worklet.js",
            get(|| async {
                (
                    [("Content-Type", "text/javascript")],
                    include_str!("voice-worklet.js"),
                )
            }),
        )
        .route(
            "/assets/media-login.js",
            get(|| async {
                (
                    [("Content-Type", "text/javascript")],
                    include_str!("media-login.js"),
                )
            }),
        )
}

async fn capabilities(
    State(state): State<Arc<VoiceState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    match crate::web::authorize_headers(&state.app, &headers, Permission::ViewData).await {
        Ok(p) => axum::Json(serde_json::json!({"can_transmit":!p.anonymous && p.permissions.allows(Permission::VoiceTransmit),"broadcast_enabled":state.hub.enabled().await})).into_response(),
        Err(response) => response,
    }
}

async fn upgrade(State(state): State<Arc<VoiceState>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.max_message_size(2048)
        .max_frame_size(2048)
        .on_upgrade(move |socket| connection(state, socket))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Control {
    Auth { token: String },
    Transmitting { enabled: bool },
    Ping,
}

fn text(value: serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}
fn roster(peers: &BTreeMap<u32, Peer>) {
    let message = text(
        serde_json::json!({"type":"roster", "peers":peers.iter().map(|(id,p)|
        serde_json::json!({"id":id,"name":p.name,"transmitting":p.transmitting})).collect::<Vec<_>>() }),
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
    let token = match tokio::time::timeout(Duration::from_secs(5), socket.recv()).await {
        Ok(Some(Ok(Message::Text(raw)))) => match serde_json::from_str::<Control>(&raw) {
            Ok(Control::Auth { token }) if !token.is_empty() => token,
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
        .authorize_token(&state.app.auth_db, Some(&token), Permission::VoiceTransmit)
        .await
    {
        Ok(p) if !p.anonymous => p,
        _ => {
            reject(
                &mut socket,
                "A signed-in account with the voice_transmit permission is required",
            )
            .await;
            return;
        }
    };
    let id = state.next.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::channel(16); // bounded to about 320 ms of audio per listener
    {
        let mut peers = state.peers.lock().await;
        if peers.len() >= MAX_PEERS {
            drop(peers);
            reject(&mut socket, "Voice channel is full (12 participants)").await;
            return;
        }
        let _ = tx.try_send(text(
            serde_json::json!({"type":"joined","id":id,"sample_rate":16000}),
        ));
        peers.insert(
            id,
            Peer {
                name: principal.username.unwrap_or_else(|| "Crew".into()),
                transmitting: false,
                tx,
            },
        );
        roster(&peers);
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
                        if !budget.accept() || bytes.len() != FRAME_BYTES { break; }
                        let peers = state.peers.lock().await;
                        if !peers.get(&id).is_some_and(|p| p.transmitting) { continue; }
                        let mut packet = Vec::with_capacity(4 + FRAME_BYTES);
                        packet.extend_from_slice(&id.to_le_bytes()); packet.extend_from_slice(&bytes);
                        let packet = Message::Binary(packet.into());
                        for (other, peer) in peers.iter() {
                            if *other != id { let _ = peer.tx.try_send(packet.clone()); }
                        }
                        drop(peers);
                        state.hub.capture(id, &bytes).await;
                    }
                    Message::Text(raw) => {
                        if !budget.accept() { break; }
                        match serde_json::from_str::<Control>(&raw) {
                            Ok(Control::Transmitting { enabled }) => {
                                let mut peers = state.peers.lock().await;
                                if let Some(peer) = peers.get_mut(&id) { peer.transmitting = enabled; }
                                roster(&peers);
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
                if !matches!(state.app.auth.authorize_token(&state.app.auth_db, Some(&token), Permission::VoiceTransmit).await, Ok(p) if !p.anonymous) { break; }
                roster(&*state.peers.lock().await);
                let enabled = state.hub.enabled().await;
                if !matches!(tokio::time::timeout(Duration::from_secs(2), socket.send(text(serde_json::json!({"type":"broadcast_state","enabled":enabled})))).await, Ok(Ok(()))) { break; }
            }
        }
    }
    let mut peers = state.peers.lock().await;
    peers.remove(&id);
    roster(&peers);
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
        hub.set_broadcast(true).await;
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
        hub.set_broadcast(false).await;
        assert!(hub.history.lock().await.frames.is_empty());
        hub.capture(1, &[0; FRAME_BYTES]).await;
        hub.set_broadcast(true).await;
        assert_eq!(
            hub.audience_batch(0, 3).await["frames"]
                .as_array()
                .unwrap()
                .len(),
            0
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
