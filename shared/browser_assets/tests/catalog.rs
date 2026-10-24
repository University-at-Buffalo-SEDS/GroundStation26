#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new(replay: bool) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gs-asset-catalog-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("station_ui")).unwrap();
        fs::write(path.join("page.html"), "text asset").unwrap();
        fs::write(path.join("binary.bin"), [0_u8, 255, 1]).unwrap();
        let mut exports = vec![
            serde_json::json!({"name":"PAGE_HTML","path":"page.html","kind":"text"}),
            serde_json::json!({"name":"BINARY","path":"binary.bin","kind":"bytes"}),
        ];
        if replay {
            fs::write(path.join("replay.js"), b"export const replay = true;").unwrap();
            exports.push(
                serde_json::json!({"name":"REPLAY_SCENE_JS","path":"replay.js","kind":"bytes"}),
            );
        }
        fs::write(
            path.join("station_ui/manifest.json"),
            serde_json::to_vec(&serde_json::json!({"version":1,"exports":exports})).unwrap(),
        )
        .unwrap();
        Self(path)
    }
    fn verify(&self, replay: bool) {
        let generated = build_script::generate(&self.0);
        if !replay {
            assert!(!generated.contains("pub const REPLAY_SCENE_JS"));
        }
        let source = format!(
            "{}\nfn main() {{ let _: &str = PAGE_HTML; let _: &[u8] = BINARY; assert_eq!(asset_bytes(\"PAGE_HTML\"),Some(b\"text asset\".as_slice())); assert_eq!(asset_bytes(\"BINARY\"),Some([0_u8,255,1].as_slice())); assert_eq!(asset_bytes(\"unknown\"),None); assert_eq!(asset_bytes(\"REPLAY_SCENE_JS\").is_some(),{}); }}",
            generated, replay
        );
        let input = self.0.join("generated.rs");
        let output = self.0.join("verify");
        fs::write(&input, source).unwrap();
        assert!(
            Command::new("rustc")
                .args(["--edition=2024", "-C", "debuginfo=0"])
                .arg(&input)
                .arg("-o")
                .arg(&output)
                .status()
                .unwrap()
                .success()
        );
        assert!(Command::new(output).status().unwrap().success());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn old_manifest_has_no_invented_optional_export() {
    Fixture::new(false).verify(false);
}
#[test]
fn new_manifest_exports_actual_binary_and_text_bytes() {
    Fixture::new(true).verify(true);
}
#[test]
fn declared_missing_asset_is_still_an_error() {
    let fixture = Fixture::new(true);
    fs::remove_file(fixture.0.join("replay.js")).unwrap();
    assert!(std::panic::catch_unwind(|| build_script::generate(&fixture.0)).is_err());
}
