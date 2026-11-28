use anyhow::Result;
use flexi_logger::{
    Cleanup, Criterion, DeferredNow, Duplicate, FileSpec, Logger, Naming, Record, WriteMode,
};
use std::cell::RefCell;
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, OnceLock};

const LOG_BASENAME: &str = "groundstation";
const LOG_CURRENT_INFIX: &str = "rCURRENT";
const LOG_ROTATE_BYTES: u64 = 100 * 1024 * 1024;
static LOG_ENTRY_ID: AtomicU64 = AtomicU64::new(1);
static ARCHIVE_LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Flushes the queued lines when main returns.
pub struct Flush;
impl Drop for Flush {
    fn drop(&mut self) {
        log::logger().flush();
    }
}

pub fn init() -> Result<Flush> {
    let log_dir = log_dir();
    std::fs::create_dir_all(&log_dir)?;
    init_archive_log_file(&log_dir)?;
    write_file_open_separator(&log_dir)?;

    let spec = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "info,sqlx=warn,hyper=warn,h2=warn,tower_http=warn".to_string());

    // `build` also applies the specification's maximum level. The handle only
    // reconfigures the logger; the files are written unbuffered.
    let (files, _handle) = Logger::try_with_str(spec)?
        .log_to_file(
            FileSpec::default()
                .directory(&log_dir)
                .basename(LOG_BASENAME)
                .suppress_timestamp(),
        )
        .append()
        .duplicate_to_stderr(Duplicate::Info)
        .rotate(
            Criterion::Size(LOG_ROTATE_BYTES),
            Naming::Numbers,
            Cleanup::Never,
        )
        .write_mode(WriteMode::Direct)
        .format_for_files(format_file_record)
        .format_for_stderr(format_console_record)
        .build()?;
    log::set_boxed_logger(Box::new(Queued::start(files)?))?;

    install_panic_hook();
    Ok(Flush)
}

/// Lines waiting for a disk that has stopped answering. Past this they are
/// counted and dropped: the station goes on without them.
const QUEUED_LINES: usize = 16_384;
/// Longest a flush (exit, panic) waits for such a disk.
const FLUSH_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Writes log lines on a thread of their own, in order. The thread that logs
/// may be handling an operator Abort, or be the router worker that sends it;
/// neither waits for the log files.
pub(crate) struct Queued {
    files: Arc<dyn log::Log>,
    queue: SyncSender<Queue>,
    dropped: Arc<AtomicU64>,
}
enum Queue {
    Line(Box<Line>),
    Flush(SyncSender<()>),
}
struct Line {
    origin: Origin,
    level: log::Level,
    target: String,
    module: Option<String>,
    file: Option<String>,
    line: Option<u32>,
    text: String,
}
/// Where and when a queued line was logged.
struct Origin {
    now: DeferredNow,
    thread: String,
}
thread_local! {
    static ORIGIN: RefCell<Option<Origin>> = const { RefCell::new(None) };
}
fn this_thread() -> String {
    let thread = std::thread::current();
    format!("{} {:?}", thread.name().unwrap_or("unnamed"), thread.id())
}
impl Queued {
    pub(crate) fn start(files: impl Into<Arc<dyn log::Log>>) -> Result<Self> {
        let files = files.into();
        let (queue, lines) = std::sync::mpsc::sync_channel(QUEUED_LINES);
        let dropped = Arc::new(AtomicU64::new(0));
        let (writer, missed) = (files.clone(), dropped.clone());
        std::thread::Builder::new().name("log_writer".into()).spawn(move || {
            for item in lines {
                match item {
                    Queue::Line(line) => line.write(writer.as_ref()),
                    Queue::Flush(done) => {
                        writer.flush();
                        let _ = done.try_send(());
                    }
                }
                let missed = missed.swap(0, Ordering::Relaxed);
                if missed > 0 {
                    writer.log(
                        &Record::builder()
                            .level(log::Level::Error)
                            .target(module_path!())
                            .args(format_args!("{missed} log line(s) dropped: the log files were not being written"))
                            .build(),
                    );
                }
            }
        })?;
        Ok(Self { files, queue, dropped })
    }
}
impl Line {
    fn write(self: Box<Self>, files: &dyn log::Log) {
        let line = *self;
        ORIGIN.with(|origin| *origin.borrow_mut() = Some(line.origin));
        files.log(
            &Record::builder()
                .level(line.level)
                .target(&line.target)
                .module_path(line.module.as_deref())
                .file(line.file.as_deref())
                .line(line.line)
                .args(format_args!("{}", line.text))
                .build(),
        );
        ORIGIN.with(|origin| *origin.borrow_mut() = None);
    }
}
impl log::Log for Queued {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.files.enabled(metadata)
    }
    fn log(&self, record: &Record<'_>) {
        if !self.files.enabled(record.metadata()) {
            return;
        }
        let mut now = DeferredNow::new();
        now.now();
        let line = Line {
            origin: Origin { now, thread: this_thread() },
            level: record.level(),
            target: record.target().to_owned(),
            module: record.module_path().map(str::to_owned),
            file: record.file().map(str::to_owned),
            line: record.line(),
            text: record.args().to_string(),
        };
        if self.queue.try_send(Queue::Line(Box::new(line))).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn flush(&self) {
        let (done, finished) = std::sync::mpsc::sync_channel(1);
        if self.queue.try_send(Queue::Flush(done)).is_ok() {
            let _ = finished.recv_timeout(FLUSH_WAIT);
        }
    }
}

fn log_dir() -> PathBuf {
    if let Ok(path) = std::env::var("GS_LOG_DIR") {
        return PathBuf::from(path);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join("logs")
}

fn init_archive_log_file(log_dir: &std::path::Path) -> Result<()> {
    let archive_path = log_dir.join(format!("{LOG_BASENAME}.log"));
    let archive_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(archive_path)?;
    let _ = ARCHIVE_LOG_FILE.set(Mutex::new(archive_file));
    Ok(())
}

fn write_file_open_separator(log_dir: &std::path::Path) -> Result<()> {
    let entry_id = LOG_ENTRY_ID.fetch_add(1, Ordering::Relaxed);
    let current_log_path = log_dir.join(format!("{LOG_BASENAME}_{LOG_CURRENT_INFIX}.log"));
    let mut current_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(current_log_path)?;
    let separator = format!("==========({entry_id:010})=============\n");
    current_file.write_all(separator.as_bytes())?;
    current_file.flush()?;
    append_to_archive_raw(separator.as_bytes())?;
    Ok(())
}

fn rendered_record(now: &mut DeferredNow, record: &Record<'_>) -> String {
    let file = record.file().unwrap_or("unknown");
    let line = record.line().unwrap_or(0);
    let render = |now: &mut DeferredNow, thread: &str| {
        format!(
            "{} {:<5} [{}] {}:{} {}",
            now.now().format("%Y-%m-%d %H:%M:%S%.3f%:z"),
            record.level(),
            thread,
            file,
            line,
            record.args()
        )
    };
    // A queued line states when and where it was logged, not written.
    ORIGIN.with(|origin| match origin.borrow_mut().as_mut() {
        Some(Origin { now, thread }) => render(now, thread),
        None => render(now, &this_thread()),
    })
}

fn format_console_record(
    writer: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record<'_>,
) -> std::io::Result<()> {
    let rendered = rendered_record(now, record);
    let safe = crate::log_redaction::redact_urls(&rendered);
    writer.write_all(safe.as_bytes())
}

fn format_file_record(
    writer: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record<'_>,
) -> std::io::Result<()> {
    let rendered = rendered_record(now, record);
    let safe = crate::log_redaction::redact_urls(&rendered);
    writer.write_all(safe.as_bytes())?;
    append_to_archive(safe.as_bytes())?;
    Ok(())
}

fn append_to_archive(bytes: &[u8]) -> std::io::Result<()> {
    append_to_archive_raw(bytes)?;
    append_to_archive_raw(b"\n")
}

fn append_to_archive_raw(bytes: &[u8]) -> std::io::Result<()> {
    let Some(file) = ARCHIVE_LOG_FILE.get() else {
        return Ok(());
    };
    let mut file = file
        .lock()
        .map_err(|_| std::io::Error::other("archive log mutex poisoned"))?;
    file.write_all(bytes)?;
    file.flush()
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        log::error!("panic: {panic_info}");
        log::logger().flush();
        previous(panic_info);
    }));
}
#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn console_and_file_sinks_hide_credentials_without_hiding_endpoint_or_error() {
        let record = Record::builder()
            .args(format_args!("request failed https://fake-user:FAKE_PASSWORD@station.local/api?ticket=FAKE_TICKET (timeout)"))
            .level(flexi_logger::Level::Warn)
            .file(Some("fake.rs"))
            .line(Some(42))
            .build();
        for formatter in [format_console_record, format_file_record] {
            let mut output = Vec::new();
            formatter(&mut output, &mut DeferredNow::new(), &record).unwrap();
            let text = String::from_utf8(output).unwrap();
            for secret in ["fake-user", "FAKE_PASSWORD", "FAKE_TICKET"] {
                assert!(!text.contains(secret));
            }
            assert!(text.contains("station.local/api"));
            assert!(text.contains("timeout"));
            assert!(text.contains("WARN"));
            assert!(text.contains("fake.rs:42"));
        }
    }
}

#[cfg(test)]
pub(crate) mod queue_tests {
    use super::*;
    use crate::disk_stall::{Site, Stall, wait};
    use log::Log as _;
    use std::time::Duration;

    /// Log files on a disk a test can stop.
    #[derive(Clone, Default)]
    pub(crate) struct Files(Arc<Mutex<Vec<String>>>);
    impl log::Log for Files {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::Level::Info
        }
        fn log(&self, record: &Record<'_>) {
            wait(Site::Log);
            self.0.lock().unwrap().push(rendered_record(&mut DeferredNow::new(), record));
        }
        fn flush(&self) {}
    }
    impl From<Files> for Arc<dyn log::Log> {
        fn from(files: Files) -> Self {
            Arc::new(files)
        }
    }
    fn info(logger: &Queued, text: &str) {
        logger.log(&Record::builder().level(log::Level::Info).file(Some("fake.rs")).line(Some(7)).args(format_args!("{text}")).build());
    }

    #[test]
    fn a_stopped_log_file_holds_no_thread_that_logs_and_loses_no_line() {
        if crate::operator_queue::isolated_case(module_path!(), "a_stopped_log_file_holds_no_thread_that_logs_and_loses_no_line") {
            return;
        }
        let files = Files::default();
        let logger = Arc::new(Queued::start(files.clone()).unwrap());
        let stall = Stall::begin(Site::Log);
        let (logged, done) = std::sync::mpsc::channel();
        let abort = {
            let logger = logger.clone();
            std::thread::Builder::new()
                .name("abort".into())
                .spawn(move || {
                    for n in 0..500 {
                        info(&logger, &format!("line {n}"));
                    }
                    let _ = logged.send(this_thread());
                })
                .unwrap()
        };
        assert!(stall.entered());
        let thread = done.recv_timeout(Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
        let logged_by = DeferredNow::new().now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        std::thread::sleep(Duration::from_millis(20));
        drop(stall);
        abort.join().unwrap();
        let thread = thread.expect("the thread that logs is not held by the log file");
        logger.flush();
        let written = files.0.lock().unwrap();
        assert_eq!(written.len(), 500);
        for (n, line) in written.iter().enumerate() {
            assert!(line.ends_with(&format!("fake.rs:7 line {n}")), "{line}");
            assert!(line.contains(&format!("[{thread}]")), "the thread that logged it: {line}");
        }
        assert!(written[499].as_str() < logged_by.as_str(), "stamped when logged, not when written");
    }
}
