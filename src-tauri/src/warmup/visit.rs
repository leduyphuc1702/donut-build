//! A visit to one site, the way a person who has the time makes one.
//!
//! The visitor opens the site's address, lets the page load and takes it in,
//! deals with a cookie banner if one is up, then reads: scrolls in bursts,
//! sometimes back up, rests the pointer on a heading or a picture, lets it
//! drift, stops to read what is on screen. After a while, on most pages, it
//! picks a link of the same site that a reader would, moves to it, hesitates,
//! and clicks. It follows up to five links, now and then goes back, and
//! sometimes just leaves without clicking anything. How long each page is kept
//! is drawn from the site's share of the run's time.
//!
//! Every choice is made from what the page shows (`page.rs`), by the planners
//! of `pointer`, `scroll`, `targets`, `consent` and `rhythm`. Nothing here
//! knows a site, a selector or a path.

use super::consent::pick_accept;
use super::driver::{ClickOutcome, Driver, Fault};
use super::page::Snapshot;
use super::pointer::{plan_click, plan_click_point, Rect};
use super::rhythm::{
  action_gap_ms, banner_notice_ms, plan_site, reading_pause_ms, settle_after_load_ms, think_ms,
  Draw,
};
use super::scroll::{intent_px, plan_scroll, scrolls_back, ScrollStyle};
use super::targets::{canonical_url, choose_link, pick_weighted, registrable_domain, Context};
use super::text::normalize;
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::Instant;
use url::Url;

/// How a visit ended.
pub struct SiteResult {
  /// The site's first page loaded and was read.
  pub loaded: bool,
  /// What stopped the visit early, if anything.
  pub fault: Option<Fault>,
}

/// The least time worth starting a click for: reaching a link, hesitating,
/// pressing and waiting for the page takes this long.
const MIN_FOR_CLICK: Duration = Duration::from_secs(9);
/// A banner is looked for this many times on a site: at arrival, and again as
/// the page settles, because many appear late.
const CONSENT_SCANS: u32 = 3;
/// Chance that a visit ends without clicking anything further.
const LEAVES_WITHOUT_CLICKING: f64 = 0.12;
/// Chance that a visitor goes back after reading a followed page.
const GOES_BACK: f64 = 0.18;
/// How many times one wish to follow a link is tried: a look further down the
/// page, a link that moved, a click that led nowhere on the site.
const LINK_ATTEMPTS: u32 = 4;
/// A reading pause longer than this is broken up by small pointer movements.
const FIDGET_AFTER_MS: u64 = 4_000;
/// Chance that the pointer shifts between two parts of a long pause.
const FIDGETS: f64 = 0.6;

fn canonical_str(raw: &str) -> Option<String> {
  Url::parse(raw).ok().map(|url| canonical_url(&url))
}

fn host_of(raw: &str) -> String {
  Url::parse(raw)
    .ok()
    .and_then(|url| url.host_str().map(str::to_string))
    .unwrap_or_default()
}

fn millis_until(moment: Instant) -> u64 {
  moment.saturating_duration_since(Instant::now()).as_millis() as u64
}

enum Action {
  ScrollDown,
  ScrollUp,
  Hover(Rect),
  Drift,
  Rest,
}

pub(super) struct Visit<'a, D: Draw> {
  dr: &'a mut Driver,
  d: &'a mut D,
  style: ScrollStyle,
  /// The registrable domain the visit stays on.
  site: String,
  visited: HashSet<String>,
  current: String,
  consent_done: bool,
  consent_scans: u32,
  /// When this site's share of time is over.
  until: Instant,
}

/// Visit `url` and read around it for about `share`.
pub async fn warm_site(
  dr: &mut Driver,
  d: &mut impl Draw,
  style: ScrollStyle,
  url: &str,
  share: Duration,
) -> SiteResult {
  let Some(host) = Url::parse(url)
    .ok()
    .and_then(|parsed| parsed.host_str().map(str::to_string))
  else {
    return SiteResult {
      loaded: false,
      fault: None,
    };
  };
  let until = Instant::now() + share;
  if let Err(fault) = dr.goto(url).await {
    return SiteResult {
      loaded: false,
      fault: fault.is_fatal().then_some(fault),
    };
  }
  let mut visit = Visit::reader(dr, d, style, &registrable_domain(&host), until);
  let first = match visit.arrive().await {
    Ok(Some(reading)) => reading,
    Ok(None) => {
      return SiteResult {
        loaded: false,
        fault: None,
      }
    }
    Err(fault) => {
      return SiteResult {
        loaded: false,
        fault: Some(fault),
      }
    }
  };
  let fault = visit
    .pages(first, share.as_secs_f64())
    .await
    .err()
    .filter(Fault::is_fatal);
  SiteResult {
    loaded: true,
    fault,
  }
}

impl<'a, D: Draw> Visit<'a, D> {
  /// A reader of pages of `site` that follows no link of its own, for a
  /// scenario that decides where to go itself. It reads until `until`.
  pub(super) fn reader(
    dr: &'a mut Driver,
    d: &'a mut D,
    style: ScrollStyle,
    site: &str,
    until: Instant,
  ) -> Self {
    Visit {
      dr,
      d,
      style,
      site: site.to_string(),
      visited: HashSet::new(),
      current: String::new(),
      consent_done: false,
      consent_scans: 0,
      until,
    }
  }
}

impl<D: Draw> Visit<'_, D> {
  /// Let the first page load, take it in and read it. `None` when the address
  /// gave no page (an error page, a blank).
  async fn arrive(&mut self) -> Result<Option<Snapshot>, Fault> {
    let settle = settle_after_load_ms(self.d);
    self.dr.idle(settle).await?;
    let reading = match self.dr.observe(true).await {
      Ok(reading) => reading,
      Err(fault) if fault.is_fatal() => return Err(fault),
      Err(_) => return Ok(None),
    };
    self.consent_scans += 1;
    Ok(reading.is_document().then_some(reading))
  }

  /// Read the page the visit is on, looking for a banner while one may still
  /// appear.
  async fn read_page(&mut self) -> Result<Snapshot, Fault> {
    let look = !self.consent_done && self.consent_scans < CONSENT_SCANS;
    if look {
      self.consent_scans += 1;
    }
    self.dr.observe(look).await
  }

  /// The pages of the visit: the one it landed on, then the links it follows.
  async fn pages(&mut self, first: Snapshot, share_secs: f64) -> Result<(), Fault> {
    let plan = plan_site(share_secs, self.d);
    let mut reading = first;
    let mut page = 0;
    loop {
      self.dr.tally.pages += 1;
      let dwell = Duration::from_millis(plan.dwell_ms[page]);
      self.browse_page(reading, dwell).await?;
      if page == plan.links || self.until.saturating_duration_since(Instant::now()) < MIN_FOR_CLICK
      {
        return Ok(());
      }
      if self.d.chance(LEAVES_WITHOUT_CLICKING) {
        return Ok(());
      }
      if page > 0 && self.d.chance(GOES_BACK) && self.dr.back().await? {
        self.dr.idle(settle_after_load_ms(self.d)).await?;
        let earlier = self.read_page().await?;
        if !earlier.is_document() {
          return Ok(());
        }
        self.note_page(&earlier);
        self.dr.tally.pages += 1;
        let short = Duration::from_millis(self.d.range(3_500.0, 9_000.0) as u64);
        self.browse_page(earlier, short).await?;
      }
      match self.follow_link().await? {
        Some(next) => reading = next,
        None => return Ok(()),
      }
      page += 1;
    }
  }

  fn note_page(&mut self, reading: &Snapshot) {
    if let Some(canonical) = canonical_str(&reading.url) {
      self.visited.insert(canonical.clone());
      self.current = canonical;
    }
  }

  /// Stay on the page for about `dwell`, reading it.
  pub(super) async fn browse_page(
    &mut self,
    mut reading: Snapshot,
    dwell: Duration,
  ) -> Result<(), Fault> {
    self.note_page(&reading);
    let page_until = (Instant::now() + dwell).min(self.until);
    let (view, dpr) = (reading.viewport(), reading.dpr);
    self.dr.ensure_pointer(view, dpr, self.d).await?;
    reading = self.deal_with_banner(reading).await?;

    while Instant::now() + Duration::from_millis(2_500) < page_until {
      let action = self.pick_action(&reading);
      self.act(action, &reading).await?;
      let pause = reading_pause_ms(reading.visible_chars, self.d).min(millis_until(page_until));
      self.read_for(pause, view, dpr).await?;
      match self.read_page().await {
        Ok(next) if next.is_document() => reading = next,
        Ok(_) => break,
        Err(fault) if fault.is_fatal() => return Err(fault),
        Err(_) => break,
      }
      reading = self.deal_with_banner(reading).await?;
    }
    Ok(())
  }

  /// Spend `pause_ms` reading. A hand does not lie still on the desk for ten
  /// seconds: now and then the pointer shifts a little.
  pub(super) async fn read_for(
    &mut self,
    pause_ms: u64,
    view: (f64, f64),
    dpr: f64,
  ) -> Result<(), Fault> {
    let mut left = pause_ms;
    while left > 0 {
      let slice = if left > FIDGET_AFTER_MS {
        (self.d.log_normal(3_200.0, 0.5).clamp(1_500.0, 7_000.0) as u64).min(left)
      } else {
        left
      };
      self.dr.idle(slice).await?;
      left -= slice;
      if left > FIDGET_AFTER_MS / 2 && self.d.chance(FIDGETS) {
        self.dr.drift(view, dpr, self.d).await?;
      }
    }
    Ok(())
  }

  /// Accept a cookie banner, with the mouse, if the reading shows one.
  async fn deal_with_banner(&mut self, reading: Snapshot) -> Result<Snapshot, Fault> {
    let Some(index) = pick_accept(&reading.buttons) else {
      return Ok(reading);
    };
    let button = reading.buttons[index].clone();
    let (view, dpr) = (reading.viewport(), reading.dpr);
    let notice = banner_notice_ms(self.d);
    self.dr.idle(notice).await?;
    let aim = plan_click_point(button.rect, dpr, self.d);
    let width = button.rect.h.min(button.rect.w);
    self.dr.move_to(aim, width, view, dpr, self.d).await?;
    let hesitation = think_ms(self.d);
    self.dr.idle(hesitation).await?;
    // The banner may have moved or closed while the pointer was on its way.
    if let Some(under) = self.dr.control_at(aim).await? {
      if normalize(&under) != normalize(&button.name) {
        return Ok(reading);
      }
    }
    let click = plan_click(dpr, self.d);
    self.dr.click(&click).await?;
    self.consent_done = true;
    self.dr.tally.consents += 1;
    let after = self.d.range(900.0, 1_900.0) as u64;
    self.dr.idle(after).await?;
    match self.dr.observe(false).await {
      Ok(next) if next.is_document() => Ok(next),
      Ok(_) => Ok(reading),
      Err(fault) if fault.is_fatal() => Err(fault),
      Err(_) => Ok(reading),
    }
  }

  fn pick_action(&mut self, reading: &Snapshot) -> Action {
    let down_room = !reading.at_bottom();
    let below_top = reading.scroll_y > reading.vh * 0.4;
    if below_top && scrolls_back(reading.scrolled_fraction(), self.d) {
      return Action::ScrollUp;
    }
    let scores = [
      down_room.then_some(60.0),
      (!down_room && below_top).then_some(40.0),
      (!reading.hover.is_empty()).then_some(14.0),
      Some(10.0),
      Some(8.0),
    ];
    match pick_weighted(&scores, self.d) {
      Some(0) => Action::ScrollDown,
      Some(1) => Action::ScrollUp,
      Some(2) => {
        let index = self.d.index(reading.hover.len());
        Action::Hover(reading.hover[index])
      }
      Some(3) => Action::Drift,
      _ => Action::Rest,
    }
  }

  async fn act(&mut self, action: Action, reading: &Snapshot) -> Result<(), Fault> {
    let (view, dpr) = (reading.viewport(), reading.dpr);
    match action {
      Action::ScrollDown | Action::ScrollUp => {
        let down = matches!(action, Action::ScrollDown);
        let mut intent = intent_px(reading.vh, self.d);
        if !down {
          intent *= self.d.range(0.35, 0.8);
        }
        let plan = plan_scroll(self.style, intent, down, self.d);
        self.dr.ensure_pointer(view, dpr, self.d).await?;
        self.dr.scroll(&plan).await?;
        let settle = self.d.range(250.0, 600.0) as u64;
        self.dr.idle(settle).await
      }
      Action::Hover(rect) => {
        let aim = plan_click_point(rect, dpr, self.d);
        self
          .dr
          .move_to(aim, rect.h.min(rect.w), view, dpr, self.d)
          .await?;
        let rest = self.d.log_normal(900.0, 0.5).clamp(300.0, 3_500.0) as u64;
        self.dr.idle(rest).await
      }
      Action::Drift => self.dr.drift(view, dpr, self.d).await,
      Action::Rest => {
        let gap = action_gap_ms(self.d);
        self.dr.idle(gap).await
      }
    }
  }

  /// Pick a link a reader would, go to it with the pointer, and click. The new
  /// page's reading, or `None` when no fair link could be followed.
  async fn follow_link(&mut self) -> Result<Option<Snapshot>, Fault> {
    let next = self.try_follow_link().await?;
    if next.is_none() {
      self.dr.tally.link_misses += 1;
    }
    Ok(next)
  }

  async fn try_follow_link(&mut self) -> Result<Option<Snapshot>, Fault> {
    for _ in 0..LINK_ATTEMPTS {
      if self.until.saturating_duration_since(Instant::now()) < MIN_FOR_CLICK {
        return Ok(None);
      }
      let reading = self.dr.observe(false).await?;
      let chosen = {
        let ctx = Context {
          site: &self.site,
          current: Some(self.current.as_str()),
          visited: &self.visited,
          viewport: reading.viewport(),
        };
        choose_link(&reading.links, &ctx, self.d)
      };
      let (view, dpr) = (reading.viewport(), reading.dpr);
      let Some(index) = chosen else {
        // Nothing fair in view: look further down, or give up at the bottom.
        if reading.at_bottom() {
          return Ok(None);
        }
        let plan = plan_scroll(self.style, intent_px(reading.vh, self.d), true, self.d);
        self.dr.ensure_pointer(view, dpr, self.d).await?;
        self.dr.scroll(&plan).await?;
        let pause = reading_pause_ms(reading.visible_chars * 0.4, self.d);
        self.dr.idle(pause).await?;
        continue;
      };
      let link = reading.links[index].clone();
      let aim = plan_click_point(link.rect, dpr, self.d);
      let width = link.rect.h.max(14.0).min(link.rect.w);
      self.dr.move_to(aim, width, view, dpr, self.d).await?;
      let hesitation = think_ms(self.d);
      self.dr.idle(hesitation).await?;

      // The page may have shifted while the pointer travelled: press only if
      // the link is still under it.
      let under = self.dr.link_at(aim).await?;
      let still_there = under.as_deref().and_then(canonical_str) == canonical_str(&link.href);
      if !still_there {
        continue;
      }
      if let Some(target) = canonical_str(&link.href) {
        self.visited.insert(target);
      }
      let click = plan_click(dpr, self.d);
      self.dr.click(&click).await?;
      let outcome = self.dr.settle_after_click(Duration::from_secs(25)).await?;
      if let Some(next) = self.after_click(outcome).await? {
        return Ok(Some(next));
      }
    }
    Ok(None)
  }

  /// Make sense of what a click did. The reading of the page the visit is now
  /// on, if it is a page of the site.
  async fn after_click(&mut self, outcome: ClickOutcome) -> Result<Option<Snapshot>, Fault> {
    match outcome {
      ClickOutcome::Nothing => Ok(None),
      ClickOutcome::NewTab(target) => {
        let site = self.site.clone();
        let kept = self
          .dr
          .adopt_new_tab(&target, |url| registrable_domain(&host_of(url)) == site)
          .await?;
        if !kept {
          return Ok(None);
        }
        let settle = settle_after_load_ms(self.d);
        self.dr.idle(settle).await?;
        self.accept_reading().await
      }
      ClickOutcome::Navigated | ClickOutcome::TimedOut => {
        let settle = settle_after_load_ms(self.d);
        self.dr.idle(settle).await?;
        let reading = match self.read_page().await {
          Ok(reading) => reading,
          Err(fault) if fault.is_fatal() => return Err(fault),
          Err(_) => {
            self.dr.back().await?;
            return Ok(None);
          }
        };
        let on_site =
          reading.is_document() && registrable_domain(&host_of(&reading.url)) == self.site;
        if !on_site {
          // An error page, or a redirect off the site: step back from it.
          self.dr.back().await?;
          return Ok(None);
        }
        self.note_page(&reading);
        Ok(Some(reading))
      }
    }
  }

  async fn accept_reading(&mut self) -> Result<Option<Snapshot>, Fault> {
    let reading = self.read_page().await?;
    if !reading.is_document() {
      return Ok(None);
    }
    self.note_page(&reading);
    Ok(Some(reading))
  }
}
