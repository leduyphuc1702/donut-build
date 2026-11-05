//! Starting a profile on the Apostate engine.
//!
//! A child of `wayfern_manager` so it shares the browser instance table, the
//! CDP readiness wait and the log tap with the Wayfern launch, and tracks the
//! process the same way: stop, kill, CDP and MCP find it exactly as they find a
//! Wayfern browser. What differs is everything Wayfern-specific: no token, no
//! identity document, no `Wayfern.setIdentity` apply and verify. The persona
//! travels on the command line and is committed before the first navigation.

use super::*;
use crate::apostate::{self, Persona};
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use std::collections::HashSet;

/// Everything the command line of one launch is built from.
pub(super) struct ArgsInput<'a> {
  pub port: u16,
  pub profile_path: &'a str,
  pub headless: bool,
  pub ephemeral: bool,
  pub restore_session: bool,
  pub extension_paths: &'a [String],
  pub proxy_url: Option<&'a str>,
  pub persona: Persona<'a>,
  pub pages: &'a [&'a str],
  pub custom_launch_args: &'a [String],
}

/// The Apostate command line. Every entry is its own argument, as the engine
/// requires.
pub(super) fn build_args(input: &ArgsInput) -> Result<Vec<String>, String> {
  let mut args = vec![
    format!("--remote-debugging-port={}", input.port),
    "--remote-debugging-address=127.0.0.1".to_string(),
    format!("--user-data-dir={}", input.profile_path),
    "--no-first-run".to_string(),
    "--no-default-browser-check".to_string(),
    "--disable-background-mode".to_string(),
    "--hide-crash-restore-bubble".to_string(),
    "--disable-infobars".to_string(),
    LEAK_FEATURES_SWITCH.to_string(),
    "--use-mock-keychain".to_string(),
    "--password-store=basic".to_string(),
  ];
  if input.headless {
    args.push("--headless=new".to_string());
  }
  #[cfg(target_os = "linux")]
  {
    args.push("--no-sandbox".to_string());
    args.push("--disable-setuid-sandbox".to_string());
    args.push("--disable-dev-shm-usage".to_string());
  }
  if input.ephemeral {
    args.push("--disk-cache-size=1".to_string());
    args.push("--disable-breakpad".to_string());
    args.push("--disable-crash-reporter".to_string());
    args.push("--no-service-autorun".to_string());
    args.push("--disable-sync".to_string());
  }
  if !input.extension_paths.is_empty() {
    args.push(format!(
      "--load-extension={}",
      input.extension_paths.join(",")
    ));
  }
  if let Some(proxy) = input.proxy_url {
    args.extend(proxy_switches(proxy));
    // Chromium's own guard, so a page cannot open a UDP socket around the proxy.
    args.push("--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string());
  }
  if input.restore_session {
    args.push("--restore-last-session".to_string());
  }
  args.extend(apostate::persona_args(&input.persona)?);
  apply_launch_args(&mut args, input.custom_launch_args);
  // A page named on the command line must not read as a switch.
  args.extend(
    input
      .pages
      .iter()
      .filter(|page| !page.starts_with('-') && !page.trim().is_empty())
      .map(|page| page.to_string()),
  );
  Ok(args)
}

/// The exit's location for this launch: what the profile stores, completed
/// through the route the browser is about to use when it holds no timezone.
/// The host's timezone is never filled in.
async fn launch_location(
  config: &WayfernConfig,
  proxy_url: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
  let mut location: serde_json::Map<String, serde_json::Value> = config
    .fingerprint
    .as_deref()
    .and_then(WayfernManager::fingerprint_object)
    .map(|device| {
      LOCALE_CARRY_OVER_KEYS
        .iter()
        .filter_map(|key| {
          device
            .get(*key)
            .filter(|value| !value.is_null())
            .map(|value| ((*key).to_string(), value.clone()))
        })
        .collect()
    })
    .unwrap_or_default();
  location.extend(WayfernManager::stored_object(config.location.as_deref()));
  let has_timezone = location
    .get("timezone")
    .and_then(serde_json::Value::as_str)
    .is_some_and(|timezone| !timezone.is_empty());
  if !has_timezone {
    if let Some(geo) = WayfernManager::resolve_geolocation(proxy_url, config.geoip.as_ref()).await {
      let mut resolved = serde_json::Value::Object(location);
      WayfernManager::write_geolocation(&mut resolved, &geo);
      location = resolved.as_object().cloned().unwrap_or_default();
    }
  }
  location
}

fn languages_of(location: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
  let ladder: Vec<String> = location
    .get("languages")
    .and_then(serde_json::Value::as_array)
    .map(|tags| {
      tags
        .iter()
        .filter_map(|tag| tag.as_str().map(str::to_string))
        .collect()
    })
    .unwrap_or_default();
  if !ladder.is_empty() {
    return ladder;
  }
  location
    .get("language")
    .and_then(serde_json::Value::as_str)
    .map(|tag| vec![tag.to_string()])
    .unwrap_or_default()
}

impl WayfernManager {
  #[allow(clippy::too_many_arguments)]
  pub(super) async fn launch_apostate(
    &self,
    app_handle: &AppHandle,
    profile: &BrowserProfile,
    profile_path: &str,
    config: &WayfernConfig,
    url: Option<&str>,
    proxy_url: Option<&str>,
    ephemeral: bool,
    extension_paths: &[String],
    remote_debugging_port: Option<u16>,
    headless: bool,
    kind: LaunchKind,
  ) -> Result<WayfernLaunchResult, Box<dyn std::error::Error + Send + Sync>> {
    let _ = app_handle;
    let started = std::time::Instant::now();
    let custom_launch_args = match config.checked_launch_args().await {
      Ok(args) => args,
      Err(error) => {
        log::warn!(
          "Custom launch arguments dropped profile={} err=\"{error}\"",
          profile.id
        );
        let _ = crate::events::emit(
          "profile-launch-args-skipped",
          serde_json::json!({ "profile_name": profile.name, "error": error }),
        );
        Vec::new()
      }
    };
    let executable_path = apostate::ensure_installed().await?;
    let platform = crate::launch_gate::claimed_fingerprint_os(profile);
    let fonts_present = apostate::persona_fonts_present(&platform);
    if !fonts_present {
      log::warn!(
        "Persona fonts missing, text measured by a page shows this machine's fonts profile={} platform={}",
        profile.id,
        crate::log_redaction::Plain(&platform)
      );
    }

    let port = match remote_debugging_port {
      Some(p) => p,
      None => Self::find_free_port().await?,
    };
    crate::cookie_health::log_before_launch(
      &profile.id.to_string(),
      std::path::Path::new(profile_path),
    );

    let location = launch_location(config, proxy_url).await;
    let timezone = location
      .get("timezone")
      .and_then(serde_json::Value::as_str)
      .map(str::to_string);
    if timezone.is_none() {
      log::warn!(
        "No timezone for the exit, the page sees this machine's profile={}",
        profile.id
      );
    }
    let webrtc_mode = WebRtcMode::from_config(config);
    let exit_ip = crate::fingerprint_consistency::cached_exit_ip(profile);
    let seed = profile.id.to_string();
    let persona = Persona {
      seed: &seed,
      platform: &platform,
      languages: languages_of(&location),
      timezone: timezone.as_deref(),
      webrtc_ip: exit_ip
        .as_deref()
        .filter(|_| proxy_url.is_some() && webrtc_mode != WebRtcMode::Block),
      block_webrtc: webrtc_mode == WebRtcMode::Block,
      device: config
        .fingerprint
        .as_deref()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .as_ref()
        .and_then(crate::apostate::DeviceClaim::from_payload),
    };

    let pages = launch_pages(url, &profile.start_urls, kind);
    let restore_session = session_restore_verdict(
      config,
      kind,
      headless,
      ephemeral,
      profile.clear_on_close,
      true,
      !profile.start_urls.is_empty(),
    )
    .is_ok();
    let args = build_args(&ArgsInput {
      port,
      profile_path,
      headless,
      ephemeral,
      restore_session,
      extension_paths,
      proxy_url,
      persona,
      pages: &pages,
      custom_launch_args: &custom_launch_args,
    })?;

    let mut command = TokioCommand::new(&executable_path);
    command
      .args(&args)
      .envs(apostate::ENVIRONMENT.iter().copied())
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::piped());
    let mut child = command
      .spawn()
      .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
        format!("Failed to spawn Apostate: {e}").into()
      })?;
    let process_id = child.id();
    let log_tap = BrowserLogTap::default();
    if let Some(stderr) = child.stderr.take() {
      tap_browser_stderr(
        stderr,
        log_tap.clone(),
        format!(
          "profile={} pid={}",
          profile.id,
          process_id.unwrap_or_default()
        ),
      );
    }

    if let Err(e) = self
      .wait_for_cdp_ready_while_running(port, &mut child, &log_tap)
      .await
    {
      if let Some(pid) = process_id {
        if crate::proxy_storage::is_process_running(pid) {
          log::warn!(
            "Browser killed during launch profile={} pid={pid} reason=cdp_not_ready",
            profile.id
          );
        }
        kill_browser_process(pid);
      }
      return Err(e);
    }
    drop(child);

    let holds_position = match apostate::geolocation_override(&location) {
      Some(position) => {
        tauri::async_runtime::spawn(hold_geolocation(
          port,
          position,
          format!(
            "profile={} pid={}",
            profile.id,
            process_id.unwrap_or_default()
          ),
        ));
        true
      }
      None => false,
    };

    log::info!(
      "Apostate launched profile={} pid={} cdp_port={port} engine_version={} platform={} timezone={} fonts={} geolocation={} elapsed_ms={}",
      profile.id,
      process_id.unwrap_or_default(),
      apostate::VERSION,
      crate::log_redaction::Plain(&platform),
      if timezone.is_some() { "set" } else { "host" },
      if fonts_present { "present" } else { "missing" },
      if holds_position { "held" } else { "none" },
      started.elapsed().as_millis()
    );

    let id = uuid::Uuid::new_v4().to_string();
    let instance = WayfernInstance {
      process_id,
      profile_path: Some(profile_path.to_string()),
      url: url.map(|s| s.to_string()),
      cdp_port: Some(port),
      log_tap,
    };
    self
      .inner
      .lock()
      .await
      .instances
      .insert(id.clone(), instance);

    Ok(WayfernLaunchResult {
      id,
      processId: process_id,
      profilePath: Some(profile_path.to_string()),
      url: url.map(|s| s.to_string()),
      cdp_port: Some(port),
      stored_profile: None,
    })
  }
}

/// Keep `position` as the geolocation of every page until the browser exits.
///
/// Apostate has no switch for it, and a CDP emulation override is dropped when
/// the session that set it closes, so one browser-level session stays open and
/// sets the override on each page as it appears. Best effort: without it a site
/// that asks for the position gets a prompt, never the position unasked.
async fn hold_geolocation(port: u16, position: serde_json::Value, owner: String) {
  let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
    let version: serde_json::Value = reqwest::Client::builder()
      .no_proxy()
      .timeout(Duration::from_secs(3))
      .build()?
      .get(format!("http://127.0.0.1:{port}/json/version"))
      .send()
      .await?
      .json()
      .await?;
    let ws_url = version["webSocketDebuggerUrl"]
      .as_str()
      .ok_or("the browser reports no debugger socket")?;
    let (mut socket, _) = connect_async(ws_url).await?;
    let mut next_id = 0u64;
    let mut send = |method: &str, params: serde_json::Value, session: Option<&str>| {
      next_id += 1;
      let mut command = json!({ "id": next_id, "method": method, "params": params });
      if let Some(session) = session {
        command["sessionId"] = json!(session);
      }
      (next_id, Message::Text(command.to_string().into()))
    };

    let (_, message) = send(
      "Target.setAutoAttach",
      json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }),
      None,
    );
    socket.send(message).await?;
    let (targets_id, message) = send("Target.getTargets", json!({}), None);
    socket.send(message).await?;

    let mut handled: HashSet<String> = HashSet::new();
    let mut attach_ids: HashSet<u64> = HashSet::new();
    while let Some(message) = socket.next().await {
      let Message::Text(text) = message? else {
        continue;
      };
      let message: serde_json::Value = serde_json::from_str(text.as_str())?;
      let mut sessions: Vec<String> = Vec::new();
      let mut to_attach: Vec<String> = Vec::new();
      if message["method"] == "Target.attachedToTarget"
        && message["params"]["targetInfo"]["type"] == "page"
      {
        if let Some(session) = message["params"]["sessionId"].as_str() {
          sessions.push(session.to_string());
        }
      } else if message["id"] == json!(targets_id) {
        for info in message["result"]["targetInfos"]
          .as_array()
          .into_iter()
          .flatten()
        {
          if let (Some("page"), Some(target_id)) =
            (info["type"].as_str(), info["targetId"].as_str())
          {
            to_attach.push(target_id.to_string());
          }
        }
      } else if let Some(id) = message["id"].as_u64().filter(|id| attach_ids.remove(id)) {
        let _ = id;
        if let Some(session) = message["result"]["sessionId"].as_str() {
          sessions.push(session.to_string());
        }
      }
      for target_id in to_attach {
        let (id, message) = send(
          "Target.attachToTarget",
          json!({ "targetId": target_id, "flatten": true }),
          None,
        );
        attach_ids.insert(id);
        socket.send(message).await?;
      }
      for session in sessions {
        if handled.insert(session.clone()) {
          let (_, message) = send(
            "Emulation.setGeolocationOverride",
            position.clone(),
            Some(&session),
          );
          socket.send(message).await?;
        }
      }
    }
    Ok(())
  }
  .await;
  match result {
    Ok(()) => log::debug!("Geolocation hold ended, the browser closed {owner}"),
    Err(e) => log::debug!(
      "Geolocation hold ended {owner} err=\"{}\"",
      crate::log_redaction::text(&e.to_string())
    ),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn input<'a>(persona: Persona<'a>, pages: &'a [&'a str], custom: &'a [String]) -> ArgsInput<'a> {
    ArgsInput {
      port: 9333,
      profile_path: "/tmp/profile dir",
      headless: false,
      ephemeral: false,
      restore_session: false,
      extension_paths: &[],
      proxy_url: Some("socks5://127.0.0.1:5555"),
      persona,
      pages,
      custom_launch_args: custom,
    }
  }

  fn persona(platform: &str) -> Persona<'_> {
    Persona {
      seed: "0b5d0a7e-0000-4000-8000-000000000002",
      platform,
      languages: vec!["en-US".into(), "en".into()],
      timezone: Some("America/Chicago"),
      webrtc_ip: None,
      block_webrtc: false,
      device: None,
    }
  }

  #[test]
  fn the_command_line_has_no_wayfern_switch_and_keeps_paths_whole() {
    let args = build_args(&input(persona("windows"), &[], &[])).unwrap();
    assert!(!args.iter().any(|a| a.contains("wayfern")), "{args:?}");
    assert!(args.contains(&"--user-data-dir=/tmp/profile dir".to_string()));
    assert!(args.contains(&"--fingerprint-platform=windows".to_string()));
    assert!(args.contains(&"--fingerprint-noise".to_string()));
    assert!(args.contains(&"--use-mock-keychain".to_string()));
    assert!(args.iter().any(|a| a.starts_with("--proxy-pac-url=")));
    assert!(args.contains(&"--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string()));
  }

  #[test]
  fn a_page_never_reads_as_a_switch_and_follows_the_switches() {
    let pages = ["https://example.com/", "--no-sandbox", "  "];
    let args = build_args(&input(persona("windows"), &pages, &[])).unwrap();
    assert_eq!(
      args.last().map(String::as_str),
      Some("https://example.com/")
    );
    assert_eq!(args.iter().filter(|a| *a == "--no-sandbox").count(), {
      #[cfg(target_os = "linux")]
      {
        1
      }
      #[cfg(not(target_os = "linux"))]
      {
        0
      }
    });
  }

  #[test]
  fn an_unknown_platform_stops_the_launch_before_a_process_exists() {
    let error = build_args(&input(persona("android"), &[], &[])).unwrap_err();
    assert!(error.contains("FINGERPRINT_HOST_OS_MISMATCH"), "{error}");
  }

  #[test]
  fn the_locale_ladder_is_the_exits_and_falls_back_to_its_language() {
    let location = |json: &str| {
      serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json).unwrap()
    };
    assert_eq!(
      languages_of(&location(
        r#"{"languages":["de-DE","de"],"language":"de-DE"}"#
      )),
      vec!["de-DE", "de"]
    );
    assert_eq!(
      languages_of(&location(r#"{"language":"fr-FR"}"#)),
      vec!["fr-FR"]
    );
    assert!(languages_of(&location("{}")).is_empty());
  }

  /// One evaluation in a page, on a browser-level socket that grants the
  /// geolocation permission first (a grant lasts as long as its session).
  async fn evaluate_in_first_page(browser_ws: &str, expression: &str) -> serde_json::Value {
    let (mut socket, _) = connect_async(browser_ws).await.unwrap();
    let call = |id: u64, method: &str, params: serde_json::Value, session: Option<&str>| {
      let mut command = json!({ "id": id, "method": method, "params": params });
      if let Some(session) = session {
        command["sessionId"] = json!(session);
      }
      Message::Text(command.to_string().into())
    };
    socket
      .send(call(
        1,
        "Browser.grantPermissions",
        json!({ "permissions": ["geolocation"] }),
        None,
      ))
      .await
      .unwrap();
    socket
      .send(call(2, "Target.getTargets", json!({}), None))
      .await
      .unwrap();
    let mut session: Option<String> = None;
    let mut answer = serde_json::Value::Null;
    while let Some(message) = socket.next().await {
      let Message::Text(text) = message.unwrap() else {
        continue;
      };
      let value: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
      match value["id"].as_u64() {
        Some(2) => {
          let target = value["result"]["targetInfos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|info| info["type"] == "page")
            .unwrap()["targetId"]
            .clone();
          socket
            .send(call(
              3,
              "Target.attachToTarget",
              json!({ "targetId": target, "flatten": true }),
              None,
            ))
            .await
            .unwrap();
        }
        Some(3) => {
          session = value["result"]["sessionId"].as_str().map(str::to_string);
          socket
            .send(call(
              4,
              "Runtime.evaluate",
              json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
              session.as_deref(),
            ))
            .await
            .unwrap();
        }
        Some(4) => {
          answer = value["result"]["result"]["value"].clone();
          break;
        }
        _ => {}
      }
    }
    assert!(session.is_some(), "no page session");
    answer
  }

  /// Ends the browser even when an assertion panics before the test reaches its cleanup.
  struct KillOnDrop(u32);

  impl Drop for KillOnDrop {
    fn drop(&mut self) {
      kill_browser_process(self.0);
    }
  }

  /// Downloads the pinned release (~158 MB) into a throwaway data dir, checks it
  /// through the production installer, starts the real browser with the argv the
  /// launcher builds, and reads the persona back over CDP.
  /// `cargo test --lib real_engine -- --ignored --nocapture`
  #[tokio::test(flavor = "current_thread")]
  #[ignore = "downloads the engine and starts a real browser"]
  async fn real_engine_presents_the_windows_persona_over_cdp() {
    let root = tempfile::tempdir().unwrap();
    let _data = crate::app_dirs::set_test_data_dir(root.path().to_path_buf());

    let exe = apostate::ensure_installed().await.unwrap();
    assert!(
      exe.starts_with(root.path()),
      "installed outside the throwaway dir"
    );
    assert_eq!(
      apostate::ensure_installed().await.unwrap(),
      exe,
      "second call must not download"
    );

    let manager = WayfernManager::new();
    let port = WayfernManager::find_free_port().await.unwrap();
    let profile_dir = root.path().join("profile");
    std::fs::create_dir_all(&profile_dir).unwrap();
    let profile_path = profile_dir.to_string_lossy().to_string();
    let seed = uuid::Uuid::new_v4().to_string();
    let platform = if cfg!(target_os = "windows") {
      "macos"
    } else {
      "windows"
    };
    let expected_platform = if platform == "windows" {
      "Win32"
    } else {
      "MacIntel"
    };
    let args = build_args(&ArgsInput {
      port,
      profile_path: &profile_path,
      headless: true,
      ephemeral: false,
      restore_session: false,
      extension_paths: &[],
      proxy_url: None,
      persona: Persona {
        seed: &seed,
        platform,
        languages: vec!["en-US".into(), "en".into()],
        timezone: Some("America/Los_Angeles"),
        webrtc_ip: None,
        block_webrtc: false,
        device: Some(crate::apostate::DeviceClaim {
          screen: Some((1920, 1080)),
          cores: Some(4),
          memory_gb: Some(8),
        }),
      },
      pages: &["about:blank"],
      custom_launch_args: &[],
    })
    .unwrap();

    let mut child = TokioCommand::new(&exe)
      .args(&args)
      .envs(apostate::ENVIRONMENT.iter().copied())
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let pid = child.id().unwrap();
    let guard = KillOnDrop(pid);
    let tap = BrowserLogTap::default();
    tap_browser_stderr(child.stderr.take().unwrap(), tap.clone(), "test".into());
    let targets = manager
      .wait_for_cdp_ready_while_running(port, &mut child, &tap)
      .await;
    let result = async {
      let targets = targets.map_err(|e| e.to_string())?;
      let ws = targets[0].websocket_debugger_url.clone().unwrap();
      let read = manager
        .send_cdp_command(
          &ws,
          "Runtime.evaluate",
          json!({
            "expression": "JSON.stringify({platform: navigator.platform, timezone: Intl.DateTimeFormat().resolvedOptions().timeZone, languages: navigator.languages, webdriver: navigator.webdriver, ua: navigator.userAgent})",
            "returnByValue": true,
          }),
        )
        .await
        .map_err(|e| e.to_string())?;
      let seen: serde_json::Value =
        serde_json::from_str(read["result"]["value"].as_str().unwrap()).unwrap();
      println!("persona over CDP: {seen}");
      assert_eq!(seen["platform"], expected_platform);
      assert_eq!(seen["timezone"], "America/Los_Angeles");
      assert_eq!(seen["languages"], json!(["en-US", "en"]));
      assert_eq!(seen["webdriver"], false);

      // The same seed on the same profile directory is the same machine.
      let canvas = "(() => { const c = document.createElement('canvas'); c.width = 60; c.height = 20; const x = c.getContext('2d'); x.font = '14px Arial'; x.fillText('apostate', 2, 14); return c.toDataURL().length + ':' + (navigator.hardwareConcurrency) })()";
      let first = manager
        .send_cdp_command(&ws, "Runtime.evaluate", json!({ "expression": canvas, "returnByValue": true }))
        .await
        .map_err(|e| e.to_string())?;
      println!("canvas/cores: {}", first["result"]["value"]);

      // The held geolocation override reaches a page that asks for it.
      let location = json!({ "latitude": 47.6062, "longitude": -122.3321, "accuracy": 50.0 });
      let holder = tauri::async_runtime::spawn(hold_geolocation(port, location, "test".into()));
      // Geolocation needs a secure origin, and loopback is one.
      let site = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
      let site_port = site.local_addr().unwrap().port();
      let server = tauri::async_runtime::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut stream, _)) = site.accept().await {
          let mut buffer = [0u8; 2048];
          let _ = stream.read(&mut buffer).await;
          let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 15\r\nConnection: close\r\n\r\n<html>ok</html>")
            .await;
        }
      });
      manager
        .send_cdp_command(&ws, "Page.navigate", json!({ "url": format!("http://127.0.0.1:{site_port}/") }))
        .await
        .map_err(|e| e.to_string())?;
      tokio::time::sleep(Duration::from_secs(2)).await;
      let version: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/json/version"))
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
      let position = evaluate_in_first_page(
        version["webSocketDebuggerUrl"].as_str().unwrap(),
        "new Promise(r => navigator.geolocation.getCurrentPosition(p => r([p.coords.latitude, p.coords.longitude]), e => r('error: ' + e.message), { timeout: 8000 }))",
      )
      .await;
      println!("geolocation seen by the page: {position}");
      holder.abort();
      server.abort();
      assert_eq!(position, json!([47.6062, -122.3321]));
      Ok::<(), String>(())
    }
    .await;
    drop(guard);
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    result.unwrap();
  }

  #[tokio::test]
  async fn a_stored_location_is_used_as_is_and_the_hosts_is_never_filled_in() {
    let config = WayfernConfig {
      location: Some(r#"{"timezone":"Asia/Tokyo","language":"ja-JP"}"#.into()),
      fingerprint: Some(r#"{"timezone":"Europe/Paris","latitude":48.8,"longitude":2.3}"#.into()),
      ..Default::default()
    };
    let location = launch_location(&config, None).await;
    assert_eq!(location["timezone"], "Asia/Tokyo");
    assert_eq!(location["latitude"], 48.8);
    assert_eq!(location["language"], "ja-JP");
  }
}
