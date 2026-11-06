use crate::profile::BrowserProfile;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod apostate_launch;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WayfernConfig {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub fingerprint: Option<String>,
  #[serde(default)]
  pub randomize_fingerprint_on_launch: Option<bool>,
  #[serde(default)]
  pub os: Option<String>,
  #[serde(default)]
  pub screen_max_width: Option<u32>,
  #[serde(default)]
  pub screen_max_height: Option<u32>,
  #[serde(default)]
  pub screen_min_width: Option<u32>,
  #[serde(default)]
  pub screen_min_height: Option<u32>,
  #[serde(default)]
  pub geoip: Option<serde_json::Value>, // For compatibility with shared config form
  #[serde(default)]
  pub block_images: Option<bool>, // For compatibility with shared config form
  /// LEGACY on/off switch kept for stored configs; `webrtc_mode` wins when
  /// both are present, and `Some(true)` alone reads as `block`.
  #[serde(default)]
  pub block_webrtc: Option<bool>,
  /// How WebRTC may reach the network: `auto` (real STUN only where the UDP
  /// it needs is carried by the route, else the exit-IP posture), `tcp_only`
  /// (one server-reflexive candidate on the proxy's exit IP), or `block` (no
  /// ICE candidates at all). `None` is `auto`.
  #[serde(default)]
  pub webrtc_mode: Option<String>,
  /// The user's edits to the profile's persona, as a JSON array of
  /// `{id,label,value}`. Everything not edited is derived from the profile's
  /// own seed, so this holds edits and nothing else. An empty value removes
  /// that row from what the browser offers.
  #[serde(default)]
  pub persona: Option<String>,
  /// A still (.png) or clip (.y4m/.mjpeg) the claimed camera serves, as an
  /// absolute path. `None` means the camera shows the computer's own camera
  /// picture. A file that is set is always used, never the computer's camera.
  #[serde(default)]
  pub camera_file: Option<String>,
  /// `x,y,width,height` in SOURCE pixels, cropped before the frame is scaled.
  #[serde(default)]
  pub camera_crop: Option<String>,
  /// Whether an interactive launch reopens the windows and tabs of the last
  /// session. `None` is the default, which is on. Automation, headless,
  /// ephemeral and clear-on-close launches never restore, whatever this says,
  /// and neither does a browser that cannot take the identity at launch: a
  /// restored tab loads before any post-launch `setIdentity`, so it would
  /// carry the host device for its whole lifetime.
  #[serde(default)]
  pub restore_session: Option<bool>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub launch_args: Vec<String>,
  #[serde(default)]
  pub block_webgl: Option<bool>,
  #[serde(default, skip_serializing)]
  pub proxy: Option<String>,
  /// Stable signature of the proxy/VPN/geoip the fingerprint's location data
  /// (timezone, latitude/longitude, language) was last computed for. Compared
  /// on launch to detect that the routing changed since creation, so the
  /// location can be refreshed instead of showing stale data.
  #[serde(default)]
  pub geo_proxy_signature: Option<String>,
  #[serde(default)]
  pub identity_id: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub identity_baseline: Option<String>,
  /// The user's own edits to an identity-backed device, as a JSON object of
  /// fingerprint fields. Sent verbatim as `setIdentity` overrides; everything
  /// not listed here comes from the identity. `None` means no edits.
  #[serde(default)]
  pub identity_overrides: Option<String>,
  /// The location the profile's exit resolves to (timezone, timezoneOffset,
  /// language, languages, latitude, longitude, accuracy) as a JSON object.
  /// It depends on the proxy, not on the identity, which is why it is the one
  /// piece of device state an identity-backed profile persists.
  #[serde(default)]
  pub location: Option<String>,
  /// The engine that runs the profile. Always `"apostate"` now: it is stamped at
  /// creation and by the startup migration, and a profile stored without it
  /// (an older build, a synced copy) reads the same.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub engine: Option<String>,
}

impl WayfernConfig {
  pub async fn checked_launch_args(&self) -> Result<Vec<String>, String> {
    let has_args = self.launch_args.iter().any(|arg| !arg.trim().is_empty());
    let allowed = !has_args
      || crate::cloud_auth::CLOUD_AUTH
        .can_use_browser_automation()
        .await;
    validate_launch_args(&self.launch_args, allowed)
  }

  /// For imports: keep only arguments this account may store, or none.
  pub async fn sanitize_launch_args(&mut self) {
    match self.checked_launch_args().await {
      Ok(args) => self.launch_args = args,
      Err(error) => {
        log::warn!("Imported launch arguments dropped err=\"{error}\"");
        self.launch_args.clear();
      }
    }
  }
}

pub const MAX_LAUNCH_ARGS: usize = 64;
pub const MAX_LAUNCH_ARG_CHARS: usize = 1024;

#[derive(Clone, Copy)]
enum SwitchValue {
  Flag,
  OneOf(&'static [&'static str]),
  Count(u32, u32),
  Features,
}

// Arguments reach a profile from team members, sync storage and archives, so
// only switches that cannot run programs, weaken the sandbox or change what
// Donut controls are accepted.
const ALLOWED_LAUNCH_SWITCHES: &[(&str, SwitchValue)] = &[
  (
    "--autoplay-policy",
    SwitchValue::OneOf(&[
      "no-user-gesture-required",
      "user-gesture-required",
      "document-user-activation-required",
    ]),
  ),
  ("--disable-background-networking", SwitchValue::Flag),
  (
    "--disable-backgrounding-occluded-windows",
    SwitchValue::Flag,
  ),
  ("--disable-default-apps", SwitchValue::Flag),
  ("--disable-features", SwitchValue::Features),
  ("--disable-gpu", SwitchValue::Flag),
  ("--disable-gpu-compositing", SwitchValue::Flag),
  ("--disable-hang-monitor", SwitchValue::Flag),
  ("--disable-ipc-flooding-protection", SwitchValue::Flag),
  ("--disable-notifications", SwitchValue::Flag),
  ("--disable-pinch", SwitchValue::Flag),
  ("--disable-popup-blocking", SwitchValue::Flag),
  ("--disable-print-preview", SwitchValue::Flag),
  ("--disable-renderer-backgrounding", SwitchValue::Flag),
  ("--disable-session-crashed-bubble", SwitchValue::Flag),
  ("--disable-smooth-scrolling", SwitchValue::Flag),
  ("--disable-software-rasterizer", SwitchValue::Flag),
  ("--enable-features", SwitchValue::Features),
  ("--enable-gpu-rasterization", SwitchValue::Flag),
  ("--enable-smooth-scrolling", SwitchValue::Flag),
  ("--enable-zero-copy", SwitchValue::Flag),
  ("--force-dark-mode", SwitchValue::Flag),
  ("--hide-scrollbars", SwitchValue::Flag),
  ("--ignore-gpu-blocklist", SwitchValue::Flag),
  ("--mute-audio", SwitchValue::Flag),
  ("--num-raster-threads", SwitchValue::Count(1, 16)),
  ("--process-per-site", SwitchValue::Flag),
  ("--renderer-process-limit", SwitchValue::Count(1, 64)),
];

// Switches Donut sets or depends on; named in the refusal so the user knows why.
const DONUT_MANAGED_SWITCHES: &[&str] = &[
  "--accept-lang",
  "--crash-server-url",
  "--disable-extensions-except",
  "--disk-cache-dir",
  "--disk-cache-size",
  "--enable-logging",
  "--headless",
  "--host-resolver-rules",
  "--host-rules",
  "--kiosk",
  "--lang",
  "--load-extension",
  "--log-file",
  "--log-level",
  "--no-proxy-server",
  "--no-sandbox",
  "--password-store",
  "--profile-directory",
  "--restore-last-session",
  "--start-fullscreen",
  "--start-maximized",
  "--type",
  "--use-mock-keychain",
  "--user-agent",
  "--user-data-dir",
  "--window-position",
  "--window-size",
];

// Lowercase fragments of feature names that touch privacy, the network path,
// the sandbox or fingerprint surfaces.
const BLOCKED_FEATURE_FRAGMENTS: &[&str] = &[
  "automation",
  "battery",
  "bluetooth",
  "canvas",
  "certificate",
  "clienthint",
  "cookie",
  "devtools",
  "dns",
  "extension",
  "fingerprint",
  "font",
  "gamepad",
  "geolocation",
  "headless",
  "hid",
  "incognito",
  "insecure",
  "isolation",
  "language",
  "locale",
  "partition",
  "prefetch",
  "privacy",
  "privatenetwork",
  "proxy",
  "remote",
  "sandbox",
  "sensor",
  "serial",
  "storage",
  "sync",
  "thirdparty",
  "timezone",
  "tracking",
  "unsafe",
  "update",
  "usb",
  "useragent",
  "wayfern",
  "webgl",
  "webgpu",
  "webrtc",
];

fn feature_list_allowed(value: &str) -> bool {
  !value.is_empty()
    && value.split(',').all(|feature| {
      let lower = feature.to_ascii_lowercase();
      feature.starts_with(|c: char| c.is_ascii_alphabetic())
        && feature.chars().all(|c| c.is_ascii_alphanumeric())
        && !BLOCKED_FEATURE_FRAGMENTS
          .iter()
          .any(|fragment| lower.contains(fragment))
    })
}

fn launch_arg_error(code: &str, name: &str) -> String {
  serde_json::json!({ "code": code, "params": { "argument": name } }).to_string()
}

fn validate_launch_args(args: &[String], allowed: bool) -> Result<Vec<String>, String> {
  let args: Vec<&str> = args
    .iter()
    .map(|arg| arg.trim())
    .filter(|arg| !arg.is_empty())
    .collect();
  if args.is_empty() {
    return Ok(Vec::new());
  }
  if !allowed {
    return Err(crate::backend_error("WAYFERN_LAUNCH_ARGS_REQUIRES_PRO"));
  }
  if args.len() > MAX_LAUNCH_ARGS
    || args
      .iter()
      .any(|arg| arg.chars().count() > MAX_LAUNCH_ARG_CHARS)
  {
    return Err(
      serde_json::json!({
        "code": "WAYFERN_LAUNCH_ARGS_LIMIT",
        "params": {
          "count": MAX_LAUNCH_ARGS.to_string(),
          "length": MAX_LAUNCH_ARG_CHARS.to_string(),
        }
      })
      .to_string(),
    );
  }
  let mut result = Vec::new();
  for arg in args {
    let (name, value) = match arg.split_once('=') {
      Some((name, value)) => (name, Some(value)),
      None => (arg, None),
    };
    let valid_name = name.strip_prefix("--").is_some_and(|name| {
      name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
          .chars()
          .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    });
    if !valid_name || arg.chars().any(char::is_control) {
      return Err(crate::backend_error("WAYFERN_LAUNCH_ARG_INVALID"));
    }
    let Some((_, kind)) = ALLOWED_LAUNCH_SWITCHES
      .iter()
      .find(|(allowed, _)| *allowed == name)
    else {
      let managed = DONUT_MANAGED_SWITCHES.contains(&name)
        || name.starts_with("--remote-debugging-")
        || name.starts_with("--proxy-")
        || name.starts_with("--wayfern-");
      return Err(launch_arg_error(
        if managed {
          "WAYFERN_LAUNCH_ARG_RESERVED"
        } else {
          "WAYFERN_LAUNCH_ARG_NOT_ALLOWED"
        },
        name,
      ));
    };
    let value_ok = match (kind, value) {
      (SwitchValue::Flag, None) => true,
      (SwitchValue::OneOf(choices), Some(value)) => choices.contains(&value),
      (SwitchValue::Count(min, max), Some(value)) => value
        .parse::<u32>()
        .is_ok_and(|count| (*min..=*max).contains(&count)),
      (SwitchValue::Features, Some(value)) => feature_list_allowed(value),
      _ => false,
    };
    if !value_ok {
      return Err(launch_arg_error("WAYFERN_LAUNCH_ARG_VALUE_INVALID", name));
    }
    result.push(arg.to_string());
  }
  Ok(result)
}

// Prefetch* / NoStatePrefetch: cross-site Speculation-Rules prefetch uses an
// isolated NetworkContext that defaults to DIRECT egress (real host IP leaks
// past the per-profile proxy). Disabling via a LAUNCH FLAG cannot be re-enabled
// by an imported/synced network_prediction_options pref (which a compile-time
// pref default could be).
const LEAK_FEATURES_SWITCH: &str = "--disable-features=DialMediaRouteProvider,DnsOverHttps,AsyncDns,Prefetch,PrefetchProxy,SpeculationRulesPrefetchFuture,NoStatePrefetch";

/// The switches that send a browser through Donut's local proxy. SOCKS5 lets
/// Chromium route UDP (QUIC/WebRTC) and resolve DNS through the proxy; PROXY
/// is HTTP CONNECT (TCP only). The host:port is the same either way.
fn proxy_switches(proxy: &str) -> Vec<String> {
  let (pac_directive, host_port) = if let Some(rest) = proxy.strip_prefix("socks5://") {
    ("SOCKS5", rest)
  } else {
    (
      "PROXY",
      proxy
        .trim_start_matches("http://")
        .trim_start_matches("https://"),
    )
  };
  let pac_data = format!(
    "data:application/x-ns-proxy-autoconfig,function FindProxyForURL(url,host){{return \"{pac_directive} {host_port}\";}}",
  );
  vec![
    format!("--proxy-pac-url={pac_data}"),
    "--dns-prefetch-disable".to_string(),
  ]
}

fn apply_launch_args(args: &mut Vec<String>, custom: &[String]) {
  for arg in custom {
    let name = arg.split('=').next().unwrap_or_default();
    if name == "--disable-features" || name == "--enable-features" {
      let mut features = Vec::new();
      for value in args.iter().chain(std::iter::once(arg)) {
        if let Some((key, value)) = value.split_once('=') {
          if key == name {
            for feature in value.split(',').filter(|feature| !feature.is_empty()) {
              if !features.contains(&feature) {
                features.push(feature);
              }
            }
          }
        }
      }
      let merged = format!("{name}={}", features.join(","));
      args.retain(|existing| existing.split('=').next() != Some(name));
      args.push(merged);
    } else {
      args.retain(|existing| existing.split('=').next() != Some(name));
      args.push(arg.clone());
    }
  }
}

/// Whether the major version in `version` is at least `minimum`. An
/// unparsable version reads as 0, which is the older path.
fn at_least_major(version: &str, minimum: &str) -> bool {
  let major = |text: &str| -> u32 {
    text
      .split('.')
      .next()
      .and_then(|part| part.trim().parse().ok())
      .unwrap_or(0)
  };
  major(version) >= major(minimum)
}

/// First Wayfern version that ships the 152 launch contract: the identity file
/// (`--wayfern-identity-file`), the `--wayfern-token` switch, WebRTC mode and
/// exit IP, persona/icon/camera/entitlement-cache switches, the
/// `Vellum` input domain and the perception/locator/extraction commands.
/// Everything gated on it keeps its pre-152 path on any older browser.
const WAYFERN_152_MIN_VERSION: &str = "152";

/// Whether `version` speaks the Wayfern 152 launch and automation contract.
/// Same rule as [`supports_identity_api`]: pass `BrowserProfile::version`, read
/// it at the point of use, and an unparsable version takes the older path.
pub fn supports_wayfern_152(version: &str) -> bool {
  at_least_major(version, WAYFERN_152_MIN_VERSION)
}

/// What kind of launch this is, from the browser's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchKind {
  /// A person opened the profile from the app: the window is theirs, and the
  /// session they left is the one they expect to find again.
  Interactive,
  /// REST, MCP or a batch run drives the browser: it starts clean on the URL
  /// the caller named and never reopens what a person left behind.
  Automation,
}

/// Whether this launch reopens the last session, or why it does not.
///
/// `identity_at_launch` says the device is committed before the first
/// navigation (the 152 identity file), or that there is no device to commit.
/// Anything applied over CDP after the window opens loses the race with a
/// restored tab, which then carries the host device for its whole lifetime,
/// so such a launch starts on a fresh tab instead and the log says why.
pub fn session_restore_verdict(
  config: &WayfernConfig,
  kind: LaunchKind,
  headless: bool,
  ephemeral: bool,
  clear_on_close: bool,
  identity_at_launch: bool,
  has_start_pages: bool,
) -> Result<(), &'static str> {
  if kind == LaunchKind::Automation {
    return Err("an automation run starts clean");
  }
  // As in Chrome: a set of start pages replaces the last session, also on a
  // launch that opens a given URL. Restoring would add the start pages to
  // the restored tabs on every launch, or bring them back with the URL.
  if has_start_pages {
    return Err("the profile starts on its start pages, not its last session");
  }
  if headless {
    return Err("a headless launch has no session to show");
  }
  if ephemeral {
    return Err("an ephemeral profile keeps nothing between launches");
  }
  if clear_on_close {
    return Err("the profile clears its data on close");
  }
  if config.randomize_fingerprint_on_launch == Some(true) {
    return Err("a device randomized on every launch has no session to continue");
  }
  if config.restore_session == Some(false) {
    return Err("the profile has session restore switched off");
  }
  if !identity_at_launch {
    return Err(
      "this browser applies the identity after the window opens, so a restored tab would load on the host device",
    );
  }
  Ok(())
}

/// What a launch opens: the URL it was given, or else, for a person's own
/// launch, the profile's start pages. An automation run without a URL starts
/// on a blank tab, as its caller expects.
pub fn launch_pages<'a>(
  url: Option<&'a str>,
  start_urls: &'a [String],
  kind: LaunchKind,
) -> Vec<&'a str> {
  match (url, kind) {
    (Some(url), _) => vec![url],
    (None, LaunchKind::Interactive) => start_urls.iter().map(String::as_str).collect(),
    (None, LaunchKind::Automation) => Vec::new(),
  }
}

/// The WebRTC posture a 152 browser is launched with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebRtcMode {
  Auto,
  TcpOnly,
  Block,
}

impl WebRtcMode {
  pub fn parse(value: &str) -> Option<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "auto" => Some(Self::Auto),
      "tcp_only" | "tcp-only" | "tcponly" => Some(Self::TcpOnly),
      "block" | "blocked" | "off" => Some(Self::Block),
      _ => None,
    }
  }

  /// The mode a stored config asks for. `webrtc_mode` wins; the legacy
  /// `block_webrtc: true` reads as `block`; anything else is `auto`. An
  /// unknown string is `auto` too, logged by the caller, never a launch error.
  pub fn from_config(config: &WayfernConfig) -> Self {
    if let Some(mode) = config.webrtc_mode.as_deref().and_then(Self::parse) {
      return mode;
    }
    if config.block_webrtc == Some(true) {
      return Self::Block;
    }
    Self::Auto
  }

  pub fn switch_value(self) -> &'static str {
    match self {
      Self::Auto => "auto",
      Self::TcpOnly => "tcp_only",
      Self::Block => "block",
    }
  }
}

/// The primary display's size in CSS pixels, which is what a page reads.
pub fn host_screen_size(app_handle: &AppHandle) -> Option<(u32, u32)> {
  let monitor = app_handle.primary_monitor().ok().flatten()?;
  let scale = monitor.scale_factor();
  let size = monitor.size().to_logical::<f64>(scale);
  if size.width < 1.0 || size.height < 1.0 {
    return None;
  }
  Some((size.width as u32, size.height as u32))
}

pub fn cdp_error_message(detail: &str) -> String {
  if let Some((_, rest)) = detail.split_once("CDP error: ") {
    if let Ok(error) = serde_json::from_str::<serde_json::Value>(rest.trim()) {
      if let Some(message) = error.get("message").and_then(|m| m.as_str()) {
        return message.to_string();
      }
    }
  }
  detail.trim().to_string()
}

pub fn known_wayfern_failure(detail: &str, claimed_os: Option<&str>) -> Option<String> {
  let message = cdp_error_message(detail);
  let says = |needle: &str| detail.contains(needle) || message.contains(needle);
  if says("Wayfern instance limit reached") {
    return Some(crate::backend_error("WAYFERN_INSTANCE_LIMIT_REACHED"));
  }
  if says("Before using Wayfern, you must read and agree") {
    return Some(crate::backend_error("WAYFERN_TERMS_REQUIRED"));
  }
  if says("temporarily unavailable") {
    return Some(crate::backend_error("WAYFERN_PLAN_CHECK_UNAVAILABLE"));
  }
  if says("Cross-OS fingerprinting requires")
    || says("Cross-OS fingerprinting authorization failed")
    || says("a cross-OS claim requires")
  {
    return Some(crate::backend_error_with_detail(
      "WAYFERN_CROSS_OS_REQUIRES_PLAN",
      claimed_os.unwrap_or("another operating system"),
    ));
  }
  if says("Custom fingerprints require") {
    return Some(crate::backend_error(
      "WAYFERN_CUSTOM_FINGERPRINT_REQUIRES_PLAN",
    ));
  }
  if says("generation limit reached") || says("Too many profiles") {
    return Some(crate::backend_error("WAYFERN_GENERATION_LIMIT_REACHED"));
  }
  if says("Fingerprint generation state is unavailable") {
    return Some(crate::backend_error("WAYFERN_GENERATION_UNAVAILABLE"));
  }
  if says("identity change is in progress")
    || says("Target closed during fingerprint identity change")
    || says("another identity change already owns this profile")
    || says("the identity could not be derived")
    || says("the derived identity was not committed")
  {
    return Some(crate::backend_error("WAYFERN_BROWSER_BUSY"));
  }
  None
}

pub fn wayfern_failure(detail: &str, fallback_code: &str, claimed_os: Option<&str>) -> String {
  if detail.trim_start().starts_with('{') {
    return detail.to_string();
  }
  known_wayfern_failure(detail, claimed_os)
    .unwrap_or_else(|| crate::backend_error_with_detail(fallback_code, cdp_error_message(detail)))
}

/// The last lines a browser printed before it quit, redacted, for one log
/// line.
fn last_said(lines: &[String]) -> String {
  let start = lines.len().saturating_sub(2);
  lines[start..]
    .iter()
    .map(|line| {
      crate::log_redaction::text(line)
        .chars()
        .take(300)
        .collect::<String>()
    })
    .collect::<Vec<_>>()
    .join(" | ")
}

fn early_exit_error(said: &str, status: &std::process::ExitStatus) -> String {
  known_wayfern_failure(said, None).unwrap_or_else(|| {
    crate::backend_error_with_detail("WAYFERN_BROWSER_EXITED", status.to_string())
  })
}

/// The last lines the browser wrote about Wayfern itself.
///
/// A refusal of the launch identity is logged by the browser and never fatal
/// to it (it keeps the device it would have used anyway), so the launcher has
/// to read the verdict off stderr to turn it into an error the user sees.
#[derive(Clone, Default)]
pub struct BrowserLogTap(Arc<std::sync::Mutex<VecDeque<String>>>);

impl BrowserLogTap {
  const CAPACITY: usize = 32;

  pub fn push(&self, line: String) {
    let mut lines = self.0.lock().unwrap_or_else(|e| e.into_inner());
    if lines.len() >= Self::CAPACITY {
      lines.pop_front();
    }
    lines.push_back(line);
  }

  pub fn lines(&self) -> Vec<String> {
    self
      .0
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .iter()
      .cloned()
      .collect()
  }
}

/// The level a browser stderr line is logged at, or `None` to drop it. Only
/// what the browser says about Wayfern, its own errors, and its cookie key
/// (`os_crypt`) are kept.
fn browser_line_level(line: &str) -> Option<log::Level> {
  if line.contains("os_crypt") {
    // The browser could not read or write the key that seals cookies: the
    // profile is about to lose its sign-ins.
    return Some(log::Level::Warn);
  }
  // Chromium's own errors are kept too: a browser that quits before its
  // control port opens says why only there.
  if line.contains("Wayfern")
    || line.contains("wayfern_")
    || line.starts_with("apostate:")
    || line.contains(":ERROR:")
    || line.contains(":FATAL:")
  {
    // What the browser did with the identity explains a profile that
    // launched without one; the rest is detail.
    return Some(if line.contains("Wayfern launch identity") {
      log::Level::Info
    } else {
      log::Level::Debug
    });
  }
  None
}

/// One `os_crypt` line per profile a minute: a browser that cannot open its
/// key says so for every cookie.
static OS_CRYPT_LINES: crate::log_streak::Throttle =
  crate::log_streak::Throttle::new(Duration::from_secs(60));

/// Keep reading the browser's stderr for its lifetime, keeping only what it
/// says about Wayfern. Reading it all is what keeps the pipe from filling.
/// `owner` is `key=value` text naming whose browser this is.
fn tap_browser_stderr(stderr: tokio::process::ChildStderr, tap: BrowserLogTap, owner: String) {
  tauri::async_runtime::spawn(async move {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
      let Some(level) = browser_line_level(&line) else {
        continue;
      };
      let text = crate::log_redaction::text(&line);
      if level == log::Level::Warn {
        let key = owner.split_whitespace().next().unwrap_or_default();
        if let Some(dropped) = OS_CRYPT_LINES.allow(key) {
          log::warn!(
            "Browser cookie key error {owner} line=\"{}\"{}",
            crate::log_redaction::Plain(&text),
            crate::log_streak::suppressed(dropped)
          );
        }
      } else {
        log::log!(
          level,
          "Browser said {owner} line=\"{}\"",
          crate::log_redaction::Plain(&text)
        );
      }
      tap.push(line);
    }
  });
}

/// The profile id inside a profile data path (`profiles/<uuid>/profile`, or an
/// ephemeral `<base>/<uuid>`), so a line names the profile and not the path.
fn profile_id_in_path(path: &str) -> String {
  Path::new(path)
    .components()
    .rev()
    .filter_map(|component| component.as_os_str().to_str())
    .find(|component| uuid::Uuid::parse_str(component).is_ok())
    .unwrap_or("unknown")
    .to_string()
}

/// How a browser process ended up stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
  /// It shut itself down after `Browser.close`, so its session files are
  /// complete and the next launch can restore them.
  Closed,
  /// It exited on a termination request.
  Terminated,
  /// It had to be killed outright.
  Killed,
  /// Nothing this function did stopped it.
  StillRunning,
}

impl std::fmt::Display for StopOutcome {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      Self::Closed => "closed",
      Self::Terminated => "terminated",
      Self::Killed => "killed",
      Self::StillRunning => "still_running",
    })
  }
}

/// Ask a process to exit: SIGTERM, which Chromium handles as a normal
/// shutdown, or `taskkill` without `/F`, which only reaches a process with a
/// window. The caller escalates when this is not enough.
fn terminate_process(pid: u32) {
  #[cfg(unix)]
  {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
  }
  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let _ = std::process::Command::new("taskkill")
      .args(["/PID", &pid.to_string()])
      .creation_flags(CREATE_NO_WINDOW)
      .output();
  }
}

/// End a process without asking.
fn force_kill_process(pid: u32) {
  #[cfg(unix)]
  {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
  }
  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let _ = std::process::Command::new("taskkill")
      .args(["/PID", &pid.to_string(), "/F"])
      .creation_flags(CREATE_NO_WINDOW)
      .output();
  }
}

/// Wait up to `limit` for `pid` to leave the process table.
async fn wait_for_exit(pid: u32, limit: Duration) -> bool {
  let started = std::time::Instant::now();
  loop {
    if !crate::proxy_storage::is_process_running(pid) {
      return true;
    }
    if started.elapsed() >= limit {
      return false;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
}

/// Fingerprint fields the browser takes as dedicated parameters rather than as
/// overrides. They describe the exit IP, so they travel through their own
/// channel instead of being duplicated into `overrides`.
const GEO_PARAM_KEYS: [&str; 4] = ["timezone", "language", "latitude", "longitude"];

/// Fields the browser refuses in `overrides`. It rejects the entire call when
/// one appears, so they must never diff into the override set — that would be a
/// permanent, every-launch failure rather than a one-off.
const DERIVED_PROVENANCE_KEYS: [&str; 3] =
  ["webglProfileId", "mediaProfile", "deviceProfileApplied"];

/// Location fields donutbrowser owns end to end (`apply_geolocation` writes all
/// of them). Any of these the browser does not echo back after an identity is
/// applied is refilled from the stored fingerprint, so what donut persists
/// always carries the location the launch gate reads.
const LOCALE_CARRY_OVER_KEYS: [&str; 7] = [
  "timezone",
  "timezoneOffset",
  "language",
  "languages",
  "latitude",
  "longitude",
  "accuracy",
];

/// How the geolocation probe reaches this profile's exit.
///
/// Every variant either genuinely carries the traffic or refuses. There is no
/// "try it and see" arm on purpose: a probe that does not cross the profile's
/// upstream resolves THIS MACHINE'S address, and its location was then written
/// into the fingerprint as the exit's.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeRoute {
  /// `reqwest` proxies this scheme itself. Carries the rewritten URL, so
  /// `httpstls` is already the `https` reqwest understands.
  Reqwest(String),
  /// A temporary local `donut-proxy` worker dials this upstream. Carries the
  /// URL the WORKER should dial, which is not always the stored one.
  Worker(String),
  /// The upstream is an Xray share link (VLESS, VMess, Trojan, Hysteria2),
  /// which no `donut-proxy` worker can speak. An Xray-core sidecar carries the
  /// hop and a `donut-proxy` worker fronts its loopback SOCKS5 endpoint, the
  /// same two-stage path `browser_runner` builds for such a launch. Carries the
  /// share link.
  Xray(String),
  /// Nothing available here carries this upstream. The probe is skipped and
  /// the fingerprint keeps no location at all, which is the only honest
  /// outcome: a location that is neither the user's nor the exit's is worse
  /// than none.
  Unroutable,
}

/// A probe transport built for one fingerprint generation, plus the temporary
/// workers that have to be stopped once the probe is done.
#[derive(Default)]
struct ProbeTransport {
  /// The proxy URL to hand `reqwest`, or `None` when nothing could carry the
  /// probe and it must be skipped rather than sent unproxied.
  proxy: Option<String>,
  donut_worker_id: Option<String>,
  xray_worker_id: Option<String>,
}

impl ProbeTransport {
  /// Stop everything this transport started. The `donut-proxy` worker goes
  /// first because it is the one holding connections open through the Xray
  /// sidecar behind it.
  async fn shutdown(self) {
    if let Some(id) = self.donut_worker_id {
      let _ = crate::proxy_runner::stop_proxy_process_because(&id, "geo_probe_done").await;
    }
    if let Some(id) = self.xray_worker_id {
      let _ = crate::xray_worker_runner::stop_xray_worker(&id).await;
    }
  }
}

/// A freshly generated device, plus its identity handle when the browser
/// supports identities.
pub struct GeneratedFingerprint {
  /// The device the browser produced, as a flat camelCase JSON object. For a
  /// LEGACY browser this is what `WayfernConfig::fingerprint` stores. For an
  /// identity-backed profile it is a VIEW for the caller to show once and
  /// discard: only `identity_id` and `location` are persisted.
  pub fingerprint: String,
  pub identity_id: Option<String>,
  /// `WayfernConfig::location` for the exit this device was generated
  /// against, or `None` when no location field was resolved.
  pub location: Option<String>,
  /// Whether fresh geolocation was resolved and applied. Callers must only
  /// stamp `geo_proxy_signature` when this is true.
  pub geolocation_applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct WayfernLaunchResult {
  pub id: String,
  #[serde(alias = "process_id")]
  pub processId: Option<u32>,
  #[serde(alias = "profile_path")]
  pub profilePath: Option<String>,
  pub url: Option<String>,
  pub cdp_port: Option<u16>,
}

/// How long a started browser has to open its DevTools port.
const CDP_READY_LIMIT: Duration = Duration::from_secs(90);
/// How long one DevTools command may take to answer.
const CDP_COMMAND_LIMIT: Duration = Duration::from_secs(60);

struct WayfernInstance {
  process_id: Option<u32>,
  profile_path: Option<String>,
  url: Option<String>,
  cdp_port: Option<u16>,
  /// What the browser said about Wayfern on stderr, for diagnostics. Read
  /// through `browser_log_lines`, which the WebRTC and persona launch checks
  /// consult; nothing else needs it yet.
  #[allow(dead_code)]
  log_tap: BrowserLogTap,
}

struct WayfernManagerInner {
  instances: HashMap<String, WayfernInstance>,
}

pub struct WayfernManager {
  inner: Arc<AsyncMutex<WayfernManagerInner>>,
  http_client: Client,
}

#[derive(Debug, Deserialize)]
struct CdpTarget {
  #[serde(rename = "type")]
  target_type: String,
  #[serde(rename = "webSocketDebuggerUrl")]
  websocket_debugger_url: Option<String>,
}

impl WayfernManager {
  fn new() -> Self {
    Self {
      inner: Arc::new(AsyncMutex::new(WayfernManagerInner {
        instances: HashMap::new(),
      })),
      // CDP is always on loopback. Disable env/system proxies so a Windows
      // WinHTTP/IE proxy (or HTTP_PROXY) cannot intercept /json/version and
      // return 502 Bad Gateway while the browser is actually listening.
      http_client: Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
        .expect("Failed to build reqwest client for wayfern_manager"),
    }
  }

  pub fn instance() -> &'static WayfernManager {
    &WAYFERN_MANAGER
  }

  #[allow(dead_code)]
  pub fn get_profiles_dir(&self) -> PathBuf {
    crate::app_dirs::profiles_dir()
  }

  #[allow(dead_code)]
  fn get_binaries_dir(&self) -> PathBuf {
    crate::app_dirs::binaries_dir()
  }

  pub(crate) async fn find_free_port() -> Result<u16, Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
  }

  /// Denormalize fingerprint data from our storage format to Wayfern CDP format.
  /// Wayfern expects certain fields as JSON strings.
  fn denormalize_fingerprint(fingerprint: serde_json::Value) -> serde_json::Value {
    // Our storage format matches what Wayfern expects:
    // - fonts, plugins, mimeTypes, voices are JSON strings
    // - webglParameters, webgl2Parameters, etc. are JSON strings
    // So no conversion is needed
    fingerprint
  }

  /// The fingerprint value a stored `WayfernConfig::fingerprint` string holds:
  /// the object itself, or the one nested in the legacy
  /// `{ "fingerprint": {...} }` wrapper some old profiles carry.
  ///
  /// The single place that shape is resolved. Everything that reads a stored
  /// fingerprint goes through this or through [`Self::fingerprint_object`], so
  /// no two readers can end up disagreeing about which shapes count.
  fn unwrap_stored_fingerprint(stored: &serde_json::Value) -> &serde_json::Value {
    stored.get("fingerprint").unwrap_or(stored)
  }

  /// Parse a stored fingerprint JSON into its object, tolerating the legacy
  /// `{ "fingerprint": {...} }` wrapper some old profiles carry.
  ///
  /// Shared with `fingerprint_consistency`, which reads the timezone and
  /// language it compares against the measured exit through this exact
  /// accessor. Two readers with their own idea of the stored shape is how the
  /// gate came to report "this profile declares no timezone" for a wrapped
  /// fingerprint whose launch presented the timezone nested one level down.
  pub fn fingerprint_object(
    fingerprint_json: &str,
  ) -> Option<serde_json::Map<String, serde_json::Value>> {
    let parsed: serde_json::Value = serde_json::from_str(fingerprint_json).ok()?;
    Self::unwrap_stored_fingerprint(&parsed)
      .as_object()
      .cloned()
  }

  /// The device this launch hands the browser, derived from what the profile
  /// stores. Pure, and the only place that payload is built, so what the
  /// browser is actually given can be asserted without one running.
  ///
  /// It never invents a field. A stored fingerprint that declares no timezone
  /// produces a payload with no timezone, and the engine keeps whatever it
  /// reports natively. The launcher used to insert `America/New_York` and
  /// offset 300 here, which put a US clock behind whatever exit the profile
  /// routed through, while `fingerprint_consistency`, reading the same stored
  /// fingerprint, told the user its timezone had never been compared. That is
  /// the one combination that must never happen: the app cannot claim it
  /// compared nothing while shipping a location it made up.
  pub(crate) fn launch_fingerprint_payload(
    fingerprint_json: &str,
  ) -> Result<serde_json::Value, String> {
    let stored: serde_json::Value = serde_json::from_str(fingerprint_json)
      .map_err(|e| format!("Failed to parse stored fingerprint JSON: {e}"))?;

    // Denormalize for Wayfern CDP (arrays/objects travel as JSON strings).
    let mut payload =
      Self::denormalize_fingerprint(Self::unwrap_stored_fingerprint(&stored).clone());

    // Normalize languages: a comma-separated string becomes the array the
    // browser expects.
    if let Some(obj) = payload.as_object_mut() {
      if let Some(serde_json::Value::String(s)) = obj.get("languages").cloned() {
        let arr: Vec<&str> = s.split(',').map(|l| l.trim()).collect();
        obj.insert("languages".to_string(), json!(arr));
      }
    }

    Ok(payload)
  }

  /// A stored JSON object field (`identity_overrides`, `location`), or an
  /// empty map when absent or unparsable.
  pub fn stored_object(json: Option<&str>) -> serde_json::Map<String, serde_json::Value> {
    json.and_then(Self::fingerprint_object).unwrap_or_default()
  }

  /// The exit-derived location fields a device object carries, in the shape
  /// `WayfernConfig::location` stores; `None` when it carries none.
  pub fn location_of(device: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let mut location = serde_json::Map::new();
    for key in LOCALE_CARRY_OVER_KEYS {
      if let Some(value) = device.get(key) {
        if !value.is_null() {
          location.insert(key.to_string(), value.clone());
        }
      }
    }
    if location.is_empty() {
      None
    } else {
      serde_json::to_string(&location).ok()
    }
  }

  /// Overrides from a WHOLE fingerprint an API or MCP caller supplied for an
  /// identity-backed profile: every field it names is taken as an explicit
  /// edit, except the provenance keys the browser refuses and the location
  /// keys, which travel through `location`.
  pub fn overrides_from_explicit_fingerprint(
    fingerprint: &serde_json::Map<String, serde_json::Value>,
  ) -> serde_json::Map<String, serde_json::Value> {
    let mut overrides = serde_json::Map::new();
    for (key, value) in fingerprint {
      if DERIVED_PROVENANCE_KEYS.contains(&key.as_str())
        || GEO_PARAM_KEYS.contains(&key.as_str())
        || LOCALE_CARRY_OVER_KEYS.contains(&key.as_str())
        || value.is_null()
      {
        continue;
      }
      overrides.insert(key.clone(), value.clone());
    }
    overrides
  }

  /// The offset of `timezone` from UTC in minutes, in the sign convention
  /// `Date.prototype.getTimezoneOffset` uses (positive west of UTC), or `None`
  /// when the IANA name does not parse.
  ///
  /// It reads the offset AT THE CURRENT INSTANT, so a zone that observes DST
  /// answers differently either side of a transition. That is what a browser
  /// reports too, and it is why `apply_geolocation` and `identity_overrides`
  /// share this one implementation instead of each computing their own.
  fn timezone_offset_minutes(timezone: &str) -> Option<i32> {
    use chrono::Offset;
    let tz = timezone.parse::<chrono_tz::Tz>().ok()?;
    let offset_seconds = chrono::Utc::now()
      .with_timezone(&tz)
      .offset()
      .fix()
      .local_minus_utc();
    Some(-(offset_seconds / 60))
  }

  /// The bare language subtag of a BCP 47 tag (`de-DE` -> `de`), which is the
  /// second entry of the `languages` ladder donut builds. `Locale::language` is
  /// itself the first `-`-separated part of the tag, so this reproduces it.
  fn base_language(locale: &str) -> &str {
    locale.split('-').next().unwrap_or(locale)
  }

  /// One of Wayfern's five `operatingSystem` names, or `None` for anything
  /// else. Unknown names are not guessed at: the caller treats `None` as "donut
  /// does not know what this profile claims" and lets the browser decide.
  pub(crate) fn normalize_os_name(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
      "windows" => Some("windows"),
      "macos" => Some("macos"),
      "linux" => Some("linux"),
      "android" => Some("android"),
      "ios" => Some("ios"),
      _ => None,
    }
  }

  async fn wait_for_cdp_ready_while_running(
    &self,
    port: u16,
    child: &mut tokio::process::Child,
    log: &BrowserLogTap,
  ) -> Result<Vec<CdpTarget>, Box<dyn std::error::Error + Send + Sync>> {
    self
      .wait_for_cdp_ready_within(port, child, log, CDP_READY_LIMIT)
      .await
  }

  async fn wait_for_cdp_ready_within(
    &self,
    port: u16,
    child: &mut tokio::process::Child,
    log: &BrowserLogTap,
    limit: Duration,
  ) -> Result<Vec<CdpTarget>, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let delay = Duration::from_millis(500);
    // A refused connection can take seconds on Windows, so the wait is bounded
    // by time, not by a count of tries.
    let deadline = std::time::Instant::now() + limit;

    let pid = child.id().unwrap_or_default();
    let mut last_error: Option<String> = None;
    let mut attempt = 0u32;
    while std::time::Instant::now() < deadline {
      attempt += 1;
      if let Ok(Some(status)) = child.try_wait() {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let lines = log.lines();
        let said = lines.join("\n");
        log::warn!(
          "Browser exited before CDP was ready pid={pid} cdp_port={port} status=\"{status}\" said=\"{}\"",
          crate::log_redaction::Plain(&last_said(&lines))
        );
        return Err(early_exit_error(&said, &status).into());
      }
      match self.http_client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
          log::debug!("CDP port open pid={pid} cdp_port={port} attempts={attempt}");
          return self
            .wait_for_page_targets_while_running(port, child, log)
            .await;
        }
        Ok(resp) => {
          last_error = Some(format!("HTTP {} from {url}", resp.status()));
          tokio::time::sleep(delay).await;
        }
        Err(e) => {
          last_error = Some(format!("request failed: {e}"));
          tokio::time::sleep(delay).await;
        }
      }
    }

    let detail = last_error.unwrap_or_else(|| "no attempts completed".to_string());
    log::warn!(
      "Browser did not answer CDP pid={pid} cdp_port={port} attempts={attempt} err=\"{detail}\""
    );
    Err(
      serde_json::json!({
        "code": "WAYFERN_NOT_RESPONDING",
        "params": {
          "seconds": limit.as_secs().to_string(),
          "detail": format!("CDP not ready on port {port}: {detail}"),
        }
      })
      .to_string()
      .into(),
    )
  }

  // The control port can open before the first tab. Both launch paths must
  // wait for a page socket before they apply or check the fingerprint.
  async fn wait_for_page_targets_while_running(
    &self,
    port: u16,
    child: &mut tokio::process::Child,
    log: &BrowserLogTap,
  ) -> Result<Vec<CdpTarget>, Box<dyn std::error::Error + Send + Sync>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let pid = child.id().unwrap_or_default();
    loop {
      if let Ok(Some(status)) = child.try_wait() {
        tokio::time::sleep(Duration::from_millis(300)).await;
        log::warn!(
          "Browser exited before a page was ready pid={pid} cdp_port={port} status=\"{status}\""
        );
        return Err(early_exit_error(&log.lines().join("\n"), &status).into());
      }
      let detail = match self.get_cdp_targets(port).await {
        Ok(mut targets) => {
          targets.retain(|target| {
            target.target_type == "page"
              && target
                .websocket_debugger_url
                .as_ref()
                .is_some_and(|url| !url.is_empty())
          });
          if !targets.is_empty() {
            return Ok(targets);
          }
          "the browser reported no page target with a socket".to_string()
        }
        Err(e) => e.to_string(),
      };
      if std::time::Instant::now() >= deadline {
        return Err(
          crate::backend_error_with_detail(
            "WAYFERN_FINGERPRINT_APPLY_FAILED",
            format!("No page target found for CDP: {detail}"),
          )
          .into(),
        );
      }
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
  }

  async fn get_cdp_targets(
    &self,
    port: u16,
  ) -> Result<Vec<CdpTarget>, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("http://127.0.0.1:{port}/json");
    let resp = self
      .http_client
      .get(&url)
      .send()
      .await?
      .error_for_status()?;
    let targets: Vec<CdpTarget> = resp.json().await?;
    Ok(targets)
  }

  pub(crate) async fn send_cdp_command(
    &self,
    ws_url: &str,
    method: &str,
    params: serde_json::Value,
  ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    match tokio::time::timeout(CDP_COMMAND_LIMIT, self.cdp_exchange(ws_url, method, params)).await {
      Ok(result) => result,
      Err(_) => Err(
        format!(
          "CDP command {method} got no answer in {} s",
          CDP_COMMAND_LIMIT.as_secs()
        )
        .into(),
      ),
    }
  }

  async fn cdp_exchange(
    &self,
    ws_url: &str,
    method: &str,
    params: serde_json::Value,
  ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws_stream, _) = connect_async(ws_url).await?;

    let command = json!({
      "id": 1,
      "method": method,
      "params": params
    });

    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;

    ws_stream
      .send(Message::Text(command.to_string().into()))
      .await?;

    while let Some(msg) = ws_stream.next().await {
      match msg? {
        Message::Text(text) => {
          let response: serde_json::Value = serde_json::from_str(text.as_str())?;
          if response.get("id") == Some(&json!(1)) {
            if let Some(error) = response.get("error") {
              return Err(format!("CDP error: {}", error).into());
            }
            return Ok(response.get("result").cloned().unwrap_or(json!({})));
          }
        }
        Message::Close(_) => break,
        _ => {}
      }
    }

    Err("No response received from CDP".into())
  }

  /// Stable signature describing what determines this profile's geolocation
  /// (timezone, latitude/longitude, language): the geoip mode first, then the
  /// VPN, the proxy, or a direct connection. Compared across creation and
  /// launch to detect a change. The VPN case keys off `vpn_id` rather than the
  /// per-launch local port, and the proxy case off type/host/port/username so
  /// that editing the proxy is also caught.
  pub fn geo_signature(
    proxy: Option<&crate::browser::ProxySettings>,
    vpn_id: Option<&str>,
    geoip: Option<&serde_json::Value>,
  ) -> String {
    // The "v2:" prefix invalidates every signature stamped before geolocation
    // failures stopped being stamped: those may describe fingerprints that
    // silently carry the host's location, so each pre-v2 profile gets one
    // launch-time refresh and is re-stamped in the current format.
    let base = match geoip {
      Some(serde_json::Value::Bool(false)) => "off".to_string(),
      Some(serde_json::Value::String(ip)) if !ip.is_empty() => format!("ip:{ip}"),
      _ => {
        if let Some(id) = vpn_id {
          format!("vpn:{id}")
        } else if let Some(p) = proxy {
          format!(
            "proxy:{}://{}@{}:{}",
            p.proxy_type.to_lowercase(),
            p.username.as_deref().unwrap_or(""),
            p.host,
            p.port
          )
        } else {
          "direct".to_string()
        }
      }
    };
    format!("v2:{base}")
  }

  /// Apply timezone/geolocation fields to a fingerprint object from the proxy's
  /// exit IP (or a fixed geoip IP). Mutates `fingerprint` in place. Returns true
  /// if fresh geolocation was fetched and applied, false if geolocation is
  /// disabled or could not be resolved (in which case only safe defaults are
  /// filled in). Shared by fingerprint generation and the launch-time refresh
  /// so both produce identical location data.
  async fn apply_geolocation(
    fingerprint: &mut serde_json::Value,
    proxy: Option<&str>,
    geoip: Option<&serde_json::Value>,
  ) -> bool {
    match Self::resolve_geolocation(proxy, geoip).await {
      Some(geo) => {
        Self::write_geolocation(fingerprint, &geo);
        true
      }
      None => false,
    }
  }

  /// The exit's location, or None when geolocation is off or the exit could
  /// not be resolved.
  async fn resolve_geolocation(
    proxy: Option<&str>,
    geoip: Option<&serde_json::Value>,
  ) -> Option<crate::geolocation::Geolocation> {
    // Default to auto-detect; only an explicit `false` disables geolocation.
    let should_geolocate = !matches!(geoip, Some(serde_json::Value::Bool(false)));
    if !should_geolocate {
      return None;
    }

    let geo_result = async {
      let ip = match geoip {
        Some(serde_json::Value::String(ip_str)) => ip_str.clone(),
        _ => crate::ip_utils::fetch_public_ip(proxy)
          .await
          .map_err(|e| format!("Failed to fetch public IP: {e}"))?,
      };
      crate::geolocation::get_geolocation(&ip)
        .map_err(|e| format!("Failed to get geolocation for IP {ip}: {e}"))
    }
    .await;

    match geo_result {
      Ok(geo) => Some(geo),
      Err(e) => {
        // NOTHING is written here, deliberately. A failed probe used to fill in
        // America/New_York and offset 300, which made "we could not resolve the
        // exit" indistinguishable from "the exit is in New York": the profile
        // then presented a US location as its proxy's, in the one field the
        // consistency gate and the user both read as authoritative. A location
        // that is neither the user's nor the exit's is worse than no location,
        // so the fields are left ungenerated.
        //
        // Returning false is what makes that recoverable: the caller must not
        // stamp `geo_proxy_signature`, so the launch-time refresh sees a
        // signature mismatch and probes again through the local worker the
        // browser is about to use.
        log::warn!(
          "Geolocation failed, fingerprint location left ungenerated err=\"{}\"",
          crate::log_redaction::text(&e.to_string())
        );
        None
      }
    }
  }

  /// Writes the exit's location into a fingerprint.
  fn write_geolocation(fingerprint: &mut serde_json::Value, geo: &crate::geolocation::Geolocation) {
    if let Some(obj) = fingerprint.as_object_mut() {
      obj.insert("timezone".to_string(), json!(geo.timezone));
      // Both derived fields go through the same helpers `identity_overrides`
      // uses to recognise them, so a value written here can never look like
      // a user edit to the override diff.
      if let Some(offset_minutes) = Self::timezone_offset_minutes(&geo.timezone) {
        obj.insert("timezoneOffset".to_string(), json!(offset_minutes));
      }
      obj.insert("latitude".to_string(), json!(geo.latitude));
      obj.insert("longitude".to_string(), json!(geo.longitude));
      let locale_str = geo.locale.as_string();
      obj.insert("language".to_string(), json!(&locale_str));
      obj.insert(
        "languages".to_string(),
        json!([&locale_str, Self::base_language(&locale_str)]),
      );
    }
    log::debug!("Geolocation applied to fingerprint");
  }

  /// Refresh ONLY the location fields (timezone, offset, latitude/longitude,
  /// language) of an already-generated fingerprint to match the current proxy,
  /// leaving every other fingerprint field untouched. `proxy` is the local
  /// proxy URL the browser will use. Returns the updated fingerprint JSON on
  /// success, or None if geolocation is disabled or could not be resolved, in
  /// which case the caller keeps the existing fingerprint and retries on the
  /// next launch.
  pub async fn refresh_fingerprint_geolocation(
    fingerprint_json: &str,
    proxy: Option<&str>,
    geoip: Option<&serde_json::Value>,
  ) -> Option<String> {
    let mut fp: serde_json::Value = serde_json::from_str(fingerprint_json).ok()?;
    if Self::apply_geolocation(&mut fp, proxy, geoip).await {
      serde_json::to_string(&fp).ok()
    } else {
      None
    }
  }

  /// True when `url` is a socks proxy on a remote (non-loopback) host — the
  /// case where reqwest's SOCKS connector can't be trusted with the
  /// geolocation fetch. Loopback socks URLs are the app's own donut-proxy
  /// workers, whose single-segment replies don't trigger the connector bug.
  /// Upstreams the geolocation probe must reach through a local donut-proxy
  /// worker instead of handing to `reqwest`.
  ///
  /// Two groups. Remote SOCKS, which reqwest could proxy but which this code has
  /// always routed through a worker. And EVERY scheme reqwest cannot proxy,
  /// whatever its host, that group is the dangerous one: `Proxy::all` ACCEPTS
  /// such a URL, then matches nothing, so the probe went out from the user's
  /// REAL address and its geolocation was written into the profile fingerprint.
  /// Nothing failed and nothing was logged: exactly the exit-vs-fingerprint
  /// mismatch `fingerprint_consistency.rs` exists to catch, manufactured by the
  /// fingerprint generator itself. And `probe_url` skips `ss`/`vless` entirely,
  /// so the launch-time gate cannot catch it either.
  ///
  /// The loopback exemption applies ONLY to schemes reqwest can proxy. A
  /// loopback SOCKS upstream IS the local worker, so it needs no second one; a
  /// loopback `ss://127.0.0.1:8388` is still a scheme reqwest discards, and
  /// exempting it re-opened the whole leak.
  ///
  /// True here means only "reqwest must not be handed this". It does NOT mean a
  /// `donut-proxy` worker can carry it, reading it that way is what sent
  /// `vless://` to a worker that cannot speak VLESS, so the probe could never
  /// succeed. `worker_upstream_url` answers what a worker can actually dial,
  /// and `probe_route` puts the two together.
  fn needs_local_worker_for_probe(url: &str) -> bool {
    // Measured on the REWRITTEN url, because `httpstls://` becomes `https://`
    // before reqwest ever sees it and is proxyable from that point on.
    let rewritten = crate::proxy_storage::reqwest_upstream_url(url);
    let scheme = rewritten
      .split("://")
      .next()
      .unwrap_or_default()
      .to_ascii_lowercase();

    if !crate::proxy_storage::reqwest_can_proxy(&rewritten) {
      // No host exemption here: reqwest discards it wherever it points.
      return true;
    }

    scheme.starts_with("socks")
      && url::Url::parse(&rewritten)
        .ok()
        .and_then(|u| match u.host() {
          Some(url::Host::Ipv4(ip)) => Some(!ip.is_loopback()),
          Some(url::Host::Ipv6(ip)) => Some(!ip.is_loopback()),
          // socks is a non-special scheme, so the url crate keeps even
          // IP-literal hosts as Domain — parse them before comparing.
          Some(url::Host::Domain(domain)) => Some(
            domain != "localhost"
              && domain
                .parse::<std::net::IpAddr>()
                .map(|ip| !ip.is_loopback())
                .unwrap_or(true),
          ),
          None => None,
        })
        .unwrap_or(false)
  }

  /// The URL a `donut-proxy` worker can actually dial for this upstream, or
  /// `None` when no worker can carry it at all.
  ///
  /// The allow-list is the exact set `proxy_server::dial_upstream` matches on.
  /// Everything outside it reaches that function's `_` arm, so every request
  /// through the worker dies as "Unsupported upstream proxy scheme", a worker
  /// started on such a URL is not a fallback, it is a guaranteed failure. That
  /// is how `vless://` came to be routed here: `needs_local_worker_for_probe`
  /// answers "reqwest cannot proxy this", which was read as "a worker can", and
  /// the probe could then never succeed. VLESS is carried by an Xray-core
  /// sidecar instead (see `probe_route`), and anything else honestly has no
  /// transport here.
  ///
  /// `socks5h`/`socks4a` are the "resolve at the exit" spellings of `socks5`/
  /// `socks4`. The worker hands the target hostname to the SOCKS server rather
  /// than resolving it locally, which is exactly what the `h` asks for, so they
  /// are normalized to the spelling the worker matches instead of rejected.
  fn worker_upstream_url(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
      "http" | "https" | "httpstls" | "socks4" | "socks5" | "ss" | "shadowsocks" => {
        Some(format!("{scheme}://{rest}"))
      }
      "socks5h" => Some(format!("socks5://{rest}")),
      "socks4a" => Some(format!("socks4://{rest}")),
      _ => None,
    }
  }

  /// Decide how the geolocation probe for `url` reaches the exit.
  ///
  /// Preference order, and the reason for it: probe through something that
  /// genuinely carries the traffic, or do not probe at all. Every arm that
  /// cannot carry it resolves to `Unroutable`, which the caller turns into a
  /// skipped probe and an ungenerated location, never into a probe sent from
  /// this machine's own address, and never into a default location presented as
  /// the exit's.
  fn probe_route(url: &str) -> ProbeRoute {
    if !Self::needs_local_worker_for_probe(url) {
      let rewritten = crate::proxy_storage::reqwest_upstream_url(url);
      // Checked rather than assumed. `needs_local_worker_for_probe` already
      // returns true for everything reqwest cannot proxy, but handing reqwest a
      // URL it cannot match makes it send the request DIRECT with no error and
      // no log line, so the invariant is re-asserted where it matters.
      return if crate::proxy_storage::reqwest_can_proxy(&rewritten) {
        ProbeRoute::Reqwest(rewritten)
      } else {
        ProbeRoute::Unroutable
      };
    }

    // The worker gets the STORED url, never the reqwest rewrite: to
    // `donut-proxy`, `httpstls` means "TLS to the proxy, then CONNECT" while
    // `https` means a plaintext CONNECT, so handing it the reqwest spelling
    // would silently downgrade that hop.
    if let Some(upstream) = Self::worker_upstream_url(url) {
      return ProbeRoute::Worker(upstream);
    }

    if crate::xray::XrayProtocol::from_share_link(url).is_some() {
      return match crate::xray::parse_share_link(url) {
        Ok(_) => ProbeRoute::Xray(url.to_string()),
        // A `vless://host:port` with no id or security parameters is what a
        // stored Xray proxy collapses to when it is rendered as
        // `type://host:port`. Xray cannot dial that, so there is nothing to
        // probe through and the launch-time refresh does the location instead -
        // by then the upstream is the loopback SOCKS5 endpoint of a real Xray
        // worker, which any transport here can carry.
        Err(_) => ProbeRoute::Unroutable,
      };
    }

    ProbeRoute::Unroutable
  }

  /// Whether the probe has to be skipped outright.
  ///
  /// True when the profile routes its traffic somewhere, the probe would
  /// actually leave this machine, and no transport was built to carry it.
  /// Probing anyway resolves this machine's own address and writes its location
  /// into the fingerprint as the exit's.
  ///
  /// `probe_leaves_this_machine` is false when the location comes from a pinned
  /// geoip IP or geolocation is switched off. `apply_geolocation` never touches
  /// the network through the proxy in either case, so a routed profile with a
  /// pinned IP must still get its location.
  fn must_skip_probe(
    routes_traffic: bool,
    probe_leaves_this_machine: bool,
    have_probe_proxy: bool,
  ) -> bool {
    routes_traffic && probe_leaves_this_machine && !have_probe_proxy
  }

  /// Build the transport the geolocation probe for `url` will use. Callers must
  /// `shutdown()` the result once the probe is done, on every path.
  async fn build_probe_transport(url: &str) -> ProbeTransport {
    match Self::probe_route(url) {
      ProbeRoute::Reqwest(proxy) => ProbeTransport {
        proxy: Some(proxy),
        ..Default::default()
      },
      ProbeRoute::Worker(upstream) => Self::front_with_local_worker(upstream, None).await,
      ProbeRoute::Xray(uri) => {
        // The error is flattened to a String on the spot. `start_xray_worker`
        // reports a bare `Box<dyn Error>`, which is not `Send`, and holding one
        // across the `front_with_local_worker` await below would make this
        // whole future non-`Send`, and with it every Tauri command and spawned
        // task that reaches fingerprint generation.
        let started = crate::xray_worker_runner::start_xray_worker(None, &uri)
          .await
          .map_err(|error| error.to_string());
        match started {
          Ok(worker) => {
            let upstream =
              crate::proxy_manager::ProxyManager::build_proxy_url(&worker.local_proxy_settings());
            Self::front_with_local_worker(upstream, Some(worker.id)).await
          }
          Err(e) => {
            log::warn!(
              "Geolocation probe skipped, Xray worker start failed err=\"{}\"",
              crate::log_redaction::text(&e)
            );
            ProbeTransport::default()
          }
        }
      }
      ProbeRoute::Unroutable => {
        log::warn!("Geolocation probe skipped, no transport can carry it through the upstream");
        ProbeTransport::default()
      }
    }
  }

  /// Put a temporary local `donut-proxy` worker in front of `upstream`, the
  /// same path the browser itself uses. `xray_worker_id` is threaded through so
  /// a sidecar started for a VLESS upstream is still stopped when the worker in
  /// front of it fails to start.
  async fn front_with_local_worker(
    upstream: String,
    xray_worker_id: Option<String>,
  ) -> ProbeTransport {
    match crate::proxy_runner::start_proxy_process(Some(upstream), None).await {
      Ok(worker) => ProbeTransport {
        proxy: Some(format!(
          "http://127.0.0.1:{}",
          worker.local_port.unwrap_or(0)
        )),
        donut_worker_id: Some(worker.id),
        xray_worker_id,
      },
      Err(e) => {
        // NOT the raw upstream. reqwest silently ignores a proxy URL it cannot
        // match, so handing it back here sent the probe from this machine's
        // real address. `None` means "no proxied probe available", and the
        // caller skips the probe entirely rather than making it unproxied.
        log::warn!(
          "Geolocation probe skipped, proxy worker start failed err=\"{}\"",
          crate::log_redaction::text(&e.to_string())
        );
        ProbeTransport {
          proxy: None,
          donut_worker_id: None,
          xray_worker_id,
        }
      }
    }
  }

  /// Resolve the location of the exit a fingerprint is generated against.
  ///
  /// Builds a transport that genuinely carries the probe through this
  /// profile's upstream, or none at all. `probe_route` decides which: reqwest
  /// where it can proxy the scheme itself, a temporary local donut-proxy worker
  /// for the schemes that worker speaks (including every remote SOCKS one,
  /// because reqwest's SOCKS connector corrupts its parse buffer when a proxy
  /// splits a handshake reply across TCP segments), an Xray-core sidecar behind
  /// such a worker for VLESS, and nothing for an upstream none of them can
  /// speak.
  ///
  /// No transport is built when the probe would not leave this machine anyway:
  /// `resolve_geolocation` ignores `proxy` entirely when geolocation is off or
  /// the location comes from a pinned geoip IP.
  async fn resolve_generation_location(
    profile: &BrowserProfile,
    config: &WayfernConfig,
  ) -> Option<crate::geolocation::Geolocation> {
    let probe_leaves_this_machine = !matches!(
      config.geoip.as_ref(),
      Some(serde_json::Value::Bool(false)) | Some(serde_json::Value::String(_))
    );
    let transport = match config.proxy.as_deref() {
      Some(url) if probe_leaves_this_machine => Self::build_probe_transport(url).await,
      _ => ProbeTransport::default(),
    };

    // A profile that ROUTES ITS TRAFFIC must never probe from the real
    // address: `resolve_geolocation` treats `None` as "no proxy configured",
    // which is right for a direct profile and an IP leak for a routed one.
    //
    // `config.proxy.is_some()` alone was not that predicate. A WireGuard
    // profile carries its route in `vpn_id` and reaches here with
    // `config.proxy == None`, so the guard never fired and the host's own
    // timezone, latitude/longitude and language were written into the
    // fingerprint as authoritative.
    let routes_traffic = config.proxy.is_some() || profile.vpn_id.is_some();
    let exit_location = if Self::must_skip_probe(
      routes_traffic,
      probe_leaves_this_machine,
      transport.proxy.is_some(),
    ) {
      log::warn!(
        "Geolocation probe skipped, profile has an upstream but no proxied probe profile={}",
        profile.id
      );
      None
    } else {
      Self::resolve_geolocation(transport.proxy.as_deref(), config.geoip.as_ref()).await
    };

    transport.shutdown().await;
    exit_location
  }

  /// Build a device with Donut's own generator and store it as a full payload.
  ///
  /// Desktop OSes only: a phone or tablet claim has no persona and is refused
  /// with its own code. A desktop claim for another host is generated like any
  /// other; the launch gate decides whether this machine may open it.
  async fn generate_local_fingerprint(
    profile: &BrowserProfile,
    config: &WayfernConfig,
    os: &str,
  ) -> Result<GeneratedFingerprint, Box<dyn std::error::Error + Send + Sync>> {
    if !crate::apostate::is_desktop_os(os) {
      return Err(crate::backend_error("FINGERPRINT_MOBILE_UNSUPPORTED").into());
    }

    let exit_location = Self::resolve_generation_location(profile, config).await;
    let device = crate::fingerprint_generator::generate(
      &profile.id.to_string(),
      os,
      &profile.version,
      exit_location.as_ref(),
    );
    let location = device.as_object().and_then(Self::location_of);
    let geolocation_applied = exit_location.is_some();
    log::info!(
      "Fingerprint generated locally profile={} os={} geolocation={}",
      profile.id,
      crate::log_redaction::Plain(os),
      if geolocation_applied {
        "applied"
      } else {
        "skipped"
      }
    );
    Ok(GeneratedFingerprint {
      fingerprint: device.to_string(),
      identity_id: None,
      location,
      geolocation_applied,
    })
  }

  /// Put the exit's location into a locally supplied payload.
  ///
  /// The form can generate a device before the profile exists, when the
  /// profile's route is not yet known, so that device carries no location. A
  /// profile that routes its traffic must never keep a location it did not
  /// verify: the keys are removed and the caller leaves `geolocation_applied`
  /// false, which keeps the launch-time refresh available.
  pub async fn localize_supplied_fingerprint(
    profile: &BrowserProfile,
    config: &WayfernConfig,
    fingerprint_json: &str,
  ) -> (String, bool) {
    let Some(device) = Self::fingerprint_object(fingerprint_json) else {
      return (fingerprint_json.to_string(), false);
    };
    let exit_location = Self::resolve_generation_location(profile, config).await;
    let routed = config.proxy.is_some() || profile.vpn_id.is_some();
    let (device, geolocation_applied) =
      Self::localize_device(device, exit_location.as_ref(), routed);
    (
      serde_json::Value::Object(device).to_string(),
      geolocation_applied,
    )
  }

  /// Merge an exit location into a supplied device, or strip the location it
  /// carried when the exit could not be resolved and the profile is routed.
  fn localize_device(
    mut device: serde_json::Map<String, serde_json::Value>,
    exit_location: Option<&crate::geolocation::Geolocation>,
    routed: bool,
  ) -> (serde_json::Map<String, serde_json::Value>, bool) {
    if let Some(geo) = exit_location {
      let mut value = serde_json::Value::Object(device);
      Self::write_geolocation(&mut value, geo);
      return (value.as_object().cloned().unwrap_or_default(), true);
    }
    if routed {
      for key in LOCALE_CARRY_OVER_KEYS {
        device.remove(key);
      }
    }
    (device, false)
  }

  /// Build the device a profile presents, with Donut's own generator. The
  /// exit's location is resolved through the profile's route and merged in.
  ///
  /// Callers only stamp `geo_proxy_signature` when `geolocation_applied` is true:
  /// on failure the device carries no location, and a stamped signature would
  /// tell the launch-time refresh the location is already right for this
  /// route, which disables the one path that can repair it.
  pub async fn generate_fingerprint_config(
    &self,
    _app_handle: &AppHandle,
    profile: &BrowserProfile,
    config: &WayfernConfig,
  ) -> Result<GeneratedFingerprint, Box<dyn std::error::Error + Send + Sync>> {
    // A profile without an explicit claim keeps the OS of the machine that
    // created it, wherever it is regenerated.
    let native_os = profile.native_os();
    let os = config.os.as_deref().unwrap_or(&native_os);
    Self::generate_local_fingerprint(profile, config, os).await
  }
  /// The device a profile presents, as a JSON object string, or `None` when
  /// none is stored yet (the next launch draws it).
  pub async fn profile_fingerprint(
    &self,
    profile: &BrowserProfile,
  ) -> Result<Option<String>, String> {
    Ok(
      profile
        .wayfern_config
        .as_ref()
        .and_then(|config| config.fingerprint.clone()),
    )
  }
  pub async fn stop_wayfern(
    &self,
    id: &str,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Taken out of the map first, and the lock dropped: a clean shutdown can
    // take seconds, and nothing else should wait on it.
    let instance = {
      let mut inner = self.inner.lock().await;
      inner.instances.remove(id)
    };

    if let Some(instance) = instance {
      if let Some(pid) = instance.process_id {
        let started = std::time::Instant::now();
        let outcome = self.stop_browser_process(pid, instance.cdp_port).await;
        let profile = profile_id_in_path(instance.profile_path.as_deref().unwrap_or_default());
        let elapsed_ms = started.elapsed().as_millis();
        if outcome == StopOutcome::StillRunning {
          log::warn!(
            "Browser stop failed, still running profile={profile} pid={pid} elapsed_ms={elapsed_ms}"
          );
        } else {
          log::info!(
            "Browser stopped profile={profile} pid={pid} how={outcome} elapsed_ms={elapsed_ms}"
          );
        }
      }
    }

    Ok(())
  }

  /// Stop a browser the way its session files need: ask it to close over
  /// CDP so Chromium runs its own shutdown (that is what writes the session
  /// and commits the cookie jar), then terminate, then kill. Each step gets a
  /// bounded wait, so a wedged browser still ends within seconds.
  pub async fn stop_browser_process(&self, pid: u32, cdp_port: Option<u16>) -> StopOutcome {
    if !crate::proxy_storage::is_process_running(pid) {
      return StopOutcome::Closed;
    }
    if let Some(port) = cdp_port {
      match tokio::time::timeout(Duration::from_secs(3), self.browser_close(port)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::debug!("Browser.close refused pid={pid} cdp_port={port} err=\"{e}\""),
        Err(_) => log::debug!("Browser.close timed out pid={pid} cdp_port={port}"),
      }
      if wait_for_exit(pid, Duration::from_secs(5)).await {
        return StopOutcome::Closed;
      }
    }
    terminate_process(pid);
    if wait_for_exit(pid, Duration::from_secs(5)).await {
      return StopOutcome::Terminated;
    }
    force_kill_process(pid);
    if wait_for_exit(pid, Duration::from_secs(2)).await {
      return StopOutcome::Killed;
    }
    StopOutcome::StillRunning
  }

  /// Send `Browser.close` on the browser endpoint. The browser answers with an
  /// empty result, or simply drops the socket on its way out; both mean it
  /// agreed, and the caller watches the process rather than the reply.
  async fn browser_close(&self, port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let version: serde_json::Value = self
      .http_client
      .get(format!("http://127.0.0.1:{port}/json/version"))
      .send()
      .await?
      .json()
      .await?;
    let ws_url = version["webSocketDebuggerUrl"]
      .as_str()
      .ok_or("the browser reported no webSocketDebuggerUrl")?;
    match self
      .send_cdp_command(ws_url, "Browser.close", json!({}))
      .await
    {
      Ok(_) => Ok(()),
      Err(e) if e.to_string().contains("No response received") => Ok(()),
      Err(e) => Err(e),
    }
  }

  /// Opens a URL in a new tab for an existing Wayfern instance.
  pub async fn open_url_in_tab(
    &self,
    profile_path: &str,
    url: &str,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let inner = self.inner.lock().await;
    let target_path = std::path::Path::new(profile_path)
      .canonicalize()
      .unwrap_or_else(|_| std::path::Path::new(profile_path).to_path_buf());

    let port = inner
      .instances
      .values()
      .find(|i| {
        i.profile_path
          .as_deref()
          .map(|p| {
            std::path::Path::new(p)
              .canonicalize()
              .unwrap_or_else(|_| std::path::Path::new(p).to_path_buf())
              == target_path
          })
          .unwrap_or(false)
      })
      .and_then(|i| i.cdp_port)
      .ok_or("Wayfern instance (with CDP port) not found for profile")?;
    drop(inner);
    self.open_url_on_port(port, url).await
  }

  /// Open `url` in a new tab of the browser on `port`, through the CDP HTTP
  /// convenience endpoint, which needs no page target to exist yet.
  async fn open_url_on_port(
    &self,
    port: u16,
    url: &str,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let new_tab_url = format!(
      "http://127.0.0.1:{port}/json/new?{}",
      urlencoding::encode(url)
    );
    let resp = self
      .http_client
      .put(&new_tab_url)
      .send()
      .await
      .map_err(|e| format!("Failed to open new tab: {e}"))?;
    if !resp.status().is_success() {
      return Err(format!("CDP /json/new returned HTTP {}", resp.status()).into());
    }

    log::debug!("Opened URL in new tab cdp_port={port}");
    Ok(())
  }

  /// What the running browser for `profile_path` has said about Wayfern on
  /// stderr so far (launch identity, refusals), newest last.
  #[allow(dead_code)]
  pub async fn browser_log_lines(&self, profile_path: &str) -> Vec<String> {
    let inner = self.inner.lock().await;
    let target_path = std::path::Path::new(profile_path)
      .canonicalize()
      .unwrap_or_else(|_| std::path::Path::new(profile_path).to_path_buf());
    inner
      .instances
      .values()
      .find(|instance| {
        instance.profile_path.as_deref().is_some_and(|path| {
          std::path::Path::new(path)
            .canonicalize()
            .unwrap_or_else(|_| std::path::Path::new(path).to_path_buf())
            == target_path
        })
      })
      .map(|instance| instance.log_tap.lines())
      .unwrap_or_default()
  }

  /// The DevTools port of the browser running `profile`. An ephemeral or
  /// password-protected profile runs from its RAM directory, not from its
  /// path under the profiles dir, so the lookup goes by the path it ran with.
  pub async fn cdp_port_for_profile(&self, profile: &BrowserProfile) -> Option<u16> {
    let profiles_dir = crate::profile::ProfileManager::instance().get_profiles_dir();
    let path = crate::ephemeral_dirs::get_effective_profile_path(profile, &profiles_dir);
    self.get_cdp_port(&path.to_string_lossy()).await
  }

  pub async fn get_cdp_port(&self, profile_path: &str) -> Option<u16> {
    let inner = self.inner.lock().await;
    let target_path = std::path::Path::new(profile_path)
      .canonicalize()
      .unwrap_or_else(|_| std::path::Path::new(profile_path).to_path_buf());

    for instance in inner.instances.values() {
      if let Some(path) = &instance.profile_path {
        let instance_path = std::path::Path::new(path)
          .canonicalize()
          .unwrap_or_else(|_| std::path::Path::new(path).to_path_buf());
        if instance_path == target_path {
          return instance.cdp_port;
        }
      }
    }
    None
  }

  /// The in-memory instance registered for a profile path, if any.
  fn instance_for_path(
    inner: &WayfernManagerInner,
    target_path: &std::path::Path,
  ) -> Option<WayfernLaunchResult> {
    inner.instances.iter().find_map(|(id, instance)| {
      let path = instance.profile_path.as_deref()?;
      let instance_path = std::path::Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| std::path::Path::new(path).to_path_buf());
      (instance_path == target_path).then(|| WayfernLaunchResult {
        id: id.clone(),
        processId: instance.process_id,
        profilePath: instance.profile_path.clone(),
        url: instance.url.clone(),
        cdp_port: instance.cdp_port,
      })
    })
  }

  /// The running browser of a profile. Process checks run without the
  /// `inner` lock, so launch, stop and CDP calls never wait on them.
  pub async fn find_wayfern_by_profile(&self, profile_path: &str) -> Option<WayfernLaunchResult> {
    let target_path = std::path::Path::new(profile_path)
      .canonicalize()
      .unwrap_or_else(|_| std::path::Path::new(profile_path).to_path_buf());

    let known = {
      let inner = self.inner.lock().await;
      Self::instance_for_path(&inner, &target_path)
    };

    if let Some(found) = known {
      if let Some(pid) = found.processId {
        if crate::proxy_storage::is_process_running(pid) {
          return Some(found);
        }
        let mut inner = self.inner.lock().await;
        let same_process = inner
          .instances
          .get(&found.id)
          .is_some_and(|instance| instance.process_id == Some(pid));
        if same_process {
          log::info!(
            "Browser exited profile={} pid={pid}",
            profile_id_in_path(profile_path)
          );
          inner.instances.remove(&found.id);
        }
        return None;
      }
    }

    // If not found in in-memory instances, scan system processes.
    // This handles the case where the GUI was restarted but Wayfern is still running.
    let scan_path = target_path.clone();
    let scanned =
      tokio::task::spawn_blocking(move || Self::find_wayfern_process_by_profile(&scan_path))
        .await
        .ok()
        .flatten();
    if let Some((pid, found_profile_path, cdp_port)) = scanned {
      // A browser seconds old is almost always one this app is still
      // launching: its instance is registered only once the launch finishes.
      let age_s = crate::proxy_storage::process_start_time(pid).map(|start| {
        std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .map(|now| now.as_secs().saturating_sub(start))
          .unwrap_or_default()
      });

      let mut inner = self.inner.lock().await;
      // A launch may have registered its browser while the scan ran.
      if let Some(registered) = Self::instance_for_path(&inner, &target_path)
        .filter(|registered| registered.processId.is_some())
      {
        return Some(registered);
      }

      let level = if age_s.is_some_and(|age| age < 120) {
        log::Level::Debug
      } else {
        log::Level::Info
      };
      log::log!(
        level,
        "Browser adopted from a running process profile={} pid={pid} cdp_port={cdp_port:?} age_s={age_s:?}",
        profile_id_in_path(&found_profile_path)
      );

      let instance_id = format!("recovered_{}", pid);
      inner.instances.insert(
        instance_id.clone(),
        WayfernInstance {
          process_id: Some(pid),
          profile_path: Some(found_profile_path.clone()),
          url: None,
          cdp_port,
          log_tap: BrowserLogTap::default(),
        },
      );

      return Some(WayfernLaunchResult {
        id: instance_id,
        processId: Some(pid),
        profilePath: Some(found_profile_path),
        url: None,
        cdp_port,
      });
    }

    None
  }

  /// Scan system processes to find a Wayfern/Chromium process using a specific profile path
  fn find_wayfern_process_by_profile(
    target_path: &std::path::Path,
  ) -> Option<(u32, String, Option<u16>)> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, UpdateKind};

    let system = crate::proxy_storage::process_table(
      RefreshKind::nothing().with_processes(
        ProcessRefreshKind::nothing()
          .with_cmd(UpdateKind::OnlyIfNotSet)
          .without_tasks(),
      ),
    );

    let target_path_str = target_path.to_string_lossy();

    for (pid, process) in system.processes() {
      let cmd = process.cmd();
      if cmd.is_empty() {
        continue;
      }

      let exe_name = process.name().to_string_lossy().to_lowercase();
      let is_chromium_like = exe_name.contains("wayfern")
        || exe_name.contains("chromium")
        || exe_name.contains("chrome");

      if !is_chromium_like {
        continue;
      }

      // Skip child processes (renderer, GPU, utility, zygote, etc.)
      // Only the main browser process lacks a --type= argument
      let is_child = cmd
        .iter()
        .any(|a| a.to_str().is_some_and(|s| s.starts_with("--type=")));
      if is_child {
        continue;
      }

      let mut matched = false;
      let mut cdp_port: Option<u16> = None;

      for arg in cmd.iter() {
        if let Some(arg_str) = arg.to_str() {
          if let Some(dir_val) = arg_str.strip_prefix("--user-data-dir=") {
            let cmd_path = std::path::Path::new(dir_val)
              .canonicalize()
              .unwrap_or_else(|_| std::path::Path::new(dir_val).to_path_buf());
            if cmd_path == target_path {
              matched = true;
            }
          }

          if let Some(port_val) = arg_str.strip_prefix("--remote-debugging-port=") {
            cdp_port = port_val.parse().ok();
          }
        }
      }

      if matched {
        return Some((pid.as_u32(), target_path_str.to_string(), cdp_port));
      }
    }

    None
  }

  #[allow(dead_code)]
  pub async fn cleanup_dead_instances(&self) {
    let known: Vec<(String, u32)> = {
      let inner = self.inner.lock().await;
      inner
        .instances
        .iter()
        .filter_map(|(id, instance)| instance.process_id.map(|pid| (id.clone(), pid)))
        .collect()
    };
    let dead: Vec<(String, u32)> = known
      .into_iter()
      .filter(|(_, pid)| !crate::proxy_storage::is_process_running(*pid))
      .collect();
    if dead.is_empty() {
      return;
    }

    let mut inner = self.inner.lock().await;
    for (id, pid) in dead {
      if inner
        .instances
        .get(&id)
        .is_some_and(|instance| instance.process_id == Some(pid))
      {
        log::debug!("Removed dead browser instance id={id}");
        inner.instances.remove(&id);
      }
    }
  }
}

/// Terminate a browser process by pid.
///
/// Shared with the launch path, which cannot go through `stop_wayfern`: an
/// instance is only registered in `inner.instances` at the very end of
/// `launch_wayfern`, so a launch that aborts part-way has a live process and no
/// id to stop it by.
fn kill_browser_process(pid: u32) {
  #[cfg(unix)]
  terminate_process(pid);
  #[cfg(windows)]
  force_kill_process(pid);
}

static WAYFERN_MANAGER: std::sync::LazyLock<WayfernManager> =
  std::sync::LazyLock::new(WayfernManager::new);

/// Deterministically derive a pleasant, distinct window frame color from a
/// profile id so concurrent profile windows are visually distinguishable even
/// when the user has not picked a custom color. Stable per profile (same id
/// always yields the same color). Returns "#RRGGBB".
pub fn derive_profile_color(id: &uuid::Uuid) -> String {
  // FNV-1a over the 16 id bytes -> hue in [0,360). The hue varies per profile
  // while saturation/lightness are fixed to a pastel band (see below).
  let mut h: u32 = 2166136261;
  for &b in id.as_bytes() {
    h = (h ^ u32::from(b)).wrapping_mul(16777619);
  }
  let hue = f64::from(h % 360);
  // Pastel: high lightness + soft saturation so windows stay easy to tell apart
  // without a garish frame.
  let (r, g, b) = hsl_to_rgb(hue, 0.6, 0.8);
  format!("#{r:02x}{g:02x}{b:02x}")
}

/// Convert HSL (h in [0,360), s/l in [0,1]) to 8-bit RGB.
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
  let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
  let hp = h / 60.0;
  let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
  let (r1, g1, b1) = match hp as i32 {
    0 => (c, x, 0.0),
    1 => (x, c, 0.0),
    2 => (0.0, c, x),
    3 => (0.0, x, c),
    4 => (x, 0.0, c),
    _ => (c, 0.0, x),
  };
  let m = l - c / 2.0;
  let to_u8 = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
  (to_u8(r1), to_u8(g1), to_u8(b1))
}

#[cfg(test)]
mod tests {
  #[test]
  fn only_wayfern_errors_and_cookie_key_lines_leave_the_browser() {
    use super::browser_line_level;
    assert_eq!(
      browser_line_level(
        "[1:2:1102/0:ERROR:components\\os_crypt\\sync\\os_crypt_win.cc:90] Failed to decrypt"
      ),
      Some(log::Level::Warn)
    );
    assert_eq!(
      browser_line_level("[1:2:INFO:wayfern_launch_identity.cc:1] Wayfern launch identity applied"),
      Some(log::Level::Info)
    );
    assert_eq!(
      browser_line_level("[1:2:ERROR:gpu_init.cc:1] no GPU"),
      Some(log::Level::Debug)
    );
    assert_eq!(
      browser_line_level("[1:2:INFO:visited.cc:1] https://example.com"),
      None
    );
  }

  use super::*;

  #[test]
  fn localizing_a_supplied_device_writes_the_exit_and_strips_an_unverified_location() {
    let device = json!({
      "userAgent": "Mozilla/5.0",
      "timezone": "Asia/Ho_Chi_Minh",
      "language": "vi-VN",
      "latitude": 10.0,
      "longitude": 106.0,
    })
    .as_object()
    .cloned()
    .unwrap();
    let geo = crate::geolocation::Geolocation {
      locale: crate::geolocation::Locale {
        language: "en".into(),
        region: Some("US".into()),
      },
      longitude: -122.6765,
      latitude: 45.5231,
      timezone: "America/Los_Angeles".into(),
    };
    let (localized, applied) = WayfernManager::localize_device(device.clone(), Some(&geo), true);
    assert!(applied);
    assert_eq!(localized["timezone"], json!("America/Los_Angeles"));
    assert_eq!(localized["language"], json!("en-US"));
    assert_eq!(localized["latitude"], json!(45.5231));
    assert_eq!(localized["userAgent"], json!("Mozilla/5.0"));

    let (stripped, applied) = WayfernManager::localize_device(device, None, true);
    assert!(!applied);
    for key in ["timezone", "language", "latitude", "longitude"] {
      assert!(
        stripped.get(key).is_none(),
        "{key} must not survive an unverified route"
      );
    }
    assert_eq!(stripped["userAgent"], json!("Mozilla/5.0"));
  }

  #[test]
  fn localizing_a_direct_device_with_no_exit_keeps_what_the_user_supplied() {
    let device = json!({"timezone": "Asia/Ho_Chi_Minh"})
      .as_object()
      .cloned()
      .unwrap();
    let (localized, applied) = WayfernManager::localize_device(device, None, false);
    assert!(!applied);
    assert_eq!(localized["timezone"], json!("Asia/Ho_Chi_Minh"));
  }

  #[test]
  fn an_early_exit_names_the_last_lines_the_browser_printed_redacted() {
    let lines = vec![
      "first".to_string(),
      "[1:2:1023/171839.224:ERROR:chrome_browser_main.cc(1)] Cannot open C:\\Users\\PC\\AppData"
        .to_string(),
      "[1:2:1023/171839.225:FATAL:x.cc(2)] gone".to_string(),
    ];
    let said = last_said(&lines);
    assert!(!said.contains("first"), "{said}");
    assert!(
      said.contains(":ERROR:") && said.contains(":FATAL:"),
      "{said}"
    );
    assert!(
      !said.contains("PC"),
      "a user folder must not reach the log: {said}"
    );
    assert_eq!(last_said(&[]), "");
  }

  #[test]
  fn launch_args_default_and_round_trip() {
    let legacy: WayfernConfig = serde_json::from_str("{}").unwrap();
    assert!(legacy.launch_args.is_empty());
    let config: WayfernConfig = serde_json::from_str(
      r#"{"launch_args":["--disable-gpu","--disk-cache-dir=/tmp/cache with spaces"]}"#,
    )
    .unwrap();
    let saved = serde_json::to_string(&config).unwrap();
    let loaded: WayfernConfig = serde_json::from_str(&saved).unwrap();
    assert_eq!(loaded.launch_args, config.launch_args);
  }

  fn code_and_argument(error: &str) -> (String, String) {
    let value: serde_json::Value = serde_json::from_str(error).unwrap();
    (
      value["code"].as_str().unwrap_or_default().to_string(),
      value["params"]["argument"]
        .as_str()
        .unwrap_or_default()
        .to_string(),
    )
  }

  #[test]
  fn launch_args_accept_supported_switches_as_single_arguments() {
    let args = vec![
      "  --disable-gpu  ".to_string(),
      "".to_string(),
      "--autoplay-policy=no-user-gesture-required".to_string(),
      "--renderer-process-limit=8".to_string(),
      "--disable-features=OverlayScrollbar,TranslateUI".to_string(),
    ];
    assert_eq!(
      validate_launch_args(&args, true).unwrap(),
      vec![
        "--disable-gpu",
        "--autoplay-policy=no-user-gesture-required",
        "--renderer-process-limit=8",
        "--disable-features=OverlayScrollbar,TranslateUI",
      ]
    );
  }

  #[test]
  fn launch_args_need_automation_but_clearing_them_does_not() {
    assert!(validate_launch_args(&[], false).unwrap().is_empty());
    assert!(validate_launch_args(&["  ".to_string()], false)
      .unwrap()
      .is_empty());
    let error = validate_launch_args(&["--disable-gpu".to_string()], false).unwrap_err();
    assert_eq!(
      code_and_argument(&error).0,
      "WAYFERN_LAUNCH_ARGS_REQUIRES_PRO"
    );
  }

  #[test]
  fn launch_args_refuse_switches_that_run_programs_or_weaken_the_browser() {
    for arg in [
      "--renderer-cmd-prefix=/tmp/x",
      "--gpu-launcher=/tmp/x",
      "--utility-cmd-prefix=/tmp/x",
      "--browser-subprocess-path=/tmp/x",
      "--zygote-cmd-prefix=/tmp/x",
      "--disable-web-security",
      "--ignore-certificate-errors",
      "--allow-file-access-from-files",
      "--remote-allow-origins=*",
      "--custom-devtools-frontend=http://x",
      "--js-flags=--expose-gc",
      "--single-process",
      "--log-net-log=/tmp/net.json",
      "--force-webrtc-ip-handling-policy=default",
      "--disable-blink-features=AutomationControlled",
      "--enable-automation",
    ] {
      let error = validate_launch_args(&[arg.to_string()], true).unwrap_err();
      let (code, argument) = code_and_argument(&error);
      assert_eq!(code, "WAYFERN_LAUNCH_ARG_NOT_ALLOWED", "{arg}");
      assert_eq!(argument, arg.split('=').next().unwrap());
    }
  }

  #[test]
  fn launch_args_name_the_switches_donut_controls() {
    for arg in [
      "--user-data-dir=/tmp/other",
      "--profile-directory=Other",
      "--remote-debugging-port=9222",
      "--remote-debugging-pipe",
      "--proxy-server=http://other:8080",
      "--no-proxy-server",
      "--host-resolver-rules=MAP * other",
      "--host-rules=MAP * other",
      "--headless=new",
      "--wayfern-identity-file=/tmp/other",
      "--window-size=800,600",
      "--load-extension=/tmp/ext",
      "--enable-logging=stderr",
      "--log-level=3",
      "--password-store=gnome",
      "--no-sandbox",
      "--user-agent=Other",
      "--lang=de",
      "--disk-cache-dir=/tmp/cache",
    ] {
      let error = validate_launch_args(&[arg.to_string()], true).unwrap_err();
      let (code, argument) = code_and_argument(&error);
      assert_eq!(code, "WAYFERN_LAUNCH_ARG_RESERVED", "{arg}");
      assert_eq!(argument, arg.split('=').next().unwrap());
    }
  }

  #[test]
  fn launch_args_reject_malformed_input_and_bad_values() {
    for arg in [
      "--",
      "https://example.com",
      "--window-size 800,600",
      "--USER-DATA-DIR=/tmp/other",
      "-disable-gpu",
      "/disable-gpu",
      "--disable\u{200b}-gpu",
      "--value=one\ntwo",
      "--value=one\0two",
    ] {
      let error = validate_launch_args(&[arg.to_string()], true).unwrap_err();
      assert!(error.contains("WAYFERN_LAUNCH_ARG_INVALID"), "{arg:?}");
    }
    for arg in [
      "--disable-gpu=1",
      "--autoplay-policy=always",
      "--renderer-process-limit=0",
      "--renderer-process-limit=1000",
      "--num-raster-threads=two",
      "--disable-features=",
      "--disable-features=WebRtcHideLocalIpsWithMdns",
      "--enable-features=WayfernSomething",
      "--enable-features=Good,AsyncDns",
      "--enable-features=Name<Trial",
      "--enable-features=Name:param/value",
    ] {
      let error = validate_launch_args(&[arg.to_string()], true).unwrap_err();
      assert_eq!(
        code_and_argument(&error).0,
        "WAYFERN_LAUNCH_ARG_VALUE_INVALID",
        "{arg}"
      );
    }
  }

  #[test]
  fn launch_args_are_limited_in_count_and_length() {
    let many = vec!["--disable-gpu".to_string(); MAX_LAUNCH_ARGS + 1];
    let error = validate_launch_args(&many, true).unwrap_err();
    assert_eq!(code_and_argument(&error).0, "WAYFERN_LAUNCH_ARGS_LIMIT");
    let long = format!("--disable-features={}", "A".repeat(MAX_LAUNCH_ARG_CHARS));
    let error = validate_launch_args(&[long], true).unwrap_err();
    assert_eq!(code_and_argument(&error).0, "WAYFERN_LAUNCH_ARGS_LIMIT");
    assert!(validate_launch_args(&vec!["--mute-audio".to_string(); MAX_LAUNCH_ARGS], true).is_ok());
  }

  #[test]
  fn launch_args_merge_feature_lists_with_donuts_own() {
    let mut args = vec![
      "--user-data-dir=/tmp/profile".to_string(),
      "--disable-features=AsyncDns,Prefetch".to_string(),
    ];
    let custom = validate_launch_args(
      &[
        "--disable-features=OverlayScrollbar,CustomFeature".to_string(),
        "--disable-features=AnotherFeature".to_string(),
        "--enable-features=EnabledFeature".to_string(),
        "--enable-features=OtherEnabledFeature".to_string(),
        "--disable-gpu".to_string(),
      ],
      true,
    )
    .unwrap();
    apply_launch_args(&mut args, &custom);
    assert_eq!(
      args,
      vec![
        "--user-data-dir=/tmp/profile",
        "--disable-features=AsyncDns,Prefetch,OverlayScrollbar,CustomFeature,AnotherFeature",
        "--enable-features=EnabledFeature,OtherEnabledFeature",
        "--disable-gpu",
      ]
    );
  }

  #[test]
  fn remote_socks_url_detection() {
    // Remote socks upstreams (the hyper-util-affected case) are detected...
    assert!(WayfernManager::needs_local_worker_for_probe(
      "socks5://user:pass@gw.dataimpulse.com:10000"
    ));
    assert!(WayfernManager::needs_local_worker_for_probe(
      "socks5://1.2.3.4:1080"
    ));
    assert!(WayfernManager::needs_local_worker_for_probe(
      "socks4://1.2.3.4:1080"
    ));

    // ...but the app's own loopback workers are not. socks is a non-special
    // URL scheme, so the IP literal parses as Host::Domain — the launch-time
    // randomize path depends on this returning false.
    assert!(!WayfernManager::needs_local_worker_for_probe(
      "socks5://127.0.0.1:24001"
    ));
    assert!(!WayfernManager::needs_local_worker_for_probe(
      "socks5://[::1]:24001"
    ));
    assert!(!WayfernManager::needs_local_worker_for_probe(
      "socks5://localhost:24001"
    ));

    // http/https reqwest proxies natively, so they stay on the direct path.
    assert!(!WayfernManager::needs_local_worker_for_probe(
      "http://gw.dataimpulse.com:10000"
    ));
    assert!(!WayfernManager::needs_local_worker_for_probe(
      "https://gw.dataimpulse.com:10000"
    ));
    // A hostless socks URL has nothing to route to; reqwest cannot use it, and
    // the worker path ends in a skipped probe rather than an unproxied one.
    assert!(!WayfernManager::needs_local_worker_for_probe("socks5://"));
    // An unparsable upstream asks for a worker ON PURPOSE. It used to answer
    // "no worker needed", which sent it down the arm that hands reqwest the raw
    // string, and reqwest answers garbage by silently going DIRECT. The worker
    // will fail to start for a malformed URL, and that failure now skips the
    // probe entirely, which is the outcome we want: no geolocation beats
    // geolocation taken from the user's own address.
    assert!(WayfernManager::needs_local_worker_for_probe("not a url"));

    // The schemes reqwest ACCEPTS and then silently does not proxy. Before
    // these were routed through a local worker, the fingerprint's geolocation
    // probe went out from the user's real address with nothing logged.
    // `httpstls` is NOT in this group: `reqwest_upstream_url` rewrites it to
    // `https://`, which reqwest proxies natively and over TLS, so the probe
    // genuinely goes through the proxy without spawning a worker. The rewrite
    // is what makes it safe, which is why the filter measures the REWRITTEN url
    // rather than the stored one.
    assert!(!WayfernManager::needs_local_worker_for_probe(
      "httpstls://user:pass@proxy.example.com:443"
    ));
    assert!(WayfernManager::needs_local_worker_for_probe(
      "ss://method:pass@1.2.3.4:8388"
    ));
    assert!(WayfernManager::needs_local_worker_for_probe(
      "vless://uuid@1.2.3.4:443"
    ));
    // Loopback is exempt ONLY for schemes reqwest can proxy. `httpstls` is
    // rewritten to `https`, so a loopback one genuinely needs no worker.
    assert!(!WayfernManager::needs_local_worker_for_probe(
      "httpstls://127.0.0.1:8443"
    ));
    // But a LOOPBACK ss/vless is still a scheme reqwest discards. Exempting
    // these re-opened the entire leak for anyone pointing at a locally
    // forwarded Shadowsocks or VLESS endpoint.
    for loopback in [
      "ss://cipher:pw@127.0.0.1:8388",
      "ss://cipher:pw@localhost:8388",
      "ss://cipher:pw@[::1]:8388",
      "shadowsocks://cipher:pw@127.0.0.1:8388",
      "vless://uuid@127.0.0.1:443",
    ] {
      assert!(
        WayfernManager::needs_local_worker_for_probe(loopback),
        "{loopback} must still go through a worker"
      );
    }
    // An ALLOW-list, so a scheme nobody anticipated cannot fall through.
    // `proxy_type` is free text via POST /v1/proxies, import_proxies_json and
    // the MCP tool, so this is reachable without a code change. Note this only
    // says "reqwest must not be handed it", whether anything here can carry it
    // is `probe_route`'s answer, tested below.
    for unknown in [
      "trojan://gw.example.com:443",
      "hysteria2://gw.example.com:443",
      "wireguard://gw.example.com:51820",
      "SS://cipher:pw@1.2.3.4:8388",
    ] {
      assert!(
        WayfernManager::needs_local_worker_for_probe(unknown),
        "{unknown} is not proxyable by reqwest and must not be handed to it"
      );
    }
    // socks5h is proxyable and remote, so it keeps the socks rule.
    assert!(WayfernManager::needs_local_worker_for_probe(
      "socks5h://1.2.3.4:1080"
    ));
    // http/https reqwest proxies natively, so they stay on the direct path.
    assert!(!WayfernManager::needs_local_worker_for_probe(
      "https://user:pass@proxy.example.com:8443"
    ));
  }

  /// A complete VLESS + XTLS Vision + REALITY URI.
  fn valid_vless_uri() -> String {
    "vless://6d6e21a1-4829-4d2b-bc7f-1b25707b61e4@vpn.example.com:443\
?security=reality&flow=xtls-rprx-vision&encryption=none&type=tcp&sni=a.com\
&pbk=mQB9jxUDHO7g49VaNXLEdcNQ_jLhTbLolUsMUNwb6W4&sid=00&fp=chrome"
      .to_string()
  }

  #[test]
  fn worker_upstream_url_mirrors_what_donut_proxy_can_actually_dial() {
    // This list is `proxy_server::dial_upstream`'s match arms. Anything outside
    // it reaches that function's `_` arm, and every request through the worker
    // fails as "Unsupported upstream proxy scheme".
    for carried in [
      "http://gw.example.com:8080",
      "https://gw.example.com:8080",
      "httpstls://user:pass@gw.example.com:443",
      "socks4://1.2.3.4:1080",
      "socks5://user:pass@gw.example.com:1080",
      "ss://aes-256-gcm:pw@1.2.3.4:8388",
      "shadowsocks://aes-256-gcm:pw@1.2.3.4:8388",
    ] {
      assert_eq!(
        WayfernManager::worker_upstream_url(carried).as_deref(),
        Some(carried),
        "{carried} is dialable by a donut-proxy worker and must pass through unchanged"
      );
    }

    // VLESS is the whole point of this defect: a worker started on it can never
    // complete a single request, so it must not be offered as a transport.
    assert_eq!(
      WayfernManager::worker_upstream_url(&valid_vless_uri()),
      None,
      "a donut-proxy worker cannot speak VLESS"
    );
    for unspeakable in [
      "vless://uuid@1.2.3.4:443",
      "trojan://gw.example.com:443",
      "hysteria2://gw.example.com:443",
      "wireguard://gw.example.com:51820",
      "not a url",
    ] {
      assert_eq!(
        WayfernManager::worker_upstream_url(unspeakable),
        None,
        "{unspeakable} is not dialable by a donut-proxy worker"
      );
    }

    // The "resolve at the exit" spellings mean the same thing to the worker,
    // which hands the target hostname to the SOCKS server rather than resolving
    // it here, but `dial_upstream` matches the scheme literally, so they have
    // to arrive spelled the way it matches.
    assert_eq!(
      WayfernManager::worker_upstream_url("socks5h://user:pass@gw.example.com:1080").as_deref(),
      Some("socks5://user:pass@gw.example.com:1080")
    );
    assert_eq!(
      WayfernManager::worker_upstream_url("socks4a://1.2.3.4:1080").as_deref(),
      Some("socks4://1.2.3.4:1080")
    );
    // An uppercase scheme is still the same scheme; `Url::parse` lowercases it
    // anyway, so normalizing here keeps the allow-list from rejecting it.
    assert_eq!(
      WayfernManager::worker_upstream_url("SS://aes-256-gcm:pw@1.2.3.4:8388").as_deref(),
      Some("ss://aes-256-gcm:pw@1.2.3.4:8388")
    );
  }

  #[test]
  fn vless_is_probed_through_xray_or_not_at_all_but_never_through_a_worker() {
    // A complete VLESS URI: an Xray-core sidecar carries the hop, exactly as
    // browser_runner does for a VLESS launch.
    let uri = valid_vless_uri();
    assert_eq!(
      WayfernManager::probe_route(&uri),
      ProbeRoute::Xray(uri.clone())
    );
    // Every other protocol the sidecar speaks takes the same route.
    for link in [
      "vless://6d6e21a1-4829-4d2b-bc7f-1b25707b61e4@a.com:443?security=tls&type=ws",
      "trojan://pw@tj.example.com:443?sni=tj.example.com",
      "hysteria2://pw@hy.example.com:443/",
    ] {
      assert_eq!(
        WayfernManager::probe_route(link),
        ProbeRoute::Xray(link.to_string())
      );
    }

    // A `vless://host:port` is what a stored VLESS proxy collapses to when it
    // is rendered as `type://host:port`: no id, no flow, no REALITY key. Xray
    // cannot dial it and neither can a worker, so the probe is skipped and the
    // location is left ungenerated rather than defaulted.
    for lossy in [
      "vless://vpn.example.com:443",
      "vless://uuid@vpn.example.com:443",
      // Right shape, unsupported transport, still nothing that can carry it.
      "vless://6d6e21a1-4829-4d2b-bc7f-1b25707b61e4@a.com:443?security=tls&type=kcp",
      "trojan://gw.example.com:443",
    ] {
      assert_eq!(
        WayfernManager::probe_route(lossy),
        ProbeRoute::Unroutable,
        "{lossy} has no transport and must not start a worker"
      );
    }

    // And no VLESS shape may EVER resolve to a plain donut-proxy worker: that
    // worker answers every request with "Unsupported upstream proxy scheme",
    // which used to land as a default America/New_York in the fingerprint.
    for any_vless in [
      uri.as_str(),
      "vless://vpn.example.com:443",
      "vless://uuid@127.0.0.1:443",
      "VLESS://vpn.example.com:443",
    ] {
      assert!(
        !matches!(
          WayfernManager::probe_route(any_vless),
          ProbeRoute::Worker(_) | ProbeRoute::Reqwest(_)
        ),
        "{any_vless} must never be handed to a donut-proxy worker or to reqwest"
      );
    }
  }

  #[test]
  fn probe_route_sends_each_upstream_to_something_that_carries_it() {
    // reqwest proxies these itself; `httpstls` arrives already rewritten to the
    // `https` spelling reqwest understands.
    assert_eq!(
      WayfernManager::probe_route("http://gw.example.com:8080"),
      ProbeRoute::Reqwest("http://gw.example.com:8080".into())
    );
    assert_eq!(
      WayfernManager::probe_route("httpstls://user:pass@gw.example.com:443"),
      ProbeRoute::Reqwest("https://user:pass@gw.example.com:443".into())
    );
    // A loopback socks upstream IS the local worker the browser already uses,
    // so it needs no second one. This is the launch-time path.
    assert_eq!(
      WayfernManager::probe_route("socks5://127.0.0.1:24001"),
      ProbeRoute::Reqwest("socks5://127.0.0.1:24001".into())
    );

    // Remote SOCKS and Shadowsocks go through a worker, which speaks both.
    assert_eq!(
      WayfernManager::probe_route("socks5://user:pass@gw.example.com:1080"),
      ProbeRoute::Worker("socks5://user:pass@gw.example.com:1080".into())
    );
    assert_eq!(
      WayfernManager::probe_route("ss://aes-256-gcm:pw@1.2.3.4:8388"),
      ProbeRoute::Worker("ss://aes-256-gcm:pw@1.2.3.4:8388".into())
    );
    // The worker matches `socks5`, not `socks5h`, so the route carries the
    // spelling it dials rather than the stored one.
    assert_eq!(
      WayfernManager::probe_route("socks5h://1.2.3.4:1080"),
      ProbeRoute::Worker("socks5://1.2.3.4:1080".into())
    );

    // Schemes nothing here speaks, and Xray links with no credential to dial.
    // `proxy_type` is free text through the REST API and MCP, so these are
    // reachable without a code change, and each one must end in a skipped
    // probe rather than a fabricated location.
    for unroutable in [
      "trojan://gw.example.com:443",
      "hysteria2://gw.example.com:443",
      "wireguard://gw.example.com:51820",
      "not a url",
    ] {
      assert_eq!(
        WayfernManager::probe_route(unroutable),
        ProbeRoute::Unroutable,
        "{unroutable} has no transport that carries it"
      );
    }
  }

  #[test]
  fn a_pinned_geoip_ip_still_resolves_on_a_routed_profile() {
    // The probe is skipped only when it would actually leave this machine with
    // nothing to carry it. A routed profile whose location comes from a pinned
    // geoip IP resolves it locally, so it must not be skipped for want of a
    // transport it never needed.
    assert!(!WayfernManager::must_skip_probe(true, false, false));
    // Geolocation genuinely going out over a routed profile's upstream, with no
    // transport built: this is the case that must never probe.
    assert!(WayfernManager::must_skip_probe(true, true, false));
    // Transport built, or nothing routed: probe away.
    assert!(!WayfernManager::must_skip_probe(true, true, true));
    assert!(!WayfernManager::must_skip_probe(false, true, false));
  }

  #[tokio::test]
  async fn a_failed_geolocation_probe_leaves_the_location_ungenerated() {
    // A geoip string skips the network fetch entirely and fails in the local
    // MaxMind lookup, so this drives apply_geolocation's Err branch with no
    // network, no proxy and no worker.
    let mut fingerprint = json!({ "platform": "Win32" });
    let applied =
      WayfernManager::apply_geolocation(&mut fingerprint, None, Some(&json!("not-an-ip"))).await;

    assert!(!applied, "a failed lookup must not report success");
    let obj = fingerprint
      .as_object()
      .expect("fingerprint stays an object");
    for invented in [
      "timezone",
      "timezoneOffset",
      "latitude",
      "longitude",
      "language",
      "languages",
    ] {
      assert!(
        !obj.contains_key(invented),
        "a failed probe must not invent {invented}: {obj:?}"
      );
    }
    assert_eq!(obj.get("platform"), Some(&json!("Win32")));
  }

  #[tokio::test]
  async fn a_failed_geolocation_probe_does_not_overwrite_a_real_location() {
    // The other half: a fingerprint that already carries a resolved location
    // keeps it verbatim when a later probe fails.
    let mut fingerprint = json!({
      "timezone": "Europe/Berlin",
      "timezoneOffset": -60,
      "latitude": 52.52,
    });
    let applied =
      WayfernManager::apply_geolocation(&mut fingerprint, None, Some(&json!("not-an-ip"))).await;

    assert!(!applied);
    assert_eq!(fingerprint["timezone"], json!("Europe/Berlin"));
    assert_eq!(fingerprint["timezoneOffset"], json!(-60));
    assert_eq!(fingerprint["latitude"], json!(52.52));
  }

  #[test]
  fn start_pages_open_on_a_persons_launch_and_replace_the_last_session() {
    let pages = vec![
      "https://a.example/".to_string(),
      "https://b.example/".to_string(),
    ];
    assert_eq!(
      launch_pages(None, &pages, LaunchKind::Interactive),
      ["https://a.example/", "https://b.example/"]
    );
    assert_eq!(
      launch_pages(
        Some("https://given.example/"),
        &pages,
        LaunchKind::Interactive
      ),
      ["https://given.example/"],
      "an explicit URL wins over the start pages"
    );
    assert!(launch_pages(None, &pages, LaunchKind::Automation).is_empty());
    assert!(session_restore_verdict(
      &WayfernConfig::default(),
      LaunchKind::Interactive,
      false,
      false,
      false,
      true,
      true
    )
    .is_err());
  }

  #[test]
  fn session_restore_is_only_for_a_person_on_a_device_committed_at_launch() {
    let config = WayfernConfig::default();
    let interactive = |c: &WayfernConfig| {
      session_restore_verdict(c, LaunchKind::Interactive, false, false, false, true, false)
    };
    assert_eq!(interactive(&config), Ok(()));
    assert!(session_restore_verdict(
      &config,
      LaunchKind::Automation,
      false,
      false,
      false,
      true,
      false
    )
    .is_err());
    assert!(session_restore_verdict(
      &config,
      LaunchKind::Interactive,
      true,
      false,
      false,
      true,
      false
    )
    .is_err());
    assert!(session_restore_verdict(
      &config,
      LaunchKind::Interactive,
      false,
      true,
      false,
      true,
      false
    )
    .is_err());
    assert!(session_restore_verdict(
      &config,
      LaunchKind::Interactive,
      false,
      false,
      true,
      true,
      false
    )
    .is_err());
    assert!(
      session_restore_verdict(
        &config,
        LaunchKind::Interactive,
        false,
        false,
        false,
        false,
        false
      )
      .is_err(),
      "a device applied after the window opens loses the race with a restored tab"
    );
    let off = WayfernConfig {
      restore_session: Some(false),
      ..Default::default()
    };
    assert!(interactive(&off).is_err());
    let randomized = WayfernConfig {
      randomize_fingerprint_on_launch: Some(true),
      ..Default::default()
    };
    assert!(interactive(&randomized).is_err());
    let explicit_on = WayfernConfig {
      restore_session: Some(true),
      ..Default::default()
    };
    assert_eq!(interactive(&explicit_on), Ok(()));
  }

  #[test]
  fn the_webrtc_mode_reads_the_new_field_then_the_legacy_flag() {
    assert_eq!(
      WebRtcMode::from_config(&WayfernConfig::default()),
      WebRtcMode::Auto
    );
    let legacy = WayfernConfig {
      block_webrtc: Some(true),
      ..Default::default()
    };
    assert_eq!(WebRtcMode::from_config(&legacy), WebRtcMode::Block);
    let both = WayfernConfig {
      block_webrtc: Some(true),
      webrtc_mode: Some("tcp_only".into()),
      ..Default::default()
    };
    assert_eq!(WebRtcMode::from_config(&both), WebRtcMode::TcpOnly);
    let unknown = WayfernConfig {
      webrtc_mode: Some("sideways".into()),
      ..Default::default()
    };
    assert_eq!(WebRtcMode::from_config(&unknown), WebRtcMode::Auto);
    assert_eq!(WebRtcMode::parse("TCP-ONLY"), Some(WebRtcMode::TcpOnly));
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn a_termination_request_ends_a_process_and_the_wait_notices() {
    let mut child = std::process::Command::new("sleep")
      .arg("30")
      .spawn()
      .expect("sleep spawns");
    let pid = child.id();
    assert!(!wait_for_exit(pid, Duration::from_millis(200)).await);
    terminate_process(pid);
    let _ = child.wait();
    assert!(wait_for_exit(pid, Duration::from_secs(5)).await);
  }

  fn cdp_test_process() -> tokio::process::Child {
    #[cfg(unix)]
    let mut command = {
      let mut command = TokioCommand::new("sleep");
      command.arg("60");
      command
    };
    #[cfg(windows)]
    let mut command = {
      let mut command = TokioCommand::new("ping");
      command.args(["-n", "60", "127.0.0.1"]);
      command
    };
    command
      .kill_on_drop(true)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .unwrap()
  }

  async fn cdp_startup_server(
    replies: Vec<(axum::http::StatusCode, serde_json::Value)>,
  ) -> (
    u16,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
  ) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let app = axum::Router::new()
      .route("/json/version", axum::routing::get(|| async { "{}" }))
      .route(
        "/json",
        axum::routing::get(move || {
          let attempt = seen.fetch_add(1, Ordering::SeqCst);
          let (status, body) = replies[attempt.min(replies.len() - 1)].clone();
          async move { (status, axum::Json(body)) }
        }),
      );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
      axum::serve(listener, app).await.unwrap();
    });
    (port, requests, server)
  }

  #[tokio::test]
  async fn cdp_readiness_waits_for_a_page_socket_after_the_port_opens() {
    use axum::http::StatusCode;
    use std::sync::atomic::Ordering;

    let ready = json!([
      { "type": "page" },
      { "type": "page", "webSocketDebuggerUrl": "" },
      { "type": "service_worker", "webSocketDebuggerUrl": "ws://worker" },
      { "type": "page", "webSocketDebuggerUrl": "ws://ready-page" }
    ]);
    let (port, requests, server) = cdp_startup_server(vec![
      (StatusCode::OK, json!([])),
      (StatusCode::SERVICE_UNAVAILABLE, ready.clone()),
      (StatusCode::OK, json!([{ "type": "page" }])),
      (StatusCode::OK, ready),
    ])
    .await;
    let mut child = cdp_test_process();
    let targets = tokio::time::timeout(
      Duration::from_secs(5),
      WayfernManager::new().wait_for_cdp_ready_while_running(
        port,
        &mut child,
        &BrowserLogTap::default(),
      ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 4);
    assert_eq!(targets.len(), 1);
    assert_eq!(
      targets[0].websocket_debugger_url.as_deref(),
      Some("ws://ready-page")
    );
    child.kill().await.unwrap();
    server.abort();
  }

  #[tokio::test]
  async fn a_browser_that_never_opens_cdp_is_reported_in_time() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let mut child = cdp_test_process();
    let pid = child.id().unwrap();
    let error = tokio::time::timeout(
      Duration::from_secs(10),
      WayfernManager::new().wait_for_cdp_ready_within(
        port,
        &mut child,
        &BrowserLogTap::default(),
        Duration::from_secs(1),
      ),
    )
    .await
    .expect("the wait ends at its limit")
    .unwrap_err();
    assert_eq!(code_of(&error.to_string()), "WAYFERN_NOT_RESPONDING");
    force_kill_process(pid);
  }

  #[tokio::test]
  async fn cdp_page_wait_stops_when_the_browser_exits() {
    use std::sync::atomic::Ordering;

    let (port, requests, server) =
      cdp_startup_server(vec![(axum::http::StatusCode::OK, json!([]))]).await;
    let mut child = cdp_test_process();
    let pid = child.id().unwrap();
    let stop = tokio::spawn(async move {
      while requests.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
      force_kill_process(pid);
    });
    let log = BrowserLogTap::default();
    log.push("Wayfern instance limit reached: 4 browsers are already running on this plan.".into());
    let error = tokio::time::timeout(
      Duration::from_secs(5),
      WayfernManager::new().wait_for_cdp_ready_while_running(port, &mut child, &log),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(
      code_of(&error.to_string()),
      "WAYFERN_INSTANCE_LIMIT_REACHED"
    );
    stop.await.unwrap();
    server.abort();
  }

  fn cdp(code: i64, message: &str) -> String {
    format!("CDP error: {}", json!({ "code": code, "message": message }))
  }

  fn code_of(coded: &str) -> String {
    serde_json::from_str::<serde_json::Value>(coded).unwrap()["code"]
      .as_str()
      .unwrap()
      .to_string()
  }

  #[test]
  fn every_browser_refusal_reaches_the_frontend_as_its_own_code() {
    for (message, expected) in [
      (
        "Custom fingerprints require a Donut Browser plan that includes them.",
        "WAYFERN_CUSTOM_FINGERPRINT_REQUIRES_PLAN",
      ),
      (
        "Fingerprint authorization service is temporarily unavailable. Retry the command.",
        "WAYFERN_PLAN_CHECK_UNAVAILABLE",
      ),
      (
        "Cross-OS fingerprinting authorization service is temporarily unavailable. Retry the command.",
        "WAYFERN_PLAN_CHECK_UNAVAILABLE",
      ),
      (
        "Cross-OS fingerprinting requires a paid plan. Provide a wayfernToken parameter.",
        "WAYFERN_CROSS_OS_REQUIRES_PLAN",
      ),
      (
        "Cross-OS fingerprinting authorization failed. Token is invalid or expired.",
        "WAYFERN_CROSS_OS_REQUIRES_PLAN",
      ),
      (
        "Temporary fingerprint generation limit reached.",
        "WAYFERN_GENERATION_LIMIT_REACHED",
      ),
      (
        "Fingerprint generation state is unavailable.",
        "WAYFERN_GENERATION_UNAVAILABLE",
      ),
      (
        "A fingerprint identity change is in progress",
        "WAYFERN_BROWSER_BUSY",
      ),
      (
        "Target closed during fingerprint identity change",
        "WAYFERN_BROWSER_BUSY",
      ),
      (
        "Fingerprint generation is not available for this profile.",
        "WAYFERN_FINGERPRINT_GENERATION_FAILED",
      ),
    ] {
      let coded = wayfern_failure(
        &cdp(-32000, message),
        "WAYFERN_FINGERPRINT_GENERATION_FAILED",
        Some("macos"),
      );
      assert_eq!(code_of(&coded), expected, "{message}");
    }

    let generic = wayfern_failure(
      &format!(
        "Failed to create identity: {}",
        cdp(-32000, "Some new refusal.")
      ),
      "WAYFERN_FINGERPRINT_GENERATION_FAILED",
      None,
    );
    assert_eq!(code_of(&generic), "WAYFERN_FINGERPRINT_GENERATION_FAILED");
    assert!(generic.contains("Some new refusal."));
    assert!(!generic.contains("-32000"));

    let already_coded = crate::backend_error("WAYFERN_INSTANCE_LIMIT_REACHED");
    assert_eq!(
      wayfern_failure(&already_coded, "WAYFERN_FINGERPRINT_APPLY_FAILED", None),
      already_coded
    );
  }

  #[test]
  fn launch_identity_refusals_keep_their_meaning() {
    for (reason, expected) in [
      (
        "a cross-OS claim requires a valid paid plan token; the browser is starting on its host device instead",
        "WAYFERN_CROSS_OS_REQUIRES_PLAN",
      ),
      (
        "another identity change already owns this profile",
        "WAYFERN_BROWSER_BUSY",
      ),
      ("the derived identity was not committed", "WAYFERN_BROWSER_BUSY"),
      (
        "the identity file carries no valid `identityId`",
        "WAYFERN_IDENTITY_REFUSED",
      ),
    ] {
      let coded = wayfern_failure(reason, "WAYFERN_IDENTITY_REFUSED", Some("windows"));
      assert_eq!(code_of(&coded), expected, "{reason}");
    }
  }

  #[cfg(windows)]
  fn exit_status(code: u32) -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code)
  }

  #[test]
  fn the_launch_payload_never_invents_a_location() {
    // The regression, and the whole reason the gate is allowed to say "nothing
    // was compared": the launcher used to insert `America/New_York` and offset
    // 300 into any fingerprint that declared no timezone. The browser then
    // presented a US clock behind whatever exit the profile routed through,
    // while the app told the user the timezone had never been compared, the
    // exact mismatch `fingerprint_consistency` exists to surface, manufactured
    // by the launcher and then hidden by it.
    for stored in [
      r#"{"platform": "Win32"}"#,
      // The legacy wrapper takes the same path.
      r#"{"fingerprint": {"platform": "Win32"}}"#,
    ] {
      let payload =
        WayfernManager::launch_fingerprint_payload(stored).expect("a stored fingerprint parses");
      let obj = payload.as_object().expect("stays an object");
      for invented in ["timezone", "timezoneOffset", "latitude", "longitude"] {
        assert!(
          !obj.contains_key(invented),
          "the launch payload invented {invented} for {stored}: {obj:?}"
        );
      }
      assert_eq!(obj.get("platform"), Some(&json!("Win32")));
    }
  }

  #[test]
  fn the_launch_payload_unwraps_the_legacy_shape_and_keeps_a_declared_location() {
    let payload = WayfernManager::launch_fingerprint_payload(
      r#"{"fingerprint": {"timezone": "Europe/Berlin", "timezoneOffset": -60,
                          "languages": "de-DE, de"}}"#,
    )
    .expect("the legacy wrapper parses");

    assert_eq!(payload["timezone"], json!("Europe/Berlin"));
    assert_eq!(payload["timezoneOffset"], json!(-60));
    // A comma-separated ladder still becomes the array the browser expects.
    assert_eq!(payload["languages"], json!(["de-DE", "de"]));
    // The wrapper itself must never reach the browser.
    assert!(payload.get("fingerprint").is_none());
  }

  #[test]
  fn the_launch_payload_refuses_unparsable_json() {
    assert!(WayfernManager::launch_fingerprint_payload("not json").is_err());
  }

  #[tokio::test]
  #[serial_test::serial]
  async fn an_ephemeral_profile_finds_the_browser_running_from_its_ram_dir() {
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("DONUTBROWSER_EPHEMERAL_ROOT", root.path());
    let profile = BrowserProfile {
      id: uuid::Uuid::new_v4(),
      browser: "wayfern".to_string(),
      ephemeral: true,
      ..Default::default()
    };
    let id = profile.id.to_string();
    let ram_dir = crate::ephemeral_dirs::create_ephemeral_dir(&id).unwrap();

    let manager = WayfernManager::new();
    manager.inner.lock().await.instances.insert(
      "launch".to_string(),
      WayfernInstance {
        process_id: Some(1),
        profile_path: Some(ram_dir.to_string_lossy().into_owned()),
        url: None,
        cdp_port: Some(9333),
        log_tap: BrowserLogTap::default(),
      },
    );

    assert_eq!(manager.cdp_port_for_profile(&profile).await, Some(9333));

    crate::ephemeral_dirs::remove_ephemeral_dir(&id);
    std::env::remove_var("DONUTBROWSER_EPHEMERAL_ROOT");
  }
}
