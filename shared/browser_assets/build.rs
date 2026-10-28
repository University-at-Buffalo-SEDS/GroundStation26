use serde::Deserialize;
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Component, Path, PathBuf},
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    exports: Vec<Export>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Export {
    name: String,
    path: String,
    kind: String,
}
fn main() {
    println!("cargo:rerun-if-env-changed=GS_FRONTEND_SOURCE_DIR");
    let source = selected_frontend_source();
    let source = source.canonicalize().unwrap_or_else(|error| {
        panic!("Frontend source {} missing: {error}. Run python3 build.py backend_only to fetch it, or set GS_FRONTEND_SOURCE_DIR to a frontend checkout.", source.display())
    });
    prepare_browser_assets(&source);
    let generated = generate(&source);
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("browser_assets.rs"),
        generated,
    )
    .unwrap();
}

// The build wrapper supplies one immutable external checkout to both the
// WebUI and backend. Direct Cargo reuses that WebUI receipt, or fetches main
// into the same external cache when no WebUI has been built yet.
fn selected_frontend_source() -> PathBuf {
    if let Some(source) = env::var_os("GS_FRONTEND_SOURCE_DIR") {
        return PathBuf::from(source);
    }
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let script = root.join("scripts/prepare_frontend_source.py");
    println!("cargo:rerun-if-changed={}", script.display());
    println!("cargo:rerun-if-changed={}", root.join("build.py").display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("frontend/dist/station-source.json").display()
    );
    println!("cargo:rerun-if-env-changed=GS26_FRONTEND_CHECKOUT_DIR");
    println!("cargo:rerun-if-env-changed=GS_PYTHON");
    let receipt = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("frontend-source.json");
    let output = python_command()
        .arg(&script)
        .arg("--output")
        .arg(&receipt)
        .output()
        .expect("Unable to resolve the external frontend source");
    assert!(
        output.status.success(),
        "Frontend source preparation failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    #[derive(Deserialize)]
    struct Source {
        version: u32,
        source_dir: PathBuf,
    }
    let source: Source =
        serde_json::from_slice(&fs::read(receipt).expect("Frontend source receipt missing"))
            .expect("Invalid frontend source receipt");
    assert_eq!(source.version, 1, "Unsupported frontend source receipt");
    source.source_dir
}

fn python_command() -> std::process::Command {
    let candidates = env::var_os("GS_PYTHON")
        .map(|python| vec![python])
        .unwrap_or_else(|| vec!["python3".into(), "python".into(), "py".into()]);
    for python in candidates {
        let mut probe = std::process::Command::new(&python);
        if python == "py" {
            probe.arg("-3");
        }
        match probe
            .args([
                "-c",
                "import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)",
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                let mut command = std::process::Command::new(python);
                if command.get_program() == "py" {
                    command.arg("-3");
                }
                return command;
            }
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("Unable to find Python3.10+ for browser assets: {error}"),
        }
    }
    panic!("Python3.10+ is required for frontend assets; set GS_PYTHON to its executable");
}

// Backend-only cargo builds must prepare the selected frontend's generated
// dependencies too. This performs no UI compilation or package installation.
pub fn prepare_browser_assets(source: &Path) {
    for name in [
        "MAPLIBRE_GL_VERSION",
        "MAPLIBRE_GL_BASE_URL",
        "GS_BROWSER_ASSETS_CACHE",
        "GS_PYTHON",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let script = source.join("scripts/browser_assets.py");
    println!("cargo:rerun-if-changed={}", script.display());
    if !script.is_file() {
        // Existing frontends without generated exports can still be selected.
        // generate() retains its explicit missing-asset error for older sources.
        return;
    }
    let output = python_command()
        .arg(&script)
        .arg("--root")
        .arg(source)
        .output()
        .expect("Unable to prepare selected frontend browser assets");
    assert!(
        output.status.success(),
        "Selected frontend browser asset preparation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// Export discovery is generated from the selected frontend, not a fixed revision.
pub fn generate(source: &Path) -> String {
    let source = source.canonicalize().expect("Frontend source missing");
    let manifest = source.join("station_ui/manifest.json");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(&manifest).expect("Selected frontend has no station asset manifest"),
    )
    .expect("Invalid station asset manifest");
    assert_eq!(manifest.version, 1, "Unsupported station asset manifest");
    assert!(!manifest.exports.is_empty() && manifest.exports.len() <= 64);
    let mut names = BTreeSet::new();
    let mut generated = String::new();
    let mut catalog = String::new();
    for asset in manifest.exports {
        assert!(
            !asset.name.is_empty()
                && asset.name.as_bytes()[0].is_ascii_uppercase()
                && asset
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                && names.insert(asset.name.clone()),
            "Invalid or duplicate asset identifier"
        );
        assert!(
            PathBuf::from(&asset.path)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
            "Asset path must be relative"
        );
        let path = source
            .join(&asset.path)
            .canonicalize()
            .expect("Frontend asset missing");
        assert!(
            path.starts_with(&source) && path.is_file(),
            "Asset must be inside selected frontend"
        );
        println!("cargo:rerun-if-changed={}", path.display());
        let (ty, include) = match asset.kind.as_str() {
            "text" => ("&str", "include_str"),
            "bytes" => ("&[u8]", "include_bytes"),
            _ => panic!("Unknown asset kind"),
        };
        catalog.push_str(&format!(
            "        {:?} => Some({}{}),\n",
            asset.name,
            asset.name,
            if asset.kind == "text" {
                ".as_bytes()"
            } else {
                ""
            },
        ));
        generated.push_str(&format!(
            "pub const {}: {} = {}!({:?});\n",
            asset.name,
            ty,
            include,
            path.to_str().expect("Asset path is not UTF-8")
        ));
    }
    generated
        .push_str("pub fn asset_bytes(name: &str) -> Option<&'static [u8]> {\n    match name {\n");
    generated.push_str(&catalog);
    generated.push_str("        _ => None,\n    }\n}\n");
    generated
}
