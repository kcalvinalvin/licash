//! The hub's inbox ([`licash_api::inbox`]): the payments it released that
//! their payers asked it to keep for their payees, for
//! [`inbox::KEPT_FOR`], in a SQLite file of its own. Nothing in it is
//! secret, and deleting it loses only the payments kept: their payers mail
//! them once no ack comes. Its sequence numbers start, at each opening,
//! above the time of day in microseconds, so that they only grow even when
//! the file is lost or an older copy restored.

use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{ensure, Context};
use licash::spend::ProposalId;
use licash_api::{
    inbox::{self, Entry, Feed, Receipt},
    Hex,
};
use rusqlite::{params, Connection, OptionalExtension};

/// The schema this inbox reads and writes, as SQLite's `user_version`.
const VERSION: i64 = 1;

const SCHEMA: &str = "
-- Each payment kept, by its proposal, as its payer asked: the tag its
-- payee finds it by, the key it takes it with, the commitment to its ack,
-- and the ack once told. The payment is a Receipt, in JSON.
CREATE TABLE kept (
    seq INTEGER PRIMARY KEY,
    proposal BLOB NOT NULL UNIQUE,
    ephemeral BLOB NOT NULL,
    tag BLOB NOT NULL,
    key BLOB NOT NULL,
    commitment BLOB NOT NULL,
    ack BLOB,
    payment TEXT NOT NULL,
    kept_at INTEGER NOT NULL
) STRICT;
CREATE INDEX kept_by_key ON kept (key);
CREATE INDEX kept_by_time ON kept (kept_at);
";

/// The inbox, in its file.
pub struct Inbox {
    conn: Mutex<Connection>,
    /// The least sequence number given from this opening on.
    floor: i64,
}

impl Inbox {
    /// The inbox at `path`, a new file or one a hub wrote.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // A payment answered kept is kept.
        conn.pragma_update(None, "synchronous", "FULL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            let tables: i64 =
                conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
            ensure!(tables == 0, "{} is not an inbox", path.display());
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.commit()?;
        } else {
            ensure!(
                version == VERSION,
                "inbox schema {version}, this hub reads {VERSION}"
            );
        }
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("the clock")?
            .as_micros();
        Ok(Self {
            conn: Mutex::new(conn),
            floor: i64::try_from(micros).context("the clock")?,
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keep the payment `receipt` of proposal `id`, of ephemeral key
    /// `ephemeral`, as `keep` asks: unless kept already, as first asked.
    /// Payments kept for longer than [`inbox::KEPT_FOR`] are forgotten.
    pub fn keep(
        &self,
        id: &ProposalId,
        ephemeral: &[u8; 33],
        keep: &inbox::Keep,
        receipt: &Receipt,
    ) -> anyhow::Result<()> {
        let (tag, key, commitment) = keep.decode()?;
        let payment = serde_json::to_string(receipt)?;
        let now = now();
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM kept WHERE kept_at < ?1",
            [now - inbox::KEPT_FOR],
        )?;
        let last: Option<i64> = tx.query_row("SELECT max(seq) FROM kept", [], |r| r.get(0))?;
        let seq = last.map_or(self.floor, |last| (last + 1).max(self.floor));
        tx.execute(
            "INSERT INTO kept (seq, proposal, ephemeral, tag, key, commitment, payment, kept_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT (proposal) DO NOTHING",
            params![
                seq,
                id.as_bytes(),
                ephemeral,
                tag,
                key,
                commitment,
                payment,
                now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The entries kept after `after`, at most [`inbox::MAX_ENTRIES`].
    pub fn feed(&self, after: u64) -> anyhow::Result<Feed> {
        let after = i64::try_from(after).unwrap_or(i64::MAX);
        let conn = self.conn();
        let mut query = conn
            .prepare("SELECT seq, ephemeral, tag FROM kept WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
        let limit = inbox::MAX_ENTRIES as i64 + 1;
        let rows = query.query_map(params![after, limit], |r| {
            Ok(Entry {
                seq: r.get::<_, i64>(0)? as u64,
                ephemeral: Hex(r.get(1)?),
                tag: Hex(r.get(2)?),
            })
        })?;
        let mut entries = rows.collect::<Result<Vec<_>, _>>()?;
        let more = entries.len() > inbox::MAX_ENTRIES;
        entries.truncate(inbox::MAX_ENTRIES);
        Ok(Feed { entries, more })
    }

    /// The payments of the entries `asked`, by sequence number, each if
    /// kept under its key.
    pub fn take(&self, asked: &[(u64, [u8; 32])]) -> anyhow::Result<Vec<Option<Receipt>>> {
        let conn = self.conn();
        let mut query = conn.prepare("SELECT payment FROM kept WHERE seq = ?1 AND key = ?2")?;
        asked
            .iter()
            .map(|(seq, key)| {
                let seq = i64::try_from(*seq).unwrap_or(i64::MAX);
                let payment: Option<String> = query
                    .query_row(params![seq, key], |r| r.get(0))
                    .optional()?;
                Ok(match payment {
                    Some(payment) => Some(serde_json::from_str(&payment)?),
                    None => None,
                })
            })
            .collect()
    }

    /// Take each of `acks`, a key and the ack of the payment kept under it:
    /// whether it is that payment's, which then tells its payer.
    pub fn acknowledge(&self, acks: &[([u8; 32], [u8; 32])]) -> anyhow::Result<Vec<bool>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut acked = Vec::with_capacity(acks.len());
        for (key, ack) in acks {
            let commitment = inbox::commitment(ack);
            let changed = tx.execute(
                "UPDATE kept SET ack = ?3 WHERE key = ?1 AND commitment = ?2",
                params![key, commitment, ack],
            )?;
            acked.push(changed > 0);
        }
        tx.commit()?;
        Ok(acked)
    }

    /// The ack told of the payment kept under each of `keys`, if any.
    pub fn acks(&self, keys: &[[u8; 32]]) -> anyhow::Result<Vec<Option<[u8; 32]>>> {
        let conn = self.conn();
        let mut query = conn.prepare(
            "SELECT ack FROM kept WHERE key = ?1 AND ack IS NOT NULL ORDER BY seq LIMIT 1",
        )?;
        keys.iter()
            .map(|key| {
                let ack: Option<[u8; 32]> = query.query_row([key], |r| r.get(0)).optional()?;
                Ok(ack)
            })
            .collect()
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(n: u8) -> Receipt {
        Receipt {
            release: licash_api::Release {
                keyset: Hex(vec![n; 32]),
                signed: Hex(vec![n; 8]),
                notes: Vec::new(),
            },
            unlock: Hex(vec![n; 32]),
        }
    }

    fn keep(n: u8) -> (inbox::Keep, [u8; 32]) {
        inbox::Keep::new(&[n; 32], &ProposalId::from_bytes([n; 32]))
    }

    #[test]
    fn a_payment_is_kept_once_taken_with_its_key_and_acknowledged_with_its_ack() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::open(&dir.path().join("inbox.sqlite")).unwrap();
        let (asked, ack) = keep(1);
        let id = ProposalId::from_bytes([1; 32]);
        inbox.keep(&id, &[2; 33], &asked, &receipt(1)).unwrap();
        // Asked again, as a payer retrying its release: kept once, as first.
        let (other, _) = keep(9);
        inbox.keep(&id, &[3; 33], &other, &receipt(9)).unwrap();
        let feed = inbox.feed(0).unwrap();
        assert_eq!(feed.entries.len(), 1);
        assert!(!feed.more);
        let entry = &feed.entries[0];
        assert_eq!(entry.decode().unwrap(), ([2; 33], inbox::tag(&[1; 32])));
        assert!(inbox.feed(entry.seq).unwrap().entries.is_empty());

        let key = inbox::key(&[1; 32]);
        let taken = inbox
            .take(&[(entry.seq, key), (entry.seq, [0; 32])])
            .unwrap();
        assert_eq!(taken[0].as_ref().unwrap().unlock, receipt(1).unlock);
        assert!(taken[1].is_none(), "not without its key");
        assert!(inbox.take(&[(entry.seq + 1, key)]).unwrap()[0].is_none());

        assert_eq!(inbox.acks(&[key]).unwrap(), [None]);
        let acked = inbox.acknowledge(&[(key, [7; 32]), (key, ack)]).unwrap();
        assert_eq!(acked, [false, true], "only the ack committed to");
        assert_eq!(inbox.acks(&[key, [0; 32]]).unwrap(), [Some(ack), None]);
    }

    #[test]
    fn its_numbers_only_grow_though_the_file_is_lost_or_restored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        let inbox = Inbox::open(&path).unwrap();
        for n in 1..=3 {
            let id = ProposalId::from_bytes([n; 32]);
            inbox.keep(&id, &[2; 33], &keep(n).0, &receipt(n)).unwrap();
        }
        let seen = inbox.feed(0).unwrap().entries;
        let last = seen.last().unwrap().seq;
        assert!(seen.windows(2).all(|w| w[0].seq < w[1].seq));
        drop(inbox);

        // Lost: a new file goes on above what payees saw.
        std::fs::remove_file(&path).unwrap();
        let inbox = Inbox::open(&path).unwrap();
        let id = ProposalId::from_bytes([4; 32]);
        inbox.keep(&id, &[2; 33], &keep(4).0, &receipt(4)).unwrap();
        let after = inbox.feed(last).unwrap().entries;
        assert_eq!(after.len(), 1);
        assert!(after[0].seq > last);
    }

    #[test]
    fn the_feed_comes_in_pages() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::open(&dir.path().join("inbox.sqlite")).unwrap();
        {
            // In one transaction, rather than a sync each.
            let mut conn = inbox.conn();
            let tx = conn.transaction().unwrap();
            for n in 1..=inbox::MAX_ENTRIES as i64 + 1 {
                tx.execute(
                    "INSERT INTO kept (seq, proposal, ephemeral, tag, key, commitment, payment,
                        kept_at) VALUES (?1, ?2, x'02', x'00', x'00', x'00', '{}', ?3)",
                    params![n, n.to_be_bytes(), now()],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let first = inbox.feed(0).unwrap();
        assert_eq!(first.entries.len(), inbox::MAX_ENTRIES);
        assert!(first.more);
        let rest = inbox.feed(first.entries.last().unwrap().seq).unwrap();
        assert_eq!(rest.entries.len(), 1);
        assert!(!rest.more);
    }

    #[test]
    fn a_file_of_another_kind_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t (x INTEGER)")
            .unwrap();
        assert!(Inbox::open(&path).is_err());
    }
}
