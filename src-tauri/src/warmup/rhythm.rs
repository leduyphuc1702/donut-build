//! The clock of a person browsing: how long they stay on a page, how long they
//! read what is in front of them, how long they think before a click.
//!
//! None of these is a fixed number. Human waiting times are right-skewed: most
//! pauses are short, now and then one is long. Each is drawn from a log-normal
//! distribution around a median and clamped to what a person plausibly does,
//! so a trace of them never shows a repeated interval.

use rand::RngExt;

/// A source of uniform numbers in `[0, 1)`, with the draws built on it. The
/// pure planners take one, so a test hands them a fixed sequence and a run hands
/// them the thread's generator.
pub trait Draw {
  fn unit(&mut self) -> f64;

  fn range(&mut self, low: f64, high: f64) -> f64 {
    low + (high - low) * self.unit()
  }

  fn chance(&mut self, probability: f64) -> bool {
    self.unit() < probability
  }

  /// A standard normal draw (Box-Muller).
  fn normal(&mut self) -> f64 {
    let u1 = self.unit().max(1e-12);
    let u2 = self.unit();
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
  }

  fn gauss(&mut self, mean: f64, sd: f64) -> f64 {
    mean + sd * self.normal()
  }

  /// A log-normal draw whose median is `median`; `sigma` is the spread of the
  /// logarithm, so 0.3 is tight and 0.8 has a long tail.
  fn log_normal(&mut self, median: f64, sigma: f64) -> f64 {
    median * (sigma * self.normal()).exp()
  }

  /// An index below `len` (0 when empty).
  fn index(&mut self, len: usize) -> usize {
    ((self.unit() * len as f64) as usize).min(len.saturating_sub(1))
  }

  /// A coin that is +1.0 or -1.0.
  fn sign(&mut self) -> f64 {
    if self.unit() < 0.5 {
      -1.0
    } else {
      1.0
    }
  }
}

/// The thread's generator. The generator is fetched on every draw rather than
/// held, so a task that holds a `ThreadDraw` across an await stays `Send`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadDraw;

impl Draw for ThreadDraw {
  fn unit(&mut self) -> f64 {
    rand::rng().random::<f64>()
  }
}

/// A fixed sequence, so a plan is the same on every run of a test.
#[cfg(test)]
pub struct Seeded(u64);

#[cfg(test)]
impl Seeded {
  pub fn new(seed: u64) -> Self {
    Seeded(seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1))
  }
}

#[cfg(test)]
impl Draw for Seeded {
  fn unit(&mut self) -> f64 {
    self.0 = self
      .0
      .wrapping_mul(6_364_136_223_846_793_005)
      .wrapping_add(1_442_695_040_888_963_407);
    (self.0 >> 11) as f64 / (1u64 << 53) as f64
  }
}

/// A page is never left sooner than this, nor stayed on past it.
const MIN_PAGE_DWELL_MS: f64 = 5_000.0;
const MAX_PAGE_DWELL_MS: f64 = 8.0 * 60.0 * 1_000.0;
/// A link is followed only when the site's share has room for the page behind
/// it, at about this much time per page.
const SECONDS_PER_LINK: f64 = 25.0;
/// Most internal links followed on one site.
pub const MAX_LINKS: usize = 5;

/// How one site's time is spent: how many links are followed and how long the
/// person stays on each page.
#[derive(Clone, Debug, PartialEq)]
pub struct SitePlan {
  /// Links followed after the landing page; the visit has `links + 1` pages.
  pub links: usize,
  /// Time on each page, landing page first, in milliseconds.
  pub dwell_ms: Vec<u64>,
}

/// Plan a visit that has `share_secs` to spend. Few links fit a small share;
/// the dwell of each page is a log-normal weight scaled so the visit fills most
/// of the share, never all of it, which leaves the page loads their time.
pub fn plan_site(share_secs: f64, d: &mut impl Draw) -> SitePlan {
  let fits = ((share_secs / SECONDS_PER_LINK).floor() as usize).min(MAX_LINKS);
  let links = if fits == 0 {
    0
  } else {
    // Mostly a couple of links, now and then the most the share allows.
    1 + ((d.unit().powf(1.4) * fits as f64) as usize).min(fits - 1)
  };
  let weights: Vec<f64> = (0..=links).map(|_| d.log_normal(1.0, 0.6)).collect();
  let sum: f64 = weights.iter().sum();
  let fill = d.range(0.78, 0.95);
  let total_ms = share_secs.max(0.0) * 1_000.0 * fill;
  let dwell_ms = weights
    .iter()
    .map(|weight| (weight / sum * total_ms).clamp(MIN_PAGE_DWELL_MS, MAX_PAGE_DWELL_MS) as u64)
    .collect();
  SitePlan { links, dwell_ms }
}

/// The wait after a page loads before the person does anything: the page is
/// taken in first.
pub fn settle_after_load_ms(d: &mut impl Draw) -> u64 {
  d.log_normal(1_400.0, 0.4).clamp(650.0, 4_500.0) as u64
}

/// The pause to take in `visible_chars` characters on screen before the next
/// move. A visitor skims, a screenful in some seconds rather than word by word,
/// so the pause grows with the text but is bounded and a page is scrolled every
/// ten seconds or so, not every half minute; a screen of pictures gets a short
/// look.
pub fn reading_pause_ms(visible_chars: f64, d: &mut impl Draw) -> u64 {
  let chars_per_second = d.log_normal(120.0, 0.35).clamp(50.0, 260.0);
  let reading = visible_chars.clamp(0.0, 3_000.0) / chars_per_second * 1_000.0;
  let look = d.log_normal(1_300.0, 0.5);
  (reading * d.range(0.5, 1.0) + look).clamp(MIN_READING_PAUSE_MS, MAX_READING_PAUSE_MS) as u64
}

const MIN_READING_PAUSE_MS: f64 = 1_100.0;
const MAX_READING_PAUSE_MS: f64 = 16_000.0;

/// The time between deciding to click and pressing: reaching the target is
/// the movement, this is the half-second of making up one's mind.
pub fn think_ms(d: &mut impl Draw) -> u64 {
  d.log_normal(430.0, 0.5).clamp(170.0, 2_600.0) as u64
}

/// The gap between two small actions on one page.
pub fn action_gap_ms(d: &mut impl Draw) -> u64 {
  d.log_normal(1_700.0, 0.7).clamp(500.0, 14_000.0) as u64
}

/// The look at a banner before dealing with it.
pub fn banner_notice_ms(d: &mut impl Draw) -> u64 {
  d.log_normal(1_150.0, 0.45).clamp(500.0, 4_500.0) as u64
}

/// The coefficient of variation of `values`: spread over mean. A timer is near
/// zero; people are well above 0.2.
#[cfg(test)]
pub fn variation(values: &[f64]) -> f64 {
  let n = values.len() as f64;
  let mean = values.iter().sum::<f64>() / n;
  let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
  var.sqrt() / mean
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn draws_stay_in_their_ranges() {
    let mut d = Seeded::new(1);
    for _ in 0..10_000 {
      let unit = d.unit();
      assert!((0.0..1.0).contains(&unit));
      assert!(d.index(7) < 7);
      let value = d.range(3.0, 9.0);
      assert!((3.0..9.0).contains(&value));
    }
    assert_eq!(d.index(0), 0);
  }

  #[test]
  fn a_log_normal_draw_centres_on_its_median_and_is_right_skewed() {
    let mut d = Seeded::new(2);
    let mut values: Vec<f64> = (0..20_000).map(|_| d.log_normal(1_000.0, 0.6)).collect();
    values.sort_by(f64::total_cmp);
    let median = values[values.len() / 2];
    assert!((900.0..1_100.0).contains(&median), "{median}");
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    assert!(
      mean > median * 1.1,
      "heavy right tail: mean {mean} median {median}"
    );
    assert!(values[values.len() * 99 / 100] > median * 3.0);
  }

  #[test]
  fn a_small_share_follows_no_link_and_a_large_one_at_most_five() {
    for seed in 0..200 {
      let tiny = plan_site(20.0, &mut Seeded::new(seed));
      assert_eq!(tiny.links, 0);
      assert_eq!(tiny.dwell_ms.len(), 1);
      let big = plan_site(1_200.0, &mut Seeded::new(seed));
      assert!((1..=MAX_LINKS).contains(&big.links), "{big:?}");
      assert_eq!(big.dwell_ms.len(), big.links + 1);
    }
  }

  #[test]
  fn a_visit_fills_most_of_its_share_and_never_all_of_it() {
    for seed in 0..300 {
      for share in [60.0, 180.0, 420.0] {
        let plan = plan_site(share, &mut Seeded::new(seed));
        let total: u64 = plan.dwell_ms.iter().sum();
        assert!(
          total as f64 <= share * 1_000.0 * 0.96 + 5.0 * 5_000.0,
          "{plan:?}"
        );
        assert!(total as f64 >= share * 1_000.0 * 0.5, "{plan:?}");
        assert!(plan.dwell_ms.iter().all(|ms| *ms >= 5_000));
      }
    }
  }

  #[test]
  fn page_dwell_times_are_uneven_and_never_repeat() {
    let mut all = Vec::new();
    for seed in 0..400 {
      all.extend(
        plan_site(400.0, &mut Seeded::new(seed))
          .dwell_ms
          .into_iter()
          .map(|ms| ms as f64),
      );
    }
    assert!(variation(&all) > 0.3, "{}", variation(&all));
    let mut distinct = all.clone();
    distinct.sort_by(f64::total_cmp);
    distinct.dedup();
    assert!(distinct.len() * 10 > all.len() * 9);
  }

  #[test]
  fn reading_takes_longer_for_more_text_and_is_bounded() {
    let mean = |chars: f64| {
      let mut d = Seeded::new(5);
      (0..2_000)
        .map(|_| reading_pause_ms(chars, &mut d) as f64)
        .sum::<f64>()
        / 2_000.0
    };
    assert!(mean(2_500.0) > mean(300.0) * 1.8);
    let mut d = Seeded::new(6);
    for _ in 0..5_000 {
      let pause = reading_pause_ms(1.0e9, &mut d);
      assert!((1_100..=16_000).contains(&pause));
      assert!(reading_pause_ms(0.0, &mut d) >= 1_100);
    }
  }

  #[test]
  fn waits_before_acting_are_irregular() {
    let mut d = Seeded::new(7);
    let thinks: Vec<f64> = (0..2_000).map(|_| think_ms(&mut d) as f64).collect();
    assert!(thinks.iter().all(|ms| (170.0..=2_600.0).contains(ms)));
    assert!(variation(&thinks) > 0.25);
    let gaps: Vec<f64> = (0..2_000).map(|_| action_gap_ms(&mut d) as f64).collect();
    assert!(variation(&gaps) > 0.4);
    let settles: Vec<f64> = (0..2_000)
      .map(|_| settle_after_load_ms(&mut d) as f64)
      .collect();
    assert!(variation(&settles) > 0.2);
    let notices: Vec<f64> = (0..500).map(|_| banner_notice_ms(&mut d) as f64).collect();
    assert!(notices.iter().all(|ms| (500.0..=4_500.0).contains(ms)));
  }
}
