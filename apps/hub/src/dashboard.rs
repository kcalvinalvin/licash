//! The hub's dashboard (`licash-hub dashboard`): what its files in its
//! data directory recorded, read only, beside the running hub or without
//! it, into a [`Report`] for its operator. History comes from the records
//! themselves: the ledger's notes and events, the Lightning gateway's
//! receipts, the deposits and the openings. Nothing is written anywhere.

mod pages;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use licash_contracts::hub::Summary;
use licash_dashboard::{grouped, Probe, Report, Series, Unit};
use licash_hub::{Event, ForwardRecord, Ledger, LockRecord, Notes, Notice, Settled};
use rusqlite::{Connection, OpenFlags};

/// The kinds of payment, as the dashboard labels them.
const TRANSFER: &str = "transfers";
const LOCK: &str = "locks";
const PAY: &str = "lightning out";
const RECEIVE: &str = "lightning in";
const DEPOSIT: &str = "deposits";
const CONTRACT: &str = "contracts";

/// A receipt of the Lightning gateway: notes asked for over Lightning.
struct Receipt {
    amount: u64,
    msat: u64,
    lock: Option<[u8; 32]>,
    state: String,
    created_at: i64,
    done_at: Option<i64>,
}

/// A deposit asked for on Bitcoin.
struct Deposit {
    amount: u64,
    pays: u64,
    /// Its payment confirmed.
    confirmed: bool,
    lock: Option<[u8; 32]>,
    state: String,
    created_at: i64,
    done_at: Option<i64>,
}

/// Everything the hub's files say, read at one moment each.
struct Records {
    created_at: i64,
    tip: Option<u32>,
    notes: Notes,
    locks: Vec<LockRecord>,
    forwards: Vec<ForwardRecord>,
    events: Vec<Event>,
    /// None if the hub never ran a Lightning gateway.
    receipts: Option<Vec<Receipt>>,
    /// None if the hub never took deposits.
    deposits: Option<Vec<Deposit>>,
    openings: Vec<Summary>,
}

/// The hub's dashboard of its data directory `data` as of now, its service
/// asked by `probe`.
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

fn read(data: &Path) -> Result<Records> {
    let ledger = Ledger::open(&data.join("hub.sqlite")).context("read the hub")?;
    let mut events = Vec::new();
    loop {
        let after = events.last().map_or(0, |e: &Event| e.seq);
        let page = ledger.events(after, 10_000)?;
        if page.is_empty() {
            break;
        }
        events.extend(page);
    }
    Ok(Records {
        created_at: ledger.created_at()? as i64,
        tip: ledger.tip()?,
        notes: ledger.notes()?,
        locks: ledger.locks()?,
        forwards: ledger.forwards()?,
        events,
        receipts: receipts(&data.join("lightning.sqlite"))?,
        deposits: deposits(&data.join("deposits.sqlite"))?,
        openings: licash_contracts::hub::openings(data).context("read the openings")?,
    })
}

/// The SQLite file at `path`, of schema 1, read only: none if there is
/// none, or it is still empty.
fn read_only(path: &Path) -> Result<Option<Connection>> {
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("open {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.pragma_update(None, "query_only", true)?;
    match conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? {
        0 => Ok(None),
        1 => Ok(Some(conn)),
        v => bail!(
            "{} is of version {v}, newer than this hub reads",
            path.display()
        ),
    }
}

fn lock_id(bytes: Option<Vec<u8>>) -> Option<[u8; 32]> {
    bytes.and_then(|b| b.try_into().ok())
}

fn receipts(path: &Path) -> Result<Option<Vec<Receipt>>> {
    let Some(conn) = read_only(path)? else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT amount, msat, lock, state, created_at, done_at FROM receipts
         ORDER BY created_at, rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Receipt {
            amount: r.get::<_, i64>(0)? as u64,
            msat: r.get::<_, i64>(1)? as u64,
            lock: lock_id(r.get(2)?),
            state: r.get(3)?,
            created_at: r.get(4)?,
            done_at: r.get(5)?,
        })
    })?;
    Ok(Some(rows.collect::<Result<_, _>>()?))
}

fn deposits(path: &Path) -> Result<Option<Vec<Deposit>>> {
    let Some(conn) = read_only(path)? else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT amount, pays, txid IS NOT NULL, lock, state, created_at, done_at FROM deposits
         ORDER BY created_at, rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Deposit {
            amount: r.get::<_, i64>(0)? as u64,
            pays: r.get::<_, i64>(1)? as u64,
            confirmed: r.get(2)?,
            lock: lock_id(r.get(3)?),
            state: r.get(4)?,
            created_at: r.get(5)?,
            done_at: r.get(6)?,
        })
    })?;
    Ok(Some(rows.collect::<Result<_, _>>()?))
}

/// What each lock paid for, beside a transfer between holders: a payment
/// out over Lightning, notes for one in, or a deposit's.
fn lock_kinds(r: &Records) -> HashMap<[u8; 32], &'static str> {
    let mut kinds = HashMap::new();
    for f in &r.forwards {
        kinds.insert(f.lock, PAY);
    }
    for lock in r.receipts.iter().flatten().filter_map(|x| x.lock) {
        kinds.insert(lock, RECEIVE);
    }
    for lock in r.deposits.iter().flatten().filter_map(|x| x.lock) {
        kinds.insert(lock, DEPOSIT);
    }
    kinds
}

/// A payment's failure as an operator reads it: LND's reasons without
/// their prefix.
fn reason(why: &str) -> String {
    match why.strip_prefix("FAILURE_REASON_") {
        Some(r) => r.to_lowercase().replace('_', " "),
        None => why.to_owned(),
    }
}

/// The sat an invoice asks for, if it reads as one.
fn invoice_sat(request: &str) -> Option<u64> {
    let invoice: lightning_invoice::Bolt11Invoice = request.parse().ok()?;
    Some(invoice.amount_milli_satoshis()?.div_ceil(1000))
}

/// The series of what the hub's records say happened.
fn flows(r: &Records) -> Vec<Series> {
    let mut volume = Series::flow("volume", "Volume", Unit::Sat);
    let mut fees = Series::flow("fees", "Fees", Unit::Sat);
    let mut failures = Series::flow("failures", "Failed payments", Unit::Sat);
    let mut unpaid = Series::flow("unpaid", "Asked for, never paid", Unit::Count);
    let mut openings = Series::flow("openings", "Openings", Unit::Count);
    for kind in [TRANSFER, LOCK, PAY, RECEIVE, DEPOSIT] {
        volume.label(kind);
        fees.label(kind);
    }
    let kinds = lock_kinds(r);
    for e in &r.events {
        let at = e.at as i64;
        match e.notice() {
            Some(Notice::Released { amount, fee, .. }) => {
                volume.add(at, TRANSFER, amount as i64);
                fees.add(at, TRANSFER, fee as i64);
            }
            Some(Notice::Settled {
                proposal,
                amount,
                fee,
                branch,
            }) => {
                let id: Option<[u8; 32]> =
                    hex::decode(&proposal).ok().and_then(|b| b.try_into().ok());
                // The others' fees and outcomes are their records'.
                if id.is_some_and(|id| kinds.contains_key(&id)) {
                    continue;
                }
                match branch {
                    Settled::Pay => volume.add(at, LOCK, amount as i64),
                    Settled::Refund => failures.add(at, "lock refunded", amount as i64),
                }
                fees.add(at, LOCK, fee as i64);
            }
            Some(Notice::DoubleSpend { .. }) => failures.add(at, "double spend", 0),
            _ => {}
        }
    }
    for f in &r.forwards {
        let Some(at) = f.resolved_at.map(|t| t as i64) else {
            continue;
        };
        match f.state.as_str() {
            "paid" => {
                volume.add(at, PAY, f.amount as i64);
                // Gross: the routing fee the node paid is not recorded.
                if let Some(sat) = invoice_sat(&f.request) {
                    fees.add(at, PAY, f.amount.saturating_sub(sat) as i64);
                }
            }
            "failed" => {
                let why = reason(f.reason.as_deref().unwrap_or("unknown"));
                failures.add(at, &why, f.amount as i64);
            }
            _ => {}
        }
    }
    for x in r.receipts.iter().flatten() {
        let at = x.done_at.unwrap_or(x.created_at);
        match (x.state.as_str(), x.lock) {
            ("settled", _) => {
                volume.add(at, RECEIVE, (x.msat / 1000) as i64);
                fees.add(at, RECEIVE, (x.msat / 1000).saturating_sub(x.amount) as i64);
            }
            ("canceled", Some(_)) => failures.add(at, "receipt canceled", x.amount as i64),
            ("canceled", None) => unpaid.add(at, "invoices", 1),
            _ => {}
        }
    }
    for x in r.deposits.iter().flatten() {
        let at = x.done_at.unwrap_or(x.created_at);
        match (x.state.as_str(), x.confirmed) {
            ("settled", _) => {
                volume.add(at, DEPOSIT, x.pays as i64);
                fees.add(at, DEPOSIT, x.pays.saturating_sub(x.amount) as i64);
            }
            ("canceled", true) => failures.add(at, "deposit canceled", x.pays as i64),
            ("canceled", false) => unpaid.add(at, "deposit addresses", 1),
            _ => {}
        }
    }
    for o in &r.openings {
        if o.signed {
            openings.add(o.created_at as i64, "signed", 1);
            if let Some(terms) = &o.terms {
                fees.add(o.created_at as i64, CONTRACT, terms.fee as i64);
            }
        } else if o.abandoned {
            openings.add(o.updated_at as i64, "given up", 1);
        }
    }
    vec![volume, fees, failures, unpaid, openings]
}

/// An anonymity set's name: its denomination, and its keyset if the hub
/// issued under `many`.
fn set_name(many: bool, keyset: u32, denom: u64) -> String {
    match many {
        false => format!("{} sat", grouped(denom as i64)),
        true => format!("k{keyset} {} sat", grouped(denom as i64)),
    }
}

/// The series of levels the hub's records moved: its notes outstanding,
/// by value and by denomination.
fn levels(r: &Records) -> Vec<Series> {
    let mut ecash = Series::level("ecash", "Notes outstanding", Unit::Sat);
    let mut notes = Series::level("notes", "Notes outstanding by denomination", Unit::Count);
    for label in [HOLDERS, HUB, LOCKED] {
        ecash.label(label);
    }
    let keysets: HashSet<u32> = r.notes.movements.iter().map(|m| m.keyset).collect();
    let mut sets: Vec<(u32, u64)> = r
        .notes
        .movements
        .iter()
        .map(|m| (m.keyset, m.denom))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    sets.sort();
    let name = |keyset: u32, denom: u64| set_name(keysets.len() > 1, keyset, denom);
    for (keyset, denom) in &sets {
        notes.label(&name(*keyset, *denom));
    }
    for m in &r.notes.movements {
        let sign = if m.issued { 1 } else { -1 };
        let holder = if m.hub { HUB } else { HOLDERS };
        ecash.add(m.at as i64, holder, sign * m.denom as i64);
        notes.add(m.at as i64, &name(m.keyset, m.denom), sign);
    }
    // A lock's inputs are retired when it is admitted, its branch issued
    // when it settles: in between, its value is behind it. An own lock's
    // inputs were never notes.
    for lock in r.locks.iter().filter(|l| !l.own) {
        ecash.add(lock.created_at as i64, LOCKED, lock.amount as i64);
        if let Some(at) = lock.settled_at {
            ecash.add(at as i64, LOCKED, -(lock.amount as i64));
        }
    }
    vec![ecash, notes]
}

/// The labels of the notes outstanding.
const HOLDERS: &str = "holders";
const HUB: &str = "hub's own";
const LOCKED: &str = "behind locks";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_and_sets_read_as_an_operator_would_say_them() {
        assert_eq!(reason("FAILURE_REASON_NO_ROUTE"), "no route");
        assert_eq!(
            reason("FAILURE_REASON_INSUFFICIENT_BALANCE"),
            "insufficient balance"
        );
        assert_eq!(reason("invoice expired"), "invoice expired");
        assert_eq!(set_name(false, 0, 1_048_576), "1,048,576 sat");
        assert_eq!(set_name(true, 2, 64), "k2 64 sat");
        assert_eq!(invoice_sat("not an invoice"), None);
    }
}
