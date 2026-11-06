//! What a shopper sees on a search engine or a shop: the search box and its
//! suggestions, the results that lead to the shop, the listings, the searches
//! the shop offers next, and an item's pictures.
//!
//! Read by one more script in the same isolated world as `page.rs`, under the
//! same rule: it measures and returns, and never acts on the page.

use super::pointer::Rect;
use serde::Deserialize;

const SHOP_SCRIPT: &str = include_str!("read_shop.js");

pub fn shop_expression() -> String {
  SHOP_SCRIPT.trim().to_string()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchField {
  pub rect: Rect,
  #[serde(default)]
  pub value: String,
  #[serde(default)]
  pub focused: bool,
  /// The form's search button, when one shows.
  #[serde(default)]
  pub submit: Option<Rect>,
}

/// A search engine's result that leads to the shop: where its title is.
#[derive(Clone, Debug, Deserialize)]
pub struct ShopResult {
  pub rect: Rect,
}

/// A suggestion of a search box, read even once the list has closed.
#[derive(Clone, Debug, Deserialize)]
pub struct Suggestion {
  pub text: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Listing {
  pub id: String,
  pub title: String,
  pub rect: Rect,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ItemPage {
  pub title: String,
  #[serde(default)]
  pub pictures: Vec<Rect>,
  #[serde(default)]
  pub next: Option<Rect>,
  #[serde(default)]
  pub picture: Option<Rect>,
}

/// Something over the page that is answered before anything under it: where
/// its close button is, and its confirm button when the page asks where to ship.
#[derive(Clone, Debug, Deserialize)]
pub struct Blocker {
  #[serde(default)]
  pub close: Option<Rect>,
  #[serde(default)]
  pub confirm: Option<Rect>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShopReading {
  pub url: String,
  pub vw: f64,
  pub vh: f64,
  pub dpr: f64,
  pub scroll_y: f64,
  pub scroll_h: f64,
  /// The page is a bot check, not the page that was asked for.
  #[serde(default)]
  pub challenged: bool,
  #[serde(default)]
  pub blocker: Option<Blocker>,
  #[serde(default)]
  pub field: Option<SearchField>,
  /// The search box's suggestions for what was typed.
  #[serde(default)]
  pub suggestions: Vec<Suggestion>,
  /// A search engine's results that lead to the shop, on screen.
  #[serde(default)]
  pub results: Vec<ShopResult>,
  #[serde(default)]
  pub results_below: u32,
  /// The shop's listings, on screen.
  #[serde(default)]
  pub items: Vec<Listing>,
  #[serde(default)]
  pub items_below: u32,
  /// The searches the shop suggests next to its results.
  #[serde(default)]
  pub related: Vec<String>,
  #[serde(default)]
  pub item: Option<ItemPage>,
}

impl ShopReading {
  pub fn viewport(&self) -> (f64, f64) {
    (self.vw, self.vh)
  }

  pub fn at_bottom(&self) -> bool {
    self.scroll_y + self.vh >= self.scroll_h - 4.0
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_shop_script_only_reads() {
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
      assert!(!SHOP_SCRIPT.contains(forbidden), "{forbidden}");
    }
  }

  #[test]
  fn a_reading_of_a_results_page_parses() {
    let reading: ShopReading = serde_json::from_value(serde_json::json!({
      "url": "https://www.ebay.com/sch/i.html?_nkw=iphone+13+mini",
      "vw": 1280, "vh": 760, "dpr": 2, "scrollY": 0, "scrollH": 21000,
      "challenged": false,
      "field": { "rect": { "x": 300, "y": 20, "w": 600, "h": 40 }, "value": "iphone 13 mini",
                 "focused": false, "submit": null },
      "suggestions": [{ "text": "iphone 13 mini 128gb", "rect": null }],
      "results": [], "resultsBelow": 0,
      "items": [{ "id": "1234567", "title": "Apple iPhone 13 mini 128GB",
                  "rect": { "x": 400, "y": 300, "w": 500, "h": 22 } }],
      "itemsBelow": 59,
      "related": ["iphone 13 mini unlocked", "iphone 12 mini"],
      "item": null
    }))
    .unwrap();
    assert_eq!(reading.suggestions[0].text, "iphone 13 mini 128gb");
    assert_eq!(reading.items.len(), 1);
    assert_eq!(reading.items_below, 59);
    assert_eq!(reading.related.len(), 2);
    assert!(!reading.at_bottom());
    assert!(reading.blocker.is_none());
  }

  #[test]
  fn a_shipping_question_over_the_page_is_read() {
    let reading: ShopReading = serde_json::from_value(serde_json::json!({
      "url": "https://www.ebay.com/sch/i.html?_nkw=iphone",
      "vw": 1280, "vh": 760, "dpr": 2, "scrollY": 0, "scrollH": 9000,
      "blocker": { "close": { "x": 700, "y": 200, "w": 32, "h": 32 },
                   "confirm": { "x": 300, "y": 330, "w": 420, "h": 30 } }
    }))
    .unwrap();
    let blocker = reading.blocker.unwrap();
    assert!(blocker.close.is_some());
    assert!(blocker.confirm.is_some());
  }
}
