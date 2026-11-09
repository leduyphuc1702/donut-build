use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::browser::ProxySettings;
use crate::log_streak::Streak;
use crate::proxy_manager::PROXY_MANAGER;
use crate::settings_manager::{SettingsManager, StoredMcpRemoteKey};
use crate::sync;

/// Donut cloud's address. This build never calls it: the value stays only as
/// the marker that tells an account Donut cloud issued from one this fork's
/// control plane issued, so a stored legacy address can be recognized.
pub const DEFAULT_CLOUD_API_URL: &str = "https://api.donutbrowser.com";
pub const CLOUD_SYNC_URL: &str = "https://sync.donutbrowser.com";

/// The control plane this fork ships with. Every account call goes here when
/// no other server was saved and `DONUT_CLOUD_API_URL` is not set.
pub const BUILTIN_CONTROL_PLANE_URL: &str = "https://control.45-76-164-228.sslip.io";

/// The configured cloud API base, formatted on demand.
///
/// This is deliberately a `Display` value rather than a `&str`: the account
/// backend can move to a self-hosted control plane, and the many
/// `format!("{CLOUD_API_URL}/...")` call sites pick up that base without each
/// one knowing which server it talks to.
pub struct CloudApiBase;

impl std::fmt::Display for CloudApiBase {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&cloud_api_base())
  }
}

pub const CLOUD_API_URL: CloudApiBase = CloudApiBase;

static CLOUD_API_BASE: std::sync::LazyLock<std::sync::RwLock<String>> =
  std::sync::LazyLock::new(|| std::sync::RwLock::new(effective_cloud_api_base(None)));

/// Normalize a configured control-plane address, or `None` when it is not an
/// absolute HTTP(S) URL. A trailing slash and a trailing `/api` are accepted
/// because people paste either from a browser.
pub fn normalize_cloud_api_base(url: &str) -> Option<String> {
  let trimmed = url.trim().trim_end_matches('/');
  let trimmed = trimmed.strip_suffix("/api").unwrap_or(trimmed);
  let rest = trimmed
    .strip_prefix("https://")
    .or_else(|| trimmed.strip_prefix("http://"))?;
  if rest.is_empty() || rest.chars().any(char::is_whitespace) {
    return None;
  }
  Some(trimmed.to_string())
}

pub fn cloud_api_base() -> String {
  CLOUD_API_BASE
    .read()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
    .clone()
}

/// Whether the account comes from a control plane this deployment runs, rather
/// than Donut cloud. Cloud-only services branch on this to use their local
/// equivalent instead of asking for a route that cannot exist.
pub fn is_self_hosted_account() -> bool {
  cloud_api_base() != DEFAULT_CLOUD_API_URL
}

/// Whether `base` is Donut cloud's own address. A value stored by an older
/// build is ignored rather than honored: this fork has no route to it.
fn is_donut_cloud(base: &str) -> bool {
  base.trim_end_matches('/') == DEFAULT_CLOUD_API_URL
}

/// The base `set_cloud_api_base` will apply: the configured address, else
/// `DONUT_CLOUD_API_URL`, else this fork's control plane.
fn effective_cloud_api_base(configured: Option<&str>) -> String {
  configured
    .and_then(normalize_cloud_api_base)
    .filter(|base| !is_donut_cloud(base))
    .unwrap_or_else(|| {
      std::env::var("DONUT_CLOUD_API_URL")
        .ok()
        .and_then(|value| normalize_cloud_api_base(&value))
        .unwrap_or_else(|| BUILTIN_CONTROL_PLANE_URL.to_string())
    })
}

/// Point every account call at `url`. `None` restores `DONUT_CLOUD_API_URL`
/// when it is set, else this fork's control plane.
pub fn set_cloud_api_base(url: Option<&str>) {
  let next = effective_cloud_api_base(url);
  *CLOUD_API_BASE
    .write()
    .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
}

/// A build meant for a team ships the self-hosted account it adopts on first
/// launch. `DONUT_SELF_HOST_URL` / `DONUT_SELF_HOST_KEY` at runtime win over
/// the values baked in at compile time, so a developer can point one binary at
/// a local control plane without rebuilding.
const BAKED_SELF_HOST_URL: Option<&str> = option_env!("DONUT_SELF_HOST_URL");
const BAKED_SELF_HOST_KEY: Option<&str> = option_env!("DONUT_SELF_HOST_KEY");

fn non_empty(value: Option<&str>) -> Option<&str> {
  value.map(str::trim).filter(|value| !value.is_empty())
}

/// Resolve the address and key an install uses to mint the shared self-hosted
/// account. `configured_base` (what the person saved under Account → Server)
/// wins; a stock build with no key resolves to `None`, which keeps every
/// existing sign-in path unchanged.
fn provisioning_config(
  configured_base: &str,
  runtime_url: Option<&str>,
  runtime_key: Option<&str>,
  baked_url: Option<&str>,
  baked_key: Option<&str>,
) -> Option<(String, String)> {
  let key = non_empty(runtime_key).or_else(|| non_empty(baked_key))?;
  let base = if configured_base != DEFAULT_CLOUD_API_URL {
    configured_base.to_string()
  } else {
    normalize_cloud_api_base(non_empty(runtime_url).or_else(|| non_empty(baked_url))?)?
  };
  if base == DEFAULT_CLOUD_API_URL {
    return None;
  }
  Some((base, key.to_string()))
}

fn self_host_provisioning() -> Option<(String, String)> {
  let runtime_url = std::env::var("DONUT_SELF_HOST_URL").ok();
  let runtime_key = std::env::var("DONUT_SELF_HOST_KEY").ok();
  provisioning_config(
    &cloud_api_base(),
    runtime_url.as_deref(),
    runtime_key.as_deref(),
    BAKED_SELF_HOST_URL,
    BAKED_SELF_HOST_KEY,
  )
}

/// Point the account backend at the build's self-hosted control plane before
/// any background task reads it. Sign-in itself happens later, in the startup
/// cloud task; this only settles which server the session, sync source and
/// account page belong to.
pub fn apply_self_host_base() {
  if let Some((base, _)) = self_host_provisioning() {
    set_cloud_api_base(Some(base.as_str()));
  }
}

/// Default per-hour cap on local automation API / MCP requests, used when the
/// cloud API has not sent one.
const DEFAULT_REQUESTS_PER_HOUR: i64 = 100;

const ACCESS_TOKEN_REFRESH_MARGIN_SECS: i64 = 15 * 60;
const SYNC_TOKEN_REFRESH_MARGIN_SECS: i64 = 120;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

static ACCESS_TOKEN_REFRESH: Streak = Streak::new(module_path!(), "Access token refresh");
static SYNC_TOKEN_FETCH: Streak = Streak::new(module_path!(), "Sync token fetch");
static SERVER_SYNC_TOKEN_FETCH: Streak = Streak::new(module_path!(), "Own server sync token fetch");

/// Tokens for the customer's own sync server, by address. In memory only:
/// they last minutes and cost one request to replace.
static SERVER_SYNC_TOKENS: std::sync::LazyLock<
  std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(Default::default);
static PROFILE_REFRESH: Streak = Streak::new(module_path!(), "Cloud profile refresh");
static PROXY_CONFIG_FETCH: Streak = Streak::new(module_path!(), "Cloud proxy config fetch");
static PROXY_USAGE_FETCH: Streak = Streak::new(module_path!(), "Cloud proxy usage fetch");

/// What the last cloud proxy sync found, so an unchanged result is not
/// logged every ten minutes.
static CLOUD_PROXY_STATE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn cloud_proxy_state_changed(state: String) -> bool {
  let mut last = CLOUD_PROXY_STATE
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  if last.as_deref() == Some(state.as_str()) {
    return false;
  }
  *last = Some(state);
  true
}

pub(crate) fn parse_retry_after(value: &str, now: chrono::DateTime<Utc>) -> Option<Duration> {
  let value = value.trim();
  if let Ok(seconds) = value.parse::<u64>() {
    return Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER));
  }
  let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
  let seconds = u64::try_from((at.with_timezone(&Utc) - now).num_seconds()).unwrap_or(0);
  Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER))
}

pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
  headers
    .get(reqwest::header::RETRY_AFTER)
    .and_then(|value| value.to_str().ok())
    .and_then(|value| parse_retry_after(value, Utc::now()))
    .unwrap_or(DEFAULT_RETRY_AFTER)
}

fn is_refused_refresh(error: &str) -> bool {
  error.contains("(401") || error.contains("No refresh token")
}

#[derive(Debug)]
pub(crate) enum AuthorizedError {
  SignedOut,
  RateLimited(Duration),
  Transport(String),
}

impl AuthorizedError {
  fn from_token_error(error: String) -> Self {
    if error.contains("Not logged in") || is_refused_refresh(&error) {
      Self::SignedOut
    } else {
      Self::Transport(error)
    }
  }
}

impl std::fmt::Display for AuthorizedError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::SignedOut => write!(f, "not signed in"),
      Self::RateLimited(wait) => write!(f, "rate limited for {}s", wait.as_secs()),
      Self::Transport(error) => write!(f, "{error}"),
    }
  }
}

/// Capability + limit set the account is entitled to, derived from its plan.
/// Mirrors the entitlement set the cloud API sends. Features are gated on these
/// flags instead of a single "is paid?" boolean, so a plan like "solo" (cloud
/// backup + nightly cookie bot, no automation, no fingerprint editing, no
/// hands-on remote session) is just data here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entitlements {
  #[serde(default)]
  pub active: bool,
  #[serde(rename = "browserAutomation", default)]
  pub browser_automation: bool,
  #[serde(rename = "crossOsFingerprints", default)]
  pub cross_os_fingerprints: bool,
  #[serde(rename = "cloudBackup", default)]
  pub cloud_backup: bool,
  #[serde(rename = "teamCollaboration", default)]
  pub team_collaboration: bool,
  /// Overnight profile warming on a leased remote host. Present on the wire
  /// since the cookie-bot release; a field missing here is silently dropped on
  /// the way to the UI, which is why every mirror of this struct has to move
  /// together.
  #[serde(rename = "cookieBot", default)]
  pub cookie_bot: bool,
  /// Whether the plan may open a HANDS-ON remote session. Distinct from
  /// `cookie_bot`: solo funds a nightly bot out of its remote hours but may not
  /// drive a remote browser itself, so anything that offers interactive remote
  /// control must read THIS rather than `remote_browser_hours > 0`.
  #[serde(rename = "remoteInteractive", default)]
  pub remote_interactive: bool,
  /// Whether the plan may drive THIS desktop from Donut cloud: the remote MCP
  /// endpoint and the API in front of it.
  ///
  /// Read only by the UI. The bridge itself never gates on this: the relay
  /// decides who may send work, and a cached entitlement that is a refresh
  /// cycle out of date must not be what refuses a customer their own machine.
  #[serde(rename = "remoteControl", default)]
  pub remote_control: bool,
  /// Recipes Donut cloud runs on a calendar. Read only by the UI: the cloud
  /// checks the plan when a schedule is saved and again at every fire, so a
  /// stale or edited copy here can open a screen but never run anything.
  #[serde(rename = "scheduledAutomation", default)]
  pub scheduled_automation: bool,
  /// Natural-language tasks and routines that the agent engine runs. A hidden
  /// rollout flag the cloud sets per account, on any plan including free.
  /// Never derived from the plan: a backend that omits it means `false`.
  #[serde(default)]
  pub agent: bool,
  #[serde(rename = "profileLimit", default)]
  pub profile_limit: i64,
  #[serde(rename = "requestsPerHour", default)]
  pub requests_per_hour: i64,
  /// Per-seat monthly remote-session allowance. Reporting only — a team pools
  /// it across seats, so the spendable figure comes from the quota route.
  #[serde(rename = "remoteBrowserHours", default)]
  pub remote_browser_hours: i64,
}

/// Local fallback mirror of the backend plan -> capability matrix, used only when
/// the server hasn't sent an entitlements object (older cached state / backend).
fn derive_entitlements(
  plan: &str,
  plan_period: Option<&str>,
  subscription_status: &str,
  profile_limit: i64,
) -> Entitlements {
  let active =
    plan != "free" && (subscription_status == "active" || plan_period == Some("lifetime"));
  if !active {
    return Entitlements {
      active: false,
      browser_automation: false,
      cross_os_fingerprints: false,
      cloud_backup: false,
      team_collaboration: false,
      cookie_bot: false,
      remote_interactive: false,
      remote_control: false,
      scheduled_automation: false,
      agent: false,
      profile_limit: 0,
      requests_per_hour: 0,
      remote_browser_hours: 0,
    };
  }
  // Tuple order: (browser_automation, cross_os_fingerprints, cloud_backup,
  // team_collaboration, cookie_bot, remote_interactive, remote_control).
  //
  // pro and any unrecognized paid plan -> pro-level (never team). Solo is the
  // one row where cookie_bot and browser_automation disagree, which is why
  // cookie_bot can no longer be derived from browser_automation below.
  //
  // remote_control belongs to solo (MCP without browser automation), pro,
  // team and enterprise, and is withheld from the unrecognized row rather
  // than granted with the rest. Everything else here defaults
  // generous so a comped account is never locked out of what it is paying for;
  // an internet-facing hook into this machine is the one capability where
  // guessing "probably yes" is not the safe direction to guess in.
  let (
    browser_automation,
    cross_os_fingerprints,
    cloud_backup,
    team_collaboration,
    cookie_bot,
    remote_interactive,
    remote_control,
  ) = match plan {
    "solo" => (false, false, true, false, true, false, true),
    "enterprise" => (true, true, true, true, true, true, true),
    "team" => (true, true, true, true, true, true, true),
    "pro" => (true, true, true, false, true, true, true),
    _ => (true, true, true, false, true, true, false),
  };
  Entitlements {
    active,
    browser_automation,
    cross_os_fingerprints,
    cloud_backup,
    team_collaboration,
    cookie_bot,
    remote_interactive,
    remote_control,
    // Every plan that is sold with it; an unknown one waits for the server.
    scheduled_automation: matches!(plan, "solo" | "pro" | "team" | "enterprise"),
    agent: false,
    profile_limit,
    requests_per_hour: if browser_automation {
      DEFAULT_REQUESTS_PER_HOUR
    } else {
      0
    },
    // Deliberately 0 in the fallback: the allowance is the server's to state and
    // guessing it here would show a customer hours they may not have.
    remote_browser_hours: 0,
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudUser {
  pub id: String,
  pub email: String,
  pub plan: String,
  #[serde(rename = "planPeriod")]
  pub plan_period: Option<String>,
  #[serde(rename = "subscriptionStatus")]
  pub subscription_status: String,
  #[serde(rename = "profileLimit")]
  pub profile_limit: i64,
  #[serde(rename = "cloudProfilesUsed")]
  pub cloud_profiles_used: i64,
  #[serde(rename = "proxyBandwidthLimitMb")]
  pub proxy_bandwidth_limit_mb: i64,
  #[serde(rename = "proxyBandwidthUsedMb")]
  pub proxy_bandwidth_used_mb: i64,
  #[serde(rename = "proxyBandwidthExtraMb", default)]
  pub proxy_bandwidth_extra_mb: i64,
  #[serde(rename = "teamId", default)]
  pub team_id: Option<String>,
  #[serde(rename = "teamName", default)]
  pub team_name: Option<String>,
  #[serde(rename = "teamRole", default)]
  pub team_role: Option<String>,
  /// The plan this account is served under. A team member's `plan` stays
  /// `"free"` (the owner pays) while the backend resolves this to the owner's
  /// tier. `default` keeps the login response and older backends deserializing;
  /// read it through `effective_plan()`.
  #[serde(rename = "effectivePlan", default)]
  pub effective_plan: Option<String>,
  // This desktop session's position among the user's active devices, oldest
  // first, as the cloud API reports it. Shown in the UI. `default` keeps older
  // login/state payloads (which lack these fields) deserializing cleanly.
  #[serde(rename = "deviceOrdinal", default)]
  pub device_ordinal: Option<i64>,
  #[serde(rename = "deviceCount", default)]
  pub device_count: Option<i64>,
  #[serde(rename = "isPrimaryDevice", default)]
  pub is_primary_device: Option<bool>,
  /// Capability/limit set derived from the plan by the backend. `default` (None)
  /// keeps older login/state payloads deserializing; resolve via `entitlements()`.
  #[serde(default)]
  pub entitlements: Option<Entitlements>,
  /// Whether a self-hosted control plane issued this account. The control
  /// plane sends it; `cloud_get_user` also stamps the current answer, so a
  /// state cached by a build without this field still tells the UI which
  /// local services are available.
  #[serde(rename = "selfHosted", default)]
  pub self_hosted: Option<bool>,
}

impl CloudUser {
  /// The plan the account is actually served under: `effectivePlan` when the
  /// backend sent one, else the row's own `plan`. Gates that ask "is this a
  /// paid / team account" read this; billing-only surfaces keep `plan`.
  pub fn effective_plan(&self) -> &str {
    self.effective_plan.as_deref().unwrap_or(&self.plan)
  }

  /// Authoritative entitlements: the server-sent set when present, else derived
  /// locally from the plan fields (keeps older cached state / backends working).
  pub fn entitlements(&self) -> Entitlements {
    if let Some(e) = &self.entitlements {
      // Returned verbatim, INCLUDING the `#[serde(default)]` false that a
      // backend older than this release leaves on `cookie_bot` /
      // `remote_interactive`. Repairing it here is impossible anyway — serde's
      // default erases the difference between "sent false" and "not sent" — and
      // it is not this layer's job: nothing in Rust gates on either flag, and
      // `getEntitlements()` in `src/lib/entitlements.ts` fills both gaps at the
      // single point every UI consumer already goes through.
      return e.clone();
    }
    derive_entitlements(
      &self.plan,
      self.plan_period.as_deref(),
      &self.subscription_status,
      self.profile_limit,
    )
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudAuthState {
  pub user: CloudUser,
  pub logged_in_at: String,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeChallengeResponse {
  #[serde(rename = "challengeId")]
  challenge_id: String,
  prefix: String,
  difficulty: u32,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeExchangeResponse {
  #[serde(rename = "accessToken")]
  access_token: String,
  #[serde(rename = "refreshToken")]
  refresh_token: String,
  user: CloudUser,
}

#[derive(Debug, Deserialize)]
struct RefreshTokenResponse {
  #[serde(rename = "accessToken")]
  access_token: String,
  #[serde(rename = "refreshToken")]
  refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct SyncTokenResponse {
  #[serde(rename = "syncToken")]
  sync_token: String,
}

/// Prefix of a remote MCP credential. Only a key carrying this prefix may ever
/// be stored here; a credential of any other kind is rejected.
pub const MCP_KEY_PREFIX: &str = "dmk_";

/// A freshly minted remote MCP credential. The plaintext `key` is shown by the
/// server exactly once, in this response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpKeyGrant {
  pub id: String,
  pub token_prefix: String,
  pub key: String,
}

/// What a key endpoint answered once the request itself went through.
///
/// `api_call_with_retry` reads a 401 out of the ERROR string and refreshes the
/// session once, so only a 401 (and a transport failure) may be an `Err`. Every
/// other refusal travels as a value, so it reaches the code mapping below
/// instead of being mistaken for a dead session.
enum McpKeyAnswer<T> {
  Granted(T),
  Refused { status: u16, body: String },
}

/// The `{"code"}` the UI shows for a refused credential mint.
///
/// A 409 on `POST /api/mcp/keys` is the per-account cap, whether or not the
/// server bothered to name it; everything else is "not right now", carrying
/// the server's message as the detail so the log says why.
fn mcp_key_refusal(status: u16, body: &str) -> String {
  if status == 409 {
    return crate::backend_error("MCP_REMOTE_KEY_LIMIT");
  }
  let message = serde_json::from_str::<serde_json::Value>(body)
    .ok()
    .and_then(|v| {
      v.get("message")
        .and_then(|m| m.as_str())
        .map(std::string::ToString::to_string)
    })
    .unwrap_or_else(|| body.to_string());
  crate::backend_error_with_detail("MCP_REMOTE_KEY_UNAVAILABLE", format!("{status}: {message}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationItem {
  pub code: String,
  pub name: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CloudProxyConfigResponse {
  host: String,
  port: u16,
  username: Option<String>,
  password: Option<String>,
  protocol: String,
  #[serde(rename = "bandwidthLimitMb")]
  bandwidth_limit_mb: i64,
  #[serde(rename = "bandwidthUsedMb")]
  bandwidth_used_mb: i64,
}

pub struct CloudAuthManager {
  client: crate::app_proxy::SharedClient,
  state: Mutex<Option<CloudAuthState>>,
  refresh_lock: tokio::sync::Mutex<()>,
  refresh_deferred_until: std::sync::Mutex<Option<Instant>>,
}

/// Tell what depends on the team seat that the signed-in account changed.
fn observe_account(user: Option<&CloudUser>) {
  crate::team_roles::observe(user);
  crate::team_activity::observe(user);
}

pub static CLOUD_AUTH: std::sync::LazyLock<CloudAuthManager> =
  std::sync::LazyLock::new(CloudAuthManager::new);

impl CloudAuthManager {
  fn new() -> Self {
    let state = Self::load_auth_state_from_disk();
    observe_account(state.as_ref().map(|auth| &auth.user));
    // Bound every cloud API call so no single slow / hung request can stall
    // the startup chain (sync-token → proxy-config), which otherwise waits on
    // whichever endpoint is slowest.
    let client = crate::app_proxy::SharedClient::new(|builder| {
      builder
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(5))
    });
    Self {
      client,
      state: Mutex::new(state),
      refresh_lock: tokio::sync::Mutex::new(()),
      refresh_deferred_until: std::sync::Mutex::new(None),
    }
  }

  // --- Settings directory (reuse SettingsManager path) ---

  fn get_settings_dir() -> PathBuf {
    SettingsManager::instance().get_settings_dir()
  }

  // --- Encrypted file storage (shared with settings_manager.rs via crate::vault) ---

  fn magic(header: &[u8; 5]) -> [u8; 6] {
    let mut magic = [0u8; 6];
    magic[..5].copy_from_slice(header);
    magic[5] = 2;
    magic
  }

  fn encrypt_and_store(file_path: &Path, header: &[u8; 5], data: &str) -> Result<(), String> {
    crate::vault::seal(file_path, &Self::magic(header), data)
  }

  fn decrypt_from_file(file_path: &Path, header: &[u8; 5]) -> Result<Option<String>, String> {
    crate::vault::open(file_path, &Self::magic(header))
  }

  // --- Token storage methods ---

  fn store_access_token(token: &str) -> Result<(), String> {
    let path = Self::get_settings_dir().join("cloud_access_token.dat");
    Self::encrypt_and_store(&path, b"DBCAT", token)
  }

  pub(crate) fn load_access_token() -> Result<Option<String>, String> {
    let path = Self::get_settings_dir().join("cloud_access_token.dat");
    Self::decrypt_from_file(&path, b"DBCAT")
  }

  fn store_refresh_token(token: &str) -> Result<(), String> {
    let path = Self::get_settings_dir().join("cloud_refresh_token.dat");
    Self::encrypt_and_store(&path, b"DBCRT", token)
  }

  fn load_refresh_token() -> Result<Option<String>, String> {
    let path = Self::get_settings_dir().join("cloud_refresh_token.dat");
    Self::decrypt_from_file(&path, b"DBCRT")
  }

  fn store_cloud_sync_token(token: &str) -> Result<(), String> {
    let path = Self::get_settings_dir().join("cloud_sync_token.dat");
    Self::encrypt_and_store(&path, b"DBCST", token)
  }

  fn load_cloud_sync_token() -> Result<Option<String>, String> {
    let path = Self::get_settings_dir().join("cloud_sync_token.dat");
    Self::decrypt_from_file(&path, b"DBCST")
  }

  pub(crate) fn discard_cloud_sync_token(refused: &str) {
    if let Ok(Some(cached)) = Self::load_cloud_sync_token() {
      if cached == refused {
        let _ = fs::remove_file(Self::get_settings_dir().join("cloud_sync_token.dat"));
      }
    }
  }

  fn store_auth_state(state: &CloudAuthState) -> Result<(), String> {
    observe_account(Some(&state.user));
    let path = Self::get_settings_dir().join("cloud_auth_state.json");
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }
    let json =
      serde_json::to_string_pretty(state).map_err(|e| format!("Failed to serialize: {e}"))?;
    fs::write(&path, json).map_err(|e| format!("Failed to write auth state: {e}"))?;
    crate::app_dirs::restrict_to_owner(&path);
    Ok(())
  }

  fn load_auth_state_from_disk() -> Option<CloudAuthState> {
    let path = Self::get_settings_dir().join("cloud_auth_state.json");
    if !path.exists() {
      return None;
    }
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
  }

  fn delete_all_cloud_files() {
    let dir = Self::get_settings_dir();
    let files = [
      "cloud_access_token.dat",
      "cloud_refresh_token.dat",
      "cloud_sync_token.dat",
      "cloud_auth_state.json",
    ];
    for f in &files {
      let path = dir.join(f);
      if path.exists() {
        let _ = fs::remove_file(path);
      }
    }
  }

  /// Whether this install has already had its first self-hosted launch. The
  /// marker outlives the session files, so an explicit sign-out is not undone
  /// by the next start.
  fn self_host_provisioned() -> bool {
    Self::get_settings_dir()
      .join("self_host_provisioned")
      .exists()
  }

  fn remember_self_host_provisioned() {
    let path = Self::get_settings_dir().join("self_host_provisioned");
    if path.exists() {
      return;
    }
    if let Some(parent) = path.parent() {
      if let Err(e) = fs::create_dir_all(parent) {
        log::warn!("Self-hosted marker directory failed err=\"{e}\"");
        return;
      }
    }
    if let Err(e) = fs::write(&path, b"1") {
      log::warn!("Self-hosted marker write failed err=\"{e}\"");
    }
  }

  // --- JWT expiry check ---

  /// A JWT's claims, unverified.
  fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
      return None;
    }

    use base64::{engine::general_purpose, Engine as _};
    let payload = match general_purpose::URL_SAFE_NO_PAD.decode(parts[1]) {
      Ok(bytes) => bytes,
      Err(_) => {
        // Try standard base64 with padding
        general_purpose::STANDARD.decode(parts[1]).ok()?
      }
    };

    serde_json::from_slice(&payload).ok()
  }

  fn jwt_expiry(token: &str) -> Option<i64> {
    Self::jwt_claims(token)?.get("exp").and_then(|v| v.as_i64())
  }

  /// Whether `token` is one the Donut API bound to `server`. Anything else,
  /// such as a token for Donut cloud from an API that does not know the
  /// `server` field yet, must never be sent to a third party's server.
  fn is_server_sync_token_for(token: &str, server: &str) -> bool {
    Self::jwt_claims(token).is_some_and(|claims| {
      claims.get("scope").and_then(|v| v.as_str()) == Some("server-sync")
        && claims.get("aud").and_then(|v| v.as_str()) == Some(server)
    })
  }

  fn jwt_expires_within(token: &str, margin_secs: i64, now: i64) -> bool {
    Self::jwt_expiry(token).is_none_or(|exp| exp - now < margin_secs)
  }

  fn is_jwt_expiring_soon(token: &str) -> bool {
    Self::jwt_expires_within(
      token,
      SYNC_TOKEN_REFRESH_MARGIN_SECS,
      Utc::now().timestamp(),
    )
  }

  fn access_token_needs_refresh(token: &str) -> bool {
    Self::jwt_expires_within(
      token,
      ACCESS_TOKEN_REFRESH_MARGIN_SECS,
      Utc::now().timestamp(),
    )
  }

  // --- API methods ---

  pub async fn exchange_device_code(&self, code: &str) -> Result<CloudAuthState, String> {
    let challenge_url = format!("{CLOUD_API_URL}/api/auth/device-code/challenge");
    let challenge_response = self
      .client
      .get()
      .post(&challenge_url)
      .send()
      .await
      .map_err(|e| format!("Failed to fetch challenge: {e}"))?;

    if !challenge_response.status().is_success() {
      let status = challenge_response.status();
      let body = challenge_response.text().await.unwrap_or_default();
      return Err(format!("Challenge request failed ({status}): {body}"));
    }

    let challenge: DeviceCodeChallengeResponse = challenge_response
      .json()
      .await
      .map_err(|e| format!("Failed to parse challenge: {e}"))?;

    let nonce = solve_pow(&challenge.prefix, challenge.difficulty)
      .ok_or_else(|| "Failed to solve proof-of-work".to_string())?;

    let exchange_url = format!("{CLOUD_API_URL}/api/auth/device-code/exchange");
    let response = self
      .client
      .get()
      .post(&exchange_url)
      .json(&serde_json::json!({
        "code": code,
        "challengeId": challenge.challenge_id,
        "nonce": nonce,
      }))
      .send()
      .await
      .map_err(|e| format!("Failed to verify code: {e}"))?;

    if !response.status().is_success() {
      let status = response.status();
      let body = response.text().await.unwrap_or_default();
      // The cloud API returns { message, code, … } for 4xx. Surface the
      // human-readable message rather than the raw JSON so the sign-in screen
      // is clear.
      let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
          v.get("message")
            .and_then(|m| m.as_str())
            .map(std::string::ToString::to_string)
        })
        .unwrap_or_else(|| format!("Login failed ({status})"));
      return Err(message);
    }

    let result: DeviceCodeExchangeResponse = response
      .json()
      .await
      .map_err(|e| format!("Failed to parse response: {e}"))?;

    // Store tokens
    Self::store_access_token(&result.access_token)?;
    Self::store_refresh_token(&result.refresh_token)?;

    // Verify tokens survived the encrypt/decrypt round-trip
    match Self::load_access_token() {
      Ok(Some(loaded)) if loaded == result.access_token => {}
      Ok(Some(loaded)) => {
        log::error!(
          "Access token corrupted by store and load stored_len={} loaded_len={}",
          result.access_token.len(),
          loaded.len()
        );
      }
      Ok(None) => {
        log::error!("Access token missing right after store");
      }
      Err(e) => {
        log::error!("Access token not readable after store err=\"{e}\"");
      }
    }

    // Build and persist auth state
    let auth_state = CloudAuthState {
      user: result.user,
      logged_in_at: Utc::now().to_rfc3339(),
    };
    Self::store_auth_state(&auth_state)?;

    log::info!(
      "Signed in plan={} subscription_status={} proxy_bandwidth_limit_mb={}",
      auth_state.user.plan,
      auth_state.user.subscription_status,
      auth_state.user.proxy_bandwidth_limit_mb
    );

    // Update in-memory state
    let mut state = self.state.lock().await;
    *state = Some(auth_state.clone());

    Ok(auth_state)
  }

  /// Adopt the build's shared self-hosted account when this install has no
  /// session yet. The control plane mints it from the provisioning key in the
  /// build, so a first launch is the pro account without a device code.
  ///
  /// Returns whether a session exists afterwards. A build without a key, or
  /// one already signed in, is left untouched.
  pub async fn provision_self_hosted(&self) -> bool {
    let Some((base, key)) = self_host_provisioning() else {
      return false;
    };
    if self.is_logged_in().await {
      Self::remember_self_host_provisioned();
      return true;
    }
    // A person who signed out on a provisioned install stays signed out: the
    // marker records that this install has already had its first launch, so
    // the next start does not force the account back in.
    if Self::self_host_provisioned() {
      return false;
    }
    // The account belongs to this server, not to Donut cloud; every later
    // call (sync token, profile refresh) must use the same base.
    set_cloud_api_base(Some(base.as_str()));

    let url = format!("{base}/api/auth/auto");
    let response = match self
      .client
      .get()
      .post(&url)
      .json(&serde_json::json!({ "key": key }))
      .send()
      .await
    {
      Ok(response) => response,
      Err(e) => {
        log::warn!("Self-hosted account sign-in failed err=\"{e}\"");
        return false;
      }
    };

    if !response.status().is_success() {
      let status = response.status();
      let body = response.text().await.unwrap_or_default();
      log::warn!(
        "Self-hosted account sign-in refused status={status} detail=\"{}\"",
        body.trim()
      );
      return false;
    }

    let login: DeviceCodeExchangeResponse = match response.json().await {
      Ok(login) => login,
      Err(e) => {
        log::warn!("Self-hosted account sign-in response invalid err=\"{e}\"");
        return false;
      }
    };

    if let Err(e) = Self::store_access_token(&login.access_token) {
      log::error!("Self-hosted access token store failed err=\"{e}\"");
      return false;
    }
    if let Err(e) = Self::store_refresh_token(&login.refresh_token) {
      log::error!("Self-hosted refresh token store failed err=\"{e}\"");
      return false;
    }

    let auth_state = CloudAuthState {
      user: login.user,
      logged_in_at: Utc::now().to_rfc3339(),
    };
    if let Err(e) = Self::store_auth_state(&auth_state) {
      log::error!("Self-hosted account state store failed err=\"{e}\"");
    }
    *self.state.lock().await = Some(auth_state.clone());
    Self::remember_self_host_provisioned();

    log::info!(
      "Self-hosted account signed in plan={} entitlements_active={}",
      auth_state.user.effective_plan(),
      auth_state.user.entitlements().active
    );
    let _ = crate::events::emit_empty("cloud-auth-changed");
    true
  }

  pub async fn refresh_access_token(&self) -> Result<(), String> {
    let _guard = self.refresh_lock.lock().await;
    self.refresh_while_locked().await
  }

  async fn refresh_while_locked(&self) -> Result<(), String> {
    if let Some(wait) = self.refresh_deferral() {
      return Err(format!(
        "Token refresh deferred for {}s (429 Too Many Requests)",
        wait.as_secs()
      ));
    }
    let refresh_token =
      Self::load_refresh_token()?.ok_or_else(|| "No refresh token stored".to_string())?;

    let url = format!("{CLOUD_API_URL}/api/auth/token/refresh");
    let response = self
      .client
      .get()
      .post(&url)
      .json(&serde_json::json!({ "refreshToken": refresh_token }))
      .send()
      .await
      .map_err(|e| format!("Failed to refresh token: {e}"))?;

    if !response.status().is_success() {
      let status = response.status();
      log::debug!("Access token refresh refused status={status}");
      if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        self.defer_refresh(retry_after(response.headers()));
      } else if status == reqwest::StatusCode::UNAUTHORIZED {
        Self::forget_refresh_token();
        tauri::async_runtime::spawn(async {
          CLOUD_AUTH.invalidate_session().await;
        });
      }
      return Err(format!("Token refresh failed ({status})"));
    }

    let result: RefreshTokenResponse = response
      .json()
      .await
      .map_err(|e| format!("Failed to parse response: {e}"))?;

    Self::store_access_token(&result.access_token)?;
    Self::store_refresh_token(&result.refresh_token)?;

    log::debug!("Access token refreshed");
    Ok(())
  }

  fn refresh_deferral(&self) -> Option<Duration> {
    let until = (*self
      .refresh_deferred_until
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner))?;
    until.checked_duration_since(Instant::now())
  }

  fn defer_refresh(&self, wait: Duration) {
    *self
      .refresh_deferred_until
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now() + wait);
  }

  fn forget_refresh_token() {
    let path = Self::get_settings_dir().join("cloud_refresh_token.dat");
    if path.exists() {
      let _ = fs::remove_file(path);
    }
  }

  async fn ensure_fresh_access_token(&self) -> Result<String, String> {
    let token = Self::load_access_token()?.ok_or_else(|| "Not logged in".to_string())?;
    if !Self::access_token_needs_refresh(&token) {
      return Ok(token);
    }
    let _guard = self.refresh_lock.lock().await;
    let current = Self::load_access_token()?.ok_or_else(|| "Not logged in".to_string())?;
    if current != token && !Self::access_token_needs_refresh(&current) {
      return Ok(current);
    }
    self.refresh_while_locked().await?;
    Self::load_access_token()?.ok_or_else(|| "Not logged in after refresh".to_string())
  }

  async fn usable_access_token(&self) -> Result<String, String> {
    let token = Self::load_access_token()?.ok_or_else(|| "Not logged in".to_string())?;
    if !Self::access_token_needs_refresh(&token) {
      return Ok(token);
    }
    match self.ensure_fresh_access_token().await {
      Ok(fresh) => Ok(fresh),
      Err(e) if e.contains("(401") => Err(e),
      Err(e) => {
        log::debug!("Early access token refresh not done err=\"{e}\"");
        Ok(token)
      }
    }
  }

  async fn refresh_after_rejection(&self, rejected: &str) -> Result<String, String> {
    let _guard = self.refresh_lock.lock().await;
    if let Some(current) = Self::load_access_token()? {
      if current != rejected && !current.is_empty() {
        log::debug!("Access token already refreshed by another caller");
        return Ok(current);
      }
    }
    self.refresh_while_locked().await?;
    Self::load_access_token()?.ok_or_else(|| "Not logged in after refresh".to_string())
  }

  pub(crate) async fn authorized_request<F>(
    &self,
    build: F,
  ) -> Result<reqwest::Response, AuthorizedError>
  where
    F: Fn(&Client, &str) -> reqwest::RequestBuilder + Send,
  {
    let token = self
      .usable_access_token()
      .await
      .map_err(AuthorizedError::from_token_error)?;
    let mut response = build(&self.client.get(), &token)
      .send()
      .await
      .map_err(|e| AuthorizedError::Transport(e.to_string()))?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
      let token = self
        .refresh_after_rejection(&token)
        .await
        .map_err(AuthorizedError::from_token_error)?;
      response = build(&self.client.get(), &token)
        .send()
        .await
        .map_err(|e| AuthorizedError::Transport(e.to_string()))?;
    }
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
      return Err(AuthorizedError::RateLimited(retry_after(
        response.headers(),
      )));
    }
    Ok(response)
  }

  /// Invalidate the session: clear all auth state and notify the frontend.
  /// Only call this when the session is definitively dead (explicit logout
  /// or repeated background refresh failures).
  pub async fn invalidate_session(&self) {
    log::warn!("Session invalidated, auth state cleared");
    PROXY_MANAGER.remove_cloud_proxy();
    // Same reason `logout` does it: left running, the bridge reconnects with a
    // credential that no longer exists, fails, and backs off into an
    // "unauthorized" the account page shows to somebody whose session simply
    // expired. This is the AUTOMATIC twin of logout, reached when the
    // background refresh loop gives up, and it was the one teardown path that
    // did not close the bridge.
    crate::mcp_remote::stop(None);
    // The stored `dmk_` key belongs to the account that just left this
    // machine, and the agent configs would keep presenting it. There is no
    // session left to revoke it with, so only the local copy goes; the key
    // itself is retired by the next mint, or from the account page.
    Self::forget_mcp_key_locally("after the session expired");
    self.clear_auth().await;
    crate::team_lock::PROFILE_LOCK.disconnect().await;
    sync::stop_subscription();
    crate::remote_session::stop_session_events();
    crate::agent_console::reset();
    let _ = crate::events::emit_empty("cloud-auth-expired");
  }

  /// Ask the server whether this account may drive a desktop remotely.
  ///
  /// The AUTHORITATIVE answer, and the only one that is correct for a team
  /// member: a team member's locally cached plan does not describe what their
  /// seat is entitled to, so the server is asked rather than guessed at.
  ///
  /// It cannot be inferred from the socket either. A connected bridge only
  /// proves the plan is active, and nothing about this capability.
  pub async fn fetch_remote_control_entitlement(&self) -> Result<bool, String> {
    self
      .api_call_with_retry(|access_token| {
        let url = format!("{CLOUD_API_URL}/api/mcp/status");
        let client = self.client.get();
        async move {
          let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|e| format!("Failed to read remote-control status: {e}"))?;

          if !response.status().is_success() {
            let status = response.status();
            return Err(format!("Remote-control status failed ({status})"));
          }

          let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse remote-control status: {e}"))?;

          Ok(
            body
              .get("entitled")
              .and_then(serde_json::Value::as_bool)
              .unwrap_or(false),
          )
        }
      })
      .await
  }

  /// Mint a remote MCP credential for this account.
  ///
  /// The server caps how many live keys one account may hold and answers a mint
  /// past that cap with a 409; the rotation command handles that by retiring the
  /// key it is replacing first. The plaintext in the answer is the only copy
  /// there will ever be.
  pub async fn create_mcp_key(&self, label: &str) -> Result<McpKeyGrant, String> {
    let answer = self
      .api_call_with_retry(|access_token| {
        let url = format!("{CLOUD_API_URL}/api/mcp/keys");
        let client = self.client.get();
        let label = label.to_string();
        async move {
          let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .json(&serde_json::json!({ "label": label }))
            .send()
            .await
            .map_err(|e| format!("Failed to request an MCP credential: {e}"))?;
          let status = response.status();
          let body = response.text().await.unwrap_or_default();
          if status.as_u16() == 401 {
            // Worded so `api_call_with_retry` recognises it and refreshes once.
            return Err(format!(
              "MCP credential request failed (401 Unauthorized): {body}"
            ));
          }
          if !status.is_success() {
            return Ok(McpKeyAnswer::Refused {
              status: status.as_u16(),
              body,
            });
          }
          serde_json::from_str::<McpKeyGrant>(&body)
            .map(McpKeyAnswer::Granted)
            .map_err(|e| format!("Failed to parse the MCP credential response: {e}"))
        }
      })
      .await
      .map_err(|e| crate::backend_error_with_detail("MCP_REMOTE_KEY_UNAVAILABLE", e))?;

    match answer {
      McpKeyAnswer::Granted(grant) if grant.key.starts_with(MCP_KEY_PREFIX) => Ok(grant),
      // A credential of any other kind must never be stored as if it were ours,
      // however well it would authenticate.
      McpKeyAnswer::Granted(_) => Err(crate::backend_error_with_detail(
        "MCP_REMOTE_KEY_UNAVAILABLE",
        "the server issued a credential of an unexpected shape",
      )),
      McpKeyAnswer::Refused { status, body } => Err(mcp_key_refusal(status, &body)),
    }
  }

  /// Revoke a remote MCP credential by id. A key the server no longer knows
  /// (404) counts as revoked: that is the state the caller wanted.
  pub async fn revoke_mcp_key(&self, key_id: &str) -> Result<(), String> {
    let answer = self
      .api_call_with_retry(|access_token| {
        let url = format!(
          "{CLOUD_API_URL}/api/mcp/keys/{}",
          urlencoding::encode(key_id)
        );
        let client = self.client.get();
        async move {
          let response = client
            .delete(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|e| format!("Failed to revoke the MCP credential: {e}"))?;
          let status = response.status();
          if status.as_u16() == 401 {
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
              "MCP credential revocation failed (401 Unauthorized): {body}"
            ));
          }
          if status.is_success() || status.as_u16() == 404 {
            return Ok(McpKeyAnswer::Granted(()));
          }
          Ok(McpKeyAnswer::Refused {
            status: status.as_u16(),
            body: response.text().await.unwrap_or_default(),
          })
        }
      })
      .await
      .map_err(|e| crate::backend_error_with_detail("MCP_REMOTE_KEY_UNAVAILABLE", e))?;

    match answer {
      McpKeyAnswer::Granted(()) => Ok(()),
      McpKeyAnswer::Refused { status, body } => Err(crate::backend_error_with_detail(
        "MCP_REMOTE_KEY_UNAVAILABLE",
        format!("{status}: {body}"),
      )),
    }
  }

  /// Retire the stored remote MCP credential on sign-out.
  ///
  /// Best effort, and the local copy goes regardless. The revoke needs the
  /// session that is about to be deleted, so this runs before `clear_auth`;
  /// if it fails the key stays live server-side and the account page can
  /// revoke it, but a signed-out desktop must not keep a credential that
  /// belongs to the account that just left it.
  async fn retire_mcp_key_on_logout(&self) {
    let settings = SettingsManager::instance();
    // Stringified at once: the settings error is a `Box<dyn Error>`, which is
    // not `Send`, and the command future this runs in has to be.
    let stored = settings.get_mcp_remote_key().map_err(|e| e.to_string());
    match stored {
      Ok(Some(StoredMcpRemoteKey { id: Some(id), .. })) => {
        if let Err(e) = self.revoke_mcp_key(&id).await {
          log::warn!(
            "Remote MCP key not revoked on logout, revoke it on the account page err=\"{e}\""
          );
        }
      }
      Ok(Some(StoredMcpRemoteKey { id: None, .. })) => {
        log::warn!("Remote MCP key has no stored id, revoke it on the account page");
      }
      Ok(None) => return,
      Err(e) => {
        log::warn!("Remote MCP key not readable on logout err=\"{e}\"");
      }
    }
    Self::forget_mcp_key_locally("on logout");
  }

  /// Drop the local copy of the remote MCP credential: the storage half of
  /// `retire_mcp_key_on_logout`, shared with the expiry path, which has no
  /// session left to revoke with. Logs rather than fails, because the
  /// credential is leaving with the session either way.
  ///
  /// The key id stays: a key that was not revoked is still live on the
  /// server, and the next mint retires it by that id (a revoked one answers
  /// 404, which counts as retired).
  fn forget_mcp_key_locally(when: &str) {
    if let Err(e) = SettingsManager::instance().drop_mcp_remote_key_plaintext() {
      log::warn!("Remote MCP key not removed locally when=\"{when}\" err=\"{e}\"");
    }
  }

  pub async fn fetch_profile(&self) -> Result<CloudUser, String> {
    let user = self
      .api_call_with_retry(|access_token| {
        let url = format!("{CLOUD_API_URL}/api/auth/me");
        let client = self.client.get();
        async move {
          let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|e| format!("Failed to fetch profile: {e}"))?;

          if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Profile fetch failed ({status}): {body}"));
          }

          response
            .json::<CloudUser>()
            .await
            .map_err(|e| format!("Failed to parse profile: {e}"))
        }
      })
      .await?;

    // Update cached state
    let mut state = self.state.lock().await;
    if let Some(auth_state) = state.as_mut() {
      auth_state.user = user.clone();
      let _ = Self::store_auth_state(auth_state);
    }

    Ok(user)
  }

  pub async fn get_or_refresh_sync_token(&self) -> Result<Option<String>, String> {
    if !self.is_logged_in().await {
      return Ok(None);
    }

    // Check cached sync token
    if let Ok(Some(token)) = Self::load_cloud_sync_token() {
      if !Self::is_jwt_expiring_soon(&token) {
        return Ok(Some(token));
      }
    }

    // Fetch new sync token
    let fetched = self
      .api_call_with_retry(|access_token| {
        let url = format!("{CLOUD_API_URL}/api/auth/sync-token");
        let client = self.client.get();
        async move {
          let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|e| format!("Failed to get sync token: {e}"))?;

          if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Sync token request failed ({status}): {body}"));
          }

          let result: SyncTokenResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse sync token response: {e}"))?;

          Ok(result.sync_token)
        }
      })
      .await;
    let sync_token = match fetched {
      Ok(token) => {
        SYNC_TOKEN_FETCH.succeeded();
        token
      }
      Err(e) => {
        SYNC_TOKEN_FETCH.failed(&e);
        return Err(e);
      }
    };

    if let Err(e) = Self::store_cloud_sync_token(&sync_token) {
      log::error!("Sync token store failed err=\"{e}\"");
      return Err(e);
    }
    Ok(Some(sync_token))
  }

  /// A token for the customer's own donut-sync at `server`, which the Donut
  /// API binds to that one address (`sync::account_server`).
  pub async fn server_sync_token(&self, server: &str) -> Result<String, String> {
    if !self.is_logged_in().await {
      return Err(crate::backend_error("ACCOUNT_SYNC_SIGN_IN_REQUIRED"));
    }
    let cached = SERVER_SYNC_TOKENS
      .lock()
      .ok()
      .and_then(|tokens| tokens.get(server).cloned());
    if let Some(token) = cached.filter(|token| !Self::is_jwt_expiring_soon(token)) {
      return Ok(token);
    }

    let fetched = self
      .api_call_with_retry(|access_token| {
        let url = format!("{CLOUD_API_URL}/api/auth/sync-token");
        let client = self.client.get();
        let body = serde_json::json!({ "server": server });
        async move {
          let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Failed to get sync token: {e}"))?;
          let status = response.status();
          if status == reqwest::StatusCode::FORBIDDEN {
            return Err(crate::backend_error("ACCOUNT_SYNC_PLAN_REQUIRED"));
          }
          if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Sync token request failed ({status}): {body}"));
          }
          let result: SyncTokenResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse sync token response: {e}"))?;
          Ok(result.sync_token)
        }
      })
      .await;
    let fetched = fetched.and_then(|token| {
      if Self::is_server_sync_token_for(&token, server) {
        Ok(token)
      } else {
        Err(crate::backend_error("ACCOUNT_SYNC_UNAVAILABLE"))
      }
    });
    match fetched {
      Ok(token) => {
        SERVER_SYNC_TOKEN_FETCH.succeeded();
        if let Ok(mut tokens) = SERVER_SYNC_TOKENS.lock() {
          tokens.insert(server.to_string(), token.clone());
        }
        Ok(token)
      }
      Err(e) => {
        SERVER_SYNC_TOKEN_FETCH.failed(&e);
        Err(e)
      }
    }
  }

  /// Forget a token the own server refused, so the next request fetches one.
  pub(crate) fn discard_server_sync_token(refused: &str) {
    if let Ok(mut tokens) = SERVER_SYNC_TOKENS.lock() {
      tokens.retain(|_, token| token != refused);
    }
  }

  pub async fn logout(&self) -> Result<(), String> {
    if let Ok(mut tokens) = SERVER_SYNC_TOKENS.lock() {
      tokens.clear();
    }
    // The own server is this account's choice; the next account starts on
    // Donut cloud.
    crate::sync::account_server::clear();

    crate::team_lock::PROFILE_LOCK
      .release_all_held_within(crate::team_lock::SHUTDOWN_RELEASE_TIMEOUT)
      .await;

    // Disconnect profile lock manager
    crate::team_lock::PROFILE_LOCK.disconnect().await;

    // Hang up the remote-control bridge before the credential it authenticated
    // with is deleted. Left running it would reconnect, fail, and back off into
    // an "unauthorized" the account page shows to somebody who has simply
    // signed out, and it would hold the account's bridge slot meanwhile.
    crate::mcp_remote::stop(None);
    crate::agent_console::reset();

    // Before the session is closed server-side and the tokens are deleted:
    // both of those take away the only thing that can revoke it.
    self.retire_mcp_key_on_logout().await;

    // Try to call the logout API (best-effort)
    if let Ok(Some(access_token)) = Self::load_access_token() {
      let refresh_token = Self::load_refresh_token().ok().flatten();
      let url = format!("{CLOUD_API_URL}/api/auth/logout");
      let mut body = serde_json::json!({});
      if let Some(rt) = &refresh_token {
        body = serde_json::json!({ "refreshToken": rt });
      }
      let _ = self
        .client
        .get()
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .json(&body)
        .send()
        .await;
    }

    // Remove cloud proxy on logout
    PROXY_MANAGER.remove_cloud_proxy();

    self.clear_auth().await;
    Ok(())
  }

  pub async fn is_logged_in(&self) -> bool {
    let state = self.state.lock().await;
    state.is_some()
  }

  /// Resolve this session's entitlements (server-sent or locally derived).
  pub async fn entitlements(&self) -> Option<Entitlements> {
    let state = self.state.lock().await;
    state.as_ref().map(|auth| auth.user.entitlements())
  }

  /// Account is in a paid/active state. Used for the "any active plan" gates
  /// (sync token); per-feature access uses the capability helpers.
  pub async fn has_active_paid_subscription(&self) -> bool {
    #[cfg(feature = "e2e")]
    if crate::e2e_automation_enabled()
      && std::env::var_os("DONUT_E2E_PAID_PLAN").is_some_and(|token| !token.is_empty())
    {
      return true;
    }

    self.entitlements().await.map(|e| e.active).unwrap_or(false)
  }

  /// Non-async version that uses try_lock, defaults to false if lock can't be acquired.
  pub fn has_active_paid_subscription_sync(&self) -> bool {
    match self.state.try_lock() {
      Ok(state) => state
        .as_ref()
        .map(|auth| auth.user.entitlements().active)
        .unwrap_or(false),
      Err(_) => false,
    }
  }

  /// Launch/drive profiles programmatically (local API + MCP automation).
  /// Whether this account may run the nightly Cookie Bot.
  ///
  /// NOT `can_use_browser_automation`. Solo is exactly the plan where the two
  /// disagree — it pays for a nightly bot and has `browser_automation: false` —
  /// so gating the bot on automation refused a Solo customer the one feature
  /// their plan is sold on, and answered 402 while their scheduled runs kept
  /// working server-side.
  pub async fn can_use_cookie_bot(&self) -> bool {
    self
      .entitlements()
      .await
      .map(|e| e.cookie_bot)
      .unwrap_or(false)
  }

  pub async fn can_use_browser_automation(&self) -> bool {
    #[cfg(feature = "e2e")]
    if crate::e2e_automation_enabled()
      && std::env::var_os("DONUT_E2E_PAID_PLAN").is_some_and(|token| !token.is_empty())
    {
      return true;
    }

    self
      .entitlements()
      .await
      .map(|e| e.browser_automation)
      .unwrap_or(false)
  }

  /// Edit fingerprints / use a non-native OS fingerprint.
  pub async fn can_use_cross_os_fingerprints(&self) -> bool {
    #[cfg(feature = "e2e")]
    if crate::e2e_automation_enabled()
      && std::env::var_os("DONUT_E2E_PAID_PLAN").is_some_and(|token| !token.is_empty())
    {
      return true;
    }

    self
      .entitlements()
      .await
      .map(|e| e.cross_os_fingerprints)
      .unwrap_or(false)
  }

  /// Cloud profile sync / backup (async).
  pub async fn can_use_cloud_backup(&self) -> bool {
    self
      .entitlements()
      .await
      .map(|e| e.cloud_backup)
      .unwrap_or(false)
  }

  /// Cloud profile sync / backup (non-async, try_lock; false if unavailable).
  pub fn can_use_cloud_backup_sync(&self) -> bool {
    match self.state.try_lock() {
      Ok(state) => state
        .as_ref()
        .map(|auth| auth.user.entitlements().cloud_backup)
        .unwrap_or(false),
      Err(_) => false,
    }
  }

  /// Identity and positive per-hour cap for the shared REST/MCP automation
  /// limiter. No active automation entitlement means no limiter entry; the
  /// capability gates still reject paid operations independently.
  pub async fn automation_rate_limit(&self) -> Option<(String, u64)> {
    #[cfg(feature = "e2e")]
    if crate::e2e_automation_enabled() {
      if let Ok(limit) = std::env::var("DONUT_E2E_REQUESTS_PER_HOUR") {
        if let Ok(limit) = limit.parse::<u64>() {
          if limit > 0 {
            return Some(("e2e-automation".to_string(), limit));
          }
        }
      }
    }

    let state = self.get_user().await?;
    let limit = state.user.entitlements().requests_per_hour;
    (limit > 0).then_some((state.user.id, limit as u64))
  }

  pub async fn is_fingerprint_os_allowed(&self, fingerprint_os: Option<&str>) -> bool {
    let host_os = crate::profile::types::get_host_os();
    match fingerprint_os {
      None => true,
      Some(os) if os == host_os => true,
      Some(_) => self.can_use_cross_os_fingerprints().await,
    }
  }

  pub async fn is_on_team_plan(&self) -> bool {
    if let Some(state) = self.get_user().await {
      return state.user.team_id.is_some();
    }
    false
  }

  pub async fn get_user(&self) -> Option<CloudAuthState> {
    let state = self.state.lock().await;
    state.clone()
  }

  async fn plan_marker(&self) -> Option<(String, bool, bool)> {
    let state = self.state.lock().await;
    state.as_ref().map(|auth| {
      let entitlements = auth.user.entitlements();
      (
        auth.user.effective_plan().to_string(),
        entitlements.active,
        entitlements.cloud_backup,
      )
    })
  }

  async fn clear_auth(&self) {
    let mut state = self.state.lock().await;
    *state = None;
    observe_account(None);
    Self::delete_all_cloud_files();
    crate::cookie_bot::forget_enrolments();
  }

  /// API call with 401 retry: if first attempt gets 401, refresh access token and retry once.
  /// Uses refresh_lock to prevent concurrent token rotations from racing.
  pub async fn api_call_with_retry<F, Fut, T>(&self, make_request: F) -> Result<T, String>
  where
    F: Fn(String) -> Fut + Send,
    Fut: std::future::Future<Output = Result<T, String>> + Send,
  {
    let access_token = self.usable_access_token().await?;

    match make_request(access_token.clone()).await {
      Ok(result) => Ok(result),
      Err(e) if e.contains("(401") || e.contains("Unauthorized") => {
        log::debug!("Cloud API returned 401, refreshing the access token");

        // Check if another caller already refreshed while we waited
        let current_token = Self::load_access_token()?.unwrap_or_default();
        if current_token != access_token && !current_token.is_empty() {
          return make_request(current_token).await;
        }

        let new_token = self.refresh_after_rejection(&access_token).await?;
        make_request(new_token).await
      }
      Err(e) => Err(e),
    }
  }

  /// Fetch proxy configuration from the cloud backend
  async fn fetch_proxy_config(&self) -> Result<Option<CloudProxyConfigResponse>, String> {
    // Check cached user state for proxy bandwidth (subscription or extra)
    {
      let state = self.state.lock().await;
      match &*state {
        Some(auth)
          if auth.user.proxy_bandwidth_limit_mb > 0 || auth.user.proxy_bandwidth_extra_mb > 0 => {}
        _ => return Ok(None),
      }
    }

    match self
      .api_call_with_retry(|access_token| {
        let url = format!("{CLOUD_API_URL}/api/proxy/config");
        let client = self.client.get();
        async move {
          let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|e| format!("Failed to fetch proxy config: {e}"))?;

          let status = response.status();
          if status == reqwest::StatusCode::FORBIDDEN {
            return Err("__403__".to_string());
          }

          if !response.status().is_success() {
            return Err(format!("Proxy config fetch failed ({status})"));
          }

          response
            .json::<CloudProxyConfigResponse>()
            .await
            .map_err(|e| format!("Failed to parse proxy config: {e}"))
        }
      })
      .await
    {
      Ok(config) => {
        PROXY_CONFIG_FETCH.succeeded();
        Ok(Some(config))
      }
      Err(e) if e.contains("__403__") => {
        PROXY_CONFIG_FETCH.succeeded();
        Ok(None)
      }
      Err(e) => {
        PROXY_CONFIG_FETCH.failed(e);
        Ok(None)
      }
    }
  }

  /// Sync the cloud-managed proxy: fetch config and upsert or remove
  pub async fn sync_cloud_proxy(&self) {
    match self.fetch_proxy_config().await {
      Ok(Some(config)) => {
        let state = format!(
          "host={} port={} protocol={}",
          config.host, config.port, config.protocol
        );
        if cloud_proxy_state_changed(state.clone()) {
          log::info!("Cloud proxy configured {state}");
        }
        let settings = ProxySettings {
          proxy_type: config.protocol,
          host: config.host,
          port: config.port,
          username: config.username,
          password: config.password,
          vless_uri: None,
        };
        match PROXY_MANAGER.upsert_cloud_proxy(settings) {
          Ok(_) => {
            // Propagate credential changes to derived location proxies
            PROXY_MANAGER.update_cloud_derived_proxies();
          }
          Err(e) => log::warn!("Cloud proxy not saved err=\"{e}\""),
        }
      }
      Ok(None) => {
        if cloud_proxy_state_changed("none".to_string()) {
          log::info!("Cloud proxy removed: no config available");
        }
        PROXY_MANAGER.remove_cloud_proxy();
      }
      Err(e) => {
        log::error!("Cloud proxy sync failed err=\"{e}\"");
      }
    }
  }

  /// Report the number of sync-enabled profiles to the cloud backend
  pub async fn report_sync_profile_count(&self, count: i64) -> Result<(), String> {
    self
      .api_call_with_retry(|access_token| {
        let url = format!("{CLOUD_API_URL}/api/auth/sync-profile-usage");
        let client = CLOUD_AUTH.client.get();
        async move {
          let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .json(&serde_json::json!({ "count": count }))
            .send()
            .await
            .map_err(|e| format!("Failed to report profile usage: {e}"))?;

          if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Profile usage report failed ({status}): {body}"));
          }

          Ok(())
        }
      })
      .await
  }

  /// Fetch country list from the cloud backend
  pub async fn fetch_countries(&self) -> Result<Vec<LocationItem>, String> {
    self
      .api_call_with_retry(|access_token| {
        let url = format!("{CLOUD_API_URL}/api/proxy/locations/countries");
        let client = self.client.get();
        async move {
          let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|e| format!("Failed to fetch countries: {e}"))?;

          if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Countries fetch failed ({status}): {body}"));
          }

          response
            .json::<Vec<LocationItem>>()
            .await
            .map_err(|e| format!("Failed to parse countries: {e}"))
        }
      })
      .await
  }

  /// Background loop that refreshes the sync token periodically
  pub async fn start_sync_token_refresh_loop(app_handle: tauri::AppHandle) {
    loop {
      tokio::time::sleep(std::time::Duration::from_secs(600)).await; // 10 minutes

      if !CLOUD_AUTH.is_logged_in().await {
        continue;
      }

      // Proactively refresh the access token if it's expired or expiring soon.
      // This runs first so subsequent API calls use a fresh token.
      if let Ok(Some(token)) = Self::load_access_token() {
        if Self::access_token_needs_refresh(&token) {
          match CLOUD_AUTH.ensure_fresh_access_token().await {
            Ok(_) => ACCESS_TOKEN_REFRESH.succeeded(),
            Err(e) => {
              ACCESS_TOKEN_REFRESH.failed(&e);
              // If the refresh token itself was rejected, session is irrecoverable
              if e.contains("(401") || e.contains("Unauthorized") {
                continue;
              }
            }
          }
        }
      }

      // Failures are logged inside.
      let _ = CLOUD_AUTH.get_or_refresh_sync_token().await;

      let plan_before = CLOUD_AUTH.plan_marker().await;

      // Refresh profile data periodically. A failure here leaves the cached
      // plan stale, which silently gates paid features, so it belongs at warn
      // rather than debug where the shipped log level hides it.
      match CLOUD_AUTH.fetch_profile().await {
        Ok(_) => PROFILE_REFRESH.succeeded(),
        Err(e) => PROFILE_REFRESH.failed(e),
      }

      if CLOUD_AUTH.plan_marker().await != plan_before {
        sync::resume_subscription(app_handle.clone()).await;
      }

      // Reconnect profile lock manager if needed
      if let Some(auth_state) = CLOUD_AUTH.get_user().await {
        if auth_state.user.effective_plan() != "free"
          && !crate::team_lock::PROFILE_LOCK.is_connected().await
        {
          crate::team_lock::PROFILE_LOCK.connect().await;
        }
      }

      // And the remote-control bridge, for the same reason one tick below it
      // reconnects the profile lock: a setting that says "on" and a bridge that
      // is not running is a disagreement only something periodic can notice.
      // `mcp_remote::start` is idempotent, so a healthy bridge costs a load of
      // the settings file every ten minutes and nothing else.
      ensure_remote_bridge(&app_handle).await;

      // Sync cloud proxy credentials
      CLOUD_AUTH.sync_cloud_proxy().await;

      let _ = &app_handle; // keep app_handle alive
    }
  }
}

fn solve_pow(prefix: &str, difficulty: u32) -> Option<String> {
  if difficulty == 0 || difficulty > 32 {
    return None;
  }
  let prefix_bytes = prefix.as_bytes();
  let mut buf = Vec::with_capacity(prefix_bytes.len() + 24);
  for nonce in 0u64..u64::MAX {
    buf.clear();
    buf.extend_from_slice(prefix_bytes);
    let nonce_str = nonce.to_string();
    buf.extend_from_slice(nonce_str.as_bytes());
    let digest = Sha256::digest(&buf);
    if has_leading_zero_bits(&digest, difficulty) {
      return Some(nonce_str);
    }
  }
  None
}

fn has_leading_zero_bits(digest: &[u8], bits: u32) -> bool {
  let full_bytes = (bits / 8) as usize;
  if digest.len() < full_bytes + 1 {
    return false;
  }
  for &b in &digest[..full_bytes] {
    if b != 0 {
      return false;
    }
  }
  let remainder = bits % 8;
  if remainder == 0 {
    return true;
  }
  let mask = 0xffu8 << (8 - remainder);
  (digest[full_bytes] & mask) == 0
}

// --- Tauri commands ---

#[tauri::command]
pub async fn cloud_exchange_device_code(
  app_handle: tauri::AppHandle,
  code: String,
) -> Result<CloudAuthState, String> {
  let mut state = CLOUD_AUTH.exchange_device_code(&code).await?;

  // The login response carries the row's own plan and entitlements only: no
  // team membership and no `effectivePlan`. For an invited member that reads
  // as a free account, and it stayed that way until the ten-minute loop got
  // round to `/api/auth/me`. Resolve the served plan here so the sync token
  // and the profile lock below see the seat. Best effort: a failure leaves
  // the login response in place.
  match CLOUD_AUTH.fetch_profile().await {
    Ok(user) => state.user = user,
    Err(e) => log::warn!("Post-login profile refresh failed err=\"{e}\""),
  }

  let has_subscription = CLOUD_AUTH.has_active_paid_subscription().await;
  log::info!(
    "Post-login plan resolved plan={} effective_plan={} has_active_subscription={}",
    state.user.plan,
    state.user.effective_plan(),
    has_subscription
  );

  // Pre-fetch sync token so sync can start immediately
  if has_subscription {
    // Failures of both are logged inside.
    if let Ok(None) = CLOUD_AUTH.get_or_refresh_sync_token().await {
      log::warn!("Sync token missing despite an active subscription");
    }
  }

  // Sync cloud proxy after login
  CLOUD_AUTH.sync_cloud_proxy().await;

  // Connect profile lock manager for paid users
  if state.user.effective_plan() != "free" {
    crate::team_lock::PROFILE_LOCK.connect().await;
  }

  // Reopen the remote-control bridge the user had switched on.
  //
  // Signing out stops the bridge but deliberately does NOT clear the setting:
  // it is a preference, not a session. So without this, "sign out, sign back
  // in" left remote control switched on in Settings and dead in fact until the
  // app was restarted, which is the worst of both: the UI says yes and the
  // account page says no desktop is connected.
  ensure_remote_bridge(&app_handle).await;

  // Signing out left the connected clients with a dead key; give them a new
  // one without holding up the sign-in.
  tauri::async_runtime::spawn(crate::restore_mcp_credential_after_sign_in());

  let _ = crate::events::emit_empty("cloud-auth-changed");

  let _ = &app_handle;
  state.user.entitlements = Some(state.user.entitlements());
  Ok(state)
}

/// The account backend this build talks to, after settings and
/// `DONUT_CLOUD_API_URL` are applied.
#[tauri::command]
pub fn get_cloud_api_url() -> String {
  cloud_api_base()
}

/// Point the account backend at another server. `None` clears the stored
/// address, falling back to `DONUT_CLOUD_API_URL` or this fork's control plane.
#[tauri::command]
pub async fn set_cloud_api_url(url: Option<String>) -> Result<String, String> {
  let normalized = match url.as_deref().map(str::trim).filter(|url| !url.is_empty()) {
    Some(value) => Some(
      normalize_cloud_api_base(value)
        .ok_or_else(|| crate::backend_error("CLOUD_API_URL_INVALID"))?,
    ),
    None => None,
  };

  let manager = SettingsManager::instance();
  let mut settings = manager
    .load_settings()
    .map_err(|e| crate::backend_error_with_detail("INTERNAL_ERROR", e))?;
  let before = cloud_api_base();
  let after = effective_cloud_api_base(normalized.as_deref());
  settings.cloud_api_url = normalized.clone();
  if before != after {
    // An internet-facing bridge belongs to the account it was enabled for,
    // and the next server may not implement `/api/mcp-bridge` at all.
    settings.mcp_remote_enabled = false;
  }
  manager
    .save_settings(&settings)
    .map_err(|e| crate::backend_error_with_detail("INTERNAL_ERROR", e))?;

  set_cloud_api_base(normalized.as_deref());
  if before != after && CLOUD_AUTH.is_logged_in().await {
    // A session belongs to the server that issued it; keeping it would send
    // this server's bearer token to the next one. This also hangs up the
    // remote-control bridge the old account had opened.
    CLOUD_AUTH.invalidate_session().await;
  }
  Ok(after)
}

/// Open the remote-control bridge if the user asked for it and is signed in.
///
/// Idempotent, and safe to call from anywhere: `mcp_remote::start` returns
/// immediately when the bridge is already up.
///
/// The signed-in half matters as much as the setting. A bridge opened without a
/// credential cannot authenticate, so it would spend its life in the terminal
/// backoff band reporting "not signed in" on the account page: an error
/// describing nothing the user did.
pub(crate) async fn ensure_remote_bridge(app_handle: &tauri::AppHandle) {
  if crate::mcp_remote::is_running() {
    return;
  }
  if !CLOUD_AUTH.is_logged_in().await {
    return;
  }
  // A self-hosted control plane that does not declare remote control has no
  // `/api/mcp-bridge` for the socket to reach; opening it would only log a
  // reconnect failure every few seconds.
  if cloud_api_base() != DEFAULT_CLOUD_API_URL
    && !CLOUD_AUTH
      .entitlements()
      .await
      .is_some_and(|entitlements| entitlements.remote_control)
  {
    return;
  }
  let enabled = crate::settings_manager::SettingsManager::instance()
    .load_settings()
    .map(|settings| settings.mcp_remote_enabled)
    .unwrap_or(false);
  if enabled {
    log::info!("Remote control bridge opening: enabled and signed in");
    crate::mcp_remote::start(app_handle.clone());
  }
}

#[tauri::command]
pub async fn cloud_get_user() -> Result<Option<CloudAuthState>, String> {
  Ok(CLOUD_AUTH.get_user().await.map(|mut state| {
    // Always hand the frontend a resolved entitlements object so it never has to
    // derive capabilities itself (covers older cached state with no entitlements).
    state.user.entitlements = Some(state.user.entitlements());
    // The base in effect decides this, not the cached payload: state written
    // before the field existed would otherwise read as a Donut cloud account
    // and hide every local service.
    state.user.self_hosted = Some(is_self_hosted_account());
    state
  }))
}

#[tauri::command]
pub async fn cloud_refresh_profile() -> Result<CloudUser, String> {
  let mut user = CLOUD_AUTH.fetch_profile().await?;
  user.entitlements = Some(user.entitlements());
  user.self_hosted = Some(is_self_hosted_account());

  // `fetch_profile` rewrote the cached state, but every `useCloudAuth` keeps
  // its own copy. Without this event an account that refreshed here left the
  // Profiles page holding the old entitlements until a restart, so a feature
  // the control plane had just granted still rendered locked.
  let _ = crate::events::emit_empty("cloud-auth-changed");

  Ok(user)
}

#[tauri::command]
pub async fn cloud_logout(app_handle: tauri::AppHandle) -> Result<(), String> {
  let keep_self_hosted = crate::sync::account_server::self_hosted_picked().await;
  CLOUD_AUTH.logout().await?;

  // Stop the remote session-events stream. Its credential is now invalid, so
  // the SSE connection would fail its next credential re-check anyway — but
  // until it did, its reconnect loop keeps retrying every 1..60s against a
  // signed-out account. The frontend store stops its own subscription on
  // logout; nothing stopped the Rust stream, so a sign-out left it churning.
  crate::remote_session::stop_session_events();

  // Clear the stored sync URL and token on cloud logout, unless the person
  // synced with that server while signed in. Older versions populated these
  // with the hosted sync server's URL + a server-issued token — leaving them
  // in place would pre-fill the Self-Hosted tab with our production URL and a
  // token the user never typed. The cloud-URL-only check we used to do here
  // missed trailing-slash / scheme variants and any future cloud endpoint
  // moves.
  if !keep_self_hosted {
    let manager = crate::settings_manager::SettingsManager::instance();
    let _ = manager.save_sync_server_url(None);
    let _ = manager.remove_sync_token(&app_handle).await;
  }

  // Remove cloud-managed and cloud-derived proxies
  crate::proxy_manager::PROXY_MANAGER.remove_cloud_proxies();

  let _ = crate::events::emit_empty("cloud-auth-changed");
  Ok(())
}

#[tauri::command]
pub async fn cloud_has_active_subscription() -> Result<bool, String> {
  Ok(CLOUD_AUTH.has_active_paid_subscription().await)
}

#[tauri::command]
pub async fn cloud_get_countries() -> Result<Vec<LocationItem>, String> {
  CLOUD_AUTH.fetch_countries().await
}

#[tauri::command]
pub async fn create_cloud_location_proxy(
  name: String,
  country: String,
  region: Option<String>,
  city: Option<String>,
  isp: Option<String>,
) -> Result<crate::proxy_manager::StoredProxy, String> {
  // If no cloud proxy exists yet, attempt to sync it first
  if !PROXY_MANAGER.has_cloud_proxy() {
    CLOUD_AUTH.sync_cloud_proxy().await;
  }
  PROXY_MANAGER.create_cloud_location_proxy(name, country, region, city, isp)
}

#[derive(Debug, Serialize)]
pub struct CloudProxyUsage {
  pub used_mb: i64,
  pub limit_mb: i64,
  pub remaining_mb: i64,
  pub recurring_limit_mb: i64,
  pub extra_limit_mb: i64,
}

#[derive(Debug, Deserialize)]
struct ProxyUsageResponse {
  #[serde(rename = "usedMb")]
  used_mb: i64,
  #[serde(rename = "limitMb")]
  limit_mb: i64,
  #[serde(rename = "remainingMb")]
  remaining_mb: i64,
  // Optional rather than defaulted to 0 so an omitted half of the split stays
  // distinguishable from a backend that genuinely reports zero for it.
  #[serde(rename = "recurringLimitMb")]
  recurring_limit_mb: Option<i64>,
  #[serde(rename = "extraLimitMb")]
  extra_limit_mb: Option<i64>,
}

/// Combine the live usage response with the cached account snapshot, each half
/// of the recurring/extra split falling back on its own.
///
/// Gating both halves on `recurringLimitMb` made an omitted `extraLimitMb`
/// report 0 against a total that included it, and threw away a live
/// `extraLimitMb` whenever the recurring half happened to be 0.
fn merge_proxy_usage(
  usage: &ProxyUsageResponse,
  cached_recurring: i64,
  cached_extra: i64,
) -> CloudProxyUsage {
  CloudProxyUsage {
    used_mb: usage.used_mb,
    limit_mb: usage.limit_mb,
    remaining_mb: usage.remaining_mb,
    recurring_limit_mb: usage.recurring_limit_mb.unwrap_or(cached_recurring),
    extra_limit_mb: usage.extra_limit_mb.unwrap_or(cached_extra),
  }
}

#[tauri::command]
pub async fn cloud_get_proxy_usage() -> Result<Option<CloudProxyUsage>, String> {
  let (has_proxy, cached_recurring, cached_extra) = {
    let state = CLOUD_AUTH.state.lock().await;
    match &*state {
      Some(auth)
        if auth.user.proxy_bandwidth_limit_mb > 0 || auth.user.proxy_bandwidth_extra_mb > 0 =>
      {
        (
          true,
          auth.user.proxy_bandwidth_limit_mb,
          auth.user.proxy_bandwidth_extra_mb,
        )
      }
      _ => return Ok(None),
    }
  };

  if !has_proxy {
    return Ok(None);
  }

  // Fetch live usage from the API
  match CLOUD_AUTH
    .api_call_with_retry(|access_token| {
      let url = format!("{CLOUD_API_URL}/api/proxy/usage");
      let client = CLOUD_AUTH.client.get();
      async move {
        let response = client
          .get(&url)
          .header("Authorization", format!("Bearer {access_token}"))
          .send()
          .await
          .map_err(|e| format!("Failed to fetch proxy usage: {e}"))?;

        if !response.status().is_success() {
          return Err(format!(
            "Proxy usage API returned status {}",
            response.status()
          ));
        }

        response
          .json::<ProxyUsageResponse>()
          .await
          .map_err(|e| format!("Failed to parse proxy usage: {e}"))
      }
    })
    .await
  {
    Ok(usage) => {
      PROXY_USAGE_FETCH.succeeded();
      Ok(Some(merge_proxy_usage(
        &usage,
        cached_recurring,
        cached_extra,
      )))
    }
    Err(e) => {
      PROXY_USAGE_FETCH.failed(format!("{e}; using cached usage"));
      // Fallback to cached values
      let state = CLOUD_AUTH.state.lock().await;
      match &*state {
        Some(auth) => {
          let used = auth.user.proxy_bandwidth_used_mb;
          let total = cached_recurring + cached_extra;
          Ok(Some(CloudProxyUsage {
            used_mb: used,
            limit_mb: total,
            remaining_mb: (total - used).max(0),
            recurring_limit_mb: cached_recurring,
            extra_limit_mb: cached_extra,
          }))
        }
        _ => Ok(None),
      }
    }
  }
}

#[tauri::command]
pub async fn restart_sync_service(app_handle: tauri::AppHandle) -> Result<(), String> {
  // Rebuilding the pipeline reaches the network, so do it off the command and
  // let the caller's dialog close. `start_pipeline` retires the previous
  // scheduler and the previous subscription itself.
  tauri::async_runtime::spawn(async move {
    sync::start_pipeline(app_handle).await;
  });

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cloud_api_base_normalizes_what_people_paste() {
    for (input, expected) in [
      ("https://control.example.com", "https://control.example.com"),
      (
        "https://control.example.com/",
        "https://control.example.com",
      ),
      (
        "https://control.example.com/api",
        "https://control.example.com",
      ),
      ("http://localhost:8080/api/", "http://localhost:8080"),
      (
        "  https://control.example.com  ",
        "https://control.example.com",
      ),
    ] {
      assert_eq!(
        normalize_cloud_api_base(input).as_deref(),
        Some(expected),
        "input={input}"
      );
    }
    for invalid in [
      "",
      "control.example.com",
      "ftp://control.example.com",
      "https://",
    ] {
      assert_eq!(normalize_cloud_api_base(invalid), None, "input={invalid}");
    }
  }

  #[test]
  fn cloud_api_url_formats_the_configured_base() {
    let base = cloud_api_base();
    assert_eq!(format!("{CLOUD_API_URL}"), base);
    assert_eq!(
      format!("{CLOUD_API_URL}/api/auth/me"),
      format!("{base}/api/auth/me")
    );
  }

  #[test]
  fn a_stock_build_and_a_stored_cloud_address_both_use_the_fork_control_plane() {
    if std::env::var_os("DONUT_CLOUD_API_URL").is_some() {
      return;
    }
    assert_eq!(
      effective_cloud_api_base(None),
      BUILTIN_CONTROL_PLANE_URL,
      "a stock build has no Donut cloud fallback"
    );
    assert_eq!(
      effective_cloud_api_base(Some(DEFAULT_CLOUD_API_URL)),
      BUILTIN_CONTROL_PLANE_URL,
      "an address saved by an older build is ignored"
    );
    assert_eq!(
      effective_cloud_api_base(Some("https://control.example.com")),
      "https://control.example.com",
      "another self-hosted server is still honored"
    );
  }

  #[test]
  fn provisioning_needs_a_key_and_a_self_hosted_base() {
    assert_eq!(
      provisioning_config(DEFAULT_CLOUD_API_URL, None, None, None, None),
      None,
      "a stock build has no provisioning key"
    );
    assert_eq!(
      provisioning_config(
        DEFAULT_CLOUD_API_URL,
        None,
        Some("  "),
        Some("https://control.example.com"),
        Some("")
      ),
      None,
      "blank values are not credentials"
    );
    assert_eq!(
      provisioning_config(
        DEFAULT_CLOUD_API_URL,
        Some(DEFAULT_CLOUD_API_URL),
        Some("key"),
        None,
        None
      ),
      None,
      "pointing at Donut cloud is never a self-hosted provisioning"
    );
  }

  #[test]
  fn provisioning_prefers_the_saved_server_then_runtime_then_build() {
    assert_eq!(
      provisioning_config(
        "https://saved.example.com",
        Some("https://runtime.example.com"),
        Some("key"),
        Some("https://baked.example.com"),
        Some("baked-key")
      ),
      Some(("https://saved.example.com".to_string(), "key".to_string())),
      "Account → Server is the person's choice and wins"
    );
    assert_eq!(
      provisioning_config(
        DEFAULT_CLOUD_API_URL,
        Some("https://runtime.example.com/api/"),
        Some(" runtime-key "),
        Some("https://baked.example.com"),
        Some("baked-key")
      ),
      Some((
        "https://runtime.example.com".to_string(),
        "runtime-key".to_string()
      )),
      "a developer can point one binary at a local control plane"
    );
    assert_eq!(
      provisioning_config(
        DEFAULT_CLOUD_API_URL,
        None,
        None,
        Some("https://baked.example.com/"),
        Some("baked-key")
      ),
      Some((
        "https://baked.example.com".to_string(),
        "baked-key".to_string()
      )),
      "a distributed build uses what it was compiled with"
    );
  }

  fn jwt_with(claims: serde_json::Value) -> String {
    use base64::{engine::general_purpose, Engine as _};
    let encode =
      |value: &serde_json::Value| general_purpose::URL_SAFE_NO_PAD.encode(value.to_string());
    format!(
      "{}.{}.signature",
      encode(&serde_json::json!({ "alg": "RS256" })),
      encode(&claims)
    )
  }

  #[test]
  fn only_a_token_bound_to_the_server_goes_to_it() {
    let server = "https://sync.example.com";
    let bound = jwt_with(serde_json::json!({ "scope": "server-sync", "aud": server }));
    assert!(CloudAuthManager::is_server_sync_token_for(&bound, server));
    // An API that ignores `server` answers with a token for Donut cloud, which
    // must never reach someone else's server.
    let cloud = jwt_with(serde_json::json!({ "scope": "sync", "prefix": "users/u1/" }));
    assert!(!CloudAuthManager::is_server_sync_token_for(&cloud, server));
    assert!(!CloudAuthManager::is_server_sync_token_for(
      &bound,
      "https://other.example.com"
    ));
    assert!(!CloudAuthManager::is_server_sync_token_for(
      "not-a-jwt",
      server
    ));
  }

  #[test]
  fn the_agent_flag_is_off_unless_the_server_sends_it() {
    let older: Entitlements = serde_json::from_str(r#"{"active":true,"browserAutomation":true}"#)
      .expect("an older backend omits agent");
    assert!(!older.agent);
    let granted: Entitlements =
      serde_json::from_str(r#"{"active":false,"agent":true}"#).expect("a free account with agent");
    assert!(granted.agent);
    for plan in ["free", "solo", "pro", "team", "enterprise", "mystery"] {
      assert!(
        !derive_entitlements(plan, Some("monthly"), "active", 20).agent,
        "{plan} must not imply agent"
      );
    }
  }

  fn active_solo() -> Entitlements {
    derive_entitlements("solo", Some("monthly"), "active", 20)
  }

  #[test]
  fn solo_is_active_without_browser_automation() {
    let solo = active_solo();
    assert!(solo.active, "solo is a paid, active plan");
    assert!(solo.cloud_backup, "solo buys cloud profile backups");
    assert!(solo.cookie_bot, "solo buys the nightly cookie bot");
    assert!(
      !solo.browser_automation,
      "solo is sold without browser automation"
    );
    assert!(
      !solo.cross_os_fingerprints,
      "solo is sold without fingerprint editing"
    );
  }

  #[test]
  fn scheduled_automation_is_paid_only_and_not_guessed_for_an_unknown_plan() {
    for plan in ["solo", "pro", "team", "enterprise"] {
      assert!(
        derive_entitlements(plan, Some("monthly"), "active", 20).scheduled_automation,
        "{plan}"
      );
    }
    assert!(!derive_entitlements("free", None, "active", 3).scheduled_automation);
    assert!(!derive_entitlements("pro", Some("monthly"), "canceled", 20).scheduled_automation);
    assert!(!derive_entitlements("mystery", Some("monthly"), "active", 20).scheduled_automation);
    let older_backend: Entitlements =
      serde_json::from_str(r#"{"active":true,"browserAutomation":true}"#).unwrap();
    assert!(
      !older_backend.scheduled_automation,
      "a backend that does not send the key does not open the feature"
    );
  }

  #[test]
  fn remote_control_is_granted_to_every_sold_plan_but_not_an_unknown_one() {
    for plan in ["solo", "pro", "team", "enterprise"] {
      let derived = derive_entitlements(plan, Some("monthly"), "active", 50);
      assert!(derived.remote_control, "{plan} should get remote control");
    }
    assert!(
      !active_solo().browser_automation,
      "solo gets MCP without browser automation"
    );
    assert!(!derive_entitlements("some-comped-plan", Some("monthly"), "active", 50).remote_control);
    assert!(!derive_entitlements("pro", Some("monthly"), "canceled", 50).remote_control);
  }

  #[test]
  fn a_refused_credential_mint_maps_to_the_codes_the_ui_knows() {
    let limit: serde_json::Value = serde_json::from_str(&mcp_key_refusal(
      409,
      r#"{"message":"Too many MCP keys","code":"MCP_KEY_LIMIT","statusCode":409}"#,
    ))
    .unwrap();
    assert_eq!(limit["code"], "MCP_REMOTE_KEY_LIMIT");

    // The cap is the only thing a 409 on that route means, named or not.
    let unnamed: serde_json::Value = serde_json::from_str(&mcp_key_refusal(409, "")).unwrap();
    assert_eq!(unnamed["code"], "MCP_REMOTE_KEY_LIMIT");

    let other: serde_json::Value = serde_json::from_str(&mcp_key_refusal(
      429,
      r#"{"message":"Too many requests","statusCode":429}"#,
    ))
    .unwrap();
    assert_eq!(other["code"], "MCP_REMOTE_KEY_UNAVAILABLE");
    assert_eq!(other["params"]["detail"], "429: Too many requests");

    // A body that is not JSON still reaches the log verbatim.
    let plain: serde_json::Value =
      serde_json::from_str(&mcp_key_refusal(502, "bad gateway")).unwrap();
    assert_eq!(plain["params"]["detail"], "502: bad gateway");
  }

  #[test]
  fn logout_retires_the_remote_credential_before_the_session_is_gone() {
    // The revoke needs the access token; `clear_auth` deletes it and the
    // `/api/auth/logout` call may invalidate it server-side. Both must come
    // after.
    let source = include_str!("cloud_auth.rs");
    let logout = source
      .split("pub async fn logout(&self)")
      .nth(1)
      .expect("logout must exist");
    let body = &logout[..logout.find("\n  }").unwrap_or(logout.len())];
    let retire = body
      .find("retire_mcp_key_on_logout()")
      .expect("logout must retire the remote MCP credential");
    let api_logout = body
      .find("/api/auth/logout")
      .expect("logout must still call the logout endpoint");
    let clear = body
      .find("clear_auth()")
      .expect("logout must still clear the session");
    assert!(
      retire < api_logout,
      "revoke before the server closes the session"
    );
    assert!(retire < clear, "revoke before the tokens are deleted");
  }

  #[test]
  fn session_expiry_forgets_the_remote_credential_without_a_network_revoke() {
    // The automatic twin of logout: the refresh loop gave up, so the session
    // is already dead and there is nothing to revoke with. The local copy
    // still has to go, or the agents keep presenting a key that belongs to an
    // account this machine is no longer signed in to.
    let source = include_str!("cloud_auth.rs");
    let expiry = source
      .split("pub async fn invalidate_session(&self)")
      .nth(1)
      .expect("invalidate_session must exist");
    let body = &expiry[..expiry.find("\n  }").unwrap_or(expiry.len())];
    let forget = body
      .find("forget_mcp_key_locally(")
      .expect("invalidate_session must forget the remote MCP credential");
    let clear = body
      .find("clear_auth()")
      .expect("invalidate_session must still clear the session");
    assert!(
      forget < clear,
      "forget the credential before the auth state is torn down"
    );
    assert!(
      !body.contains("revoke_mcp_key("),
      "a dead session has nothing to revoke with; the call could only fail"
    );
  }

  fn usage_response(recurring: Option<i64>, extra: Option<i64>) -> ProxyUsageResponse {
    ProxyUsageResponse {
      used_mb: 40,
      limit_mb: 600,
      remaining_mb: 560,
      recurring_limit_mb: recurring,
      extra_limit_mb: extra,
    }
  }

  #[test]
  fn each_half_of_the_proxy_limit_falls_back_on_its_own() {
    let both = merge_proxy_usage(&usage_response(Some(500), Some(100)), 400, 0);
    assert_eq!(both.used_mb, 40);
    assert_eq!(both.limit_mb, 600);
    assert_eq!(both.remaining_mb, 560);
    assert_eq!(both.recurring_limit_mb, 500);
    assert_eq!(both.extra_limit_mb, 100);

    // A backend that does not report the split at all keeps the cached one.
    let neither = merge_proxy_usage(&usage_response(None, None), 500, 100);
    assert_eq!(neither.recurring_limit_mb, 500);
    assert_eq!(neither.extra_limit_mb, 100);

    // An omitted extra half must not be read off the recurring half, which is
    // how a cached 100 MB top-up used to vanish from the split.
    let recurring_only = merge_proxy_usage(&usage_response(Some(500), None), 400, 100);
    assert_eq!(recurring_only.recurring_limit_mb, 500);
    assert_eq!(recurring_only.extra_limit_mb, 100);

    // A fresh extra allowance survives a recurring half of zero.
    let extra_only = merge_proxy_usage(&usage_response(Some(0), Some(250)), 500, 100);
    assert_eq!(extra_only.recurring_limit_mb, 0);
    assert_eq!(extra_only.extra_limit_mb, 250);

    // And a spent top-up reported as a live zero is not resurrected from cache.
    let spent_extra = merge_proxy_usage(&usage_response(Some(500), Some(0)), 500, 100);
    assert_eq!(spent_extra.recurring_limit_mb, 500);
    assert_eq!(spent_extra.extra_limit_mb, 0);
  }

  fn token_expiring_at(exp: i64) -> String {
    use base64::{engine::general_purpose, Engine as _};
    let header = general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let payload =
      general_purpose::URL_SAFE_NO_PAD.encode(serde_json::json!({ "exp": exp }).to_string());
    format!("{header}.{payload}.signature")
  }

  #[test]
  fn the_access_token_is_renewed_fifteen_minutes_before_it_expires() {
    let now = 1_800_000_000;
    let ten_minutes_left = token_expiring_at(now + 10 * 60);
    let twenty_minutes_left = token_expiring_at(now + 20 * 60);

    assert!(CloudAuthManager::jwt_expires_within(
      &ten_minutes_left,
      ACCESS_TOKEN_REFRESH_MARGIN_SECS,
      now
    ));
    assert!(!CloudAuthManager::jwt_expires_within(
      &twenty_minutes_left,
      ACCESS_TOKEN_REFRESH_MARGIN_SECS,
      now
    ));
    assert!(!CloudAuthManager::jwt_expires_within(
      &ten_minutes_left,
      SYNC_TOKEN_REFRESH_MARGIN_SECS,
      now
    ));
    assert!(CloudAuthManager::jwt_expires_within(
      "not-a-jwt",
      ACCESS_TOKEN_REFRESH_MARGIN_SECS,
      now
    ));
  }

  #[test]
  fn the_background_loop_renews_before_the_margin_runs_out() {
    let source = include_str!("cloud_auth.rs");
    let body = source
      .split("pub async fn start_sync_token_refresh_loop(")
      .nth(1)
      .expect("the refresh loop must exist");
    let period = body
      .split("tokio::time::sleep(std::time::Duration::from_secs(")
      .nth(1)
      .and_then(|rest| rest.split(')').next())
      .and_then(|secs| secs.parse::<i64>().ok())
      .expect("the loop sleeps a fixed number of seconds");
    assert!(period < ACCESS_TOKEN_REFRESH_MARGIN_SECS);
    assert!(body.contains("access_token_needs_refresh(&token)"));
  }

  #[test]
  fn a_plan_change_brings_a_stopped_sync_subscription_back() {
    let source = include_str!("cloud_auth.rs");
    let body = source
      .split("pub async fn start_sync_token_refresh_loop(")
      .nth(1)
      .expect("the refresh loop must exist");
    let before = body
      .find("let plan_before = CLOUD_AUTH.plan_marker().await;")
      .expect("the plan is read before the profile refresh");
    let fetch = body
      .find("CLOUD_AUTH.fetch_profile().await")
      .expect("the profile refresh");
    let resume = body
      .find("sync::resume_subscription(app_handle.clone()).await;")
      .expect("a changed plan resumes the subscription");
    assert!(before < fetch && fetch < resume);
  }

  #[test]
  fn retry_after_accepts_seconds_and_http_dates() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-27T10:00:00Z")
      .unwrap()
      .with_timezone(&Utc);
    assert_eq!(
      parse_retry_after("120", now),
      Some(Duration::from_secs(120))
    );
    assert_eq!(
      parse_retry_after("Sun, 27 Sep 2026 10:01:30 GMT", now),
      Some(Duration::from_secs(90))
    );
    assert_eq!(
      parse_retry_after("Sun, 27 Sep 2026 09:00:00 GMT", now),
      Some(Duration::ZERO)
    );
    assert_eq!(parse_retry_after("999999", now), Some(MAX_RETRY_AFTER));
    assert_eq!(parse_retry_after("soon", now), None);
  }

  #[test]
  fn a_missing_retry_after_falls_back_to_a_default_wait() {
    let mut headers = reqwest::header::HeaderMap::new();
    assert_eq!(retry_after(&headers), DEFAULT_RETRY_AFTER);
    headers.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
    assert_eq!(retry_after(&headers), Duration::from_secs(7));
  }

  #[test]
  fn only_a_dead_session_reads_as_signed_out() {
    assert!(matches!(
      AuthorizedError::from_token_error("Token refresh failed (401 Unauthorized)".to_string()),
      AuthorizedError::SignedOut
    ));
    assert!(matches!(
      AuthorizedError::from_token_error("Not logged in".to_string()),
      AuthorizedError::SignedOut
    ));
    assert!(matches!(
      AuthorizedError::from_token_error("No refresh token stored".to_string()),
      AuthorizedError::SignedOut
    ));
    assert!(matches!(
      AuthorizedError::from_token_error("Failed to refresh token: connection reset".to_string()),
      AuthorizedError::Transport(_)
    ));
    assert!(matches!(
      AuthorizedError::from_token_error(
        "Token refresh deferred for 30s (429 Too Many Requests)".to_string()
      ),
      AuthorizedError::Transport(_)
    ));
  }

  #[test]
  fn a_refused_refresh_ends_the_session_and_a_throttled_one_waits() {
    let source = include_str!("cloud_auth.rs");
    let body = source
      .split("async fn refresh_while_locked(&self)")
      .nth(1)
      .expect("refresh_while_locked must exist");
    let body = &body[..body.find("\n  }").unwrap_or(body.len())];
    let refused = body
      .find("StatusCode::UNAUTHORIZED")
      .expect("a 401 must be handled");
    let invalidate = body
      .find("invalidate_session()")
      .expect("a refused refresh must end the session");
    assert!(refused < invalidate);
    assert!(body.contains("forget_refresh_token()"));
    assert!(body.contains("defer_refresh(retry_after(response.headers()))"));
    assert!(body.find("refresh_deferral()") < body.find(".send()"));
  }

  #[test]
  fn an_ended_session_stops_every_loop_that_sends_a_token() {
    let source = include_str!("cloud_auth.rs");
    let body = source
      .split("pub async fn invalidate_session(&self)")
      .nth(1)
      .expect("invalidate_session must exist");
    let body = &body[..body.find("\n  }").unwrap_or(body.len())];
    for stop in [
      "crate::team_lock::PROFILE_LOCK.disconnect()",
      "sync::stop_subscription()",
      "crate::remote_session::stop_session_events()",
      "crate::mcp_remote::stop(None)",
    ] {
      assert!(body.contains(stop), "invalidate_session must call {stop}");
    }
  }

  #[test]
  fn every_api_call_starts_from_a_token_that_is_not_about_to_expire() {
    let source = include_str!("cloud_auth.rs");
    for owner in [
      "pub async fn api_call_with_retry<",
      "pub(crate) async fn authorized_request<",
    ] {
      let body = source
        .split(owner)
        .nth(1)
        .unwrap_or_else(|| panic!("{owner} must exist"));
      let body = &body[..body.find("\n  }").unwrap_or(body.len())];
      assert!(body.contains(".usable_access_token()"), "{owner}");
      assert!(body.contains("refresh_after_rejection("), "{owner}");
    }
  }
}
