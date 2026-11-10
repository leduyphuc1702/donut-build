//! Sync is on by default. Once this device has somewhere to sync, every
//! profile it can sync is switched on, unless the person switched that profile
//! off here. The switch-off is remembered per device in the profile's
//! `.donut-sync` folder, which sync never uploads.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::engine::{apply_profile_sync_mode, syncs_browser_files, SyncEngine};
use crate::log_streak::KeyedStreak;
use crate::profile::types::{BrowserProfile, SyncMode};
use crate::profile::ProfileManager;

/// How often the scheduler looks for profiles to switch on.
pub(super) const SWEEP_EVERY: Duration = Duration::from_secs(30);

/// A profile younger than this may still be in the middle of being set up, and
/// switching it on then could overwrite a save the setup makes after it.
const SETTLE_SECS: u64 = 15;

static SWEEPING: AtomicBool = AtomicBool::new(false);
static SWITCH_ON: KeyedStreak = KeyedStreak::new(module_path!(), "Default sync switch-on");

fn off_marker(profile_id: &str) -> PathBuf {
  crate::app_dirs::profiles_dir()
    .join(profile_id)
    .join(".donut-sync")
    .join("sync_off")
}

/// Remember whether the person switched this profile's sync off on this
/// device, so the default does not switch it back on.
pub(super) fn note_switched_off(profile_id: &str, off: bool) {
  let marker = off_marker(profile_id);
  if off {
    if let Some(parent) = marker.parent() {
      let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&marker, b"");
  } else {
    let _ = std::fs::remove_file(&marker);
  }
}

fn switched_off(profile_id: &str) -> bool {
  off_marker(profile_id).exists()
}

/// The profiles the default switches on now.
fn candidates(
  profiles: &[BrowserProfile],
  now_secs: u64,
  switched_off: impl Fn(&str) -> bool,
) -> Vec<String> {
  profiles
    .iter()
    .filter(|p| p.sync_mode == SyncMode::Disabled)
    .filter(|p| !p.ephemeral && syncs_browser_files(p) && p.process_id.is_none())
    .filter(|p| {
      p.created_at
        .is_none_or(|created| now_secs.saturating_sub(created) >= SETTLE_SECS)
    })
    .map(|p| p.id.to_string())
    .filter(|id| !switched_off(id))
    .collect()
}

/// Switch on, in the background, every profile the default covers. A sweep
/// still running makes this a no-op.
pub(super) fn spawn_sweep(app_handle: tauri::AppHandle) {
  if crate::team_roles::is_read_only() || SWEEPING.swap(true, Ordering::SeqCst) {
    return;
  }
  tauri::async_runtime::spawn(async move {
    sweep(&app_handle).await;
    SWEEPING.store(false, Ordering::SeqCst);
  });
}

async fn sweep(app_handle: &tauri::AppHandle) {
  let profiles = match ProfileManager::instance().list_profiles() {
    Ok(profiles) => profiles,
    Err(e) => {
      log::debug!("Default sync sweep skipped: profile list failed err=\"{e}\"");
      return;
    }
  };
  let ids = candidates(&profiles, crate::proxy_manager::now_secs(), switched_off);
  if ids.is_empty() {
    return;
  }
  if let Err(e) = SyncEngine::create_from_settings(app_handle).await {
    log::debug!("Default sync sweep skipped: no sync target err=\"{e}\"");
    return;
  }

  let mut switched = 0;
  for id in ids {
    match apply_profile_sync_mode(app_handle.clone(), id.clone(), SyncMode::Regular, false).await {
      Ok(()) => {
        SWITCH_ON.succeeded(&format!("profile={id}"));
        switched += 1;
      }
      Err(e) => SWITCH_ON.failed(&format!("profile={id}"), e),
    }
  }
  if switched > 0 {
    log::info!("Sync switched on by default profiles={switched}");
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn profile(sync_mode: SyncMode) -> BrowserProfile {
    BrowserProfile {
      id: uuid::Uuid::new_v4(),
      browser: "wayfern".into(),
      sync_mode,
      created_at: Some(1_000),
      ..BrowserProfile::default()
    }
  }

  #[test]
  fn switches_on_every_profile_that_can_sync() {
    let off = profile(SyncMode::Disabled);
    let on = profile(SyncMode::Regular);
    let encrypted = profile(SyncMode::Encrypted);
    let legacy = BrowserProfile {
      created_at: None,
      ..profile(SyncMode::Disabled)
    };
    let picked = candidates(&[off.clone(), on, encrypted, legacy.clone()], 2_000, |_| {
      false
    });
    assert_eq!(picked, vec![off.id.to_string(), legacy.id.to_string()]);
  }

  #[test]
  fn leaves_what_cannot_or_should_not_sync() {
    let ephemeral = BrowserProfile {
      ephemeral: true,
      ..profile(SyncMode::Disabled)
    };
    let running = BrowserProfile {
      process_id: Some(42),
      ..profile(SyncMode::Disabled)
    };
    let fresh = BrowserProfile {
      created_at: Some(2_000 - SETTLE_SECS + 1),
      ..profile(SyncMode::Disabled)
    };
    let chosen_off = profile(SyncMode::Disabled);
    let chosen_off_id = chosen_off.id.to_string();
    let picked = candidates(&[ephemeral, running, fresh, chosen_off], 2_000, |id| {
      id == chosen_off_id
    });
    assert!(picked.is_empty(), "{picked:?}");
  }
}
