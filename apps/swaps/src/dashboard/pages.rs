//! The provider's records as the pages its operator reads.

use std::collections::HashMap;
use std::path::Path;

use licash_dashboard::{
    grouped, span, Better, Card, Cell, Check, Column, Measure, Page, Panel, Ref, Report, Row,
    Table, Tone, Unit, Value,
};
use licash_swaps::Notice;

use super::{flows, levels, short, Hub, Records, Swap, NEVER_CLAIMED};

const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

/// Blocks to a paid swap's expiry at its source, by which the provider
/// claims the client's lock or loses the swap, that call for a look.
const SOON: i64 = 6;

/// A hub last heard of longer ago than this: the provider, asking it
/// every round while it runs, does not reach it.
const STALE: i64 = 15 * 60;

/// What is true of the provider now, beside its series.
struct Now<'a> {
    stock: Vec<Stock<'a>>,
    /// None if the wallet does not follow Bitcoin.
    coins: Option<(i64, usize)>,
    under_way: Vec<&'a Swap>,
}

/// A hub the owner added, and what the provider holds there.
struct Stock<'a> {
    hub: &'a Hub,
    held: i64,
    /// What the swaps taken and not locked yet will lock there.
    reserved: i64,
    notes: usize,
    unlinked: usize,
}

impl Stock<'_> {
    /// The most the provider can lock there now: what it holds, less what
    /// is reserved and the hub's fee, as it quotes it.
    fn free(&self) -> i64 {
        let fee = self.hub.info.as_ref().map_or(0, |i| {
            i64::try_from(i.fees.locking().at_most(self.held.max(0) as u64)).unwrap_or(i64::MAX)
        });
        (self.held - self.reserved).saturating_sub(fee).max(0)
    }

    /// The provider swaps to it: it follows Bitcoin, so takes locks.
    fn swaps(&self) -> bool {
        self.hub.height().is_some()
    }
}

impl<'a> Now<'a> {
    fn of(r: &'a Records) -> Self {
        let stock = r
            .hubs
            .iter()
            .filter(|h| h.trusted)
            .map(|hub| {
                let held: Vec<_> = r
                    .notes
                    .iter()
                    .filter(|n| n.hub == hub.id && n.left_at.is_none())
                    .collect();
                let reserved = r
                    .swaps
                    .iter()
                    .filter(|s| s.to == hub.url && s.state == "accepted")
                    .map(|s| s.amount)
                    .sum();
                Stock {
                    hub,
                    held: held.iter().map(|n| n.denom).sum(),
                    reserved,
                    notes: held.len(),
                    unlinked: held.iter().filter(|n| n.unlinked).count(),
                }
            })
            .collect();
        Self {
            stock,
            coins: r
                .coins
                .as_ref()
                .map(|coins| (coins.iter().sum(), coins.len())),
            under_way: r.swaps.iter().filter(|s| s.open()).collect(),
        }
    }

    /// The height of the chain at `url`: its hub's scan, or for Bitcoin
    /// itself, and Lightning's routes, the furthest any hub scanned.
    fn height(&self, url: &str) -> Option<i64> {
        if url.starts_with("bitcoin:") || url.starts_with("lightning:") {
            return self.stock.iter().filter_map(|s| s.hub.height()).max();
        }
        self.stock
            .iter()
            .find(|s| s.hub.url == url)
            .and_then(|s| s.hub.height())
    }

    /// Blocks until `swap` must have moved on: the provider's lock refunds
    /// at its expiry at `to`, and once paid, the client's lock at `from`.
    fn left(&self, swap: &Swap) -> Option<i64> {
        match swap.state.as_str() {
            "paid" => Some(swap.expiry_from - self.height(&swap.from)?),
            _ => Some(swap.expiry - self.height(&swap.to)?),
        }
    }
}

pub(super) fn report(r: &Records, now: i64, service: Check, data: &Path) -> Report {
    let state = Now::of(r);
    let mut series = flows(r);
    series.extend(levels(r));
    let checks = checks(r, &state, now, service);
    let pages = vec![
        overview(checks.clone(), recent(r)),
        swaps(r, &state),
        liquidity(&state),
        fees(),
        Page::new(
            "Health",
            vec![vec![Panel::Health(checks), Panel::Table(alerts(r))]],
        ),
    ];
    Report {
        title: "licash swaps".into(),
        subtitle: format!("{} · {}", data.display(), r.network),
        now,
        since: r.created_at,
        series,
        pages,
    }
}

fn total(title: &str, of: Ref, better: Better) -> Card {
    Card::new(title, Value::of(of), better)
}

fn now_card(title: &str, value: Option<i64>, unit: Unit, detail: impl Into<String>) -> Card {
    Card::new(
        title,
        Value::now(value, unit, detail, Tone::Normal),
        Better::Neither,
    )
}

/// The fees earned less what the swaps lost cost.
fn net() -> Value {
    Value::Total {
        plus: vec![Ref::sum("fees")],
        minus: vec![Ref::sum("lost")],
    }
}

/// The swaps done over those and the ones that ended otherwise.
fn success() -> Value {
    Value::Ratio {
        num: vec![Ref::count("volume")],
        den: vec![Ref::count("volume"), Ref::count("failures")],
    }
}

fn overview(checks: Vec<Check>, recent: Table) -> Page {
    Page::new(
        "Overview",
        vec![
            vec![Panel::Cards(vec![
                total("Swapped", Ref::sum("volume"), Better::Up),
                total("Swaps", Ref::count("volume"), Better::Up),
                total("Fees", Ref::sum("fees"), Better::Up),
                Card::new("Net", net(), Better::Up),
                Card::new("Success", success(), Better::Up),
                total("Notes held", Ref::level("stock"), Better::Neither),
            ])],
            // Fees and the notes held are charted on their own pages.
            vec![
                Panel::Health(checks),
                Panel::chart("Swapped", "volume", Measure::Sum),
            ],
            vec![Panel::Table(recent)],
        ],
    )
}

/// What the provider did, newest first: the notices it recorded, a swap's
/// with its route.
fn recent(r: &Records) -> Table {
    let swaps: HashMap<&str, &Swap> = r.swaps.iter().map(|s| (s.id.as_str(), s)).collect();
    let route = |id: &str| swaps.get(id).map_or(String::new(), |s| s.route());
    let rows = r
        .events
        .iter()
        .map(|e| {
            let (tone, event, route, amount, detail) = match &e.notice {
                Some(Notice::Accepted {
                    amount,
                    fee,
                    from,
                    to,
                    ..
                }) => (
                    Tone::Normal,
                    "taken".into(),
                    format!("{} → {}", short(from), short(to)),
                    Some(*amount as i64),
                    format!("fee {}", licash_dashboard::fee(*fee as i64, *amount as i64)),
                ),
                Some(Notice::Done { id, amount, fee }) => (
                    Tone::Good,
                    "done".into(),
                    route(id),
                    Some(*amount as i64),
                    format!(
                        "earned {}",
                        licash_dashboard::fee(*fee as i64, *amount as i64)
                    ),
                ),
                Some(Notice::Refunded { id, amount }) => (
                    Tone::Warn,
                    "refunded".into(),
                    route(id),
                    Some(*amount as i64),
                    NEVER_CLAIMED.to_owned(),
                ),
                Some(Notice::Failed { id, reason }) => (
                    Tone::Warn,
                    "failed".into(),
                    route(id),
                    swaps.get(id.as_str()).map(|s| s.amount),
                    reason.clone(),
                ),
                Some(Notice::Lost { id, amount, reason }) => (
                    Tone::Bad,
                    "lost".into(),
                    route(id),
                    Some(*amount as i64),
                    reason.clone(),
                ),
                Some(Notice::Alert { message }) => (
                    Tone::Warn,
                    "alert".into(),
                    String::new(),
                    None,
                    message.clone(),
                ),
                // Of a newer provider.
                _ => (
                    Tone::Muted,
                    e.kind.replace('_', " "),
                    String::new(),
                    None,
                    String::new(),
                ),
            };
            Row::new(
                Some(e.at),
                tone,
                vec![
                    Cell::Time(e.at),
                    Cell::Text(event),
                    Cell::Text(route),
                    amount.map_or(Cell::Empty, Cell::Sat),
                    Cell::Text(detail),
                ],
            )
        })
        .collect();
    Table::in_range(
        "Recent events",
        &["when", "event", "route", "amount", "detail"],
        rows,
        "nothing happened",
    )
}

fn swaps(r: &Records, state: &Now) -> Page {
    let paid = state.under_way.iter().filter(|s| s.state == "paid").count();
    let under_way = match paid {
        0 => "taken, not ended".to_owned(),
        n => format!("{n} paid, claiming"),
    };
    Page::new(
        "Swaps",
        vec![
            vec![Panel::Cards(vec![
                total("Taken", Ref::count("taken"), Better::Up),
                total("Done", Ref::count("volume"), Better::Up),
                total(
                    "Never claimed",
                    Ref::count("failures").of(NEVER_CLAIMED),
                    Better::Down,
                ),
                total("Failures", Ref::count("failures"), Better::Down),
                total("Lost", Ref::count("lost"), Better::Down),
                now_card(
                    "Under way",
                    Some(state.under_way.len() as i64),
                    Unit::Count,
                    under_way,
                ),
            ])],
            vec![
                Panel::chart("Swaps taken", "taken", Measure::Count),
                Panel::breakdown(
                    "Failures by reason",
                    "failures",
                    vec![
                        Column::value("failed", Ref::count("failures")),
                        Column::value("amount", Ref::sum("failures")),
                        Column::change("change", Ref::count("failures")),
                    ],
                ),
            ],
            vec![Panel::breakdown(
                "Routes",
                "taken",
                vec![
                    Column::value("taken", Ref::count("taken")),
                    Column::value("done", Ref::count("volume")),
                    Column::value("swapped", Ref::sum("volume")),
                    Column::value("fees", Ref::sum("fees")),
                    Column::value("lost", Ref::sum("lost")),
                    Column::share("share", Ref::sum("volume")),
                    Column::change("change", Ref::sum("volume")),
                ],
            )],
            vec![Panel::Table(under_way_table(state))],
            vec![Panel::Table(failures(r))],
        ],
    )
}

/// The swaps under way, the soonest due first.
fn under_way_table(state: &Now) -> Table {
    let mut rows: Vec<(i64, Row)> = state
        .under_way
        .iter()
        .map(|s| {
            let left = state.left(s);
            let what = match (s.state.as_str(), s.chain) {
                ("accepted", _) if s.lightning => "paying the invoice",
                ("accepted", false) => "locking",
                ("accepted", true) => "paying the coin",
                ("locked", false) => "client to claim",
                ("locked", true) => "client to claim the coin",
                _ => "claiming the client's lock",
            };
            let tone = match left {
                Some(left) if left <= SOON && s.state == "paid" => Tone::Bad,
                Some(left) if left <= SOON => Tone::Warn,
                _ => Tone::Normal,
            };
            let row = Row::new(
                None,
                tone,
                vec![
                    Cell::Time(s.created_at),
                    Cell::Text(s.route()),
                    Cell::Sat(s.amount),
                    left.map_or(Cell::Empty, Cell::Blocks),
                    Cell::Text(what.into()),
                ],
            );
            (left.unwrap_or(i64::MAX), row)
        })
        .collect();
    rows.sort_by_key(|(left, _)| *left);
    Table::now(
        "Under way",
        &["taken", "route", "amount", "due in", "now"],
        rows.into_iter().map(|(_, row)| row).collect(),
        "no swap under way",
    )
}

/// The swaps that ended without paying, the latest first.
fn failures(r: &Records) -> Table {
    let mut rows: Vec<Row> = r
        .swaps
        .iter()
        .filter(|s| s.failure().is_some())
        .map(|s| {
            let tone = if s.state == "lost" {
                Tone::Bad
            } else {
                Tone::Warn
            };
            let reason = match s.state.as_str() {
                "refunded" => format!("{NEVER_CLAIMED}: refunded"),
                state => format!("{state}: {}", s.reason.as_deref().unwrap_or("no reason")),
            };
            Row::new(
                Some(s.updated_at),
                tone,
                vec![
                    Cell::Time(s.updated_at),
                    Cell::Text(s.route()),
                    Cell::Sat(s.amount),
                    Cell::Text(reason),
                ],
            )
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.at));
    Table::in_range(
        "Failed swaps",
        &["when", "route", "amount", "why"],
        rows,
        "no swap failed",
    )
}

fn liquidity(state: &Now) -> Page {
    let sum = |f: fn(&Stock) -> i64| state.stock.iter().map(f).sum::<i64>();
    let swapping = state.stock.iter().filter(|s| s.swaps()).count();
    let taking = state
        .under_way
        .iter()
        .filter(|s| s.state == "accepted")
        .count();
    let on_chain = match state.coins {
        Some((value, coins)) => now_card(
            "On chain",
            Some(value),
            Unit::Sat,
            format!("{coins} coins unspent"),
        ),
        None => now_card("On chain", None, Unit::Sat, "the wallet follows no chain"),
    };
    let (notes, unlinked) = (
        state.stock.iter().map(|s| s.notes).sum::<usize>(),
        state.stock.iter().map(|s| s.unlinked).sum::<usize>(),
    );
    let unlinked = match notes {
        0 => now_card("Unlinked", Some(0), Unit::Count, "no notes held"),
        n => now_card(
            "Unlinked",
            Some(unlinked as i64),
            Unit::Count,
            format!(
                "{:.0}% of the {} notes held",
                100.0 * unlinked as f64 / n as f64,
                grouped(n as i64)
            ),
        ),
    };
    Page::new(
        "Liquidity",
        vec![
            vec![Panel::Cards(vec![
                total("Notes held", Ref::level("stock"), Better::Neither),
                now_card(
                    "Reserved",
                    Some(sum(|s| s.reserved)),
                    Unit::Sat,
                    format!("for {taking} swaps locking"),
                ),
                now_card(
                    "Free",
                    Some(sum(|s| s.free())),
                    Unit::Sat,
                    format!("to lock at {swapping} hubs"),
                ),
                on_chain,
                unlinked,
            ])],
            vec![
                Panel::chart("Notes held", "stock", Measure::Level),
                Panel::breakdown(
                    "By hub",
                    "stock",
                    vec![
                        Column::value("held", Ref::level("stock")),
                        Column::share("share", Ref::level("stock")),
                        Column::value("moved", Ref::sum("stock")),
                        Column::per_day("per day", Ref::sum("stock")),
                        Column::Runway {
                            title: "runway".into(),
                            of: "stock".into(),
                        },
                    ],
                ),
            ],
            vec![Panel::Table(hubs(state))],
        ],
    )
}

/// The hubs the owner added, as the provider swaps to them now.
fn hubs(state: &Now) -> Table {
    let rows = state
        .stock
        .iter()
        .map(|s| {
            let tone = match (s.swaps(), s.free()) {
                (false, _) => Tone::Muted,
                (true, 0) => Tone::Warn,
                _ => Tone::Normal,
            };
            Row::new(
                None,
                tone,
                vec![
                    Cell::Text(short(&s.hub.url)),
                    Cell::Sat(s.held),
                    Cell::Sat(s.reserved),
                    Cell::Sat(s.free()),
                    s.hub.height().map_or(Cell::Empty, Cell::Int),
                    Cell::Time(s.hub.updated_at),
                ],
            )
        })
        .collect();
    Table::now(
        "Hubs",
        &["hub", "held", "reserved", "free", "height", "heard"],
        rows,
        "no hub added: add one with licash-wallet",
    )
}

fn fees() -> Page {
    let rate = Value::Ratio {
        num: vec![Ref::sum("fees")],
        den: vec![Ref::sum("volume")],
    };
    Page::new(
        "Fees",
        vec![
            vec![Panel::Cards(vec![
                total("Fees", Ref::sum("fees"), Better::Up),
                total("Lost", Ref::sum("lost"), Better::Down),
                Card::new("Net", net(), Better::Up),
                Card::new("Of the volume", rate, Better::Neither),
            ])],
            vec![
                Panel::chart("Fees by route", "fees", Measure::Sum),
                Panel::chart("Lost by route", "lost", Measure::Sum),
            ],
            vec![Panel::breakdown(
                "Fees by route",
                "fees",
                vec![
                    Column::value("fees", Ref::sum("fees")),
                    Column::value("swapped", Ref::sum("volume")),
                    Column::share("share", Ref::sum("fees")),
                    Column::change("change", Ref::sum("fees")),
                    Column::per_day("per day", Ref::sum("fees")),
                ],
            )],
            vec![Panel::breakdown(
                "Losses by route",
                "lost",
                vec![
                    Column::value("lost", Ref::sum("lost")),
                    Column::value("swaps", Ref::count("lost")),
                    Column::change("change", Ref::sum("lost")),
                ],
            )],
        ],
    )
}

fn alerts(r: &Records) -> Table {
    let mut rows: Vec<Row> = r
        .alerts
        .iter()
        .map(|(at, message)| {
            Row::new(
                Some(*at),
                Tone::Warn,
                vec![Cell::Time(*at), Cell::Text(message.clone())],
            )
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.at));
    Table::in_range("Alerts", &["when", "alert"], rows, "no alert")
}

fn checks(r: &Records, state: &Now, now: i64, service: Check) -> Vec<Check> {
    let mut checks = vec![service];
    let due: Vec<_> = state
        .under_way
        .iter()
        .filter(|s| s.state == "paid" && state.left(s).is_some_and(|left| left <= SOON))
        .collect();
    let old = state
        .under_way
        .iter()
        .filter(|s| now - s.created_at > HOUR)
        .count();
    checks.push(match (due.len(), old, state.under_way.len()) {
        (0, 0, 0) => Check::new("Swaps", Tone::Good, "none under way"),
        (0, 0, n) => Check::new("Swaps", Tone::Good, format!("{n} under way")),
        (0, o, n) => Check::new(
            "Swaps",
            Tone::Warn,
            format!("{o} of {n} under way for over an hour"),
        ),
        (d, _, _) => Check::new(
            "Swaps",
            Tone::Bad,
            format!("{d} paid, the client's lock to claim within {SOON} blocks or lose it"),
        ),
    });
    let swapping: Vec<_> = state.stock.iter().filter(|s| s.swaps()).collect();
    let empty: Vec<_> = swapping
        .iter()
        .filter(|s| s.free() == 0)
        .map(|s| short(&s.hub.url))
        .collect();
    checks.push(match (state.stock.len(), swapping.len(), empty.len()) {
        (0, _, _) => Check::new("Stock", Tone::Bad, "no hub added"),
        (_, 0, _) => Check::new(
            "Stock",
            Tone::Warn,
            "no hub added follows Bitcoin: no swaps",
        ),
        (_, _, 0) => Check::new("Stock", Tone::Good, "notes free at every hub"),
        _ => Check::new(
            "Stock",
            Tone::Warn,
            format!("nothing free to lock at {}", empty.join(", ")),
        ),
    });
    let stale: Vec<_> = state
        .stock
        .iter()
        .filter(|s| now - s.hub.updated_at > STALE)
        .collect();
    checks.push(match stale.iter().min_by_key(|s| s.hub.updated_at) {
        None if state.stock.is_empty() => Check::new("Hubs", Tone::Muted, "none added"),
        None => Check::new("Hubs", Tone::Good, "every hub heard from lately"),
        Some(s) => Check::new(
            "Hubs",
            Tone::Warn,
            format!(
                "{} of {} not heard from lately, {} for {}",
                stale.len(),
                state.stock.len(),
                short(&s.hub.url),
                span(now - s.hub.updated_at)
            ),
        ),
    });
    let lost: Vec<_> = r
        .swaps
        .iter()
        .filter(|s| s.state == "lost" && now - s.updated_at < 7 * DAY)
        .collect();
    checks.push(match lost.len() {
        0 => Check::new("Losses", Tone::Good, "none in the last week"),
        n => Check::new(
            "Losses",
            Tone::Bad,
            format!(
                "{n} in the last week, {} sat",
                grouped(lost.iter().map(|s| s.amount).sum())
            ),
        ),
    });
    let ended: Vec<_> = r
        .swaps
        .iter()
        .filter(|s| !s.open() && now - s.updated_at < DAY)
        .collect();
    let failed = ended.iter().filter(|s| s.failure().is_some()).count();
    checks.push(match (failed, ended.len()) {
        (_, 0) => Check::new("Failures", Tone::Muted, "no swap ended in the last day"),
        (0, n) => Check::new(
            "Failures",
            Tone::Good,
            format!("none of {n} in the last day"),
        ),
        (f, n) => Check::new(
            "Failures",
            if 5 * f >= n { Tone::Warn } else { Tone::Normal },
            format!("{f} of {n} in the last day"),
        ),
    });
    let recent = r.alerts.iter().filter(|(at, _)| now - at < DAY).count();
    checks.push(match recent {
        0 => Check::new("Alerts", Tone::Good, "none in the last day"),
        n => Check::new("Alerts", Tone::Warn, format!("{n} in the last day")),
    });
    checks
}
