//! How a page is scrolled by a person: bursts of wheel events, not a jump.
//!
//! A mouse wheel turns in notches: a few events a few tens of milliseconds
//! apart, each the same size. A trackpad (the Mac) sends a stream of small
//! events, one per frame, that grows as the fingers move and then decays on its
//! own (momentum) after they lift. Either way the page moves over a fraction of
//! a second, the amount differs every time, and a pause follows while the
//! person reads. `window.scrollTo` and an instant jump are what a script does.

use super::rhythm::Draw;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollStyle {
  /// A wheel mouse: notches of 100 or 120 pixels.
  Wheel,
  /// A trackpad: a ramp and a decaying tail of small deltas.
  Trackpad,
}

impl ScrollStyle {
  /// The hardware a persona claiming `os` would scroll with.
  pub fn for_os(os: &str) -> Self {
    if os.eq_ignore_ascii_case("macos") {
      ScrollStyle::Trackpad
    } else {
      ScrollStyle::Wheel
    }
  }
}

/// One wheel event: when, and how far (positive scrolls down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelEvent {
  pub at_ms: f64,
  pub delta_y: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScrollPlan {
  pub events: Vec<WheelEvent>,
  /// The distance the events add up to, signed.
  pub total_px: f64,
  pub duration_ms: f64,
}

/// How far a person scrolls in one go, from a fraction of the window to a bit
/// over it. `viewport_height` is in CSS pixels.
pub fn intent_px(viewport_height: f64, d: &mut impl Draw) -> f64 {
  let height = viewport_height.max(200.0);
  (height * d.log_normal(0.62, 0.35)).clamp(height * 0.2, height * 1.5)
}

/// Plan a burst that scrolls about `intent_px` pixels, down or up.
pub fn plan_scroll(
  style: ScrollStyle,
  intent_px: f64,
  down: bool,
  d: &mut impl Draw,
) -> ScrollPlan {
  let sign = if down { 1.0 } else { -1.0 };
  let intent = intent_px.max(20.0);
  let mut events = Vec::new();
  match style {
    ScrollStyle::Wheel => {
      let notch = if d.chance(0.2) { 120.0 } else { 100.0 };
      let notches = ((intent / notch * d.range(0.85, 1.15)).round() as usize).max(1);
      let mut at = 0.0;
      for index in 0..notches {
        if index > 0 {
          at += if notches >= 4 && d.chance(0.1) {
            d.range(200.0, 520.0)
          } else {
            d.log_normal(58.0, 0.5).clamp(16.0, 240.0)
          };
        }
        events.push(WheelEvent {
          at_ms: at,
          delta_y: sign * notch,
        });
      }
    }
    ScrollStyle::Trackpad => {
      let decay = d.range(0.90, 0.945);
      let peak = intent * (1.0 - decay);
      let base_interval = d.range(11.0, 17.0);
      let mut at = 0.0;
      let mut index = 0usize;
      loop {
        let ramp = match index {
          0 => 0.25,
          1 => 0.6,
          2 => 0.9,
          n => decay.powi(n as i32 - 2),
        };
        let delta = (peak * ramp * d.range(0.9, 1.1)).round();
        if index >= 3 && delta < 1.0 {
          break;
        }
        if delta >= 1.0 {
          events.push(WheelEvent {
            at_ms: at,
            delta_y: sign * delta,
          });
        }
        index += 1;
        at += base_interval * d.range(0.8, 1.25);
        if index > 140 {
          break;
        }
      }
    }
  }
  let total_px = events.iter().map(|event| event.delta_y).sum();
  let duration_ms = events.last().map_or(0.0, |event| event.at_ms);
  ScrollPlan {
    events,
    total_px,
    duration_ms,
  }
}

/// Whether to scroll back up a little instead of down: people do it to re-read
/// something, more often the further down they are.
pub fn scrolls_back(scrolled_fraction: f64, d: &mut impl Draw) -> bool {
  d.chance(0.05 + 0.12 * scrolled_fraction.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
  use super::super::rhythm::{variation, Seeded};
  use super::*;

  #[test]
  fn a_trackpad_ramps_up_then_decays_like_momentum() {
    for seed in 0..200 {
      let mut d = Seeded::new(seed);
      let plan = plan_scroll(ScrollStyle::Trackpad, 500.0, true, &mut d);
      let deltas: Vec<f64> = plan.events.iter().map(|event| event.delta_y).collect();
      assert!(deltas.len() >= 20, "{} events", deltas.len());
      assert!(deltas.iter().all(|delta| *delta >= 1.0));
      let peak = deltas.iter().copied().fold(0.0, f64::max);
      let peak_at = deltas.iter().position(|delta| *delta == peak).unwrap();
      assert!(peak_at <= 6, "the stream peaks soon after it starts");
      assert!(deltas[0] < peak, "it ramps up");
      // The tail decays: the second half is smaller than the first.
      let half = deltas.len() / 2;
      let first: f64 = deltas[..half].iter().sum();
      let second: f64 = deltas[half..].iter().sum();
      assert!(first > second * 1.5, "{first} vs {second}");
      assert!(
        plan.total_px > 500.0 * 0.6 && plan.total_px < 500.0 * 1.5,
        "{}",
        plan.total_px
      );
    }
  }

  #[test]
  fn a_wheel_turns_in_equal_notches_at_uneven_times() {
    let mut gaps = Vec::new();
    for seed in 0..300 {
      let mut d = Seeded::new(seed);
      let plan = plan_scroll(ScrollStyle::Wheel, 450.0, true, &mut d);
      let notch = plan.events[0].delta_y;
      assert!(notch == 100.0 || notch == 120.0);
      assert!(plan.events.iter().all(|event| event.delta_y == notch));
      assert!(
        (1..=7).contains(&plan.events.len()),
        "{}",
        plan.events.len()
      );
      assert_eq!(plan.events[0].at_ms, 0.0);
      gaps.extend(
        plan
          .events
          .windows(2)
          .map(|pair| pair[1].at_ms - pair[0].at_ms),
      );
      assert!(plan.total_px > 450.0 * 0.55 && plan.total_px < 450.0 * 1.6);
    }
    assert!(gaps.iter().all(|gap| *gap >= 16.0));
    assert!(variation(&gaps) > 0.3, "gaps vary: {}", variation(&gaps));
  }

  #[test]
  fn scrolling_up_reverses_the_sign_and_nothing_is_instant() {
    for style in [ScrollStyle::Wheel, ScrollStyle::Trackpad] {
      let mut d = Seeded::new(9);
      let up = plan_scroll(style, 400.0, false, &mut d);
      assert!(up.total_px < 0.0 && up.events.iter().all(|event| event.delta_y < 0.0));
      let mut d = Seeded::new(10);
      let down = plan_scroll(style, 400.0, true, &mut d);
      assert!(down.total_px > 0.0);
      assert!(down.events.len() > 1, "one big event is a jump");
      assert!(down.events.iter().all(|event| event.delta_y.abs() <= 130.0));
      assert!(down.duration_ms >= 15.0);
    }
  }

  #[test]
  fn the_distance_drawn_is_a_fraction_of_the_window() {
    let mut d = Seeded::new(3);
    let amounts: Vec<f64> = (0..3_000).map(|_| intent_px(800.0, &mut d)).collect();
    assert!(amounts.iter().all(|px| (160.0..=1_200.0).contains(px)));
    assert!(variation(&amounts) > 0.2);
    let mean = amounts.iter().sum::<f64>() / amounts.len() as f64;
    assert!((400.0..700.0).contains(&mean), "{mean}");
  }

  #[test]
  fn the_hardware_follows_the_claimed_os() {
    assert_eq!(ScrollStyle::for_os("macos"), ScrollStyle::Trackpad);
    assert_eq!(ScrollStyle::for_os("windows"), ScrollStyle::Wheel);
    assert_eq!(ScrollStyle::for_os("linux"), ScrollStyle::Wheel);
  }

  #[test]
  fn scrolling_back_is_rare_and_likelier_further_down() {
    let rate = |fraction: f64| {
      let mut d = Seeded::new(5);
      (0..20_000)
        .filter(|_| scrolls_back(fraction, &mut d))
        .count() as f64
        / 20_000.0
    };
    assert!(rate(0.0) < 0.08 && rate(1.0) > 0.12 && rate(1.0) < 0.22);
  }
}
