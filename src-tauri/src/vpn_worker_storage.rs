use crate::proxy_storage::{get_storage_dir, is_process_running};
use crate::vpn::tcp_shape::OsStack;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpnWorkerConfig {
  pub id: String,
  pub vpn_id: String,
  pub vpn_type: String,
  pub config_file_path: String,
  pub local_port: Option<u16>,
  pub local_url: Option<String>,
  pub pid: Option<u32>,
  /// Pins `pid` to one exact process, so a recycled PID cannot make a dead
  /// worker look alive or get an unrelated process signalled. Defaulted
  /// because configs written before this field existed must still deserialize.
  #[serde(default)]
  pub pid_start_time: Option<u64>,
  /// The OS (`windows`, `macos` or `linux`) whose TCP/IP stack this worker's
  /// packets imitate: the one the profile using it claims. One WireGuard
  /// config gets one worker per OS. Defaulted to this machine's OS, which is
  /// what workers written before this field existed imitated.
  #[serde(default = "host_os_name")]
  pub os: String,
}

fn host_os_name() -> String {
  OsStack::host().as_str().to_string()
}

impl VpnWorkerConfig {
  pub fn new(
    id: String,
    vpn_id: String,
    vpn_type: String,
    config_file_path: String,
    os: OsStack,
  ) -> Self {
    Self {
      id,
      vpn_id,
      vpn_type,
      config_file_path,
      local_port: None,
      local_url: None,
      pid: None,
      pid_start_time: None,
      os: os.as_str().to_string(),
    }
  }

  /// The stack this worker imitates; an unreadable name reads as this machine's.
  pub fn stack_os(&self) -> OsStack {
    OsStack::parse(&self.os).unwrap_or_else(OsStack::host)
  }

  /// Whether this is the worker for `vpn_id` as seen from `os`. Workers are
  /// shared per (VPN, OS): two profiles on one WireGuard config share a tunnel
  /// only when they claim the same OS.
  pub fn serves(&self, vpn_id: &str, os: OsStack) -> bool {
    self.vpn_id == vpn_id && self.stack_os() == os
  }
}

pub fn vpn_worker_config_path(id: &str) -> std::path::PathBuf {
  get_storage_dir().join(format!("vpn_worker_{}.json", id))
}

pub fn save_vpn_worker_config(config: &VpnWorkerConfig) -> Result<(), Box<dyn std::error::Error>> {
  let storage_dir = get_storage_dir();
  fs::create_dir_all(&storage_dir)?;

  let file_path = storage_dir.join(format!("vpn_worker_{}.json", config.id));
  save_vpn_worker_config_to_path(config, &file_path)
}

/// Write a worker config to a specific path. Used by detached worker
/// processes that already know their config file path (passed via
/// `--config-path`) and must write back to the same location regardless of
/// how `get_storage_dir()` resolves in the worker process — which can
/// differ from the parent on Linux distros that sandbox the GUI (Qubes,
/// flatpak, etc.).
pub fn save_vpn_worker_config_to_path(
  config: &VpnWorkerConfig,
  path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)?;
  }
  let content = serde_json::to_string_pretty(config)?;
  fs::write(path, content)?;
  Ok(())
}

/// Read a worker config from a specific path. Counterpart to
/// `save_vpn_worker_config_to_path`.
pub fn get_vpn_worker_config_from_path(path: &Path) -> Option<VpnWorkerConfig> {
  let content = fs::read_to_string(path).ok()?;
  serde_json::from_str(&content).ok()
}

pub fn get_vpn_worker_config(id: &str) -> Option<VpnWorkerConfig> {
  let storage_dir = get_storage_dir();
  let file_path = storage_dir.join(format!("vpn_worker_{}.json", id));

  if !file_path.exists() {
    return None;
  }

  match fs::read_to_string(&file_path) {
    Ok(content) => serde_json::from_str(&content).ok(),
    Err(_) => None,
  }
}

pub fn delete_vpn_worker_config(id: &str) -> bool {
  let storage_dir = get_storage_dir();
  let file_path = storage_dir.join(format!("vpn_worker_{}.json", id));

  if !file_path.exists() {
    return false;
  }

  fs::remove_file(&file_path).is_ok()
}

pub fn list_vpn_worker_configs() -> Vec<VpnWorkerConfig> {
  let storage_dir = get_storage_dir();

  if !storage_dir.exists() {
    return Vec::new();
  }

  let mut configs = Vec::new();
  if let Ok(entries) = fs::read_dir(&storage_dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.starts_with("vpn_worker_") && name.ends_with(".json") {
          if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(config) = serde_json::from_str::<VpnWorkerConfig>(&content) {
              configs.push(config);
            }
          }
        }
      }
    }
  }

  configs
}

/// Every worker of a VPN, one per OS that has used it.
pub fn find_vpn_workers_by_vpn_id(vpn_id: &str) -> Vec<VpnWorkerConfig> {
  list_vpn_worker_configs()
    .into_iter()
    .filter(|c| c.vpn_id == vpn_id)
    .collect()
}

/// Any worker of a VPN, a running one first. For callers that only ask whether
/// the VPN has a tunnel up, whatever OS it imitates.
pub fn find_vpn_worker_by_vpn_id(vpn_id: &str) -> Option<VpnWorkerConfig> {
  let mut workers = find_vpn_workers_by_vpn_id(vpn_id);
  workers.sort_by_key(|worker| !worker.pid.is_some_and(is_process_running));
  workers.into_iter().next()
}

/// The worker that serves `vpn_id` for profiles claiming `os`.
pub fn find_vpn_worker(vpn_id: &str, os: OsStack) -> Option<VpnWorkerConfig> {
  list_vpn_worker_configs()
    .into_iter()
    .find(|c| c.serves(vpn_id, os))
}

pub fn generate_vpn_worker_id() -> String {
  format!(
    "vpnw_{}_{}",
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_secs(),
    rand::random::<u32>()
  )
}
