//! The hands and eyes: one DevTools connection that moves a real pointer over
//! one page and reads what is on it.
//!
//! Input goes only through `Input.dispatchMouseEvent` (moves, presses, wheel)
//! and `Input.dispatchKeyEvent`, so every event the page sees is trusted and
//! shaped by the planners. The page is read with one isolated-world script
//! (`page.rs`). The only domain enabled is `Page`, for load and navigation
//! events; `Runtime` is never enabled, and nothing is evaluated in the page's
//! own world.
//!
//! The connection is the browser's, with the page attached as a flattened
//! session, so a tab opened by a click can be found, attached and switched to.

use super::keys::{key_for, Key, KeyPress, BACKSPACE, CONTROL, ENTER, META, SHIFT};
use super::page::{control_at_expression, link_at_expression, read_expression, Snapshot};
use super::pointer::{drift_target, entry_point, plan_move, ClickPlan, MovePlan, Point};
use super::rhythm::Draw;
use super::scroll::ScrollPlan;
use super::Tally;
use crate::cdp_target::CdpConnection;
use crate::navigation_watch::NavigationWatch;
use crate::paste_typing::Stroke;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// How long an ordinary DevTools command may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest wait for a page to load before reading what is there.
const LOAD_TIMEOUT: Duration = Duration::from_secs(25);
/// Events kept for the next wait; older ones are dropped.
const EVENT_CAP: usize = 3_000;
/// A window tucked aside keeps this much of itself on the screen: a window
/// wholly off every display is hidden by the system, and so by the page.
const SLIVER_PX: i64 = 48;

/// Whether the run has been told to stop.
pub type StopFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// Why a step could not be completed.
#[derive(Debug)]
pub enum Fault {
  /// The run was cancelled, or its time is spent.
  Stopped,
  /// The browser went away.
  Closed(String),
  /// The browser refused a command.
  Protocol(String),
  /// A command or a wait did not finish.
  Timeout(&'static str),
}

impl std::fmt::Display for Fault {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Fault::Stopped => write!(f, "stopped"),
      Fault::Closed(detail) => write!(f, "browser connection closed: {detail}"),
      Fault::Protocol(detail) => write!(f, "browser refused: {detail}"),
      Fault::Timeout(what) => write!(f, "timed out: {what}"),
    }
  }
}

impl Fault {
  /// The browser is gone; nothing more can be done in this run.
  pub fn is_fatal(&self) -> bool {
    matches!(self, Fault::Closed(_) | Fault::Stopped)
  }
}

struct Event {
  session: Option<String>,
  method: String,
  params: Value,
}

/// What a click did to the page.
#[derive(Debug, PartialEq, Eq)]
pub enum ClickOutcome {
  /// The page loaded another document, or changed route.
  Navigated,
  /// Another tab opened, by this target id.
  NewTab(String),
  /// A navigation began and had not finished in time.
  TimedOut,
  /// Nothing on the page navigated.
  Nothing,
}

/// Where the window was before it was tucked aside.
pub struct WindowSpot {
  window_id: i64,
  original: Value,
}

/// The tab the driver left to look at another one, to go back to.
pub struct ParkedTab {
  session: Option<String>,
  target_id: String,
  frame_id: String,
}

pub struct Driver {
  conn: CdpConnection,
  next_id: u64,
  session: Option<String>,
  target_id: String,
  frame_id: String,
  world: Option<i64>,
  events: VecDeque<Event>,
  stop: StopFn,
  until: Instant,
  /// Whether a pending JavaScript dialog is to be accepted.
  dialog: Option<bool>,
  pub pointer: Option<Point>,
  pub tally: Tally,
  /// Responses of the shop's search suggestions seen on the page, by request
  /// id: loading, then finished and ready to be read.
  suggest_loading: Vec<String>,
  suggest_ready: Vec<String>,
}

impl Driver {
  pub fn new(conn: CdpConnection, stop: StopFn, until: Instant) -> Self {
    Driver {
      conn,
      next_id: 0,
      session: None,
      target_id: String::new(),
      frame_id: String::new(),
      world: None,
      events: VecDeque::new(),
      stop,
      until,
      dialog: None,
      pointer: None,
      tally: Tally::default(),
      suggest_loading: Vec::new(),
      suggest_ready: Vec::new(),
    }
  }

  /// Hang up. A socket left open holds the browser's attachment.
  pub async fn close(self) {
    self.conn.close().await;
  }

  pub fn stopped(&self) -> bool {
    (self.stop)() || Instant::now() >= self.until
  }

  pub fn check(&self) -> Result<(), Fault> {
    if self.stopped() {
      Err(Fault::Stopped)
    } else {
      Ok(())
    }
  }

  /// Time left before the run's budget is spent.
  pub fn remaining(&self) -> Duration {
    self.until.saturating_duration_since(Instant::now())
  }

  async fn read_until(&mut self, deadline: Instant) -> Result<Option<Value>, Fault> {
    match tokio::time::timeout_at(deadline, self.conn.next_text()).await {
      Err(_) => Ok(None),
      Ok(None) => Err(Fault::Closed("the connection ended".to_string())),
      Ok(Some(Err(error))) => Err(Fault::Closed(error.to_string())),
      Ok(Some(Ok(text))) => Ok(Some(serde_json::from_str(&text).unwrap_or(Value::Null))),
    }
  }

  /// Take in a message that is not the answer being waited for: remember it as
  /// an event, and note what changes the driver's own state.
  fn absorb(&mut self, message: Value) {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
      return;
    };
    let session = message
      .get("sessionId")
      .and_then(Value::as_str)
      .map(str::to_string);
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let ours = session.is_some() && session == self.session;
    if method.starts_with("Network.") {
      // Network events serve only the suggestion watch; the page's traffic is
      // never kept as events.
      if ours {
        self.note_network(method, &params);
      }
      return;
    }
    if ours && method == "Page.frameNavigated" && params.pointer("/frame/parentId").is_none() {
      self.world = None;
    }
    if ours && method == "Page.javascriptDialogOpening" {
      let kind = params
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("alert");
      // A page being left, or an alert, is let go; a question is declined.
      self.dialog = Some(!matches!(kind, "confirm" | "prompt"));
    }
    if self.events.len() >= EVENT_CAP {
      self.events.pop_front();
    }
    self.events.push_back(Event {
      session,
      method: method.to_string(),
      params,
    });
  }

  async fn service_dialog(&mut self) {
    if let Some(accept) = self.dialog.take() {
      self.next_id += 1;
      let session = self.session.clone();
      let _ = self
        .conn
        .send_to(
          session.as_deref(),
          self.next_id,
          "Page.handleJavaScriptDialog",
          json!({ "accept": accept }),
        )
        .await;
    }
  }

  async fn request(
    &mut self,
    session: Option<String>,
    method: &str,
    params: Value,
    timeout: Duration,
  ) -> Result<Value, Fault> {
    self.next_id += 1;
    let id = self.next_id;
    self
      .conn
      .send_to(session.as_deref(), id, method, params)
      .await
      .map_err(|error| Fault::Closed(error.to_string()))?;
    let deadline = Instant::now() + timeout;
    loop {
      let Some(message) = self.read_until(deadline).await? else {
        log::debug!("Warm-up command unanswered method={method}");
        return Err(Fault::Timeout("a DevTools command"));
      };
      if message.get("id").and_then(Value::as_u64) == Some(id) {
        if let Some(error) = message.get("error") {
          let detail = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error");
          return Err(Fault::Protocol(detail.to_string()));
        }
        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
      }
      self.absorb(message);
      self.service_dialog().await;
    }
  }

  /// A command to the attached page.
  pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, Fault> {
    let session = self.session.clone();
    self.request(session, method, params, COMMAND_TIMEOUT).await
  }

  /// A command to the browser itself.
  pub async fn browser_call(&mut self, method: &str, params: Value) -> Result<Value, Fault> {
    self.request(None, method, params, COMMAND_TIMEOUT).await
  }

  /// Wait `ms` milliseconds while still taking in what the browser sends.
  pub async fn idle(&mut self, ms: u64) -> Result<(), Fault> {
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
      self.check()?;
      let now = Instant::now();
      if now >= end {
        return Ok(());
      }
      let slice = end.min(now + Duration::from_millis(200));
      if let Some(message) = self.read_until(slice).await? {
        self.absorb(message);
        self.service_dialog().await;
      }
    }
  }

  // --- Tabs and window ------------------------------------------------------

  /// Ask to be told when tabs open.
  pub async fn watch_targets(&mut self) -> Result<(), Fault> {
    self
      .browser_call("Target.setDiscoverTargets", json!({ "discover": true }))
      .await
      .map(|_| ())
  }

  /// A page of the browser that is not DevTools; one is opened when there is
  /// none.
  pub async fn working_target(&mut self) -> Result<String, Fault> {
    let listed = self.browser_call("Target.getTargets", json!({})).await?;
    let found = listed
      .get("targetInfos")
      .and_then(Value::as_array)
      .and_then(|infos| {
        infos.iter().find(|info| {
          info.get("type").and_then(Value::as_str) == Some("page")
            && !info
              .get("url")
              .and_then(Value::as_str)
              .unwrap_or_default()
              .starts_with("devtools://")
        })
      })
      .and_then(|info| info.get("targetId").and_then(Value::as_str))
      .map(str::to_string);
    if let Some(id) = found {
      return Ok(id);
    }
    let created = self
      .browser_call("Target.createTarget", json!({ "url": "about:blank" }))
      .await?;
    created
      .get("targetId")
      .and_then(Value::as_str)
      .map(str::to_string)
      .ok_or_else(|| Fault::Protocol("no target was created".to_string()))
  }

  /// Make `target_id` the page being driven: attach, turn on page events, and
  /// let the page see itself as focused and visible even though its window is
  /// behind others (`Emulation.setFocusEmulationEnabled`).
  pub async fn attach(&mut self, target_id: &str) -> Result<(), Fault> {
    let attached = self
      .browser_call(
        "Target.attachToTarget",
        json!({ "targetId": target_id, "flatten": true }),
      )
      .await?;
    let session = attached
      .get("sessionId")
      .and_then(Value::as_str)
      .ok_or_else(|| Fault::Protocol("no session id".to_string()))?
      .to_string();
    self.session = Some(session);
    self.target_id = target_id.to_string();
    self.world = None;
    self.call("Page.enable", json!({})).await?;
    self
      .call(
        "Emulation.setFocusEmulationEnabled",
        json!({ "enabled": true }),
      )
      .await?;
    let tree = self.call("Page.getFrameTree", json!({})).await?;
    self.frame_id = tree
      .pointer("/frameTree/frame/id")
      .and_then(Value::as_str)
      .unwrap_or(target_id)
      .to_string();
    Ok(())
  }

  /// Move the browser window so only a sliver of it shows at the left edge of
  /// the screen. A minimized window is hidden to the page; one wholly off the
  /// screen is hidden by the system; a window with some of itself on screen is
  /// still a visible, rendering window, and out of the way.
  pub async fn tuck_window(&mut self) -> Option<WindowSpot> {
    let target = self.target_id.clone();
    let found = self
      .browser_call("Browser.getWindowForTarget", json!({ "targetId": target }))
      .await
      .ok()?;
    let window_id = found.get("windowId").and_then(Value::as_i64)?;
    let bounds = found.get("bounds")?.clone();
    if bounds.get("windowState").and_then(Value::as_str) != Some("normal") {
      return None;
    }
    let width = bounds.get("width").and_then(Value::as_i64)?;
    let top = bounds.get("top").and_then(Value::as_i64).unwrap_or(0);
    self
      .browser_call(
        "Browser.setWindowBounds",
        json!({ "windowId": window_id, "bounds": { "left": SLIVER_PX - width, "top": top } }),
      )
      .await
      .ok()?;
    Some(WindowSpot {
      window_id,
      original: bounds,
    })
  }

  /// Put the window back where the person had it, so the next time the
  /// profile opens the browser does not restore a window off to the side.
  pub async fn restore_window(&mut self, spot: WindowSpot) {
    let _ = self
      .browser_call(
        "Browser.setWindowBounds",
        json!({ "windowId": spot.window_id, "bounds": spot.original }),
      )
      .await;
  }

  // --- Reading --------------------------------------------------------------

  async fn create_world(&mut self) -> Result<i64, Fault> {
    let frame_id = self.frame_id.clone();
    let created = self
      .call(
        "Page.createIsolatedWorld",
        json!({ "frameId": frame_id, "worldName": "content" }),
      )
      .await?;
    let context = created
      .get("executionContextId")
      .and_then(Value::as_i64)
      .ok_or_else(|| Fault::Protocol("no execution context".to_string()))?;
    self.world = Some(context);
    Ok(context)
  }

  /// Run a read-only expression in the isolated world and hand back its value.
  async fn evaluate(&mut self, expression: &str) -> Result<Value, Fault> {
    let mut last = Fault::Protocol("the page could not be read".to_string());
    for _ in 0..3 {
      let context = match self.world {
        Some(context) => context,
        None => self.create_world().await?,
      };
      let outcome = self
        .call(
          "Runtime.evaluate",
          json!({
            "expression": expression,
            "contextId": context,
            "returnByValue": true,
            "silent": true,
            "awaitPromise": false,
          }),
        )
        .await;
      match outcome {
        Ok(result) => {
          if result.get("exceptionDetails").is_some() {
            return Err(Fault::Protocol("the read raised".to_string()));
          }
          return Ok(
            result
              .pointer("/result/value")
              .cloned()
              .unwrap_or(Value::Null),
          );
        }
        // The document changed under the read: its world is gone.
        Err(Fault::Protocol(detail)) => {
          self.world = None;
          last = Fault::Protocol(detail);
          self.idle(150).await?;
        }
        Err(other) => return Err(other),
      }
    }
    Err(last)
  }

  /// Read the page. `consent` also looks for a cookie banner's buttons.
  pub async fn observe(&mut self, consent: bool) -> Result<Snapshot, Fault> {
    self.read(&read_expression(consent)).await
  }

  /// Run a read-only `expression` in the isolated world and decode its value.
  pub async fn read<T: serde::de::DeserializeOwned>(
    &mut self,
    expression: &str,
  ) -> Result<T, Fault> {
    let value = self.evaluate(expression).await?;
    serde_json::from_value(value).map_err(|_| Fault::Protocol("unreadable page".to_string()))
  }

  /// The address of the link under `point`, if there is one.
  pub async fn link_at(&mut self, point: Point) -> Result<Option<String>, Fault> {
    let value = self.evaluate(&link_at_expression(point.x, point.y)).await?;
    Ok(value.as_str().map(str::to_string))
  }

  /// The name of the button or link under `point`, if there is one.
  pub async fn control_at(&mut self, point: Point) -> Result<Option<String>, Fault> {
    let value = self
      .evaluate(&control_at_expression(point.x, point.y))
      .await?;
    Ok(value.as_str().map(str::to_string))
  }

  // --- The shop's suggestions -----------------------------------------------

  /// Watch the page's own requests for the shop's search suggestions. The
  /// page asks for them as it is typed into; nothing is requested here.
  pub async fn watch_suggestions(&mut self) -> Result<(), Fault> {
    self
      .call(
        "Network.enable",
        json!({ "maxTotalBufferSize": 4_000_000, "maxResourceBufferSize": 400_000 }),
      )
      .await
      .map(|_| ())
  }

  fn note_network(&mut self, method: &str, params: &Value) {
    let Some(id) = params.get("requestId").and_then(Value::as_str) else {
      return;
    };
    match method {
      "Network.responseReceived" => {
        let url = params
          .pointer("/response/url")
          .and_then(Value::as_str)
          .unwrap_or_default();
        if is_suggest_url(url) && self.suggest_loading.len() < 64 {
          self.suggest_loading.push(id.to_string());
        }
      }
      "Network.loadingFinished" => {
        if let Some(at) = self.suggest_loading.iter().position(|known| known == id) {
          let id = self.suggest_loading.swap_remove(at);
          self.suggest_ready.push(id);
        }
      }
      "Network.loadingFailed" => self.suggest_loading.retain(|known| known != id),
      _ => {}
    }
  }

  /// The bodies of the suggestion responses that finished since the last call.
  pub async fn take_suggestions(&mut self) -> Vec<String> {
    let ready = std::mem::take(&mut self.suggest_ready);
    let mut bodies = Vec::new();
    for id in ready {
      let Ok(reply) = self
        .call("Network.getResponseBody", json!({ "requestId": id }))
        .await
      else {
        continue;
      };
      let body = reply
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
      if reply.get("base64Encoded").and_then(Value::as_bool) == Some(true) {
        use base64::Engine;
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(body) {
          bodies.push(String::from_utf8_lossy(&bytes).into_owned());
        }
      } else {
        bodies.push(body.to_string());
      }
    }
    bodies
  }

  // --- Navigation -----------------------------------------------------------

  /// Wait for the page to finish what a navigation started.
  async fn wait_navigation(&mut self, timeout: Duration, expected: bool) -> Result<bool, Fault> {
    let mut watch = NavigationWatch::new(Some(self.frame_id.clone()), expected);
    watch.on_reply(Instant::now());
    let deadline = Instant::now() + timeout;
    loop {
      self.check()?;
      while let Some(event) = self.events.pop_front() {
        if event.session == self.session {
          watch.on_event(&event.method, Some(&event.params));
        }
      }
      let now = Instant::now();
      if watch.done(now) {
        return Ok(watch.navigated());
      }
      if now >= deadline {
        return Ok(false);
      }
      let wake = watch
        .wake_at(deadline)
        .min(now + Duration::from_millis(200));
      if let Some(message) = self.read_until(wake).await? {
        self.absorb(message);
        self.service_dialog().await;
      }
    }
  }

  /// Open `url` in the page and wait for it to load. A page that does not
  /// finish loading in time is read as it stands.
  pub async fn goto(&mut self, url: &str) -> Result<(), Fault> {
    self.events.clear();
    let result = self.call("Page.navigate", json!({ "url": url })).await?;
    if let Some(error) = result.get("errorText").and_then(Value::as_str) {
      return Err(Fault::Protocol(format!("navigation failed: {error}")));
    }
    self.world = None;
    if !self.wait_navigation(LOAD_TIMEOUT, true).await? {
      let _ = self.call("Page.stopLoading", json!({})).await;
    }
    Ok(())
  }

  /// Go back one entry of the page's history. `false` when there is none.
  pub async fn back(&mut self) -> Result<bool, Fault> {
    let history = self.call("Page.getNavigationHistory", json!({})).await?;
    let index = history
      .get("currentIndex")
      .and_then(Value::as_i64)
      .unwrap_or(0);
    if index <= 0 {
      return Ok(false);
    }
    let Some(entry) = history
      .pointer(&format!("/entries/{}/id", index - 1))
      .and_then(Value::as_i64)
    else {
      return Ok(false);
    };
    self.events.clear();
    self
      .call("Page.navigateToHistoryEntry", json!({ "entryId": entry }))
      .await?;
    self.world = None;
    self.wait_navigation(Duration::from_secs(15), true).await?;
    self.tally.backs += 1;
    Ok(true)
  }

  /// Wait until the attached document is usable.
  async fn wait_ready(&mut self, timeout: Duration) -> Result<(), Fault> {
    let deadline = Instant::now() + timeout;
    loop {
      let ready = self
        .evaluate("document.readyState")
        .await
        .ok()
        .and_then(|value| value.as_str().map(str::to_string));
      if matches!(ready.as_deref(), Some("complete" | "interactive")) || Instant::now() >= deadline
      {
        return Ok(());
      }
      self.idle(400).await?;
    }
  }

  /// Switch to the tab a click opened, when it is on the site; otherwise close
  /// it and stay. Returns whether the new tab was kept.
  pub async fn adopt_new_tab(
    &mut self,
    target_id: &str,
    on_site: impl Fn(&str) -> bool,
  ) -> Result<bool, Fault> {
    let previous = (
      self.session.clone(),
      self.target_id.clone(),
      self.frame_id.clone(),
    );
    self.attach(target_id).await?;
    self.wait_ready(Duration::from_secs(15)).await?;
    let keep = match self.observe(false).await {
      Ok(reading) => reading.is_document() && on_site(&reading.url),
      Err(_) => false,
    };
    if keep {
      let _ = self
        .browser_call("Target.closeTarget", json!({ "targetId": previous.1 }))
        .await;
    } else {
      let _ = self
        .browser_call("Target.closeTarget", json!({ "targetId": target_id }))
        .await;
      self.session = previous.0;
      self.target_id = previous.1;
      self.frame_id = previous.2;
      self.world = None;
    }
    Ok(keep)
  }

  /// Look at the tab a click opened, leaving the current one where it is.
  pub async fn open_tab(&mut self, target_id: &str) -> Result<ParkedTab, Fault> {
    let parked = ParkedTab {
      session: self.session.clone(),
      target_id: self.target_id.clone(),
      frame_id: self.frame_id.clone(),
    };
    if let Err(fault) = self.attach(target_id).await {
      self.restore(&parked);
      let _ = self
        .browser_call("Target.closeTarget", json!({ "targetId": target_id }))
        .await;
      return Err(fault);
    }
    self.wait_ready(Duration::from_secs(15)).await?;
    Ok(parked)
  }

  /// Close the tab being looked at and go back to the one it was opened from.
  pub async fn close_tab(&mut self, parked: ParkedTab) -> Result<(), Fault> {
    let current = self.target_id.clone();
    let _ = self
      .browser_call("Target.closeTarget", json!({ "targetId": current }))
      .await;
    self.restore(&parked);
    self
      .browser_call(
        "Target.activateTarget",
        json!({ "targetId": parked.target_id }),
      )
      .await
      .map(|_| ())
  }

  fn restore(&mut self, parked: &ParkedTab) {
    self.suggest_loading.clear();
    self.suggest_ready.clear();
    self.session = parked.session.clone();
    self.target_id = parked.target_id.clone();
    self.frame_id = parked.frame_id.clone();
    self.world = None;
  }

  // --- Keyboard -------------------------------------------------------------

  /// One key event. `ident` is the key's DOM `key`, its `code` and its
  /// virtual key code.
  async fn key_event(
    &mut self,
    kind: &str,
    ident: (&str, &str, u32),
    modifiers: u32,
    text: Option<&str>,
    commands: &[&str],
  ) -> Result<(), Fault> {
    let (key, code, vk) = ident;
    let mut params = json!({
      "type": kind,
      "key": key,
      "code": code,
      "windowsVirtualKeyCode": vk,
      "nativeVirtualKeyCode": vk,
      "modifiers": modifiers,
    });
    if let Some(text) = text {
      params["text"] = json!(text);
      params["unmodifiedText"] = json!(text);
    }
    if !commands.is_empty() {
      params["commands"] = json!(commands);
    }
    self
      .call("Input.dispatchKeyEvent", params)
      .await
      .map(|_| ())
  }

  /// Press and let go of a named key such as Enter or Backspace.
  pub async fn press_key(&mut self, key: Key, hold_ms: u64) -> Result<(), Fault> {
    let text = (key == ENTER).then_some("\r");
    if key == ENTER {
      self.events.clear();
    }
    self
      .key_event("keyDown", (key.key, key.code, key.vk), 0, text, &[])
      .await?;
    tokio::time::sleep(Duration::from_millis(hold_ms)).await;
    self
      .key_event("keyUp", (key.key, key.code, key.vk), 0, None, &[])
      .await
  }

  /// Select everything in the focused field with the platform's shortcut.
  pub async fn select_all(&mut self, mac: bool, hold_ms: u64) -> Result<(), Fault> {
    let (modifier, flag) = if mac { (META, 4) } else { (CONTROL, 2) };
    self
      .key_event(
        "rawKeyDown",
        (modifier.key, modifier.code, modifier.vk),
        flag,
        None,
        &[],
      )
      .await?;
    tokio::time::sleep(Duration::from_millis(hold_ms / 2 + 40)).await;
    self
      .key_event("rawKeyDown", ("a", "KeyA", 65), flag, None, &["selectAll"])
      .await?;
    tokio::time::sleep(Duration::from_millis(hold_ms)).await;
    self
      .key_event("keyUp", ("a", "KeyA", 65), flag, None, &[])
      .await?;
    tokio::time::sleep(Duration::from_millis(hold_ms / 2 + 30)).await;
    self
      .key_event(
        "keyUp",
        (modifier.key, modifier.code, modifier.vk),
        0,
        None,
        &[],
      )
      .await
  }

  /// Type into the focused field as planned.
  pub async fn type_keys(&mut self, plan: &[KeyPress]) -> Result<(), Fault> {
    let started = Instant::now();
    for (index, press) in plan.iter().enumerate() {
      tokio::time::sleep_until(started + Duration::from_secs_f64(press.at_ms / 1_000.0)).await;
      if index % 8 == 0 {
        self.check()?;
      }
      let hold = Duration::from_secs_f64(press.hold_ms / 1_000.0);
      match press.stroke {
        Stroke::Backspace => {
          let b = BACKSPACE;
          self
            .key_event("rawKeyDown", (b.key, b.code, b.vk), 0, None, &[])
            .await?;
          tokio::time::sleep(hold).await;
          self
            .key_event("keyUp", (b.key, b.code, b.vk), 0, None, &[])
            .await?;
        }
        Stroke::Char(c) => {
          let Some((code, vk, shift)) = key_for(c) else {
            self
              .call("Input.insertText", json!({ "text": c.to_string() }))
              .await?;
            continue;
          };
          let typed = c.to_string();
          let modifiers = if shift { 8 } else { 0 };
          if shift {
            let s = SHIFT;
            self
              .key_event("rawKeyDown", (s.key, s.code, s.vk), 8, None, &[])
              .await?;
            tokio::time::sleep(Duration::from_millis(35)).await;
          }
          self
            .key_event("keyDown", (&typed, code, vk), modifiers, Some(&typed), &[])
            .await?;
          tokio::time::sleep(hold).await;
          self
            .key_event("keyUp", (&typed, code, vk), modifiers, None, &[])
            .await?;
          if shift {
            let s = SHIFT;
            tokio::time::sleep(Duration::from_millis(25)).await;
            self
              .key_event("keyUp", (s.key, s.code, s.vk), 0, None, &[])
              .await?;
          }
        }
      }
    }
    Ok(())
  }

  // --- Input ----------------------------------------------------------------

  async fn mouse(
    &mut self,
    kind: &str,
    at: Point,
    button: &str,
    buttons: u32,
    click_count: u32,
  ) -> Result<(), Fault> {
    let mut params = json!({
      "type": kind,
      "x": at.x,
      "y": at.y,
      "button": button,
      "buttons": buttons,
      "pointerType": "mouse",
      "modifiers": 0,
    });
    if click_count > 0 {
      params["clickCount"] = json!(click_count);
    }
    self
      .call("Input.dispatchMouseEvent", params)
      .await
      .map(|_| ())
  }

  /// The pointer's place, bringing it into the window first if the page has
  /// not yet seen one.
  pub async fn ensure_pointer(
    &mut self,
    view: (f64, f64),
    dpr: f64,
    d: &mut impl Draw,
  ) -> Result<Point, Fault> {
    if let Some(at) = self.pointer {
      return Ok(at);
    }
    let entry = entry_point(view, dpr, d);
    self.mouse("mouseMoved", entry, "none", 0, 0).await?;
    self.pointer = Some(entry);
    Ok(entry)
  }

  /// Replay a planned move at its own pace.
  pub async fn play_move(&mut self, plan: &MovePlan) -> Result<(), Fault> {
    let started = Instant::now();
    for (index, step) in plan.steps.iter().enumerate() {
      tokio::time::sleep_until(started + Duration::from_secs_f64(step.at_ms / 1_000.0)).await;
      if index % 24 == 0 {
        self.check()?;
      }
      let at = Point::new(step.x, step.y);
      self.mouse("mouseMoved", at, "none", 0, 0).await?;
      self.pointer = Some(at);
    }
    self.tally.moves += 1;
    Ok(())
  }

  /// Move the pointer to `to`, a target about `width` across.
  pub async fn move_to(
    &mut self,
    to: Point,
    width: f64,
    view: (f64, f64),
    dpr: f64,
    d: &mut impl Draw,
  ) -> Result<(), Fault> {
    let from = self.ensure_pointer(view, dpr, d).await?;
    let plan = plan_move(from, to, width, dpr, d);
    self.play_move(&plan).await
  }

  /// A small drift of the pointer, as a reader's hand gives.
  pub async fn drift(
    &mut self,
    view: (f64, f64),
    dpr: f64,
    d: &mut impl Draw,
  ) -> Result<(), Fault> {
    let from = self.ensure_pointer(view, dpr, d).await?;
    let to = drift_target(from, view, d);
    self.move_to(to, 40.0, view, dpr, d).await
  }

  /// Press and release the left button where the pointer is.
  pub async fn click(&mut self, plan: &ClickPlan) -> Result<(), Fault> {
    let Some(at) = self.pointer else {
      return Err(Fault::Protocol("no pointer".to_string()));
    };
    self.idle(plan.press_after_ms).await?;
    self.events.clear();
    self.mouse("mousePressed", at, "left", 1, 1).await?;
    let pressed = Instant::now();
    let mut release_at = at;
    if let Some(nudge) = plan.nudge {
      tokio::time::sleep_until(pressed + Duration::from_millis(nudge.at_ms)).await;
      release_at = Point::new(at.x + nudge.dx, at.y + nudge.dy);
      self.mouse("mouseMoved", release_at, "left", 1, 0).await?;
      self.pointer = Some(release_at);
    }
    tokio::time::sleep_until(pressed + Duration::from_millis(plan.hold_ms)).await;
    self
      .mouse("mouseReleased", release_at, "left", 0, 1)
      .await?;
    self.tally.clicks += 1;
    Ok(())
  }

  /// What the click just made did to the page: loaded something, opened a tab,
  /// or nothing.
  pub async fn settle_after_click(&mut self, timeout: Duration) -> Result<ClickOutcome, Fault> {
    let mut watch = NavigationWatch::new(Some(self.frame_id.clone()), false);
    watch.on_reply(Instant::now());
    let deadline = Instant::now() + timeout;
    let mut new_tab = None;
    loop {
      self.check()?;
      while let Some(event) = self.events.pop_front() {
        if event.session == self.session {
          watch.on_event(&event.method, Some(&event.params));
        }
        if event.method == "Target.targetCreated" {
          if let Some(info) = event.params.get("targetInfo") {
            let opened_here =
              info.get("openerId").and_then(Value::as_str) == Some(self.target_id.as_str());
            if opened_here && info.get("type").and_then(Value::as_str) == Some("page") {
              new_tab = info
                .get("targetId")
                .and_then(Value::as_str)
                .map(str::to_string);
            }
          }
        }
      }
      let now = Instant::now();
      if watch.done(now) || now >= deadline {
        break;
      }
      let wake = watch
        .wake_at(deadline)
        .min(now + Duration::from_millis(200));
      if let Some(message) = self.read_until(wake).await? {
        self.absorb(message);
        self.service_dialog().await;
      }
    }
    if watch.navigated() {
      self.world = None;
      Ok(ClickOutcome::Navigated)
    } else if let Some(id) = new_tab {
      Ok(ClickOutcome::NewTab(id))
    } else if Instant::now() >= deadline {
      Ok(ClickOutcome::TimedOut)
    } else {
      Ok(ClickOutcome::Nothing)
    }
  }

  /// Turn the wheel as planned, with the pointer where it is.
  pub async fn scroll(&mut self, plan: &ScrollPlan) -> Result<(), Fault> {
    let Some(at) = self.pointer else {
      return Err(Fault::Protocol("no pointer".to_string()));
    };
    let started = Instant::now();
    for (index, event) in plan.events.iter().enumerate() {
      tokio::time::sleep_until(started + Duration::from_secs_f64(event.at_ms / 1_000.0)).await;
      if index % 24 == 0 {
        self.check()?;
      }
      self
        .call(
          "Input.dispatchMouseEvent",
          json!({
            "type": "mouseWheel",
            "x": at.x,
            "y": at.y,
            "deltaX": 0,
            "deltaY": event.delta_y,
            "button": "none",
            "buttons": 0,
            "pointerType": "mouse",
            "modifiers": 0,
          }),
        )
        .await?;
    }
    self.tally.scrolls += 1;
    Ok(())
  }
}

/// Whether `url` is the shop's search-suggestion endpoint.
fn is_suggest_url(url: &str) -> bool {
  url::Url::parse(url).is_ok_and(|parsed| {
    parsed.path() == "/autosug"
      && parsed.host_str().is_some_and(|host| {
        host == "ebay.com" || host.ends_with(".ebay.com") || host.contains(".ebay.")
      })
  })
}

#[cfg(test)]
mod tests {
  use super::is_suggest_url;

  #[test]
  fn only_the_shops_suggestions_are_watched() {
    assert!(is_suggest_url(
      "https://www.ebay.com/autosug?kwd=iphone&_richres=1"
    ));
    assert!(is_suggest_url("https://www.ebay.co.uk/autosug?kwd=x"));
    assert!(!is_suggest_url(
      "https://www.ebay.com/sch/i.html?_nkw=autosug"
    ));
    assert!(!is_suggest_url("https://autosug.example.com/autosug"));
  }
}
