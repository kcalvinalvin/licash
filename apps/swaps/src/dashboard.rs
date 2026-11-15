//! The provider's dashboard (`licash-swaps dashboard`): what its swaps file
//! and its wallet's recorded, read only, beside the running provider or
//! without it, into a [`Report`] for its operator. History comes from the
//! records themselves: the swaps, dated by when each was taken and when it
//! ended, and the wallet's notes, by when each came and went. Nothing is
//! written anywhere: files of older versions are read as they are, never
//! migrated.

mod pages;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use licash_dashboard::{Probe, Report, Series, Unit};
use rusqlite::{Connection, OpenFlags};

/// The newest schemas read, as each file's `user_version`.
const SWAPS_VERSION: i64 = 3;
const WALLET_VERSION: i64 = 7;

/// Why a swap ended without paying, beside a reason recorded.
const NEVER_CLAIMED: &str = "never claimed";
const NOT_LOCKED: &str = "not locked";

/// The most events the overview lists.
const RECENT: usize = 100;

/// A swap as the provider recorded it.
struct Swap {
    /// Its id, in hex, as its notices name it.
    id: String,
    /// A payout on Bitcoin, not a swap of notes.
    chain: bool,
    from: String,
    to: String,
    amount: i64,
    fee: i64,
    expiry_from: i64,
    expiry: i64,
    state: String,
    reason: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl Swap {
    fn open(&self) -> bool {
        matches!(self.state.as_str(), "accepted" | "locked" | "paid")
    }

    /// Why it failed, if it did: the reason recorded, short of its detail.
    fn failure(&self) -> Option<String> {
        let recorded = || {
            let reason = self.reason.as_deref().unwrap_or(NOT_LOCKED);
            reason.split(": ").next().unwrap_or(reason).to_owned()
        };
        match self.state.as_str() {
            "failed" | "lost" => Some(recorded()),
            "refunded" => Some(NEVER_CLAIMED.to_owned()),
            _ => None,
        }
    }

    fn route(&self) -> String {
        format!("{} → {}", short(&self.from), short(&self.to))
    }
}

/// A hub of the wallet's, as it last said of itself.
struct Hub {
    id: i64,
    url: String,
    /// The owner added it, rather than visited it to pay there.
    trusted: bool,
    /// None if the wallet could not read what it said.
    info: Option<licash_api::Info>,
    updated_at: i64,
}

impl Hub {
    /// The height it scanned Bitcoin to, if it follows it.
    fn height(&self) -> Option<i64> {
        let locks = self.info.as_ref()?.locks.as_ref()?;
        Some(i64::from(locks.height))
    }
}

/// A note the wallet held, at its hub: from when it came to when it was
/// spent, or went.
struct Note {
    hub: i64,
    denom: i64,
    added_at: i64,
    left_at: Option<i64>,
    unlinked: bool,
}

/// A notice the provider recorded for its operator.
struct Logged {
    at: i64,
    kind: String,
    /// None if of a newer provider.
    notice: Option<licash_swaps::Notice>,
}

struct Records {
    created_at: i64,
    network: String,
    swaps: Vec<Swap>,
    alerts: Vec<(i64, String)>,
    /// The latest [`RECENT`], newest first.
    events: Vec<Logged>,
    hubs: Vec<Hub>,
    notes: Vec<Note>,
    /// What the wallet's unspent coins on Bitcoin pay: none before its
    /// schema followed Bitcoin.
    coins: Option<Vec<i64>>,
}

/// The provider's dashboard of its data directory `data` as of now, its
/// service asked by `probe`.
pub fn report(data: &Path, probe: Option<&Probe>) -> Result<Report> {
    let records = read(data)?;
    let service = probe.map_or_else(licash_dashboard::unasked, Probe::check);
    Ok(pages::report(&records, now(), service, data))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// The SQLite file at `path` read only, and its schema: none if there is
/// none, or it is still empty. Never one newer than `newest`.
fn read_only(path: &Path, newest: i64) -> Result<Option<(Connection, i64)>> {
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("open {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.pragma_update(None, "query_only", true)?;
    match conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? {
        0 => Ok(None),
        v if v <= newest => Ok(Some((conn, v))),
        v => bail!(
            "{} is of version {v}, newer than this provider reads",
            path.display()
        ),
    }
}

fn read(data: &Path) -> Result<Records> {
    let path = data.join("swaps.sqlite");
    let Some((swaps, version)) = read_only(&path, SWAPS_VERSION)? else {
        bail!("no swaps file at {}: create it with init", path.display());
    };
    let path = data.join("wallet.sqlite");
    let Some((wallet, wallet_version)) = read_only(&path, WALLET_VERSION)? else {
        bail!("no wallet at {}: create it with init", path.display());
    };
    // Each file read in one transaction: what it says is whole.
    swaps.execute_batch("BEGIN")?;
    wallet.execute_batch("BEGIN")?;
    Ok(Records {
        created_at: swaps.query_row("SELECT created_at FROM meta", [], |r| r.get(0))?,
        network: wallet.query_row("SELECT network FROM meta", [], |r| r.get(0))?,
        swaps: read_swaps(&swaps, version)?,
        alerts: read_alerts(&swaps)?,
        events: read_events(&swaps, RECENT)?,
        hubs: read_hubs(&wallet, wallet_version)?,
        notes: read_notes(&wallet)?,
        coins: read_coins(&wallet, wallet_version)?,
    })
}

fn read_swaps(c: &Connection, version: i64) -> Result<Vec<Swap>> {
    // Version 1 swapped notes only.
    let kind = if version >= 2 { "kind" } else { "'notes'" };
    let mut stmt = c.prepare(&format!(
        "SELECT {kind}, from_url, to_url, amount, fee, expiry_from, expiry, state, reason,
            created_at, updated_at, lower(hex(id))
         FROM swaps ORDER BY created_at, rowid"
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok(Swap {
            chain: r.get::<_, String>(0)? == "chain",
            from: r.get(1)?,
            to: r.get(2)?,
            amount: r.get(3)?,
            fee: r.get(4)?,
            expiry_from: r.get(5)?,
            expiry: r.get(6)?,
            state: r.get(7)?,
            reason: r.get(8)?,
            created_at: r.get(9)?,
            updated_at: r.get(10)?,
            id: r.get(11)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The latest `limit` notices the provider recorded, newest first.
fn read_events(c: &Connection, limit: usize) -> Result<Vec<Logged>> {
    let mut stmt = c.prepare("SELECT at, kind, json FROM events ORDER BY seq DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(Logged {
            at: r.get(0)?,
            kind: r.get(1)?,
            notice: serde_json::from_str(&r.get::<_, String>(2)?).ok(),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn read_alerts(c: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = c.prepare("SELECT at, json FROM events WHERE kind = 'alert' ORDER BY seq")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?;
    let mut alerts = Vec::new();
    for row in rows {
        let (at, json) = row?;
        let message = serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("message")?.as_str().map(str::to_owned))
            .unwrap_or(json);
        alerts.push((at, message));
    }
    Ok(alerts)
}

fn read_hubs(c: &Connection, version: i64) -> Result<Vec<Hub>> {
    // Every hub of a file before version 6 was added.
    let trusted = if version >= 6 { "h.trusted" } else { "1" };
    let mut stmt = c.prepare(&format!(
        "SELECT h.id, r.url, h.info, h.updated_at, {trusted}
         FROM hubs h JOIN reach r ON r.hub = h.id ORDER BY r.url"
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok(Hub {
            id: r.get(0)?,
            url: r.get(1)?,
            info: serde_json::from_str(&r.get::<_, String>(2)?).ok(),
            updated_at: r.get(3)?,
            trusted: r.get::<_, i64>(4)? == 1,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Every note the wallet held: a note spent left when the payment
/// spending it was made, as the wallet's balance counts it.
fn read_notes(c: &Connection) -> Result<Vec<Note>> {
    let mut stmt = c.prepare(
        "SELECT k.hub, n.denom, n.added_at, coalesce(s.created_at, l.created_at, n.gone_at),
            n.unlinked = 1
         FROM notes n JOIN keysets k ON k.id = n.keyset
         LEFT JOIN sends s ON s.id = n.spent_by
         LEFT JOIN locks l ON l.id = n.spent_by
         ORDER BY n.added_at, n.rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Note {
            hub: r.get(0)?,
            denom: r.get(1)?,
            added_at: r.get(2)?,
            left_at: r.get(3)?,
            unlinked: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn read_coins(c: &Connection, version: i64) -> Result<Option<Vec<i64>>> {
    if version < 2 {
        return Ok(None);
    }
    let mut stmt =
        c.prepare("SELECT value FROM silent_coins WHERE spent_by IS NULL AND spent_at IS NULL")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(Some(rows.collect::<Result<_, _>>()?))
}

/// A hub's URL as a label: its host, an onion's shortened.
fn short(url: &str) -> String {
    if url.starts_with("bitcoin:") {
        return "on-chain".to_owned();
    }
    let host = url.split("://").nth(1).unwrap_or(url).trim_end_matches('/');
    match host.split_once(".onion") {
        Some((name, rest)) if name.chars().count() > 10 => {
            format!("{}….onion{rest}", name.chars().take(8).collect::<String>())
        }
        _ => host.to_owned(),
    }
}

/// The series of what the swaps did, each at the time it did it: taken
/// when accepted, paid and failed when they ended.
fn flows(r: &Records) -> Vec<Series> {
    let mut taken = Series::flow("taken", "Swaps taken", Unit::Sat);
    let mut volume = Series::flow("volume", "Swapped", Unit::Sat);
    let mut fees = Series::flow("fees", "Fees earned", Unit::Sat);
    let mut lost = Series::flow("lost", "Lost", Unit::Sat);
    let mut failures = Series::flow("failures", "Failures", Unit::Sat);
    for s in &r.swaps {
        let route = s.route();
        taken.add(s.created_at, &route, s.amount);
        match s.state.as_str() {
            "done" => {
                volume.add(s.updated_at, &route, s.amount);
                fees.add(s.updated_at, &route, s.fee);
            }
            "lost" => lost.add(s.updated_at, &route, s.amount),
            _ => {}
        }
        if let Some(why) = s.failure() {
            failures.add(s.updated_at, &why, s.amount);
        }
    }
    vec![taken, volume, fees, lost, failures]
}

/// The series of the notes the wallet held, by hub: those it swaps from.
fn levels(r: &Records) -> Vec<Series> {
    let mut stock = Series::level("stock", "Notes held", Unit::Sat);
    for hub in r.hubs.iter().filter(|h| h.trusted) {
        stock.label(&short(&hub.url));
    }
    for note in &r.notes {
        let Some(hub) = r.hubs.iter().find(|h| h.id == note.hub && h.trusted) else {
            continue;
        };
        let label = short(&hub.url);
        stock.add(note.added_at, &label, note.denom);
        if let Some(at) = note.left_at {
            stock.add(at, &label, -note.denom);
        }
    }
    vec![stock]
}
