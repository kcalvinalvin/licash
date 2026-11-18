//! The hub's deposits on Bitcoin: its notes for coins.
//!
//! A wallet asks for a deposit as for a receipt over Lightning, notes to
//! lock to it on the preimage of a hash only it knows, and names a key to
//! take its coins back with. The hub answers the address of a taproot
//! output ([`Htlc`]) that it claims with the preimage and a key of its own
//! for the deposit, and that the payer takes back after a timeout. Once a
//! payment of exactly the amount asked confirms at depth in time, the hub
//! locks its own notes to the wallet, early enough to claim the coins
//! before the timeout with the preimage the wallet reveals to claim the
//! notes. A lock refunded unclaimed leaves the coins to their payer.
//!
//! The deposits, and the blocks the gateway scanned for their payments and
//! claims, are kept in its own SQLite file.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::Hash,
    key::Secp256k1,
    secp256k1::{SecretKey, XOnlyPublicKey},
    Amount, Block, BlockHash, FeeRate, OutPoint, ScriptBuf, Transaction, Txid,
};
use licash::spend::{ChainProfile, ProposalId, TimedTerms};
use licash_api::{
    Code, Deposit, DepositStatus, Depositing, Deposits, Fee, Funding, Hex, Problem, Receive,
    ReceiveState, ReceiveStatus,
};
use licash_chain_client::{Follow, Source};
use licash_hub::{Error, Hub, Refusal, Settled, Timing};
use licash_onchain::{
    deadline,
    deposit::{self, Htlc},
};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{
    peer::{self, Peer, Recorded},
    Failure,
};

/// What deposits cost, the amounts taken and their timing, in satoshis
/// and blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// On top of the notes a deposit locks.
    pub fee: Fee,
    /// The notes a deposit locks: at least and at most.
    pub min: u64,
    pub max: u64,
    /// Confirmations a payment needs.
    pub depth: u32,
    /// Blocks from the request within which its payment must confirm.
    pub window: u32,
    /// Blocks a deposit's lock lasts.
    pub lock_blocks: u32,
    /// Blocks the gateway may be down while a deposit is locked.
    pub downtime: u32,
    /// Blocks the hub's claim has to confirm in before the timeout.
    pub sweep_blocks: u32,
    /// The fee rate of claims, at least, and when the chain estimates none.
    pub min_fee_rate: FeeRate,
    /// Where claims pay.
    pub sweep_to: ScriptBuf,
    /// Between rounds of scanning and following deposits.
    pub poll: Duration,
    /// Deposits asked and not yet paid at once, from which new ones are
    /// refused as unavailable: each is scanned for in every block of its
    /// window, at no cost to whoever asked.
    pub max_open: u32,
}

impl Config {
    /// Deposits that claims pay `sweep_to`, otherwise as by default.
    pub fn new(sweep_to: ScriptBuf) -> Self {
        Self {
            fee: Fee {
                base: 1_000,
                ppm: 0,
            },
            min: 10_000,
            max: 5_000_000,
            depth: 3,
            window: 144,
            lock_blocks: 12,
            downtime: 6,
            sweep_blocks: 36,
            min_fee_rate: FeeRate::from_sat_per_vb_u32(2),
            sweep_to,
            poll: Duration::from_secs(5),
            max_open: 1024,
        }
    }

    /// Whether the hub, of `timing`, admits the locks of deposits.
    pub fn check(&self, timing: &Timing) -> Result<(), String> {
        let lock = [self.lock_blocks, timing.min_depth, timing.reaction]
            .iter()
            .try_fold(0u32, |sum, b| sum.checked_add(*b));
        if self.lock_blocks == 0 || lock.is_none_or(|l| l > timing.window) {
            return Err(format!(
                "deposit lock blocks must be positive and, with the depth and reaction of the \
                 hub's locks, within their window of {}",
                timing.window
            ));
        }
        if self.depth == 0 || self.window == 0 || self.sweep_blocks == 0 {
            return Err("the deposit depth, window and sweep blocks must be positive".into());
        }
        if self.min == 0 || self.min > self.max {
            return Err("the least deposit must be positive and at most the largest".into());
        }
        if self.max_open == 0 {
            return Err("the limit of deposits asked must be positive".into());
        }
        if self.timeout_blocks(timing.min_depth).is_none() {
            return Err("the deposit timing overflows".into());
        }
        Ok(())
    }

    /// Blocks from a request to its timeout: its window and depth, then a
    /// lock's life and depth, the gateway's downtime and a claim's time to
    /// confirm.
    fn timeout_blocks(&self, lock_depth: u32) -> Option<u32> {
        [
            self.depth,
            self.lock_blocks,
            lock_depth,
            self.downtime,
            self.sweep_blocks,
        ]
        .iter()
        .try_fold(self.window, |sum, b| sum.checked_add(*b))
    }

    /// Whether a lock expiring at `expiry` at `lock_depth` leaves the hub
    /// time to claim the coins before `timeout`, down a while.
    fn claims_in_time(&self, expiry: u32, lock_depth: u32, timeout: u32) -> bool {
        [lock_depth, self.downtime, self.sweep_blocks]
            .iter()
            .try_fold(expiry, |at, b| at.checked_add(*b))
            .is_some_and(|latest| latest <= timeout)
    }
}

const VERSION: i64 = 1;

const SCHEMA: &str = "
-- Deposits asked for, recorded before their address is answered: paid
-- once a payment confirms, locked, then settled and claimed, or canceled.
-- Live until done.
CREATE TABLE deposits (
    hash BLOB PRIMARY KEY,
    -- The deposit asked for, as JSON.
    body TEXT NOT NULL,
    amount INTEGER NOT NULL,
    pays INTEGER NOT NULL,
    claim BLOB NOT NULL,
    refund BLOB NOT NULL,
    timeout INTEGER NOT NULL,
    fund_by INTEGER NOT NULL,
    script BLOB NOT NULL,
    -- Its payment, once confirmed.
    txid BLOB,
    vout INTEGER,
    height INTEGER,
    lock BLOB,
    preimage BLOB,
    -- The hub's claim, once made, and the spend of the payment, once
    -- confirmed.
    sweep BLOB,
    sweep_txid BLOB,
    spent INTEGER,
    spent_by BLOB,
    state TEXT NOT NULL CHECK (state IN ('open', 'locked', 'settled', 'canceled')),
    created_at INTEGER NOT NULL,
    done_at INTEGER
) STRICT;
CREATE INDEX live_deposits ON deposits (state) WHERE done_at IS NULL;
-- The blocks of the best chain scanned, of late.
CREATE TABLE blocks (
    height INTEGER PRIMARY KEY,
    hash BLOB NOT NULL
) STRICT;
";

/// Scanned blocks kept below the newest, to find where a reorganization
/// forked.
const KEPT_BLOCKS: u32 = 1_000;

/// The gateway's deposits, in a SQLite file of their own.
struct Store(Mutex<Connection>);

/// A deposit as recorded.
#[derive(Clone, Debug)]
struct Record {
    hash: [u8; 32],
    body: String,
    amount: u64,
    pays: u64,
    htlc: Htlc,
    fund_by: u32,
    funding: Option<(OutPoint, u32)>,
    lock: Option<ProposalId>,
    preimage: Option<[u8; 32]>,
    sweep: Option<Vec<u8>>,
    spent_by: Option<Txid>,
    state: ReceiveState,
}

const RECORDS: &str = "SELECT hash, body, amount, pays, claim, refund, timeout, fund_by, txid,
    vout, height, lock, preimage, sweep, spent_by, state FROM deposits";

/// The key in column `i` of `r`.
fn x_only(r: &Row<'_>, i: usize) -> rusqlite::Result<XOnlyPublicKey> {
    let bytes: [u8; 32] = r.get(i)?;
    XOnlyPublicKey::from_slice(&bytes).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Blob, e.into())
    })
}

fn record(r: &Row<'_>) -> rusqlite::Result<Record> {
    let key = |i| x_only(r, i);
    let hash: [u8; 32] = r.get(0)?;
    let funding = match (r.get::<_, Option<[u8; 32]>>(8)?, r.get(9)?, r.get(10)?) {
        (Some(txid), Some(vout), Some(height)) => Some((
            OutPoint {
                txid: Txid::from_byte_array(txid),
                vout,
            },
            height,
        )),
        _ => None,
    };
    let state = match r.get::<_, String>(15)?.as_str() {
        "open" => ReceiveState::Open,
        "locked" => ReceiveState::Locked,
        "settled" => ReceiveState::Settled,
        _ => ReceiveState::Canceled,
    };
    Ok(Record {
        hash,
        body: r.get(1)?,
        amount: r.get::<_, i64>(2)? as u64,
        pays: r.get::<_, i64>(3)? as u64,
        htlc: Htlc {
            hash,
            claim: key(4)?,
            refund: key(5)?,
            timeout: r.get(6)?,
        },
        fund_by: r.get(7)?,
        funding,
        lock: r
            .get::<_, Option<[u8; 32]>>(11)?
            .map(ProposalId::from_bytes),
        preimage: r.get(12)?,
        sweep: r.get(13)?,
        spent_by: r.get::<_, Option<[u8; 32]>>(14)?.map(Txid::from_byte_array),
        state,
    })
}

fn sql(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl Store {
    /// The deposits at `path`, a new file or one this gateway wrote.
    fn open(path: &Path) -> anyhow::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            let tables: i64 =
                conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
            anyhow::ensure!(tables == 0, "{} is not the hub's deposits", path.display());
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.commit()?;
        } else {
            anyhow::ensure!(
                version == VERSION,
                "deposits schema {version}, this hub reads {VERSION}"
            );
        }
        Ok(Self(Mutex::new(conn)))
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, hash: &[u8; 32]) -> rusqlite::Result<Option<Record>> {
        let conn = self.conn();
        conn.query_row(&format!("{RECORDS} WHERE hash = ?1"), [hash], record)
            .optional()
    }

    /// The deposits not done, oldest first.
    fn live(&self) -> rusqlite::Result<Vec<Record>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "{RECORDS} WHERE done_at IS NULL ORDER BY created_at, rowid"
        ))?;
        let live = stmt.query_map([], record)?.collect();
        live
    }

    /// Record a deposit unless one under its hash is, or `most` are
    /// asked and not paid: false then.
    fn insert(
        &self,
        body: &str,
        amount: u64,
        pays: u64,
        htlc: &Htlc,
        fund_by: u32,
        most: u32,
    ) -> rusqlite::Result<bool> {
        let conn = self.conn();
        let inserted = conn.execute(
            "INSERT INTO deposits (hash, body, amount, pays, claim, refund, timeout, fund_by,
                 script, state, created_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'open', ?10
             WHERE (SELECT count(*) FROM deposits WHERE state = 'open' AND done_at IS NULL) < ?11
             ON CONFLICT DO NOTHING",
            params![
                htlc.hash,
                body,
                sql(amount),
                sql(pays),
                htlc.claim.serialize(),
                htlc.refund.serialize(),
                htlc.timeout,
                fund_by,
                htlc.script_pubkey().as_bytes(),
                sql(now()),
                most,
            ],
        )?;
        Ok(inserted == 1)
    }

    fn set(
        &self,
        hash: &[u8; 32],
        column: &str,
        value: &dyn rusqlite::ToSql,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            &format!("UPDATE deposits SET {column} = ?2 WHERE hash = ?1"),
            params![hash, value],
        )?;
        Ok(())
    }

    /// Move a deposit to `state`, done unless it is to be claimed.
    fn finish(&self, hash: &[u8; 32], state: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE deposits SET state = ?2,
                 done_at = CASE WHEN ?2 IN ('locked', 'settled') THEN NULL ELSE ?3 END
             WHERE hash = ?1",
            params![hash, state, sql(now())],
        )?;
        Ok(())
    }

    /// Done with a deposit as it stands.
    fn done(&self, hash: &[u8; 32]) -> rusqlite::Result<()> {
        self.set(hash, "done_at", &sql(now()))
    }

    /// The newest block scanned.
    fn top(&self) -> rusqlite::Result<Option<(u32, BlockHash)>> {
        let conn = self.conn();
        let top = conn
            .query_row(
                "SELECT height, hash FROM blocks ORDER BY height DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get::<_, [u8; 32]>(1)?)),
            )
            .optional()?;
        Ok(top.map(|(height, hash)| (height, BlockHash::from_byte_array(hash))))
    }

    fn hash_at(&self, height: u32) -> rusqlite::Result<Option<BlockHash>> {
        let conn = self.conn();
        let hash = conn
            .query_row("SELECT hash FROM blocks WHERE height = ?1", [height], |r| {
                r.get::<_, [u8; 32]>(0)
            })
            .optional()?;
        Ok(hash.map(BlockHash::from_byte_array))
    }
}

impl Store {
    /// Record `block`, at `height` on the best chain, with the payments of
    /// deposits in it and the spends of those paid.
    fn scanned(&self, height: u32, block: &Block) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut unpaid: HashMap<Vec<u8>, ([u8; 32], u64, u32)> = tx
            .prepare(
                "SELECT script, hash, pays, fund_by FROM deposits
                 WHERE done_at IS NULL AND txid IS NULL",
            )?
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    (r.get(1)?, r.get::<_, i64>(2)? as u64, r.get(3)?),
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut paid: HashMap<OutPoint, Htlc> = tx
            .prepare(
                "SELECT txid, vout, hash, claim, refund, timeout FROM deposits
                 WHERE done_at IS NULL AND txid IS NOT NULL AND spent IS NULL",
            )?
            .query_map([], |r| {
                let txid = Txid::from_byte_array(r.get(0)?);
                Ok((
                    OutPoint {
                        txid,
                        vout: r.get(1)?,
                    },
                    Htlc {
                        hash: r.get(2)?,
                        claim: x_only(r, 3)?,
                        refund: x_only(r, 4)?,
                        timeout: r.get(5)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for t in &block.txdata {
            let txid = t.compute_txid();
            for input in &t.input {
                let Some(htlc) = paid.remove(&input.previous_output) else {
                    continue;
                };
                let hash = htlc.hash;
                // Claimed by the hub, by whichever of its claims, done;
                // else the payer took it back.
                let ours = htlc.claimed_by(t, input.previous_output);
                tx.execute(
                    "UPDATE deposits SET spent = ?2, spent_by = ?3,
                         done_at = CASE WHEN ?4 THEN ?5 ELSE done_at END
                     WHERE hash = ?1",
                    params![hash, height, txid.to_byte_array(), ours, sql(now())],
                )?;
                let hash = hex::encode(hash);
                tracing::info!(%hash, %txid, height, ours, "a deposit's payment spent");
            }
            for (vout, output) in (0u32..).zip(&t.output) {
                let script = output.script_pubkey.as_bytes();
                let Some(&(hash, pays, fund_by)) = unpaid.get(script) else {
                    continue;
                };
                let name = hex::encode(hash);
                if output.value.to_sat() != pays || height > fund_by {
                    let value = output.value.to_sat();
                    tracing::warn!(hash = %name, %txid, value, pays, height, fund_by, "a payment to a deposit not as asked: its payer takes it back");
                    continue;
                }
                unpaid.remove(script);
                tx.execute(
                    "UPDATE deposits SET txid = ?2, vout = ?3, height = ?4 WHERE hash = ?1",
                    params![hash, txid.to_byte_array(), vout, height],
                )?;
                tracing::info!(hash = %name, %txid, vout, height, "deposit paid");
            }
        }
        tx.execute(
            "INSERT INTO blocks (height, hash) VALUES (?1, ?2)",
            params![height, block.block_hash().to_byte_array()],
        )?;
        tx.execute(
            "DELETE FROM blocks WHERE height < ?1",
            [height.saturating_sub(KEPT_BLOCKS)],
        )?;
        tx.commit()
    }

    /// The oldest block kept.
    fn oldest(&self) -> rusqlite::Result<Option<u32>> {
        self.conn()
            .query_row("SELECT MIN(height) FROM blocks", [], |r| r.get(0))
    }

    /// Forget the blocks from `height` up, which left the best chain, and
    /// what was found in them: the payments of deposits not locked yet,
    /// and spends. Those locked keep theirs: the hub's notes are out.
    /// `below`, the best chain's block before `height`, is kept as scanned
    /// should none older be: the scan goes on from `height`.
    fn rewind(&self, height: u32, below: Option<BlockHash>) -> rusqlite::Result<Vec<[u8; 32]>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM blocks WHERE height >= ?1", [height])?;
        if let (Some(hash), Some(at)) = (below, height.checked_sub(1)) {
            tx.execute(
                "INSERT INTO blocks (height, hash) VALUES (?1, ?2)
                 ON CONFLICT (height) DO UPDATE SET hash = excluded.hash",
                params![at, hash.to_byte_array()],
            )?;
        }
        tx.execute(
            "UPDATE deposits SET txid = NULL, vout = NULL, height = NULL
             WHERE height >= ?1 AND state = 'open' AND done_at IS NULL",
            [height],
        )?;
        tx.execute(
            "UPDATE deposits SET spent = NULL, spent_by = NULL,
                 done_at = CASE WHEN state = 'settled' THEN NULL ELSE done_at END
             WHERE spent >= ?1",
            [height],
        )?;
        let unbacked = tx
            .prepare(
                "SELECT hash FROM deposits
                 WHERE height >= ?1 AND state IN ('locked', 'settled') AND spent IS NULL",
            )?
            .query_map([height], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        tx.commit()?;
        Ok(unbacked)
    }
}

/// The current Unix time, in seconds.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn store_down(e: rusqlite::Error) -> Failure {
    tracing::error!(error = %e, "the deposits cannot be read or written");
    Failure(Problem::new(Code::Unavailable, "the hub cannot serve now"))
}

fn store_gone() -> Failure {
    tracing::error!("a deposit does not read as written");
    Failure(Problem::new(Code::Unavailable, "the hub cannot serve now"))
}

fn refused(detail: &str) -> Failure {
    Failure(Problem::new(Code::Forward, detail))
}

impl Store {
    /// Record the hub's claim of a deposit, before it is broadcast.
    fn set_sweep(&self, hash: &[u8; 32], tx: &Transaction) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE deposits SET sweep = ?2, sweep_txid = ?3 WHERE hash = ?1",
            params![hash, serialize(tx), tx.compute_txid().to_byte_array()],
        )?;
        Ok(())
    }
}

/// The hub's deposits on Bitcoin.
pub struct Gateway {
    hub: Arc<Hub>,
    source: Source,
    profile: ChainProfile,
    config: Config,
    store: Store,
    /// What the operator was alerted of, once each.
    alerted: Mutex<HashSet<String>>,
    /// The Lightning gateway, if the hub runs one.
    pub(crate) peer: Peer,
}

impl Gateway {
    /// The deposits of `hub` on the chain of `source`, of `profile`, kept
    /// at `path`.
    pub fn open(
        hub: Arc<Hub>,
        source: Source,
        profile: ChainProfile,
        config: Config,
        path: &Path,
    ) -> anyhow::Result<Self> {
        config
            .check(&hub.info().policy.timing)
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            store: Store::open(path)?,
            hub,
            source,
            profile,
            config,
            alerted: Mutex::default(),
            peer: Peer::default(),
        })
    }

    fn lock_depth(&self) -> u32 {
        self.hub.info().policy.timing.min_depth
    }

    /// What the hub's info says of its deposits.
    pub fn advertise(&self) -> Deposits {
        let c = &self.config;
        Deposits {
            fee: c.fee,
            min: c.min,
            max: c.max,
            depth: c.depth,
            window: c.window,
            timeout: c.timeout_blocks(self.lock_depth()).unwrap_or(u32::MAX),
        }
    }

    /// The address and amount whose payment locks the hub's notes to the
    /// outputs of `body`: the same again for the same deposit.
    pub async fn deposit(&self, body: &Deposit) -> Result<Depositing, Failure> {
        let outputs = body.receive.outputs()?;
        let (keyset, ephemeral, hash) = body.receive.terms()?;
        if self.hub.keyset(&keyset).is_none() {
            return Err(Failure(Problem::new(Code::UnknownKeyset, "unknown keyset")));
        }
        let amount = outputs
            .iter()
            .try_fold(0u64, |sum, o| sum.checked_add(o.denomination()))
            .ok_or_else(|| Failure(Problem::invalid("the outputs overflow")))?;
        let c = &self.config;
        if outputs.is_empty() || amount < c.min || amount > c.max {
            return Err(refused("outside the amounts the hub takes in deposits"));
        }
        let pays = amount
            .checked_add(c.fee.due(amount))
            .filter(|pays| Amount::from_sat(*pays) <= Amount::MAX_MONEY)
            .ok_or_else(|| Failure(Problem::invalid("the amount overflows")))?;
        let refund = XOnlyPublicKey::from_slice(&body.refund.0)
            .map_err(|_| Failure(Problem::invalid("refund: not an x-only public key")))?;
        let canonical = Deposit {
            receive: Receive::new(&keyset, &ephemeral, &outputs, &hash),
            refund: Hex(refund.serialize().to_vec()),
        };
        let canonical = serde_json::to_string(&canonical).expect("a deposit serializes");
        let turn = self.peer.turn().await;
        if self.store.get(&hash).map_err(store_down)?.is_none() {
            if self.peer.recorded(&hash)?.is_some() || self.peer.holds_swap(&hash)? {
                return Err(refused("the hub holds this hash for another payment"));
            }
            if self
                .on_hub(move |hub| hub.own_lock_of(&hash))
                .await?
                .is_some()
            {
                return Err(refused("the hub locked notes under this hash before"));
            }
            let (from, _) = self.on_hub(|hub| hub.scanned()).await?.ok_or_else(|| {
                Failure(Problem::new(
                    Code::Unavailable,
                    "the hub follows no chain yet",
                ))
            })?;
            let claim = self
                .claim_key(&hash)?
                .x_only_public_key(&Secp256k1::new())
                .0;
            let timeout = c
                .timeout_blocks(self.lock_depth())
                .and_then(|blocks| from.checked_add(blocks))
                .ok_or_else(|| Failure(Problem::invalid("the deposit's timeout overflows")))?;
            let htlc = Htlc {
                hash,
                claim,
                refund,
                timeout,
            };
            let fund_by = from.saturating_add(c.window);
            let inserted = self
                .store
                .insert(&canonical, amount, pays, &htlc, fund_by, c.max_open)
                .map_err(store_down)?;
            if !inserted && self.store.get(&hash).map_err(store_down)?.is_none() {
                return Err(Failure(Problem::new(
                    Code::Unavailable,
                    "too many deposits asked and not paid: retry later",
                )));
            }
            tracing::info!(hash = %hex::encode(hash), amount, pays, fund_by, timeout, "deposit asked");
        }
        drop(turn);
        let record = self
            .store
            .get(&hash)
            .map_err(store_down)?
            .ok_or_else(store_gone)?;
        if record.body != canonical {
            return Err(refused("another deposit under this hash"));
        }
        Ok(self.terms(&record))
    }

    /// The terms answered for the deposit `record`.
    fn terms(&self, record: &Record) -> Depositing {
        Depositing {
            address: record
                .htlc
                .address(licash_chain_client::network(self.profile))
                .to_string(),
            amount: record.pays,
            claim: Hex(record.htlc.claim.serialize().to_vec()),
            timeout: record.htlc.timeout,
            fund_by: record.fund_by,
        }
    }

    /// Where the deposit under `hash` stands, with what was asked and the
    /// terms answered: whoever knows its hash, its wallet, finds it again.
    pub fn deposit_status(&self, hash: &[u8; 32]) -> Result<DepositStatus, Failure> {
        let record = self.store.get(hash).map_err(store_down)?.ok_or_else(|| {
            Failure(Problem::new(
                Code::UnknownLock,
                "no deposit under this hash",
            ))
        })?;
        Ok(DepositStatus {
            status: ReceiveStatus {
                state: record.state,
                lock: record.lock.map(|id| Hex(id.as_bytes().to_vec())),
            },
            funding: record.funding.map(|(outpoint, height)| Funding {
                txid: outpoint.txid.to_string(),
                vout: outpoint.vout,
                height,
            }),
            request: serde_json::from_str(&record.body).ok(),
            terms: Some(self.terms(&record)),
        })
    }

    /// The key the hub claims the deposit under `hash` with.
    fn claim_key(&self, hash: &[u8; 32]) -> Result<SecretKey, Failure> {
        SecretKey::from_slice(&self.hub.deposit_secret(hash)).map_err(|_| {
            tracing::error!(hash = %hex::encode(hash), "no deposit key under this hash");
            Failure(Problem::new(Code::Unavailable, "the hub cannot serve now"))
        })
    }
}

impl Gateway {
    /// Whether a deposit is under `hash`, which no other lock of the hub's
    /// own notes may take.
    pub fn holds(&self, hash: &[u8; 32]) -> Result<bool, String> {
        Ok(self.recorded(hash)?.is_some())
    }

    /// Which of `hashes` the deposits file at `path` holds, if there is one.
    pub fn holding(path: &Path, hashes: &[[u8; 32]]) -> rusqlite::Result<HashSet<[u8; 32]>> {
        peer::recorded_in(path, "SELECT 1 FROM deposits WHERE hash = ?1", hashes)
    }

    /// The deposit under `hash`, if any, and the lock it backs once
    /// locked.
    pub fn recorded(&self, hash: &[u8; 32]) -> Result<Recorded, String> {
        let record = self.store.get(hash).map_err(|e| e.to_string())?;
        Ok(record.map(|record| record.lock))
    }

    /// Follow the chain and the deposits until the task is dropped, once
    /// the chain is of the hub's network.
    pub async fn run(self: Arc<Self>) {
        let profile = loop {
            match self.source.profile().await {
                Ok(profile) => break profile,
                Err(e) => tracing::debug!(chain = %self.source, "deposits: {e:#}"),
            }
            tokio::time::sleep(self.config.poll).await;
        };
        if profile != self.profile {
            // The hub's own scan alerts of it.
            let (chain, network) = (&self.source, self.profile);
            return tracing::error!(%chain, %profile, %network, "taking no deposits on another network");
        }
        loop {
            self.round().await;
            tokio::time::sleep(self.config.poll).await;
        }
    }

    /// One round: scan the blocks beyond those scanned, then lock the
    /// deposits paid at depth, settle or cancel those decided, and claim
    /// the coins of those settled.
    pub async fn round(&self) {
        if let Err(e) = self.scan().await {
            tracing::warn!(chain = %self.source, "scanning for deposits: {e:#}");
        }
        if let Err(e) = self.follow().await {
            tracing::warn!(error = %e.0.message, "cannot follow deposits");
        }
    }

    /// Scan the best chain from the newest block scanned, or the tip.
    async fn scan(&self) -> anyhow::Result<()> {
        loop {
            let top = self.store.top()?;
            let first = match top {
                Some(_) => 0,
                None => self.source.tip().await?.height,
            };
            let ours = |height| Ok(self.store.hash_at(height)?);
            match self.source.follow(top, first, ours).await? {
                Follow::Next(height, block) => self.store.scanned(height, &block)?,
                Follow::Rewind(height) => {
                    // None kept below: rather than from the tip, on from
                    // there, the best chain's block before it as scanned.
                    let below = match (self.store.oldest()?, height.checked_sub(1)) {
                        (Some(oldest), Some(at)) if oldest >= height => {
                            tracing::warn!(chain = %self.source, height, "deposits: the chain left the oldest block scanned; scanning again from it");
                            Some(self.source.hash(at).await?)
                        }
                        _ => None,
                    };
                    for hash in self.store.rewind(height, below)? {
                        let name = hex::encode(hash);
                        let message = format!(
                            "The payment of deposit {name}, whose notes the hub locked, left the \
                             best chain."
                        );
                        self.alert(format!("unbacked {name}"), message).await;
                    }
                }
                Follow::AtTip => return Ok(()),
            }
        }
    }

    async fn follow(&self) -> Result<(), Failure> {
        let Some((scanned, _)) = self.store.top().map_err(store_down)? else {
            return Ok(());
        };
        for record in self.store.live().map_err(store_down)? {
            let followed = match record.state {
                ReceiveState::Open => self.accept(&record, scanned).await,
                ReceiveState::Locked => self.decide(&record).await,
                ReceiveState::Settled => self.claim(&record, scanned).await,
                ReceiveState::Canceled => Ok(()),
            };
            if let Err(e) = followed {
                let hash = hex::encode(record.hash);
                tracing::warn!(%hash, error = %e.0.message, "cannot follow a deposit");
            }
        }
        Ok(())
    }

    /// Lock the hub's notes for `record` once its payment is at depth, if
    /// the lock leaves time to claim the coins before the timeout; give it
    /// up unpaid once its window passed.
    async fn accept(&self, record: &Record, scanned: u32) -> Result<(), Failure> {
        let hash = record.hash;
        let name = hex::encode(hash);
        let Some((_, height)) = record.funding else {
            if scanned > record.fund_by {
                tracing::info!(hash = %name, "deposit unpaid in time");
                return self.finish(record, "canceled");
            }
            return Ok(());
        };
        if scanned.saturating_sub(height).saturating_add(1) < self.config.depth {
            return Ok(());
        }
        let body: Deposit = serde_json::from_str(&record.body).map_err(|_| store_gone())?;
        let ((keyset, ephemeral, _), outputs) = (body.receive.terms()?, body.receive.outputs()?);
        let (lock_depth, timeout) = (self.lock_depth(), record.htlc.timeout);
        // Until the lock under the hash is recorded.
        let _turn = self.peer.turn().await;
        // A lock made before the gateway stopped, or for a receipt under
        // the same hash, which backs it once the Lightning gateway recorded
        // it: one payment per lock.
        if let Some(lock) = self.on_hub(move |hub| hub.own_lock_of(&hash)).await? {
            let receipt = self.peer.recorded(&hash)? == Some(Some(lock.proposal.id()));
            if receipt || self.peer.swap_owns(&hash, &lock.proposal.id())? {
                let owner = if receipt {
                    "a Lightning receipt"
                } else {
                    "a swap"
                };
                let message = format!(
                    "The hash of deposit {name} is locked for {owner}: its payer \
                     takes its coins back."
                );
                self.alert(format!("taken {name}"), message).await;
                return self.finish(record, "canceled");
            }
            let expiry = lock
                .proposal
                .lock()
                .ok_or_else(store_gone)?
                .terms()
                .expiry();
            if lock.proposal.outputs() != outputs.as_slice()
                || !self.config.claims_in_time(expiry, lock_depth, timeout)
            {
                let message = format!(
                    "The hash of deposit {name} is locked to other notes or for too long: its \
                     payer takes its coins back."
                );
                self.alert(format!("taken {name}"), message).await;
                return self.finish(record, "canceled");
            }
            self.store
                .set(&hash, "lock", &lock.proposal.id().as_bytes())
                .map_err(store_down)?;
            return self.finish(record, "locked");
        }
        let (tip, checkpoint) = self
            .on_hub(|hub| Ok((hub.scanned()?, hub.checkpoint())))
            .await?;
        let (Some((tip, _)), Some(checkpoint)) = (tip, checkpoint) else {
            return Ok(());
        };
        let expiry = tip.saturating_add(self.config.lock_blocks);
        if !self.config.claims_in_time(expiry, lock_depth, timeout) {
            tracing::warn!(hash = %name, expiry, timeout, "deposit paid too late to lock: its payer takes its coins back");
            return self.finish(record, "canceled");
        }
        let terms = TimedTerms::new(
            checkpoint.profile(),
            checkpoint.commitment(),
            expiry,
            lock_depth,
            hash,
        )
        .map_err(|e| Failure(Problem::invalid(e.to_string())))?;
        let locked = self
            .on_hub(move |hub| hub.lock_own(&keyset, ephemeral, outputs, terms))
            .await;
        match locked {
            Ok(lock) => {
                let id = lock.proposal.id();
                self.store
                    .set(&hash, "lock", &id.as_bytes())
                    .map_err(store_down)?;
                self.finish(record, "locked")?;
                tracing::info!(hash = %name, lock = %hex::encode(id.as_bytes()), expiry, "deposit locked");
                Ok(())
            }
            // Retried: the hub catches up, or adopts its lock above.
            Err(Error::Refused(Refusal::Unsynced | Refusal::NoChain | Refusal::Exists)) => Ok(()),
            Err(Error::Refused(refusal)) => {
                tracing::warn!(hash = %name, %refusal, "the hub refuses to lock a deposit");
                self.finish(record, "canceled")
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl Gateway {
    /// Settle `record` once its lock paid, learning the preimage that
    /// claims its coins, or cancel it once its lock is refunded, which the
    /// gateway asks for once the lock expired at depth.
    async fn decide(&self, record: &Record) -> Result<(), Failure> {
        let id = record.lock.ok_or_else(store_gone)?;
        let lock = self.on_hub(move |hub| hub.locked(&id)).await?;
        let terms = lock.proposal.lock().ok_or_else(store_gone)?.terms().clone();
        match (lock.settled, lock.preimage) {
            (Some(Settled::Refund), _) => {
                let hash = hex::encode(record.hash);
                tracing::info!(%hash, "deposit refunded unclaimed: its payer takes its coins back");
                self.finish(record, "canceled")
            }
            (Some(Settled::Pay), Some(preimage)) => self.settle(record, preimage),
            // Activated on Bitcoin: the lock pays.
            (None, Some(preimage)) => {
                self.on_hub(move |hub| hub.settle(&id, &preimage)).await?;
                self.settle(record, preimage)
            }
            (None, None) => {
                let tip = self
                    .on_hub(|hub| hub.scanned())
                    .await?
                    .map_or(0, |(h, _)| h);
                if tip < terms.expiry().saturating_add(terms.depth()) {
                    return Ok(());
                }
                match self.on_hub(move |hub| hub.refund(&id)).await {
                    Ok(_) => self.finish(record, "canceled"),
                    Err(Error::Refused(Refusal::Unexpired | Refusal::Activated)) => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }
            (Some(Settled::Pay), None) => Err(store_gone()),
        }
    }

    fn settle(&self, record: &Record, preimage: [u8; 32]) -> Result<(), Failure> {
        self.store
            .set(&record.hash, "preimage", &preimage)
            .map_err(store_down)?;
        tracing::info!(hash = %hex::encode(record.hash), amount = record.amount, "deposit settled");
        self.finish(record, "settled")
    }

    /// Claim the coins of settled `record` to the sweep address, broadcast
    /// each round until a block spends them, and made again in place of
    /// the one before as the rate its timeout calls for rises; alert the
    /// operator if another spend did, or once the timeout nears.
    async fn claim(&self, record: &Record, scanned: u32) -> Result<(), Failure> {
        let name = hex::encode(record.hash);
        if let Some(by) = record.spent_by {
            let message = format!(
                "The payment of deposit {name} was spent by {by}, not by the hub's claim, after \
                 its notes were issued: the hub lost {}.",
                licash_notify::sat(record.pays)
            );
            self.alert(format!("lost {name}"), message).await;
            return self.store.done(&record.hash).map_err(store_down);
        }
        let old = match &record.sweep {
            Some(raw) => Some(deserialize::<Transaction>(raw).map_err(|_| store_gone())?),
            None => None,
        };
        let left = record.htlc.timeout.saturating_sub(scanned);
        let tx = match self.make_claim(record, old.as_ref(), left).await? {
            Some(tx) => {
                self.store
                    .set_sweep(&record.hash, &tx)
                    .map_err(store_down)?;
                let txid = tx.compute_txid();
                tracing::info!(hash = %name, %txid, left, "deposit claim made");
                tx
            }
            None => match old {
                Some(old) => old,
                None => return Ok(()),
            },
        };
        if let Err(e) = self.source.broadcast(&tx).await {
            tracing::warn!(hash = %name, "cannot broadcast a deposit's claim: {e:#}");
        }
        if left <= self.config.sweep_blocks / 2 {
            let message = format!(
                "The claim {} of deposit {name} is unconfirmed {left} blocks before its payer may \
                 take the coins back.",
                tx.compute_txid()
            );
            self.alert(format!("unclaimed {name}"), message).await;
        }
        Ok(())
    }

    /// The claim of `record` to broadcast in place of `old`, the one made
    /// before if any, `left` blocks before its timeout: at the fee rate the
    /// chain estimates confirms well within the sweep blocks or half of
    /// those left, whichever is sooner, rising as they run out
    /// ([`deadline::rate`]), all of its coins but dust at most. None while
    /// `old` pays that rate already, or if its coins cannot pay a claim.
    async fn make_claim(
        &self,
        record: &Record,
        old: Option<&Transaction>,
        left: u32,
    ) -> Result<Option<Transaction>, Failure> {
        let c = &self.config;
        let (outpoint, _) = record.funding.ok_or_else(store_gone)?;
        let preimage = record.preimage.ok_or_else(store_gone)?;
        // Well within the sweep blocks, sooner as the timeout nears.
        let sweep = u16::try_from(c.sweep_blocks / 3).unwrap_or(u16::MAX).max(1);
        let target = deadline::target(left).min(sweep);
        let estimated = self.source.fee_rate(target).await.unwrap_or_else(|e| {
            tracing::warn!(chain = %self.source, "cannot estimate a fee rate: {e:#}");
            None
        });
        let rate = deadline::rate(estimated, c.min_fee_rate, left);
        let secret = self.claim_key(&record.hash)?;
        let value = Amount::from_sat(record.pays);
        let build = |rate| {
            record.htlc.claim_capped(
                outpoint,
                value,
                c.sweep_to.clone(),
                rate,
                &secret,
                &preimage,
            )
        };
        match deadline::remake(old, value, rate, build) {
            Ok(tx) => Ok(tx),
            Err(e @ deposit::Error::TooSmall { .. }) => {
                let name = hex::encode(record.hash);
                let message = format!("Deposit {name} cannot be claimed: {e}.");
                self.alert(format!("unclaimable {name}"), message).await;
                Ok(None)
            }
            Err(e) => {
                tracing::error!(hash = %hex::encode(record.hash), "cannot claim a deposit: {e}");
                Err(store_gone())
            }
        }
    }

    fn finish(&self, record: &Record, state: &str) -> Result<(), Failure> {
        self.store.finish(&record.hash, state).map_err(store_down)
    }

    /// Run `op` on the hub off the async workers.
    async fn on_hub<T: Send + 'static>(
        &self,
        op: impl FnOnce(&Hub) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let hub = self.hub.clone();
        tokio::task::spawn_blocking(move || op(&hub))
            .await
            .map_err(|e| Error::Io(std::io::Error::other(e)))?
    }

    /// Tell the operator `message` once for `key`.
    async fn alert(&self, key: String, message: String) {
        let new = self
            .alerted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key);
        if new {
            tracing::warn!("{message}");
            if let Err(e) = self.on_hub(move |hub| hub.alert(&message)).await {
                tracing::error!(error = %e, "cannot alert the operator");
            }
        }
    }
}
