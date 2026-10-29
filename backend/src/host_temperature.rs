//! Best-effort host CPU temperature. No subprocess, privilege request or radio traffic.
use crate::types::NetworkBoardHealthEntry;
use std::{
    path::Path,
    sync::{
        OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
static STARTED: AtomicBool = AtomicBool::new(false);
static CACHE: OnceLock<RwLock<Option<(i32, Instant)>>> = OnceLock::new();
fn cache() -> &'static RwLock<Option<(i32, Instant)>> {
    CACHE.get_or_init(|| RwLock::new(None))
}
fn milli_to_centi(raw: &str) -> Option<i32> {
    let milli: i32 = raw.trim().parse().ok()?;
    (-40000..=150000).contains(&milli).then_some(milli / 10)
}
fn read_small(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut out = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(128)
        .read_to_string(&mut out)
        .ok()?;
    Some(out)
}
fn thermal_cpu(kind: &str) -> bool {
    matches!(
        kind.trim().to_ascii_lowercase().as_str(),
        "cpu-thermal" | "cpu_thermal" | "soc_thermal" | "x86_pkg_temp"
    )
}
fn read_linux(root: &Path) -> Option<i32> {
    let mut highest = None;
    for n in 0..32 {
        let dir = root.join(format!("class/thermal/thermal_zone{n}"));
        if read_small(&dir.join("type"))
            .as_deref()
            .is_some_and(thermal_cpu)
            && let Some(value) = read_small(&dir.join("temp")).and_then(|s| milli_to_centi(&s))
        {
            highest = Some(highest.map_or(value, |v: i32| v.max(value)));
        }
    }
    for n in 0..32 {
        let dir = root.join(format!("class/hwmon/hwmon{n}"));
        let Some(name) = read_small(&dir.join("name")) else {
            continue;
        };
        if !matches!(
            name.trim(),
            "coretemp" | "k10temp" | "zenpower" | "cpu_thermal"
        ) {
            continue;
        }
        for sensor in 1..=32 {
            if let Some(value) = read_small(&dir.join(format!("temp{sensor}_input")))
                .and_then(|s| milli_to_centi(&s))
            {
                highest = Some(highest.map_or(value, |v: i32| v.max(value)));
            }
        }
    }
    highest
}
fn read_host() -> Option<i32> {
    // macOS has no supported unprivileged CPU-temperature API. Do not invent a
    // reading or silently install/use a privileged SMC helper on RRLS Macs.
    if cfg!(target_os = "linux") {
        read_linux(Path::new("/sys"))
    } else {
        None
    }
}
pub fn start() -> tokio::task::JoinHandle<()> {
    STARTED.store(true, Ordering::Relaxed);
    tokio::spawn(async {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let value = tokio::task::spawn_blocking(read_host).await.ok().flatten();
            if let Ok(mut out) = cache().write() {
                *out = value.map(|v| (v, Instant::now()));
            }
        }
    })
}
pub fn append_local(rows: &mut Vec<NetworkBoardHealthEntry>, wall_ms: u64, simulated: bool) {
    if simulated || !STARTED.load(Ordering::Relaxed) {
        return;
    } // Real host measurements must never label a simulated board.
    let sender = if crate::rrls::is_rrls() { "RRLS" } else { "GS" };
    let sample = cache().read().ok().and_then(|v| *v);
    let age = sample.map_or(0, |(_, t)| {
        t.elapsed().as_millis().min(u64::MAX as u128) as u64
    });
    let value = sample.and_then(|(v, _)| (age <= 15000).then_some(v));
    if let Some(row) = rows.iter_mut().find(|r| r.sender_id == sender) {
        row.cpu_temperature_centi_c = value;
        row.cpu_temperature_age_ms = age;
    } else {
        // Host process presence is separate from physical network observations.
        rows.push(NetworkBoardHealthEntry {
            sender_id: sender.to_owned(),
            healthy: true,
            last_seen_ms: wall_ms,
            cpu_temperature_centi_c: value,
            cpu_temperature_age_ms: age,
            ..Default::default()
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linux_reader_selects_cpu_only_and_handles_missing_sensors() {
        let dir = std::env::temp_dir().join(format!(
            "cpu-temperature-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_linux(&dir), None);
        let sensor = |relative: &str, kind: &str, value: &str| {
            let path = dir.join(relative);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("type"), kind).unwrap();
            std::fs::write(path.join("temp"), value).unwrap();
        };
        sensor("class/thermal/thermal_zone0", "battery", "99000");
        sensor("class/thermal/thermal_zone1", "cpu-thermal", "45321");
        sensor("class/thermal/thermal_zone2", "x86_pkg_temp", "nan");
        assert_eq!(read_linux(&dir), Some(4532));
        let hw = dir.join("class/hwmon/hwmon0");
        std::fs::create_dir_all(&hw).unwrap();
        std::fs::write(hw.join("name"), "coretemp").unwrap();
        std::fs::write(hw.join("temp1_input"), "55123").unwrap();
        std::fs::write(hw.join("temp2_input"), "9999999").unwrap();
        assert_eq!(read_linux(&dir), Some(5512));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn units_and_invalid_samples() {
        assert_eq!(milli_to_centi("45321\n"), Some(4532));
        assert_eq!(milli_to_centi("-1000"), Some(-100));
        for bad in ["nan", "", "150001", "-40001", "999999999999"] {
            assert_eq!(milli_to_centi(bad), None);
        }
        assert!(thermal_cpu("cpu-thermal\n"));
        assert!(!thermal_cpu("acpitz"));
        assert!(!thermal_cpu("battery"));
    }
}
