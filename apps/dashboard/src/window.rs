//! The span a range reads series over, its buckets, and the span before it.

use crate::page::{Measure, Ref};
use crate::series::{Series, Unit};
use crate::Range;

const HOUR: i64 = 3600;
const DAY: i64 = 86_400;

/// Buckets aligned to UTC, the last holding `now`; and the same span just
/// before, up to as far into its last bucket as now is into this one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The first bucket's start.
    pub from: i64,
    /// The end: now, inclusive.
    pub to: i64,
    pub bucket: i64,
    pub buckets: usize,
}

impl Window {
    pub fn new(range: Range, now: i64, since: i64) -> Self {
        let (span, bucket) = match range {
            Range::Day => (DAY, HOUR),
            Range::Week => (7 * DAY, 6 * HOUR),
            Range::Month => (30 * DAY, DAY),
            Range::Quarter => (90 * DAY, DAY),
            Range::Year => (365 * DAY, 7 * DAY),
            Range::All => {
                let span = (now - since).max(DAY);
                let bucket = [HOUR, 6 * HOUR, DAY, 7 * DAY, 30 * DAY]
                    .into_iter()
                    .find(|b| span / b < 90)
                    .unwrap_or(30 * DAY);
                (span, bucket)
            }
        };
        let buckets = (span + bucket - 1) / bucket;
        let last = now.div_euclid(bucket) * bucket;
        Self {
            from: last - (buckets - 1) * bucket,
            to: now,
            bucket,
            buckets: buckets as usize,
        }
    }

    /// The window as long before this one.
    pub fn previous(&self) -> Self {
        let shift = self.buckets as i64 * self.bucket;
        Self {
            from: self.from - shift,
            to: self.to - shift,
            ..*self
        }
    }

    /// Its length in days, at least an hour's worth.
    pub fn days(&self) -> f64 {
        ((self.to - self.from + 1) as f64 / DAY as f64).max(1.0 / 24.0)
    }

    pub fn contains(&self, at: i64) -> bool {
        self.from <= at && at <= self.to
    }

    /// Each bucket's start.
    pub fn starts(&self) -> impl Iterator<Item = i64> + '_ {
        (0..self.buckets as i64).map(|i| self.from + i * self.bucket)
    }

    /// The bucket `at` falls in, if any.
    pub fn index(&self, at: i64) -> Option<usize> {
        self.contains(at)
            .then(|| ((at - self.from) / self.bucket) as usize)
    }
}

/// The series a report names by id, if it has it.
pub fn find<'a>(series: &'a [Series], id: &str) -> Option<&'a Series> {
    series.iter().find(|s| s.id == id)
}

/// What a value reads in: counts count, sums and levels take their
/// series' unit.
pub fn unit(series: &[Series], r: &Ref) -> Unit {
    match (r.measure, find(series, &r.series)) {
        (Measure::Count, _) | (_, None) => Unit::Count,
        (_, Some(s)) => s.unit,
    }
}

/// `r` over `window`, of `label` if given (else of `r`'s, else of all).
pub fn measure(series: &[Series], r: &Ref, label: Option<&str>, window: &Window) -> i64 {
    let Some(s) = find(series, &r.series) else {
        return 0;
    };
    let label = match label.or(r.label.as_deref()) {
        Some(name) => match s.labels.iter().position(|l| l == name) {
            Some(i) => Some(i as u32),
            None => return 0,
        },
        None => None,
    };
    let facts = s
        .facts
        .iter()
        .filter(|f| label.is_none_or(|l| f.label == l));
    match r.measure {
        Measure::Count => facts.filter(|f| window.contains(f.at)).count() as i64,
        Measure::Sum => facts
            .filter(|f| window.contains(f.at))
            .map(|f| f.value)
            .sum(),
        Measure::Level => facts.filter(|f| f.at <= window.to).map(|f| f.value).sum(),
    }
}

/// `r` in each bucket of `window`, per label of its series: counts and sums
/// within the bucket, a level at its end. Empty if the series is missing.
pub fn buckets(series: &[Series], r: &Ref, window: &Window) -> Vec<Vec<i64>> {
    let Some(s) = find(series, &r.series) else {
        return Vec::new();
    };
    let mut out = vec![vec![0; window.buckets]; s.labels.len()];
    let level = r.measure == Measure::Level;
    for f in &s.facts {
        let value = if r.measure == Measure::Count {
            1
        } else {
            f.value
        };
        let row = &mut out[f.label as usize];
        if level && f.at < window.from {
            row[0] += value;
        } else if let Some(i) = window.index(f.at) {
            row[i] += value;
        }
    }
    if level {
        for row in &mut out {
            for i in 1..row.len() {
                row[i] += row[i - 1];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIDNIGHT: i64 = 1_794_700_800; // 2026-11-15
    const NOW: i64 = MIDNIGHT + 13 * HOUR + 20 * 60;

    #[test]
    fn a_month_is_thirty_utc_days_up_to_now() {
        let w = Window::new(Range::Month, NOW, 0);
        assert_eq!((w.buckets, w.bucket), (30, DAY));
        assert_eq!(w.from, MIDNIGHT - 29 * DAY);
        assert_eq!(w.to, NOW);
        let p = w.previous();
        assert_eq!(p.to, NOW - 30 * DAY);
        assert_eq!(p.from, w.from - 30 * DAY);
        assert_eq!(w.index(NOW), Some(29));
        assert_eq!(w.index(w.from - 1), None);
    }

    #[test]
    fn all_time_picks_buckets_to_fit_the_history() {
        let w = Window::new(Range::All, NOW, NOW - 3 * DAY);
        assert_eq!((w.bucket, w.buckets), (HOUR, 72));
        let w = Window::new(Range::All, NOW, NOW - 10 * DAY);
        assert_eq!((w.bucket, w.buckets), (6 * HOUR, 40));
        let w = Window::new(Range::All, NOW, NOW - 400 * DAY);
        assert_eq!(w.bucket, 7 * DAY);
        let w = Window::new(Range::All, NOW, NOW);
        assert_eq!((w.bucket, w.buckets), (HOUR, 24));
    }

    fn sample() -> Vec<Series> {
        let mut pay = Series::flow("pay", "Payments", Unit::Sat);
        pay.add(NOW - 40 * DAY, "a", 100);
        pay.add(NOW - 2 * DAY, "a", 10);
        pay.add(NOW - HOUR, "b", 5);
        pay.add(NOW + DAY, "b", 1_000);
        let mut bal = Series::level("bal", "Balance", Unit::Sat);
        bal.add(NOW - 40 * DAY, "x", 50);
        bal.add(NOW - 2 * DAY, "x", -20);
        vec![pay, bal]
    }

    #[test]
    fn flows_count_and_sum_within_the_window_levels_read_at_its_end() {
        let s = sample();
        let w = Window::new(Range::Month, NOW, 0);
        assert_eq!(measure(&s, &Ref::count("pay"), None, &w), 2);
        assert_eq!(measure(&s, &Ref::sum("pay"), None, &w), 15);
        assert_eq!(measure(&s, &Ref::sum("pay").of("b"), None, &w), 5);
        assert_eq!(measure(&s, &Ref::sum("pay"), Some("missing"), &w), 0);
        assert_eq!(measure(&s, &Ref::level("bal"), None, &w), 30);
        assert_eq!(measure(&s, &Ref::level("bal"), None, &w.previous()), 50);
        assert_eq!(measure(&s, &Ref::sum("bal"), None, &w), -20);
        assert_eq!(measure(&s, &Ref::sum("nothing"), None, &w), 0);
    }

    #[test]
    fn buckets_stack_flows_and_carry_levels() {
        let s = sample();
        let w = Window::new(Range::Month, NOW, 0);
        let pay = buckets(&s, &Ref::sum("pay"), &w);
        assert_eq!(pay.len(), 2);
        assert_eq!(pay[0][27], 10);
        assert_eq!(pay[1][29], 5);
        assert_eq!(pay[0].iter().sum::<i64>(), 10);
        let bal = buckets(&s, &Ref::level("bal"), &w);
        assert_eq!(bal[0][0], 50);
        assert_eq!(bal[0][26], 50);
        assert_eq!(bal[0][27], 30);
        assert_eq!(bal[0][29], 30);
    }
}
