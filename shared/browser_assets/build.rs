use serde::Deserialize;
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Component, PathBuf},
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
    let source = env::var_os("GS_FRONTEND_SOURCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(
                env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory missing"),
            )
            .join("../../ui")
        });
    let source = source.canonicalize().unwrap_or_else(|error| {
        panic!("Frontend source {} missing: {error}. Initialize the ui submodule with git submodule update --init ui, or set GS_FRONTEND_SOURCE_DIR to its checkout.", source.display())
    });
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
        generated.push_str(&format!(
            "pub const {}: {} = {}!({:?});\n",
            asset.name,
            ty,
            include,
            path.to_str().expect("Asset path is not UTF-8")
        ));
    }
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("browser_assets.rs"),
        generated,
    )
    .unwrap();
}
