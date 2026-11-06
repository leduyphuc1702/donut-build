//! A warm-up that browses like a person.
//!
//! Cookies and site storage are only worth what they were earned by: a profile
//! that has really visited a site, read it and accepted its banner is a
//! returning visitor, and one that loaded a page and left is not. Sites also
//! watch how a visit behaves, so the visit has to look like a person's: a
//! pointer that travels, a hand that scrolls, time that is spent unevenly,
//! links that a reader would follow. This module does that for any site, with
//! no script per site:
//!
//! - `pointer`, `scroll`, `rhythm`, `targets`, `consent` and `text` are pure
//!   planners and matchers. They draw from a random source they are handed, so
//!   each is tested on its own;
//! - `page` reads what is on screen with one read-only script in an isolated
//!   world;
//! - `driver` sends the planned input through `Input.dispatchMouseEvent` and
//!   reads the replies, over standard DevTools only (no `Runtime.enable`, no
//!   evaluation in the page's own world, no click that is not a mouse event);
//! - `visit` chooses what to do on a site, from what the page shows.
//!
//! A run given a topic (`TopicSpec`) is a shopper instead: `topic` searches the
//! keyword on Google, opens the shop's results, searches the shop for what it
//! read, keeps the searches the shop suggests, and opens listings, typing on
//! the keyboard through `keys` and reading the shop through `shop`.

mod consent;
mod driver;
mod keys;
mod page;
mod pointer;
mod rhythm;
mod scroll;
mod shop;
mod targets;
mod text;
mod topic;
mod visit;

#[cfg(test)]
mod fixture_tests;

pub use driver::{Fault, StopFn};
pub use scroll::ScrollStyle;

use crate::cdp_target::CdpConnection;
use rhythm::{action_gap_ms, ThreadDraw};
use std::time::Duration;
use tokio::time::Instant;

/// The smallest share of time a site is started for.
const MIN_SHARE: Duration = Duration::from_secs(8);
/// The most one site is given, however few sites there are.
const MAX_SHARE: Duration = Duration::from_secs(12 * 60);

/// What the run did, as counts. Never an address or a name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
  /// Pages looked at, landing pages and followed links.
  pub pages: u32,
  /// Mouse clicks, banner buttons and links.
  pub clicks: u32,
  /// Bursts of the wheel.
  pub scrolls: u32,
  /// Pointer movements.
  pub moves: u32,
  /// Cookie banners accepted.
  pub consents: u32,
  /// Times the visitor went back.
  pub backs: u32,
  /// Links the visitor set out to follow and did not: none fair in reach, one
  /// that moved away, a click that went nowhere on the site.
  pub link_misses: u32,
  /// Searches typed and sent, on the search engine and in the shop.
  pub searches: u32,
  /// Listings opened and read.
  pub items: u32,
  /// Pictures of a listing looked at.
  pub pictures: u32,
}

/// The shops a topic run knows how to browse.
pub const TOPIC_SITES: &[&str] = &["ebay"];

/// The address a topic run's shop is entered at. eBay is the only shop yet.
pub fn topic_home(_site: &str) -> &'static str {
  "https://www.ebay.com/"
}

/// Whether the persona claims macOS, whose shortcuts use Command.
pub fn is_mac_os(os: &str) -> bool {
  os.eq_ignore_ascii_case("macos")
}

/// A run about one thing: the shop, the keyword, and the searches earlier runs
/// collected.
#[derive(Clone, Debug)]
pub struct TopicSpec {
  pub site: String,
  pub keyword: String,
  pub learned: Vec<String>,
  pub mac: bool,
}

pub struct RunSpec {
  pub sites: Vec<String>,
  /// All the time the run may take.
  pub budget: Duration,
  /// The hardware the visitor scrolls with.
  pub style: ScrollStyle,
  /// Shop for this instead of visiting `sites`.
  pub topic: Option<TopicSpec>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunReport {
  /// Sites whose first page loaded.
  pub visited: u32,
  /// Sites that could not be opened.
  pub failed: u32,
  /// Sites not reached: the time ran out, or the run was stopped.
  pub skipped: u32,
  pub tally: Tally,
  /// Searches the shop suggested in this run, on the topic.
  pub learned: Vec<String>,
  /// A site showed a bot check, and the run stopped there.
  pub challenged: bool,
}

/// Browse `spec.sites` for up to `spec.budget` in the browser `connection`
/// reaches. `stop` is asked often and ends the run at the next safe moment.
/// `Err` only when the browser could not be set up at all.
pub async fn run(
  connection: CdpConnection,
  spec: RunSpec,
  stop: StopFn,
) -> Result<RunReport, Fault> {
  let until = Instant::now() + spec.budget;
  let mut driver = driver::Driver::new(connection, stop, until);
  let mut draw = ThreadDraw;

  let setup = async {
    driver.watch_targets().await?;
    let target = driver.working_target().await?;
    driver.attach(&target).await
  }
  .await;
  if let Err(fault) = setup {
    driver.close().await;
    return Err(fault);
  }
  let window = driver.tuck_window().await;

  let total = spec.sites.len() as u32;
  let mut report = RunReport::default();
  if let Some(topic) = &spec.topic {
    let outcome = topic::shop(&mut driver, &mut draw, spec.style, topic).await;
    report.visited = u32::from(outcome.loaded);
    report.failed = u32::from(!outcome.loaded && !driver.stopped());
    report.skipped = total.saturating_sub(report.visited + report.failed);
    report.learned = outcome.learned;
    report.challenged = outcome.challenged;
    if let Some(fault) = outcome.fault {
      log::debug!("Warm-up topic ended early reason=\"{fault}\"");
    }
  }
  let sites = if spec.topic.is_some() {
    &[][..]
  } else {
    &spec.sites[..]
  };
  for (index, site) in sites.iter().enumerate() {
    let remaining = driver.remaining();
    if driver.stopped() || remaining < MIN_SHARE {
      break;
    }
    let left = (sites.len() - index) as f64;
    let share = Duration::from_secs_f64(remaining.as_secs_f64() / left).clamp(MIN_SHARE, MAX_SHARE);
    let result = visit::warm_site(&mut driver, &mut draw, spec.style, site, share).await;
    if result.loaded {
      report.visited += 1;
    } else if !matches!(result.fault, Some(Fault::Stopped)) {
      report.failed += 1;
    }
    if let Some(fault) = result.fault {
      log::debug!("Warm-up visit ended early reason=\"{fault}\"");
      break;
    }
    // A person does not open the next site the instant the last one is done.
    let gap = action_gap_ms(&mut draw) * 2;
    if driver.idle(gap).await.is_err() {
      break;
    }
  }
  if spec.topic.is_none() {
    report.skipped = total.saturating_sub(report.visited + report.failed);
  }

  if let Some(spot) = window {
    driver.restore_window(spot).await;
  }
  report.tally = driver.tally;
  driver.close().await;
  Ok(report)
}
