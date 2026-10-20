//! Native Linux root-service provisioning for DHCP + IPv4LL on wired interfaces.
//! Address acquisition remains owned by the OS; the application binds all addresses.
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod linux {
    use anyhow::{Context, Result, bail};
    use std::{
        path::{Path, PathBuf},
        process::{Command, Output, Stdio},
        time::{Duration, Instant},
    };
    const HEADER: &str = "# Managed by GroundStation Ethernet auto configuration\n";
    fn valid_interface(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
    }
    fn networkmanager_config(name: &str) -> Result<String> {
        if !valid_interface(name) {
            bail!("invalid Ethernet interface name");
        }
        Ok(format!(
            "{HEADER}[connection-groundstation-{name}]\nmatch-device=interface-name:{name}\nipv4.link-local=3\nipv4.dhcp-timeout=2147483647\n"
        ))
    }
    fn networkd_config() -> String {
        format!("{HEADER}[Network]\nLinkLocalAddressing=yes\n[DHCPv4]\nMaxAttempts=infinity\n")
    }
    fn write_managed(path: &Path, text: &str) -> Result<bool> {
        if path
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            bail!("refusing symlink at {}", path.display());
        }
        if let Ok(old) = std::fs::read_to_string(path) {
            if old == text {
                return Ok(false);
            }
            if !old.starts_with(HEADER) {
                bail!("preserving administrator-managed file {}", path.display());
            }
        }
        let parent = path.parent().context("missing configuration directory")?;
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(
            ".groundstation-{}-{}.tmp",
            std::process::id(),
            rand::random::<u64>()
        ));
        let result = (|| -> Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result?;
        Ok(true)
    }
    fn command(binary: &str, args: &[&str]) -> Result<Output> {
        // Absolute OS paths: never execute a user-writable helper as root.
        let mut child = Command::new(binary)
            .args(args)
            .env("LC_ALL", "C")
            .env("SYSTEMD_COLORS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let started = Instant::now();
        while child.try_wait()?.is_none() {
            if started.elapsed() >= Duration::from_secs(4) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{binary} timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(child.wait_with_output()?)
    }
    fn active(service: &str) -> bool {
        command("/usr/bin/systemctl", &["is-active", "--quiet", service])
            .is_ok_and(|v| v.status.success())
    }
    fn physical_interfaces(selected: Option<&str>) -> Result<Vec<String>> {
        if selected.is_some_and(|name| !valid_interface(name)) {
            bail!("invalid selected Ethernet interface");
        }
        let mut names = Vec::new();
        for entry in std::fs::read_dir("/sys/class/net")? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if selected.is_some_and(|wanted| name != wanted) || !valid_interface(&name) {
                continue;
            }
            let base = entry.path();
            if base.join("device").exists()
                && !base.join("wireless").exists()
                && std::fs::read_to_string(base.join("type")).is_ok_and(|v| v.trim() == "1")
            {
                names.push(name);
            }
        }
        names.sort();
        names.truncate(8);
        Ok(names)
    }
    fn networkd_base(status: &str) -> Option<PathBuf> {
        let path = status
            .lines()
            .find_map(|line| line.trim().strip_prefix("Network File:"))?
            .trim();
        let path = PathBuf::from(path);
        (path.is_absolute() && path.extension().is_some_and(|v| v == "network")).then_some(path)
    }
    fn dhcp_enabled(text: &str) -> bool {
        let mut network = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                network = line == "[Network]";
            }
            if network && let Some(value) = line.strip_prefix("DHCP=") {
                return matches!(value.trim(), "yes" | "true" | "ipv4" | "both");
            }
        }
        false
    }
    pub(super) fn prepare(selected: Option<String>) -> Result<()> {
        if std::env::var("GS_ETHERNET_AUTOCONFIG")
            .is_ok_and(|v| matches!(v.as_str(), "off" | "0" | "false"))
            || std::env::var("GS_SIMULATED_SERIAL_PTY").as_deref() == Ok("1")
            || Path::new("/.dockerenv").exists()
            || Path::new("/run/.containerenv").exists()
        {
            return Ok(());
        }
        if unsafe { libc::geteuid() } != 0 {
            log::info!(
                "Ethernet addressing is OS-managed; run as root to provision DHCP + IPv4LL automatically"
            );
            return Ok(());
        }
        let nm = active("NetworkManager.service");
        let networkd = active("systemd-networkd.service");
        for name in physical_interfaces(selected.as_deref())? {
            let managed_nm = nm
                && command(
                    "/usr/bin/nmcli",
                    &["-g", "GENERAL.STATE", "device", "show", &name],
                )
                .is_ok_and(|v| {
                    v.status.success()
                        && String::from_utf8_lossy(&v.stdout).split_whitespace().next()
                            != Some("10")
                });
            if managed_nm {
                let path = PathBuf::from(format!(
                    "/etc/NetworkManager/conf.d/90-groundstation-auto-{name}.conf"
                ));
                {
                    let changed = write_managed(&path, &networkmanager_config(&name)?)?;
                    let output = command("/usr/bin/nmcli", &["general", "reload", "conf"])?;
                    if !output.status.success() {
                        bail!(
                            "NetworkManager could not reload Ethernet defaults: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    // No activation/restart of Wi-Fi, VPN or the whole manager.
                    if changed {
                        let _ = command(
                            "/usr/bin/nmcli",
                            &["--wait", "0", "device", "reapply", &name],
                        );
                    }
                    log::info!(
                        "Ethernet {name}: persistent IPv4LL + ongoing DHCP enabled; existing explicit/static settings retained"
                    );
                }
            } else if networkd {
                let output = command(
                    "/usr/bin/networkctl",
                    &["status", "--no-pager", "--lines=0", &name],
                )?;
                let Some(base) = networkd_base(&String::from_utf8_lossy(&output.stdout)) else {
                    continue;
                };
                if !dhcp_enabled(&std::fs::read_to_string(&base)?) {
                    log::warn!(
                        "Ethernet {name}: preserving non-DHCP networkd profile; configure DHCP in Netplan for automatic addressing"
                    );
                    continue;
                }
                let file = base
                    .file_name()
                    .context("missing networkd profile name")?
                    .to_string_lossy();
                let path = PathBuf::from(format!(
                    "/etc/systemd/network/{file}.d/90-groundstation-auto.conf"
                ));
                {
                    let changed = write_managed(&path, &networkd_config())?;
                    let output = command("/usr/bin/networkctl", &["reload"])?;
                    if !output.status.success() {
                        bail!("networkd reload failed");
                    }
                    if changed {
                        let _ = command("/usr/bin/networkctl", &["reconfigure", &name]);
                    }
                    log::info!(
                        "Ethernet {name}: persistent IPv4LL + ongoing DHCP enabled through networkd"
                    );
                }
            }
        }
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn networkmanager_defaults_are_scoped_and_keep_dhcp_running() {
            let text = networkmanager_config("eth0").unwrap();
            assert!(text.contains("match-device=interface-name:eth0\n"));
            assert!(text.contains("ipv4.link-local=3\n"));
            assert!(text.contains("ipv4.dhcp-timeout=2147483647\n"));
            assert!(!text.contains("ipv4.method="));
            assert!(!text.contains("gateway="));
            assert!(networkmanager_config("eth0\n[other]").is_err());
        }
        #[test]
        fn networkd_targets_only_an_existing_dhcp_profile() {
            assert!(dhcp_enabled("[Match]\nName=eth0\n[Network]\nDHCP=ipv4\n"));
            assert!(!dhcp_enabled("[Network]\nAddress=192.168.4.1/24\n"));
            assert!(
                networkd_base("  Network File: /run/systemd/network/10-netplan-eth0.network")
                    .is_some()
            );
            assert!(networkd_base("  Network File: n/a").is_none());
            assert!(!networkd_config().contains("Address="));
        }
        #[test]
        fn installation_is_idempotent_and_preserves_admin_files_and_symlinks() {
            use std::os::unix::fs::symlink;
            let dir = std::env::temp_dir().join(format!("gs-ethernet-{}", rand::random::<u64>()));
            let file = dir.join("config.conf");
            let text = networkmanager_config("eth0").unwrap();
            assert!(write_managed(&file, &text).unwrap());
            assert!(!write_managed(&file, &text).unwrap());
            std::fs::write(&file, "administrator settings").unwrap();
            assert!(write_managed(&file, &text).is_err());
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                "administrator settings"
            );
            let link = dir.join("symlink.conf");
            symlink(&file, &link).unwrap();
            assert!(write_managed(&link, &text).is_err());
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

pub async fn prepare(selected: Option<String>) {
    #[cfg(target_os = "linux")]
    match tokio::task::spawn_blocking(move || linux::prepare(selected)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::warn!(
            "Ethernet automatic setup unavailable: {error:#}. Existing network configuration retained."
        ),
        Err(error) => log::warn!("Ethernet setup task failed: {error}"),
    }
    #[cfg(not(target_os = "linux"))]
    let _ = selected;
}
