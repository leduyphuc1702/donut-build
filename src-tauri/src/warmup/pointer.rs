//! How a hand moves a mouse, as a list of pointer events with times.
//!
//! A pointer path is never a line and never a teleport. These planners draw one
//! the way people produce it:
//!
//! - the time a move takes follows Fitts's law: it grows with the distance and
//!   shrinks with how big the target is;
//! - the path is a curve, and its speed rises and falls smoothly (the
//!   minimum-jerk profile), with a small wobble across the line of travel;
//! - a long move toward a small target often overshoots a little, stops, and
//!   corrects with a second short move;
//! - events arrive at 60 to 125 Hz with jitter, never at a fixed interval, and
//!   sit on the screen's physical pixel grid;
//! - a click lands inside the target, off-centre, and the button stays down for
//!   a human 60 to 180 ms.
//!
//! Everything here is a pure function of the draws it is handed. The driver
//! replays the plans through `Input.dispatchMouseEvent`.

use super::rhythm::Draw;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
pub struct Point {
  pub x: f64,
  pub y: f64,
}

impl Point {
  pub fn new(x: f64, y: f64) -> Self {
    Point { x, y }
  }

  pub fn distance(self, other: Point) -> f64 {
    (self.x - other.x).hypot(self.y - other.y)
  }
}

/// A box in viewport coordinates (CSS pixels from the top left of the page's
/// visible area).
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
pub struct Rect {
  pub x: f64,
  pub y: f64,
  pub w: f64,
  pub h: f64,
}

impl Rect {
  pub fn center(&self) -> Point {
    Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
  }

  pub fn area(&self) -> f64 {
    self.w.max(0.0) * self.h.max(0.0)
  }

  pub fn contains(&self, point: Point) -> bool {
    point.x >= self.x
      && point.x <= self.x + self.w
      && point.y >= self.y
      && point.y <= self.y + self.h
  }
}

/// One pointer event of a move: when (milliseconds after the move began) and
/// where.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
  pub at_ms: f64,
  pub x: f64,
  pub y: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MovePlan {
  pub steps: Vec<Step>,
  pub duration_ms: f64,
}

impl MovePlan {
  /// Where the pointer ends up, if the plan moves it.
  #[cfg(test)]
  pub fn end(&self) -> Option<Point> {
    self.steps.last().map(|step| Point::new(step.x, step.y))
  }
}

/// A coordinate on the physical pixel grid of a screen with `dpr` device
/// pixels per CSS pixel. A real pointer reports positions on that grid: whole
/// numbers on a 1x screen, halves on a 2x one.
pub fn snap(value: f64, dpr: f64) -> f64 {
  let dpr = if dpr.is_finite() {
    dpr.clamp(1.0, 4.0)
  } else {
    1.0
  };
  (value * dpr).round() / dpr
}

fn snap_point(point: Point, dpr: f64) -> Point {
  Point::new(snap(point.x, dpr), snap(point.y, dpr))
}

/// Fitts's law: the time to reach a target `width` wide that is `distance`
/// away, in milliseconds. `a + b * log2(distance / width + 1)` with the
/// constants of a person at a desktop mouse, drawn per move so two people, or
/// one person on two moves, differ.
pub fn fitts_ms(distance: f64, width: f64, d: &mut impl Draw) -> f64 {
  let width = width.clamp(6.0, 240.0);
  let difficulty = (distance.max(0.0) / width + 1.0).log2();
  let start = d.range(95.0, 150.0);
  let per_bit = d.range(110.0, 170.0);
  ((start + per_bit * difficulty) * d.log_normal(1.0, 0.1)).clamp(90.0, 2_600.0)
}

/// The minimum-jerk position profile: 0 to 1 as `t` goes 0 to 1, slow at both
/// ends, fastest in the middle.
fn minimum_jerk(t: f64) -> f64 {
  let t = t.clamp(0.0, 1.0);
  t * t * t * (10.0 - 15.0 * t + 6.0 * t * t)
}

fn cubic(p0: Point, p1: Point, p2: Point, p3: Point, s: f64) -> Point {
  let u = 1.0 - s;
  let (a, b, c, e) = (u * u * u, 3.0 * u * u * s, 3.0 * u * s * s, s * s * s);
  Point::new(
    a * p0.x + b * p1.x + c * p2.x + e * p3.x,
    a * p0.y + b * p1.y + c * p2.y + e * p3.y,
  )
}

/// One smooth stroke from `from` to `to` in `duration_ms`, as unquantized
/// points with their times (offset by `at_ms`). The last point is exactly `to`.
fn stroke(
  from: Point,
  to: Point,
  duration_ms: f64,
  at_ms: f64,
  d: &mut impl Draw,
  out: &mut Vec<(f64, Point)>,
) {
  let distance = from.distance(to);
  if distance < 0.01 || duration_ms <= 0.0 {
    out.push((at_ms + duration_ms.max(0.0), to));
    return;
  }
  let (ux, uy) = ((to.x - from.x) / distance, (to.y - from.y) / distance);
  let (px, py) = (-uy, ux);
  // How far the path bends away from the straight line, in the fraction of the
  // distance. A short move bends little.
  let bend = d.range(0.03, 0.14) * d.sign() * (distance / 180.0).min(1.0);
  let other_bend = bend * d.range(-0.5, 1.1);
  let (along1, along2) = (d.range(0.18, 0.38), d.range(0.62, 0.85));
  let p1 = Point::new(
    from.x + (to.x - from.x) * along1 + px * bend * distance,
    from.y + (to.y - from.y) * along1 + py * bend * distance,
  );
  let p2 = Point::new(
    from.x + (to.x - from.x) * along2 + px * other_bend * distance,
    from.y + (to.y - from.y) * along2 + py * other_bend * distance,
  );
  // A hand reaches its top speed a little before the middle of the stroke.
  let early = d.range(0.86, 1.0);
  let wobble = d.range(0.3, 1.4) * (distance / 120.0).min(1.0);
  let wobble_turns = d.range(1.0, 2.5);
  let wobble_phase = d.range(0.0, std::f64::consts::TAU);
  // Between 62 and 125 events a second.
  let base_interval = d.range(8.0, 15.5);

  let ideal = |t: f64| {
    let tau = t / duration_ms;
    let on_curve = cubic(from, p1, p2, to, minimum_jerk(tau.powf(early)));
    let across = wobble
      * (std::f64::consts::TAU * wobble_turns * tau + wobble_phase).sin()
      * (std::f64::consts::PI * tau).sin();
    Point::new(on_curve.x + px * across, on_curve.y + py * across)
  };

  let mut t = 0.0;
  let mut previous = from;
  loop {
    let mut gap = (base_interval * d.range(0.78, 1.3)).max(MIN_EVENT_GAP_MS);
    // The fastest part of a long stroke covers many pixels between two events
    // at a 60 Hz report rate; a mouse that polls faster reports it in smaller
    // steps, so the next event comes sooner rather than farther.
    let (next_t, next) = loop {
      let candidate = ideal(t + gap);
      if candidate.distance(previous) <= MAX_EVENT_STEP_PX || gap <= MIN_EVENT_GAP_MS {
        break (t + gap, candidate);
      }
      gap = (gap * 0.75).max(MIN_EVENT_GAP_MS);
    };
    if next_t >= duration_ms - 3.0 {
      break;
    }
    t = next_t;
    previous = next;
    out.push((
      at_ms + t,
      Point::new(next.x + d.gauss(0.0, 0.28), next.y + d.gauss(0.0, 0.28)),
    ));
  }
  out.push((at_ms + duration_ms, to));
}

/// The longest distance between two consecutive events of a stroke.
pub const MAX_EVENT_STEP_PX: f64 = 42.0;
const MIN_EVENT_GAP_MS: f64 = 4.0;

/// Plan the pointer's way from `from` to `to`, a target about `target_width`
/// across. `dpr` is the page's device pixel ratio, for the pixel grid.
pub fn plan_move(
  from: Point,
  to: Point,
  target_width: f64,
  dpr: f64,
  d: &mut impl Draw,
) -> MovePlan {
  let to = snap_point(to, dpr);
  let distance = from.distance(to);
  if distance < 0.5 {
    return MovePlan::default();
  }
  let total = fitts_ms(distance, target_width, d);
  let mut raw: Vec<(f64, Point)> = Vec::new();
  let overshoots = distance > 240.0 && target_width < 220.0 && d.chance(0.45);
  if overshoots {
    let (ux, uy) = ((to.x - from.x) / distance, (to.y - from.y) / distance);
    let amount = (distance * d.range(0.025, 0.07)).clamp(5.0, 36.0);
    let side = d.gauss(0.0, amount * 0.25);
    let past = Point::new(
      to.x + ux * amount - uy * side,
      to.y + uy * amount + ux * side,
    );
    let main = total * 0.8;
    stroke(from, past, main, 0.0, d, &mut raw);
    let pause = d.range(35.0, 110.0);
    let correction = d.log_normal(170.0, 0.25).clamp(110.0, 320.0);
    stroke(past, to, correction, main + pause, d, &mut raw);
  } else {
    stroke(from, to, total, 0.0, d, &mut raw);
  }

  let mut steps: Vec<Step> = Vec::with_capacity(raw.len());
  let mut previous = snap_point(from, dpr);
  let last = raw.len().saturating_sub(1);
  for (index, (at_ms, point)) in raw.into_iter().enumerate() {
    // The last event is the target itself, on the grid, however the noise fell.
    let point = if index == last {
      to
    } else {
      snap_point(point, dpr)
    };
    // A mouse sends nothing while it has not moved.
    if point == previous {
      continue;
    }
    previous = point;
    steps.push(Step {
      at_ms,
      x: point.x,
      y: point.y,
    });
  }
  let duration_ms = steps.last().map_or(0.0, |step| step.at_ms);
  MovePlan { steps, duration_ms }
}

/// Where to press inside `rect`: a Gaussian around a point a little left of
/// and below the middle, kept off the box's edge and never on its exact centre.
pub fn plan_click_point(rect: Rect, dpr: f64, d: &mut impl Draw) -> Point {
  let center = rect.center();
  let margin_x = (rect.w * 0.15).min(3.0);
  let margin_y = (rect.h * 0.15).min(3.0);
  let inner = Rect {
    x: rect.x + margin_x,
    y: rect.y + margin_y,
    w: (rect.w - 2.0 * margin_x).max(0.0),
    h: (rect.h - 2.0 * margin_y).max(0.0),
  };
  let aim = Point::new(
    center.x + rect.w * d.range(-0.05, 0.015),
    center.y + rect.h * d.range(-0.03, 0.05),
  );
  let spread_x = (inner.w * 0.17).clamp(0.8, 80.0);
  let spread_y = (inner.h * 0.17).clamp(0.8, 40.0);
  let step = 1.0 / dpr.clamp(1.0, 4.0);
  for _ in 0..24 {
    let point = snap_point(
      Point::new(d.gauss(aim.x, spread_x), d.gauss(aim.y, spread_y)),
      dpr,
    );
    let off_center = point.distance(center) >= step.max(0.9);
    if inner.contains(point) && off_center {
      return point;
    }
  }
  // The draws kept missing a small box: a fixed offset inside it.
  let fallback = Point::new(
    center.x + d.sign() * (inner.w * 0.2).max(step),
    center.y + (inner.h * 0.12).max(step),
  );
  let fallback = snap_point(fallback, dpr);
  if inner.contains(fallback) || inner.w <= 0.0 || inner.h <= 0.0 {
    fallback
  } else {
    snap_point(
      Point::new(inner.x + inner.w * 0.4, inner.y + inner.h * 0.6),
      dpr,
    )
  }
}

/// The small nudge of the pointer while the button is down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Nudge {
  pub at_ms: u64,
  pub dx: f64,
  pub dy: f64,
}

/// How one click is timed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClickPlan {
  /// The pause between the pointer arriving and the press.
  pub press_after_ms: u64,
  /// How long the button stays down.
  pub hold_ms: u64,
  /// Now and then the hand shifts a pixel between press and release.
  pub nudge: Option<Nudge>,
}

pub const MIN_CLICK_HOLD_MS: u64 = 60;
pub const MAX_CLICK_HOLD_MS: u64 = 180;

pub fn plan_click(dpr: f64, d: &mut impl Draw) -> ClickPlan {
  let press_after_ms = d.log_normal(150.0, 0.45).clamp(70.0, 900.0) as u64;
  let hold_ms = (d
    .log_normal(92.0, 0.28)
    .clamp(MIN_CLICK_HOLD_MS as f64, MAX_CLICK_HOLD_MS as f64)) as u64;
  let nudge = d.chance(0.25).then(|| {
    let step = 1.0 / dpr.clamp(1.0, 4.0);
    Nudge {
      at_ms: d.range(12.0, (hold_ms as f64 - 18.0).max(13.0)) as u64,
      dx: d.sign() * step * if d.chance(0.5) { 1.0 } else { 0.0 },
      dy: d.sign() * step,
    }
  });
  ClickPlan {
    press_after_ms,
    hold_ms,
    nudge,
  }
}

/// A short drift of the pointer while a person reads: a few to ninety pixels
/// in a random direction, kept inside the visible page.
pub fn drift_target(from: Point, viewport: (f64, f64), d: &mut impl Draw) -> Point {
  let length = d.log_normal(22.0, 0.6).clamp(4.0, 90.0);
  let heading = d.range(0.0, std::f64::consts::TAU);
  let margin = 6.0;
  Point::new(
    (from.x + heading.cos() * length).clamp(margin, (viewport.0 - margin).max(margin)),
    (from.y + heading.sin() * length).clamp(margin, (viewport.1 - margin).max(margin)),
  )
}

/// Where a pointer that has never touched the page first appears: at an edge,
/// as if it had come in from outside the window.
pub fn entry_point(viewport: (f64, f64), dpr: f64, d: &mut impl Draw) -> Point {
  let (w, h) = (viewport.0.max(40.0), viewport.1.max(40.0));
  let point = match d.index(3) {
    0 => Point::new(d.range(0.15, 0.85) * w, d.range(3.0, 14.0)),
    1 => Point::new(d.range(3.0, 14.0), d.range(0.1, 0.8) * h),
    _ => Point::new(w - d.range(3.0, 14.0), d.range(0.1, 0.8) * h),
  };
  snap_point(point, dpr)
}

#[cfg(test)]
mod tests {
  use super::super::rhythm::{variation, Seeded};
  use super::*;

  fn jumps(from: Point, plan: &MovePlan) -> Vec<f64> {
    let mut previous = from;
    plan
      .steps
      .iter()
      .map(|step| {
        let here = Point::new(step.x, step.y);
        let jump = previous.distance(here);
        previous = here;
        jump
      })
      .collect()
  }

  #[test]
  fn a_move_time_follows_fitts_law() {
    let mean = |distance: f64, width: f64| {
      let mut d = Seeded::new(11);
      (0..3_000)
        .map(|_| fitts_ms(distance, width, &mut d))
        .sum::<f64>()
        / 3_000.0
    };
    assert!(
      mean(800.0, 30.0) > mean(100.0, 30.0) * 1.5,
      "farther takes longer"
    );
    assert!(
      mean(400.0, 12.0) > mean(400.0, 160.0) * 1.3,
      "smaller takes longer"
    );
    // The model's own shape: time is linear in log2(D/W + 1).
    let slope = (mean(1_500.0, 20.0) - mean(150.0, 20.0))
      / ((1_500.0_f64 / 20.0 + 1.0).log2() - (150.0_f64 / 20.0 + 1.0).log2());
    assert!((100.0..190.0).contains(&slope), "ms per bit {slope}");
    let mut d = Seeded::new(12);
    for _ in 0..2_000 {
      assert!((90.0..=2_600.0).contains(&fitts_ms(5_000.0, 6.0, &mut d)));
    }
  }

  #[test]
  fn a_path_has_no_jump_and_ends_on_target() {
    for seed in 0..300 {
      let mut d = Seeded::new(seed);
      let from = Point::new(d.range(0.0, 1_900.0), d.range(0.0, 1_000.0));
      let to = Point::new(d.range(0.0, 1_900.0), d.range(0.0, 1_000.0));
      let dpr = if seed % 2 == 0 { 1.0 } else { 2.0 };
      let plan = plan_move(from, to, d.range(10.0, 200.0), dpr, &mut d);
      if from.distance(to) < 1.0 {
        continue;
      }
      let distance = from.distance(to);
      // The cap, plus the noise and the pixel grid.
      let limit = MAX_EVENT_STEP_PX + 6.0;
      for jump in jumps(snap_point(from, dpr), &plan) {
        assert!(jump <= limit, "jump {jump}px on a {distance}px move");
      }
      assert_eq!(plan.end(), Some(snap_point(to, dpr)));
      // Times only go forward.
      assert!(plan
        .steps
        .windows(2)
        .all(|pair| pair[1].at_ms > pair[0].at_ms));
      assert!(
        plan.steps.first().unwrap().at_ms >= 4.0,
        "no event at time zero"
      );
      // Never instant: a real distance takes real time.
      if distance > 30.0 {
        assert!(
          plan.duration_ms >= 90.0,
          "{} ms for {distance}px",
          plan.duration_ms
        );
        assert!(plan.steps.len() >= 5);
      }
    }
  }

  #[test]
  fn events_arrive_at_a_human_rate_and_never_at_a_fixed_interval() {
    let mut gaps = Vec::new();
    for seed in 0..200 {
      let mut d = Seeded::new(1_000 + seed);
      let plan = plan_move(
        Point::new(100.0, 100.0),
        Point::new(900.0, 500.0),
        40.0,
        1.0,
        &mut d,
      );
      let mut previous = 0.0;
      let mut own = Vec::new();
      for step in &plan.steps {
        own.push(step.at_ms - previous);
        previous = step.at_ms;
      }
      // Within one move the gaps are never all alike.
      let mut distinct: Vec<i64> = own.iter().map(|gap| (gap * 100.0).round() as i64).collect();
      distinct.sort_unstable();
      distinct.dedup();
      assert!(
        distinct.len() * 10 >= own.len() * 7,
        "{} of {}",
        distinct.len(),
        own.len()
      );
      gaps.extend(own);
    }
    // The pointer starts and ends a stroke slowly, so some events are skipped
    // there (it did not move a whole pixel) and their gap is a few intervals.
    // The rest are 4 to 21 ms apart: between roughly 48 and 250 events a second.
    let typical = gaps
      .iter()
      .filter(|gap| (4.0..=45.0).contains(*gap))
      .count();
    assert!(
      typical * 100 >= gaps.len() * 95,
      "{typical} of {}",
      gaps.len()
    );
    let mut sorted = gaps.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    assert!((8.0..=18.0).contains(&median), "median gap {median}");
    assert!(variation(&gaps) > 0.15, "variation {}", variation(&gaps));
  }

  #[test]
  fn coordinates_sit_on_the_pixel_grid_of_the_screen() {
    for (dpr, grid) in [(1.0, 1.0), (2.0, 0.5), (1.5, 1.0 / 1.5)] {
      let mut d = Seeded::new(21);
      let plan = plan_move(
        Point::new(5.0, 5.0),
        Point::new(700.3, 400.7),
        30.0,
        dpr,
        &mut d,
      );
      for step in &plan.steps {
        for value in [step.x, step.y] {
          let cells = value / grid;
          assert!(
            (cells - cells.round()).abs() < 1e-6,
            "{value} off the {grid} grid"
          );
        }
      }
    }
    assert_eq!(snap(10.4, 1.0), 10.0);
    assert_eq!(snap(10.3, 2.0), 10.5);
    assert_eq!(snap(10.3, f64::NAN), 10.0);
  }

  #[test]
  fn paths_curve_and_do_not_run_straight() {
    let mut deviations = Vec::new();
    for seed in 0..100 {
      let mut d = Seeded::new(2_000 + seed);
      let from = Point::new(100.0, 100.0);
      let to = Point::new(700.0, 400.0);
      let plan = plan_move(from, to, 30.0, 2.0, &mut d);
      let length = from.distance(to);
      let off_line = plan
        .steps
        .iter()
        .map(|step| {
          ((to.y - from.y) * step.x - (to.x - from.x) * step.y + to.x * from.y - to.y * from.x)
            .abs()
            / length
        })
        .fold(0.0, f64::max);
      deviations.push(off_line);
    }
    let curved = deviations.iter().filter(|dev| **dev > 4.0).count();
    assert!(
      curved * 10 >= deviations.len() * 8,
      "{curved} curved of {}",
      deviations.len()
    );
    assert!(deviations.iter().all(|dev| *dev < 200.0));
  }

  #[test]
  fn a_long_move_toward_a_small_target_sometimes_overshoots_and_corrects() {
    let mut overshoots = 0;
    for seed in 0..300 {
      let mut d = Seeded::new(3_000 + seed);
      let from = Point::new(100.0, 300.0);
      let to = Point::new(1_300.0, 300.0);
      let plan = plan_move(from, to, 24.0, 1.0, &mut d);
      // Past the target along the line of travel, then back to it.
      if plan.steps.iter().any(|step| step.x > to.x + 4.0) {
        overshoots += 1;
        let end = plan.end().unwrap();
        assert_eq!(end, to);
      }
    }
    assert!((60..=220).contains(&overshoots), "{overshoots} of 300");
    // A short move never overshoots.
    for seed in 0..100 {
      let mut d = Seeded::new(seed);
      let plan = plan_move(
        Point::new(100.0, 100.0),
        Point::new(180.0, 100.0),
        24.0,
        1.0,
        &mut d,
      );
      assert!(plan.steps.iter().all(|step| step.x <= 186.0));
    }
  }

  #[test]
  fn standing_still_is_no_move() {
    let mut d = Seeded::new(1);
    let here = Point::new(50.0, 50.0);
    assert!(plan_move(here, here, 30.0, 1.0, &mut d).steps.is_empty());
    assert!(plan_move(here, Point::new(50.2, 50.2), 30.0, 1.0, &mut d)
      .steps
      .is_empty());
  }

  #[test]
  fn a_click_lands_inside_the_box_off_centre() {
    let mut d = Seeded::new(31);
    let mut exact_center = 0;
    for i in 0..6_000 {
      let rect = Rect {
        x: d.range(0.0, 1_500.0),
        y: d.range(0.0, 800.0),
        w: d.range(14.0, 600.0),
        h: d.range(8.0, 120.0),
      };
      let dpr = if i % 2 == 0 { 1.0 } else { 2.0 };
      let point = plan_click_point(rect, dpr, &mut d);
      assert!(rect.contains(point), "{point:?} outside {rect:?}");
      if point == snap_point(rect.center(), dpr) {
        exact_center += 1;
      }
      // Off the box's edge too: a person does not press the very rim.
      let margin = (rect.w * 0.15).min(3.0) - 1.0 / dpr;
      assert!(point.x >= rect.x + margin && point.x <= rect.x + rect.w - margin);
    }
    assert_eq!(exact_center, 0);
  }

  #[test]
  fn click_points_scatter_instead_of_repeating() {
    let rect = Rect {
      x: 200.0,
      y: 300.0,
      w: 180.0,
      h: 36.0,
    };
    let mut d = Seeded::new(32);
    let points: Vec<Point> = (0..500)
      .map(|_| plan_click_point(rect, 2.0, &mut d))
      .collect();
    let xs: Vec<f64> = points.iter().map(|point| point.x).collect();
    let ys: Vec<f64> = points.iter().map(|point| point.y).collect();
    let spread = |values: &[f64]| {
      let mean = values.iter().sum::<f64>() / values.len() as f64;
      (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
    };
    assert!(spread(&xs) > 12.0, "x spread {}", spread(&xs));
    assert!(spread(&ys) > 2.0, "y spread {}", spread(&ys));
    let mean_x = xs.iter().sum::<f64>() / xs.len() as f64;
    assert!(
      mean_x < rect.center().x + 6.0,
      "slightly left of centre: {mean_x}"
    );
    // A tiny box still yields a point inside it.
    let tiny = Rect {
      x: 10.0,
      y: 10.0,
      w: 14.0,
      h: 8.0,
    };
    for _ in 0..500 {
      assert!(tiny.contains(plan_click_point(tiny, 1.0, &mut d)));
    }
  }

  #[test]
  fn a_click_holds_the_button_a_human_while() {
    let mut holds = Vec::new();
    let mut presses = Vec::new();
    let mut nudges = 0;
    let mut d = Seeded::new(41);
    for _ in 0..4_000 {
      let click = plan_click(2.0, &mut d);
      assert!((60..=180).contains(&click.hold_ms), "{click:?}");
      assert!((70..=900).contains(&click.press_after_ms));
      if let Some(nudge) = click.nudge {
        nudges += 1;
        assert!(nudge.at_ms < click.hold_ms);
        assert!(nudge.dx.abs() <= 0.5 && nudge.dy.abs() <= 0.5 && nudge.dy != 0.0);
      }
      holds.push(click.hold_ms as f64);
      presses.push(click.press_after_ms as f64);
    }
    assert!(variation(&holds) > 0.1, "holds vary");
    assert!(variation(&presses) > 0.25);
    assert!((600..1_500).contains(&nudges), "{nudges}");
  }

  #[test]
  fn drift_stays_in_the_page_and_the_entry_is_at_an_edge() {
    let mut d = Seeded::new(51);
    for _ in 0..2_000 {
      let from = Point::new(d.range(0.0, 1_280.0), d.range(0.0, 700.0));
      let target = drift_target(from, (1_280.0, 700.0), &mut d);
      assert!(target.x >= 6.0 && target.x <= 1_274.0 && target.y >= 6.0 && target.y <= 694.0);
      assert!(from.distance(target) <= 91.0 + 12.0);
      let entry = entry_point((1_280.0, 700.0), 2.0, &mut d);
      assert!(entry.x >= 0.0 && entry.x <= 1_280.0 && entry.y >= 0.0 && entry.y <= 700.0);
      let near_edge = entry.y <= 15.0 || entry.x <= 15.0 || entry.x >= 1_280.0 - 15.0;
      assert!(near_edge, "{entry:?}");
    }
  }
}
