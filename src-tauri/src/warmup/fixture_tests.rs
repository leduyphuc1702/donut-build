//! The warm-up against a site served from this machine, in a real Chromium.
//!
//! The fixture's own pages listen to pointer, wheel and scroll events and post
//! what they saw back to the server, so the test reads the visit the way a
//! site's analytics would: whether each event is trusted, how far the pointer
//! jumps between two events, how long a button stays down, which pages were
//! requested and whether the cookie banner got its answer.
//!
//! It needs a Chromium and a desktop session, so it is ignored by default:
//!
//! `cargo test --lib warmup::fixture_tests -- --ignored --nocapture`
//!
//! `DONUT_E2E_CHROMIUM` names the binary when it is not in a usual place.

use super::*;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Seen {
  events: Vec<Ev>,
  hits: Vec<String>,
  consent: Vec<String>,
}

type Shared = Arc<Mutex<Seen>>;

/// One event as the fixture's listener recorded it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct Ev {
  t: String,
  x: f64,
  y: f64,
  /// `isTrusted`.
  tr: bool,
  /// Wall-clock milliseconds.
  ts: f64,
  dy: f64,
  /// For a `win` report: the page is visible to the system.
  vis: bool,
}

const LOGGER: &str = r#"<script>
(function () {
  var buffer = [];
  function now() { return Math.round((performance.timeOrigin + performance.now()) * 10) / 10; }
  function push(e) {
    buffer.push({ t: e.type, x: e.clientX, y: e.clientY, tr: e.isTrusted, ts: now(), dy: e.deltaY || 0 });
  }
  ['mousemove', 'mousedown', 'mouseup', 'click', 'wheel'].forEach(function (name) {
    document.addEventListener(name, push, { capture: true, passive: true });
  });
  document.addEventListener('scroll', function () {
    buffer.push({ t: 'scroll', x: 0, y: window.scrollY, tr: true, ts: now(), dy: 0 });
  }, { passive: true });
  function flush() {
    buffer.push({ t: 'win', x: window.screenX, y: window.screenY, vis: document.visibilityState === 'visible', ts: now(), dy: 0 });
    var sent = buffer; buffer = [];
    fetch('/log', { method: 'POST', body: JSON.stringify(sent), keepalive: true });
  }
  setInterval(flush, 300);
  window.addEventListener('pagehide', flush);
})();
</script>"#;

const BANNER: &str = r#"<div id="cookie-banner" role="dialog" aria-label="Cookie consent" style="display:none;position:fixed;left:0;right:0;bottom:0;background:#fff;border-top:1px solid #bbb;padding:14px 24px;z-index:9999;box-shadow:0 -2px 8px rgba(0,0,0,.15)">
<p style="margin:0 0 10px">We use cookies to personalise content and to analyse traffic. See our privacy policy.</p>
<button id="cb-reject" style="padding:8px 18px;margin-right:10px">Reject all</button>
<button id="cb-settings" style="padding:8px 18px;margin-right:10px">Manage preferences</button>
<button id="cb-accept" style="padding:8px 22px;background:#1a73e8;color:#fff;border:0">Accept all cookies</button>
</div>
<script>
(function () {
  var banner = document.getElementById('cookie-banner');
  if (document.cookie.indexOf('consent=') >= 0) { banner.remove(); return; }
  setTimeout(function () { banner.style.display = 'block'; }, 700);
  function answer(value) {
    document.cookie = 'consent=' + value + ';path=/;max-age=86400';
    banner.remove();
    fetch('/consent', { method: 'POST', body: value });
  }
  document.getElementById('cb-accept').onclick = function () { answer('accept'); };
  document.getElementById('cb-reject').onclick = function () { answer('reject'); };
  document.getElementById('cb-settings').onclick = function () { answer('settings'); };
})();
</script>"#;

const TEMPLATE: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>@@TITLE@@</title>
<style>
body { margin: 0; font: 17px/1.6 Georgia, serif; color: #222; }
header, footer { background: #f2f2f2; padding: 14px 24px; }
nav a, footer a { margin-right: 18px; }
main { max-width: 760px; margin: 0 auto; padding: 24px; }
p { margin: 0 0 18px; }
li { margin-bottom: 10px; }
</style>
@@LOGGER@@</head><body>
<header><nav><a href="/">Home</a><a href="/about">About</a><a href="/cart">Cart (0)</a><a href="/logout">Log out</a></nav></header>
<main>@@BODY@@</main>
<footer><a href="/about">About us</a><a href="http://localhost:@@PORT@@/ext">Partner site</a><a href="mailto:hi@example.test">Email us</a><a href="tel:+10000000">Call us</a><a href="/files/report.pdf" download>Download the report</a></footer>
@@BANNER@@</body></html>"#;

const SENTENCES: [&str; 8] = [
  "The first frost came late this year, and the beds along the south wall were still green when the others had turned.",
  "Most people plant too early, because the weather in spring is kind for a week and then it is not.",
  "A good soil is dark, crumbles in the hand, and smells of rain, which is something no bag of compost can promise.",
  "Water in the morning, if you can, so that the leaves are dry by the time the evening damp arrives.",
  "Neighbours trade seedlings over the fence, and half the garden on any street is a gift from someone else.",
  "Beans climb anything, but they prefer a string they can find on their own, a little to the left of where you expect.",
  "The old varieties are smaller and odder than the ones in the shops, and nearly all of them taste better.",
  "By the end of summer the work is mostly waiting, and the waiting is the part that nobody photographs.",
];

fn prose(seed: usize, paragraphs: usize, links: &[(&str, &str)]) -> String {
  let mut out = String::new();
  for index in 0..paragraphs {
    out.push_str("<p>");
    for step in 0..4 {
      out.push_str(SENTENCES[(seed + index * 3 + step) % SENTENCES.len()]);
      out.push(' ');
    }
    if let Some((href, text)) = links.get(index % links.len().max(1)) {
      out.push_str(&format!("Read more in <a href=\"{href}\">{text}</a>. "));
    }
    out.push_str("</p>");
  }
  out
}

fn page(port: u16, title: &str, body: &str) -> Html<String> {
  Html(
    TEMPLATE
      .replace("@@TITLE@@", title)
      .replace("@@LOGGER@@", LOGGER)
      .replace("@@BANNER@@", BANNER)
      .replace("@@PORT@@", &port.to_string())
      .replace("@@BODY@@", body),
  )
}

async fn serve_page(State((seen, port)): State<(Shared, u16)>, uri: Uri) -> Response {
  let path = uri.path().to_string();
  if path == "/favicon.ico" {
    return StatusCode::NOT_FOUND.into_response();
  }
  seen.lock().unwrap().hits.push(path.clone());
  let links = [
    ("/a", "the harvest report"),
    ("/b", "our guide to seed saving"),
    ("/c", "notes on winter beds"),
    ("/", "the front page"),
  ];
  let (title, body) = match path.as_str() {
    "/" => (
      "Garden journal",
      format!(
        "<h1>Garden journal</h1>{}<h2>Related reading</h2><ul><li><a href=\"/a\">The harvest report</a></li><li><a href=\"/b\">A guide to seed saving</a></li><li><a href=\"/c\">Notes on winter beds</a></li></ul>{}",
        prose(0, 6, &links),
        prose(5, 4, &links)
      ),
    ),
    "/a" | "/b" | "/c" | "/about" => (
      "Article",
      format!(
        "<h1>Article {}</h1>{}<p>Back to <a href=\"/\">the front page</a>.</p>",
        path.trim_start_matches('/'),
        prose(path.len() + path.as_bytes()[path.len() - 1] as usize, 8, &links)
      ),
    ),
    "/cart" | "/logout" | "/ext" => ("Off limits", "<h1>Off limits</h1>".to_string()),
    _ => return StatusCode::NOT_FOUND.into_response(),
  };
  page(port, title, &body).into_response()
}

async fn log_events(State((seen, _)): State<(Shared, u16)>, body: String) -> StatusCode {
  if let Ok(batch) = serde_json::from_str::<Vec<Ev>>(&body) {
    seen.lock().unwrap().events.extend(batch);
  }
  StatusCode::NO_CONTENT
}

async fn log_consent(State((seen, _)): State<(Shared, u16)>, body: String) -> StatusCode {
  seen.lock().unwrap().consent.push(body);
  StatusCode::NO_CONTENT
}

async fn start_fixture() -> (u16, Shared) {
  let seen = Shared::default();
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let port = listener.local_addr().unwrap().port();
  let app = Router::new()
    .route("/log", post(log_events))
    .route("/consent", post(log_consent))
    .fallback(serve_page)
    .with_state((seen.clone(), port));
  tokio::spawn(async move {
    let _ = axum::serve(listener, app).await;
  });
  (port, seen)
}

fn chromium() -> Option<PathBuf> {
  if let Ok(path) = std::env::var("DONUT_E2E_CHROMIUM") {
    return Some(PathBuf::from(path));
  }
  [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
  ]
  .iter()
  .map(PathBuf::from)
  .find(|path| path.exists())
}

struct Browser(std::process::Child);

impl Drop for Browser {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

fn free_port() -> u16 {
  std::net::TcpListener::bind("127.0.0.1:0")
    .unwrap()
    .local_addr()
    .unwrap()
    .port()
}

/// What a site's analytics could tell about the visit.
#[derive(Debug, Default)]
struct Stats {
  untrusted: usize,
  moves: usize,
  /// The largest distance between two moves less than 120 ms apart.
  max_jump: f64,
  clicks: usize,
  holds_ms: Vec<f64>,
  wheel: Vec<f64>,
  scrolls: usize,
  /// Window reports in which the page was hidden by the system.
  hidden_reports: usize,
  /// Where the window's left edge was at its farthest left, and at the end.
  leftmost_screen_x: f64,
  final_screen_x: f64,
}

/// Moves less than this far apart in time are one stroke of the pointer.
const STROKE_GAP_MS: f64 = 120.0;

fn analyse(events: &[Ev]) -> Stats {
  let mut events = events.to_vec();
  events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
  let mut stats = Stats {
    untrusted: events
      .iter()
      .filter(|event| event.t != "win" && !event.tr)
      .count(),
    leftmost_screen_x: f64::MAX,
    ..Stats::default()
  };
  let mut last_move: Option<&Ev> = None;
  let mut pressed_at: Option<f64> = None;
  for event in &events {
    match event.t.as_str() {
      "win" => {
        stats.hidden_reports += usize::from(!event.vis);
        stats.leftmost_screen_x = stats.leftmost_screen_x.min(event.x);
        stats.final_screen_x = event.x;
      }
      "mousemove" => {
        stats.moves += 1;
        if let Some(before) = last_move {
          if event.ts - before.ts <= STROKE_GAP_MS {
            let jump = (event.x - before.x).hypot(event.y - before.y);
            stats.max_jump = stats.max_jump.max(jump);
          }
        }
        last_move = Some(event);
      }
      "mousedown" => pressed_at = Some(event.ts),
      "mouseup" => {
        if let Some(down) = pressed_at.take() {
          stats.holds_ms.push(event.ts - down);
        }
      }
      "click" => stats.clicks += 1,
      "wheel" => stats.wheel.push(event.dy),
      "scroll" => stats.scrolls += 1,
      _ => {}
    }
  }
  stats
}

fn ev(t: &str, x: f64, y: f64, ts: f64) -> Ev {
  Ev {
    t: t.into(),
    x,
    y,
    tr: true,
    ts,
    dy: 0.0,
    vis: false,
  }
}

#[test]
fn the_analysis_sees_a_teleport_and_a_scripted_click() {
  let mut events = vec![ev("mousemove", 10.0, 10.0, 0.0)];
  for step in 1..=20 {
    events.push(ev(
      "mousemove",
      10.0 + f64::from(step) * 12.0,
      10.0,
      f64::from(step) * 10.0,
    ));
  }
  assert!(analyse(&events).max_jump <= 12.5);
  events.push(ev("mousemove", 900.0, 600.0, 215.0));
  assert!(analyse(&events).max_jump > 500.0);

  let mut script = ev("click", 5.0, 5.0, 300.0);
  script.tr = false;
  let stats = analyse(&[script]);
  assert_eq!(stats.untrusted, 1);
  assert!(stats.holds_ms.is_empty());

  let held = analyse(&[
    ev("mousedown", 1.0, 1.0, 100.0),
    ev("mouseup", 1.0, 1.0, 171.0),
  ]);
  assert_eq!(held.holds_ms, vec![71.0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a Chromium and a desktop session"]
async fn a_visit_to_a_local_site_looks_like_a_person_did_it() {
  let Some(binary) = chromium() else {
    eprintln!("No Chromium found, set DONUT_E2E_CHROMIUM");
    return;
  };
  let (http_port, seen) = start_fixture().await;
  let cdp_port = free_port();
  let profile = tempfile::tempdir().unwrap();
  let child = std::process::Command::new(binary)
    .args([
      format!("--remote-debugging-port={cdp_port}"),
      "--remote-debugging-address=127.0.0.1".to_string(),
      format!("--user-data-dir={}", profile.path().display()),
      "--no-first-run".to_string(),
      "--no-default-browser-check".to_string(),
      "--window-size=1280,800".to_string(),
      "--window-position=80,60".to_string(),
      "about:blank".to_string(),
    ])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("start the browser");
  let _browser = Browser(child);

  let mut connection = None;
  for _ in 0..40 {
    if let Some(found) = crate::cdp_target::connect_browser_at(cdp_port).await {
      connection = Some(found);
      break;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
  }
  let connection = connection.expect("the browser's DevTools port opens");

  let spec = RunSpec {
    sites: vec![format!("http://127.0.0.1:{http_port}/")],
    budget: Duration::from_secs(90),
    style: ScrollStyle::Trackpad,
    topic: None,
  };
  let started = std::time::Instant::now();
  let report = run(connection, spec, Arc::new(|| false))
    .await
    .expect("the run is set up");
  // The page posts what it saw every 300 ms.
  tokio::time::sleep(Duration::from_millis(1_500)).await;

  let seen = seen.lock().unwrap();
  let stats = analyse(&seen.events);
  let mut holds = stats.holds_ms.clone();
  holds.sort_by(f64::total_cmp);
  eprintln!("run took {:?}", started.elapsed());
  eprintln!("report {report:?}");
  eprintln!("pages requested {:?}", seen.hits);
  eprintln!("consent answers {:?}", seen.consent);
  eprintln!(
    "page saw: {} events, untrusted {}, moves {}, largest jump {:.1}px, clicks {}, holds {:?} ms, wheel events {}, scroll events {}",
    seen.events.len(),
    stats.untrusted,
    stats.moves,
    stats.max_jump,
    stats.clicks,
    holds,
    stats.wheel.len(),
    stats.scrolls
  );
  eprintln!(
    "window: hidden in {} reports, leftmost x {}, final x {}",
    stats.hidden_reports, stats.leftmost_screen_x, stats.final_screen_x
  );

  assert_eq!(report.visited, 1);
  assert_eq!(report.failed, 0);
  assert_eq!(
    stats.untrusted, 0,
    "every event the page saw was a real one"
  );
  assert!(
    stats.moves >= 80,
    "a pointer that travels: {} moves",
    stats.moves
  );
  assert!(stats.max_jump <= 140.0, "no teleport: {}px", stats.max_jump);
  assert!(
    !stats.wheel.is_empty(),
    "the page was scrolled with the wheel"
  );
  assert!(
    stats.wheel.iter().any(|delta| *delta != stats.wheel[0]),
    "wheel deltas vary"
  );
  assert!(
    holds.iter().all(|hold| (50.0..=220.0).contains(hold)),
    "button held a human while: {holds:?}"
  );
  assert_eq!(
    stats.hidden_reports, 0,
    "the page was never hidden by the system"
  );
  assert!(
    stats.leftmost_screen_x < 0.0,
    "the window was moved out of the way"
  );
  assert!(
    (stats.final_screen_x - 80.0).abs() <= 2.0,
    "the window is back where it was: {}",
    stats.final_screen_x
  );
  assert_eq!(
    seen.consent,
    vec!["accept".to_string()],
    "the banner was accepted, once"
  );
  assert_eq!(report.tally.consents, 1);
  for forbidden in ["/cart", "/logout", "/ext", "/files/report.pdf"] {
    assert!(
      !seen.hits.iter().any(|hit| hit == forbidden),
      "{forbidden} must never be opened: {:?}",
      seen.hits
    );
  }
}

/// The eBay topic on the real sites. Run by hand: it browses Google and eBay
/// from this machine for `DONUT_WARMUP_TOPIC_SECS` (default 240).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "browses google.com and ebay.com; needs a Chromium and a desktop session"]
async fn a_topic_run_shops_on_ebay() {
  let _ = env_logger::builder().is_test(true).try_init();
  let Some(binary) = chromium() else {
    eprintln!("No Chromium found, set DONUT_E2E_CHROMIUM");
    return;
  };
  let secs = std::env::var("DONUT_WARMUP_TOPIC_SECS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(240);
  let cdp_port = free_port();
  let profile = tempfile::tempdir().unwrap();
  let child = std::process::Command::new(binary)
    .args([
      format!("--remote-debugging-port={cdp_port}"),
      "--remote-debugging-address=127.0.0.1".to_string(),
      format!("--user-data-dir={}", profile.path().display()),
      "--no-first-run".to_string(),
      "--no-default-browser-check".to_string(),
      // A real Keychain prompt would hold the first HTTPS load.
      "--use-mock-keychain".to_string(),
      "--password-store=basic".to_string(),
      "--window-size=1280,800".to_string(),
      "--window-position=80,60".to_string(),
      "about:blank".to_string(),
    ])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("start the browser");
  let _browser = Browser(child);

  let mut connection = None;
  for _ in 0..40 {
    if let Some(found) = crate::cdp_target::connect_browser_at(cdp_port).await {
      connection = Some(found);
      break;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
  }
  let connection = connection.expect("the browser's DevTools port opens");

  let spec = RunSpec {
    sites: vec![super::topic_home("ebay").to_string()],
    budget: Duration::from_secs(secs),
    style: ScrollStyle::Trackpad,
    topic: Some(super::TopicSpec {
      site: "ebay".to_string(),
      keyword: std::env::var("DONUT_WARMUP_TOPIC").unwrap_or_else(|_| "iphone".to_string()),
      learned: Vec::new(),
      mac: cfg!(target_os = "macos"),
    }),
  };
  let started = std::time::Instant::now();
  let report = run(connection, spec, Arc::new(|| false))
    .await
    .expect("the run is set up");
  eprintln!("run took {:?}", started.elapsed());
  eprintln!("report {report:?}");
  assert!(!report.challenged, "a bot check stopped the run");
  assert_eq!(report.visited, 1, "eBay was reached");
  assert!(report.tally.searches >= 2, "Google and eBay were searched");
  assert!(report.tally.items >= 1, "a listing was opened");
  assert!(!report.learned.is_empty(), "eBay's suggestions were kept");
}
