#[cfg(unix)]
mod unix;

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    unix::main()
}

#[cfg(not(unix))]
fn main() {
    eprintln!("gs-video-sender requires Linux/Unix (Raspberry Pi OS recommended)");
    std::process::exit(1);
}
