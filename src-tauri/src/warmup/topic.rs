//! A shopper warming a profile up on one topic.
//!
//! The scenario is the one a person shopping for `keyword` goes through:
//!
//! 1. search Google for the keyword with the shop's name, and open one to three
//!    of the shop's results, reading each;
//! 2. take a product's title from what was read, shorten it to what a person
//!    types, and search the shop for it;
//! 3. keep the searches the shop suggests, in its search box and next to its
//!    results, when they stay on the topic;
//! 4. open listings of that search in their own tab, look through the
//!    pictures, read the page, and close the tab;
//! 5. search the kept suggestions one at a time, opening listings of each, and
//!    now and then start over from Google, until the time is spent.
//!
//! Typing is key presses at a person's pace (`keys`), every click is the
//! pointer's, and a bot check ends the run where it is: pushing through one is
//! what a bot does.

use super::driver::{ClickOutcome, Driver, Fault};
use super::keys::{plan_typing, KeyPress, ENTER};
use super::pointer::{plan_click, plan_click_point, Rect};
use super::rhythm::{action_gap_ms, reading_pause_ms, settle_after_load_ms, think_ms, Draw};
use super::scroll::{intent_px, plan_scroll, ScrollStyle};
use super::shop::{shop_expression, Blocker, ItemPage, ShopReading};
use super::targets::{pick_weighted, registrable_domain};
use super::visit::Visit;
use super::{topic_home, TopicSpec};
use crate::paste_typing::Stroke;
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::Instant;
use url::Url;

const GOOGLE: &str = "https://www.google.com/";
/// The least time worth starting another search for.
const MIN_FOR_SEARCH: Duration = Duration::from_secs(50);
/// The least time worth opening another listing for.
const MIN_FOR_ITEM: Duration = Duration::from_secs(25);
/// Suggestions kept from one run.
const MAX_NEW_VARIANTS: usize = 40;
/// Product titles remembered to search from.
const MAX_TITLES: usize = 50;
/// Steps in a row that went nowhere before the run gives up.
const MAX_MISSES: u32 = 4;
/// Chance that after a search of the shop the shopper starts over on Google.
const BACK_TO_GOOGLE: f64 = 0.18;
/// Chance that the shopper searches the keyword itself again.
const ROOT_AGAIN: f64 = 0.15;
/// Chance that the pictures of a listing are looked through.
const LOOKS_AT_PICTURES: f64 = 0.85;
/// Chance that the shop's question about where to ship is confirmed rather
/// than closed.
const CONFIRMS_SHIPPING: f64 = 0.7;
/// Dialogs answered in one reading before the page is taken as it is.
const MAX_ANSWERS: u32 = 2;
/// The longest search typed.
const MAX_TYPED: usize = 60;

/// Words of a listing title nobody types into a search.
const NOISE: &[&str] = &[
  "new",
  "listing",
  "sponsored",
  "brand",
  "used",
  "sealed",
  "open",
  "opened",
  "box",
  "excellent",
  "good",
  "very",
  "fair",
  "great",
  "mint",
  "pristine",
  "condition",
  "read",
  "description",
  "see",
  "free",
  "fast",
  "shipping",
  "ship",
  "ships",
  "lot",
  "of",
  "for",
  "parts",
  "only",
  "with",
  "w/",
  "and",
  "the",
  "a",
  "an",
  "in",
  "&",
  "+",
  "-",
  "renewed",
  "refurbished",
  "certified",
  "pre-owned",
  "preowned",
  "grade",
  "genuine",
  "original",
  "authentic",
  "oem",
  "seller",
  "warranty",
  "nwt",
  "nib",
  "works",
  "working",
  "tested",
  "clean",
];

// --- Planners ---------------------------------------------------------------

/// A search as a person types it: lower case, single spaces, no symbols but
/// the ones that belong in a model name.
pub fn normalize_term(text: &str) -> String {
  let kept: String = text
    .to_lowercase()
    .chars()
    .map(|c| {
      if c.is_alphanumeric() || matches!(c, '-' | '+' | '.' | '&' | '\'' | '/') {
        c
      } else {
        ' '
      }
    })
    .collect();
  let joined = kept.split_whitespace().collect::<Vec<_>>().join(" ");
  joined
    .chars()
    .take(MAX_TYPED)
    .collect::<String>()
    .trim()
    .to_string()
}

/// The words of `root` a search must keep to stay on the topic: its words of
/// three letters or more, or all of them when it has none.
fn anchors(root: &str) -> Vec<String> {
  let words: Vec<String> = normalize_term(root)
    .split(' ')
    .filter(|word| !word.is_empty())
    .map(str::to_string)
    .collect();
  let named: Vec<String> = words
    .iter()
    .filter(|word| word.chars().filter(|c| c.is_alphabetic()).count() >= 3)
    .cloned()
    .collect();
  if named.is_empty() {
    words
  } else {
    named
  }
}

fn has_anchor(word: &str, anchor: &str) -> bool {
  word == anchor || word.starts_with(anchor)
}

/// Whether `text` is still about `root`.
pub fn on_topic(text: &str, root: &str) -> bool {
  let text = normalize_term(text);
  let words: Vec<&str> = text.split(' ').collect();
  let anchors = anchors(root);
  !anchors.is_empty()
    && anchors
      .iter()
      .all(|anchor| words.iter().any(|word| has_anchor(word, anchor)))
}

/// The suggestions of `candidates` worth keeping: on the topic, not the
/// keyword itself, and not already known. At most `room` of them.
pub fn fresh_variants(
  candidates: &[String],
  root: &str,
  known: &[String],
  room: usize,
) -> Vec<String> {
  let root = normalize_term(root);
  let mut seen: HashSet<String> = known.iter().map(|k| normalize_term(k)).collect();
  let mut out = Vec::new();
  for candidate in candidates {
    if out.len() >= room {
      break;
    }
    let term = normalize_term(candidate);
    if term.chars().count() < 2 || term == root || term.contains("http") || term.contains('@') {
      continue;
    }
    if !on_topic(&term, &root) || !seen.insert(term.clone()) {
      continue;
    }
    out.push(term);
  }
  out
}

fn without_brackets(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  let mut depth = 0u32;
  for c in text.chars() {
    match c {
      '(' | '[' | '{' => depth += 1,
      ')' | ']' | '}' => {
        depth = depth.saturating_sub(1);
        out.push(' ');
      }
      _ if depth == 0 => out.push(c),
      _ => {}
    }
  }
  out
}

/// What a person who read the listing `title` would type to look for more of
/// it: two to five of its words around the topic's word, brand first now and
/// then. `None` when the listing is not about the topic.
pub fn product_keyword(title: &str, root: &str, d: &mut impl Draw) -> Option<String> {
  let anchors = anchors(root);
  let plain = without_brackets(title);
  let segments: Vec<&str> = plain
    .split([',', '|'])
    .flat_map(|part| part.split(" - "))
    .collect();
  let segment = segments
    .iter()
    .find(|segment| {
      let words = normalize_term(segment);
      anchors
        .iter()
        .any(|anchor| words.split(' ').any(|word| has_anchor(word, anchor)))
    })
    .copied()?;
  let tokens: Vec<String> = normalize_term(segment)
    .split(' ')
    .filter(|word| {
      !word.is_empty()
        && word.len() <= 20
        && word.chars().any(char::is_alphanumeric)
        && !NOISE.contains(word)
    })
    .map(str::to_string)
    .collect();
  let at = tokens
    .iter()
    .position(|word| anchors.iter().any(|anchor| has_anchor(word, anchor)))?;
  let want = match pick_weighted(&[Some(20.0), Some(40.0), Some(30.0), Some(10.0)], d) {
    Some(0) => 2,
    Some(2) => 4,
    Some(3) => 5,
    _ => 3,
  };
  let start = if at > 0 && d.chance(0.45) { at - 1 } else { at };
  let end = (start + want).min(tokens.len());
  let start = end.saturating_sub(want).min(start);
  if end - start < 2 {
    return None;
  }
  let term = normalize_term(&tokens[start..end].join(" "));
  (on_topic(&term, root) && term != normalize_term(root)).then_some(term)
}

/// The searches in a body of the shop's suggestion endpoint: `richRes.sug[].kwd`,
/// or the plain `res.sug[]` list. A body wrapped in a callback is unwrapped.
pub fn suggestion_terms(body: &str) -> Vec<String> {
  let (Some(start), Some(end)) = (body.find('{'), body.rfind('}')) else {
    return Vec::new();
  };
  if end <= start {
    return Vec::new();
  }
  let Ok(value) = serde_json::from_str::<serde_json::Value>(&body[start..=end]) else {
    return Vec::new();
  };
  let rich = value
    .pointer("/richRes/sug")
    .and_then(|list| list.as_array())
    .into_iter()
    .flatten()
    .filter_map(|entry| entry.get("kwd").and_then(|kwd| kwd.as_str()));
  let plain = value
    .pointer("/res/sug")
    .and_then(|list| list.as_array())
    .into_iter()
    .flatten()
    .filter_map(|entry| entry.as_str());
  rich.chain(plain).map(str::to_string).collect()
}

/// What is typed into Google to find the shop's listings of `term`.
pub fn google_query(term: &str, d: &mut impl Draw) -> String {
  if d.chance(0.75) {
    format!("{term} ebay")
  } else {
    format!("ebay {term}")
  }
}

/// How many of Google's results for the shop are opened: mostly one or two.
pub fn results_to_open(d: &mut impl Draw) -> usize {
  match pick_weighted(&[Some(55.0), Some(35.0), Some(10.0)], d) {
    Some(1) => 2,
    Some(2) => 3,
    _ => 1,
  }
}

/// How many listings of one search are opened.
pub fn listings_to_open(d: &mut impl Draw) -> usize {
  match pick_weighted(&[Some(40.0), Some(40.0), Some(20.0)], d) {
    Some(1) => 2,
    Some(2) => 3,
    _ => 1,
  }
}

/// How long a listing is looked at, all told.
pub fn item_dwell_ms(d: &mut impl Draw) -> u64 {
  d.log_normal(48_000.0, 0.4).clamp(18_000.0, 130_000.0) as u64
}

fn is_shop(url: &str) -> bool {
  Url::parse(url)
    .ok()
    .and_then(|url| url.host_str().map(registrable_domain))
    .is_some_and(|domain| domain.starts_with("ebay."))
}

// --- The shopper --------------------------------------------------------------

/// How a topic run ended.
pub struct TopicOutcome {
  /// A page of the shop loaded.
  pub loaded: bool,
  /// Suggestions found in this run.
  pub learned: Vec<String>,
  /// A bot check stopped the run.
  pub challenged: bool,
  pub fault: Option<Fault>,
}

enum Halt {
  Fault(Fault),
  Challenged,
}

impl From<Fault> for Halt {
  fn from(fault: Fault) -> Self {
    Halt::Fault(fault)
  }
}

type Step<T> = Result<T, Halt>;

struct Shopper<'a, D: Draw> {
  dr: &'a mut Driver,
  d: &'a mut D,
  style: ScrollStyle,
  mac: bool,
  root: String,
  home: &'static str,
  /// Suggestions from earlier runs.
  known: Vec<String>,
  /// Suggestions found in this run.
  found: Vec<String>,
  searched: HashSet<String>,
  opened: HashSet<String>,
  titles: Vec<String>,
  loaded: bool,
}

/// Shop for `spec.keyword` until the run's time is spent.
pub async fn shop<D: Draw>(
  dr: &mut Driver,
  d: &mut D,
  style: ScrollStyle,
  spec: &TopicSpec,
) -> TopicOutcome {
  let mut shopper = Shopper {
    dr,
    d,
    style,
    mac: spec.mac,
    root: normalize_term(&spec.keyword),
    home: topic_home(&spec.site),
    known: spec.learned.iter().map(|k| normalize_term(k)).collect(),
    found: Vec::new(),
    searched: HashSet::new(),
    opened: HashSet::new(),
    titles: Vec::new(),
    loaded: false,
  };
  if let Err(fault) = shopper.dr.watch_suggestions().await {
    log::debug!("Warm-up suggestion watch unavailable reason=\"{fault}\"");
  }
  let (challenged, fault) = match shopper.cycles().await {
    Ok(()) => (false, None),
    Err(Halt::Challenged) => (true, None),
    Err(Halt::Fault(fault)) => (false, Some(fault)),
  };
  TopicOutcome {
    loaded: shopper.loaded,
    learned: shopper.found,
    challenged,
    fault,
  }
}

impl<D: Draw> Shopper<'_, D> {
  async fn cycles(&mut self) -> Step<()> {
    let mut misses = 0;
    let mut from_google = true;
    while self.dr.remaining() >= MIN_FOR_SEARCH {
      let went = if from_google {
        self.via_google().await
      } else {
        let term = self.next_term();
        self.search_shop(&term).await
      };
      match went {
        Ok(true) => misses = 0,
        Ok(false) => misses += 1,
        Err(Halt::Fault(fault)) if !fault.is_fatal() => {
          log::debug!("Warm-up topic step failed reason=\"{fault}\"");
          misses += 1;
        }
        Err(other) => return Err(other),
      }
      if misses >= MAX_MISSES {
        return Ok(());
      }
      from_google = !self.loaded || self.d.chance(BACK_TO_GOOGLE);
      let gap = action_gap_ms(self.d);
      self.dr.idle(gap).await?;
    }
    Ok(())
  }

  // --- Steps of the scenario -------------------------------------------------

  /// Search Google, read the shop's results, then search the shop for a
  /// product that was read.
  async fn via_google(&mut self) -> Step<bool> {
    self.dr.goto(GOOGLE).await?;
    self.settle().await?;
    let look = Duration::from_millis(self.d.range(2_500.0, 6_000.0) as u64);
    self.read_around(look).await?;
    let term = self.google_term();
    let query = google_query(&term, self.d);
    if !self.search(&query).await? {
      return Ok(false);
    }
    let look = Duration::from_millis(self.d.range(4_000.0, 11_000.0) as u64);
    self.read_around(look).await?;

    let wanted = results_to_open(self.d);
    let mut opened = 0;
    for n in 0..wanted {
      if self.dr.remaining() < MIN_FOR_SEARCH {
        break;
      }
      if n > 0 {
        if !self.dr.back().await? {
          break;
        }
        self.settle().await?;
        self.read_shop().await?;
      }
      if !self.open_result().await? {
        break;
      }
      opened += 1;
      self.read_landing().await?;
    }
    if opened == 0 {
      // No result for the shop: a person types its address.
      self.dr.goto(self.home).await?;
      self.settle().await?;
      self.read_shop().await?;
      let look = Duration::from_millis(self.d.range(5_000.0, 14_000.0) as u64);
      self.read_around(look).await?;
    }
    if self.dr.remaining() < MIN_FOR_SEARCH {
      return Ok(true);
    }
    let term = self.product_term();
    self.search_shop(&term).await
  }

  /// Search the shop for `term`, keep what it suggests, and open listings.
  async fn search_shop(&mut self, term: &str) -> Step<bool> {
    if !self.search(term).await? {
      return Ok(false);
    }
    let reading = self.read_shop().await?;
    self.keep(&reading.related);
    for listing in &reading.items {
      self.note_title(&listing.title);
    }
    let look = Duration::from_millis(self.d.range(6_000.0, 16_000.0) as u64);
    self.read_around(look).await?;
    let wanted = listings_to_open(self.d);
    for _ in 0..wanted {
      if self.dr.remaining() < MIN_FOR_ITEM || !self.open_listing().await? {
        break;
      }
      let gap = action_gap_ms(self.d);
      self.dr.idle(gap).await?;
    }
    Ok(true)
  }

  /// Type `text` into the page's search box and send it. Whether a results
  /// page came.
  async fn search(&mut self, text: &str) -> Step<bool> {
    let Some(reading) = self.find_field().await? else {
      return Ok(false);
    };
    let Some(field) = reading.field.clone() else {
      return Ok(false);
    };
    let (view, dpr) = (reading.viewport(), reading.dpr);
    if !field.focused {
      self.click_at(field.rect, view, dpr).await?;
      let wait = self.d.range(250.0, 700.0) as u64;
      self.dr.idle(wait).await?;
    }
    let now = self.read_shop().await?;
    let holds_text = now
      .field
      .as_ref()
      .map_or(!field.value.is_empty(), |f| !f.value.is_empty());
    if holds_text {
      let hold = self.hold_ms();
      self.dr.select_all(self.mac, hold).await?;
      let wait = self.d.range(150.0, 450.0) as u64;
      self.dr.idle(wait).await?;
    }
    let plan = plan_typing(text, self.d);
    self.dr.type_keys(&plan).await?;
    let on_shop = is_shop(&reading.url);
    if on_shop {
      // The shop asks for suggestions only once typing has paused: a space
      // typed after the pause opens the list.
      let pause = self.d.log_normal(700.0, 0.35).clamp(350.0, 1_600.0) as u64;
      self.dr.idle(pause).await?;
      let space = KeyPress {
        at_ms: 0.0,
        hold_ms: self.hold_ms() as f64,
        stroke: Stroke::Char(' '),
      };
      self.dr.type_keys(&[space]).await?;
    }
    // A look at what the box suggests before sending.
    let look = self.d.log_normal(1_100.0, 0.4).clamp(600.0, 2_800.0) as u64;
    self.dr.idle(look).await?;
    let typed = self.read_shop().await?;
    if on_shop {
      let mut suggested: Vec<String> = typed.suggestions.iter().map(|s| s.text.clone()).collect();
      for body in self.dr.take_suggestions().await {
        suggested.extend(suggestion_terms(&body));
      }
      self.keep(&suggested);
    }
    let decide = think_ms(self.d) / 2;
    self.dr.idle(decide).await?;
    let hold = self.hold_ms();
    self.dr.press_key(ENTER, hold).await?;
    let mut outcome = self.dr.settle_after_click(Duration::from_secs(25)).await?;
    if outcome == ClickOutcome::Nothing {
      // The key did not send the form; its button does.
      if let Some(button) = typed.field.and_then(|f| f.submit) {
        self.click_at(button, view, dpr).await?;
        outcome = self.dr.settle_after_click(Duration::from_secs(25)).await?;
      }
    }
    if !matches!(outcome, ClickOutcome::Navigated | ClickOutcome::TimedOut) {
      return Ok(false);
    }
    self.dr.tally.searches += 1;
    self.searched.insert(normalize_term(text));
    self.settle().await?;
    self.read_shop().await?;
    Ok(true)
  }

  /// The search box, scrolling back up to it when the page has been read
  /// further down.
  async fn find_field(&mut self) -> Step<Option<ShopReading>> {
    for _ in 0..12 {
      let reading = self.read_shop().await?;
      if reading.field.is_some() {
        return Ok(Some(reading));
      }
      if reading.scroll_y <= 0.0 {
        return Ok(None);
      }
      let intent = reading.vh * self.d.range(1.0, 1.45);
      self.scroll(&reading, intent, false).await?;
    }
    Ok(None)
  }

  /// Open one of Google's results for the shop.
  async fn open_result(&mut self) -> Step<bool> {
    for _ in 0..4 {
      let reading = self.read_shop().await?;
      if reading.results.is_empty() {
        if reading.results_below == 0 || reading.at_bottom() {
          return Ok(false);
        }
        let intent = intent_px(reading.vh, self.d);
        self.scroll(&reading, intent, true).await?;
        continue;
      }
      // The first results get most of the clicks.
      let scores: Vec<Option<f64>> = (0..reading.results.len())
        .map(|i| Some(1.0 / (1.0 + i as f64)))
        .collect();
      let index = pick_weighted(&scores, self.d).unwrap_or(0);
      let rect = reading.results[index].rect;
      let Some(outcome) = self
        .click_link(rect, reading.viewport(), reading.dpr, |_| true)
        .await?
      else {
        continue;
      };
      match outcome {
        ClickOutcome::Nothing => continue,
        ClickOutcome::NewTab(target) => {
          if !self.dr.adopt_new_tab(&target, is_shop).await? {
            continue;
          }
        }
        ClickOutcome::Navigated | ClickOutcome::TimedOut => {}
      }
      self.settle().await?;
      let landed = self.read_shop().await?;
      if is_shop(&landed.url) {
        return Ok(true);
      }
      self.dr.back().await?;
      self.settle().await?;
    }
    Ok(false)
  }

  /// Read the shop's page a Google result led to.
  async fn read_landing(&mut self) -> Step<()> {
    let reading = self.read_shop().await?;
    if let Some(item) = reading.item.clone() {
      return self.look_at_item(&item, &reading).await;
    }
    for listing in &reading.items {
      self.note_title(&listing.title);
    }
    let look = Duration::from_millis(self.d.range(12_000.0, 35_000.0) as u64);
    self.read_around(look).await?;
    let after = self.read_shop().await?;
    for listing in &after.items {
      self.note_title(&listing.title);
    }
    Ok(())
  }

  /// Open one listing of the results on screen in its tab, look at it, and
  /// come back.
  async fn open_listing(&mut self) -> Step<bool> {
    for _ in 0..5 {
      let reading = self.read_shop().await?;
      let fresh: Vec<usize> = (0..reading.items.len())
        .filter(|&i| !self.opened.contains(&reading.items[i].id))
        .collect();
      let further = reading.items_below > 0 && !reading.at_bottom();
      if fresh.is_empty() || (further && self.d.chance(0.3)) {
        if !further {
          return Ok(false);
        }
        let intent = intent_px(reading.vh, self.d);
        self.scroll(&reading, intent, true).await?;
        let pause = reading_pause_ms(400.0, self.d);
        self.dr.idle(pause).await?;
        continue;
      }
      let scores: Vec<Option<f64>> = fresh
        .iter()
        .map(|&i| {
          Some(if on_topic(&reading.items[i].title, &self.root) {
            3.0
          } else {
            1.0
          })
        })
        .collect();
      let index = fresh[pick_weighted(&scores, self.d).unwrap_or(0)];
      let listing = reading.items[index].clone();
      self.opened.insert(listing.id.clone());
      self.note_title(&listing.title);
      let id = listing.id.clone();
      let Some(outcome) = self
        .click_link(listing.rect, reading.viewport(), reading.dpr, move |href| {
          href.contains(&id)
        })
        .await?
      else {
        continue;
      };
      match outcome {
        ClickOutcome::Nothing => continue,
        ClickOutcome::NewTab(target) => {
          let parked = self.dr.open_tab(&target).await?;
          let looked = self.look_here().await;
          let back = self.dr.close_tab(parked).await;
          looked?;
          back?;
        }
        ClickOutcome::Navigated | ClickOutcome::TimedOut => {
          self.look_here().await?;
          self.dr.back().await?;
        }
      }
      self.settle().await?;
      return Ok(true);
    }
    Ok(false)
  }

  async fn look_here(&mut self) -> Step<()> {
    self.settle().await?;
    let reading = self.read_shop().await?;
    match reading.item.clone() {
      Some(item) => self.look_at_item(&item, &reading).await,
      None => {
        let look = Duration::from_millis(self.d.range(6_000.0, 15_000.0) as u64);
        self.read_around(look).await
      }
    }
  }

  /// Look at a listing: its pictures, then the page, down to the description.
  async fn look_at_item(&mut self, item: &ItemPage, reading: &ShopReading) -> Step<()> {
    self.dr.tally.items += 1;
    self.note_title(&item.title);
    let left = self.dr.remaining().saturating_sub(Duration::from_secs(5));
    let until = Instant::now() + Duration::from_millis(item_dwell_ms(self.d)).min(left);
    if self.d.chance(LOOKS_AT_PICTURES) {
      self.look_at_pictures(item, reading).await?;
    }
    let rest = until.saturating_duration_since(Instant::now());
    if rest > Duration::from_secs(3) {
      self.read_around(rest).await?;
    }
    Ok(())
  }

  async fn look_at_pictures(&mut self, item: &ItemPage, reading: &ShopReading) -> Step<()> {
    let (view, dpr) = (reading.viewport(), reading.dpr);
    if let Some(main) = item.picture {
      let aim = plan_click_point(main, dpr, self.d);
      self
        .dr
        .move_to(aim, main.w.min(main.h), view, dpr, self.d)
        .await?;
      let look = self.picture_look_ms();
      self.dr.idle(look).await?;
    }
    let others = item.pictures.len().saturating_sub(1);
    if others > 0 {
      let wanted = 1 + self.d.index(others.min(5));
      let mut at = 0;
      for _ in 0..wanted {
        at = if self.d.chance(0.75) {
          (at + 1).min(others)
        } else {
          1 + self.d.index(others)
        };
        self.look_at_picture(item.pictures[at], view, dpr).await?;
      }
    } else if let Some(next) = item.next {
      let wanted = 1 + self.d.index(4);
      for _ in 0..wanted {
        self.look_at_picture(next, view, dpr).await?;
      }
    }
    Ok(())
  }

  async fn look_at_picture(&mut self, rect: Rect, view: (f64, f64), dpr: f64) -> Step<()> {
    let aim = plan_click_point(rect, dpr, self.d);
    self
      .dr
      .move_to(aim, rect.w.min(rect.h), view, dpr, self.d)
      .await?;
    let wait = think_ms(self.d) / 2;
    self.dr.idle(wait).await?;
    if self.dr.control_at(aim).await?.is_none() {
      return Ok(());
    }
    let click = plan_click(dpr, self.d);
    self.dr.click(&click).await?;
    self.dr.tally.pictures += 1;
    let look = self.picture_look_ms();
    self.dr.idle(look).await?;
    Ok(())
  }

  // --- Hands and eyes ----------------------------------------------------------

  /// Read the page as a shopper. A bot check ends the run; a dialog over the
  /// page is answered first.
  async fn read_shop(&mut self) -> Step<ShopReading> {
    let mut answered = 0;
    loop {
      let mut reading: ShopReading = self.dr.read(&shop_expression()).await?;
      if reading.challenged {
        return Err(Halt::Challenged);
      }
      if is_shop(&reading.url) {
        self.loaded = true;
      }
      let Some(blocker) = reading.blocker.take() else {
        return Ok(reading);
      };
      if answered >= MAX_ANSWERS
        || !self
          .answer(&blocker, reading.viewport(), reading.dpr)
          .await?
      {
        return Ok(reading);
      }
      answered += 1;
    }
  }

  /// Deal with a dialog the way a person does: read it, then mostly confirm
  /// the shipping question and close anything else.
  async fn answer(&mut self, blocker: &Blocker, view: (f64, f64), dpr: f64) -> Step<bool> {
    let target = match (blocker.confirm, blocker.close) {
      (Some(confirm), Some(close)) => {
        if self.d.chance(CONFIRMS_SHIPPING) {
          confirm
        } else {
          close
        }
      }
      (Some(only), None) | (None, Some(only)) => only,
      (None, None) => return Ok(false),
    };
    let read = self.d.log_normal(1_400.0, 0.4).clamp(600.0, 4_000.0) as u64;
    self.dr.idle(read).await?;
    self.click_at(target, view, dpr).await?;
    self.dr.settle_after_click(Duration::from_secs(25)).await?;
    Ok(true)
  }

  /// Read the page for about `dwell` the way any visit does: scroll, rest the
  /// pointer, take a cookie banner.
  async fn read_around(&mut self, dwell: Duration) -> Step<()> {
    let reading = self.dr.observe(true).await?;
    if !reading.is_document() {
      return Ok(());
    }
    self.dr.tally.pages += 1;
    let until = Instant::now() + dwell;
    let mut visit = Visit::reader(&mut *self.dr, &mut *self.d, self.style, "", until);
    visit.browse_page(reading, dwell).await?;
    Ok(())
  }

  async fn settle(&mut self) -> Step<()> {
    let settle = settle_after_load_ms(self.d);
    self.dr.idle(settle).await?;
    Ok(())
  }

  async fn scroll(&mut self, reading: &ShopReading, intent: f64, down: bool) -> Step<()> {
    let (view, dpr) = (reading.viewport(), reading.dpr);
    let plan = plan_scroll(self.style, intent, down, self.d);
    self.dr.ensure_pointer(view, dpr, self.d).await?;
    self.dr.scroll(&plan).await?;
    let settle = self.d.range(300.0, 800.0) as u64;
    self.dr.idle(settle).await?;
    Ok(())
  }

  async fn click_at(&mut self, rect: Rect, view: (f64, f64), dpr: f64) -> Step<()> {
    let aim = plan_click_point(rect, dpr, self.d);
    self
      .dr
      .move_to(aim, rect.h.min(rect.w), view, dpr, self.d)
      .await?;
    let hesitation = think_ms(self.d);
    self.dr.idle(hesitation).await?;
    let click = plan_click(dpr, self.d);
    self.dr.click(&click).await?;
    Ok(())
  }

  /// Go to a link with the pointer and click it if what is under the pointer
  /// is still a link `expected` accepts. `None` when it was not.
  async fn click_link(
    &mut self,
    rect: Rect,
    view: (f64, f64),
    dpr: f64,
    expected: impl Fn(&str) -> bool,
  ) -> Step<Option<ClickOutcome>> {
    let aim = plan_click_point(rect, dpr, self.d);
    let width = rect.h.max(14.0).min(rect.w);
    self.dr.move_to(aim, width, view, dpr, self.d).await?;
    let hesitation = think_ms(self.d);
    self.dr.idle(hesitation).await?;
    let under = self.dr.link_at(aim).await?;
    if !under.as_deref().is_some_and(expected) {
      return Ok(None);
    }
    let click = plan_click(dpr, self.d);
    self.dr.click(&click).await?;
    let outcome = self.dr.settle_after_click(Duration::from_secs(25)).await?;
    Ok(Some(outcome))
  }

  fn hold_ms(&mut self) -> u64 {
    self.d.log_normal(85.0, 0.25).clamp(45.0, 170.0) as u64
  }

  fn picture_look_ms(&mut self) -> u64 {
    self.d.log_normal(2_300.0, 0.45).clamp(900.0, 7_000.0) as u64
  }

  // --- What the shopper remembers -------------------------------------------------

  fn keep(&mut self, candidates: &[String]) {
    let room = MAX_NEW_VARIANTS.saturating_sub(self.found.len());
    let mut known = self.known.clone();
    known.extend(self.found.iter().cloned());
    let fresh = fresh_variants(candidates, &self.root, &known, room);
    self.found.extend(fresh);
  }

  fn note_title(&mut self, title: &str) {
    if !on_topic(title, &self.root) || self.titles.iter().any(|t| t == title) {
      return;
    }
    if self.titles.len() >= MAX_TITLES {
      self.titles.remove(0);
    }
    self.titles.push(title.to_string());
  }

  /// What to look for on Google: the keyword the first time, then now and
  /// then one of its suggestions.
  fn google_term(&mut self) -> String {
    if self.searched.is_empty() || self.d.chance(0.5) {
      return self.root.clone();
    }
    let pool: Vec<&String> = self.found.iter().chain(self.known.iter()).collect();
    if pool.is_empty() {
      return self.root.clone();
    }
    pool[self.d.index(pool.len())].clone()
  }

  /// A product read on the way, shortened to a search.
  fn product_term(&mut self) -> String {
    for _ in 0..4 {
      if self.titles.is_empty() {
        break;
      }
      // Later titles were read more recently.
      let n = self.titles.len();
      let index = n - 1 - self.d.index(n.min(6));
      if let Some(term) = product_keyword(&self.titles[index], &self.root, self.d) {
        if !self.searched.contains(&term) {
          return term;
        }
      }
    }
    self.root.clone()
  }

  /// The next search of the shop: a suggestion not yet searched, the ones
  /// found in this run more often.
  fn next_term(&mut self) -> String {
    if self.d.chance(ROOT_AGAIN) && !self.searched.contains(&self.root) {
      return self.root.clone();
    }
    let pool: Vec<(&String, f64)> = self
      .found
      .iter()
      .map(|term| (term, 2.0))
      .chain(self.known.iter().map(|term| (term, 1.0)))
      .filter(|(term, _)| !self.searched.contains(*term))
      .collect();
    if pool.is_empty() {
      return self.product_term();
    }
    let scores: Vec<Option<f64>> = pool.iter().map(|(_, weight)| Some(*weight)).collect();
    let index = pick_weighted(&scores, self.d).unwrap_or(0);
    pool[index].0.clone()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::warmup::rhythm::Seeded;

  #[test]
  fn a_term_is_typed_plainly() {
    assert_eq!(
      normalize_term("  Apple  iPhone 13 (Mini)!! "),
      "apple iphone 13 mini"
    );
    assert_eq!(normalize_term("USB-C 2.0 & more"), "usb-c 2.0 & more");
    assert!(normalize_term(&"x".repeat(200)).chars().count() <= MAX_TYPED);
  }

  #[test]
  fn a_suggestion_stays_on_the_topic() {
    assert!(on_topic("iphone 13 mini unlocked", "iphone"));
    assert!(on_topic("Apple iPhones lot", "iphone"));
    assert!(!on_topic("samsung galaxy s23", "iphone"));
    assert!(on_topic("iphone 13 case magsafe", "iphone case"));
    assert!(!on_topic("iphone 13 mini", "iphone case"));
    // A keyword of numbers alone keeps all of them.
    assert!(on_topic("model 3310 nokia", "3310"));
  }

  #[test]
  fn only_fresh_suggestions_on_the_topic_are_kept() {
    let candidates: Vec<String> = [
      "iphone 13 mini unlocked",
      "iPhone 13  Mini Unlocked",
      "iphone 12 mini",
      "samsung s23",
      "iphone",
      "iphone 13 mini case",
      "https://iphone.example",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let known = vec!["iphone 12 mini".to_string()];
    let kept = fresh_variants(&candidates, "iPhone", &known, 10);
    assert_eq!(kept, vec!["iphone 13 mini unlocked", "iphone 13 mini case"]);
    assert_eq!(fresh_variants(&candidates, "iphone", &[], 1).len(), 1);
  }

  #[test]
  fn a_listing_title_becomes_a_short_search_around_the_topic() {
    let mut d = Seeded::new(3);
    let titles = [
      "NEW LISTING Apple iPhone 13 mini 128GB Unlocked - Pink - Good (Read Description)",
      "Apple iPhone 15 Pro Max 256GB Natural Titanium | Factory Unlocked",
      "Lot of 3 OtterBox Defender cases for iPhone 13",
      "iPhone 12 mini",
    ];
    for title in titles {
      for _ in 0..40 {
        let term = product_keyword(title, "iphone", &mut d).expect(title);
        let words = term.split(' ').count();
        assert!((2..=5).contains(&words), "{term}");
        assert!(term.contains("iphone"), "{term}");
        for noise in ["listing", "good", "read", "description", "lot", "of", "for"] {
          assert!(!term.split(' ').any(|w| w == noise), "{term}");
        }
      }
    }
  }

  #[test]
  fn an_off_topic_listing_gives_no_search() {
    let mut d = Seeded::new(5);
    assert_eq!(
      product_keyword("Samsung Galaxy S23 Ultra 256GB", "iphone", &mut d),
      None
    );
    assert_eq!(
      product_keyword("Apple iPhone", "apple iphone", &mut d),
      None
    );
  }

  #[test]
  fn counts_stay_small_and_varied() {
    let mut d = Seeded::new(11);
    let mut seen = HashSet::new();
    for _ in 0..200 {
      let results = results_to_open(&mut d);
      let listings = listings_to_open(&mut d);
      assert!((1..=3).contains(&results));
      assert!((1..=3).contains(&listings));
      seen.insert((results, listings));
      let dwell = item_dwell_ms(&mut d);
      assert!((18_000..=130_000).contains(&dwell));
    }
    assert!(seen.len() >= 6);
  }

  #[test]
  fn the_suggestion_endpoint_is_read() {
    let body = r#"{"prefix":"iphone","res":{"stores":[["1","iPhonesPlus"]]},"richRes":{"sug":[
      {"kwd":"iphone 17 pro max","directType":"SRP","type":"s"},
      {"kwd":"iphone 13","directType":"SRP","type":"s"}]}}"#;
    assert_eq!(
      suggestion_terms(body),
      vec!["iphone 17 pro max", "iphone 13"]
    );
    let plain = r#"/**/cb({"res":{"sug":["iphone 12 mini","iphone 12"]}})"#;
    assert_eq!(suggestion_terms(plain), vec!["iphone 12 mini", "iphone 12"]);
    assert!(suggestion_terms("not json").is_empty());
    assert!(suggestion_terms("").is_empty());
  }

  #[test]
  fn google_is_asked_for_the_shop_by_name() {
    let mut d = Seeded::new(1);
    for _ in 0..50 {
      let query = google_query("iphone 13", &mut d);
      assert!(query == "iphone 13 ebay" || query == "ebay iphone 13");
    }
  }

  #[test]
  fn the_shop_is_known_by_its_domain() {
    assert!(is_shop("https://www.ebay.com/itm/1234567"));
    assert!(is_shop("https://www.ebay.co.uk/sch/i.html?_nkw=x"));
    assert!(!is_shop("https://www.google.com/search?q=ebay"));
    assert!(!is_shop("https://ebay.example.com/"));
  }
}
