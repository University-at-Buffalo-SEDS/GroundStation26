//! Recorded camera video and crew voice in replay bundles (`?video=1`,
//! `?voice=1`; `docs/backend/replay-bundle.md`, "Recorded media").
//!
//! The station records cameras through MediaMTX as one-minute fragmented-MP4
//! segments, `<video dir>/<stream>/<YYYY-mm-dd_HH-MM-SS-ffffff>.mp4`, and,
//! when the deployment opts in, crew voice as one WAV per transmission
//! (`voice/recorder.rs`), `<voice dir>/<stream>/<start_ms>-<speaker>.wav`.
//! A bundle takes the whole files that overlap the recording's time range
//! (no trimming), smallest first within each kind's budget, as
//! `media/video/<stream>/<file>` and `media/voice/<stream>/<file>`, and lists
//! them in `media/index.json` with each clip's stream, start and end (Unix
//! ms), size and media type. The manifest's `media` says what was included
//! and what was skipped and why.
//!
//! MediaMTX names segments in the station's local time. The UTC start is the
//! one whole-quarter-hour offset that puts the name less than 15 minutes
//! before the file's last write (a segment is one minute long), so no time
//! zone setting is needed; a file whose write time does not fit is taken as
//! UTC and one minute long (`timing` says which).
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(crate) const DEFAULT_VIDEO_BUDGET_MB: u32 = 32;
pub(crate) const MAX_VIDEO_BUDGET_MB: u32 = 64;
pub(crate) const DEFAULT_VOICE_BUDGET_MB: u32 = 8;
pub(crate) const MAX_VOICE_BUDGET_MB: u32 = 16;
const QUARTER_HOUR_MS: i64 = 15 * 60_000;
/// MediaMTX `recordSegmentDuration`.
const SEGMENT_MS: i64 = 60_000;
/// A file written to this recently is still being recorded.
const SETTLE_MS: i64 = 5_000;

/// Where the station keeps recorded media.
#[derive(Clone, Debug, Default)]
pub(crate) struct MediaDirs {
    pub video: Option<PathBuf>,
    /// `None`: crew voice recording is off on this station.
    pub voice: Option<PathBuf>,
}

impl MediaDirs {
    pub(crate) fn station() -> Self {
        Self { video: Some(crate::media::recordings_directory()), voice: crate::voice::recorder::directory() }
    }
}

/// What a bundle request asked for.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Request {
    pub video: bool,
    pub video_max_mb: Option<u32>,
    pub voice: bool,
    pub voice_max_mb: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Clip {
    pub kind: &'static str,
    pub stream: String,
    pub file: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub bytes: u64,
    pub media_type: &'static str,
    pub timing: &'static str,
    source: PathBuf,
}

impl Clip {
    pub(crate) fn path(&self) -> String {
        format!("media/{}/{}/{}", self.kind, self.stream, self.file)
    }
    fn listed(&self) -> Value {
        json!({
            "kind": self.kind, "stream": self.stream, "file": self.file, "path": self.path(),
            "start_ms": self.start_ms, "end_ms": self.end_ms, "bytes": self.bytes,
            "media_type": self.media_type, "timing": self.timing,
        })
    }
}

fn safe_stream(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-mm-dd_HH-MM-SS-ffffff.mp4` as milliseconds, reading the fields as UTC.
pub(crate) fn segment_name_ms(file: &str) -> Option<i64> {
    let stem = file.strip_suffix(".mp4")?;
    let (date, time) = stem.split_once('_')?;
    let n = |s: &str| -> Option<i64> { (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten() };
    let d: Vec<&str> = date.split('-').collect();
    let t: Vec<&str> = time.split('-').collect();
    let ([y, mo, da], [h, mi, s, f]) = (d.as_slice(), t.as_slice()) else { return None };
    let (y, mo, da, h, mi, s) = (n(y)?, n(mo)?, n(da)?, n(h)?, n(mi)?, n(s)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || s > 60 || f.len() > 9 {
        return None;
    }
    let frac_ms = n(f)? / 10_i64.pow(f.len().saturating_sub(3) as u32) * 10_i64.pow(3u32.saturating_sub(f.len() as u32));
    Some(days_from_civil(y, mo, da) * 86_400_000 + ((h * 60 + mi) * 60 + s) * 1000 + frac_ms)
}

/// A segment's (start, end, timing) from its name (station local time, read
/// as UTC) and its last write.
pub(crate) fn segment_span(name_ms: i64, modified_ms: i64) -> (i64, i64, &'static str) {
    for quarter in -56..=56i64 {
        let start = name_ms - quarter * QUARTER_HOUR_MS;
        if (0..QUARTER_HOUR_MS).contains(&(modified_ms - start)) {
            return (start, modified_ms.max(start), "name_local_time_and_last_write");
        }
    }
    (name_ms, name_ms + SEGMENT_MS, "name_as_utc")
}

fn modified_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

/// Every settled clip of one kind under `root` (`<root>/<stream>/<file>`),
/// never following symbolic links.
pub(crate) async fn scan(root: &Path, kind: &'static str, now_ms: i64) -> Vec<Clip> {
    let mut out = Vec::new();
    let Ok(mut streams) = tokio::fs::read_dir(root).await else { return out };
    while let Ok(Some(stream)) = streams.next_entry().await {
        let id = stream.file_name().to_string_lossy().into_owned();
        if !safe_stream(&id) || !stream.file_type().await.is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(mut files) = tokio::fs::read_dir(stream.path()).await else { continue };
        while let Ok(Some(file)) = files.next_entry().await {
            let name = file.file_name().to_string_lossy().into_owned();
            if !file.file_type().await.is_ok_and(|t| t.is_file()) {
                continue;
            }
            let Ok(meta) = file.metadata().await else { continue };
            let modified = modified_ms(&meta);
            if now_ms - modified < SETTLE_MS {
                continue;
            }
            let (start_ms, end_ms, timing, media_type) = match kind {
                "video" => {
                    let Some(name_ms) = segment_name_ms(&name) else { continue };
                    let (start, end, timing) = segment_span(name_ms, modified);
                    (start, end, timing, "video/mp4")
                }
                _ => {
                    let Some(start) = crate::voice::recorder::start_of(&name) else { continue };
                    (start, start + crate::voice::recorder::duration_ms(meta.len()), "name_unix_ms_and_length", "audio/wav")
                }
            };
            out.push(Clip { kind, stream: id.clone(), file: name, start_ms, end_ms, bytes: meta.len(), media_type, timing, source: file.path() });
        }
    }
    out.sort_by(|a, b| (a.start_ms, &a.stream, &a.file).cmp(&(b.start_ms, &b.stream, &b.file)));
    out
}

/// The clips overlapping `[start_ms, end_ms]`, smallest first within
/// `budget` bytes, with a note for each one left out.
pub(crate) fn select(clips: Vec<Clip>, start_ms: i64, end_ms: i64, budget: u64) -> (Vec<Clip>, Vec<Value>) {
    let mut overlapping: Vec<Clip> = clips.into_iter().filter(|c| c.start_ms <= end_ms && c.end_ms >= start_ms).collect();
    overlapping.sort_by(|a, b| (a.bytes, a.start_ms, &a.stream, &a.file).cmp(&(b.bytes, b.start_ms, &b.stream, &b.file)));
    let (mut used, mut kept, mut skipped) = (0u64, Vec::new(), Vec::new());
    for clip in overlapping {
        if used + clip.bytes <= budget {
            used += clip.bytes;
            kept.push(clip);
        } else {
            let reason = if clip.bytes > budget { "larger than the budget" } else { "over the budget" };
            skipped.push(json!({"stream": clip.stream, "file": clip.file, "start_ms": clip.start_ms, "end_ms": clip.end_ms, "bytes": clip.bytes, "reason": reason}));
        }
    }
    kept.sort_by(|a, b| (a.start_ms, &a.stream, &a.file).cmp(&(b.start_ms, &b.stream, &b.file)));
    (kept, skipped)
}

/// One bundle entry: (path, bytes, media type).
pub(crate) type Entry = (String, Vec<u8>, &'static str);

/// The media entries (clips, then `media/index.json`) and the manifest's
/// `media` summary; nothing when neither kind was asked for.
pub(crate) async fn entries(request: &Request, dirs: &MediaDirs, start_ms: Option<i64>, end_ms: Option<i64>, now_ms: i64) -> Option<(Vec<Entry>, Value)> {
    if !request.video && !request.voice {
        return None;
    }
    let mut out = Vec::new();
    let mut index = Vec::new();
    let mut summary = serde_json::Map::new();
    let kinds = [
        ("video", request.video, request.video_max_mb, DEFAULT_VIDEO_BUDGET_MB, MAX_VIDEO_BUDGET_MB, &dirs.video),
        ("voice", request.voice, request.voice_max_mb, DEFAULT_VOICE_BUDGET_MB, MAX_VOICE_BUDGET_MB, &dirs.voice),
    ];
    for (kind, wanted, max_mb, default_mb, cap_mb, dir) in kinds {
        if !wanted {
            continue;
        }
        let budget = u64::from(max_mb.unwrap_or(default_mb).min(cap_mb)) << 20;
        let mut note = json!({"requested": true, "recorded": dir.is_some(), "candidates": 0, "included": 0, "bytes": 0, "budget_bytes": budget, "complete": true, "skipped": []});
        if kind == "voice" && dir.is_none() {
            note["note"] = "Crew voice recording is off on this station (GS_VOICE_RECORDINGS_DIR is not set).".into();
        }
        if let (Some(dir), Some(start), Some(end)) = (dir, start_ms, end_ms) {
            let found = scan(dir, kind, now_ms).await;
            let (kept, mut skipped) = select(found, start, end, budget);
            note["candidates"] = (kept.len() + skipped.len()).into();
            let mut used = 0u64;
            for clip in kept {
                // Read now; a file removed or grown since the scan is skipped.
                match tokio::fs::read(&clip.source).await {
                    Ok(bytes) if bytes.len() as u64 == clip.bytes => {
                        used += clip.bytes;
                        index.push(clip.listed());
                        out.push((clip.path(), bytes, clip.media_type));
                    }
                    _ => skipped.push(json!({"stream": clip.stream, "file": clip.file, "start_ms": clip.start_ms, "end_ms": clip.end_ms, "bytes": clip.bytes, "reason": "changed while the bundle was built"})),
                }
            }
            note["included"] = (out.iter().filter(|(p, ..)| p.starts_with(&format!("media/{kind}/"))).count()).into();
            note["bytes"] = used.into();
            note["complete"] = skipped.is_empty().into();
            note["skipped"] = skipped.into();
        }
        summary.insert(kind.into(), note);
    }
    index.sort_by(|a, b| (a["start_ms"].as_i64(), a["kind"].as_str(), a["path"].as_str()).cmp(&(b["start_ms"].as_i64(), b["kind"].as_str(), b["path"].as_str())));
    let summary = Value::Object(summary);
    let document = json!({"version": 1, "clips": index, "video": summary.get("video"), "voice": summary.get("voice")});
    out.push(("media/index.json".into(), serde_json::to_vec_pretty(&document).unwrap_or_default(), "application/json"));
    Some((out, summary))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A media directory with synthetic camera segments and voice bursts,
    /// their last-write times pinned so the timing is reproducible.
    pub(crate) struct Fixture {
        pub root: PathBuf,
        pub dirs: MediaDirs,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn put(path: &Path, bytes: &[u8], modified_ms: i64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(modified_ms as u64)).unwrap();
    }

    /// Recording range 2026-01-01T00:00:00Z .. +10 min. The station's local
    /// time is UTC-6 (segment names read six hours earlier than UTC).
    pub(crate) fn fixture(t0: i64) -> Fixture {
        let root = std::env::temp_dir().join(format!("gs-bundle-media-{}", rand::random::<u64>()));
        let video = root.join("video");
        let voice = root.join("voice");
        // pad: one-minute segments from 23:58 to 00:12 local+6h; sizes grow with the minute.
        for minute in -2..12i64 {
            let start = t0 + minute * 60_000;
            let local = start - 6 * 3_600_000;
            let secs = local.div_euclid(1000);
            let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
            let (y, m, d) = civil(days);
            let name = format!("{y:04}-{m:02}-{d:02}_{:02}-{:02}-{:02}-000000.mp4", rem / 3600, rem / 60 % 60, rem % 60);
            put(&video.join("pad").join(name), &vec![b'v'; 1000 + minute.unsigned_abs() as usize * 100], start + 60_000);
        }
        // A stray file is ignored.
        put(&video.join("pad/notes.txt"), b"x", t0);
        // Two crew bursts inside the range and one radio burst after it.
        let wav = |ms: i64| {
            let mut bytes = crate::voice::recorder::wav_header((ms * 32) as u32).to_vec();
            bytes.resize(44 + (ms * 32) as usize, 0);
            bytes
        };
        put(&voice.join(format!("crew/{}-7.wav", t0 + 30_000)), &wav(200), t0 + 30_200);
        put(&voice.join(format!("crew/{}-9.wav", t0 + 300_000)), &wav(100), t0 + 300_100);
        put(&voice.join(format!("radio/{}-4294967294.wav", t0 + 900_000)), &wav(100), t0 + 900_100);
        Fixture { dirs: MediaDirs { video: Some(video), voice: Some(voice) }, root }
    }

    fn civil(days: i64) -> (i64, i64, i64) {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (yoe + era * 400 + i64::from(m <= 2), m, d)
    }

    const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

    #[test]
    fn segment_names_and_local_time() {
        assert_eq!(segment_name_ms("2026-01-01_00-00-00-000000.mp4"), Some(T0));
        assert_eq!(segment_name_ms("2026-01-01_00-00-01-500000.mp4"), Some(T0 + 1_500));
        assert_eq!(segment_name_ms("2026-13-01_00-00-00-000000.mp4"), None);
        assert_eq!(segment_name_ms("../x.mp4"), None);
        // Named 18:00 local (UTC-6), last written a minute after 00:00 UTC.
        let name = segment_name_ms("2025-12-31_18-00-00-000000.mp4").unwrap();
        assert_eq!(segment_span(name, T0 + 60_000), (T0, T0 + 60_000, "name_local_time_and_last_write"));
        // UTC+5:45 (a quarter-hour zone).
        assert_eq!(segment_span(T0 + 345 * 60_000, T0 + 59_000).0, T0);
        // A copied file whose write time says nothing: the name, as UTC.
        assert_eq!(segment_span(T0, T0 + 86_400_000 * 30), (T0, T0 + 60_000, "name_as_utc"));
        assert_eq!(civil(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn selection_is_by_overlap_then_smallest_first() {
        let clip = |stream: &str, start: i64, bytes: u64| Clip {
            kind: "video", stream: stream.into(), file: format!("{start}.mp4"), start_ms: start, end_ms: start + 60, bytes,
            media_type: "video/mp4", timing: "t", source: PathBuf::new(),
        };
        let clips = vec![clip("a", 0, 50), clip("a", 60, 10), clip("b", 60, 30), clip("a", 120, 20), clip("a", 500, 1)];
        let (kept, skipped) = select(clips, 30, 130, 60);
        let kept: Vec<_> = kept.iter().map(|c| (c.stream.as_str(), c.start_ms)).collect();
        assert_eq!(kept, [("a", 60), ("b", 60), ("a", 120)]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["start_ms"], 0);
        assert_eq!(skipped[0]["reason"], "over the budget");
        let (_, skipped) = select(vec![clip("a", 0, 100)], 0, 10, 60);
        assert_eq!(skipped[0]["reason"], "larger than the budget");
    }

    #[tokio::test]
    async fn entries_take_overlapping_clips_within_the_caps() {
        let fx = fixture(T0);
        let now = T0 + 86_400_000;
        let request = Request { video: true, video_max_mb: None, voice: true, voice_max_mb: None };
        let (files, summary) = entries(&request, &fx.dirs, Some(T0), Some(T0 + 600_000), now).await.unwrap();
        let paths: Vec<&str> = files.iter().map(|(p, ..)| p.as_str()).collect();
        // Segments starting 23:59..00:10 UTC overlap [00:00, 00:10]: the one ending at 00:00 too.
        assert_eq!(summary["video"]["included"], 12, "{paths:?}");
        assert_eq!(summary["voice"]["included"], 2);
        assert!(paths.contains(&"media/voice/crew/1767225630000-7.wav"));
        assert!(!paths.iter().any(|p| p.contains("radio")), "a burst after the range stays out");
        assert!(!paths.iter().any(|p| p.ends_with(".txt")));
        let index: Value = serde_json::from_slice(&files.last().unwrap().1).unwrap();
        let clips = index["clips"].as_array().unwrap();
        assert_eq!(clips.len(), 14);
        assert!(clips.windows(2).all(|w| w[0]["start_ms"].as_i64() <= w[1]["start_ms"].as_i64()));
        let first_video = clips.iter().find(|c| c["kind"] == "video").unwrap();
        assert_eq!(first_video["start_ms"], T0 - 60_000);
        assert_eq!(first_video["end_ms"], T0);
        assert_eq!(first_video["stream"], "pad");
        assert_eq!(first_video["timing"], "name_local_time_and_last_write");
        let burst = clips.iter().find(|c| c["kind"] == "voice").unwrap();
        assert_eq!((burst["start_ms"].as_i64(), burst["end_ms"].as_i64()), (Some(T0 + 30_000), Some(T0 + 30_200)));
        assert_eq!(burst["media_type"], "audio/wav");

        // A budget of 0 keeps none and says why.
        let request = Request { video: true, video_max_mb: Some(0), voice: false, voice_max_mb: None };
        let (files, summary) = entries(&request, &fx.dirs, Some(T0), Some(T0 + 600_000), now).await.unwrap();
        assert_eq!(files.len(), 1, "only the index");
        assert_eq!(summary["video"]["complete"], false);
        assert_eq!(summary["video"]["skipped"].as_array().unwrap().len(), 12);
        assert!(summary.get("voice").is_none());

        // Voice recording off on the station.
        let dirs = MediaDirs { video: fx.dirs.video.clone(), voice: None };
        let request = Request { video: false, video_max_mb: None, voice: true, voice_max_mb: Some(1000) };
        let (_, summary) = entries(&request, &dirs, Some(T0), Some(T0 + 600_000), now).await.unwrap();
        assert_eq!(summary["voice"]["recorded"], false);
        assert_eq!(summary["voice"]["budget_bytes"], u64::from(MAX_VOICE_BUDGET_MB) << 20, "the cap is clamped");
        assert!(entries(&Request::default(), &fx.dirs, Some(T0), Some(T0 + 1), now).await.is_none());
    }

    #[tokio::test]
    async fn segments_still_being_written_are_left_out() {
        let fx = fixture(T0);
        // "now" is 2 s after the last segment's last write.
        let found = scan(fx.dirs.video.as_ref().unwrap(), "video", T0 + 12 * 60_000 + 2_000).await;
        assert_eq!(found.len(), 13);
    }
}
