//! This computer's share of the team activity log: what a member changed
//! here (profiles, proxies, groups, VPNs, extensions, templates, statuses).
//! The cloud records sign-ins, membership changes and profile launches itself.
//!
//! Only a team seat whose plan includes team collaboration reports. Events
//! wait in memory, never on disk, and leave in batches. A batch that fails is
//! tried again a few times, then dropped: the log is a record, not a sync.
//! Names leave this computer only when the team's data is not end-to-end
//! encrypted, because the cloud then holds those names already; otherwise the
//! log carries ids, and a viewer shows its own local names for them.
//!
//! A self-hosted control plane also logs which computer a report came from, so
//! each batch carries this install's `device` (id and host name). Donut cloud
//! is sent the batch it has always taken.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cloud_auth::CloudUser;
use crate::cloud_errors::FailureCodes;
use crate::log_streak::Streak;

/// The cloud's limits: events per report, and the longest id and name.
const BATCH: usize = 50;
const MAX_ID_CHARS: usize = 128;
const MAX_NAME_CHARS: usize = 200;
/// The longest computer name the control plane keeps.
const MAX_DEVICE_NAME_CHARS: usize = 64;
/// Browser ends remembered, so one end is reported once.
const MAX_REMEMBERED_STOPS: usize = 64;
/// Events kept while the cloud cannot be reached. Older ones drop first.
const MAX_QUEUED: usize = 500;
const ATTEMPTS: u8 = 3;
const SETTLE: Duration = Duration::from_secs(5);
const RETRY_AFTER: Duration = Duration::from_secs(60);

const ACTIVITY_CODES: FailureCodes = FailureCodes {
  bad_request: "TEAM_ACTIVITY_INVALID",
  forbidden: "TEAM_ACTIVITY_FORBIDDEN",
  not_found: "TEAM_ACTIVITY_FORBIDDEN",
  conflict: "TEAM_ACTIVITY_INVALID",
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static FLUSHING: AtomicBool = AtomicBool::new(false);
static QUEUE: Mutex<VecDeque<Value>> = Mutex::new(VecDeque::new());
/// `(profile id, browser pid)` of the ends already reported.
static STOPS: Mutex<VecDeque<(String, u32)>> = Mutex::new(VecDeque::new());
static REPORT: Streak = Streak::new(module_path!(), "Team activity report");

/// Record the signed-in account, or `None` once signed out.
pub fn observe(user: Option<&CloudUser>) {
  let enabled =
    user.is_some_and(|user| user.team_id.is_some() && user.entitlements().team_collaboration);
  if !enabled {
    queue().clear();
  }
  ENABLED.store(enabled, Ordering::Relaxed);
}

fn queue() -> std::sync::MutexGuard<'static, VecDeque<Value>> {
  QUEUE
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn clip(value: &str, max: usize) -> String {
  value.chars().take(max).collect()
}

/// One event, as the cloud takes it. `action` is `<target type>.<verb>`.
fn event(action: &str, id: &str, name: Option<&str>, detail: Option<Value>, names: bool) -> Value {
  let target_type = action.split('.').next().unwrap_or_default();
  let mut event = json!({
    "action": action,
    "targetType": target_type,
    "targetId": clip(id, MAX_ID_CHARS),
  });
  if let Some(name) = name.filter(|name| names && !name.trim().is_empty()) {
    event["targetName"] = json!(clip(name.trim(), MAX_NAME_CHARS));
  }
  if let Some(detail) = detail.filter(|detail| detail.as_object().is_some_and(|o| !o.is_empty())) {
    event["detail"] = detail;
  }
  event
}

/// Note that the person changed `id`. Free when this seat does not report.
pub fn record(action: &str, id: &str, name: Option<&str>, detail: Option<Value>) {
  if !ENABLED.load(Ordering::Relaxed) {
    return;
  }
  let names = !crate::sync::encryption::has_e2e_password();
  {
    let mut queue = queue();
    if queue.len() >= MAX_QUEUED {
      queue.pop_front();
    }
    queue.push_back(event(action, id, name, detail, names));
  }
  if !FLUSHING.swap(true, Ordering::AcqRel) {
    tauri::async_runtime::spawn(flush());
  }
}

/// The fields an edit changed, as the detail the log shows.
pub fn fields(fields: &[&str]) -> Option<Value> {
  (!fields.is_empty()).then(|| json!({ "fields": fields }))
}

/// A browser opened for `profile_id` on this computer, by the person or by
/// automation (the REST API, MCP). No page address is ever logged.
pub fn launched(profile_id: &str, name: &str, automation: bool) {
  if !ENABLED.load(Ordering::Relaxed) {
    return;
  }
  // A new browser may reuse the pid of one that ended before it.
  remembered_stops().retain(|(id, _)| id != profile_id);
  record(
    "profile.launch",
    profile_id,
    Some(name),
    automation.then(|| json!({ "automation": true })),
  );
}

/// The browser of `profile_id` ended on this computer. The two places that see
/// an end, a Stop and the poll that notices a closed window, can both see the
/// same one; `pid` tells the repeat apart, and the first report wins.
pub fn stopped(profile_id: &str, name: &str, pid: Option<u32>) {
  if !ENABLED.load(Ordering::Relaxed) {
    return;
  }
  if !first_end(&mut remembered_stops(), profile_id, pid) {
    return;
  }
  record("profile.stop", profile_id, Some(name), None);
}

/// Whether this end is new, noting it. Without a pid there is nothing to tell
/// a repeat by, so the end counts.
fn first_end(stops: &mut VecDeque<(String, u32)>, profile_id: &str, pid: Option<u32>) -> bool {
  let Some(pid) = pid else {
    return true;
  };
  if stops.iter().any(|(id, p)| id == profile_id && *p == pid) {
    return false;
  }
  if stops.len() >= MAX_REMEMBERED_STOPS {
    stops.pop_front();
  }
  stops.push_back((profile_id.to_string(), pid));
  true
}

fn remembered_stops() -> std::sync::MutexGuard<'static, VecDeque<(String, u32)>> {
  STOPS
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// This install, as a control plane names the computer a report came from.
fn device() -> Value {
  json!({
    "id": crate::sync::lease::device_id(),
    "name": clip(&crate::sync::lease::device_name(), MAX_DEVICE_NAME_CHARS),
  })
}

/// The body of one report. `device` is `None` for Donut cloud.
fn report_body(batch: &[Value], device: Option<Value>) -> Value {
  let mut body = json!({ "events": batch });
  if let Some(device) = device {
    body["device"] = device;
  }
  body
}

async fn flush() {
  tokio::time::sleep(SETTLE).await;
  let mut attempts = 0u8;
  loop {
    let batch: Vec<Value> = {
      let mut queue = queue();
      let take = queue.len().min(BATCH);
      queue.drain(..take).collect()
    };
    if batch.is_empty() {
      FLUSHING.store(false, Ordering::Release);
      // An event recorded between the drain and the flag store would wait for
      // the next one; take it now instead.
      if queue().is_empty() || FLUSHING.swap(true, Ordering::AcqRel) {
        return;
      }
      continue;
    }
    let count = batch.len();
    let url = format!("{}/api/teams/activity", crate::cloud_auth::CLOUD_API_URL);
    let body = report_body(
      &batch,
      crate::cloud_auth::is_self_hosted_account().then(device),
    );
    let sent = crate::cloud_api::call::<Option<Value>>(
      reqwest::Method::POST,
      url,
      Some(body),
      ACTIVITY_CODES,
    )
    .await;
    match sent {
      Ok(_) => {
        REPORT.succeeded();
        attempts = 0;
        log::debug!("Team activity reported events={count}");
      }
      Err(err) if err.status == 400 || err.status == 403 => {
        // Not entitled any more, or events this build cannot shape: sending
        // them again changes nothing.
        log::warn!(
          "Team activity dropped events={count} code={} status={}",
          err.code,
          err.status
        );
      }
      Err(err) => {
        REPORT.failed(format!("code={} status={}", err.code, err.status));
        attempts += 1;
        if attempts >= ATTEMPTS {
          log::warn!("Team activity dropped events={count} attempts={attempts}");
          attempts = 0;
        } else {
          let mut queue = queue();
          for event in batch.into_iter().rev() {
            queue.push_front(event);
          }
          queue.truncate(MAX_QUEUED);
        }
        tokio::time::sleep(RETRY_AFTER).await;
      }
    }
  }
}

/// What part of the team's log to read. Every field is optional.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityFilter {
  /// An action (`profile.delete`) or every action of a kind (`profile.*`).
  pub action: Option<String>,
  pub actor_user_id: Option<String>,
  pub target_type: Option<String>,
  /// The oldest time to include, ISO 8601.
  pub since: Option<String>,
}

const TARGET_TYPES: &[&str] = &[
  "profile",
  "proxy",
  "group",
  "vpn",
  "extension",
  "template",
  "status",
  "member",
  "invitation",
];

impl ActivityFilter {
  /// The query pairs this filter adds, or `TEAM_ACTIVITY_INVALID`.
  fn query(&self) -> Result<Vec<String>, String> {
    let invalid = || crate::backend_error("TEAM_ACTIVITY_INVALID");
    let present = |value: &Option<String>| value.clone().filter(|v| !v.is_empty());
    let mut query = Vec::new();
    if let Some(action) = present(&self.action) {
      let (kind, verb) = action.split_once('.').ok_or_else(invalid)?;
      let word = |w: &str| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
      if action.len() > 64 || !word(kind) || !(verb == "*" || word(verb)) {
        return Err(invalid());
      }
      query.push(format!("action={}", urlencoding::encode(&action)));
    }
    if let Some(actor) = present(&self.actor_user_id) {
      if actor.len() > 128
        || !actor
          .bytes()
          .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
      {
        return Err(invalid());
      }
      query.push(format!("actorUserId={actor}"));
    }
    if let Some(target_type) = present(&self.target_type) {
      if !TARGET_TYPES.contains(&target_type.as_str()) {
        return Err(invalid());
      }
      query.push(format!("targetType={target_type}"));
    }
    if let Some(since) = present(&self.since) {
      let at = chrono::DateTime::parse_from_rfc3339(&since).map_err(|_| invalid())?;
      query.push(format!(
        "since={}",
        urlencoding::encode(
          &at
            .to_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        )
      ));
    }
    Ok(query)
  }
}

async fn signed_in_team() -> Result<String, String> {
  crate::cloud_auth::CLOUD_AUTH
    .get_user()
    .await
    .and_then(|auth| auth.user.team_id)
    .ok_or_else(|| crate::backend_error("TEAM_ACTIVITY_FORBIDDEN"))
}

fn activity_failure(err: crate::cloud_errors::BackendFailure) -> String {
  log::warn!(
    "Team activity request failed code={} status={}",
    err.code,
    err.status
  );
  err.to_error_json()
}

/// A page of the team's log, newest first. Owners and admins only; the cloud
/// checks the role and the plan.
#[tauri::command]
pub async fn list_team_activity(
  cursor: Option<String>,
  limit: Option<u32>,
  filter: Option<ActivityFilter>,
) -> Result<Value, String> {
  let team_id = signed_in_team().await?;
  let url = activity_url(
    &team_id,
    "activity",
    cursor.as_deref(),
    limit,
    &filter.unwrap_or_default(),
  )?;
  crate::cloud_api::call::<Value>(reqwest::Method::GET, url, None, ACTIVITY_CODES)
    .await
    .map_err(activity_failure)
}

/// The filtered log as a CSV file at `path`, which the person chose. Returns
/// how many rows it holds.
#[tauri::command]
pub async fn export_team_activity_csv(
  path: String,
  filter: Option<ActivityFilter>,
) -> Result<u64, String> {
  let team_id = signed_in_team().await?;
  let url = activity_url(
    &team_id,
    "activity.csv",
    None,
    None,
    &filter.unwrap_or_default(),
  )?;
  let csv = crate::cloud_api::call_text(reqwest::Method::GET, url, ACTIVITY_CODES)
    .await
    .map_err(activity_failure)?;
  std::fs::write(&path, csv.as_bytes())
    .map_err(|e| crate::backend_error_with_detail("TEAM_ACTIVITY_EXPORT_FAILED", e))?;
  let rows = csv.lines().count().saturating_sub(1) as u64;
  log::info!("Team activity exported rows={rows}");
  Ok(rows)
}

fn activity_url(
  team_id: &str,
  resource: &str,
  cursor: Option<&str>,
  limit: Option<u32>,
  filter: &ActivityFilter,
) -> Result<String, String> {
  let mut query = Vec::new();
  if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
    if cursor.len() > 15 || !cursor.bytes().all(|b| b.is_ascii_digit()) {
      return Err(crate::backend_error("TEAM_ACTIVITY_INVALID"));
    }
    query.push(format!("cursor={cursor}"));
  }
  if let Some(limit) = limit {
    query.push(format!("limit={}", limit.clamp(1, 200)));
  }
  query.extend(filter.query()?);
  let mut url = format!(
    "{}/api/teams/{}/{resource}",
    crate::cloud_auth::CLOUD_API_URL,
    urlencoding::encode(team_id)
  );
  if !query.is_empty() {
    url.push('?');
    url.push_str(&query.join("&"));
  }
  Ok(url)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_event_names_its_target_only_when_allowed() {
    let named = event(
      "profile.rename",
      "p1",
      Some("  Shop  "),
      fields(&["name"]),
      true,
    );
    assert_eq!(named["targetType"], "profile");
    assert_eq!(named["targetName"], "Shop");
    assert_eq!(named["detail"], json!({ "fields": ["name"] }));

    let sealed = event("profile.rename", "p1", Some("Shop"), None, false);
    assert!(sealed.get("targetName").is_none());
    assert!(sealed.get("detail").is_none());
  }

  #[test]
  fn an_event_fits_the_cloud_limits() {
    let long = "x".repeat(300);
    let clipped = event("proxy.update", &long, Some(&long), None, true);
    assert_eq!(clipped["targetId"].as_str().unwrap().len(), MAX_ID_CHARS);
    assert_eq!(
      clipped["targetName"].as_str().unwrap().len(),
      MAX_NAME_CHARS
    );
  }

  #[test]
  fn nothing_queues_for_a_seat_that_does_not_report() {
    observe(None);
    record("profile.delete", "p1", None, None);
    launched("p1", "Shop", false);
    stopped("p1", "Shop", Some(10));
    assert!(queue().is_empty());
  }

  #[test]
  fn a_launch_and_a_stop_are_profile_events_without_a_page() {
    let launch = event(
      "profile.launch",
      "p1",
      Some("Shop"),
      Some(json!({ "automation": true })),
      true,
    );
    assert_eq!(launch["targetType"], "profile");
    assert_eq!(launch["targetName"], "Shop");
    assert_eq!(launch["detail"], json!({ "automation": true }));
    let stop = event("profile.stop", "p1", Some("Shop"), None, true);
    assert_eq!(stop["action"], "profile.stop");
    assert!(stop.get("detail").is_none());
    assert!(!launch.to_string().contains("http"));
  }

  #[test]
  fn one_end_of_a_browser_is_reported_once() {
    let mut stops = VecDeque::new();
    assert!(first_end(&mut stops, "p1", Some(100)));
    assert!(!first_end(&mut stops, "p1", Some(100)), "the repeat");
    assert!(first_end(&mut stops, "p1", Some(101)), "another browser");
    assert!(first_end(&mut stops, "p2", Some(100)), "another profile");
    assert!(
      first_end(&mut stops, "p1", None),
      "no pid, nothing to compare"
    );
    assert!(first_end(&mut stops, "p1", None));
    for pid in 0..(MAX_REMEMBERED_STOPS as u32 * 2) {
      first_end(&mut stops, "p3", Some(pid));
    }
    assert!(stops.len() <= MAX_REMEMBERED_STOPS);
  }

  #[test]
  fn only_a_self_hosted_report_names_the_computer() {
    let batch = vec![event("proxy.update", "x1", None, None, true)];
    let cloud = report_body(&batch, None);
    assert!(cloud.get("device").is_none());
    assert_eq!(cloud["events"].as_array().unwrap().len(), 1);

    let device = json!({ "id": "abc-123", "name": "Office PC" });
    let own = report_body(&batch, Some(device.clone()));
    assert_eq!(own["device"], device);
    assert_eq!(own["events"], cloud["events"]);
  }

  #[test]
  fn the_activity_url_keeps_the_team_id_one_segment() {
    let filter = ActivityFilter {
      action: Some("profile.*".into()),
      actor_user_id: Some("u-1".into()),
      target_type: Some("proxy".into()),
      since: Some("2026-10-01T12:00:00+02:00".into()),
    };
    let url = activity_url("a/b", "activity", Some("42"), Some(500), &filter).unwrap();
    assert!(url.contains("/api/teams/a%2Fb/activity?"), "{url}");
    assert!(url.contains("cursor=42"));
    assert!(url.contains("limit=200"));
    assert!(url.contains("action=profile.%2A"), "{url}");
    assert!(url.contains("actorUserId=u-1"));
    assert!(url.contains("targetType=proxy"));
    assert!(url.contains("since=2026-10-01T10%3A00%3A00Z"), "{url}");
    let plain = ActivityFilter::default();
    assert!(activity_url("t1", "activity.csv", None, None, &plain)
      .unwrap()
      .ends_with("/api/teams/t1/activity.csv"));
    assert!(activity_url("t1", "activity", Some("1;drop"), None, &plain).is_err());
  }

  #[test]
  fn a_filter_the_cloud_would_refuse_is_refused_here() {
    for filter in [
      ActivityFilter {
        action: Some("Profile.delete".into()),
        ..Default::default()
      },
      ActivityFilter {
        action: Some("profile".into()),
        ..Default::default()
      },
      ActivityFilter {
        actor_user_id: Some("a&b=c".into()),
        ..Default::default()
      },
      ActivityFilter {
        target_type: Some("planet".into()),
        ..Default::default()
      },
      ActivityFilter {
        since: Some("yesterday".into()),
        ..Default::default()
      },
    ] {
      assert!(filter.query().is_err(), "{filter:?}");
    }
  }
}
