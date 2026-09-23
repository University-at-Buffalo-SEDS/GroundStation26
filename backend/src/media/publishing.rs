//! Browser camera ingest. Relay credentials never leave the backend.
use super::*;
use std::time::Instant;

pub(super) struct Publisher {
    owner: String,
    id: String,
    session: String,
    expires: Instant,
}

pub(super) fn routes() -> Router<Arc<MediaState>> {
    Router::new()
        .route(
            "/api/video/publish",
            post(offer).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/video/publish/{id}/{session}",
            post(renew).delete(stop),
        )
}

async fn owner(state: &MediaState, headers: &HeaderMap) -> ApiResult<String> {
    let p = crate::web::authorize_headers(&state.app, headers, Permission::ViewData).await?;
    if !p.can_manage_stream() {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Stream-management permission required to publish a camera",
        ));
    }
    p.username
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Sign in to publish a camera"))
}

async fn close(state: &MediaState, publisher: &Publisher) {
    let _ = state
        .client
        .delete(format!(
            "{}/{}/whip/{}",
            state.webrtc_url.trim_end_matches('/'),
            publisher.id,
            publisher.session
        ))
        .basic_auth("camera", Some(&state.relay_password))
        .send()
        .await;
}

pub(super) async fn reap(state: Arc<MediaState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let expired = {
            let mut publishers = state.publishers.lock().await;
            let mut expired = Vec::new();
            let mut i = 0;
            while i < publishers.len() {
                if publishers[i].expires <= Instant::now() {
                    expired.push(publishers.remove(i));
                } else {
                    i += 1;
                }
            }
            expired
        };
        for publisher in expired {
            close(&state, &publisher).await;
        }
    }
}

async fn offer(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> ApiResult<Response> {
    let username = owner(&state, &headers).await?;
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        != Some("application/sdp")
    {
        return Err(error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected application/sdp",
        ));
    }
    // A server-generated name prevents a browser from replacing a physical camera.
    use ring::rand::SecureRandom;
    let mut random = [0_u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Cannot allocate camera ID",
            )
        })?;
    let id = format!(
        "browser-{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let mut publishers = state.publishers.lock().await;
    if publishers.len() >= 64 || publishers.iter().filter(|p| p.owner == username).count() >= 2 {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Camera publishing limit reached",
        ));
    }
    let upstream = state
        .client
        .post(format!(
            "{}/{id}/whip",
            state.webrtc_url.trim_end_matches('/')
        ))
        .basic_auth("camera", Some(&state.relay_password))
        .header(header::CONTENT_TYPE, "application/sdp")
        .body(bytes)
        .send()
        .await
        .map_err(upstream_error)?;
    let status = upstream.status();
    if !status.is_success() {
        return Err(error(status, "Camera publishing unavailable"));
    }
    let session = upstream
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .and_then(session_id)
        .ok_or_else(|| error(StatusCode::BAD_GATEWAY, "Invalid publishing session"))?
        .to_string();
    let publisher = Publisher {
        owner: username,
        id: id.clone(),
        session: session.clone(),
        expires: Instant::now() + Duration::from_secs(45),
    };
    let body = match upstream.bytes().await {
        Ok(body) => body,
        Err(err) => {
            close(&state, &publisher).await;
            return Err(upstream_error(err));
        }
    };
    if let Err(err) = owner(&state, &headers).await {
        close(&state, &publisher).await;
        return Err(err);
    }
    publishers.push(publisher);
    Ok((
        status,
        [
            (header::CONTENT_TYPE, "application/sdp".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (
                header::LOCATION,
                format!("/api/video/publish/{id}/{session}"),
            ),
        ],
        body,
    )
        .into_response())
}

async fn renew(
    State(state): State<Arc<MediaState>>,
    Path((id, session)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    let username = owner(&state, &headers).await?;
    let mut publishers = state.publishers.lock().await;
    let publisher = publishers
        .iter_mut()
        .find(|p| {
            p.owner == username && p.id == id && p.session == session && p.expires > Instant::now()
        })
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Camera session expired"))?;
    publisher.expires = Instant::now() + Duration::from_secs(45);
    Ok(StatusCode::NO_CONTENT)
}

async fn stop(
    State(state): State<Arc<MediaState>>,
    Path((id, session)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    // Allow an authenticated owner to stop even after publishing permission is revoked.
    let p = crate::web::authorize_headers(&state.app, &headers, Permission::ViewData).await?;
    let publisher = {
        let mut publishers = state.publishers.lock().await;
        let index = publishers
            .iter()
            .position(|item| {
                Some(&item.owner) == p.username.as_ref() && item.id == id && item.session == session
            })
            .ok_or_else(|| error(StatusCode::NOT_FOUND, "Camera session not found"))?;
        publishers.remove(index)
    };
    close(&state, &publisher).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishing_requires_permission_and_session_ownership() {
        let dir = std::env::temp_dir().join(format!("gs-camera-test-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let users_path = dir.join("users.json");
        std::fs::write(&users_path, serde_json::to_vec(&serde_json::json!({"anonymous":{"view_data":true},"users":[
            {"username":"producer","roles":["stream_master"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"other","roles":["stream_admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"viewer","roles":["stream_viewer"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}}
        ]})).unwrap()).unwrap();
        let mut app = crate::state::tests::test_app_state().await;
        Arc::get_mut(&mut app).unwrap().auth = Arc::new(crate::auth::AuthManager::new(users_path));
        crate::ensure_auth_sessions_table(&app.auth_db)
            .await
            .unwrap();
        for name in ["producer", "other", "viewer"] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,allowed_commands_json,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,0,'[]',0,9999999999999)")
                .bind(format!("test-{name}")).bind(name).execute(&app.auth_db).await.unwrap();
        }
        let relay = Router::new()
            .route(
                "/{id}/whip",
                post(|Path(id): Path<String>, headers: HeaderMap| async move {
                    assert_eq!(headers[header::AUTHORIZATION], "Basic Y2FtZXJhOnNlY3JldA==");
                    assert!(id.starts_with("browser-"));
                    (
                        StatusCode::CREATED,
                        [(header::LOCATION, format!("/{id}/whip/session-1"))],
                        "answer",
                    )
                }),
            )
            .route(
                "/{id}/whip/{session}",
                delete(|| async { StatusCode::NO_CONTENT }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay_task = tokio::spawn(async move {
            axum::serve(listener, relay).await.unwrap();
        });
        let state = Arc::new(MediaState {
            app,
            client: reqwest::Client::new(),
            api_url: String::new(),
            webrtc_url: format!("http://{address}"),
            hls_url: String::new(),
            relay_password: "secret".into(),
            model_write: Mutex::new(()),
            models: dir.clone(),
            recordings: dir.clone(),
            voice: Arc::new(crate::voice::VoiceHub::default()),
            presentation_write: Mutex::new(()),
            tickets: Mutex::new(Vec::new()),
            program_history: Mutex::new(std::collections::VecDeque::new()),
            hls_segments: Mutex::new(std::collections::HashMap::new()),
            preview_sessions: Mutex::new(Vec::new()),
            publishers: Mutex::new(Vec::new()),
        });
        let headers = |name: &str| {
            let mut h = HeaderMap::new();
            h.insert(
                header::AUTHORIZATION,
                format!("Bearer test-{name}").parse().unwrap(),
            );
            h.insert(header::CONTENT_TYPE, "application/sdp".parse().unwrap());
            h
        };
        assert_eq!(
            offer(
                State(state.clone()),
                headers("viewer"),
                Bytes::from("offer")
            )
            .await
            .unwrap_err()
            .status(),
            StatusCode::FORBIDDEN
        );
        assert!(
            offer(State(state.clone()), HeaderMap::new(), Bytes::from("offer"))
                .await
                .is_err()
        );
        let response = offer(
            State(state.clone()),
            headers("producer"),
            Bytes::from("offer"),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let location = response.headers()[header::LOCATION].to_str().unwrap();
        assert!(location.starts_with("/api/video/publish/browser-"));
        let (id, session) = {
            let all = state.publishers.lock().await;
            (all[0].id.clone(), all[0].session.clone())
        };
        let path = || Path((id.clone(), session.clone()));
        assert_eq!(
            renew(State(state.clone()), path(), headers("other"))
                .await
                .unwrap_err()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            stop(State(state.clone()), path(), headers("other"))
                .await
                .unwrap_err()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            renew(State(state.clone()), path(), headers("producer"))
                .await
                .unwrap(),
            StatusCode::NO_CONTENT
        );
        // Revoking the user's role immediately rejects the next lease renewal.
        let mut users: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("users.json")).unwrap()).unwrap();
        users["users"][0]["roles"] = serde_json::json!(["stream_viewer"]);
        std::fs::write(dir.join("users.json"), serde_json::to_vec(&users).unwrap()).unwrap();
        assert!(
            renew(State(state.clone()), path(), headers("producer"))
                .await
                .is_err()
        );
        // The original owner can always stop their own active session.
        assert_eq!(
            stop(State(state.clone()), path(), headers("producer"))
                .await
                .unwrap(),
            StatusCode::NO_CONTENT
        );
        assert!(state.publishers.lock().await.is_empty());
        relay_task.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
