//! Reserve descriptors for HTTP concurrency, DNS, SQLite WAL and local files.
use anyhow::Result;

#[cfg(unix)]
pub fn prepare(concurrency: usize) -> Result<()> {
    use anyhow::{Context, bail};
    let required = (concurrency as libc::rlim_t)
        .saturating_mul(2)
        .saturating_add(256);
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: limits is initialized writable storage; RLIMIT_NOFILE is valid.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("reading downloader file descriptor limit");
    }
    if limits.rlim_cur >= required {
        return Ok(());
    }
    if limits.rlim_max < required {
        bail!(
            "map download concurrency {concurrency} needs a file descriptor limit of at least {required}, but this process has a hard limit of {}. Raise the launching shell/service's nofile limit or explicitly choose a smaller MAP_MAX_CONCURRENT; downloaded tiles are retained",
            limits.rlim_max
        );
    }
    let previous = limits.rlim_cur;
    limits.rlim_cur = required;
    // SAFETY: limits is valid, and only this process's soft limit is raised;
    // the existing hard limit is preserved. No privileged/system change.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!("raising downloader file descriptor limit from {previous} to {required}")
        });
    }
    println!(
        "Map downloader file descriptor limit: {previous} -> {required} (concurrency {concurrency})"
    );
    Ok(())
}

#[cfg(not(unix))]
pub fn prepare(_: usize) -> Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn low_soft_limit_is_raised_in_child_without_changing_hard_limit() {
        if std::env::var_os("MAP_TEST_FD_CHILD").is_some() {
            let mut limits = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: valid initialized storage, isolated child test process.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) },
                0
            );
            let hard = limits.rlim_max;
            assert!(hard >= 2304, "test host hard limit too low");
            limits.rlim_cur = 64;
            // SAFETY: lowering only this child process's soft limit.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) }, 0);
            prepare(1024).unwrap();
            // SAFETY: valid initialized storage.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) },
                0
            );
            assert_eq!(limits.rlim_cur, 2304);
            assert_eq!(limits.rlim_max, hard);
            let files: Vec<_> = (0..1024)
                .map(|_| std::fs::File::open("/dev/null").unwrap())
                .collect();
            assert_eq!(files.len(), 1024);
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "limits::tests::low_soft_limit_is_raised_in_child_without_changing_hard_limit",
            ])
            .env("MAP_TEST_FD_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn insufficient_hard_limit_reports_actionable_error_in_child() {
        if std::env::var_os("MAP_TEST_HARD_FD_CHILD").is_some() {
            let limits = libc::rlimit {
                rlim_cur: 64,
                rlim_max: 64,
            };
            // SAFETY: irreversibly lowers only this disposable child process.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) }, 0);
            let error = prepare(1024).unwrap_err().to_string();
            assert!(error.contains("2304"));
            assert!(error.contains("hard limit of 64"));
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "limits::tests::insufficient_hard_limit_reports_actionable_error_in_child",
            ])
            .env("MAP_TEST_HARD_FD_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
