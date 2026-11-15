//! What a service shows, page by page. Values that depend on the range
//! name series by [`Ref`]; the dashboard works them out for the range.

use serde::Serialize;

use crate::series::Unit;

/// How a [`Ref`] reads a series over a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Measure {
    /// The number of facts within the range.
    Count,
    /// The sum of the values of the facts within the range: for a level,
    /// how much it changed.
    Sum,
    /// A level's value at the range's end.
    Level,
}

/// A series, read by `measure`, of one `label` or of all of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Ref {
    pub series: String,
    pub measure: Measure,
    pub label: Option<String>,
}

impl Ref {
    pub fn count(series: &str) -> Self {
        Self::new(series, Measure::Count)
    }

    pub fn sum(series: &str) -> Self {
        Self::new(series, Measure::Sum)
    }

    pub fn level(series: &str) -> Self {
        Self::new(series, Measure::Level)
    }

    /// Only the facts under `label`.
    pub fn of(mut self, label: &str) -> Self {
        self.label = Some(label.into());
        self
    }

    fn new(series: &str, measure: Measure) -> Self {
        Self {
            series: series.into(),
            measure,
            label: None,
        }
    }
}

/// How a row, a check or a value reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Normal,
    Good,
    Warn,
    Bad,
    Muted,
}

/// Which way a value is better moving, which colors its change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Better {
    Up,
    Down,
    #[default]
    Neither,
}

/// A headline number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Card {
    pub title: String,
    pub value: Value,
    pub better: Better,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Value {
    /// The sum of `plus` less that of `minus` over the range, with its
    /// change on the range before and its trend through the range. The
    /// unit is that of the first series.
    Total { plus: Vec<Ref>, minus: Vec<Ref> },
    /// The sum of `num` over that of `den`, as a percentage, with its
    /// change on the range before.
    Ratio { num: Vec<Ref>, den: Vec<Ref> },
    /// A value as it is now, which no series holds the history of: `None`
    /// if the service could not tell, `detail` says why or what it means.
    Now {
        value: Option<i64>,
        unit: Unit,
        detail: String,
        tone: Tone,
    },
}

/// A column of a [`Panel::Breakdown`], for each label of its series.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    /// The label's value of `of` over the range.
    Value { title: String, of: Ref },
    /// That value's share of the column's total.
    Share { title: String, of: Ref },
    /// That value's change on the range before.
    Change { title: String, of: Ref },
    /// That value per day of the range.
    PerDay { title: String, of: Ref },
    /// The days until the level `of` runs out, if it fell over the range
    /// at the rate it did.
    Runway { title: String, of: String },
}

/// A table cell; the dashboard formats each type alike everywhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cell {
    Text(String),
    Sat(i64),
    Int(i64),
    /// Unix seconds, read as an age or a date.
    Time(i64),
    /// Blocks from now, read with the time they take.
    Blocks(i64),
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Row {
    /// When it happened: with [`Table::in_range`], only rows within the
    /// range show.
    pub at: Option<i64>,
    pub tone: Tone,
    pub cells: Vec<Cell>,
}

/// A list, newest or most urgent first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Table {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    pub in_range: bool,
    /// What shows when no row does.
    pub empty: String,
}

/// One check of the service's health.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub tone: Tone,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Panel {
    /// Headline numbers side by side.
    Cards(Vec<Card>),
    /// A series over the range: bars per bucket, stacked by label, by
    /// `measure`; lines per label for a level.
    Chart {
        title: String,
        series: String,
        measure: Measure,
    },
    /// A row per label of `series`, ranked by the first column.
    Breakdown {
        title: String,
        series: String,
        columns: Vec<Column>,
    },
    Table(Table),
    Health(Vec<Check>),
}

/// A page: rows of panels side by side. A row of cards keeps their height,
/// the others share the rest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Page {
    pub title: String,
    pub rows: Vec<Vec<Panel>>,
}

impl Card {
    pub fn new(title: &str, value: Value, better: Better) -> Self {
        Self {
            title: title.into(),
            value,
            better,
        }
    }
}

impl Value {
    /// The value of `of` over the range.
    pub fn of(of: Ref) -> Self {
        Self::Total {
            plus: vec![of],
            minus: vec![],
        }
    }

    pub fn now(value: Option<i64>, unit: Unit, detail: impl Into<String>, tone: Tone) -> Self {
        Self::Now {
            value,
            unit,
            detail: detail.into(),
            tone,
        }
    }
}

impl Column {
    pub fn value(title: &str, of: Ref) -> Self {
        Self::Value {
            title: title.into(),
            of,
        }
    }

    pub fn share(title: &str, of: Ref) -> Self {
        Self::Share {
            title: title.into(),
            of,
        }
    }

    pub fn change(title: &str, of: Ref) -> Self {
        Self::Change {
            title: title.into(),
            of,
        }
    }

    pub fn per_day(title: &str, of: Ref) -> Self {
        Self::PerDay {
            title: title.into(),
            of,
        }
    }
}

impl Check {
    pub fn new(name: &str, tone: Tone, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tone,
            detail: detail.into(),
        }
    }
}

impl Row {
    pub fn new(at: Option<i64>, tone: Tone, cells: Vec<Cell>) -> Self {
        Self { at, tone, cells }
    }
}

impl Table {
    /// A table of what is true now: every row shows, whatever the range.
    pub fn now(title: &str, columns: &[&str], rows: Vec<Row>, empty: &str) -> Self {
        Self {
            title: title.into(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows,
            in_range: false,
            empty: empty.into(),
        }
    }

    /// A table of what happened: only the rows within the range show.
    pub fn in_range(title: &str, columns: &[&str], rows: Vec<Row>, empty: &str) -> Self {
        Self {
            in_range: true,
            ..Self::now(title, columns, rows, empty)
        }
    }
}

impl Panel {
    pub fn chart(title: &str, series: &str, measure: Measure) -> Self {
        Self::Chart {
            title: title.into(),
            series: series.into(),
            measure,
        }
    }

    pub fn breakdown(title: &str, series: &str, columns: Vec<Column>) -> Self {
        Self::Breakdown {
            title: title.into(),
            series: series.into(),
            columns,
        }
    }
}

impl Page {
    pub fn new(title: &str, rows: Vec<Vec<Panel>>) -> Self {
        Self {
            title: title.into(),
            rows,
        }
    }
}
