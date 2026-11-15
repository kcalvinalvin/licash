//! A report read over a range: every number a page shows, worked out.
//! The terminal draws it; `--json` prints it.

use serde::Serialize;

use crate::page::{Better, Card, Cell, Check, Column, Measure, Panel, Ref, Row, Tone, Value};
use crate::series::{Kind, Unit};
use crate::window::{self, Window};
use crate::{Range, Report};

#[derive(Clone, Debug, Serialize)]
pub struct View {
    pub title: String,
    pub subtitle: String,
    pub now: i64,
    pub range: String,
    pub from: i64,
    pub bucket: i64,
    /// The worst of every health check.
    pub health: Tone,
    pub pages: Vec<PageView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PageView {
    pub title: String,
    pub rows: Vec<Vec<PanelView>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum PanelView {
    Cards { cards: Vec<CardView> },
    Chart(ChartView),
    Breakdown(BreakdownView),
    Table(TableView),
    Health { checks: Vec<Check> },
}

/// What a value reads in: a unit of a series, or a percentage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reads {
    Unit(Unit),
    Percent,
}

#[derive(Clone, Debug, Serialize)]
pub struct CardView {
    pub title: String,
    pub reads: Reads,
    pub value: Option<f64>,
    /// Over the range before, for totals and ratios.
    pub previous: Option<f64>,
    pub better: Better,
    pub tone: Tone,
    pub detail: String,
    /// Per bucket: totals' values within it or at its end.
    pub trend: Vec<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChartView {
    pub title: String,
    pub unit: Unit,
    pub kind: Kind,
    /// Each bucket's start.
    pub starts: Vec<i64>,
    /// Per label, with its index for its color, its values per bucket.
    pub lines: Vec<Line>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Line {
    pub label: String,
    pub color: usize,
    pub values: Vec<i64>,
}

/// A breakdown's cell: a value in its unit, a share, a change or days.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum Number {
    Value(i64, Unit),
    PerDay(f64, Unit),
    Share(Option<f64>),
    Change(Option<f64>, f64),
    Days(Option<f64>),
}

#[derive(Clone, Debug, Serialize)]
pub struct BreakdownView {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<BreakdownRow>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BreakdownRow {
    pub label: String,
    pub color: usize,
    pub cells: Vec<Number>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TableView {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    pub empty: String,
    /// Rows the range left out.
    pub hidden: usize,
}

impl View {
    pub fn new(report: &Report, range: Range) -> Self {
        let w = Window::new(range, report.now, report.since);
        let r = Reader {
            report,
            w,
            p: w.previous(),
        };
        let mut health = Tone::Good;
        let pages = report
            .pages
            .iter()
            .map(|page| PageView {
                title: page.title.clone(),
                rows: page
                    .rows
                    .iter()
                    .map(|row| row.iter().map(|p| r.panel(p, &mut health)).collect())
                    .collect(),
            })
            .collect();
        Self {
            title: report.title.clone(),
            subtitle: report.subtitle.clone(),
            now: report.now,
            range: range.to_string(),
            from: w.from,
            bucket: w.bucket,
            health,
            pages,
        }
    }
}

/// Bad over Warn over Good; Normal and Muted say nothing of health.
fn worse(a: Tone, b: Tone) -> Tone {
    let rank = |t| match t {
        Tone::Bad => 2,
        Tone::Warn => 1,
        _ => 0,
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

struct Reader<'a> {
    report: &'a Report,
    w: Window,
    p: Window,
}

impl Reader<'_> {
    fn sum(&self, refs: &[Ref], label: Option<&str>, w: &Window) -> i64 {
        refs.iter()
            .map(|r| window::measure(&self.report.series, r, label, w))
            .sum()
    }

    fn panel(&self, panel: &Panel, health: &mut Tone) -> PanelView {
        match panel {
            Panel::Cards(cards) => PanelView::Cards {
                cards: cards.iter().map(|c| self.card(c)).collect(),
            },
            Panel::Chart {
                title,
                series,
                measure,
            } => PanelView::Chart(self.chart(title, series, *measure)),
            Panel::Breakdown {
                title,
                series,
                columns,
            } => PanelView::Breakdown(self.breakdown(title, series, columns)),
            Panel::Table(t) => {
                let rows: Vec<Row> = t
                    .rows
                    .iter()
                    .filter(|row| !t.in_range || row.at.is_none_or(|at| self.w.contains(at)))
                    .cloned()
                    .collect();
                PanelView::Table(TableView {
                    title: t.title.clone(),
                    columns: t.columns.clone(),
                    hidden: t.rows.len() - rows.len(),
                    rows,
                    empty: t.empty.clone(),
                })
            }
            Panel::Health(checks) => {
                for c in checks {
                    *health = worse(*health, c.tone);
                }
                PanelView::Health {
                    checks: checks.clone(),
                }
            }
        }
    }

    fn card(&self, card: &Card) -> CardView {
        let series = &self.report.series;
        let mut view = CardView {
            title: card.title.clone(),
            reads: Reads::Unit(Unit::Count),
            value: None,
            previous: None,
            better: card.better,
            tone: Tone::Normal,
            detail: String::new(),
            trend: Vec::new(),
        };
        match &card.value {
            Value::Total { plus, minus } => {
                let total = |w: &Window| self.sum(plus, None, w) - self.sum(minus, None, w);
                view.reads = Reads::Unit(
                    plus.first()
                        .map_or(Unit::Count, |r| window::unit(series, r)),
                );
                view.value = Some(total(&self.w) as f64);
                view.previous = Some(total(&self.p) as f64);
                view.trend = vec![0; self.w.buckets];
                for (refs, sign) in [(plus, 1), (minus, -1)] {
                    for r in refs {
                        for line in window::buckets(series, r, &self.w) {
                            for (t, v) in view.trend.iter_mut().zip(line) {
                                *t += sign * v;
                            }
                        }
                    }
                }
            }
            Value::Ratio { num, den } => {
                let ratio = |w: &Window| {
                    let d = self.sum(den, None, w);
                    (d != 0).then(|| self.sum(num, None, w) as f64 / d as f64)
                };
                view.reads = Reads::Percent;
                view.value = ratio(&self.w);
                view.previous = ratio(&self.p);
            }
            Value::Now {
                value,
                unit,
                detail,
                tone,
            } => {
                view.reads = Reads::Unit(*unit);
                view.value = value.map(|v| v as f64);
                view.detail = detail.clone();
                view.tone = *tone;
            }
        }
        view
    }

    fn chart(&self, title: &str, series: &str, measure: Measure) -> ChartView {
        let s = window::find(&self.report.series, series);
        let r = Ref {
            series: series.into(),
            measure,
            label: None,
        };
        let lines = window::buckets(&self.report.series, &r, &self.w)
            .into_iter()
            .enumerate()
            .map(|(i, values)| Line {
                label: s.map_or_else(String::new, |s| s.labels[i].clone()),
                color: i,
                values,
            })
            .collect();
        ChartView {
            title: title.into(),
            unit: window::unit(&self.report.series, &r),
            kind: s.map_or(Kind::Flow, |s| s.kind),
            starts: self.w.starts().collect(),
            lines,
        }
    }

    fn breakdown(&self, title: &str, series: &str, columns: &[Column]) -> BreakdownView {
        let all = &self.report.series;
        let labels = window::find(all, series).map_or(&[][..], |s| &s.labels[..]);
        let level = window::find(all, series).is_some_and(|s| s.kind == Kind::Level);
        let mut rows: Vec<BreakdownRow> = labels
            .iter()
            .enumerate()
            .map(|(color, label)| BreakdownRow {
                label: label.clone(),
                color,
                cells: columns.iter().map(|c| self.cell(c, label)).collect(),
            })
            .collect();
        let key = |row: &BreakdownRow| match row.cells.first() {
            Some(Number::Value(v, _)) => *v as f64,
            Some(Number::PerDay(v, _)) => *v,
            _ => 0.0,
        };
        // Flows without anything in the range say nothing; levels at zero do.
        if !level {
            rows.retain(|row| key(row) != 0.0);
        }
        rows.sort_by(|a, b| key(b).total_cmp(&key(a)));
        BreakdownView {
            title: title.into(),
            columns: columns.iter().map(|c| column_title(c).into()).collect(),
            rows,
        }
    }

    fn cell(&self, column: &Column, label: &str) -> Number {
        let all = &self.report.series;
        let at = |r: &Ref, w: &Window| window::measure(all, r, Some(label), w);
        match column {
            Column::Value { of, .. } => Number::Value(at(of, &self.w), window::unit(all, of)),
            Column::PerDay { of, .. } => Number::PerDay(
                at(of, &self.w) as f64 / self.w.days(),
                window::unit(all, of),
            ),
            Column::Share { of, .. } => {
                let whole = window::measure(
                    all,
                    &Ref {
                        label: None,
                        ..of.clone()
                    },
                    None,
                    &self.w,
                );
                Number::Share((whole != 0).then(|| at(of, &self.w) as f64 / whole as f64))
            }
            Column::Change { of, .. } => {
                let (now, before) = (at(of, &self.w) as f64, at(of, &self.p) as f64);
                Number::Change(change(now, before), now)
            }
            Column::Runway { of, .. } => {
                let level = at(&Ref::level(of), &self.w) as f64;
                let per_day = at(&Ref::sum(of), &self.w) as f64 / self.w.days();
                Number::Days(if level <= 0.0 {
                    Some(0.0)
                } else if per_day < 0.0 {
                    Some(level / -per_day)
                } else {
                    None
                })
            }
        }
    }
}

/// The change from `before` to `now`, relative; none from zero.
pub fn change(now: f64, before: f64) -> Option<f64> {
    (before != 0.0).then(|| (now - before) / before.abs())
}

fn column_title(c: &Column) -> &str {
    match c {
        Column::Value { title, .. }
        | Column::Share { title, .. }
        | Column::Change { title, .. }
        | Column::PerDay { title, .. }
        | Column::Runway { title, .. } => title,
    }
}

/// A cell's text, as every table formats it.
pub fn cell_text(cell: &Cell, now: i64) -> String {
    use crate::format;
    match cell {
        Cell::Text(s) => s.clone(),
        Cell::Sat(v) => format::amount(*v, Unit::Sat),
        Cell::Int(v) => format::grouped(*v),
        Cell::Time(at) => format::when(*at, now),
        Cell::Blocks(b) => format::blocks(*b),
        Cell::Empty => String::new(),
    }
}
