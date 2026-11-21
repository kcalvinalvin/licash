//! What the wallet's records say of when things happened, for statistics:
//! read only, and nothing here is written or migrated. An operation's
//! start is in its own record ([`Wallet::sends`] and the like); its end,
//! and what its notes and coins did, are here.

use std::collections::{HashMap, HashSet};

use bitcoin::{hashes::Hash, Txid};

use crate::{Error, Transport, Wallet};

/// When the wallet's operations ended, and its notes and coins came and went.
#[derive(Clone, Debug, Default)]
pub struct Journal {
    /// When each operation that succeeded ended, in seconds since 1970, by
    /// its id in hex (a send's, a lock's, an invoice's, a deposit's, a swap's
    /// or a payout's): a lock when its hub settled it. One that failed, or
    /// is under way, has none.
    pub ended: HashMap<String, i64>,
    /// The sends that renewed notes, by id in hex.
    pub renewals: HashSet<String>,
    /// Every note the wallet ever held at a hub it trusts.
    pub notes: Vec<NoteSpan>,
    /// Every coin the wallet ever found at its silent payment address.
    pub coins: Vec<CoinSpan>,
}

/// A note held from `added_at` until the operation that spent it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteSpan {
    pub hub: String,
    pub amount: u64,
    pub added_at: i64,
    /// When the operation spending it ended: none while it is held, or if
    /// a contract's recovery claimed it (which records no time).
    pub spent_at: Option<i64>,
    /// When its hub showed a copy of the wallet spent it, if it did.
    pub gone_at: Option<i64>,
    /// When it finished unlinking, if it did.
    pub unlinked_at: Option<i64>,
}

/// A coin found at the silent payment address, spent or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoinSpan {
    pub txid: Txid,
    pub value: u64,
    /// The block it is in.
    pub height: u32,
    /// At the wallet's change address: one of its own spends paid it.
    pub change: bool,
    /// The transaction that spends it, the wallet's own or seen.
    pub spent_by: Option<Txid>,
    /// The block of the transaction that spent it, once in one.
    pub spent_height: Option<u32>,
}

impl<T: Transport> Wallet<T> {
    /// The journal, read in one pass.
    pub fn journal(&self) -> Result<Journal, Error> {
        self.store.read(|c| {
            let mut journal = Journal::default();
            let ends = [
                "SELECT id, done_at FROM sends WHERE done_at IS NOT NULL",
                "SELECT id, settled_at FROM lock_settlements",
                "SELECT hash, done_at FROM invoices WHERE done_at IS NOT NULL",
                "SELECT hash, done_at FROM deposits WHERE done_at IS NOT NULL",
                "SELECT hash, done_at FROM swaps WHERE done_at IS NOT NULL",
                "SELECT hash, done_at FROM payouts WHERE done_at IS NOT NULL",
            ];
            for sql in ends {
                let mut stmt = c.prepare(sql)?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, [u8; 32]>(0)?, r.get::<_, i64>(1)?)))?;
                for row in rows {
                    let (id, at) = row?;
                    journal.ended.insert(hex::encode(id), at);
                }
            }
            let mut stmt = c.prepare("SELECT id FROM sends WHERE renewal = 1")?;
            for id in stmt.query_map([], |r| r.get::<_, [u8; 32]>(0))? {
                journal.renewals.insert(hex::encode(id?));
            }
            // A note is spent when what spends it ended.
            let mut stmt = c.prepare(
                "SELECT r.url, n.denom, n.added_at,
                    COALESCE(s.done_at, l.settled_at), n.gone_at, n.unlinked_at
                 FROM notes n
                 JOIN keysets k ON k.id = n.keyset
                 JOIN hubs h ON h.id = k.hub AND h.trusted = 1
                 JOIN reach r ON r.hub = h.id
                 LEFT JOIN sends s ON s.id = n.spent_by AND n.spent_kind = 'send'
                 LEFT JOIN lock_settlements l ON l.id = n.spent_by AND n.spent_kind = 'lock'
                 ORDER BY n.added_at, n.y",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(NoteSpan {
                    hub: r.get(0)?,
                    amount: r.get::<_, i64>(1)? as u64,
                    added_at: r.get(2)?,
                    spent_at: r.get(3)?,
                    gone_at: r.get(4)?,
                    unlinked_at: r.get(5)?,
                })
            })?;
            for row in rows {
                journal.notes.push(row?);
            }
            let mut stmt = c.prepare(
                "SELECT txid, value, height, label, spent_by, spent_height FROM silent_coins
                 ORDER BY height, txid, vout",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(CoinSpan {
                    txid: Txid::from_byte_array(r.get::<_, [u8; 32]>(0)?),
                    value: r.get::<_, i64>(1)? as u64,
                    height: r.get(2)?,
                    change: r.get::<_, Option<u32>>(3)? == Some(licash_onchain::silent::CHANGE),
                    spent_by: r.get::<_, Option<[u8; 32]>>(4)?.map(Txid::from_byte_array),
                    spent_height: r.get(5)?,
                })
            })?;
            for row in rows {
                journal.coins.push(row?);
            }
            Ok(journal)
        })
    }
}
