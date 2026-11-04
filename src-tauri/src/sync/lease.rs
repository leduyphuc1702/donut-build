//! Sync leases: one device at a time runs a synced profile.
//!
//! A browser rotates its session cookies while it runs. When two devices run
//! the same profile, or one starts from an older copy, the site revokes the
//! session on both. Before a synced profile launches, this device takes the
//! profile's lease on the sync server (`/v1/locks/*`), renews it while the
//! browser runs, and releases it after the browser exits and the upload that
//! follows is done. A server without the routes, or one that cannot be
//! reached, never blocks a launch.
//!
//! Handing a profile over: the device that wants it asks the holder to give
//! it up (`request_profile_release`). The holder reads the request on its next
//! renew, closes its browser the way Chromium needs to write its cookies,
//! uploads, and releases; the asking device waits for that and then launches
//! as usual, from the copy the holder just uploaded. A holder that loses its
//! lease to a forced take-over closes its browser too, so two devices never
//! run the same session.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{SyncClient, SyncEngine};
use crate::log_streak::{KeyedStreak, Streak};
use crate::profile::BrowserProfile;

/// What the lease is asked for; the server bounds it to 90-300 s.
const TTL_SECS: u64 = 120;
/// Also how soon a holder hears that another device asked for the profile.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// How often a device waiting for a profile asks how far the holder got.
const RELEASE_POLL: Duration = Duration::from_secs(2);
/// A holder that has not answered a request by then is not coming: it is
/// offline, asleep, or runs an app without hand-over.
const RELEASE_ANSWER_LIMIT: Duration = Duration::from_secs(30);
/// An answered hand-over that takes longer than this is given up on.
const RELEASE_LIMIT: Duration = Duration::from_secs(600);
/// A stopped profile whose upload never reports back still frees its lease.
const STOPPED_LIMIT: Duration = Duration::from_secs(3600);
/// Written into the profile when another device took the lease over: the
/// changes of this session must not win over that device's.
const LOST_MARK: &str = "lease-lost";

static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
static HELD: LazyLock<Mutex<HashMap<String, Held>>> = LazyLock::new(Default::default);
static HEARTBEAT_STARTED: OnceLock<()> = OnceLock::new();

static UNSUPPORTED: Streak = Streak::new(module_path!(), "Sync lease routes missing on server");
static UNREACHABLE: Streak = Streak::new(module_path!(), "Sync lease server unreachable");
static RENEW: KeyedStreak = KeyedStreak::new(module_path!(), "Sync lease renew");

struct Held {
  token: String,
  root: String,
  client: SyncClient,
  acquired: Instant,
  /// Set once the browser stopped: the lease waits for the upload.
  stopped: Option<Instant>,
  /// Set once another device asked for the profile: this device is closing
  /// its browser and uploading, and says so on each renew.
  handing_over: bool,
}

impl Held {
  fn release_stage(&self) -> Option<&'static str> {
    release_stage(self.handing_over, self.stopped.is_some())
  }
}

/// What a holder reports on its renews while it hands the profile over.
fn release_stage(handing_over: bool, stopped: bool) -> Option<&'static str> {
  match (handing_over, stopped) {
    (false, _) => None,
    (true, false) => Some("closing"),
    (true, true) => Some("uploading"),
  }
}

/// Called once at startup: the sync client needs the app handle.
/// How long after startup the leases of browsers still running from an
/// earlier session are taken again: sign-in and sync settings load first.
const RESUME_AFTER: Duration = Duration::from_secs(15);

/// Called once at startup: the sync client needs the app handle.
pub fn init(app: &tauri::AppHandle) {
  let _ = APP.set(app.clone());
  tauri::async_runtime::spawn(async {
    tokio::time::sleep(RESUME_AFTER).await;
    resume_running().await;
  });
}

/// Takes the lease again for each synced profile whose browser outlived the
/// last session, and renews it from now on. Another device holding it now
/// is a lost lease.
async fn resume_running() {
  let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() else {
    return;
  };
  for profile in profiles {
    let profile_id = profile.id.to_string();
    if !profile.is_sync_enabled() || !crate::browser_runner::browser_is_running_for(&profile_id) {
      continue;
    }
    if let Err(refusal) = take(&profile, false).await {
      let Ok(body) = serde_json::from_str::<Value>(&refusal) else {
        continue;
      };
      let holder = json!({
        "holder": { "deviceId": "", "deviceName": body["params"]["device"] },
      });
      notify_lost(&profile_id, &holder);
    }
  }
}

fn held() -> std::sync::MutexGuard<'static, HashMap<String, Held>> {
  HELD
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// This install's id: random, kept beside the app's data.
fn device_id() -> String {
  static ID: OnceLock<String> = OnceLock::new();
  ID.get_or_init(|| {
    let path = crate::app_dirs::data_dir().join("device-id");
    if let Some(id) = std::fs::read_to_string(&path)
      .ok()
      .map(|s| s.trim().to_string())
      .filter(|s| {
        !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
      })
    {
      return id;
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
      let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, &id) {
      log::warn!("Device id not saved err=\"{e}\"");
    }
    id
  })
  .clone()
}

fn device_name() -> String {
  sysinfo::System::host_name()
    .ok()
    .map(|h| h.trim().to_string())
    .filter(|h| !h.is_empty())
    .unwrap_or_else(|| "Donut Browser".to_string())
}

/// A short stand-in for a device id in the log.
pub(crate) fn short(id: &str) -> String {
  use sha2::Digest;
  sha2::Sha256::digest(id.as_bytes())[..4]
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect()
}

fn lost_mark(profile_id: &str) -> PathBuf {
  crate::app_dirs::profiles_dir()
    .join(profile_id)
    .join(".donut-sync")
    .join(LOST_MARK)
}

/// Whether another device took this profile's lease over while it ran here.
pub(crate) fn was_taken_over(profile_id: &str) -> bool {
  lost_mark(profile_id).exists()
}

pub(crate) fn clear_taken_over(profile_id: &str) {
  let _ = std::fs::remove_file(lost_mark(profile_id));
}

fn note_taken_over(profile_id: &str) {
  let path = lost_mark(profile_id);
  if let Some(parent) = path.parent() {
    let _ = std::fs::create_dir_all(parent);
  }
  let _ = std::fs::write(path, crate::proxy_manager::now_secs().to_string());
}

/// The refusal the launch shows, in the app's error-code shape.
fn held_error(profile_id: &str, body: &Value) -> String {
  json!({
    "code": "PROFILE_LEASE_HELD",
    "params": {
      "profileId": profile_id,
      "device": body["holder"]["deviceName"].as_str().unwrap_or(""),
      "expiresAt": body["expiresAt"].as_str().unwrap_or(""),
      "canTakeOver": body["sameAccount"].as_bool().unwrap_or(false).to_string(),
    }
  })
  .to_string()
}

/// The client and the profile's root on the server, when the profile syncs
/// to a server this device is set up for.
async fn target(profile: &BrowserProfile) -> Option<(SyncClient, String)> {
  if !profile.is_sync_enabled() || !super::is_sync_configured() {
    return None;
  }
  let app = APP.get()?;
  if SyncEngine::is_self_hosted_sync().await && profile.created_by_id.is_some() {
    return None;
  }
  let engine = SyncEngine::create_from_settings(app).await.ok()?;
  let prefix = SyncEngine::get_team_key_prefix(profile).await;
  Some((engine.client, format!("{prefix}profiles/{}/", profile.id)))
}

/// Takes the profile's lease before its browser starts. `Ok(true)` when a
/// lease was taken; `Ok(false)` when none applies or the server cannot give
/// one; `Err` with `PROFILE_LEASE_HELD` when another device holds it.
/// `force` takes it from another device of the same account.
pub(crate) async fn acquire(profile: &BrowserProfile, force: bool) -> Result<bool, String> {
  let taken = take(profile, force).await?;
  // Another device may have run the profile since this one last synced it:
  // its newest copy comes down before the browser starts. A forced take is
  // followed by a launch, which pulls then.
  if taken && !force {
    if let Some(app) = APP.get() {
      if let Err(refusal) = super::engine::pull_before_launch(app, profile).await {
        // No browser starts, so the lease is not needed.
        release(&profile.id.to_string(), "pull_unfinished").await;
        return Err(refusal);
      }
    }
  }
  Ok(taken)
}

async fn take(profile: &BrowserProfile, force: bool) -> Result<bool, String> {
  let Some((client, root)) = target(profile).await else {
    return Ok(false);
  };
  let profile_id = profile.id.to_string();
  let device = device_id();
  let started = Instant::now();
  let body = json!({
    "profileRoot": root,
    "deviceId": device,
    "deviceName": device_name(),
    "ttlSeconds": TTL_SECS,
    "force": force,
  });
  let (status, reply) = match client.lease("acquire", &body).await {
    Ok(answer) => {
      UNREACHABLE.succeeded();
      answer
    }
    Err(e) => {
      UNREACHABLE.failed(format_args!("profile={profile_id} err=\"{e}\""));
      return Ok(false);
    }
  };
  match status {
    200 => {
      UNSUPPORTED.succeeded();
      let Some(token) = reply["token"].as_str() else {
        log::warn!("Sync lease answer has no token profile={profile_id}");
        return Ok(false);
      };
      held().insert(
        profile_id.clone(),
        Held {
          token: token.to_string(),
          root,
          client,
          acquired: Instant::now(),
          stopped: None,
          handing_over: false,
        },
      );
      clear_taken_over(&profile_id);
      start_heartbeat();
      log::info!(
        "Sync lease acquired profile={profile_id} device={} force={force} elapsed_ms={}",
        short(&device),
        started.elapsed().as_millis()
      );
      Ok(true)
    }
    409 => {
      log::info!(
        "Sync lease refused profile={profile_id} holder={} expires_at={} elapsed_ms={}",
        short(reply["holder"]["deviceId"].as_str().unwrap_or("")),
        reply["expiresAt"].as_str().unwrap_or("-"),
        started.elapsed().as_millis()
      );
      Err(held_error(&profile_id, &reply))
    }
    404 => {
      UNSUPPORTED.failed(format_args!("profile={profile_id} status=404"));
      Ok(false)
    }
    _ => {
      UNREACHABLE.failed(format_args!("profile={profile_id} status={status}"));
      Ok(false)
    }
  }
}

fn find_profile(profile_id: &str) -> Option<BrowserProfile> {
  crate::profile::ProfileManager::instance()
    .list_profiles()
    .ok()?
    .into_iter()
    .find(|p| p.id.to_string() == profile_id)
}

fn profile_for_command(profile_id: &str) -> Result<BrowserProfile, String> {
  crate::profile::ProfileManager::instance()
    .list_profiles()
    .map_err(|e| crate::wrap_backend_error(e, "Failed to list profiles"))?
    .into_iter()
    .find(|p| p.id.to_string() == profile_id)
    .ok_or_else(|| crate::backend_error("PROFILE_NOT_FOUND"))
}

/// Takes a profile's lease from another device of this account, after the
/// person confirmed it. The launch that follows takes it again as usual.
#[tauri::command]
pub async fn take_over_profile_lease(profile_id: String) -> Result<(), String> {
  let profile = profile_for_command(&profile_id)?;
  acquire(&profile, true).await?;
  if let Some(lease) = held().get_mut(&profile_id) {
    // Nothing runs yet: a launch that never comes frees it again.
    lease.stopped = Some(Instant::now());
  }
  Ok(())
}

fn no_answer(profile_id: &str, device: &str) -> String {
  json!({
    "code": "PROFILE_RELEASE_NO_ANSWER",
    "params": { "profileId": profile_id, "device": device },
  })
  .to_string()
}

fn emit_release_progress(profile_id: &str, device: &str, stage: Option<&str>) {
  let _ = crate::events::emit(
    "profile-sync-release-progress",
    json!({ "profile_id": profile_id, "device": device, "stage": stage }),
  );
}

/// Asks the device that runs the profile to close it, upload and release,
/// and waits until it did. `Ok` once the profile is free: the launch that
/// follows takes the lease and pulls what that device uploaded.
/// `PROFILE_RELEASE_NO_ANSWER` when that device does not answer in time,
/// so the person can take the profile over from the last synced copy.
#[tauri::command]
pub async fn request_profile_release(profile_id: String) -> Result<(), String> {
  let profile = profile_for_command(&profile_id)?;
  let Some((client, root)) = target(&profile).await else {
    return Ok(());
  };
  let device = device_id();
  let ask = json!({ "profileRoot": root, "deviceId": device, "deviceName": device_name() });
  let started = Instant::now();
  let (status, reply) = client
    .lease("request-release", &ask)
    .await
    .map_err(|_| no_answer(&profile_id, ""))?;
  match status {
    200 => {}
    409 => return Err(held_error(&profile_id, &reply)),
    // A server without hand-over: only a take-over is left.
    _ => return Err(no_answer(&profile_id, "")),
  }
  if reply["requested"] != json!(true) {
    return Ok(());
  }
  let holder = reply["holder"]["deviceId"]
    .as_str()
    .unwrap_or("")
    .to_string();
  let holder_name = reply["holder"]["deviceName"]
    .as_str()
    .unwrap_or("")
    .to_string();
  log::info!(
    "Sync lease release asked profile={profile_id} holder={}",
    short(&holder)
  );
  emit_release_progress(&profile_id, &holder_name, None);
  let status_body = json!({ "profileRoot": root });
  let mut stage: Option<String> = None;
  loop {
    tokio::time::sleep(RELEASE_POLL).await;
    let answered = stage.is_some();
    if started.elapsed() > RELEASE_LIMIT || (!answered && started.elapsed() > RELEASE_ANSWER_LIMIT)
    {
      log::warn!(
        "Sync lease release not answered profile={profile_id} holder={} answered={answered} elapsed_ms={}",
        short(&holder),
        started.elapsed().as_millis()
      );
      return Err(no_answer(&profile_id, &holder_name));
    }
    let Ok((200, now)) = client.lease("status", &status_body).await else {
      continue;
    };
    if now["held"] != json!(true) {
      log::info!(
        "Sync lease handed over profile={profile_id} holder={} elapsed_ms={}",
        short(&holder),
        started.elapsed().as_millis()
      );
      return Ok(());
    }
    if now["holder"]["deviceId"].as_str() != Some(holder.as_str()) {
      // A third device got there first.
      return Err(held_error(&profile_id, &now));
    }
    let reported = now["releaseRequested"]["stage"]
      .as_str()
      .map(str::to_string);
    if reported != stage {
      stage = reported;
      emit_release_progress(&profile_id, &holder_name, stage.as_deref());
    }
  }
}

async fn sync_pending(profile_id: &str) -> bool {
  match super::get_global_scheduler() {
    Some(scheduler) => scheduler.is_profile_sync_in_progress(profile_id).await,
    None => false,
  }
}

/// The browser stopped. The lease stays until the upload that follows is
/// done, or goes now when no upload is coming.
pub(crate) async fn browser_stopped(profile_id: &str) {
  {
    let mut leases = held();
    let Some(lease) = leases.get_mut(profile_id) else {
      return;
    };
    if lease.stopped.is_none() {
      lease.stopped = Some(Instant::now());
    }
  }
  if !sync_pending(profile_id).await {
    release(profile_id, "no_upload").await;
  }
}

/// A sync of the profile ended. After the browser stopped, that sync was
/// its upload: a success frees the lease; a failure leaves it to expire, so
/// another device does not start from a copy that lacks this session.
pub(crate) async fn sync_finished(profile_id: &str, uploaded: bool) {
  let stopped = held()
    .get(profile_id)
    .is_some_and(|lease| lease.stopped.is_some());
  if !stopped || crate::browser_runner::browser_is_running_for(profile_id) {
    return;
  }
  if uploaded {
    release(profile_id, "uploaded").await;
  } else if held().remove(profile_id).is_some() {
    log::warn!("Sync lease left to expire, upload failed profile={profile_id}");
  }
}

async fn release(profile_id: &str, reason: &str) {
  let Some(lease) = held().remove(profile_id) else {
    return;
  };
  let started = Instant::now();
  let body = json!({ "profileRoot": lease.root, "token": lease.token });
  match lease.client.lease("release", &body).await {
    Ok((200, _)) => log::info!(
      "Sync lease released profile={profile_id} reason={reason} held_ms={} elapsed_ms={}",
      lease.acquired.elapsed().as_millis(),
      started.elapsed().as_millis()
    ),
    Ok((status, _)) => {
      log::warn!("Sync lease release refused profile={profile_id} status={status}")
    }
    Err(e) => log::warn!("Sync lease release failed, it expires profile={profile_id} err=\"{e}\""),
  }
}

fn start_heartbeat() {
  if HEARTBEAT_STARTED.set(()).is_err() {
    return;
  }
  tauri::async_runtime::spawn(async {
    loop {
      tokio::time::sleep(HEARTBEAT).await;
      heartbeat().await;
    }
  });
}

type Renewal = (
  String,
  String,
  String,
  SyncClient,
  Option<Instant>,
  Option<&'static str>,
);

async fn heartbeat() {
  let leases: Vec<Renewal> = held()
    .iter()
    .map(|(id, l)| {
      (
        id.clone(),
        l.root.clone(),
        l.token.clone(),
        l.client.clone(),
        l.stopped,
        l.release_stage(),
      )
    })
    .collect();
  for (profile_id, root, token, client, stopped, stage) in leases {
    if let Some(at) = stopped {
      if at.elapsed() > STOPPED_LIMIT || !sync_pending(&profile_id).await {
        release(
          &profile_id,
          if at.elapsed() > STOPPED_LIMIT {
            "stopped_limit"
          } else {
            "no_upload"
          },
        )
        .await;
        continue;
      }
    }
    let body = renew_body(&root, &token, stage);
    match client.lease("renew", &body).await {
      Ok((200, reply)) => {
        RENEW.succeeded(&format!("profile={profile_id}"));
        if stage.is_none() && reply["releaseRequested"].is_object() {
          release_requested(&profile_id, &token, &reply["releaseRequested"]);
        }
      }
      Ok((409, reply)) => lost(&profile_id, &token, &reply),
      Ok((404, _)) => {
        // The server lost the routes (a downgrade): nothing left to renew.
        held().remove(&profile_id);
        UNSUPPORTED.failed(format_args!("profile={profile_id} status=404"));
      }
      Ok((status, _)) => RENEW.failed(
        &format!("profile={profile_id}"),
        format_args!("status={status}"),
      ),
      Err(e) => RENEW.failed(
        &format!("profile={profile_id}"),
        format_args!("err=\"{e}\""),
      ),
    }
  }
}

fn renew_body(root: &str, token: &str, stage: Option<&str>) -> Value {
  let mut body = json!({ "profileRoot": root, "token": token, "ttlSeconds": TTL_SECS });
  if let Some(stage) = stage {
    body["releaseStage"] = json!(stage);
  }
  body
}

fn profile_name(profile_id: &str) -> String {
  find_profile(profile_id).map(|p| p.name).unwrap_or_default()
}

/// Another device asked for the profile. Answer at once, so it knows this
/// device is on it, then close the browser; the stop that follows uploads
/// and releases as after any other close.
fn release_requested(profile_id: &str, token: &str, request: &Value) {
  let answer = {
    let mut leases = held();
    let Some(lease) = leases
      .get_mut(profile_id)
      .filter(|lease| lease.token == token && !lease.handing_over)
    else {
      return;
    };
    lease.handing_over = true;
    (
      lease.client.clone(),
      renew_body(&lease.root, token, lease.release_stage()),
    )
  };
  let device = request["deviceName"].as_str().unwrap_or("").to_string();
  log::info!(
    "Sync lease release requested profile={profile_id} by={}",
    short(request["deviceId"].as_str().unwrap_or(""))
  );
  let _ = crate::events::emit(
    "profile-sync-release-requested",
    json!({
      "profile_id": profile_id,
      "profile_name": profile_name(profile_id),
      "device": device,
    }),
  );
  let profile_id = profile_id.to_string();
  tauri::async_runtime::spawn(async move {
    let (client, body) = answer;
    if let Err(e) = client.lease("renew", &body).await {
      log::debug!("Sync lease answer not sent profile={profile_id} err=\"{e}\"");
    }
    close_browser(&profile_id, "release_requested").await;
  });
}

/// Closes this device's browser of the profile, through the same stop as
/// the Stop button: Chromium shuts down and writes its cookies, the upload
/// is queued, the lease follows. The stop waits for a launch under way, so a
/// browser that was still starting is closed too.
async fn close_browser(profile_id: &str, reason: &str) {
  let was_running = crate::browser_runner::browser_is_running_for(profile_id);
  if !was_running {
    let idle = crate::browser_runner::try_lock_profile_launch(profile_id).await;
    if idle.is_some() {
      // No browser and no launch: what is left is an upload, or nothing.
      drop(idle);
      browser_stopped(profile_id).await;
      return;
    }
  }
  let (Some(app), Some(profile)) = (APP.get(), find_profile(profile_id)) else {
    return;
  };
  let started = Instant::now();
  match crate::browser_runner::kill_browser_profile(app.clone(), profile).await {
    Ok(()) => log::info!(
      "Browser closed for another device profile={profile_id} reason={reason} was_running={was_running} elapsed_ms={}",
      started.elapsed().as_millis()
    ),
    Err(e) => log::warn!(
      "Browser close for another device failed profile={profile_id} reason={reason} err=\"{e}\""
    ),
  }
}

/// Another device took the lease over. This session's changes must not
/// replace that device's: the next sync of the profile takes the remote copy.
fn lost(profile_id: &str, token: &str, reply: &Value) {
  {
    let mut leases = held();
    if leases.get(profile_id).is_none_or(|l| l.token != token) {
      return;
    }
    leases.remove(profile_id);
  }
  notify_lost(profile_id, reply);
}

fn notify_lost(profile_id: &str, reply: &Value) {
  note_taken_over(profile_id);
  log::warn!(
    "Sync lease lost profile={profile_id} holder={}",
    short(reply["holder"]["deviceId"].as_str().unwrap_or(""))
  );
  let _ = crate::events::emit(
    "profile-sync-lease-lost",
    json!({
      "profile_id": profile_id,
      "profile_name": profile_name(profile_id),
      "device": reply["holder"]["deviceName"].as_str().unwrap_or(""),
    }),
  );
  // The other device runs this session now; one that went on here too would
  // make the site revoke it on both.
  let profile_id = profile_id.to_string();
  tauri::async_runtime::spawn(async move {
    close_browser(&profile_id, "lease_lost").await;
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_refusal_names_the_device_and_whether_it_can_be_taken_over() {
    let err = held_error(
      "p1",
      &json!({
        "code": "PROFILE_LEASE_HELD",
        "holder": { "deviceId": "d", "deviceName": "Work Mac" },
        "expiresAt": "2026-11-03T10:00:00.000Z",
        "sameAccount": true,
      }),
    );
    let parsed: Value = serde_json::from_str(&err).unwrap();
    assert_eq!(parsed["code"], "PROFILE_LEASE_HELD");
    assert_eq!(parsed["params"]["device"], "Work Mac");
    assert_eq!(parsed["params"]["canTakeOver"], "true");
    assert_eq!(parsed["params"]["expiresAt"], "2026-11-03T10:00:00.000Z");
  }

  #[test]
  fn a_holder_reports_closing_then_uploading_and_nothing_unasked() {
    assert_eq!(release_stage(false, false), None);
    assert_eq!(release_stage(false, true), None);
    assert_eq!(release_stage(true, false), Some("closing"));
    assert_eq!(release_stage(true, true), Some("uploading"));
    let body = renew_body("profiles/p1/", "t", Some("closing"));
    assert_eq!(body["releaseStage"], "closing");
    assert_eq!(body["ttlSeconds"], TTL_SECS);
    assert!(renew_body("profiles/p1/", "t", None)
      .get("releaseStage")
      .is_none());
  }

  #[test]
  fn an_unanswered_request_names_the_device() {
    let parsed: Value = serde_json::from_str(&no_answer("p1", "Work Mac")).unwrap();
    assert_eq!(parsed["code"], "PROFILE_RELEASE_NO_ANSWER");
    assert_eq!(parsed["params"]["device"], "Work Mac");
    assert_eq!(parsed["params"]["profileId"], "p1");
  }

  #[test]
  fn a_holder_hears_a_request_well_before_the_waiting_device_gives_up() {
    assert!(HEARTBEAT * 2 < RELEASE_ANSWER_LIMIT);
    assert!(HEARTBEAT * 3 < Duration::from_secs(TTL_SECS));
  }

  #[test]
  fn device_ids_are_shortened_in_the_log() {
    assert_eq!(short("abc").len(), 8);
    assert_ne!(short("abc"), short("abe"));
  }
}
