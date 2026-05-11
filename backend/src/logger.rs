use anyhow::Result;
use flexi_logger::{
    Cleanup, Criterion, DeferredNow, Duplicate, FileSpec, Logger, Naming, Record, WriteMode,
};
use std::path::PathBuf;

const LOG_BASENAME: &str = "groundstation";
const LOG_ROTATE_BYTES: u64 = 100 * 1024 * 1024;

pub fn init() -> Result<()> {
    let log_dir = log_dir();
    std::fs::create_dir_all(&log_dir)?;

    let spec = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "info,sqlx=warn,hyper=warn,h2=warn,tower_http=warn".to_string());

    Logger::try_with_str(spec)?
        .log_to_file(
            FileSpec::default()
                .directory(log_dir)
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
        .format(format_record)
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

fn format_record(
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
    )?;
    writeln!(writer)
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        log::error!("panic: {panic_info}");
        previous(panic_info);
    }));
}
