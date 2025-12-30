use std::process::Output;
use std::{env, fs, io::Write, path::Path, path::PathBuf, process::Command};

fn build_apple_objc(manifest_dir: &PathBuf, target: &str) {
    if !target.contains("apple-") {
        return;
    }

    let src = manifest_dir.join("assets/LocationShim.m");
    println!("cargo:rerun-if-changed={}", src.display());
    if !src.exists() {
        panic!("ObjC shim not found: {}", src.display());
    }

    let (sdk, clang_target, out_subdir) = if target.contains("apple-ios-sim") {
        (
            "iphonesimulator",
            "arm64-apple-ios13.0-simulator",
            "ios-sim",
        )
    } else if target.contains("apple-ios") {
        ("iphoneos", "arm64-apple-ios13.0", "ios")
    } else if target.contains("apple-darwin") {
        if target.starts_with("x86_64") {
            ("macosx", "x86_64-apple-macosx10.15", "macos")
        } else {
            ("macosx", "arm64-apple-macosx10.15", "macos")
        }
    } else {
        return;
    };

    let sdk_path = {
        let out = Command::new("xcrun")
            .arg("--sdk")
            .arg(sdk)
            .arg("--show-sdk-path")
            .output()
            .expect("failed to run xcrun --show-sdk-path");

        if !out.status.success() {
            panic!(
                "xcrun --show-sdk-path failed for sdk={sdk}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }

        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };

    let profile = env::var("PROFILE").unwrap();
    let out_dir = manifest_dir
        .join("objc-build")
        .join(profile)
        .join("gs26location")
        .join(out_subdir);
    fs::create_dir_all(&out_dir).unwrap();

    let obj = out_dir.join("LocationShim.o");
    let lib = out_dir.join("libgs26location.a");
    let mut cmd = Command::new("xcrun");
    cmd.arg("--sdk")
        .arg(sdk)
        .arg("clang")
        .arg("-target")
        .arg(clang_target)
        .arg("-isysroot")
        .arg(&sdk_path)
        .arg("-fobjc-arc")
        .arg("-c")
        .arg(&src)
        .arg("-o")
        .arg(&obj);
    // compile .m -> .o
    run(cmd);
    let mut cmd = Command::new("xcrun");

    cmd.arg("libtool")
        .arg("-static")
        .arg("-o")
        .arg(&lib)
        .arg(&obj);
    // archive -> .a
    run(cmd);

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=gs26location");

    // frameworks used
    println!("cargo:rustc-link-lib=framework=CoreLocation");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=objc");
}

fn run(mut cmd: Command) {
    let program = cmd.get_program().to_string_lossy().to_string();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();

    let out: Output = cmd.output().unwrap_or_else(|e| {
        panic!("failed to spawn: {program} {args:?}\nerror: {e}");
    });

    if !out.status.success() {
        panic!(
            "command failed: {program} {args:?}\n\
             status: {}\n\
             stdout:\n{}\n\
             stderr:\n{}\n",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

fn main() {
    let target = env::var("TARGET").unwrap();
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    build_apple_objc(&manifest_dir, &target);
    // Re-run if this file changes
    println!("cargo:rerun-if-changed=build.rs");

    // Allow changing Leaflet version via env if you ever want
    println!("cargo:rerun-if-env-changed=LEAFLET_VERSION");

    let version = env::var("LEAFLET_VERSION").unwrap_or_else(|_| "1.9.4".to_string());

    // Path to frontend/dist/vendor/leaflet relative to this crate
    let leaflet_dir = manifest_dir.join("assets").join("vendor").join("leaflet");

    if let Err(e) = fs::create_dir_all(&leaflet_dir) {
        eprintln!("Failed to create Leaflet vendor dir {leaflet_dir:?}: {e}");
        return;
    }

    // Download CSS and JS
    if let Err(e) = download_leaflet_file(&leaflet_dir, &version, "css") {
        eprintln!("Failed to download Leaflet CSS: {e}");
    }
    if let Err(e) = download_leaflet_file(&leaflet_dir, &version, "js") {
        eprintln!("Failed to download Leaflet JS: {e}");
    }
}

fn download_leaflet_file(
    leaflet_dir: &Path,
    version: &str,
    kind: &str, // "css" or "js"
) -> Result<(), Box<dyn std::error::Error>> {
    let filename = format!("leaflet.{kind}");
    let out_path = leaflet_dir.join(&filename);

    // If file already exists, don't redownload every build
    if out_path.exists() {
        return Ok(());
    }

    let url = format!("https://unpkg.com/leaflet@{version}/dist/leaflet.{kind}",);
    println!("Downloading {url} -> {}", out_path.display());

    let resp = reqwest::blocking::get(&url)?;
    if !resp.status().is_success() {
        return Err(format!("HTTP error: {}", resp.status()).into());
    }

    let bytes = resp.bytes()?;
    let mut file = fs::File::create(&out_path)?;
    file.write_all(&bytes)?;
    file.flush()?;

    Ok(())
}
