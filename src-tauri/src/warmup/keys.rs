//! The keyboard: which physical key makes a character, and the rhythm of a
//! person typing a search.
//!
//! Each character becomes a key that goes down and comes up again, with Shift
//! held around capitals and symbols, at the times the human typing model
//! (`human_typing`) draws: uneven gaps, faster on common words, a slip now and
//! then that is noticed and corrected with Backspace. What the field holds at
//! the end is always exactly the text.

use super::rhythm::Draw;
use crate::human_typing::{MarkovTyper, TypingAction};
use crate::paste_typing::{replay, us_key, Stroke};

/// A key of a US keyboard, as DevTools names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
  /// The DOM `key` value: what the key types, or its name.
  pub key: &'static str,
  /// The DOM `code` value: where the key is.
  pub code: &'static str,
  /// The Windows virtual key code, which Chromium also reports on other OSes.
  pub vk: u32,
}

pub const ENTER: Key = Key {
  key: "Enter",
  code: "Enter",
  vk: 13,
};
pub const BACKSPACE: Key = Key {
  key: "Backspace",
  code: "Backspace",
  vk: 8,
};
pub const SHIFT: Key = Key {
  key: "Shift",
  code: "ShiftLeft",
  vk: 16,
};
pub const META: Key = Key {
  key: "Meta",
  code: "MetaLeft",
  vk: 91,
};
pub const CONTROL: Key = Key {
  key: "Control",
  code: "ControlLeft",
  vk: 17,
};

const LETTERS: [&str; 26] = [
  "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK", "KeyL",
  "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV", "KeyW", "KeyX",
  "KeyY", "KeyZ",
];
const DIGITS: [&str; 10] = [
  "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8",
  "Digit9",
];

/// Where the unshifted key `base` sits, and its virtual key code.
fn place(base: char) -> Option<(&'static str, u32)> {
  Some(match base {
    'a'..='z' => (
      LETTERS[(base as u8 - b'a') as usize],
      u32::from(base.to_ascii_uppercase()),
    ),
    '0'..='9' => (DIGITS[(base as u8 - b'0') as usize], u32::from(base)),
    ' ' => ("Space", 32),
    '-' => ("Minus", 189),
    '=' => ("Equal", 187),
    '[' => ("BracketLeft", 219),
    ']' => ("BracketRight", 221),
    '\\' => ("Backslash", 220),
    ';' => ("Semicolon", 186),
    '\'' => ("Quote", 222),
    ',' => ("Comma", 188),
    '.' => ("Period", 190),
    '/' => ("Slash", 191),
    '`' => ("Backquote", 192),
    _ => return None,
  })
}

/// How to type `c` on a US keyboard: the key's position and code, and whether
/// Shift is held. `None` for a character no key makes, which is entered as
/// text instead.
pub fn key_for(c: char) -> Option<(&'static str, u32, bool)> {
  let (base, shift) = us_key(c)?;
  let (code, vk) = place(base)?;
  Some((code, vk, shift))
}

/// One key press of a plan.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyPress {
  /// When the key goes down, from the start of the typing.
  pub at_ms: f64,
  /// How long it is held.
  pub hold_ms: f64,
  pub stroke: Stroke,
}

/// The key presses that type `text` at a pace drawn for this person. The
/// typing model's slips are kept when its plan ends on exactly `text`; after a
/// few plans that do not, the text is typed cleanly at the same pace.
pub fn plan_typing(text: &str, d: &mut impl Draw) -> Vec<KeyPress> {
  let wpm = d.log_normal(52.0, 0.22).clamp(28.0, 90.0);
  let mut strokes: Option<Vec<(f64, Stroke)>> = None;
  for _ in 0..5 {
    let events = MarkovTyper::new(text, Some(wpm)).run();
    let planned: Vec<(f64, Stroke)> = events
      .iter()
      .map(|event| {
        let stroke = match event.action {
          TypingAction::Char(c) => Stroke::Char(c),
          TypingAction::Backspace => Stroke::Backspace,
        };
        (event.time * 1_000.0, stroke)
      })
      .collect();
    if replay(planned.iter().map(|(_, stroke)| *stroke)) == text {
      strokes = Some(planned);
      break;
    }
  }
  let strokes = strokes.unwrap_or_else(|| {
    let per_char = 60_000.0 / (wpm * 5.0);
    let mut at = 0.0;
    text
      .chars()
      .map(|c| {
        at += d.log_normal(per_char, 0.3).clamp(45.0, 900.0);
        (at, Stroke::Char(c))
      })
      .collect()
  });
  let mut presses: Vec<KeyPress> = strokes
    .into_iter()
    .map(|(at_ms, stroke)| KeyPress {
      at_ms,
      hold_ms: d.log_normal(82.0, 0.25).clamp(40.0, 170.0),
      stroke,
    })
    .collect();
  // A key is let go before the next one goes down: no rollover, so the order
  // of down and up events never surprises a page.
  for index in 0..presses.len().saturating_sub(1) {
    let room = presses[index + 1].at_ms - presses[index].at_ms - 8.0;
    presses[index].hold_ms = presses[index].hold_ms.min(room.max(12.0));
  }
  presses
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::warmup::rhythm::Seeded;

  #[test]
  fn every_printable_ascii_character_has_a_key() {
    for c in (' '..='~').filter(|c| *c != '\u{7f}') {
      assert!(key_for(c).is_some(), "{c:?}");
    }
    assert_eq!(key_for('a'), Some(("KeyA", 65, false)));
    assert_eq!(key_for('Q'), Some(("KeyQ", 81, true)));
    assert_eq!(key_for('7'), Some(("Digit7", 55, false)));
    assert_eq!(key_for('&'), Some(("Digit7", 55, true)));
    assert_eq!(key_for(' '), Some(("Space", 32, false)));
    assert_eq!(key_for('é'), None);
  }

  #[test]
  fn a_plan_ends_on_exactly_the_text() {
    let mut d = Seeded::new(4);
    for text in ["iphone 13 mini", "Apple iPhone 15 Pro Max 256GB", "a"] {
      for _ in 0..20 {
        let plan = plan_typing(text, &mut d);
        assert_eq!(replay(plan.iter().map(|press| press.stroke)), text);
      }
    }
  }

  #[test]
  fn keys_go_down_in_order_and_come_up_before_the_next() {
    let mut d = Seeded::new(9);
    let plan = plan_typing("iphone 14 pro max", &mut d);
    for pair in plan.windows(2) {
      assert!(pair[1].at_ms > pair[0].at_ms);
      assert!(pair[0].at_ms + pair[0].hold_ms < pair[1].at_ms);
    }
  }

  #[test]
  fn the_pace_is_uneven() {
    let mut d = Seeded::new(2);
    let plan = plan_typing("iphone 13 pro max unlocked", &mut d);
    let gaps: Vec<f64> = plan.windows(2).map(|p| p[1].at_ms - p[0].at_ms).collect();
    assert!(crate::warmup::rhythm::variation(&gaps) > 0.2);
  }
}
