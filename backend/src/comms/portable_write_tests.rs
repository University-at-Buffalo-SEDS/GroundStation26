#[cfg(target_os = "linux")]
use super::rfd_capacity::Capacity;
use super::write_all_std_counted;
use std::collections::VecDeque;
use std::io::{self, Write};

struct ScriptedWriter {
    actions: VecDeque<io::Result<usize>>,
    written: Vec<u8>,
    flush_fails: bool,
}

impl Write for ScriptedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.actions.pop_front().expect("unexpected write")?;
        self.written.extend_from_slice(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.flush_fails {
            Err(io::Error::other("original flush error"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn interrupted_zero_and_error_writes_preserve_the_exact_prefix() {
    let mut writer = ScriptedWriter {
        actions: VecDeque::from([
            Err(io::ErrorKind::Interrupted.into()),
            Ok(3),
            Err(io::ErrorKind::Interrupted.into()),
            Ok(2),
        ]),
        written: vec![],
        flush_fails: false,
    };
    let mut accepted = usize::MAX;
    write_all_std_counted(&mut writer, b"abcde", &mut accepted).unwrap();
    assert_eq!(accepted, 5);
    assert_eq!(writer.written, b"abcde");

    let mut writer = ScriptedWriter {
        actions: VecDeque::from([Ok(3), Ok(0)]),
        written: vec![],
        flush_fails: false,
    };
    let error = write_all_std_counted(&mut writer, b"abcde", &mut accepted).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    assert_eq!(accepted, 3);
    assert_eq!(writer.written, b"abc");

    let mut writer = ScriptedWriter {
        actions: VecDeque::from([
            Ok(2),
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "original write error",
            )),
        ]),
        written: vec![],
        flush_fails: false,
    };
    let error = write_all_std_counted(&mut writer, b"abcde", &mut accepted).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(error.to_string(), "original write error");
    assert_eq!(accepted, 2);
}

#[test]
fn complete_writes_match_write_all_and_flush_errors_keep_the_full_count() {
    let bytes = b"same complete UART frame";
    let mut original = Vec::new();
    original.write_all(bytes).unwrap();
    original.flush().unwrap();
    let mut counted = Vec::new();
    let mut accepted = 0;
    write_all_std_counted(&mut counted, bytes, &mut accepted)
        .and_then(|_| counted.flush())
        .unwrap();
    assert_eq!(counted, original);
    assert_eq!(accepted, bytes.len());

    let mut writer = ScriptedWriter {
        actions: VecDeque::from([Ok(5)]),
        written: vec![],
        flush_fails: true,
    };
    let error = write_all_std_counted(&mut writer, b"abcde", &mut accepted)
        .and_then(|_| writer.flush())
        .unwrap_err();
    assert_eq!(accepted, 5);
    assert_eq!(writer.written, b"abcde");
    assert_eq!(error.to_string(), "original flush error");
}

#[test]
#[cfg(target_os = "linux")]
fn boxed_serial_port_preserves_full_success_and_actual_failed_prefix() {
    const CHILD: &str = "GS_PORTABLE_STD_PREFIX_CHILD";
    const NAME: &str = "comms::portable_write_tests::boxed_serial_port_preserves_full_success_and_actual_failed_prefix";
    if std::env::var_os(CHILD).is_none() {
        // TTYPort::pair's master lacks CLOEXEC. Construct this test's ports in
        // a dedicated child so concurrently spawned air tests cannot inherit
        // their master descriptors and alter the fault under test.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env_remove("GS_SIM_CLOCK_FILE")
            .env_remove("GS_SIMULATED_SERIAL_PTY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "portable UART child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }
    use serialport::{SerialPort, TTYPort};
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    let (mut peer, port) = TTYPort::pair().unwrap();
    peer.set_timeout(Duration::from_millis(100)).unwrap();
    let mut inner: Box<dyn SerialPort> = Box::new(port);
    let frame = b"same boxed serial frame";
    let mut accepted = usize::MAX;
    write_all_std_counted(&mut inner, frame, &mut accepted)
        .and_then(|_| inner.flush())
        .unwrap();
    let mut received = vec![0; frame.len()];
    peer.read_exact(&mut received).unwrap();
    assert_eq!(received, frame);
    assert_eq!(accepted, frame.len());
    drop(inner);
    drop(peer);

    let mut proved_partial = false;
    for space in [512usize, 1024, 2048, 3072] {
        let (mut peer, port) = TTYPort::pair().unwrap();
        peer.set_timeout(Duration::from_millis(50)).unwrap();
        // Fixture-only nonblocking mode makes the Linux short write bounded.
        // All writes still use the actual boxed SerialPort std::Write API.
        let fd = port.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0);
        let mut inner: Box<dyn SerialPort> = Box::new(port);
        inner.set_timeout(Duration::from_millis(10)).unwrap();
        let mut filled = 0;
        loop {
            match inner.write(&[0x55u8; 1024]) {
                Ok(count) if count > 0 => filled += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                other => panic!("unexpected prefill {other:?}"),
            }
            assert!(filled < 1_000_000);
        }
        let mut released_bytes = vec![0; space];
        let released = peer.read(&mut released_bytes).unwrap();
        assert!(released > 0);
        let payload = vec![0x77u8; 4096];
        let frame = super::build_raw_uart_frame(&payload).unwrap();
        assert_eq!(frame.len(), 4100);
        let mut accepted = 0;
        let result =
            write_all_std_counted(&mut inner, &frame, &mut accepted).and_then(|_| inner.flush());
        let mut received = Vec::new();
        let mut bytes = [0u8; 8192];
        loop {
            match peer.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => received.extend_from_slice(&bytes[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                other => panic!("unexpected drain {other:?}"),
            }
        }
        if result.is_err() && accepted > 0 && accepted < frame.len() {
            assert_eq!(received.len(), filled - released + accepted);
            assert_eq!(&received[filled - released..], &frame[..accepted]);
            assert_eq!(
                payload.len(),
                4096,
                "caller retains the complete logical packet"
            );
            // The caller can charge the physical prefix before returning Err;
            // failed logical IO earns neither ordinary credit nor a close.
            let wall = Instant::now();
            let mut capacity = Capacity::new(64_000, 35, 16);
            capacity.committed(wall, None, accepted);
            capacity.drained(wall);
            let required_ms = ((accepted + 16) * 8000).div_ceil(22_400) as u64;
            assert!(!capacity.ready(wall + Duration::from_millis(required_ms - 1), None));
            assert!(capacity.ready(wall + Duration::from_millis(required_ms), None));
            assert_eq!(capacity.ordinary_service_credit_ms(), 0);
            eprintln!(
                "BOXED_STD_PREFIX accepted={accepted}/4100 debt_ms={required_ms} result={result:?}"
            );
            proved_partial = true;
            break;
        }
    }
    assert!(
        proved_partial,
        "fixture must observe an actual boxed SerialPort partial write"
    );
}

/// A raw UART write that has started must finish: on EAGAIN after a partial
/// write the writer waits for writability and resumes. Giving up after the
/// 10 ms poll left a frame prefix on the wire and the retry sent the whole
/// frame again behind it (bench Pi: "operation would block" on ttyAMA0).
#[cfg(target_os = "linux")]
#[test]
fn partial_uart_write_waits_and_resumes_instead_of_leaving_a_prefix() {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) }, 0);
    let mut reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETPIPE_SZ, 65536) };
    let capacity = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETPIPE_SZ) } as usize;
    // The kernel buffer is nearly full; the reader (the UART) drains it 30 ms later.
    super::write_all_fd_with_poll(writer.as_raw_fd(), &vec![0u8; capacity - 5000], super::UART_TX_POLL_TIMEOUT).unwrap();
    let drain = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        let mut all = Vec::new();
        let mut buf = [0u8; 8192];
        let until = std::time::Instant::now() + std::time::Duration::from_millis(800);
        while std::time::Instant::now() < until {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
        all
    });
    // Larger than PIPE_BUF so the pipe, like a tty, accepts a partial write.
    let frame: Vec<u8> = (0..20000u32).map(|n| n as u8).collect();
    let mut accepted = 0;
    let result = super::write_all_fd_with_poll_counted(writer.as_raw_fd(), &frame, super::UART_TX_POLL_TIMEOUT, &mut accepted);
    drop(writer);
    let received = drain.join().unwrap();
    assert!(result.is_ok(), "a started frame is finished, not abandoned: {result:?} after {accepted} bytes");
    assert_eq!(&received[capacity - 5000..], &frame[..]);
}
