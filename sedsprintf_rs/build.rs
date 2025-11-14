// build.rs
use regex::Regex;
use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};


fn main() {
    ensure_rust_target_installed();
    generate_c_header();
    generate_pyi_stub();
}

/// Ensure the current Cargo target triple has its std/core installed via rustup.
///
/// Reads the `TARGET` env var (set by Cargo), checks `rustup target list --installed`,
/// and if the target is missing, runs `rustup target add <target>`.
fn ensure_rust_target_installed() {
    let target = match env::var("TARGET") {
        Ok(t) => t,
        Err(_) => {
            // No target info, nothing to do.
            return;
        }
    };

    // Best-effort: if rustup isn't available, bail with a clear message.
    let list_output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .expect(
            "Failed to run `rustup target list --installed` – is rustup installed and on PATH?",
        );

    if !list_output.status.success() {
        panic!(
            "Failed to list installed rustup targets:\n{}",
            String::from_utf8_lossy(&list_output.stderr)
        );
    }

    let installed = String::from_utf8_lossy(&list_output.stdout);
    if installed.lines().any(|line| line.trim() == target) {
        return;
    }

    let status = Command::new("rustup")
        .args(["target", "add", &target])
        .status()
        .expect("Failed to run `rustup target add` – is rustup installed and on PATH?");

    if !status.success() {
        panic!(
            "`rustup target add {}` failed with status: {}",
            target, status
        );
    }
}

// ========================= C HEADER =========================

fn generate_c_header() {
    // 1) Run cbindgen to a visible temp file
    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = PathBuf::from(&crate_dir)
        .join("target")
        .join("cbindgen_out");
    fs::create_dir_all(&out_dir).expect("create cbindgen_out");
    let enums_tmp = out_dir.join("enums_raw.h");

    cbindgen::Builder::new()
        .with_config(cbindgen::Config::from_root_or_default(&crate_dir))
        .with_crate(&crate_dir)
        .generate()
        .expect("cbindgen failed")
        .write_to_file(&enums_tmp);

    // 2) Load the generated header
    let raw = fs::read_to_string(&enums_tmp).expect("read enums_raw.h");
    if raw.trim().is_empty() {
        panic!("cbindgen produced empty output at {}", enums_tmp.display());
    }

    // 3) Extract & transform the three enums
    let dt = extract_enum(&raw, "DataType")
        .map(|b| transform_enum_block(&b, "DataType", "SedsDataType", "SEDS_DT_"))
        .unwrap_or_else(|| {
            dump_excerpt(&raw, "DataType");
            panic!("DataType not found in cbindgen output");
        });

    let ep = extract_enum(&raw, "DataEndpoint")
        .map(|b| transform_enum_block(&b, "DataEndpoint", "SedsDataEndpoint", "SEDS_EP_"))
        .unwrap_or_else(|| {
            dump_excerpt(&raw, "DataEndpoint");
            panic!("DataEndpoint not found in cbindgen output");
        });

    let er = extract_enum(&raw, "TelemetryErrorCode")
        .map(|b| transform_errors_enum_as_seds_result(&b))
        .unwrap_or_else(|| {
            dump_excerpt(&raw, "TelemetryErrorCode");
            panic!("TelemetryErrorCode not found in cbindgen output");
        });

    let enums_joined = format!("{dt}\n\n{ep}\n\n{er}\n");

    // 4) Load template, replace marker, write final header
    let tpl_path = PathBuf::from(&crate_dir).join("header_templates/sedsprintf.h.txt");
    let tpl = fs::read_to_string(&tpl_path)
        .unwrap_or_else(|e| panic!("read template header {}: {e}", tpl_path.display()));

    let marker = "/* {{AUTOGEN:ENUMS}} */";
    if !tpl.contains(marker) {
        panic!(
            "Template {} is missing marker: {}",
            tpl_path.display(),
            marker
        );
    }

    let final_text = tpl.replace(marker, &enums_joined);
    let final_out = PathBuf::from(&crate_dir).join("C-Headers/sedsprintf.h");
    fs::create_dir_all(final_out.parent().unwrap()).expect("create C-Headers/ failed");
    fs::write(&final_out, final_text)
        .unwrap_or_else(|e| panic!("write final header {}: {e}", final_out.display()));
}

// ========================= PYI STUB =========================

fn generate_pyi_stub() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = PathBuf::from(&crate_dir)
        .join("target")
        .join("cbindgen_out");
    let enums_tmp = out_dir.join("enums_raw.h");

    // Ensure cbindgen was already run by generate_c_header()
    if !enums_tmp.exists() {
        panic!(
            "Missing {}. Ensure generate_c_header runs before generate_pyi_stub.",
            enums_tmp.display()
        );
    }
    let raw = fs::read_to_string(&enums_tmp).expect("read enums_raw.h");

    // Extract the two enums from raw cbindgen output (unprefixed names)
    let dt_block = extract_enum(&raw, "DataType").unwrap_or_else(|| {
        dump_excerpt(&raw, "DataType");
        panic!("DataType not found for .pyi");
    });
    let ep_block = extract_enum(&raw, "DataEndpoint").unwrap_or_else(|| {
        dump_excerpt(&raw, "DataEndpoint");
        panic!("DataEndpoint not found for .pyi");
    });

    let dt_members = parse_enum_members(&dt_block); // Vec<(NAME, VALUE_TEXT)>
    let ep_members = parse_enum_members(&ep_block);

    let dt_enum_text = render_python_intenum(
        "DataType",
        r#"Wire-level type tags (generated to match Rust/config.rs)."#,
        &dt_members,
    );

    let ep_enum_text = render_python_intenum(
        "DataEndpoint",
        r#"Routing endpoints for packets."#,
        &ep_members,
    );

    let joined = format!("{dt_enum_text}\n\n{ep_enum_text}\n");

    // Load .pyi template and inject
    let tpl_path = PathBuf::from(&crate_dir).join("header_templates/sedsprintf_rs.pyi.txt");
    let tpl = fs::read_to_string(&tpl_path)
        .unwrap_or_else(|e| panic!("read pyi template {}: {e}", tpl_path.display()));
    let marker = "/* {{AUTOGEN:PY_ENUMS}} */";
    if !tpl.contains(marker) {
        panic!(
            "pyi template {} missing marker {}",
            tpl_path.display(),
            marker
        );
    }

    let final_text = tpl.replace(marker, &joined);

    // Write to an importable location inside the crate workspace
    let out_pyi = PathBuf::from(&crate_dir)
        .join("python-files")
        .join("sedsprintf_rs.pyi");
    fs::create_dir_all(out_pyi.parent().unwrap()).expect("create python_bindings/");
    fs::write(&out_pyi, final_text).unwrap_or_else(|e| panic!("write {}: {e}", out_pyi.display()));
}

// ========================= Helpers (shared) =========================

/// Extract `typedef enum [Name]? { ... } Name;` OR `enum Name { ... };`.
/// If only the plain form exists, rebuild a typedef-style block.
fn extract_enum(input: &str, name: &str) -> Option<String> {
    let pat_typedef = format!(r"(?s)typedef\s+enum(?:\s+{name})?\s*\{{(.*?)\}}\s+{name}\s*;");
    let re_typedef = Regex::new(&pat_typedef).ok()?;
    if let Some(caps) = re_typedef.captures(input) {
        let full = caps.get(0)?.as_str().to_string();
        return Some(full);
    }

    let pat_plain = format!(r"(?s)\benum\s+{name}\s*\{{(.*?)\}}\s*;");
    let re_plain = Regex::new(&pat_plain).ok()?;
    if let Some(caps) = re_plain.captures(input) {
        let body = caps.get(1)?.as_str();
        let rebuilt = format!("typedef enum {name} {{\n{body}\n}} {name};");
        return Some(rebuilt);
    }

    None
}

/// Generic transformer used for DataType/DataEndpoint:
fn transform_enum_block(
    block: &str,
    old_type: &str,
    new_type: &str,
    variant_prefix: &str,
) -> String {
    let mut s = block.replace(&format!("}} {};", old_type), &format!("}} {};", new_type));
    if let (Some(l), Some(r)) = (s.find('{'), s.rfind('}')) {
        let body = &s[(l + 1)..r];
        let new_body = body
            .lines()
            .map(|line| {
                if let Some((lead, rest)) = split_leading_ws(line) {
                    if let Some(id) = rest
                        .split(|c: char| c == ' ' || c == '=' || c == ',')
                        .next()
                    {
                        if !id.is_empty() && id.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                        {
                            if rest.trim_start().starts_with(variant_prefix) {
                                return line.to_string();
                            }
                            return format!("{lead}{}{}", variant_prefix, rest);
                        }
                    }
                }
                line.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        s = format!("{}{{\n{}\n}} {};", &s[..l], new_body, new_type);
    }
    s
}

/// Specialized transformer for TelemetryErrorCode -> SedsResult
fn transform_errors_enum_as_seds_result(block: &str) -> String {
    let (l, r) = match (block.find('{'), block.rfind('}')) {
        (Some(l), Some(r)) => (l, r),
        _ => {
            return String::from(
                "typedef enum SedsResult { SEDS_OK = 0, SEDS_ERR = -1, } SedsResult;",
            );
        }
    };
    let body = &block[(l + 1)..r];

    let mut lines = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((lead, rest)) = split_leading_ws(line) {
            if let Some(id) = rest
                .split(|c: char| c == ' ' || c == '=' || c == ',')
                .next()
            {
                if !id.is_empty()
                    && id
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
                {
                    let prefixed = if rest.trim_start().starts_with("SEDS_") {
                        rest.to_string()
                    } else {
                        format!("SEDS_{}", rest)
                    };
                    lines.push(format!("{lead}{prefixed}"));
                    continue;
                }
            }
        }
        lines.push(line.to_string());
    }

    let mut injected = Vec::new();
    if !lines.iter().any(|l| l.contains("SEDS_OK")) {
        injected.push(String::from("  SEDS_OK = 0,"));
    }
    if !lines.iter().any(|l| l.contains("SEDS_ERR")) {
        injected.push(String::from("  SEDS_ERR = -1,"));
    }
    if !injected.is_empty() {
        injected.push(String::from(""));
    }
    injected.extend(lines);

    format!(
        "typedef enum SedsResult {{\n\n{}\n\n}} SedsResult;",
        injected.join("\n")
    )
}

/// Parse enum body into (NAME, VALUE_TEXT) pairs. Keeps values verbatim (e.g., "= 5", " = -2").
fn parse_enum_members(block: &str) -> Vec<(String, String)> {
    let (l, r) = match (block.find('{'), block.rfind('}')) {
        (Some(l), Some(r)) => (l, r),
        _ => return Vec::new(),
    };
    let body = &block[(l + 1)..r];
    let mut out = Vec::new();

    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("/*") || t.starts_with("//") {
            continue;
        }

        // Grab "IDENT [= VALUE] ," at start of line
        if let Some((_, rest)) = split_leading_ws(line) {
            let ident = rest
                .split(|c: char| c == ' ' || c == '=' || c == ',')
                .next()
                .unwrap_or("")
                .trim();
            if ident.is_empty() || !ident.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                continue;
            }

            // Snip out the "= value" if present
            let after_ident = &rest[rest.find(ident).unwrap() + ident.len()..];
            let value = if let Some(eq_pos) = after_ident.find('=') {
                // "= ...", stop at comma if any
                let after_eq = after_ident[eq_pos..].trim(); // starts with '='
                let val_only = if let Some(cpos) = after_eq.find(',') {
                    &after_eq[..cpos]
                } else {
                    after_eq
                };
                val_only.trim().to_string() // e.g., "= 3"
            } else {
                String::new() // no explicit value
            };

            out.push((ident.to_string(), value));
        }
    }

    out
}

fn render_python_intenum(name: &str, doc: &str, members: &[(String, String)]) -> String {
    // Build a quick lookup for per-member comment text
    let per_member_doc = std::collections::HashMap::<&str, &str>::new();

    let mut lines = Vec::new();
    lines.push(format!("class {name}(IntEnum):"));
    lines.push(format!("    \"\"\"{doc}\"\"\""));

    for (ident, valtxt) in members {
        let rhs = if valtxt.is_empty() {
            String::from("...  # (value assigned at runtime)")
        } else {
            valtxt.trim_start_matches('=').trim().to_string()
        };
        let comment = per_member_doc
            .get(ident.as_str())
            .map(|c| format!("  #: {}", c))
            .unwrap_or_default();
        lines.push(format!("    {ident}: int = {rhs}{comment}"));
    }

    lines.join("\n")
}

fn split_leading_ws(s: &str) -> Option<(&str, &str)> {
    let n = s.chars().take_while(|c| c.is_whitespace()).count();
    Some(s.split_at(n))
}

fn dump_excerpt(raw: &str, want: &str) {
    if let Some(i) = raw.find(want) {
        let start = i.saturating_sub(200);
        let end = (i + 200).min(raw.len());
        let excerpt = &raw[start..end];
        eprintln!("cargo:warning=Excerpt around `{}`:\n{}", want, excerpt);
    } else {
        eprintln!(
            "cargo:warning=`{}` not found anywhere in cbindgen output",
            want
        );
    }
}
