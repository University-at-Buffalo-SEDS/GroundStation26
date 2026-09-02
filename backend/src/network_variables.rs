use crate::telemetry_task::get_current_timestamp_ms;
use crate::types::Board;
use anyhow::{Context, Result};
use sedsnet::packet::Packet;
use sedsnet::router::{NetworkVariablePermissions, Router};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const DEFAULT_CACHE_PATH: &str = "backend/data/network_variables.json";
const UNDERGLOW_TYPE: &str = "AV_BAY_UNDERGLOW";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct PersistentVariables {
    #[serde(default)]
    av_bay_underglow: bool,
}

struct VariableStore {
    path: PathBuf,
    values: PersistentVariables,
}

static STORE: OnceLock<Mutex<VariableStore>> = OnceLock::new();

fn cache_path() -> PathBuf {
    std::env::var_os("GS_NETWORK_VARIABLE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CACHE_PATH))
}

fn load(path: &Path) -> PersistentVariables {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| PersistentVariables {
            av_bay_underglow: std::env::var("GS_AV_BAY_UNDERGLOW_DEFAULT")
                .ok()
                .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "on")),
        })
}

fn store() -> &'static Mutex<VariableStore> {
    STORE.get_or_init(|| {
        let path = cache_path();
        Mutex::new(VariableStore {
            values: load(&path),
            path,
        })
    })
}

fn persist(path: &Path, values: PersistentVariables) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "create network-variable cache directory {}",
                parent.display()
            )
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&values)?)
        .with_context(|| format!("write network-variable cache {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("replace network-variable cache {}", path.display()))?;
    Ok(())
}

fn packet(enabled: bool) -> Result<Packet> {
    Ok(Packet::new(
        crate::telemetry_schema::data_type(UNDERGLOW_TYPE),
        &[crate::telemetry_schema::endpoint("HEART_BEAT")],
        Board::GroundStation.sender_id(),
        get_current_timestamp_ms(),
        Arc::from([u8::from(enabled)]),
    )?)
}

pub fn initialize(router: &Router) -> Result<()> {
    let ty = crate::telemetry_schema::data_type(UNDERGLOW_TYPE);
    router.enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)?;
    {
        let guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        if !guard.path.exists() {
            persist(&guard.path, guard.values)?;
        }
    }
    router.seed_managed_variable(packet(underglow_enabled())?)?;
    Ok(())
}

pub fn publish_current(router: &Router) -> Result<()> {
    router.set_network_variable(packet(underglow_enabled())?)?;
    Ok(())
}

pub fn toggle_underglow(router: &Router) -> Result<bool> {
    let enabled = {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        toggle_persisted(&mut guard)?
    };
    router.set_network_variable(packet(enabled)?)?;
    Ok(enabled)
}

fn toggle_persisted(store: &mut VariableStore) -> Result<bool> {
    store.values.av_bay_underglow = !store.values.av_bay_underglow;
    persist(&store.path, store.values)?;
    Ok(store.values.av_bay_underglow)
}

pub fn underglow_enabled() -> bool {
    store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values
        .av_bay_underglow
}

#[cfg(test)]
mod tests {
    use super::*;
    use sedsnet::router::RouterConfig;

    #[test]
    fn missing_cache_defaults_to_off() {
        let path = std::env::temp_dir().join(format!(
            "gs26-missing-network-vars-{}.json",
            std::process::id()
        ));
        assert!(!load(&path).av_bay_underglow);
    }

    #[test]
    fn persisted_cache_round_trips() {
        let path =
            std::env::temp_dir().join(format!("gs26-network-vars-{}.json", std::process::id()));
        persist(
            &path,
            PersistentVariables {
                av_bay_underglow: true,
            },
        )
        .unwrap();
        assert!(load(&path).av_bay_underglow);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn persisted_toggle_survives_two_process_style_reloads() {
        let path = std::env::temp_dir().join(format!(
            "gs26-network-vars-toggle-{}.json",
            std::process::id()
        ));
        persist(
            &path,
            PersistentVariables {
                av_bay_underglow: true,
            },
        )
        .unwrap();

        let mut first_restart = VariableStore {
            path: path.clone(),
            values: load(&path),
        };
        assert!(!toggle_persisted(&mut first_restart).unwrap());
        assert!(!load(&path).av_bay_underglow);

        let mut second_restart = VariableStore {
            path: path.clone(),
            values: load(&path),
        };
        assert!(toggle_persisted(&mut second_restart).unwrap());
        assert!(load(&path).av_bay_underglow);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn network_variable_reaches_a_read_only_avionics_peer() {
        crate::telemetry_schema::initialize().unwrap();
        let source = Arc::new(Router::new(
            RouterConfig::new([]).with_sender(Board::GroundStation.sender_id()),
        ));
        let peer = Arc::new(Router::new(RouterConfig::new([]).with_sender("RF")));
        let ty = crate::telemetry_schema::data_type(UNDERGLOW_TYPE);
        source
            .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
            .unwrap();
        peer.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
            .unwrap();

        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_callback = observed.clone();
        peer.on_network_variable_update(ty, move |packet| {
            observed_callback
                .lock()
                .unwrap()
                .push(packet.payload().to_vec());
            Ok(())
        })
        .unwrap();

        let peer_rx = peer.clone();
        source.add_side_packet("to-rf", move |packet| peer_rx.rx_from_side(packet, 0));
        let source_rx = source.clone();
        peer.add_side_packet("to-gs", move |packet| source_rx.rx_from_side(packet, 0));

        source.set_network_variable(packet(true).unwrap()).unwrap();
        source.process_all_queues().unwrap();
        peer.process_all_queues().unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![vec![1]]);
    }
}
