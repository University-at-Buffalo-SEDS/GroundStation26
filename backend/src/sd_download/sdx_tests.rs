use super::*;
use crate::sd_download::test_data::{Noise, fc_launch_csv};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/test-fixtures/sdx");

/// Blocks of the C driver output (FlightComputer26
/// `tests/test_sd_xfer_codec.py` DRIVER): `[u32 start][u32 raw_len][u32
/// nmsgs]` then per message `[u16 len][bytes]`.
fn c_blocks(raw: &[u8]) -> Vec<(usize, usize, Vec<Vec<u8>>)> {
    let u32_at = |p: usize| u32::from_le_bytes(raw[p..p + 4].try_into().unwrap()) as usize;
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < raw.len() {
        let (start, len, n) = (u32_at(pos), u32_at(pos + 4), u32_at(pos + 8));
        pos += 12;
        let mut msgs = Vec::new();
        for _ in 0..n {
            let l = u16::from_le_bytes([raw[pos], raw[pos + 1]]) as usize;
            msgs.push(raw[pos + 2..pos + 2 + l].to_vec());
            pos += 2 + l;
        }
        out.push((start, len, msgs));
    }
    out
}

fn check_c_vectors(data: &[u8], raw: &[u8], cap: usize) -> (usize, usize) {
    let blocks = c_blocks(raw);
    let mut at = 0;
    let mut body_bytes = 0;
    for (start, len, msgs) in blocks.iter().rev() {
        // Any order, each block alone.
        assert!(msgs.iter().all(|m| !m.is_empty() && m.len() <= cap));
        let body: Vec<u8> = msgs.concat();
        body_bytes += body.len();
        assert_eq!(decode_block(&body, *len).unwrap(), data[*start..start + len], "block at {start}");
        assert!(decode_block(&body, len - 1).is_err(), "max_out bounds the output");
        at += len;
    }
    assert_eq!(at, data.len());
    (data.len(), body_bytes)
}

#[test]
fn the_decoder_reproduces_every_c_encoded_block_exactly() {
    let mut seen = 0;
    for entry in std::fs::read_dir(FIXTURES).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".bin") else { continue };
        let parts: Vec<&str> = stem.split('.').collect();
        let data = std::fs::read(format!("{FIXTURES}/{}.csv", parts[0])).unwrap();
        let cap: usize = parts[2].parse().unwrap();
        let (raw, body) = check_c_vectors(&data, &std::fs::read(entry.path()).unwrap(), cap);
        println!("C vectors {name}: {raw} B raw -> {body} B SDX ({:.2}x)", raw as f64 / body as f64);
        if parts[0] == "fc" || parts[0] == "daq" {
            assert!(body * 3 < raw, "{name}: SDX at least 3x on rows");
        }
        seen += 1;
    }
    assert_eq!(seen, 6, "fc, daq, noisy and three edge-case cuts");
}

fn round_trip(data: &[u8], target: usize) -> usize {
    let mut at = 0;
    let mut body_bytes = 0;
    while at < data.len() {
        let (used, body) = Encoder::encode_block(&data[at..], target);
        assert!(used > 0);
        let decoded = decode_block(&body, used).unwrap_or_else(|e| panic!("block at {at}: {e}"));
        assert!(decoded == data[at..at + used], "block at {at} (target {target})");
        body_bytes += body.len();
        at += used;
    }
    body_bytes
}

#[test]
fn the_rust_encoder_round_trips_fixtures_and_shrinks_rows() {
    for name in ["fc", "daq", "edge", "noisy"] {
        let data = std::fs::read(format!("{FIXTURES}/{name}.csv")).unwrap();
        for target in [1, 64, 700, 4096, 16_384, 1 << 20] {
            round_trip(&data, target);
        }
    }
    let fc = fc_launch_csv(20_000, 3);
    let size = round_trip(&fc, 16_384);
    println!("Rust encoder, synthetic FC launch file: {} B -> {size} B ({:.2}x)", fc.len(), fc.len() as f64 / size as f64);
    assert!(size * 4 < fc.len());
}

#[test]
fn random_bytes_and_noisy_cuts_round_trip() {
    let mut rnd = Noise::new(42);
    let base = [std::fs::read(format!("{FIXTURES}/fc.csv")).unwrap(), std::fs::read(format!("{FIXTURES}/daq.csv")).unwrap()].concat();
    let alphabet = b",\"\r\n -.eE+0123456789xX";
    for _ in 0..200 {
        let cut = rnd.below(base.len() as u64 - 3000) as usize;
        let mut noisy = base[cut..cut + 1 + rnd.below(2999) as usize].to_vec();
        for _ in 0..rnd.below(40) {
            let at = rnd.below(noisy.len() as u64) as usize;
            noisy[at] = alphabet[rnd.below(alphabet.len() as u64) as usize];
        }
        round_trip(&noisy, [1, 300, 4096][rnd.below(3) as usize]);
        // Pure noise from the CSV alphabet and from all bytes.
        let junk: Vec<u8> = (0..rnd.below(800)).map(|_| alphabet[rnd.below(alphabet.len() as u64) as usize]).collect();
        round_trip(&junk, 512);
        let bytes: Vec<u8> = (0..rnd.below(800)).map(|_| rnd.next_u64() as u8).collect();
        round_trip(&bytes, 512);
    }
}

#[test]
fn malformed_input_is_an_error_never_a_panic_or_unbounded() {
    let mut rnd = Noise::new(7);
    let data = std::fs::read(format!("{FIXTURES}/fc.csv")).unwrap();
    let (used, body) = Encoder::encode_block(&data, 4096);
    for _ in 0..3000 {
        let mut bad = body.clone();
        match rnd.below(3) {
            0 => bad.truncate(rnd.below(bad.len() as u64) as usize),
            1 => {
                for _ in 0..1 + rnd.below(4) {
                    let at = rnd.below(bad.len() as u64) as usize;
                    bad[at] = rnd.next_u64() as u8;
                }
            }
            _ => bad = (0..rnd.below(400)).map(|_| rnd.next_u64() as u8).collect(),
        }
        if let Ok(out) = decode_block(&bad, used) {
            assert!(out.len() <= used);
        }
    }
    // Specific malformations.
    for (bad, why) in [
        (vec![0x00, 0x05, b'a'], "truncated RAW"),
        (vec![0x02, 0x01, 0x00], "row before its label"),
        (vec![0x01, 0x01, b'x', 0x03], "op list reuse without a list"),
        (vec![0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f], "varint overflow"),
        (vec![0x01, 0x01, b'x', 0x02, 12, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11], "SAME without context"),
        (vec![0x01, 0x01, b'x', 0x02, 12, 0x66, 0, 0, 0, 0, 0], "COPY of a later field"),
        (vec![0x01, 0x01, b'x', 0x02, 12, 0x22, 0, 0, 0, 0, 0], "DELTA without a number"),
        (vec![0x01, 0x01, b'x', 0x02, 12, 0x33, 0, 0, 0, 0, 0, 0x02, 0x01, 0x02], "NUM with a bad shape"),
    ] {
        assert!(decode_block(&bad, 1 << 16).is_err(), "{why}");
    }
    // A long token repeated by SAME cannot exceed max_out.
    let mut amp = vec![0x01, 0x01, b'x', 0x02, 12];
    amp.extend_from_slice(&[0x04, 0, 0, 0, 0, 0]);
    amp.extend_from_slice(&[0x80, 0x40]);
    amp.extend(std::iter::repeat_n(b'z', 8192));
    amp.extend_from_slice(&[0x02, 12, 0x01, 0, 0, 0, 0, 0]);
    amp.extend(std::iter::repeat_n(0x03, 1000));
    assert!(decode_block(&amp, 100_000).is_err(), "bounded by max_out");
}

#[test]
fn numbers_rebuild_exactly_and_deltas_cross_exponents() {
    for t in ["0", "-1", "12.50", "1.00000000e+00", "-6.60799682e-01", "9.99999999E+999", "1e5", "123456789012345.123", "007", "1.5e+100"] {
        let n = parse_num(t.as_bytes()).unwrap_or_else(|| panic!("{t}"));
        let mut back = Vec::new();
        fmt_num(&n, &mut back).unwrap();
        assert_eq!(back, t.as_bytes());
    }
    for t in ["-0", "1e-0", "+3", ".5", "5.", "1.5e+1000", "1234567890123456", "99999999999999999999", "1.2.3", "", "-", "nan", "1e"] {
        assert!(parse_num(t.as_bytes()).is_none(), "{t}");
    }
    let pairs = [("1.50000000e+00", "2.50000000e+02"), ("2.5e-01", "1.5e-03"), ("9.53849983e+00", "1.00000000e+01"), ("1.0e+03", "9.9e+00"), ("-5", "7"), ("1.6e+03", "2.5e-04")];
    for (a, b) in pairs {
        let (pa, pb) = (parse_num(a.as_bytes()).unwrap(), parse_num(b.as_bytes()).unwrap());
        if let Some(u) = delta(&pa, &pb) {
            assert_eq!(apply_delta(&pa, u).unwrap(), pb, "{a} -> {b}");
        } else {
            assert!((pb.exp - pa.exp).abs() > 3 || pa.b0 != pb.b0 || pa.b1 != pb.b1, "{a} -> {b} must be a delta");
        }
    }
}

/// The C encoder on the realistic launch files (`tools/gen_sd.py` output in
/// /tmp/sdft-data), next to the Rust encoder: exact round trip and ratios.
/// Needs `cc` and the FlightComputer26 checkout beside GroundStation26
/// (`SDX_C_SOURCE` overrides); run with `--ignored`.
#[test]
#[ignore]
fn c_and_rust_encoders_on_realistic_launch_files() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../FlightComputer26");
    let source = std::env::var("SDX_C_SOURCE").map(std::path::PathBuf::from).unwrap_or_else(|_| root.join("Core/Src/sd_xfer_codec.c"));
    let include = source.parent().unwrap().join("../Inc");
    let driver_py = std::fs::read_to_string(root.join("tests/test_sd_xfer_codec.py")).unwrap();
    let driver = driver_py.split("DRIVER = r'''").nth(1).unwrap().split("'''").next().unwrap();
    let tmp = std::env::temp_dir().join(format!("sdx-c-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("drv.c"), driver).unwrap();
    let exe = tmp.join("drv");
    let status = std::process::Command::new("cc")
        .args(["-std=gnu11", "-O2", "-I"])
        .arg(&include)
        .arg(tmp.join("drv.c"))
        .arg(&source)
        .arg("-o")
        .arg(&exe)
        .status()
        .unwrap();
    assert!(status.success());
    for (name, cap) in [("fc_launch.csv", 460 - 17), ("daq_launch.csv", 1024 - 17)] {
        let path = std::path::Path::new("/tmp/sdft-data").join(name);
        let Ok(data) = std::fs::read(&path) else { continue };
        let out = tmp.join("out.bin");
        let ok = std::process::Command::new(&exe).arg(&path).args(["16384", &cap.to_string(), "1024"]).arg(&out).status().unwrap();
        assert!(ok.success());
        let (raw, c_body) = check_c_vectors(&data, &std::fs::read(&out).unwrap(), cap);
        let started = std::time::Instant::now();
        let rust_body = round_trip(&data, 16_384);
        let rust_ms = started.elapsed().as_millis();
        let mut zstd_body = 0;
        let mut at = 0;
        while at < data.len() {
            let (used, body) = Encoder::encode_block(&data[at..], 16_384);
            zstd_body += zstd::bulk::compress(&body, 5).unwrap().len();
            at += used;
        }
        println!(
            "{name}: {raw} B; C SDX {c_body} B ({:.2}x); Rust SDX {rust_body} B ({:.2}x, encode+check {rust_ms} ms); Rust SDX+zstd {zstd_body} B ({:.2}x)",
            raw as f64 / c_body as f64,
            raw as f64 / rust_body as f64,
            raw as f64 / zstd_body as f64
        );
    }
    std::fs::remove_dir_all(&tmp).unwrap();
}
