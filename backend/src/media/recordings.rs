use super::*;
use axum::extract::Request;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn routes() -> Router<Arc<MediaState>> {
    Router::new()
        .route("/api/video/recordings", get(list))
        .route("/api/media-assets/recordings/{stream}/{file}", get(asset))
}

#[derive(Default, Deserialize)]
struct Page {
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct Recording {
    stream: String,
    file: String,
    size_bytes: u64,
    modified_ms: u64,
    recently_updated: bool,
    url: String,
}

fn valid_file(file: &str) -> bool {
    file.strip_suffix(".mp4").is_some_and(|stem| {
        !stem.is_empty()
            && stem.len() <= 80
            && stem
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'_')
    })
}

fn recent(metadata: &std::fs::Metadata) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_none_or(|age| age < Duration::from_secs(5))
}

// Walk just the stream directory and never follow camera/file symlinks.
async fn scan(root: &std::path::Path) -> std::io::Result<Vec<Recording>> {
    let mut result = Vec::new();
    let mut streams = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(e),
    };
    while let Some(stream) = streams.next_entry().await? {
        let id = stream.file_name().to_string_lossy().into_owned();
        if !valid_id(&id) || !stream.file_type().await?.is_dir() {
            continue;
        }
        let mut files = tokio::fs::read_dir(stream.path()).await?;
        while let Some(file) = files.next_entry().await? {
            let name = file.file_name().to_string_lossy().into_owned();
            if !valid_file(&name) || !file.file_type().await?.is_file() {
                continue;
            }
            let meta = file.metadata().await?;
            result.push(Recording {
                stream: id.clone(),
                file: name,
                size_bytes: meta.len(),
                modified_ms: meta
                    .modified()
                    .unwrap_or(SystemTime::UNIX_EPOCH)
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                recently_updated: recent(&meta),
                url: String::new(),
            });
        }
    }
    result.sort_by(|a, b| b.file.cmp(&a.file).then(a.stream.cmp(&b.stream)));
    Ok(result)
}

async fn list(
    State(state): State<Arc<MediaState>>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> ApiResult<Response> {
    // Archives contain original live footage, so don't bypass audience delay permissions.
    contract::authorize_preview(&state, &headers).await?;
    let entries = scan(&state.recordings).await.map_err(io_error)?;
    let total = entries.len();
    let offset = page.offset.unwrap_or(0);
    let limit = page.limit.unwrap_or(50).clamp(1, 100);
    let mut entries: Vec<_> = entries.into_iter().skip(offset).take(limit).collect();
    for item in &mut entries {
        let resource = format!("recording:{}/{}", item.stream, item.file);
        let ticket = contract::ticket(&state, &headers, &resource).await?;
        item.url = format!(
            "/api/media-assets/recordings/{}/{}?ticket={ticket}",
            item.stream, item.file
        );
    }
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(serde_json::json!({
            "recordings": entries, "total": total, "offset": offset, "limit": limit
        })),
    )
        .into_response())
}

async fn safe_path(root: &std::path::Path, stream: &str, file: &str) -> ApiResult<PathBuf> {
    check_id(stream)?;
    if !valid_file(file) {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid recording filename"));
    }
    let root = tokio::fs::canonicalize(root).await.map_err(io_error)?;
    let path = root.join(stream).join(file);
    for component in [root.join(stream), path.clone()] {
        if tokio::fs::symlink_metadata(component)
            .await
            .map_err(io_error)?
            .file_type()
            .is_symlink()
        {
            return Err(error(
                StatusCode::FORBIDDEN,
                "Recording symlinks are not allowed",
            ));
        }
    }
    let resolved = tokio::fs::canonicalize(&path).await.map_err(io_error)?;
    if !resolved.starts_with(&root) {
        return Err(error(StatusCode::FORBIDDEN, "Invalid recording path"));
    }
    Ok(resolved)
}

async fn asset(
    State(state): State<Arc<MediaState>>,
    Path((stream, file)): Path<(String, String)>,
    Query(query): Query<contract::MediaQuery>,
    request: Request,
) -> ApiResult<Response> {
    contract::authorize_preview_media(
        &state,
        request.headers(),
        &query,
        &format!("recording:{stream}/{file}"),
    )
    .await?;
    let path = safe_path(&state.recordings, &stream, &file).await?;
    let metadata = tokio::fs::metadata(&path).await.map_err(io_error)?;
    if recent(&metadata) {
        return Err(error(
            StatusCode::CONFLICT,
            "Segment is still being written; retry shortly",
        ));
    }
    let mut response = tower_http::services::ServeFile::new(path)
        .try_call(request)
        .await
        .map_err(io_error)?
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "video/mp4".parse().unwrap());
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn archive_scan_and_download_reject_traversal_and_symlinks() {
        let root =
            std::env::temp_dir().join(format!("gs-recording-test-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(root.join("camera"))
            .await
            .unwrap();
        tokio::fs::write(
            root.join("camera/2026-09-17_12-00-00-000001.mp4"),
            b"fixture",
        )
        .await
        .unwrap();
        tokio::fs::write(root.join("camera/private.txt"), b"hidden")
            .await
            .unwrap();
        assert_eq!(scan(&root).await.unwrap().len(), 1);
        assert!(safe_path(&root, "..", "2026.mp4").await.is_err());
        assert!(safe_path(&root, "camera", "../private.mp4").await.is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("camera"), root.join("escape")).unwrap();
            assert!(
                safe_path(&root, "escape", "2026-09-17_12-00-00-000001.mp4")
                    .await
                    .is_err()
            );
            assert_eq!(scan(&root).await.unwrap().len(), 1);
        }
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
