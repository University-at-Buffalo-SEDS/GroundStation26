use anyhow::Result;
use flexi_logger::{
    Cleanup, Criterion, DeferredNow, Duplicate, FileSpec, Logger, Naming, Record, WriteMode,
};
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

const LOG_BASENAME: &str = "groundstation";
const LOG_CURRENT_INFIX: &str = "rCURRENT";
const LOG_ROTATE_BYTES: u64 = 100 * 1024 * 1024;
static LOG_ENTRY_ID: AtomicU64 = AtomicU64::new(1);
static ARCHIVE_LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init() -> Result<()> {
    let log_dir = log_dir();
    std::fs::create_dir_all(&log_dir)?;
    init_archive_log_file(&log_dir)?;
    write_file_open_separator(&log_dir)?;

    let spec = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "info,sqlx=warn,hyper=warn,h2=warn,tower_http=warn".to_string());

    Logger::try_with_str(spec)?
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
        .start()?;

    install_panic_hook();
    Ok(())
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

fn format_console_record(
    writer: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record<'_>,
) -> std::io::Result<()> {
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("unnamed");
    let file = record.file().unwrap_or("unknown");
    let line = record.line().unwrap_or(0);
    write!(
        writer,
        "{} {:<5} [{} {:?}] {}:{} {}",
        now.now().format("%Y-%m-%d %H:%M:%S%.3f%:z"),
        record.level(),
        thread_name,
        thread.id(),
        file,
        line,
        record.args()
    )
}

fn format_file_record(
    writer: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record<'_>,
) -> std::io::Result<()> {
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("unnamed");
    let file = record.file().unwrap_or("unknown");
    let line = record.line().unwrap_or(0);
    let rendered = format!(
        "{} {:<5} [{} {:?}] {}:{} {}",
        now.now().format("%Y-%m-%d %H:%M:%S%.3f%:z"),
        record.level(),
        thread_name,
        thread.id(),
        file,
        line,
        record.args()
    );
    writer.write_all(rendered.as_bytes())?;
    append_to_archive(rendered.as_bytes())?;
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
        previous(panic_info);
    }));
}
