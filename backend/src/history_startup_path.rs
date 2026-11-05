//! Create only the implicit default data base; never repair a journal or user path.
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn environment() -> io::Result<PathBuf> {
    match std::env::var_os("GS_BOARD_ABORT_HISTORY_DIR") {
        Some(path) => prepare(Some(path), Path::new(".")),
        None => prepare(None, &std::env::current_dir()?),
    }
}

pub(crate) fn prepare(explicit: Option<OsString>, cwd: &Path) -> io::Result<PathBuf> {
    if let Some(path) = explicit {
        if path.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "GS_BOARD_ABORT_HISTORY_DIR is empty",
            ));
        }
        return Ok(PathBuf::from(path));
    }
    prepare_default(cwd)
}

#[cfg(unix)]
fn prepare_default(cwd: &Path) -> io::Result<PathBuf> {
    use std::fs::{File, OpenOptions};
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    fn open_at(parent: &File, name: &std::ffi::CStr, directory: bool) -> io::Result<File> {
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn marker(parent: &File, expected: &str) -> io::Result<()> {
        let file = open_at(parent, c"Cargo.toml", false)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other(
                "default history manifest is not a regular file",
            ));
        }
        let mut contents = String::new();
        file.take(65_537).read_to_string(&mut contents)?;
        if contents.len() > 65_536 || !contents.lines().any(|line| line.trim() == expected) {
            return Err(io::Error::other(
                "default history manifest identity mismatch",
            ));
        }
        Ok(())
    }
    let validate = || -> io::Result<File> {
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(cwd)?;
        marker(&root, "[workspace]")?;
        let backend = open_at(&root, c"backend", true)?;
        marker(&backend, r#"name = "groundstation_backend""#)?;
        Ok(backend)
    };
    let backend = validate().map_err(|error| io::Error::new(error.kind(), format!(
        "default abort history requires the GroundStation repository root working directory; use run_groundstation.py/run_rrls.py or set GS_BOARD_ABORT_HISTORY_DIR explicitly (configure GS_LAUNCH_CLOCK_STATE too for a custom working directory): {error}"
    )))?;
    let created = unsafe { libc::mkdirat(backend.as_raw_fd(), c"data".as_ptr(), 0o700) };
    if created != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    // Refuse symlinks/files even if mkdirat reported EEXIST. Never create ancestors.
    let data = open_at(&backend, c"data", true)?;
    data.sync_all()?;
    backend.sync_all()?;
    // Store still owns journal provisioning and its existing error policy. This
    // does not claim to harden every path operation inside the unchanged Store.
    Ok(cwd.join("backend/data/board_abort_history"))
}

#[cfg(not(unix))]
fn prepare_default(_cwd: &Path) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "default history base requires reviewed directory anchoring on this platform; configure an explicit history path",
    ))
}

#[cfg(test)]
#[path = "history_startup_path_tests.rs"]
mod tests;
