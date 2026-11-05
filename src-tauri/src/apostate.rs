//! The open-source Apostate engine (GPL-3.0), for profiles that claim another
//! desktop OS than this machine's.
//!
//! Wayfern (closed) refuses a cross-OS device without a Donut cloud token.
//! Apostate takes the persona on its command line and has no such gate. It is a
//! separately downloaded executable that Donut starts as a child process, never
//! linked.
//!
//! The rule is automatic and has no picker: a profile created with a desktop OS
//! that differs from the host's stores `engine = "apostate"` once, and keeps it
//! on every machine it syncs to, so its persona does not change when it moves.
//! Same-OS profiles and every existing profile keep Wayfern.

use crate::downloader::DownloadProgress;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tokio::sync::Mutex as AsyncMutex;

pub const ENGINE: &str = "apostate";
pub const VERSION: &str = "0.5.1";
const CHROMIUM: &str = "155.0.8059.31";
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

pub fn is_desktop_os(os: &str) -> bool {
  matches!(os, "windows" | "macos" | "linux")
}

/// Whether a profile that claims `claimed` belongs on Apostate when this
/// machine runs `host`: a desktop OS that is not the host's. Android and iOS
/// have no Apostate persona and stay refused as before.
pub fn needs_apostate(claimed: &str, host: &str) -> bool {
  is_desktop_os(claimed) && claimed != host
}

/// The engine a new profile is stored with: `Some("apostate")` for a desktop OS
/// other than this machine's, `None` (Wayfern) otherwise.
pub fn engine_for_new_profile(claimed_os: Option<&str>) -> Option<String> {
  let claimed = claimed_os.map(str::trim).filter(|os| !os.is_empty())?;
  needs_apostate(claimed, &crate::profile::types::get_host_os()).then(|| ENGINE.to_string())
}

pub fn uses_apostate(profile: &crate::profile::BrowserProfile) -> bool {
  profile
    .wayfern_config
    .as_ref()
    .is_some_and(|config| config.uses_apostate())
}

/// Refuse a feature that talks to Wayfern's own protocol (`Wayfern.*`) on a
/// profile that runs Apostate, which would otherwise wait for a method that
/// does not exist.
pub fn refuse_wayfern_only(profile: &crate::profile::BrowserProfile) -> Result<(), String> {
  if uses_apostate(profile) {
    log::debug!(
      "Wayfern-only feature refused, profile runs on Apostate profile={}",
      profile.id
    );
    return Err(crate::backend_error("APOSTATE_FEATURE_UNSUPPORTED"));
  }
  Ok(())
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

/// The engine's executable, downloaded and verified first when it is not on
/// disk. One install at a time: a launch and a profile creation that both need
/// it share the download.
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
      log::info!(
        "Apostate installed version={VERSION} target={} elapsed_ms={}",
        asset.target,
        started.elapsed().as_millis()
      );
      Ok(exe)
    }
    Err(error) => {
      log::warn!(
        "Apostate install failed version={VERSION} target={} err=\"{}\"",
        asset.target,
        crate::log_redaction::text(&error)
      );
      emit_progress(DownloadProgress::failed(
        ENGINE,
        VERSION,
        Some(error.clone()),
      ));
      Err(error)
    }
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

/// Where `apostate fonts install` puts the fonts a persona of `platform` needs,
/// or `None` when this machine already has them (its own OS) or has no such
/// directory.
fn fonts_dir(platform: &str) -> Option<PathBuf> {
  if platform == crate::profile::types::get_host_os() {
    return None;
  }
  let name = match platform {
    "windows" => "apostate-windows",
    "macos" => "apostate-macos",
    _ => return None,
  };
  if cfg!(target_os = "macos") {
    dirs::home_dir().map(|home| home.join("Library/Fonts").join(name))
  } else if cfg!(target_os = "linux") {
    dirs::home_dir().map(|home| home.join(".local/share/fonts").join(name))
  } else {
    None
  }
}

/// Whether the persona's fonts are installed on this machine. A persona without
/// them still launches, but a page that measures text sees the host's fonts.
pub fn persona_fonts_present(platform: &str) -> bool {
  match fonts_dir(platform) {
    Some(dir) => std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some()),
    None => true,
  }
}

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

#[cfg(test)]
mod tests {
  use super::*;

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
  fn a_new_profile_is_stored_with_the_engine_its_os_needs() {
    let host = crate::profile::types::get_host_os();
    let other = if host == "windows" {
      "macos"
    } else {
      "windows"
    };
    assert_eq!(engine_for_new_profile(Some(other)).as_deref(), Some(ENGINE));
    assert_eq!(engine_for_new_profile(Some(&host)), None);
    assert_eq!(engine_for_new_profile(None), None);
    assert_eq!(engine_for_new_profile(Some("  ")), None);
    assert_eq!(engine_for_new_profile(Some("android")), None);
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
