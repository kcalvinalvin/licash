//! An operator's dashboard for Licash services, in the terminal.
//!
//! A service reads its own records into a [`Report`]: [`Series`] of what
//! happened (payments, fees, failures) and of levels that moved (balances,
//! notes outstanding), as facts with their times, and [`Page`]s of panels
//! that name those series or hold what is true now (open contracts,
//! payments under way, health checks). This crate does the rest: it reads
//! the series over the range the operator picks (30 days at first), against
//! the range before, and draws them; or prints the same as JSON.
//!
//! A service calls [`Args::run`] from its `dashboard` command with a loader
//! the dashboard calls again on every refresh, off the terminal's thread.

mod app;
mod chart;
mod format;
mod page;
mod probe;
mod render;
mod series;
mod style;
mod view;
mod window;

#[cfg(test)]
mod tests;

use std::fmt;
use std::io::IsTerminal;
use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;

pub use format::{grouped, span};
pub use page::{
    Better, Card, Cell, Check, Column, Measure, Page, Panel, Ref, Row, Table, Tone, Value,
};
pub use probe::{unasked, Probe};
pub use series::{Fact, Kind, Series, Unit};

/// Everything a service shows its operator, as read at `now`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    /// What the service is, such as `licash hub`.
    pub title: String,
    /// Which one: its network, data directory or URL.
    pub subtitle: String,
    /// Unix seconds.
    pub now: i64,
    /// When the service began (Unix seconds), where the range `all` starts.
    pub since: i64,
    pub series: Vec<Series>,
    pub pages: Vec<Page>,
}

/// The span of time the dashboard reads series over, up to now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Range {
    Day,
    Week,
    #[default]
    Month,
    Quarter,
    Year,
    All,
}

impl Range {
    pub const ALL: [Range; 6] = [
        Range::Day,
        Range::Week,
        Range::Month,
        Range::Quarter,
        Range::Year,
        Range::All,
    ];

    /// Its length, `None` for `all`.
    pub fn seconds(self) -> Option<i64> {
        const DAY: i64 = 86_400;
        match self {
            Range::Day => Some(DAY),
            Range::Week => Some(7 * DAY),
            Range::Month => Some(30 * DAY),
            Range::Quarter => Some(90 * DAY),
            Range::Year => Some(365 * DAY),
            Range::All => None,
        }
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Month => "30d",
            Range::Quarter => "90d",
            Range::Year => "1y",
            Range::All => "all",
        })
    }
}

impl FromStr for Range {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Range::ALL
            .into_iter()
            .find(|r| r.to_string() == s)
            .ok_or_else(|| format!("unknown range {s:?}: one of 24h, 7d, 30d, 90d, 1y, all"))
    }
}

/// The dashboard's flags, for a service's `dashboard` command to flatten.
#[derive(Clone, Debug, clap::Args)]
pub struct Args {
    /// The range shown first: 24h, 7d, 30d, 90d, 1y or all.
    #[arg(long, default_value_t = Range::Month)]
    pub range: Range,
    /// Seconds between reads of the service's records.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    pub refresh: u64,
    /// Print the range's view as JSON, once, instead of drawing it.
    #[arg(long)]
    pub json: bool,
}

impl Args {
    /// Draws the reports `load` reads until the operator quits, or prints
    /// one as JSON.
    pub fn run<F>(self, mut load: F) -> anyhow::Result<()>
    where
        F: FnMut() -> anyhow::Result<Report> + Send + 'static,
    {
        let report = load()?;
        if self.json {
            let view = view::View::new(&report, self.range);
            println!("{}", serde_json::to_string_pretty(&view)?);
            return Ok(());
        }
        if !std::io::stdout().is_terminal() {
            anyhow::bail!("the dashboard draws on a terminal: run it in one, or with --json");
        }
        app::run(report, load, self.range, Duration::from_secs(self.refresh))
    }
}
