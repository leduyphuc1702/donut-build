//! The open-source Apostate engine (GPL-3.0): the one browser every profile
//! runs on.
//!
//! Apostate takes the persona on its command line, needs no account, token or
//! terms, and presents any desktop OS. It is a separately downloaded executable
//! that Donut starts as a child process, never linked.
//!
//! Donut downloads and verifies it by itself, in the background, from the first
//! start (`start_background_install`); a launch that comes before it is done
//! waits for the same download. The browser type stays `"wayfern"` in stored
//! profiles, synced metadata and the API, so nothing already saved changes
//! shape; `engine = "apostate"` is stamped on every profile and a profile
//! stored without it reads the same (`WayfernConfig::uses_apostate`).

use crate::log_streak::Streak;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use tokio::sync::Mutex as AsyncMutex;

pub const ENGINE: &str = "apostate";
pub const VERSION: &str = "0.5.1";
/// The Chromium the engine is built on, and the `version` every profile stores:
/// the version a page's user agent reports.
pub const CHROMIUM: &str = "155.0.8059.31";
const RELEASE_BASE: &str = "https://github.com/heretic-tech/apostate/releases/download/v0.5.1";

struct Asset {
  target: &'static str,
  extension: &'static str,
  /// From the release's own `<asset>.manifest.json`, pinned here so a changed
  /// release cannot swap the binary Donut runs.
  sha256: &'static str,
  /// The executable, relative to the extracted archive's top directory.
  executable: &'static str,
}

const ASSETS: &[Asset] = &[
  Asset {
    target: "macos-arm64",
    extension: "zip",
    sha256: "58176ba9b3e9cfd74029165bdf716c636e618af17a48bd905fe6d5f1552d901a",
    executable: "Chromium.app/Contents/MacOS/Chromium",
  },
  Asset {
    target: "windows-x64",
    extension: "zip",
    sha256: "8e55e44fb7f58619461e1d42a947446dcc08a2b46493c420c8e62ae27ab9c7e5",
    executable: "chrome.exe",
  },
  Asset {
    target: "linux-x64",
    extension: "tar.zst",
    sha256: "351955271b7ba1aed314bf14c2d64f6bd59bfdcb5c4a1541c9c921c570ff97bb",
    executable: "chrome",
  },
  Asset {
    target: "linux-arm64",
    extension: "tar.zst",
    sha256: "e63e3023e4a691a1ee104074331894dec18567a41b42472e782c910588453a7c",
    executable: "chrome",
  },
];

fn host_target() -> Option<&'static str> {
  match (std::env::consts::OS, std::env::consts::ARCH) {
    ("macos", "aarch64") => Some("macos-arm64"),
    ("windows", "x86_64") => Some("windows-x64"),
    ("linux", "x86_64") => Some("linux-x64"),
    ("linux", "aarch64") => Some("linux-arm64"),
    _ => None,
  }
}

fn host_asset() -> Option<&'static Asset> {
  let target = host_target()?;
  ASSETS.iter().find(|asset| asset.target == target)
}

fn asset_file_name(asset: &Asset) -> String {
  format!("apostate-{CHROMIUM}-{}.{}", asset.target, asset.extension)
}

/// What the `download-progress` event carries while the engine installs.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DownloadProgress {
  pub browser: String,
  pub version: String,
  pub downloaded_bytes: u64,
  pub total_bytes: Option<u64>,
  pub percentage: f64,
  pub speed_bytes_per_sec: f64,
  pub eta_seconds: Option<f64>,
  /// "downloading", "extracting", "verifying", "completed" or "error".
  pub stage: String,
  /// Why an "error" stage happened: a coded backend error or plain text.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub error: Option<String>,
}

impl DownloadProgress {
  /// The terminal event for an install that stopped, so the UI can show why.
  pub fn failed(browser: &str, version: &str, error: Option<String>) -> Self {
    Self {
      browser: browser.to_string(),
      version: version.to_string(),
      downloaded_bytes: 0,
      total_bytes: None,
      percentage: 0.0,
      speed_bytes_per_sec: 0.0,
      eta_seconds: None,
      stage: "error".to_string(),
      error,
    }
  }
}

pub fn is_desktop_os(os: &str) -> bool {
  matches!(os, "windows" | "macos" | "linux")
}

pub fn is_mobile_os(os: &str) -> bool {
  matches!(os, "android" | "ios")
}

/// Whether a persona of `claimed` is another desktop OS than this machine's
/// `host`, which is the only case that needs a route whose first SYN does not
/// come from this machine's network stack.
pub fn needs_apostate(claimed: &str, host: &str) -> bool {
  is_desktop_os(claimed) && claimed != host
}

/// Stamp the engine on a profile's config. Every profile has it: the field says
/// so to anything reading the stored metadata.
pub fn stamp_engine(config: &mut crate::wayfern_manager::WayfernConfig) {
  config.engine = Some(ENGINE.to_string());
}

/// What a profile's config stores for its device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredShape {
  /// A complete device payload.
  Payload,
  /// An identity id of the closed engine, with a payload or a baseline beside it.
  IdentityWithView,
  /// An identity id alone: the closed engine derived the device from it.
  Identity,
  /// Neither: the next launch draws a device.
  Empty,
}

fn stored_identity_id(config: &crate::wayfern_manager::WayfernConfig) -> Option<&str> {
  config
    .identity_id
    .as_deref()
    .map(str::trim)
    .filter(|id| !id.is_empty())
}

fn has_stored_payload(config: &crate::wayfern_manager::WayfernConfig) -> bool {
  config
    .fingerprint
    .as_deref()
    .and_then(|json| crate::wayfern_manager::WayfernManager::launch_fingerprint_payload(json).ok())
    .and_then(|payload| payload.as_object().map(|object| !object.is_empty()))
    .unwrap_or(false)
}

pub fn stored_shape(config: &crate::wayfern_manager::WayfernConfig) -> StoredShape {
  let payload = has_stored_payload(config);
  let baseline =
    !crate::wayfern_manager::WayfernManager::stored_object(config.identity_baseline.as_deref())
      .is_empty();
  match (stored_identity_id(config).is_some(), payload, baseline) {
    (true, true, _) | (true, false, true) => StoredShape::IdentityWithView,
    (true, false, false) => StoredShape::Identity,
    (false, true, _) => StoredShape::Payload,
    (false, false, _) => StoredShape::Empty,
  }
}

/// Refuse a persona that claims a phone. Apostate presents desktop machines
/// only, and a stored mobile claim would launch as the wrong device.
pub fn refuse_mobile(claimed_os: Option<&str>) -> Result<(), String> {
  match claimed_os.map(str::trim) {
    Some(os) if is_mobile_os(os) => Err(crate::backend_error("FINGERPRINT_MOBILE_UNSUPPORTED")),
    _ => Ok(()),
  }
}

/// Every profile runs on Apostate; the argument only keeps call sites uniform
/// with the other per-profile checks.
pub fn uses_apostate(_profile: &crate::profile::BrowserProfile) -> bool {
  true
}

/// Refuse a feature that talks to Wayfern's own protocol (`Wayfern.*`). No
/// profile runs on that engine any more, so the call would wait for a method
/// that does not exist.
pub fn refuse_wayfern_only(profile: &crate::profile::BrowserProfile) -> Result<(), String> {
  log::debug!(
    "Wayfern-only feature refused, profile runs on Apostate profile={}",
    profile.id
  );
  Err(crate::backend_error("APOSTATE_FEATURE_UNSUPPORTED"))
}

/// `refuse_wayfern_only` for profiles named by id; an id that names no profile
/// is left for the caller to report.
pub fn refuse_wayfern_only_ids(ids: &[String]) -> Result<(), String> {
  let profiles = crate::profile::ProfileManager::instance()
    .list_profiles()
    .map_err(|e| e.to_string())?;
  profiles
    .iter()
    .filter(|profile| ids.contains(&profile.id.to_string()))
    .try_for_each(refuse_wayfern_only)
}

/// What a config stored for the closed engine needs to run on Apostate, and
/// nothing more. Returns whether it changed anything.
///
/// A profile whose device lived behind an identity id (the closed engine
/// derived it, so Donut holds no payload) gets a device drawn here from the
/// profile's own seed, with the person's edits to it applied on top, and the
/// identity fields go: with an id left in place the form keeps writing edits
/// into overrides that Apostate never reads. The exit's location was stored
/// separately (`location`) and stays. Nothing here asks the network, so it runs
/// at startup; the same seed and OS draw the same device on every machine.
pub fn adopt_config(
  profile: &crate::profile::BrowserProfile,
  config: &mut crate::wayfern_manager::WayfernConfig,
) -> bool {
  use crate::wayfern_manager::WayfernManager;

  let mut changed = config.engine.as_deref() != Some(ENGINE);
  stamp_engine(config);

  let shape = stored_shape(config);
  if shape == StoredShape::Identity {
    let os = config
      .os
      .clone()
      .filter(|os| !os.is_empty())
      .unwrap_or_else(|| profile.native_os());
    if is_desktop_os(&os) {
      let mut device =
        crate::fingerprint_generator::generate(&profile.id.to_string(), &os, CHROMIUM, None);
      if let Some(object) = device.as_object_mut() {
        object.extend(WayfernManager::stored_object(
          config.identity_overrides.as_deref(),
        ));
      }
      config.fingerprint = Some(device.to_string());
      if config.os.as_deref().is_none_or(str::is_empty) {
        config.os = Some(os);
      }
    }
  }
  let has_payload = stored_shape(config) == StoredShape::Payload
    || config
      .fingerprint
      .as_deref()
      .and_then(WayfernManager::fingerprint_object)
      .is_some_and(|device| !device.is_empty());
  let has_identity = config.identity_id.is_some()
    || config.identity_baseline.is_some()
    || config.identity_overrides.is_some();
  if has_payload && has_identity {
    config.identity_id = None;
    config.identity_baseline = None;
    config.identity_overrides = None;
    changed = true;
  } else if shape == StoredShape::Identity && has_payload {
    changed = true;
  }
  changed
}

/// Move every stored profile to Apostate (`adopt_profile`). Not an edit: no
/// `updated_at` bump and no team activity, so no machine uploads a metadata
/// change its peers will not make by themselves. The profile data directory is
/// never touched, so cookies and logins carry over. Returns how many profiles
/// moved.
pub fn migrate_profiles() -> usize {
  let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() else {
    return 0;
  };
  let moved = profiles
    .iter()
    .filter(|profile| adopt_profile(profile).is_some())
    .count();
  if moved > 0 {
    let _ = crate::events::emit_empty("profiles-changed");
  }
  moved
}

/// The profile as Apostate runs it, stored, or `None` when it already is one.
/// A launch calls this too, so a profile that no startup pass has reached yet
/// (a sync that just delivered it) is moved before it opens.
///
/// The stored `version` becomes the engine's Chromium: the persona's user agent
/// is built from it, and the closed engine's number would claim a browser that
/// is not the one running.
pub fn adopt_profile(
  profile: &crate::profile::BrowserProfile,
) -> Option<crate::profile::BrowserProfile> {
  if profile.browser != "wayfern" {
    return None;
  }
  let mut updated = profile.clone();
  let mut identity_replaced = false;
  let mut changed = updated.version != CHROMIUM;
  updated.version = CHROMIUM.to_string();
  if let Some(config) = updated.wayfern_config.as_mut() {
    identity_replaced = config.identity_id.is_some();
    changed |= adopt_config(profile, config);
  }
  if !changed {
    return None;
  }
  let manager = crate::profile::ProfileManager::instance();
  match manager.save_profile_unchanged(profile, &updated) {
    Ok(true) => {
      log::info!(
        "Profile moved to Apostate profile={} identity_replaced={identity_replaced}",
        profile.id
      );
      Some(updated)
    }
    Ok(false) => {
      log::debug!(
        "Profile changed while moving to Apostate, next start retries profile={}",
        profile.id
      );
      None
    }
    Err(error) => {
      log::warn!(
        "Profile not moved to Apostate profile={} err=\"{error}\"",
        profile.id
      );
      None
    }
  }
}

fn install_dir(asset: &Asset) -> PathBuf {
  crate::app_dirs::binaries_dir()
    .join(ENGINE)
    .join(VERSION)
    .join(asset.target)
}

fn executable_in(dir: &Path, asset: &Asset) -> PathBuf {
  dir.join(asset.executable)
}

/// The installed executable, if the engine is on disk.
pub fn installed_executable() -> Option<PathBuf> {
  let asset = host_asset()?;
  let exe = executable_in(&install_dir(asset), asset);
  exe.is_file().then_some(exe)
}

static INSTALL: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

fn download_failed(detail: impl std::fmt::Display) -> String {
  crate::backend_error_with_detail("APOSTATE_DOWNLOAD_FAILED", detail)
}

fn emit_progress(progress: DownloadProgress) {
  let _ = crate::events::emit("download-progress", &progress);
}

fn stage(stage: &str, downloaded: u64, total: Option<u64>) -> DownloadProgress {
  DownloadProgress {
    browser: ENGINE.to_string(),
    version: VERSION.to_string(),
    downloaded_bytes: downloaded,
    total_bytes: total,
    percentage: match total {
      Some(total) if total > 0 => (downloaded as f64 / total as f64 * 100.0).min(100.0),
      _ => 0.0,
    },
    speed_bytes_per_sec: 0.0,
    eta_seconds: None,
    stage: stage.to_string(),
    error: None,
  }
}

static INSTALL_STREAK: Streak = Streak::new(module_path!(), "Apostate install");
static BACKGROUND_INSTALL: AtomicBool = AtomicBool::new(false);

/// The engine's executable, downloaded and verified first when it is not on
/// disk. One install at a time: a launch, a profile creation and the startup
/// install that all need it share the download.
pub async fn ensure_installed() -> Result<PathBuf, String> {
  let Some(asset) = host_asset() else {
    log::warn!(
      "Apostate has no build for this machine os={} arch={}",
      std::env::consts::OS,
      std::env::consts::ARCH
    );
    return Err(crate::backend_error("APOSTATE_UNSUPPORTED_HOST"));
  };
  if let Some(exe) = installed_executable() {
    return Ok(exe);
  }
  let _install = INSTALL.lock().await;
  if let Some(exe) = installed_executable() {
    return Ok(exe);
  }
  let started = std::time::Instant::now();
  match install(asset).await {
    Ok(exe) => {
      emit_progress(stage("completed", 0, None));
      INSTALL_STREAK.succeeded();
      log::info!(
        "Apostate installed version={VERSION} target={} elapsed_ms={}",
        asset.target,
        started.elapsed().as_millis()
      );
      Ok(exe)
    }
    Err(error) => {
      INSTALL_STREAK.failed(format!(
        "version={VERSION} target={} err=\"{}\"",
        asset.target,
        crate::log_redaction::text(&error)
      ));
      emit_progress(DownloadProgress::failed(
        ENGINE,
        VERSION,
        Some(error.clone()),
      ));
      Err(error)
    }
  }
}

/// Whether the engine's download at startup is switched off: an end-to-end
/// session without network, which seeds a stand-in executable instead.
fn startup_install_disabled() -> bool {
  cfg!(test)
    || (cfg!(feature = "e2e")
      && crate::e2e_automation_enabled()
      && std::env::var_os("DONUT_E2E_DISABLE_STARTUP_NETWORK").is_some())
}

/// How long the background install waits after its n-th failed attempt.
pub(crate) fn retry_delay(failures: u32) -> std::time::Duration {
  std::time::Duration::from_secs((15u64 << failures.min(5)).min(300))
}

/// Download and verify the engine in the background until it is on disk. Idempotent:
/// a second call while one runs does nothing, and an installed engine ends it at
/// once. A failed attempt (offline, a blocked host) is retried with a growing wait,
/// logged through `INSTALL_STREAK`, and a launch that comes in the meantime tries
/// by itself. The download shows its progress through `download-progress`.
/// The persona fonts follow once the engine is on disk.
pub fn start_background_install() {
  if startup_install_disabled() {
    return;
  }
  if BACKGROUND_INSTALL.swap(true, Ordering::SeqCst) {
    return;
  }
  tauri::async_runtime::spawn(async {
    {
      let _install = INSTALL.lock().await;
      let binaries = crate::app_dirs::binaries_dir();
      let _ = tokio::task::spawn_blocking(move || sweep_leftovers(&binaries)).await;
    }
    let mut failures = 0u32;
    let installed = loop {
      match ensure_installed().await {
        Ok(_) => break true,
        Err(error) if error.contains("APOSTATE_UNSUPPORTED_HOST") => break false,
        Err(_) => {
          tokio::time::sleep(retry_delay(failures)).await;
          failures += 1;
        }
      }
    };
    if installed {
      crate::persona_fonts::keep_installing().await;
    }
    BACKGROUND_INSTALL.store(false, Ordering::SeqCst);
  });
}

/// Removes what no launch reads any more: downloads an earlier run left half
/// done when it quit, other Apostate versions, and the Wayfern builds the
/// app used before Apostate. Runs under `INSTALL`, so no download is live.
fn sweep_leftovers(binaries: &Path) {
  let Ok(entries) = std::fs::read_dir(binaries) else {
    return;
  };
  let staging_prefix = format!(".{ENGINE}-staging-");
  let mut removed = Vec::new();
  for entry in entries.flatten() {
    let name = entry.file_name().to_string_lossy().into_owned();
    if (name.starts_with(&staging_prefix) || name == "wayfern")
      && std::fs::remove_dir_all(entry.path()).is_ok()
    {
      removed.push(name);
    }
  }
  if let Ok(versions) = std::fs::read_dir(binaries.join(ENGINE)) {
    for entry in versions.flatten() {
      let name = entry.file_name().to_string_lossy().into_owned();
      if name != VERSION && std::fs::remove_dir_all(entry.path()).is_ok() {
        removed.push(format!("{ENGINE}/{name}"));
      }
    }
  }
  if !removed.is_empty() {
    log::info!(
      "Removed old engine files count={} kinds=[{}]",
      removed.len(),
      removed
        .iter()
        .map(|name| if name.starts_with(&staging_prefix) {
          "partial_download"
        } else if name == "wayfern" {
          "wayfern"
        } else {
          "old_version"
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",")
    );
  }
}

async fn install(asset: &'static Asset) -> Result<PathBuf, String> {
  use futures_util::StreamExt;
  use sha2::{Digest, Sha256};
  use tokio::io::AsyncWriteExt;

  let staging = crate::app_dirs::binaries_dir().join(format!(
    ".{ENGINE}-staging-{}",
    uuid::Uuid::new_v4().simple()
  ));
  tokio::fs::create_dir_all(&staging)
    .await
    .map_err(download_failed)?;
  let result = async {
    let archive = staging.join(asset_file_name(asset));
    let url = format!("{RELEASE_BASE}/{}", asset_file_name(asset));
    let client = crate::app_proxy::client_builder()
      .build()
      .map_err(download_failed)?;
    let response = client
      .get(&url)
      .send()
      .await
      .and_then(|response| response.error_for_status())
      .map_err(download_failed)?;
    let total = response.content_length();
    let mut file = tokio::fs::File::create(&archive)
      .await
      .map_err(download_failed)?;
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    let mut done = 0u64;
    let began = std::time::Instant::now();
    let mut last_emit = std::time::Instant::now();
    while let Some(chunk) = stream.next().await {
      let chunk = chunk.map_err(download_failed)?;
      hasher.update(&chunk);
      file.write_all(&chunk).await.map_err(download_failed)?;
      done += chunk.len() as u64;
      if last_emit.elapsed() >= std::time::Duration::from_millis(250) {
        last_emit = std::time::Instant::now();
        let mut progress = stage("downloading", done, total);
        let elapsed = began.elapsed().as_secs_f64().max(0.001);
        progress.speed_bytes_per_sec = done as f64 / elapsed;
        progress.eta_seconds = total
          .filter(|_| progress.speed_bytes_per_sec > 0.0)
          .map(|total| total.saturating_sub(done) as f64 / progress.speed_bytes_per_sec);
        emit_progress(progress);
      }
    }
    file.flush().await.map_err(download_failed)?;
    drop(file);

    emit_progress(stage("verifying", done, total));
    let digest = hasher.finalize();
    let mut actual = String::with_capacity(digest.len() * 2);
    for byte in digest {
      use std::fmt::Write;
      let _ = write!(actual, "{byte:02x}");
    }
    if !actual.eq_ignore_ascii_case(asset.sha256) {
      log::warn!(
        "Apostate archive checksum mismatch target={} expected={} actual={actual}",
        asset.target,
        asset.sha256
      );
      return Err(crate::backend_error("APOSTATE_CHECKSUM_MISMATCH"));
    }

    emit_progress(stage("extracting", 0, None));
    let unpacked = staging.join("unpacked");
    extract(&archive, &unpacked)
      .await
      .map_err(download_failed)?;
    let top = single_top_directory(&unpacked).unwrap_or_else(|| unpacked.clone());
    if !executable_in(&top, asset).is_file() {
      return Err(download_failed("the archive holds no browser executable"));
    }

    let destination = install_dir(asset);
    if let Some(parent) = destination.parent() {
      tokio::fs::create_dir_all(parent)
        .await
        .map_err(download_failed)?;
    }
    let _ = tokio::fs::remove_dir_all(&destination).await;
    tokio::fs::rename(&top, &destination)
      .await
      .map_err(download_failed)?;
    Ok(executable_in(&destination, asset))
  }
  .await;
  let _ = tokio::fs::remove_dir_all(&staging).await;
  result
}

/// The one directory an archive unpacks into, or `None` when it holds more.
fn single_top_directory(dir: &Path) -> Option<PathBuf> {
  let mut entries = std::fs::read_dir(dir).ok()?.flatten();
  let first = entries.next()?;
  (entries.next().is_none() && first.path().is_dir()).then(|| first.path())
}

async fn extract(archive: &Path, destination: &Path) -> Result<(), String> {
  tokio::fs::create_dir_all(destination)
    .await
    .map_err(|e| e.to_string())?;
  if cfg!(target_os = "macos") {
    // The app bundle's framework links are symlinks, which the zip crate would
    // write out as small files, and an app bundle with those does not start.
    return run_extractor("ditto", &["-x", "-k"], archive, destination).await;
  }
  if archive.extension().is_some_and(|ext| ext == "zst") {
    return run_extractor("tar", &["--zstd", "-xf"], archive, destination).await;
  }
  crate::extraction::Extractor::instance()
    .extract_zip(archive, destination, None)
    .await
    .map(|_| ())
    .or_else(|error| {
      // The extractor also looks for a browser executable it knows by name; the
      // caller checks for Apostate's own, so only an unpacking failure counts.
      if std::fs::read_dir(destination).is_ok_and(|mut entries| entries.next().is_some()) {
        log::debug!("Apostate zip unpacked, extractor lookup ignored err=\"{error}\"");
        Ok(())
      } else {
        Err(error.to_string())
      }
    })
}

async fn run_extractor(
  program: &str,
  flags: &[&str],
  archive: &Path,
  destination: &Path,
) -> Result<(), String> {
  let mut command = tokio::process::Command::new(program);
  command.args(flags);
  if program == "tar" {
    command.arg(archive).arg("-C").arg(destination);
  } else {
    command.arg(archive).arg(destination);
  }
  let output = command
    .stdin(std::process::Stdio::null())
    .output()
    .await
    .map_err(|e| format!("{program}: {e}"))?;
  if output.status.success() {
    return Ok(());
  }
  Err(format!(
    "{program} exited with {}: {}",
    output.status,
    String::from_utf8_lossy(&output.stderr).trim()
  ))
}

/// What the persona flags are built from.
pub struct Persona<'a> {
  /// The profile id: the same seed gives the same machine on every launch.
  pub seed: &'a str,
  pub platform: &'a str,
  /// The exit's languages, best first (`navigator.languages` and
  /// `Accept-Language`); the first is the UI locale.
  pub languages: Vec<String>,
  pub timezone: Option<&'a str>,
  /// The exit address, written into WebRTC candidates in place of the real one.
  pub webrtc_ip: Option<&'a str>,
  /// Block WebRTC UDP altogether.
  pub block_webrtc: bool,
  /// The screen, cores and memory of the device Donut stored for the profile,
  /// so the persona draws them from Donut's catalog of real desktops.
  pub device: Option<DeviceClaim>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceClaim {
  pub screen: Option<(u32, u32)>,
  pub cores: Option<u32>,
  pub memory_gb: Option<u32>,
}

impl DeviceClaim {
  /// The claim in a stored device payload. The engine refuses more cores or
  /// memory than this machine has, since a page can measure both, so those
  /// are capped at the host's.
  pub fn from_payload(device: &serde_json::Value) -> Option<Self> {
    let read = |key: &str| {
      device
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
    };
    let screen = read("screenWidth").zip(read("screenHeight"));
    let host_cores = std::thread::available_parallelism()
      .map(|n| n.get() as u32)
      .unwrap_or(4);
    let host_memory_gb = {
      let mut system = crate::proxy_storage::process_table(sysinfo::RefreshKind::nothing());
      system.refresh_memory();
      u32::try_from(system.total_memory() >> 30)
        .unwrap_or(u32::MAX)
        .max(1)
    };
    let cores = read("hardwareConcurrency").map(|n| n.min(host_cores));
    let memory_gb = read("deviceMemory").map(|n| n.min(host_memory_gb));
    let claim = Self {
      screen,
      cores,
      memory_gb,
    };
    (claim.screen.is_some() || claim.cores.is_some() || claim.memory_gb.is_some()).then_some(claim)
  }
}

fn valid_seed(seed: &str) -> bool {
  !seed.is_empty() && seed.len() <= 512 && seed.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn valid_language_tag(tag: &str) -> bool {
  tag.starts_with(|c: char| c.is_ascii_alphanumeric())
    && tag.len() <= 35
    && tag
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Persona switches for `Persona`. The engine ignores an unknown platform and
/// an unknown timezone and silently presents this machine, so both are checked
/// here instead of trusted.
pub fn persona_args(persona: &Persona) -> Result<Vec<String>, String> {
  if !is_desktop_os(persona.platform) {
    return Err(
      serde_json::json!({
        "code": "FINGERPRINT_HOST_OS_MISMATCH",
        "params": {
          "claimed": persona.platform,
          "host": crate::profile::types::get_host_os(),
        },
      })
      .to_string(),
    );
  }
  if !valid_seed(persona.seed) {
    return Err(crate::backend_error("INTERNAL_ERROR"));
  }
  let mut args = vec![
    format!("--fingerprint={}", persona.seed),
    format!("--fingerprint-platform={}", persona.platform),
    // Without it two personas on one host draw the same canvas image.
    "--fingerprint-noise".to_string(),
  ];
  let languages: Vec<&str> = persona
    .languages
    .iter()
    .map(String::as_str)
    .filter(|tag| valid_language_tag(tag))
    .collect();
  if !languages.is_empty() {
    args.push(format!("--fingerprint-locale={}", languages.join(",")));
  }
  if let Some(timezone) = persona.timezone {
    if timezone.parse::<chrono_tz::Tz>().is_ok() {
      args.push(format!("--fingerprint-timezone={timezone}"));
    }
  }
  if let Some(device) = persona.device {
    if let Some((width, height)) = device.screen {
      args.push(format!("--fingerprint-screen-width={width}"));
      args.push(format!("--fingerprint-screen-height={height}"));
    }
    if let Some(cores) = device.cores {
      args.push(format!("--fingerprint-hardware-concurrency={cores}"));
    }
    if let Some(memory) = device.memory_gb {
      args.push(format!("--fingerprint-device-memory={memory}"));
    }
  }
  if persona.block_webrtc {
    args.push("--fingerprint-webrtc-udp=block".to_string());
  } else if let Some(ip) = persona
    .webrtc_ip
    .filter(|ip| ip.parse::<std::net::IpAddr>().is_ok())
  {
    args.push(format!("--fingerprint-webrtc-ip={ip}"));
  }
  Ok(args)
}

/// The environment Apostate's Chromium is started with: no Google API keys,
/// so it never tries to reach Google's services with placeholder ones.
pub const ENVIRONMENT: &[(&str, &str)] = &[
  ("GOOGLE_API_KEY", "no"),
  ("GOOGLE_DEFAULT_CLIENT_ID", "no"),
  ("GOOGLE_DEFAULT_CLIENT_SECRET", "no"),
];

/// Geolocation as CDP's `Emulation.setGeolocationOverride` takes it. Apostate
/// has no switch for it, and an override lasts only as long as the CDP session
/// that set it, so the launcher keeps one session open for the browser's life.
pub fn geolocation_override(
  location: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
  let latitude = location
    .get("latitude")
    .and_then(serde_json::Value::as_f64)?;
  let longitude = location
    .get("longitude")
    .and_then(serde_json::Value::as_f64)?;
  if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
    return None;
  }
  let accuracy = location
    .get("accuracy")
    .and_then(serde_json::Value::as_f64)
    .filter(|accuracy| *accuracy > 0.0)
    .unwrap_or(100.0);
  Some(serde_json::json!({
    "latitude": latitude,
    "longitude": longitude,
    "accuracy": accuracy,
  }))
}

/// Whether the engine is on disk, for the UI's "preparing the engine" notes.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct InstallStatus {
  pub installed: bool,
  /// False on a machine the engine has no build for: nothing will download.
  pub supported: bool,
  pub version: &'static str,
  pub chromium: &'static str,
}

#[tauri::command]
pub fn get_engine_install_status() -> InstallStatus {
  InstallStatus {
    installed: installed_executable().is_some(),
    supported: host_asset().is_some(),
    version: VERSION,
    chromium: CHROMIUM,
  }
}

/// The engine's download, for callers that name a browser and a version (the
/// REST API and the frontend's download calls). There is one engine: any version
/// asked for gets it, and the answer is the version it is.
#[tauri::command]
pub async fn download_browser(browser_str: String, version: String) -> Result<String, String> {
  if browser_str != "wayfern" {
    return Err(crate::backend_error_with_detail(
      "INTERNAL_ERROR",
      format!(
        "Invalid browser type: {}",
        crate::log_redaction::Plain(&browser_str)
      ),
    ));
  }
  log::debug!(
    "Engine download requested version={}",
    crate::log_redaction::Plain(&version)
  );
  ensure_installed().await?;
  Ok(CHROMIUM.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sweep_keeps_only_the_pinned_engine() {
    let root = tempfile::tempdir().unwrap();
    let binaries = root.path();
    let keep = binaries.join(ENGINE).join(VERSION).join("macos-arm64");
    std::fs::create_dir_all(&keep).unwrap();
    std::fs::write(keep.join("marker"), b"x").unwrap();
    for dir in [
      binaries.join(format!(".{ENGINE}-staging-abc")),
      binaries.join("wayfern").join("156.0.1"),
      binaries.join(ENGINE).join("0.4.0"),
    ] {
      std::fs::create_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(binaries.join("other")).unwrap();

    sweep_leftovers(binaries);

    assert!(keep.join("marker").is_file());
    assert!(binaries.join("other").is_dir());
    assert!(!binaries.join(format!(".{ENGINE}-staging-abc")).exists());
    assert!(!binaries.join("wayfern").exists());
    assert!(!binaries.join(ENGINE).join("0.4.0").exists());
  }

  fn persona<'a>(platform: &'a str) -> Persona<'a> {
    Persona {
      seed: "6f1c2d3e-0000-4000-8000-000000000001",
      platform,
      languages: vec!["en-US".into(), "en".into()],
      timezone: Some("America/Los_Angeles"),
      webrtc_ip: None,
      block_webrtc: false,
      device: None,
    }
  }

  #[test]
  fn the_stored_device_sets_screen_cores_and_memory_within_the_host() {
    let device = serde_json::json!({
      "screenWidth": 1536,
      "screenHeight": 864,
      "hardwareConcurrency": 4096,
      "deviceMemory": 8,
    });
    let claim = DeviceClaim::from_payload(&device).unwrap();
    assert_eq!(claim.screen, Some((1536, 864)));
    let host_cores = std::thread::available_parallelism().unwrap().get() as u32;
    assert_eq!(claim.cores, Some(host_cores));
    let mut with_device = persona("windows");
    with_device.device = Some(claim);
    let args = persona_args(&with_device).unwrap();
    assert!(args.contains(&"--fingerprint-screen-width=1536".to_string()));
    assert!(args.contains(&"--fingerprint-screen-height=864".to_string()));
    assert!(args.contains(&format!("--fingerprint-hardware-concurrency={host_cores}")));
    assert!(args
      .iter()
      .any(|a| a.starts_with("--fingerprint-device-memory=")));
    assert!(DeviceClaim::from_payload(&serde_json::json!({})).is_none());
  }

  #[test]
  fn only_a_desktop_os_other_than_the_hosts_needs_apostate() {
    assert!(needs_apostate("windows", "macos"));
    assert!(needs_apostate("macos", "windows"));
    assert!(needs_apostate("linux", "macos"));
    assert!(!needs_apostate("macos", "macos"));
    assert!(!needs_apostate("android", "macos"));
    assert!(!needs_apostate("ios", "windows"));
    assert!(!needs_apostate("", "macos"));
  }

  #[test]
  fn the_persona_flags_are_separate_arguments_and_carry_noise() {
    let args = persona_args(&persona("windows")).unwrap();
    assert_eq!(
      args,
      vec![
        "--fingerprint=6f1c2d3e-0000-4000-8000-000000000001",
        "--fingerprint-platform=windows",
        "--fingerprint-noise",
        "--fingerprint-locale=en-US,en",
        "--fingerprint-timezone=America/Los_Angeles",
      ]
    );
  }

  #[test]
  fn an_unknown_platform_is_refused_because_the_engine_would_not_say_so() {
    for platform in ["win", "darwin", "android", "ios", ""] {
      let error = persona_args(&persona(platform)).unwrap_err();
      assert!(error.contains("FINGERPRINT_HOST_OS_MISMATCH"), "{error}");
    }
  }

  #[test]
  fn an_unknown_timezone_is_left_out_rather_than_kept_as_the_hosts() {
    let mut bad = persona("windows");
    bad.timezone = Some("Mars/Olympus");
    let args = persona_args(&bad).unwrap();
    assert!(!args.iter().any(|a| a.starts_with("--fingerprint-timezone")));
  }

  #[test]
  fn locale_tags_that_could_not_be_a_tag_are_dropped() {
    let mut odd = persona("windows");
    odd.languages = vec!["de-DE".into(), "x y".into(), "--evil".into(), "de".into()];
    let args = persona_args(&odd).unwrap();
    assert!(
      args.contains(&"--fingerprint-locale=de-DE,de".to_string()),
      "{args:?}"
    );
    odd.languages = vec!["x y".into()];
    assert!(!persona_args(&odd)
      .unwrap()
      .iter()
      .any(|a| a.starts_with("--fingerprint-locale")));
  }

  #[test]
  fn webrtc_follows_the_mode_and_only_takes_a_real_address() {
    let mut routed = persona("windows");
    routed.webrtc_ip = Some("203.0.113.9");
    assert!(persona_args(&routed)
      .unwrap()
      .contains(&"--fingerprint-webrtc-ip=203.0.113.9".to_string()));
    routed.webrtc_ip = Some("not an address");
    assert!(!persona_args(&routed)
      .unwrap()
      .iter()
      .any(|a| a.starts_with("--fingerprint-webrtc-ip")));
    routed.webrtc_ip = Some("203.0.113.9");
    routed.block_webrtc = true;
    let args = persona_args(&routed).unwrap();
    assert!(args.contains(&"--fingerprint-webrtc-udp=block".to_string()));
    assert!(!args
      .iter()
      .any(|a| a.starts_with("--fingerprint-webrtc-ip")));
  }

  #[test]
  fn a_seed_the_engine_would_refuse_is_refused_here() {
    let mut spaced = persona("windows");
    spaced.seed = "has space";
    assert!(persona_args(&spaced).is_err());
    spaced.seed = "";
    assert!(persona_args(&spaced).is_err());
  }

  #[test]
  fn every_pinned_asset_is_a_sha256_and_has_an_executable() {
    for asset in ASSETS {
      assert_eq!(asset.sha256.len(), 64, "{}", asset.target);
      assert!(
        asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "{}",
        asset.target
      );
      assert!(!asset.executable.is_empty());
      assert!(asset_file_name(asset).starts_with("apostate-155."));
    }
  }

  fn legacy_profile() -> crate::profile::BrowserProfile {
    crate::profile::BrowserProfile {
      name: "p".to_string(),
      browser: "wayfern".to_string(),
      version: "150.0.1.1".to_string(),
      ..Default::default()
    }
  }

  #[test]
  fn a_phone_claim_is_refused_with_its_code_and_a_desktop_one_is_not() {
    for os in ["android", "ios", " android "] {
      let error = refuse_mobile(Some(os)).unwrap_err();
      assert!(
        error.contains("FINGERPRINT_MOBILE_UNSUPPORTED"),
        "{os}: {error}"
      );
    }
    for os in [
      Some("windows"),
      Some("macos"),
      Some("linux"),
      Some(""),
      None,
    ] {
      assert!(refuse_mobile(os).is_ok(), "{os:?}");
    }
  }

  #[test]
  fn every_config_is_stamped_with_the_engine() {
    let mut config = crate::wayfern_manager::WayfernConfig::default();
    assert_eq!(config.engine, None);
    stamp_engine(&mut config);
    assert_eq!(config.engine.as_deref(), Some("apostate"));
  }

  #[test]
  fn the_stored_shape_tells_a_payload_from_an_identity() {
    let mut config = crate::wayfern_manager::WayfernConfig::default();
    assert_eq!(stored_shape(&config), StoredShape::Empty);

    config.identity_id = Some("id-1".to_string());
    assert_eq!(stored_shape(&config), StoredShape::Identity);

    config.identity_baseline = Some(r#"{"platform":"Win32"}"#.to_string());
    assert_eq!(stored_shape(&config), StoredShape::IdentityWithView);

    config.identity_id = None;
    config.identity_baseline = None;
    config.fingerprint = Some(r#"{"platform":"Win32"}"#.to_string());
    assert_eq!(stored_shape(&config), StoredShape::Payload);
  }

  #[test]
  fn an_identity_only_profile_gets_a_device_drawn_from_its_own_seed() {
    let profile = legacy_profile();
    let mut config = crate::wayfern_manager::WayfernConfig {
      identity_id: Some("id-1".to_string()),
      os: Some("windows".to_string()),
      ..Default::default()
    };

    assert!(adopt_config(&profile, &mut config));
    assert_eq!(config.engine.as_deref(), Some("apostate"));
    assert_eq!(config.identity_id, None);
    assert_eq!(stored_shape(&config), StoredShape::Payload);

    let first = config.fingerprint.clone();
    let mut again = crate::wayfern_manager::WayfernConfig {
      identity_id: Some("id-1".to_string()),
      os: Some("windows".to_string()),
      ..Default::default()
    };
    adopt_config(&profile, &mut again);
    assert_eq!(
      first, again.fingerprint,
      "the same seed draws the same device"
    );
  }

  #[test]
  fn an_adopted_config_is_left_alone_the_second_time() {
    let profile = legacy_profile();
    let mut config = crate::wayfern_manager::WayfernConfig {
      identity_id: Some("id-1".to_string()),
      os: Some("linux".to_string()),
      ..Default::default()
    };
    assert!(adopt_config(&profile, &mut config));
    let after_first = config.fingerprint.clone();
    assert!(!adopt_config(&profile, &mut config));
    assert_eq!(config.fingerprint, after_first);
  }

  #[test]
  fn a_payload_beside_a_stale_identity_drops_the_identity() {
    let profile = legacy_profile();
    let mut config = crate::wayfern_manager::WayfernConfig {
      identity_id: Some("id-1".to_string()),
      identity_overrides: Some(r#"{"hardwareConcurrency":4}"#.to_string()),
      fingerprint: Some(r#"{"platform":"Win32","hardwareConcurrency":8}"#.to_string()),
      ..Default::default()
    };
    assert!(adopt_config(&profile, &mut config));
    assert_eq!(config.identity_id, None);
    assert_eq!(config.identity_overrides, None);
    assert!(config.fingerprint.as_deref().unwrap().contains("Win32"));
  }

  #[test]
  fn the_background_install_waits_longer_after_each_failure_up_to_five_minutes() {
    let waits: Vec<u64> = (0..8).map(|n| retry_delay(n).as_secs()).collect();
    assert_eq!(waits, [15, 30, 60, 120, 240, 300, 300, 300]);
  }

  #[test]
  fn the_geolocation_override_needs_a_position_on_earth() {
    let location = |json: &str| {
      serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json).unwrap()
    };
    let ok = geolocation_override(&location(r#"{"latitude":47.6,"longitude":-122.3}"#)).unwrap();
    assert_eq!(ok["accuracy"], 100.0);
    assert!(geolocation_override(&location(r#"{"latitude":47.6}"#)).is_none());
    assert!(geolocation_override(&location(r#"{"latitude":95,"longitude":0}"#)).is_none());
  }
}
