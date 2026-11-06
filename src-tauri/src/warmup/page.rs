//! What the warm-up knows about the page in front of it, and how it reads it.
//!
//! The read is one script run in an isolated world of the page
//! (`Page.createIsolatedWorld` + `Runtime.evaluate` with that world's context
//! id). An isolated world sees the page's DOM but none of its JavaScript
//! objects, and the page cannot see it or anything run there. The script only
//! measures: boxes, text, computed styles and one `elementFromPoint` per
//! candidate. It changes nothing, adds no listener, and needs no
//! `Runtime.enable`.

use super::consent::ButtonCandidate;
use super::pointer::Rect;
use super::targets::LinkCandidate;
use serde::Deserialize;

/// The reading script, a function expression taking `{ consent: bool }`.
const READ_SCRIPT: &str = include_str!("read_page.js");

/// The expression that reads the page. `consent` asks for the buttons of any
/// banner as well, which costs a scan of the whole document and so is only
/// done when a banner is worth looking for.
pub fn read_expression(consent: bool) -> String {
  format!("{}({{\"consent\":{consent}}})", READ_SCRIPT.trim())
}

/// The expression that tells which link is under a point: its address.
pub fn link_at_expression(x: f64, y: f64) -> String {
  format!(
    "(function(x,y){{var e=document.elementFromPoint(x,y);var a=e&&e.closest?e.closest('a[href]'):null;return a&&typeof a.href==='string'?a.href:null;}})({x},{y})"
  )
}

/// The expression that tells the name of the control (button or link) under a
/// point, in the words the page read gave it; `null` when it is not a plain
/// one (a control inside a shadow tree is seen from outside as its host).
pub fn control_at_expression(x: f64, y: f64) -> String {
  format!(
    "(function(x,y){{var e=document.elementFromPoint(x,y);var c=e&&e.closest?e.closest('button,[role=button],a[href],input[type=button],input[type=submit]'):null;if(!c)return null;return String(c.getAttribute('aria-label')||c.innerText||c.value||c.getAttribute('title')||c.textContent||'').normalize('NFC').replace(/\\s+/g,' ').trim().slice(0,80);}})({x},{y})"
  )
}

/// One reading of the page.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
  pub url: String,
  pub vw: f64,
  pub vh: f64,
  pub dpr: f64,
  pub scroll_y: f64,
  pub scroll_h: f64,
  /// Characters of text on screen.
  #[serde(default)]
  pub visible_chars: f64,
  #[serde(default)]
  pub links: Vec<LinkCandidate>,
  /// Buttons of a banner, only when the read asked for them.
  #[serde(default)]
  pub buttons: Vec<ButtonCandidate>,
  /// Benign things to rest the pointer on: headings, text, pictures.
  #[serde(default)]
  pub hover: Vec<Rect>,
}

impl Snapshot {
  pub fn viewport(&self) -> (f64, f64) {
    (self.vw, self.vh)
  }

  /// Whether the window already shows the end of the page.
  pub fn at_bottom(&self) -> bool {
    self.scroll_y + self.vh >= self.scroll_h - 4.0
  }

  /// How far down the page the window is, 0 to 1.
  pub fn scrolled_fraction(&self) -> f64 {
    let room = (self.scroll_h - self.vh).max(1.0);
    (self.scroll_y / room).clamp(0.0, 1.0)
  }

  /// A page worth reading: a real document, not Chrome's error page or a blank.
  pub fn is_document(&self) -> bool {
    !(self.url.starts_with("chrome-error:")
      || self.url.starts_with("about:")
      || self.url.starts_with("chrome:")
      || self.url.is_empty())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_read_is_one_call_of_the_script_with_its_options() {
    let expression = read_expression(true);
    assert!(expression.starts_with("(function (options)"));
    assert!(expression.ends_with("({\"consent\":true})"));
    assert!(read_expression(false).ends_with("({\"consent\":false})"));
  }

  #[test]
  fn the_script_only_reads() {
    // It must never act on the page: no click, no event, no listener, no write.
    for forbidden in [
      ".click(",
      "dispatchEvent",
      "addEventListener",
      "scrollTo",
      "scrollBy",
      "scrollIntoView",
      ".focus(",
      ".submit(",
      "innerHTML =",
      "textContent =",
      ".value =",
      "setAttribute",
      "localStorage",
      "document.cookie",
      "fetch(",
      "XMLHttpRequest",
    ] {
      assert!(!READ_SCRIPT.contains(forbidden), "{forbidden}");
    }
  }

  #[test]
  fn a_point_lookup_is_numbers_only() {
    let expression = link_at_expression(120.5, 340.0);
    assert!(expression.ends_with("})(120.5,340)") || expression.ends_with(")(120.5,340)"));
  }

  #[test]
  fn a_reading_parses_and_knows_where_it_is() {
    let reading: Snapshot = serde_json::from_value(serde_json::json!({
      "url": "https://example.com/", "vw": 1280, "vh": 700, "dpr": 2,
      "scrollY": 1300, "scrollH": 2000, "visibleChars": 840,
      "links": [{
        "href": "https://example.com/a", "text": "A", "rect": { "x": 10, "y": 20, "w": 100, "h": 20 },
        "region": "content", "newTab": false, "download": false, "rel": ""
      }],
      "hover": [{ "x": 1, "y": 2, "w": 300, "h": 40 }],
      "buttons": [{
        "name": "Accept all", "rect": { "x": 1, "y": 2, "w": 120, "h": 40 },
        "inBanner": true, "context": "We use cookies"
      }]
    }))
    .unwrap();
    assert!(reading.at_bottom());
    assert!((reading.scrolled_fraction() - 1.0).abs() < 1e-9);
    assert_eq!(reading.links.len(), 1);
    assert_eq!(reading.buttons.len(), 1);
    assert_eq!(reading.hover.len(), 1);
    assert!(reading.is_document());

    let top: Snapshot = serde_json::from_value(serde_json::json!({
      "url": "chrome-error://chromewebdata/", "vw": 800, "vh": 600, "dpr": 1,
      "scrollY": 0, "scrollH": 600
    }))
    .unwrap();
    assert!(!top.is_document());
    assert!(top.at_bottom(), "a page that fits has nothing below");
    assert_eq!(top.scrolled_fraction(), 0.0);
  }
}
