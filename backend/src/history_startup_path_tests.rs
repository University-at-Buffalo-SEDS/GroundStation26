use super::*;
use crate::board_abort_history::Store;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "gs-startup-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        fs::write(p.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::create_dir(p.join("backend")).unwrap();
        fs::write(
            p.join("backend/Cargo.toml"),
            "[package]\nname = \"groundstation_backend\"\n",
        )
        .unwrap();
        Self(p)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn startup_actual_store_missing_parent_red_default_green_unknown_all7() {
    let r = Root::new();
    let p = r.0.join("backend/data/board_abort_history");
    assert_eq!(
        Store::provision_uncertain(&p, [0x42; 16], 0x7f)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(prepare(None, &r.0).unwrap(), p);
    let mut s = Store::provision_uncertain(&p, [0x42; 16], 0x7f).unwrap();
    let x = s.begin_session(0x7f).unwrap();
    assert_eq!(x.snapshot.unknown, 0x7f);
    assert_eq!(x.snapshot.actual_abort, 0);
    let anchor = fs::read(p.join("anchor")).unwrap();
    drop(s);
    let mut s = Store::open(&prepare(None, &r.0).unwrap(), [0x42; 16]).unwrap();
    let x = s.begin_session(0x7f).unwrap();
    assert_eq!(x.snapshot.unknown, 0x7f);
    assert!(x.previous_session_unclean);
    assert_eq!(anchor, fs::read(p.join("anchor")).unwrap());
}
#[test]
fn startup_explicit_verbatim_no_hierarchy() {
    let r = Root::new();
    for p in [
        PathBuf::from("relative/typo/history"),
        r.0.join("missing/typo/history"),
    ] {
        assert_eq!(
            prepare(Some(p.clone().into_os_string()), &r.0.join("backend")).unwrap(),
            p
        );
        assert!(!r.0.join("missing").exists());
        assert!(!r.0.join("backend/data").exists());
    }
    assert!(prepare(Some(OsString::new()), &r.0).is_err());
}
#[test]
fn startup_wrong_cwd_never_creates_second_journal() {
    let r = Root::new();
    let e = prepare(None, &r.0.join("backend")).unwrap_err();
    assert!(e.to_string().contains("repository root working directory"));
    assert!(!r.0.join("backend/backend").exists());
    assert!(!r.0.join("backend/data").exists());
}
#[test]
fn startup_bad_or_missing_backend_identity_refused() {
    let r = Root::new();
    fs::write(
        r.0.join("backend/Cargo.toml"),
        "[package]\nname = \"other\"\n",
    )
    .unwrap();
    assert!(prepare(None, &r.0).is_err());
    assert!(!r.0.join("backend/data").exists());
    fs::remove_dir_all(r.0.join("backend")).unwrap();
    assert!(prepare(None, &r.0).is_err());
    assert!(!r.0.join("backend").exists());
}
#[test]
fn startup_malformed_history_and_tombstone_preserved() {
    for bad in ["anchor", "state"] {
        let r = Root::new();
        let p = prepare(None, &r.0).unwrap();
        let s = Store::provision_uncertain(&p, [0x42; 16], 0x7f).unwrap();
        drop(s);
        fs::write(p.join(bad), b"tombstone-no-replacement").unwrap();
        assert_eq!(prepare(None, &r.0).unwrap(), p);
        assert!(Store::open(&p, [0x42; 16]).is_err());
        assert!(Store::provision_uncertain(&p, [0x42; 16], 0x7f).is_err());
        assert_eq!(fs::read(p.join(bad)).unwrap(), b"tombstone-no-replacement");
    }
}
#[cfg(unix)]
#[test]
fn startup_symlink_data_backend_manifest_refused() {
    use std::os::unix::fs::symlink;
    let r = Root::new();
    let other = Root::new();
    symlink(&other.0, r.0.join("backend/data")).unwrap();
    assert!(prepare(None, &r.0).is_err());
    assert!(!other.0.join("board_abort_history").exists());
    fs::remove_file(r.0.join("backend/data")).unwrap();
    fs::remove_file(r.0.join("backend/Cargo.toml")).unwrap();
    symlink(
        other.0.join("backend/Cargo.toml"),
        r.0.join("backend/Cargo.toml"),
    )
    .unwrap();
    assert!(prepare(None, &r.0).is_err());
    fs::remove_dir_all(r.0.join("backend")).unwrap();
    symlink(other.0.join("backend"), r.0.join("backend")).unwrap();
    assert!(prepare(None, &r.0).is_err());
    assert!(!other.0.join("backend/data").exists());
}
#[test]
fn startup_data_file_preserved() {
    let r = Root::new();
    fs::write(r.0.join("backend/data"), b"keep").unwrap();
    assert!(prepare(None, &r.0).is_err());
    assert_eq!(fs::read(r.0.join("backend/data")).unwrap(), b"keep");
}
#[cfg(unix)]
#[test]
fn startup_existing_permissions_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let r = Root::new();
    fs::create_dir(r.0.join("backend/data")).unwrap();
    fs::set_permissions(r.0.join("backend/data"), fs::Permissions::from_mode(0o750)).unwrap();
    prepare(None, &r.0).unwrap();
    assert_eq!(
        fs::metadata(r.0.join("backend/data"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
}
#[test]
fn startup_concurrent_parent_creation() {
    let r = Root::new();
    std::thread::scope(|s| {
        let a = s.spawn(|| prepare(None, &r.0));
        let b = s.spawn(|| prepare(None, &r.0));
        assert_eq!(a.join().unwrap().unwrap(), b.join().unwrap().unwrap());
    });
    assert_eq!(fs::read_dir(r.0.join("backend/data")).unwrap().count(), 0);
}
