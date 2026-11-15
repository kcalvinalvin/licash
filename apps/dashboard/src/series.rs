//! What happened over time: the facts a service reads from its records.

use serde::Serialize;

/// The quantity a series' facts count in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Sat,
    Count,
    Blocks,
}

/// How a series' facts add up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Each fact is something that happened, such as a payment of `value`:
    /// a range counts or sums those within it.
    Flow,
    /// Each fact changes a level, such as a balance, by `value`: the level
    /// at a time is the sum of every fact up to it, since the beginning.
    Level,
}

/// One fact: at `at` (Unix seconds), `value` of the series' `label`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Fact {
    pub at: i64,
    /// Index into [`Series::labels`].
    pub label: u32,
    pub value: i64,
}

/// Facts of one kind, each under a label (a kind of payment, a reason, a
/// hub), in any order. Labels keep the order they first appeared in, which
/// the dashboard colors them by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Series {
    pub id: String,
    pub title: String,
    pub unit: Unit,
    pub kind: Kind,
    pub labels: Vec<String>,
    pub facts: Vec<Fact>,
}

impl Series {
    pub fn flow(id: &str, title: &str, unit: Unit) -> Self {
        Self::new(id, title, unit, Kind::Flow)
    }

    pub fn level(id: &str, title: &str, unit: Unit) -> Self {
        Self::new(id, title, unit, Kind::Level)
    }

    fn new(id: &str, title: &str, unit: Unit, kind: Kind) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            unit,
            kind,
            labels: Vec::new(),
            facts: Vec::new(),
        }
    }

    /// Adds `value` under `label` at `at`.
    pub fn add(&mut self, at: i64, label: &str, value: i64) {
        let label = self.label(label);
        self.facts.push(Fact { at, label, value });
    }

    /// The index of `label`, added if new: a label with no facts yet still
    /// shows, at zero.
    pub fn label(&mut self, label: &str) -> u32 {
        match self.labels.iter().position(|l| l == label) {
            Some(i) => i as u32,
            None => {
                self.labels.push(label.into());
                (self.labels.len() - 1) as u32
            }
        }
    }
}
