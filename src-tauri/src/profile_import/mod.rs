//! Turning someone else's browser profile into one Wayfern will actually load.
//!
//! The old importer copied a source profile directory verbatim onto the new
//! profile's `--user-data-dir`. Chromium reads `<user-data-dir>/Default/`, so
//! every imported file sat one level above where the browser looked and the
//! profile came up empty — and even in the right place the secrets would not
//! have opened, because they are sealed with a key held in the source
//! machine's Keychain / DPAPI / secret service that Wayfern never consults.
//!
//! This module does the whole job: classify the source, recover its key, copy
//! with consistent database snapshots, put the files where Chromium reads them,
//! re-seal every secret with Wayfern's portable key, and report exactly what
//! came across.

pub mod copy;
pub mod keyring;
pub mod layout;
pub mod os_crypt;
pub mod report;
pub mod rewrite;

use layout::RejectReason;
use report::{warning, ProfileImportReport};
use std::path::Path;

/// The profile subdirectory Chromium reads when no `--profile-directory` is
/// passed (`chrome_constants.cc` `kInitialProfile`). Donut never passes one.
pub const INITIAL_PROFILE_DIR: &str = "Default";

/// Import `source` into `dest_user_data_dir`, which becomes the new profile's
/// `--user-data-dir`.
///
/// Never fails because part of the data could not be carried: partial results
/// plus an honest report beat an all-or-nothing import that leaves the user
/// with nothing and no explanation. It fails only when the source is not
/// importable at all, or when the target key cannot be established — without
/// that key, anything written would be unreadable forever.
pub fn import_into(
  source: &Path,
  dest_user_data_dir: &Path,
  source_family: &str,
  allow_running: bool,
) -> Result<ProfileImportReport, String> {
  let shape = layout::classify(source).map_err(|reason| match reason {
    RejectReason::Firefox => serde_json::json!({
      "code": "IMPORT_SOURCE_NOT_CHROMIUM",
      "params": { "family": "Firefox" }
    })
    .to_string(),
    RejectReason::NotChromium => serde_json::json!({
      "code": "IMPORT_SOURCE_NOT_CHROMIUM",
      "params": { "family": "" }
    })
    .to_string(),
  })?;

  let mut report = ProfileImportReport::default();

  if let Some(running) = running_source_browser(&shape) {
    if !allow_running {
      return Err(
        serde_json::json!({
          "code": "IMPORT_SOURCE_BROWSER_RUNNING",
          "params": { "browser": running }
        })
        .to_string(),
      );
    }
    // Databases are snapshotted transactionally, but LevelDB site data is
    // copied as files and can be mid-write.
    report.warn(warning::SOURCE_BROWSER_RUNNING);
  }

  // Mint the target key first. Everything after this point is written to be
  // readable with it, and a profile whose key could not be persisted would
  // lose every secret the first time the browser exits.
  let target = os_crypt::TargetKey::ensure(dest_user_data_dir)?;

  // Recover the source key before the copy: on macOS this may prompt, and
  // asking before a multi-GB copy respects the user's time.
  let source_keys = keyring::recover_source_keys(
    source_family,
    &shape.profile_dir,
    shape.user_data_dir.as_deref(),
    &mut report,
  );

  let default_dir = dest_user_data_dir.join(INITIAL_PROFILE_DIR);
  let outcome = copy::copy_profile_tree(&shape.profile_dir, &default_dir)?;
  report.bytes_copied = outcome.bytes_copied;
  if !outcome.unreadable_stores.is_empty() {
    report.warn(warning::STORE_UNREADABLE);
  }

  layout::normalize_network_dir(&default_dir)
    .map_err(|e| format!("Failed to place network data: {e}"))?;

  rewrite::finalize_profile(&default_dir, &source_keys, &target, &mut report);

  Ok(report)
}

/// Names, inside a profile, the scheme its stored secrets are sealed under: the
/// OS whose engine key it is (`std::env::consts::OS`). A profile that moves to
/// another OS (a sync) is re-sealed for the new host when it next opens. A
/// profile without one was sealed by Wayfern, or never opened.
pub const ENGINE_KEY_MARKER: &str = ".donut-engine-key";

/// Seal a profile's stored secrets for the key Apostate reads on this host.
///
/// Wayfern sealed cookies, saved logins and cards with the profile's own
/// `os_crypt_key` file, so a profile could move between machines and systems.
/// Apostate never reads that file: it opens secrets with the key of its host
/// ([`os_crypt::engine_key_material`]). A profile moved over without this step
/// comes up logged out of every site on its first launch and silently loses its
/// saved logins, and so does one synced from another OS.
///
/// Only rows the old scheme's key opens are rewritten. A row already sealed for
/// this host stays, and a row nothing here opens stays too (it is unreadable to
/// the engine either way, and leaving it keeps it recoverable).
///
/// Returns what was carried, or `None` when there is nothing to do: the profile
/// is already sealed for this host, or the old keys are not known here.
pub fn adopt_engine_key(user_data_dir: &Path) -> Result<Option<ProfileImportReport>, String> {
  #[cfg(target_os = "windows")]
  if let Err(error) = keyring::write_key_file_from_local_state(user_data_dir) {
    log::warn!("Portable copy of the engine key not written err=\"{error}\"");
  }

  let marker = user_data_dir.join(ENGINE_KEY_MARKER);
  let host = std::env::consts::OS;
  let sealed_under = std::fs::read_to_string(&marker)
    .ok()
    .map(|text| text.trim().to_string());
  let key_file = std::fs::read(user_data_dir.join(os_crypt::KEY_FILE_NAME))
    .ok()
    .filter(|bytes| bytes.iter().any(|byte| *byte != 0));

  let scheme = match (sealed_under.as_deref(), &key_file) {
    (Some(scheme), _) if scheme == host => return Ok(None),
    (Some(scheme), _) => scheme.to_string(),
    (None, Some(_)) => os_crypt::WAYFERN_SCHEME.to_string(),
    (None, None) => {
      // Nothing from an earlier scheme: the engine on this host seals it from here.
      std::fs::write(&marker, format!("{host}\n"))
        .map_err(|e| format!("Failed to write {ENGINE_KEY_MARKER}: {e}"))?;
      return Ok(None);
    }
  };
  let Some(source) = os_crypt::SourceKeyring::sealed_under(&scheme, key_file.as_deref()) else {
    return Ok(None);
  };

  let target = os_crypt::TargetKey::adopting(user_data_dir, key_file.as_deref().unwrap_or(&[]))?;
  let mut report = ProfileImportReport::default();
  rewrite::reseal_in_place(
    &user_data_dir.join(INITIAL_PROFILE_DIR),
    &crate::cookie_manager::CookieManager::wayfern_cookie_path(user_data_dir),
    &source,
    &target,
    &mut report,
  );

  std::fs::write(&marker, format!("{host}\n"))
    .map_err(|e| format!("Failed to write {ENGINE_KEY_MARKER}: {e}"))?;
  Ok(Some(report))
}

/// Is the browser that owns this profile currently running?
///
/// Matched on the profile path in the process command line rather than on the
/// executable name: the user may well have Chrome open on a *different*
/// profile, which is no reason to block the import.
fn running_source_browser(shape: &layout::SourceShape) -> Option<String> {
  use sysinfo::{ProcessRefreshKind, RefreshKind};

  let system = crate::proxy_storage::process_table(
    RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
  );

  let needle = shape
    .user_data_dir
    .as_deref()
    .unwrap_or(&shape.profile_dir)
    .to_string_lossy()
    .to_string();
  if needle.is_empty() {
    return None;
  }

  for process in system.processes().values() {
    let name = process.name().to_string_lossy().to_lowercase();
    let looks_like_a_browser = name.contains("chrome")
      || name.contains("chromium")
      || name.contains("brave")
      || name.contains("edge")
      || name.contains("vivaldi")
      || name.contains("opera")
      || name.contains("arc")
      || name.contains("yandex");
    if !looks_like_a_browser {
      continue;
    }
    // Donut's own browser is Wayfern; never report it as the source.
    if name.contains("wayfern") {
      continue;
    }
    if process
      .cmd()
      .iter()
      .any(|arg| arg.to_string_lossy().contains(&needle))
    {
      return Some(process.name().to_string_lossy().to_string());
    }
  }

  None
}

/// Move a profile that an earlier build imported into the broken root layout
/// down into `Default/`, where the browser reads it.
///
/// Without this, everything those users imported stays stranded: their real
/// data sits at `profile/Cookies` while Wayfern reads and writes
/// `profile/Default/Cookies`. Their secrets remain unreadable — the source key
/// was never captured and cannot be recovered after the fact — but history,
/// bookmarks, extensions and site data become visible again.
///
/// Returns `Ok(true)` when a repair was performed.
pub fn repair_legacy_layout(user_data_dir: &Path) -> Result<bool, String> {
  let default_dir = user_data_dir.join(INITIAL_PROFILE_DIR);

  // The broken shape is exactly: profile markers at the root, and no `Default/`
  // for the browser to have used instead.
  let has_root_profile = user_data_dir.join("Preferences").exists()
    || user_data_dir.join("History").exists()
    || user_data_dir.join("Cookies").exists();
  if !has_root_profile || default_dir.exists() {
    return Ok(false);
  }

  // Root-level files that belong to the user-data dir, not to the profile.
  const ROOT_LEVEL: &[&str] = &[
    "Local State",
    "os_crypt_key",
    "First Run",
    "Last Version",
    "Variations",
    "ChromeFeatureState",
    "RunningChromeVersion",
    "SingletonLock",
    "SingletonCookie",
    "SingletonSocket",
    "user.js",
    "metadata.json",
    ".donut-sync",
  ];

  let staging = user_data_dir.join(".donut-import-repair");
  if staging.exists() {
    std::fs::remove_dir_all(&staging).map_err(|e| format!("Failed to clear staging: {e}"))?;
  }
  std::fs::create_dir_all(&staging).map_err(|e| format!("Failed to create staging: {e}"))?;

  let entries =
    std::fs::read_dir(user_data_dir).map_err(|e| format!("Failed to read profile: {e}"))?;
  for entry in entries.flatten() {
    let name = entry.file_name();
    let Some(name_str) = name.to_str() else {
      continue;
    };
    if ROOT_LEVEL.contains(&name_str) || name_str == ".donut-import-repair" {
      continue;
    }
    std::fs::rename(entry.path(), staging.join(name_str))
      .map_err(|e| format!("Failed to relocate {name_str}: {e}"))?;
  }

  std::fs::rename(&staging, &default_dir)
    .map_err(|e| format!("Failed to install {INITIAL_PROFILE_DIR}: {e}"))?;

  // Now that the files are in the right place, put the network data where this
  // platform reads it too.
  let _ = layout::normalize_network_dir(&default_dir);
  // And make sure the profile has a key, so the browser does not mint one
  // mid-session and lose whatever it writes.
  let _ = os_crypt::TargetKey::ensure(user_data_dir);

  log::info!(
    "Legacy import layout repaired: content moved into {INITIAL_PROFILE_DIR}/ path=\"{}\"",
    user_data_dir.display()
  );
  Ok(true)
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  fn touch(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
  }

  #[test]
  fn import_places_everything_under_default() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("Chrome").join("Default");
    let dest = dir.path().join("profile");
    touch(&source.join("Preferences"), b"{}");
    touch(&source.join("Bookmarks"), b"{\"roots\":{}}");

    let report = import_into(&source, &dest, "chromium", true).expect("import");

    assert!(
      dest.join("Default").join("Preferences").exists(),
      "Chromium reads Default/, not the user-data-dir root"
    );
    assert!(
      !dest.join("Preferences").exists(),
      "nothing profile-scoped belongs at the root"
    );
    assert!(
      os_crypt::TargetKey::engine(&dest).is_some(),
      "the engine key is ready for the first launch"
    );
    assert!(report.bytes_copied > 0);
  }

  #[test]
  fn import_rejects_a_firefox_profile_by_name() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("xyz.default-release");
    let dest = dir.path().join("profile");
    touch(&source.join("prefs.js"), b"");
    touch(&source.join("places.sqlite"), b"");

    let err = import_into(&source, &dest, "firefox", true).expect_err("must reject");
    assert!(err.contains("IMPORT_SOURCE_NOT_CHROMIUM"));
    assert!(
      err.contains("Firefox"),
      "the user needs to be told why, not just that it failed"
    );
  }

  #[test]
  fn import_rejects_an_arbitrary_folder() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("holiday-photos");
    let dest = dir.path().join("profile");
    touch(&source.join("IMG_0001.jpg"), b"not a profile");

    let err = import_into(&source, &dest, "chromium", true).expect_err("must reject");
    assert!(err.contains("IMPORT_SOURCE_NOT_CHROMIUM"));
  }

  #[test]
  fn import_is_rerunnable_over_the_same_destination() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("Default");
    let dest = dir.path().join("profile");
    touch(&source.join("Preferences"), b"{}");

    import_into(&source, &dest, "chromium", true).expect("first");
    let key = os_crypt::engine_key_material(&dest);
    assert!(key.is_some());
    import_into(&source, &dest, "chromium", true).expect("second");
    assert_eq!(
      os_crypt::engine_key_material(&dest),
      key,
      "re-running must not orphan what the first run encrypted"
    );
  }

  #[test]
  fn legacy_layout_is_repaired_into_default() {
    let dir = TempDir::new().unwrap();
    let profile = dir.path().join("profile");
    // Exactly what the old importer produced.
    touch(&profile.join("Preferences"), b"{}");
    touch(&profile.join("History"), b"");
    touch(
      &profile
        .join("Local Storage")
        .join("leveldb")
        .join("CURRENT"),
      b"",
    );
    touch(&profile.join("Local State"), b"{}");

    assert!(repair_legacy_layout(&profile).unwrap());

    assert!(profile.join("Default").join("Preferences").exists());
    assert!(profile.join("Default").join("History").exists());
    assert!(profile
      .join("Default")
      .join("Local Storage")
      .join("leveldb")
      .join("CURRENT")
      .exists());
    assert!(
      profile.join("Local State").exists(),
      "Local State belongs to the user-data dir, not the profile"
    );
    assert!(os_crypt::TargetKey::engine(&profile).is_some());
    assert!(!profile.join(".donut-import-repair").exists());
  }

  #[cfg(any(target_os = "macos", target_os = "linux"))]
  mod adopt {
    use super::*;
    use sha2::{Digest, Sha256};

    const WAYFERN_PASSWORD: &[u8] = b"bG9jYWwtcGFzc3dvcmQhIQ==";

    fn framed(host: &str, value: &str) -> Vec<u8> {
      let mut plain = Sha256::digest(host.as_bytes()).to_vec();
      plain.extend_from_slice(value.as_bytes());
      plain
    }

    /// What Wayfern wrote: its own `os_crypt_key` file, keyed per tag.
    fn sealed_by_wayfern(plain: &[u8]) -> Vec<u8> {
      let tag: &[u8; 3] = if cfg!(target_os = "linux") {
        b"v11"
      } else {
        b"v10"
      };
      let mut out = tag.to_vec();
      out.extend(
        os_crypt::key_for_tag(WAYFERN_PASSWORD, tag)
          .unwrap()
          .encrypt(plain)
          .unwrap(),
      );
      out
    }

    fn sealed_by_engine(profile: &Path, plain: &[u8]) -> Vec<u8> {
      os_crypt::TargetKey::engine(profile)
        .unwrap()
        .encrypt(plain)
        .unwrap()
    }

    fn cookie_store(profile: &Path, rows: &[(&str, &str, &[u8])]) {
      let path = profile.join("Default").join("Cookies");
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      let conn = rusqlite::Connection::open(&path).unwrap();
      conn
        .execute_batch(
          "CREATE TABLE cookies(
             creation_utc INTEGER NOT NULL,
             host_key TEXT NOT NULL,
             top_frame_site_key TEXT NOT NULL DEFAULT '',
             name TEXT NOT NULL,
             value TEXT NOT NULL DEFAULT '',
             encrypted_value BLOB NOT NULL DEFAULT '',
             path TEXT NOT NULL DEFAULT '/'
           );
           CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
           INSERT INTO meta VALUES('version', '24');
           INSERT INTO meta VALUES('last_compatible_version', '24');",
        )
        .unwrap();
      for (host, name, sealed) in rows {
        conn
          .execute(
            "INSERT INTO cookies(creation_utc, host_key, name, encrypted_value) VALUES(0, ?1, ?2, ?3)",
            rusqlite::params![host, name, sealed],
          )
          .unwrap();
      }
    }

    fn stored(profile: &Path, name: &str) -> Vec<u8> {
      rusqlite::Connection::open(profile.join("Default").join("Cookies"))
        .unwrap()
        .query_row(
          "SELECT encrypted_value FROM cookies WHERE name = ?1",
          [name],
          |row| row.get(0),
        )
        .unwrap()
    }

    fn engine_opens(profile: &Path, name: &str, host: &str, value: &str) -> bool {
      let row = stored(profile, name);
      let engine = os_crypt::TargetKey::engine(profile).unwrap();
      if !engine.opens_cookie(&row, host) {
        return false;
      }
      let keyring = os_crypt::SourceKeyring::sealed_under(std::env::consts::OS, None).unwrap();
      matches!(
        keyring.decrypt(&row),
        os_crypt::Decrypted::Value(plain) if plain == framed(host, value)
      )
    }

    #[test]
    fn a_wayfern_sealed_profile_is_resealed_for_the_engine_once() {
      let dir = TempDir::new().unwrap();
      let profile = dir.path().join("profile");
      touch(&profile.join(os_crypt::KEY_FILE_NAME), WAYFERN_PASSWORD);
      cookie_store(
        &profile,
        &[(
          "example.test",
          "session",
          &sealed_by_wayfern(&framed("example.test", "kept-value")),
        )],
      );
      assert!(!engine_opens(
        &profile,
        "session",
        "example.test",
        "kept-value"
      ));

      let report = adopt_engine_key(&profile).unwrap().expect("a re-seal ran");
      assert_eq!(report.cookies_migrated, 1);
      assert_eq!(report.cookies_unrecoverable, 0);
      assert!(engine_opens(
        &profile,
        "session",
        "example.test",
        "kept-value"
      ));
      assert_eq!(
        std::fs::read(profile.join(os_crypt::KEY_FILE_NAME)).unwrap(),
        WAYFERN_PASSWORD,
        "the old key stays, so nothing is lost if the profile goes back"
      );
      assert_eq!(
        std::fs::read_to_string(profile.join(ENGINE_KEY_MARKER))
          .unwrap()
          .trim(),
        std::env::consts::OS
      );

      let sealed = stored(&profile, "session");
      assert!(adopt_engine_key(&profile).unwrap().is_none());
      assert_eq!(
        stored(&profile, "session"),
        sealed,
        "the second run is a no-op"
      );
    }

    #[test]
    fn a_row_the_engine_already_sealed_or_nothing_opens_is_left_alone() {
      let dir = TempDir::new().unwrap();
      let profile = dir.path().join("profile");
      touch(&profile.join(os_crypt::KEY_FILE_NAME), WAYFERN_PASSWORD);
      let engine_row = sealed_by_engine(&profile, &framed("a.test", "from-engine"));
      let mut foreign = b"v10".to_vec();
      foreign.extend([7u8; 32]);
      cookie_store(
        &profile,
        &[
          ("a.test", "engine", &engine_row),
          (
            "b.test",
            "wayfern",
            &sealed_by_wayfern(&framed("b.test", "from-wayfern")),
          ),
          ("c.test", "foreign", &foreign),
        ],
      );

      adopt_engine_key(&profile).unwrap().expect("a re-seal ran");

      assert_eq!(stored(&profile, "engine"), engine_row);
      assert!(engine_opens(&profile, "wayfern", "b.test", "from-wayfern"));
      assert_eq!(
        stored(&profile, "foreign"),
        foreign,
        "a row no key here opens is kept, not deleted"
      );
    }

    #[test]
    fn a_profile_without_an_old_key_only_gets_the_marker() {
      let dir = TempDir::new().unwrap();
      let profile = dir.path().join("profile");
      let row = sealed_by_engine(&profile, &framed("a.test", "from-engine"));
      cookie_store(&profile, &[("a.test", "engine", &row)]);

      assert!(adopt_engine_key(&profile).unwrap().is_none());

      assert_eq!(stored(&profile, "engine"), row);
      assert!(profile.join(ENGINE_KEY_MARKER).exists());
      assert!(!profile.join(os_crypt::KEY_FILE_NAME).exists());
    }

    #[test]
    fn a_profile_synced_from_another_os_is_resealed_for_this_one() {
      let dir = TempDir::new().unwrap();
      let profile = dir.path().join("profile");
      let other = if cfg!(target_os = "macos") {
        "linux"
      } else {
        "macos"
      };
      let other_key = os_crypt::SourceKeyring::sealed_under(other, None).unwrap();
      let mut sealed = b"v10".to_vec();
      sealed.extend(
        other_key
          .v10
          .as_ref()
          .unwrap()
          .encrypt(&framed("a.test", "travelled"))
          .unwrap(),
      );
      cookie_store(&profile, &[("a.test", "moved", &sealed)]);
      touch(
        &profile.join(ENGINE_KEY_MARKER),
        format!("{other}\n").as_bytes(),
      );

      adopt_engine_key(&profile).unwrap().expect("a re-seal ran");

      assert!(engine_opens(&profile, "moved", "a.test", "travelled"));
    }
  }

  #[test]
  fn repair_leaves_a_healthy_profile_alone() {
    let dir = TempDir::new().unwrap();
    let profile = dir.path().join("profile");
    touch(&profile.join("Default").join("Preferences"), b"{}");
    touch(&profile.join("Local State"), b"{}");

    assert!(!repair_legacy_layout(&profile).unwrap());
    assert!(profile.join("Default").join("Preferences").exists());
    assert!(!profile.join("Default").join("Default").exists());
  }

  #[test]
  fn repair_is_a_no_op_on_an_empty_profile() {
    let dir = TempDir::new().unwrap();
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    assert!(!repair_legacy_layout(&profile).unwrap());
  }

  #[test]
  fn repair_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let profile = dir.path().join("profile");
    touch(&profile.join("Preferences"), b"{}");

    assert!(repair_legacy_layout(&profile).unwrap());
    assert!(!repair_legacy_layout(&profile).unwrap());
    assert!(profile.join("Default").join("Preferences").exists());
  }
}
