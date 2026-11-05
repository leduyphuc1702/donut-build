//! The Cookie Bot when the account is served by a self-hosted control plane.
//!
//! The cloud version warms cookies on a leased host: the cloud owns the
//! schedule, the sites and the browsing, and `cookie_bot.rs` is only the wire
//! to it. With no cloud there is no host, so this module does the work here:
//!
//! - schedules, runs and saved site lists live in `settings/cookie_bot_local.json`;
//! - "run now" launches the profile on this machine (headless), opens each of
//!   the enrolment's sites in a tab, leaves it there for a share of the budget,
//!   closes it, and closes the browser when the budget is spent;
//! - a ticker checks the calendar once a minute while Donut is open and starts
//!   a run that is due.
//!
//! The result is a local cookie-warming loop, not the cloud bot's behaviour:
//! there is no consent-dialog handling, no persona, and no remote hours. The
//! UI shows exactly what happened, because every run this starts is recorded
//! with its visited and failed site counts.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::cloud_errors::{self, BackendFailure, FailureCodes};
use crate::cookie_bot::{
  CookieBotConflictCheck, CookieBotLimits, CookieBotPreset, CookieBotPresetList, CookieBotRun,
  CookieBotRunPage, CookieBotRunStarted, CookieBotSchedule, CookieBotScheduleDeleted,
  CookieBotScheduleInput, CookieBotScheduleList, CookieBotScheduleSaved, CookieBotSlot,
  CookieBotUsage, CookieBotUsageMember, CookieBotUsageProfile, CookieBotUserTemplate,
  RemoteHoursBreakdown, RemoteHoursMember, RemoteHoursQuota,
};

const MAX_SLOTS: usize = 14;
const MIN_MINUTES: u32 = 5;
const MAX_MINUTES: u32 = 120;
const MIN_SITES: usize = 1;
const MAX_SITES: usize = 40;
const MAX_TEMPLATE_NAME: usize = 80;
/// What the local quota meter reports. There is no billing here; the number
/// only has to be large enough never to look like a limit that will bite.
const GRANTED_HOURS: f64 = 100_000.0;
const TICK: Duration = Duration::from_secs(60);

const SCHEDULE_CODES: FailureCodes = FailureCodes {
  bad_request: "COOKIE_BOT_INVALID_SCHEDULE",
  forbidden: "COOKIE_BOT_NOT_ENTITLED",
  not_found: "COOKIE_BOT_NOT_ENROLLED",
  conflict: "COOKIE_BOT_SCHEDULE_CONFLICT",
};
const RUN_CODES: FailureCodes = FailureCodes {
  bad_request: "COOKIE_BOT_INVALID_SCHEDULE",
  forbidden: "COOKIE_BOT_NOT_ENTITLED",
  not_found: "COOKIE_BOT_RUN_NOT_FOUND",
  conflict: "COOKIE_BOT_RUN_IN_PROGRESS",
};
const TEMPLATE_CODES: FailureCodes = FailureCodes {
  bad_request: "COOKIE_BOT_INVALID_TEMPLATE_NAME",
  forbidden: "COOKIE_BOT_NOT_ENTITLED",
  not_found: "COOKIE_BOT_TEMPLATE_NOT_FOUND",
  conflict: "COOKIE_BOT_TEMPLATE_NAME_TAKEN",
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LocalState {
  #[serde(default)]
  schedules: HashMap<String, CookieBotSchedule>,
  #[serde(default)]
  runs: Vec<CookieBotRun>,
  #[serde(default)]
  templates: Vec<CookieBotUserTemplate>,
  #[serde(default)]
  next_run: u64,
}

static STATE: OnceLock<Mutex<LocalState>> = OnceLock::new();
/// Run ids the person stopped. The worker checks it between sites and before
/// its wait, so a stop is never overwritten by the finalize step.
static CANCEL: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);

fn path() -> std::path::PathBuf {
  crate::app_dirs::settings_dir().join("cookie_bot_local.json")
}

fn load() -> LocalState {
  let mut state = std::fs::read_to_string(path())
    .ok()
    .and_then(|raw| serde_json::from_str::<LocalState>(&raw).ok())
    .unwrap_or_default();
  // A file from a build that never advanced the counter must not reuse ids.
  let highest = state
    .runs
    .iter()
    .filter_map(|run| run.id.strip_prefix("local-run-"))
    .filter_map(|id| id.parse::<u64>().ok())
    .max()
    .unwrap_or(0);
  state.next_run = state.next_run.max(highest + 1);
  state
}

fn state() -> std::sync::MutexGuard<'static, LocalState> {
  STATE
    .get_or_init(|| Mutex::new(load()))
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn persist(state: &LocalState) {
  let Ok(bytes) = serde_json::to_vec(state) else {
    return;
  };
  if let Err(e) = crate::app_dirs::write_owner_only(&path(), &bytes) {
    log::warn!("Cookie bot local state not saved err=\"{e}\"");
  }
}

fn failure(status: u16, code: &str, codes: FailureCodes) -> crate::cookie_bot::CookieBotError {
  crate::cookie_bot::CookieBotError(cloud_errors::classify(
    status,
    &crate::backend_error(code),
    codes,
  ))
}

fn not_found(code: &str) -> crate::cookie_bot::CookieBotError {
  crate::cookie_bot::CookieBotError(BackendFailure {
    status: 404,
    code: code.to_string(),
    params: Default::default(),
  })
}

fn cancelled(run_id: &str) -> bool {
  CANCEL
    .lock()
    .ok()
    .and_then(|set| set.as_ref().map(|set| set.contains(run_id)))
    .unwrap_or(false)
}

fn mark_cancelled(run_id: &str) {
  if let Ok(mut slot) = CANCEL.lock() {
    slot
      .get_or_insert_with(Default::default)
      .insert(run_id.to_string());
  }
}

fn clear_cancelled(run_id: &str) {
  if let Ok(mut slot) = CANCEL.lock() {
    if let Some(set) = slot.as_mut() {
      set.remove(run_id);
    }
  }
}

fn now_iso() -> String {
  Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn valid_minutes(minutes: u32) -> Result<u32, crate::cookie_bot::CookieBotError> {
  if (MIN_MINUTES..=MAX_MINUTES).contains(&minutes) {
    Ok(minutes)
  } else {
    Err(failure(400, "COOKIE_BOT_INVALID_SCHEDULE", SCHEDULE_CODES))
  }
}

/// A web address the browser may open, on the same rule the agent uses.
fn valid_site(site: &str) -> bool {
  url::Url::parse(site.trim())
    .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

/// The slots a write describes, with the mirrored pair as the fallback.
fn checked_slots(
  input: &CookieBotScheduleInput,
) -> Result<Vec<CookieBotSlot>, crate::cookie_bot::CookieBotError> {
  let slots = input
    .slots
    .clone()
    .filter(|slots| !slots.is_empty())
    .unwrap_or_else(|| {
      vec![CookieBotSlot {
        days_mask: input.days_mask,
        run_at_minute: input.run_at_minute,
      }]
    });
  let invalid = || failure(400, "COOKIE_BOT_INVALID_SCHEDULE", SCHEDULE_CODES);
  if slots.len() > MAX_SLOTS {
    return Err(invalid());
  }
  for slot in &slots {
    if slot.days_mask == 0 || slot.days_mask > 0b0111_1111 || slot.run_at_minute >= 1440 {
      return Err(invalid());
    }
  }
  Ok(slots)
}

/// The earliest instant a calendar fires after `from`, in the schedule's zone.
fn next_run_at(
  slots: &[CookieBotSlot],
  timezone: &str,
  from: chrono::DateTime<Utc>,
) -> Option<String> {
  let tz: chrono_tz::Tz = timezone.parse().ok()?;
  let local = from.with_timezone(&tz);
  for offset in 0..8 {
    let date = local.date_naive() + chrono::Duration::days(offset);
    let weekday = date.weekday().num_days_from_monday();
    for slot in slots {
      if slot.days_mask & (1 << weekday) == 0 {
        continue;
      }
      let naive =
        date.and_hms_opt(0, 0, 0)? + chrono::Duration::minutes(i64::from(slot.run_at_minute));
      let Some(candidate) = tz.from_local_datetime(&naive).earliest() else {
        continue;
      };
      if candidate > local {
        return Some(
          candidate
            .with_timezone(&Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
      }
    }
  }
  None
}

async fn user_identity() -> (Option<String>, Option<String>) {
  match crate::cloud_auth::CLOUD_AUTH.get_user().await {
    Some(auth) => (Some(auth.user.id.clone()), Some(auth.user.email.clone())),
    None => (None, None),
  }
}

fn find_profile(profile_id: &str) -> Option<crate::profile::types::BrowserProfile> {
  crate::profile::ProfileManager::instance()
    .list_profiles()
    .ok()?
    .into_iter()
    .find(|profile| profile.id.to_string() == profile_id)
}

/// The bot drives this machine, so the platform a schedule runs on is the
/// host's, not the fingerprint OS `remote_platform` reports for a Wayfern
/// profile (that one is the leased-host platform of the cloud bot).
fn platform_of(_profile: &crate::profile::types::BrowserProfile) -> String {
  crate::profile::types::get_host_os()
}

async fn build_schedule(
  profile_id: &str,
  input: &CookieBotScheduleInput,
  previous: Option<&CookieBotSchedule>,
) -> Result<CookieBotSchedule, crate::cookie_bot::CookieBotError> {
  let profile = find_profile(profile_id);
  let slots = checked_slots(input)?;
  let timezone = input.timezone.trim().to_string();
  if timezone.parse::<chrono_tz::Tz>().is_err() {
    return Err(failure(400, "COOKIE_BOT_INVALID_SCHEDULE", SCHEDULE_CODES));
  }
  let max_minutes = valid_minutes(input.max_minutes)?;

  // A saved list stands in for the site list exactly as it does in the cloud:
  // the form sends `sites: []` when a template is chosen.
  let mut sites = input.sites.clone();
  if let Some(template_id) = input.template_id.as_deref() {
    if template_id.starts_with("user:") {
      let template = state()
        .templates
        .iter()
        .find(|template| template.id == template_id)
        .cloned()
        .ok_or_else(|| failure(404, "COOKIE_BOT_TEMPLATE_NOT_FOUND", TEMPLATE_CODES))?;
      sites = template.sites;
    } else {
      // The cloud's curated pools are server-owned and were never sent to this
      // desktop, so a local run cannot reproduce them.
      return Err(failure(
        404,
        "COOKIE_BOT_TEMPLATE_NOT_FOUND",
        TEMPLATE_CODES,
      ));
    }
  }
  sites.retain(|site| !site.trim().is_empty());
  if !(MIN_SITES..=MAX_SITES).contains(&sites.len()) || !sites.iter().all(|site| valid_site(site)) {
    return Err(failure(400, "COOKIE_BOT_INVALID_SCHEDULE", SCHEDULE_CODES));
  }

  let (owner_id, owner_email) = user_identity().await;
  let first = slots[0];
  let mut schedule = CookieBotSchedule {
    profile_id: profile_id.to_string(),
    profile_name: profile
      .as_ref()
      .map(|profile| profile.name.clone())
      .filter(|name| !name.trim().is_empty())
      .unwrap_or_else(|| input.profile_name.trim().to_string()),
    platform: profile
      .as_ref()
      .map(platform_of)
      .unwrap_or_else(|| input.platform.trim().to_string()),
    enabled: input.enabled,
    run_at_minute: first.run_at_minute,
    days_mask: first.days_mask,
    slots,
    timezone,
    preset: if input.preset.trim().is_empty() {
      "balanced".to_string()
    } else {
      input.preset.trim().to_string()
    },
    template_id: input.template_id.clone(),
    max_minutes,
    sites,
    jitter_seconds: input.jitter_seconds.unwrap_or(0),
    sync_enabled: input.sync_enabled,
    encrypted_sync: input.encrypted_sync,
    has_proxy: input.has_proxy,
    proxy_remote_reachable: input.proxy_remote_reachable,
    touch_fingerprint: input.touch_fingerprint,
    sticky_exit: input.sticky_exit,
    profile_state_at: Some(now_iso()),
    exit_check: previous.and_then(|previous| previous.exit_check.clone()),
    blocked_by: None,
    next_run_at: None,
    last_run_at: previous.and_then(|previous| previous.last_run_at.clone()),
    last_run_id: previous.and_then(|previous| previous.last_run_id.clone()),
    owner_user_id: owner_id,
    owner_email,
    updated_at: Some(now_iso()),
  };
  schedule.next_run_at = if schedule.enabled {
    next_run_at(&schedule.slots, &schedule.timezone, Utc::now())
  } else {
    None
  };
  Ok(schedule)
}

pub async fn save_schedule(
  profile_id: &str,
  input: &CookieBotScheduleInput,
  _acknowledge_conflict: bool,
) -> Result<CookieBotScheduleSaved, crate::cookie_bot::CookieBotError> {
  let previous = state().schedules.get(profile_id).cloned();
  let schedule = build_schedule(profile_id, input, previous.as_ref()).await?;
  {
    let mut state = state();
    state
      .schedules
      .insert(profile_id.to_string(), schedule.clone());
    persist(&state);
  }
  log::info!(
    "Cookie bot enrolment saved profile={profile_id} sites={}",
    schedule.sites.len()
  );
  Ok(CookieBotScheduleSaved {
    schedule,
    conflicts: Vec::new(),
  })
}

pub async fn list_schedules(
  scope: Option<&str>,
) -> Result<CookieBotScheduleList, crate::cookie_bot::CookieBotError> {
  let _ = scope;
  let mut schedules: Vec<CookieBotSchedule> = state().schedules.values().cloned().collect();
  // A renamed profile keeps its enrolment; show the current name, not the one
  // the form carried on the day it was saved.
  for schedule in &mut schedules {
    if let Some(profile) = find_profile(&schedule.profile_id) {
      schedule.profile_name = profile.name.clone();
      schedule.platform = platform_of(&profile);
    }
  }
  schedules.sort_by_key(|a| a.profile_name.to_lowercase());
  let user = crate::cloud_auth::CLOUD_AUTH.get_user().await;
  Ok(CookieBotScheduleList {
    schedules,
    team_id: user.as_ref().and_then(|auth| auth.user.team_id.clone()),
    scope: scope.map(str::to_string),
  })
}

pub fn get_schedule(
  profile_id: &str,
) -> Result<Option<CookieBotSchedule>, crate::cookie_bot::CookieBotError> {
  Ok(state().schedules.get(profile_id).cloned())
}

pub fn delete_schedule(profile_id: &str) -> CookieBotScheduleDeleted {
  let mut state = state();
  let deleted = state.schedules.remove(profile_id).is_some();
  if deleted {
    persist(&state);
  }
  CookieBotScheduleDeleted {
    profile_id: profile_id.to_string(),
    deleted,
  }
}

pub fn check_conflicts(
  profile_id: &str,
  _run_at_minute: Option<u16>,
  _timezone: Option<&str>,
  _days_mask: Option<u8>,
) -> CookieBotConflictCheck {
  CookieBotConflictCheck {
    profile_id: profile_id.to_string(),
    conflicts: Vec::new(),
  }
}

pub fn update_profile_state(
  profile_id: &str,
  input: crate::cookie_bot::ProfileState,
  identity: &crate::cookie_bot::ProfileIdentity,
) -> Result<CookieBotSchedule, crate::cookie_bot::CookieBotError> {
  let mut state = state();
  let Some(schedule) = state.schedules.get_mut(profile_id) else {
    return Err(not_found("COOKIE_BOT_NOT_ENROLLED"));
  };
  schedule.profile_state_at = Some(now_iso());
  schedule.sync_enabled = input.sync_enabled;
  schedule.encrypted_sync = input.encrypted_sync;
  schedule.has_proxy = input.has_proxy;
  schedule.proxy_remote_reachable = input.proxy_remote_reachable;
  schedule.touch_fingerprint = input.touch_fingerprint;
  schedule.sticky_exit = input.sticky_exit;
  schedule.blocked_by = None;
  if let Some(name) = &identity.profile_name {
    schedule.profile_name = name.clone();
  }
  if let Some(platform) = &identity.platform {
    schedule.platform = platform.clone();
  }
  let schedule = schedule.clone();
  persist(&state);
  Ok(schedule)
}

// --- Runs -------------------------------------------------------------------

/// Keep the history bounded. The log is a record of recent work, not an
/// archive; the local file is rewritten whole on every change.
fn push_run(state: &mut LocalState, run: CookieBotRun) {
  state.runs.push(run);
  if state.runs.len() > 1_000 {
    state
      .runs
      .sort_by(|a, b| a.scheduled_for.cmp(&b.scheduled_for));
    let excess = state.runs.len() - 1_000;
    state.runs.drain(..excess);
  }
}

fn new_run(
  schedule: &CookieBotSchedule,
  trigger: &str,
  max_minutes: u32,
  next_id: u64,
) -> CookieBotRun {
  let at = now_iso();
  CookieBotRun {
    id: format!("local-run-{next_id}"),
    profile_id: schedule.profile_id.clone(),
    profile_name: Some(schedule.profile_name.clone()),
    user_id: schedule.owner_user_id.clone(),
    email: schedule.owner_email.clone(),
    team_id: None,
    trigger: trigger.to_string(),
    status: "running".to_string(),
    scheduled_for: at.clone(),
    dispatch_after: Some(at.clone()),
    started_at: Some(at),
    ended_at: None,
    max_minutes,
    chunks_total: 1,
    chunk_index: 1,
    sites_total: schedule.sites.len() as u32,
    sites_visited: 0,
    sites_failed: 0,
    consent_dismissed: 0,
    billed_seconds: 0,
    outcome_code: None,
    session_id: None,
  }
}

pub fn run_now(
  profile_id: &str,
  max_minutes: Option<u32>,
) -> Result<CookieBotRunStarted, crate::cookie_bot::CookieBotError> {
  let Some(schedule) = state().schedules.get(profile_id).cloned() else {
    return Err(not_found("COOKIE_BOT_NOT_ENROLLED"));
  };
  let profile = find_profile(profile_id).ok_or_else(|| not_found("PROFILE_NOT_FOUND"))?;
  let minutes = valid_minutes(
    max_minutes
      .unwrap_or(schedule.max_minutes)
      .min(schedule.max_minutes.max(MIN_MINUTES)),
  )?;

  if profile
    .process_id
    .is_some_and(crate::proxy_storage::is_process_running)
  {
    let mut state = state();
    let run = new_run(&schedule, "manual", minutes, state.next_run.max(1));
    state.next_run = state.next_run.max(1) + 1;
    let mut skipped = run;
    skipped.status = "skipped".to_string();
    skipped.outcome_code = Some("profile_running".to_string());
    skipped.ended_at = Some(now_iso());
    let id = skipped.id.clone();
    if let Some(schedule) = state.schedules.get_mut(profile_id) {
      schedule.last_run_at = skipped.started_at.clone();
      schedule.last_run_id = Some(id.clone());
    }
    push_run(&mut state, skipped.clone());
    persist(&state);
    return Ok(CookieBotRunStarted {
      run: skipped,
      session_id: None,
    });
  }

  let mut state = state();
  let run = new_run(&schedule, "manual", minutes, state.next_run.max(1));
  state.next_run = state.next_run.max(1) + 1;
  if let Some(schedule) = state.schedules.get_mut(profile_id) {
    schedule.last_run_at = run.started_at.clone();
    schedule.last_run_id = Some(run.id.clone());
  }
  let id = run.id.clone();
  push_run(&mut state, run.clone());
  persist(&state);
  drop(state);

  if let Some(app) = APP.get().cloned() {
    start_worker(
      app,
      id,
      profile,
      schedule.clone(),
      minutes,
      run.scheduled_for.clone(),
    );
  }
  Ok(CookieBotRunStarted {
    run,
    session_id: None,
  })
}

/// The scheduler's own start, sharing the worker.
fn start_scheduled(schedule: CookieBotSchedule, minutes: u32) -> CookieBotRun {
  let mut state = state();
  let run = new_run(&schedule, "schedule", minutes, state.next_run.max(1));
  state.next_run = state.next_run.max(1) + 1;
  if let Some(stored) = state.schedules.get_mut(&schedule.profile_id) {
    stored.last_run_at = run.started_at.clone();
    stored.last_run_id = Some(run.id.clone());
    stored.next_run_at = next_run_at(&stored.slots, &stored.timezone, Utc::now());
  }
  push_run(&mut state, run.clone());
  persist(&state);
  drop(state);

  if let Some(app) = APP.get().cloned() {
    if let Some(profile) = find_profile(&schedule.profile_id) {
      start_worker(
        app,
        run.id.clone(),
        profile,
        schedule,
        minutes,
        run.scheduled_for.clone(),
      );
    }
  }
  run
}

fn start_worker(
  app: tauri::AppHandle,
  run_id: String,
  profile: crate::profile::types::BrowserProfile,
  schedule: CookieBotSchedule,
  minutes: u32,
  _scheduled_for: String,
) {
  tauri::async_runtime::spawn(async move {
    let started = std::time::Instant::now();
    let sites = schedule.sites.clone();
    let total_secs = u64::from(minutes) * 60;
    let per_site = (total_secs / sites.len().max(1) as u64).clamp(8, 120);

    let launch = crate::browser_runner::launch_browser_profile_impl(
      app.clone(),
      profile.clone(),
      None,
      crate::browser_runner::LaunchOptions {
        headless: true,
        force_new: true,
        // A warm-up run has no dialog to answer and cannot regenerate its
        // fingerprint mid-run. A mismatch is worth logging, not worth turning
        // the run into a failure the person never sees until it is over.
        gate: crate::launch_gate::FingerprintGate::Advisory,
        ..Default::default()
      },
    )
    .await;

    let launched = match launch {
      Ok(profile) => profile,
      Err(error) => {
        finish_run(
          &run_id,
          "failed",
          Some(error_code(&error)),
          0,
          0,
          sites.len() as u32,
          started,
        );
        return;
      }
    };
    log::info!(
      "Cookie bot local run started profile={} sites={} minutes={minutes}",
      launched.id,
      sites.len()
    );

    // The DevTools port is registered once the process is up; a tab opened
    // before that has nothing to talk to.
    let mut ready = false;
    for _ in 0..30 {
      if crate::browser_tabs::local_port(&launched).await.is_some() {
        ready = true;
        break;
      }
      tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if !ready {
      finish_run(
        &run_id,
        "failed",
        Some("BROWSER_TABS_UNAVAILABLE".to_string()),
        0,
        0,
        sites.len() as u32,
        started,
      );
      return;
    }

    let mut visited = 0u32;
    let mut failed = 0u32;
    for site in &sites {
      if cancelled(&run_id) || started.elapsed().as_secs() >= total_secs {
        break;
      }
      match crate::browser_tabs::open(&launched, site).await {
        Ok(tab) => {
          let remaining = total_secs.saturating_sub(started.elapsed().as_secs());
          let wait = per_site.min(remaining.max(3));
          sleep_cancellable(&run_id, wait).await;
          let _ = crate::browser_tabs::close(&launched, &tab.id).await;
          visited += 1;
        }
        Err(e) => {
          log::debug!(
            "Cookie bot local site failed profile={} err=\"{e}\"",
            launched.id
          );
          failed += 1;
        }
      }
      if !cancelled(&run_id) {
        sleep_cancellable(&run_id, 2 + (crate::proxy_manager::now_secs() % 4)).await;
      }
    }

    let stopped = cancelled(&run_id);
    if let Some(fresh) = find_profile(&launched.id.to_string()) {
      if let Err(e) = crate::browser_runner::kill_browser_profile(app.clone(), fresh).await {
        log::warn!(
          "Cookie bot local browser stop failed profile={} err=\"{e}\"",
          launched.id
        );
      }
    }
    let status = if stopped {
      "cancelled"
    } else if visited == 0 && failed > 0 {
      "failed"
    } else if failed > 0 || visited < sites.len() as u32 {
      "partial"
    } else {
      "succeeded"
    };
    finish_run(
      &run_id,
      status,
      None,
      visited,
      failed,
      sites.len() as u32,
      started,
    );
    log::info!(
      "Cookie bot local run finished profile={} status={status} visited={visited} failed={failed}",
      launched.id
    );
  });
}

async fn sleep_cancellable(run_id: &str, seconds: u64) {
  for _ in 0..seconds {
    if cancelled(run_id) {
      return;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
  }
}

fn error_code(message: &str) -> String {
  cloud_errors::classify_message(message, RUN_CODES).code
}

#[allow(clippy::too_many_arguments)]
fn finish_run(
  run_id: &str,
  status: &str,
  outcome_code: Option<String>,
  visited: u32,
  failed: u32,
  total: u32,
  started: std::time::Instant,
) {
  let mut state = state();
  let Some(run) = state.runs.iter_mut().find(|run| run.id == run_id) else {
    return;
  };
  // A cancel already marked the row; the worker must not turn it into a
  // completion after the browser came down, but the counters it did manage to
  // reach are still the truth.
  let was_cancelled = run.status == "cancelled" && status != "cancelled";
  if !was_cancelled {
    run.status = status.to_string();
  }
  run.sites_visited = visited;
  run.sites_failed = failed;
  run.sites_total = total;
  run.billed_seconds = started.elapsed().as_secs();
  run.ended_at = Some(now_iso());
  if let Some(code) = outcome_code {
    run.outcome_code = Some(code);
  }
  clear_cancelled(run_id);
  persist(&state);
}

pub fn list_runs(
  profile_id: Option<&str>,
  scope: Option<&str>,
  limit: Option<u32>,
  before: Option<&str>,
) -> CookieBotRunPage {
  let _ = scope;
  let mut runs: Vec<CookieBotRun> = state()
    .runs
    .iter()
    .filter(|run| profile_id.is_none_or(|id| run.profile_id == id))
    .cloned()
    .collect();
  runs.sort_by(|a, b| b.scheduled_for.cmp(&a.scheduled_for));
  if let Some(cursor) = before {
    if let Some(index) = runs.iter().position(|run| run.id == cursor) {
      runs.drain(..=index);
    }
  }
  let limit = limit.unwrap_or(50).clamp(1, 200) as usize;
  let next_before = (runs.len() > limit).then(|| runs[limit - 1].id.clone());
  runs.truncate(limit);
  CookieBotRunPage { runs, next_before }
}

pub fn cancel_run(run_id: &str) -> Result<CookieBotRun, crate::cookie_bot::CookieBotError> {
  let mut state = state();
  let Some(run) = state.runs.iter_mut().find(|run| run.id == run_id) else {
    return Err(not_found("COOKIE_BOT_RUN_NOT_FOUND"));
  };
  if run.status == "running" {
    mark_cancelled(run_id);
    run.status = "cancelled".to_string();
    run.ended_at = Some(now_iso());
  }
  let run = run.clone();
  persist(&state);
  Ok(run)
}

// --- Presets, templates and usage -------------------------------------------

pub fn list_presets() -> CookieBotPresetList {
  let preset = |id: &str, minutes: u32, recommended: bool| CookieBotPreset {
    id: id.to_string(),
    typical_minutes: Some(minutes),
    recommended,
    name: None,
    description: None,
  };
  CookieBotPresetList {
    presets: vec![
      preset("light", 20, false),
      preset("balanced", 40, true),
      preset("deep", 90, false),
    ],
    default_preset: Some("balanced".to_string()),
    // The curated pools are the cloud's; a local enrolment browses the sites
    // the person listed, or one of their own saved lists.
    templates: Vec::new(),
    limits: Some(CookieBotLimits {
      min_minutes: Some(MIN_MINUTES),
      max_minutes: Some(MAX_MINUTES),
      min_sites: Some(MIN_SITES as u32),
      max_sites: Some(MAX_SITES as u32),
      max_slots: Some(MAX_SLOTS as u32),
      max_template_name_length: Some(MAX_TEMPLATE_NAME as u32),
    }),
  }
}

fn checked_template_name(name: &str) -> Result<String, crate::cookie_bot::CookieBotError> {
  let name = name.trim();
  let length = name.chars().count();
  if !(1..=MAX_TEMPLATE_NAME).contains(&length) {
    return Err(failure(
      400,
      "COOKIE_BOT_INVALID_TEMPLATE_NAME",
      TEMPLATE_CODES,
    ));
  }
  Ok(name.to_string())
}

fn checked_template_sites(
  sites: &[String],
) -> Result<Vec<String>, crate::cookie_bot::CookieBotError> {
  let sites: Vec<String> = sites
    .iter()
    .map(|site| site.trim().to_string())
    .filter(|site| !site.is_empty())
    .collect();
  if !(MIN_SITES..=MAX_SITES).contains(&sites.len()) || !sites.iter().all(|site| valid_site(site)) {
    return Err(failure(
      400,
      "COOKIE_BOT_INVALID_TEMPLATE_NAME",
      TEMPLATE_CODES,
    ));
  }
  Ok(sites)
}

pub fn list_user_templates() -> Vec<CookieBotUserTemplate> {
  let mut templates = state().templates.clone();
  templates.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
  templates
}

pub fn create_user_template(
  name: &str,
  sites: &[String],
) -> Result<CookieBotUserTemplate, crate::cookie_bot::CookieBotError> {
  let name = checked_template_name(name)?;
  let sites = checked_template_sites(sites)?;
  let mut state = state();
  if state
    .templates
    .iter()
    .any(|template| template.name.eq_ignore_ascii_case(&name))
  {
    return Err(failure(
      409,
      "COOKIE_BOT_TEMPLATE_NAME_TAKEN",
      TEMPLATE_CODES,
    ));
  }
  let template = CookieBotUserTemplate {
    id: format!("user:{}", uuid::Uuid::new_v4()),
    name,
    sites,
    updated_at: Some(now_iso()),
  };
  state.templates.push(template.clone());
  persist(&state);
  Ok(template)
}

pub fn update_user_template(
  id: &str,
  name: Option<&str>,
  sites: Option<&[String]>,
) -> Result<CookieBotUserTemplate, crate::cookie_bot::CookieBotError> {
  let name = name.map(checked_template_name).transpose()?;
  let sites = sites.map(checked_template_sites).transpose()?;
  let mut state = state();
  if let Some(name) = &name {
    if state
      .templates
      .iter()
      .any(|template| template.id != id && template.name.eq_ignore_ascii_case(name))
    {
      return Err(failure(
        409,
        "COOKIE_BOT_TEMPLATE_NAME_TAKEN",
        TEMPLATE_CODES,
      ));
    }
  }
  let Some(template) = state
    .templates
    .iter_mut()
    .find(|template| template.id == id)
  else {
    return Err(not_found("COOKIE_BOT_TEMPLATE_NOT_FOUND"));
  };
  if let Some(name) = name {
    template.name = name;
  }
  if let Some(sites) = sites {
    template.sites = sites;
  }
  template.updated_at = Some(now_iso());
  let template = template.clone();
  persist(&state);
  Ok(template)
}

pub fn delete_user_template(id: &str) -> bool {
  let mut state = state();
  let before = state.templates.len();
  state.templates.retain(|template| template.id != id);
  let deleted = state.templates.len() != before;
  if deleted {
    persist(&state);
  }
  deleted
}

fn period_bounds(period: &str) -> Option<(String, String, String)> {
  let (year, month) = period.split_once('-')?;
  let year: i32 = year.parse().ok()?;
  let month: u32 = month.parse().ok()?;
  let start = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
  let end = if month == 12 {
    chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)?
  } else {
    chrono::NaiveDate::from_ymd_opt(year, month + 1, 1)?
  };
  Some((
    start
      .and_hms_opt(0, 0, 0)?
      .and_utc()
      .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    end
      .and_hms_opt(0, 0, 0)?
      .and_utc()
      .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    format!("{year:04}-{month:02}"),
  ))
}

fn runs_in_period(period: &str) -> Vec<CookieBotRun> {
  state()
    .runs
    .iter()
    .filter(|run| {
      run
        .started_at
        .as_deref()
        .is_some_and(|at| at.starts_with(period))
    })
    .cloned()
    .collect()
}

pub async fn team_usage(period: Option<&str>) -> CookieBotUsage {
  let default_period = Utc::now().format("%Y-%m").to_string();
  let (start, end, period) = period
    .and_then(period_bounds)
    .unwrap_or_else(|| period_bounds(&default_period).expect("a valid current month"));
  let runs = runs_in_period(&period);
  let bot_hours = runs
    .iter()
    .map(|run| run.billed_seconds as f64 / 3600.0)
    .sum::<f64>();
  let failed = runs
    .iter()
    .filter(|run| matches!(run.status.as_str(), "failed" | "partial"))
    .count() as u32;
  let (user_id, email) = user_identity().await;
  let user_id = user_id.unwrap_or_else(|| "selfhosted-owner".to_string());
  let email = email.unwrap_or_else(|| "owner@selfhosted.local".to_string());
  let member = CookieBotUsageMember {
    user_id: user_id.clone(),
    email: email.clone(),
    role: Some("owner".to_string()),
    interactive_hours: 0.0,
    bot_hours,
    used_hours: bot_hours,
    sessions: runs.len() as u32,
    bot_runs: runs.len() as u32,
    bot_runs_failed: failed,
  };
  let mut profiles: Vec<CookieBotUsageProfile> = Vec::new();
  for run in &runs {
    let entry = profiles
      .iter_mut()
      .find(|profile| profile.profile_id == run.profile_id);
    match entry {
      Some(profile) => {
        profile.runs += 1;
        profile.bot_hours += run.billed_seconds as f64 / 3600.0;
        if matches!(run.status.as_str(), "failed" | "partial") {
          profile.runs_failed += 1;
        }
        if profile
          .last_run_at
          .as_deref()
          .is_none_or(|at| run.started_at.as_deref().is_some_and(|start| start > at))
        {
          profile.last_run_at = run.started_at.clone();
          profile.last_status = Some(run.status.clone());
        }
      }
      None => profiles.push(CookieBotUsageProfile {
        profile_id: run.profile_id.clone(),
        profile_name: run.profile_name.clone(),
        owner_email: Some(email.clone()),
        bot_hours: run.billed_seconds as f64 / 3600.0,
        runs: 1,
        runs_failed: u32::from(matches!(run.status.as_str(), "failed" | "partial")),
        last_run_at: run.started_at.clone(),
        last_status: Some(run.status.clone()),
      }),
    }
  }
  let used_hours = member.used_hours;
  CookieBotUsage {
    period,
    period_start: Some(start),
    period_end: Some(end),
    team_id: crate::cloud_auth::CLOUD_AUTH
      .get_user()
      .await
      .and_then(|auth| auth.user.team_id),
    seats: 1,
    granted_hours: GRANTED_HOURS,
    used_hours,
    remaining_hours: (GRANTED_HOURS - used_hours).max(0.0),
    members: vec![member],
    profiles,
  }
}

pub async fn remote_hours_quota() -> RemoteHoursQuota {
  let usage = team_usage(None).await;
  RemoteHoursQuota {
    granted_hours: usage.granted_hours,
    remaining_hours: usage.remaining_hours,
    used_hours: usage.used_hours,
    period_start: usage.period_start.clone(),
    period_end: usage.period_end.clone(),
    scope: Some("user".to_string()),
    team_id: usage.team_id.clone(),
    seats: 1,
    per_seat_hours: 0.0,
    breakdown: Some(RemoteHoursBreakdown {
      interactive_hours: 0.0,
      bot_hours: usage.used_hours,
    }),
    members: usage
      .members
      .into_iter()
      .map(|member| RemoteHoursMember {
        user_id: member.user_id,
        email: member.email,
        role: member.role,
        used_hours: member.used_hours,
        interactive_hours: member.interactive_hours,
        bot_hours: member.bot_hours,
      })
      .collect(),
  }
}

// --- The local calendar -----------------------------------------------------

/// Start the once-a-minute calendar. Safe to call more than once; the second
/// call only stores the handle for "run now".
pub fn start(app: tauri::AppHandle) {
  let _ = APP.set(app);
  if STARTED.swap(true, Ordering::SeqCst) {
    return;
  }
  tauri::async_runtime::spawn(async {
    loop {
      tokio::time::sleep(TICK).await;
      tick().await;
    }
  });
}

async fn tick() {
  if !crate::cloud_auth::is_self_hosted_account() {
    return;
  }
  if !crate::cloud_auth::CLOUD_AUTH.is_logged_in().await {
    return;
  }
  let now = Utc::now();
  let due: Vec<CookieBotSchedule> = {
    let state = state();
    // One local browser at a time: the machine is someone's desk, not a fleet.
    if state.runs.iter().any(|run| run.status == "running") {
      return;
    }
    state
      .schedules
      .values()
      .filter(|schedule| schedule.enabled)
      .filter(|schedule| {
        schedule
          .next_run_at
          .as_deref()
          .is_some_and(|at| chrono::DateTime::parse_from_rfc3339(at).is_ok_and(|at| at <= now))
      })
      .cloned()
      .collect()
  };
  for schedule in due {
    let profile = match find_profile(&schedule.profile_id) {
      Some(profile) => profile,
      None => {
        let mut state = state();
        if let Some(stored) = state.schedules.get_mut(&schedule.profile_id) {
          stored.next_run_at = next_run_at(&stored.slots, &stored.timezone, now);
        }
        persist(&state);
        continue;
      }
    };
    if profile
      .process_id
      .is_some_and(crate::proxy_storage::is_process_running)
    {
      // The browser is already open for the person; a bot run would fight it.
      let mut state = state();
      if let Some(stored) = state.schedules.get_mut(&schedule.profile_id) {
        stored.next_run_at = next_run_at(&stored.slots, &stored.timezone, now);
      }
      persist(&state);
      continue;
    }
    start_scheduled(schedule.clone(), schedule.max_minutes);
    break;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[allow(unused_imports)]
  use crate::cookie_bot::CookieBotConflict;

  fn slot(days_mask: u8, run_at_minute: u16) -> CookieBotSlot {
    CookieBotSlot {
      days_mask,
      run_at_minute,
    }
  }

  #[test]
  fn the_next_run_lands_on_a_listed_weekday_at_the_listed_time() {
    // Monday 2026-11-02 09:00 UTC.
    let from = Utc.with_ymd_and_hms(2026, 11, 2, 9, 0, 0).unwrap();
    let next = next_run_at(&[slot(0b0000_0010, 600)], "UTC", from).unwrap();
    // Tuesday 10:00.
    assert_eq!(next, "2026-11-03T10:00:00Z");
  }

  #[test]
  fn a_time_that_has_passed_today_moves_to_next_week() {
    let from = Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap();
    let next = next_run_at(&[slot(0b0000_0001, 600)], "UTC", from).unwrap();
    assert_eq!(next, "2026-11-09T10:00:00Z");
  }

  #[test]
  fn a_schedule_with_no_weekday_is_refused() {
    let input = CookieBotScheduleInput {
      profile_name: "p".into(),
      platform: "macos".into(),
      enabled: true,
      run_at_minute: 600,
      days_mask: 0,
      slots: None,
      timezone: "UTC".into(),
      preset: "balanced".into(),
      template_id: None,
      max_minutes: 40,
      sites: vec!["https://example.test".into()],
      jitter_seconds: None,
      sync_enabled: false,
      has_proxy: false,
      proxy_remote_reachable: false,
      encrypted_sync: false,
      touch_fingerprint: false,
      sticky_exit: false,
      exit_alive: true,
    };
    assert!(checked_slots(&input).is_err());
  }

  #[test]
  fn only_http_and_https_sites_are_accepted() {
    assert!(valid_site("https://shop.example"));
    assert!(valid_site("http://shop.example/path"));
    assert!(!valid_site("file:///etc/passwd"));
    assert!(!valid_site("javascript:alert(1)"));
    assert!(!valid_site("not a url"));
  }

  #[test]
  fn a_period_is_a_calendar_month() {
    let (start, end, period) = period_bounds("2026-02").unwrap();
    assert_eq!(period, "2026-02");
    assert_eq!(start, "2026-02-01T00:00:00Z");
    assert_eq!(end, "2026-03-01T00:00:00Z");
    assert!(period_bounds("2026-13").is_none());
    assert!(period_bounds("February").is_none());
  }
}
