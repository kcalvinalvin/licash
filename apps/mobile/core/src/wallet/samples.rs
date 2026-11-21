//! What the wallet held, written down as it changed, for Stats to say how
//! much of it was protected over time: coverage is worked out live from
//! contracts and the chain's height, so the past can only be known if it was
//! kept. Advisory data, nothing a payment or a restore depends on.
//!
//! The component `stats` of the wallet's file, made by the first sample and
//! never by opening, reading or restoring a wallet: absent, it says nothing
//! was recorded, and nothing is guessed or filled in for the time before. A
//! wallet of an earlier version has none until its first sample. A version
//! of the component this build does not read, or half of one, is an error
//! (never repaired or dropped) that nothing else depends on.
//!
//! One sample is one `Balance`. A changed balance writes one (over the last,
//! if that is under a minute old), an unchanged one only once a day, so an
//! idle wallet adds no more than the backup that is already made daily. Its
//! table is in backups, and no reason for one: [`TABLES`].

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _, Result};
use licash_store::Kind;
use rusqlite::{params, Connection, OptionalExtension as _};

use super::{backup::failed, Wallet};
use crate::{Balance, LicashError};

const COMPONENT: &str = "stats";
const VERSION: i64 = 1;

/// No foreign keys, indexes or triggers: nothing an earlier build writing the
/// wallet's tables could ever trip over.
const SCHEMA: &str = "
-- What the wallet held at `at`, seconds since 1970: a `Balance`, as of its
-- last change or the day's first look.
CREATE TABLE balance_samples (
    at INTEGER PRIMARY KEY,
    notes INTEGER NOT NULL,
    notes_pending INTEGER NOT NULL,
    onchain INTEGER NOT NULL,
    moving INTEGER NOT NULL,
    kept INTEGER NOT NULL,
    opening INTEGER NOT NULL,
    covered INTEGER NOT NULL
) STRICT;
";

/// The tables of this component: kept in a backup, but their rows alone are
/// no reason for one.
pub(super) const TABLES: [&str; 1] = ["balance_samples"];

/// An unchanged balance is written again once this old: proof the wallet was
/// looked at.
const HEARTBEAT: i64 = 24 * 60 * 60;

/// A change within this of the last sample replaces it: a payment and its
/// settling are one.
const COALESCE: i64 = 60;

/// What the wallet held at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BalanceSample {
    /// Seconds since 1970.
    pub at: i64,
    pub balance: Balance,
}

/// What a call did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Wrote {
    Nothing,
    Added,
    Replaced,
}

/// Balance samples.
#[uniffi::export]
impl Wallet {
    /// Write down `balance` as the wallet's now, if it changed or it is a
    /// day since the last: as the app last read it, no more asked of the
    /// wallet. Never fails a payment or a read: say so, and go on.
    pub fn record_balance(&self, balance: Balance) -> Result<(), LicashError> {
        record(self.wallet.path(), &balance, now())
            .map(|_| ())
            .map_err(failed)
    }

    /// What was written down, oldest first: none if nothing ever was (the
    /// wallet is of an earlier version, or was restored from a backup of one,
    /// and has not been looked at since).
    pub fn balance_samples(&self) -> Result<Option<Vec<BalanceSample>>, LicashError> {
        read(self.wallet.path()).map_err(failed)
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The values of `balance`, in the order of the table's columns.
fn values(b: &Balance) -> Result<[i64; 7]> {
    let one = |v: u64| i64::try_from(v).context("a balance beyond what is stored");
    Ok([
        one(b.notes)?,
        one(b.notes_pending)?,
        one(b.onchain)?,
        one(b.moving)?,
        one(b.kept)?,
        one(b.opening)?,
        one(b.covered)?,
    ])
}

const COLUMNS: &str = "notes, notes_pending, onchain, moving, kept, opening, covered";

fn last(c: &Connection) -> rusqlite::Result<Option<(i64, [i64; 7])>> {
    c.query_row(
        &format!("SELECT at, {COLUMNS} FROM balance_samples ORDER BY at DESC LIMIT 1"),
        [],
        |r| {
            let mut v = [0; 7];
            for (i, slot) in v.iter_mut().enumerate() {
                *slot = r.get(i + 1)?;
            }
            Ok((r.get(0)?, v))
        },
    )
    .optional()
}

/// The component of the file at `path`, as made: none if there is none, an
/// error if it is of another version.
fn present(c: &Connection) -> Result<bool> {
    match licash_store::component_version(c, COMPONENT)? {
        None => Ok(false),
        Some(VERSION) => Ok(true),
        Some(v) => bail!("statistics of version {v}, this build reads {VERSION}"),
    }
}

/// Write `balance` as of `at` if the rules say so ([`Wrote`]).
pub(super) fn record(path: &Path, balance: &Balance, at: i64) -> Result<Wrote> {
    let new = values(balance)?;
    // An unchanged balance asks nothing of the file: no write is opened.
    if let Some(c) = licash_store::inspect(path, Kind::Wallet)? {
        if present(&c)? {
            if let Some((was, old)) = last(&c)? {
                if at >= was && old == new && at - was < HEARTBEAT {
                    return Ok(Wrote::Nothing);
                }
            }
        }
    }
    let mut c = licash_store::open(path, Kind::Wallet)?;
    // Made here only, in one transaction with its version: an interruption
    // leaves none of it.
    licash_store::component(&mut c, COMPONENT, VERSION, SCHEMA)?;
    licash_store::write(&mut c, |tx| -> Result<Wrote> {
        let add = |at: i64| -> rusqlite::Result<usize> {
            tx.execute(
                &format!(
                    "INSERT INTO balance_samples (at, {COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                ),
                params![at, new[0], new[1], new[2], new[3], new[4], new[5], new[6]],
            )
        };
        let Some((was, old)) = last(tx)? else {
            add(at)?;
            return Ok(Wrote::Added);
        };
        // A clock set back: nothing is written until real time passes the
        // last sample, not a row out of order.
        if at < was {
            tracing::warn!(at, was, "a balance sample from before the last");
            return Ok(Wrote::Nothing);
        }
        if old == new {
            if at - was < HEARTBEAT {
                return Ok(Wrote::Nothing);
            }
            add(at)?;
            return Ok(Wrote::Added);
        }
        if at - was < COALESCE {
            tx.execute(
                "UPDATE balance_samples SET (notes, notes_pending, onchain, moving, kept, \
                 opening, covered) = (?2, ?3, ?4, ?5, ?6, ?7, ?8) WHERE at = ?1",
                params![was, new[0], new[1], new[2], new[3], new[4], new[5], new[6]],
            )?;
            return Ok(Wrote::Replaced);
        }
        add(at)?;
        Ok(Wrote::Added)
    })
}

/// The samples of the file at `path`, oldest first, none if there are none.
/// Reads, and makes nothing.
pub(super) fn read(path: &Path) -> Result<Option<Vec<BalanceSample>>> {
    let Some(c) = licash_store::inspect(path, Kind::Wallet)? else {
        return Ok(None);
    };
    if !present(&c)? {
        return Ok(None);
    }
    let mut stmt = c.prepare(&format!(
        "SELECT at, {COLUMNS} FROM balance_samples ORDER BY at"
    ))?;
    let rows = stmt.query_map([], |r| {
        let n = |i: usize| r.get::<_, i64>(i).map(|v| v.max(0) as u64);
        Ok(BalanceSample {
            at: r.get(0)?,
            balance: Balance {
                notes: n(1)?,
                notes_pending: n(2)?,
                onchain: n(3)?,
                moving: n(4)?,
                kept: n(5)?,
                opening: n(6)?,
                covered: n(7)?,
            },
        })
    })?;
    Ok(Some(rows.collect::<rusqlite::Result<_>>()?))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;
    use crate::Network;

    fn file(dir: &Path) -> PathBuf {
        let wallet = Wallet::create(dir.display().to_string(), Network::Regtest, 42, None).unwrap();
        drop(wallet);
        dir.join("wallet.sqlite")
    }

    fn held(notes: u64, covered: u64) -> Balance {
        Balance {
            notes,
            covered,
            ..Balance::default()
        }
    }

    fn rows(path: &Path) -> Vec<(i64, u64, u64)> {
        read(path)
            .unwrap()
            .unwrap_or_default()
            .iter()
            .map(|s| (s.at, s.balance.notes, s.balance.covered))
            .collect()
    }

    fn stamp(path: &Path) -> (u64, std::time::SystemTime) {
        let m = fs::metadata(path).unwrap();
        (m.len(), m.modified().unwrap())
    }

    #[test]
    fn nothing_is_made_by_opening_or_reading_a_wallet_and_a_first_sample_makes_the_component() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        let has = |path: &Path| {
            let c = licash_store::inspect(path, Kind::Wallet).unwrap().unwrap();
            licash_store::component_version(&c, COMPONENT).unwrap()
        };
        assert_eq!(has(&path), None);
        assert_eq!(read(&path).unwrap(), None);
        assert_eq!(has(&path), None, "reading made none");
        assert_eq!(record(&path, &held(10, 5), 1_000).unwrap(), Wrote::Added);
        assert_eq!(has(&path), Some(VERSION));
        assert_eq!(rows(&path), vec![(1_000, 10, 5)]);
    }

    #[test]
    fn a_change_is_written_an_unchanged_balance_once_a_day_and_a_quick_change_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        record(&path, &held(10, 5), 1_000).unwrap();
        // Unchanged: nothing, and the file is not touched at all.
        let before = stamp(&path);
        assert_eq!(
            record(&path, &held(10, 5), 1_000 + 3_600).unwrap(),
            Wrote::Nothing
        );
        assert_eq!(
            record(&path, &held(10, 5), 1_000 + HEARTBEAT - 1).unwrap(),
            Wrote::Nothing
        );
        assert_eq!(stamp(&path), before);
        // A day on, unchanged: one more.
        assert_eq!(
            record(&path, &held(10, 5), 1_000 + HEARTBEAT).unwrap(),
            Wrote::Added
        );
        // A change: one more; within a minute of it, over it.
        let at = 1_000 + HEARTBEAT + 100;
        assert_eq!(record(&path, &held(7, 5), at).unwrap(), Wrote::Added);
        assert_eq!(
            record(&path, &held(7, 0), at + COALESCE - 1).unwrap(),
            Wrote::Replaced
        );
        assert_eq!(
            record(&path, &held(7, 0), at + COALESCE).unwrap(),
            Wrote::Nothing
        );
        assert_eq!(
            rows(&path),
            vec![(1_000, 10, 5), (1_000 + HEARTBEAT, 10, 5), (at, 7, 0)]
        );
    }

    #[test]
    fn a_clock_set_back_writes_nothing_until_time_passes_the_last_sample() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        record(&path, &held(10, 5), 5_000).unwrap();
        assert_eq!(record(&path, &held(99, 5), 4_000).unwrap(), Wrote::Nothing);
        assert_eq!(rows(&path), vec![(5_000, 10, 5)]);
        assert_eq!(
            record(&path, &held(99, 5), 5_000 + COALESCE).unwrap(),
            Wrote::Added
        );
    }

    #[test]
    fn two_callers_at_once_leave_one_sample() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || record(&path, &held(10, 5), 1_000).unwrap())
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        assert_eq!(rows(&path), vec![(1_000, 10, 5)]);
    }

    #[test]
    fn a_component_of_another_version_or_half_made_is_an_error_and_the_wallet_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let wallet =
            Wallet::create(dir.path().display().to_string(), Network::Regtest, 42, None).unwrap();
        let path = dir.path().join("wallet.sqlite");
        record(&path, &held(10, 5), 1_000).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute(
            "UPDATE schema_versions SET version = 99 WHERE component = 'stats'",
            [],
        )
        .unwrap();
        // What does not depend on it goes on, and it says why it cannot.
        assert!(wallet.balance().is_ok());
        assert!(wallet.history().is_ok());
        assert!(wallet.record_balance(held(1, 1)).is_err());
        assert!(wallet.balance_samples().is_err());
        // A version row with no table: an error, nothing made or dropped.
        db.execute(
            "UPDATE schema_versions SET version = 1 WHERE component = 'stats'",
            [],
        )
        .unwrap();
        db.execute("DROP TABLE balance_samples", []).unwrap();
        assert!(record(&path, &held(2, 2), 9_000).is_err());
        assert!(read(&path).is_err());
        // A table with no version row: the same.
        db.execute("DELETE FROM schema_versions WHERE component = 'stats'", [])
            .unwrap();
        db.execute_batch(SCHEMA).unwrap();
        assert!(record(&path, &held(2, 2), 9_000).is_err());
        assert!(wallet.balance().is_ok());
    }

    #[test]
    fn a_balance_beyond_what_is_stored_is_refused_not_cut() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        assert!(record(&path, &held(u64::MAX, 0), 1).is_err());
        assert_eq!(read(&path).unwrap(), None);
    }
}
