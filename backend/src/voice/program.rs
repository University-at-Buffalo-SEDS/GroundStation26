//! Audience-only microphone ingest. It never registers a crew peer, sends a
//! crew/radio frame, or acquires a hardware device on behalf of an operator.
use super::*;

pub(super) async fn upgrade(
    State(state): State<Arc<VoiceState>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.max_message_size(2048)
        .max_frame_size(2048)
        .on_upgrade(move |socket| connection(state, socket))
}
async fn connection(state: Arc<VoiceState>, mut socket: WebSocket) {
    let token = match tokio::time::timeout(Duration::from_secs(5), socket.recv()).await {
        Ok(Some(Ok(Message::Text(raw)))) => match serde_json::from_str::<Control>(&raw) {
            Ok(Control::Auth { token }) if !token.is_empty() => token,
            _ => {
                reject(&mut socket, "Sign in before enabling commentary").await;
                return;
            }
        },
        _ => return,
    };
    let allowed = |p: &crate::auth::AuthPrincipal| !p.anonymous && p.can_manage_stream();
    if !matches!(state.app.auth.authorize_token(&state.app.auth_db,Some(&token),Permission::ViewData).await,Ok(p) if allowed(&p))
    {
        reject(
            &mut socket,
            "Stream-management permission required for commentary",
        )
        .await;
        return;
    }
    let Ok(_slot) = state.program_slots.clone().try_acquire_owned() else {
        reject(&mut socket, "Programme commentary is full (4 microphones)").await;
        return;
    };
    let id = state.next.fetch_add(1, Ordering::Relaxed);
    if socket.send(text(serde_json::json!({"type":"joined","id":id,"sample_rate":16000,"destination":"audience_program"}))).await.is_err(){return;}
    let mut transmitting = false;
    let mut budget = RateLimit::new();
    let mut last_message = tokio::time::Instant::now();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            incoming=socket.recv()=>{
                let Some(Ok(message))=incoming else{break;};last_message=tokio::time::Instant::now();
                match message {
                    Message::Binary(bytes)=>{
                        if !budget.accept()||bytes.len()!=FRAME_BYTES {break;}
                        if transmitting {state.hub.capture_program(id,&bytes).await;}
                    },
                    Message::Text(raw)=>{
                        if !budget.accept(){break;}
                        match serde_json::from_str::<Control>(&raw) {Ok(Control::Transmitting{enabled})=>transmitting=enabled,Ok(Control::Ping)=>{},_=>break}
                    },
                    Message::Close(_)=>break,Message::Ping(_)|Message::Pong(_)=>{},
                }
            },
            _=interval.tick()=>{
                if last_message.elapsed()>Duration::from_secs(40)||!matches!(state.app.auth.authorize_token(&state.app.auth_db,Some(&token),Permission::ViewData).await,Ok(p) if allowed(&p)){break;}
                let enabled=state.hub.commentary_enabled().await;
                if !enabled {transmitting=false;}
                if !matches!(tokio::time::timeout(Duration::from_secs(2),socket.send(text(serde_json::json!({"type":"program_state","enabled":enabled})))).await,Ok(Ok(()))){break;}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message as Wire};
    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;
    async fn sign_in(url: &str, token: &str) -> (Client, serde_json::Value) {
        let (mut socket, _) = connect_async(url).await.unwrap();
        socket
            .send(Wire::Text(
                serde_json::json!({"type":"auth","token":token})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        (
            socket,
            serde_json::from_str(message.to_text().unwrap()).unwrap(),
        )
    }
    async fn no_crew_audio(socket: &mut Client) {
        let until = tokio::time::Instant::now() + Duration::from_millis(150);
        loop {
            match tokio::time::timeout_at(until, socket.next()).await {
                Ok(Some(Ok(Wire::Binary(_)))) => {
                    panic!("programme commentary leaked into crew channel")
                }
                Ok(Some(Ok(_))) => {}
                Err(_) => break,
                _ => break,
            }
        }
    }
    #[tokio::test]
    async fn actual_program_socket_requires_stream_role_and_never_becomes_a_crew_peer() {
        use crate::recording_export::tests::{TestDirectory, export_state};
        let root = TestDirectory::new();
        let app = export_state(&root.0, true).await;
        let account = |name: &str, roles: Vec<&str>, voice: bool| serde_json::json!({"username":name,"roles":roles,"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"voice_transmit":voice}});
        std::fs::write(root.0.join("auth.json"),serde_json::json!({"anonymous":{"view_data":true},"users":[account("producer",vec!["stream_master"],false),account("crew",vec![],true),account("viewer",vec![],false)]}).to_string()).unwrap();
        for name in ["producer", "crew", "viewer"] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,0,0,9999999999999)").bind(format!("audio-{name}" )).bind(name).execute(&app.auth_db).await.unwrap();
        }
        let hub = Arc::new(VoiceHub::default());
        hub.set_mix(false, true).await;
        let state = Arc::new(VoiceState {
            app,
            hub: hub.clone(),
            peers: Mutex::new(BTreeMap::new()),
            next: AtomicU32::new(1),
            program_slots: Arc::new(tokio::sync::Semaphore::new(4)),
        });
        let routes = axum::Router::new()
            .route("/commentary", get(upgrade))
            .route("/crew", get(super::super::upgrade))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        let (mut crew, joined) = sign_in(&format!("ws://{addr}/crew"), "audio-crew").await;
        assert_eq!(joined["type"], "joined");
        let (mut program, joined) =
            sign_in(&format!("ws://{addr}/commentary"), "audio-producer").await;
        assert_eq!(joined["destination"], "audience_program");
        program
            .send(Wire::Text(
                "{\"type\":\"transmitting\",\"enabled\":true}".into(),
            ))
            .await
            .unwrap();
        program
            .send(Wire::Binary(vec![7; FRAME_BYTES].into()))
            .await
            .unwrap();
        for _ in 0..100 {
            if hub.history.lock().await.frames.len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(hub.history.lock().await.frames.len(), 1);
        assert_eq!(
            state.peers.lock().await.len(),
            1,
            "commentator is not in crew roster"
        );
        no_crew_audio(&mut crew).await;
        let (_denied, error) = sign_in(&format!("ws://{addr}/commentary"), "audio-viewer").await;
        assert_eq!(error["type"], "error");
        let (mut monitor, joined) = sign_in(&format!("ws://{addr}/crew"), "audio-producer").await;
        assert_eq!(joined["type"], "joined");
        monitor
            .send(Wire::Text(
                "{\"type\":\"transmitting\",\"enabled\":true}".into(),
            ))
            .await
            .unwrap();
        monitor
            .send(Wire::Binary(vec![9; FRAME_BYTES].into()))
            .await
            .unwrap();
        no_crew_audio(&mut crew).await;
        assert!(
            !state
                .peers
                .lock()
                .await
                .values()
                .any(|p| p.name == "producer" && p.transmitting),
            "stream-only listener cannot acquire VC transmit"
        );
        assert_eq!(hub.history.lock().await.frames.len(), 1);
        hub.history.lock().await.frames[0].created =
            std::time::Instant::now() - Duration::from_millis(5600);
        let batch = hub.audience_batch(0, 3).await;
        assert_eq!(batch["enabled"], true);
        assert_eq!(
            batch["frames"].as_array().unwrap().len(),
            1,
            "programme-only mix reaches delayed audience"
        );
        hub.set_mix(false, false).await;
        program
            .send(Wire::Binary(vec![8; FRAME_BYTES].into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            hub.history.lock().await.frames.is_empty(),
            "disabled mix cannot retain private audio"
        );
        program.close(None).await.unwrap();
        monitor.close(None).await.unwrap();
        crew.close(None).await.unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn independent_mix_gates_keep_disabled_sources_out_of_history() {
        let hub = VoiceHub::default();
        hub.set_mix(false, true).await;
        hub.capture(1, &[1; FRAME_BYTES]).await;
        hub.capture_program(2, &[2; FRAME_BYTES]).await;
        assert_eq!(hub.history.lock().await.frames.len(), 1);
        assert_eq!(hub.history.lock().await.frames[0].speaker, 2);
        hub.set_mix(true, false).await;
        assert!(hub.history.lock().await.frames.is_empty());
        hub.capture_program(2, &[2; FRAME_BYTES]).await;
        hub.capture(1, &[1; FRAME_BYTES]).await;
        assert_eq!(hub.history.lock().await.frames.len(), 1);
        assert_eq!(hub.history.lock().await.frames[0].speaker, 1);
    }
}
