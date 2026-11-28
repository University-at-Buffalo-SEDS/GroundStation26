//! Opt-in crew voice recorder.
//!
//! Off unless the deployment sets `GS_VOICE_RECORDINGS_DIR`. When set, every
//! 20 ms crew or radio frame the station relays is also written, as 16 kHz
//! mono PCM16 WAV, one file per transmission ("burst") per speaker:
//! `<dir>/<stream>/<start_ms>-<speaker>.wav`, with `stream` `crew` (browser
//! and watch push-to-talk) or `radio` (the RRLS radio bridge) and `start_ms`
//! the Unix time of the burst's first frame. A burst ends after a second
//! without frames from that speaker, or after ten minutes; gaps shorter than
//! that are written as silence so the file keeps real time.
//!
//! The voice path never waits for it: frames go through a bounded queue
//! (`try_send`; a full queue drops the frame) to one writer thread, which
//! does all file I/O. Programme commentary is not recorded. Replay bundles
//! include the bursts that overlap a recording (`recording_bundle_media`).
use std::{
    collections::HashMap,
    fs::File,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    time::Duration,
};

/// Frames queued for the writer (10 s of one speaker).
const QUEUE: usize = 500;
/// Silence that ends a burst.
const BURST_GAP_MS: i64 = 1_000;
/// Longest burst before a new file starts.
const MAX_BURST_MS: i64 = 10 * 60_000;
const SAMPLE_RATE: u32 = 16_000;
const BYTES_PER_MS: i64 = (SAMPLE_RATE as i64 * 2) / 1000;
const HEADER: usize = 44;

/// Where voice bursts are written, when the deployment enabled recording.
pub(crate) fn directory() -> Option<PathBuf> {
    std::env::var_os("GS_VOICE_RECORDINGS_DIR").filter(|d| !d.is_empty()).map(PathBuf::from)
}

struct Frame {
    stream: &'static str,
    speaker: u32,
    at_ms: i64,
    pcm: Vec<u8>,
}

static RECORDER: OnceLock<Option<SyncSender<Frame>>> = OnceLock::new();

fn start() -> Option<SyncSender<Frame>> {
    let dir = directory()?;
    let (tx, rx) = sync_channel(QUEUE);
    std::thread::Builder::new().name("voice-recorder".into()).spawn(move || run(&dir, rx)).ok()?;
    log::info!("crew voice recording on: {}", directory().unwrap_or_default().display());
    Some(tx)
}

/// One relayed 20 ms frame (`stream` "crew" or "radio"). Never blocks.
pub(super) fn submit(stream: &'static str, speaker: u32, pcm: &[u8]) {
    let Some(tx) = RECORDER.get_or_init(start).as_ref() else { return };
    let at_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let _ = tx.try_send(Frame { stream, speaker, at_ms, pcm: pcm.to_vec() });
}

/// The 44-byte header of a PCM16 mono WAV holding `data` bytes.
pub(crate) fn wav_header(data: u32) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&data.saturating_add(36).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

/// Duration of a burst file from its size.
pub(crate) fn duration_ms(file_bytes: u64) -> i64 {
    (file_bytes.saturating_sub(HEADER as u64) as i64) / BYTES_PER_MS
}

/// `<start_ms>-<speaker>.wav` -> start_ms.
pub(crate) fn start_of(file: &str) -> Option<i64> {
    let (start, speaker) = file.strip_suffix(".wav")?.split_once('-')?;
    (!speaker.is_empty() && speaker.bytes().all(|b| b.is_ascii_digit()) && start.len() <= 16).then_some(())?;
    start.parse().ok()
}

struct Burst {
    file: File,
    start_ms: i64,
    /// Time just after the last written sample.
    end_ms: i64,
    data: u32,
}

impl Burst {
    fn open(dir: &Path, frame: &Frame) -> std::io::Result<Self> {
        let folder = dir.join(frame.stream);
        std::fs::create_dir_all(&folder)?;
        let mut file = File::create(folder.join(format!("{}-{}.wav", frame.at_ms, frame.speaker)))?;
        // Sizes are patched on close; a station stopped mid-burst leaves a
        // header that claims the largest size, which players read to the end.
        file.write_all(&wav_header(u32::MAX - 36))?;
        Ok(Self { file, start_ms: frame.at_ms, end_ms: frame.at_ms, data: 0 })
    }
    fn write(&mut self, frame: &Frame) -> std::io::Result<()> {
        // Arrival jitter of a frame or two is not a pause.
        let gap = (frame.at_ms - self.end_ms).min(BURST_GAP_MS);
        if gap > 40 {
            let silence = vec![0u8; (gap * BYTES_PER_MS) as usize];
            self.file.write_all(&silence)?;
            self.data += silence.len() as u32;
        }
        self.file.write_all(&frame.pcm)?;
        self.data += frame.pcm.len() as u32;
        self.end_ms = self.start_ms + i64::from(self.data) / BYTES_PER_MS;
        Ok(())
    }
    fn close(mut self) {
        let _ = self.file.seek(SeekFrom::Start(0)).and_then(|_| self.file.write_all(&wav_header(self.data)));
        let _ = self.file.flush();
    }
}

fn run(dir: &Path, rx: Receiver<Frame>) {
    let mut bursts: HashMap<(&'static str, u32), Burst> = HashMap::new();
    let mut warned = false;
    loop {
        let frame = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(frame) => Some(frame),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let now = frame.as_ref().map_or_else(
            || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64),
            |f| f.at_ms,
        );
        let ended: Vec<_> = bursts
            .iter()
            .filter(|(_, b)| now - b.end_ms > BURST_GAP_MS || b.end_ms - b.start_ms >= MAX_BURST_MS)
            .map(|(k, _)| *k)
            .collect();
        for key in ended {
            if let Some(burst) = bursts.remove(&key) {
                burst.close();
            }
        }
        let Some(frame) = frame else { continue };
        let key = (frame.stream, frame.speaker);
        let result = match bursts.get_mut(&key) {
            Some(burst) => burst.write(&frame),
            None => Burst::open(dir, &frame).and_then(|mut burst| {
                burst.write(&frame)?;
                bursts.insert(key, burst);
                Ok(())
            }),
        };
        if let Err(error) = result {
            bursts.remove(&key);
            if !warned {
                log::warn!("crew voice recording failed: {error}");
                warned = true;
            }
        }
    }
    for (_, burst) in bursts.drain() {
        burst.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursts_keep_real_time_and_close_with_their_size() {
        let dir = std::env::temp_dir().join(format!("gs-voice-rec-{}", rand::random::<u64>()));
        let (tx, rx) = sync_channel(QUEUE);
        let writer = {
            let dir = dir.clone();
            std::thread::spawn(move || run(&dir, rx))
        };
        let frame = |at_ms: i64| Frame { stream: "crew", speaker: 7, at_ms, pcm: vec![1u8; 640] };
        // 1 s of speech, a 200 ms pause (kept as silence), 1 s more, then a new burst.
        for i in 0..50 {
            tx.send(frame(1_000_000 + i * 20)).unwrap();
        }
        for i in 0..50 {
            tx.send(frame(1_001_200 + i * 20)).unwrap();
        }
        tx.send(frame(1_010_000)).unwrap();
        drop(tx);
        writer.join().unwrap();
        let first = std::fs::read(dir.join("crew/1000000-7.wav")).unwrap();
        assert_eq!(&first[..HEADER], &wav_header((first.len() - HEADER) as u32));
        assert_eq!(duration_ms(first.len() as u64), 2_200);
        assert_eq!(start_of("1000000-7.wav"), Some(1_000_000));
        assert!(dir.join("crew/1010000-7.wav").exists());
        assert_eq!(start_of("x-7.wav"), None);
        assert_eq!(start_of("1000-.wav"), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
