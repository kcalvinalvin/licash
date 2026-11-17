//! The wallet's file: one SQLite database, written in IMMEDIATE
//! transactions, so that processes sharing it see each step whole.

use std::{
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::Error;

/// The schema this wallet reads and writes, as SQLite's `user_version`:
/// [`SCHEMA`]'s, 1, with each of [`MIGRATIONS`] after it.
const VERSION: i64 = 1 + MIGRATIONS.len() as i64;

/// What each version adds to the one before: the first takes 1 to 2. A
/// file of an older version is brought up to date when opened.
const MIGRATIONS: &[&str] = &[
    SILENT,
    FALLBACK,
    UNTWEAKED,
    PAYOUTS,
    VISITED,
    RENEWALS,
    DELIVERIES,
    UNLINKED_AT,
    VIA,
    KEYED,
];

/// Version 11: payments made under a key of their payer's, at most one
/// each ([`crate::Wallet::pay_code_keyed`]), recorded with the payment:
/// a payer that stopped mid-way finds by its key whether it paid. No
/// payment of an older file has a key.
const KEYED: &str = "
ALTER TABLE sends ADD COLUMN key BLOB;
CREATE UNIQUE INDEX sends_key ON sends (key) WHERE key IS NOT NULL;
";

/// Version 10: Lightning invoices paid through swap providers. A lock
/// paying one names the provider's URL, `via`, and the invoice it asks it
/// to pay, `via_invoice`: both or neither. Its `invoice` stays NULL, which
/// names one its hub's gateway pays. No lock of an older file names one.
const VIA: &str = "
ALTER TABLE locks ADD COLUMN via TEXT;
ALTER TABLE locks ADD COLUMN via_invoice TEXT;
";

/// Version 9: a privacy rest starts when a note finishes unlinking, not
/// when it was received. Existing unlinked notes have an unknown unlink
/// time: conservatively begin their rest now. No remint or network work
/// is started by this migration.
const UNLINKED_AT: &str = "
ALTER TABLE notes ADD COLUMN unlinked_at INTEGER;
UPDATE notes SET unlinked_at = unixepoch() WHERE unlinked = 1;
";

/// Version 8: payments to payment codes kept at their hub for the payee,
/// and mailed only once it does not say it has them.
const DELIVERIES: &str = "
-- How each payment to a payment code reaches its payee: its hub asked to
-- keep it under the inbox tag and key of the receipt secret, the payee
-- telling ack through the hub once it has it; mailed to the payee's
-- mailbox (an x-only Nostr key), if the code names one, from mail_at on
-- unless acked by then. kept_at is when its hub answered that it keeps
-- it, and mail_at set once released: an hour on if kept, else then. tries
-- counts the mailings failed, the last for error. A payment neither acked
-- nor mailed in the days a hub keeps it is given up. No send of an older
-- file has one: each was mailed, or handed over, as it was made.
CREATE TABLE deliveries (
    send BLOB PRIMARY KEY REFERENCES sends (id),
    mailbox BLOB,
    tag BLOB NOT NULL,
    key BLOB NOT NULL,
    ack BLOB NOT NULL,
    kept_at INTEGER,
    mail_at INTEGER,
    acked_at INTEGER,
    mailed_at INTEGER,
    tries INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    given_up_at INTEGER
) STRICT;
-- The last entry of the hub's inbox the wallet looked through for
-- payments kept for it: none before it first did, so it looks through all.
ALTER TABLE hubs ADD COLUMN inbox_after INTEGER;
";

/// Version 7: renewals of notes a hub no longer unlinks since it restarted.
const RENEWALS: &str = "
-- Why a note stays linked (unlinked = -1): the code its hub refused to
-- unlink it with ('expired' while the wallet may renew it), or
-- 'renewal_refused' once its hub refused to renew it. None in a file of an
-- older version: its notes that stay linked are never renewed.
ALTER TABLE notes ADD COLUMN stays_linked TEXT;
-- 1 for a payment of the wallet's notes to itself renewing them, which
-- Wallet::unlink finishes: every send of an older file is a payment.
ALTER TABLE sends ADD COLUMN renewal INTEGER NOT NULL DEFAULT 0;
";

/// Version 6: hubs visited to pay a payee there, not trusted.
const VISITED: &str = "
-- Whether the owner trusts the hub, having added it (1), or the wallet
-- only visited it to pay a payee there (0), the hub proving it is the one
-- the payee's code names. Every hub of an older file was added.
ALTER TABLE hubs ADD COLUMN trusted INTEGER NOT NULL DEFAULT 1;
";

/// Version 5: notes paid out on Bitcoin through a swap provider.
const PAYOUTS: &str = "
-- Payouts by the hash of a preimage only the wallet knows, recorded with
-- it, the key that claims the coin and the payee before the lock to the
-- provider (in locks, under that hash) is made at from_hub. The provider
-- locks a coin on chain under the same hash, which the wallet claims
-- paying script (address, as shown) exactly amount, its fee the rest of
-- the coin, budget at most. fee is the provider's, its miners' included.
-- refund_key and timeout are the provider's terms once it took it; the
-- funding columns the coin once found where the provider says, worth
-- value. claim_tx is the claim once made, broadcast until it confirms:
-- the preimage is public from revealed_at on. scanned is the last block
-- looked in for the coin's spend, spent_height the block of the claim.
CREATE TABLE payouts (
    hash BLOB PRIMARY KEY,
    provider TEXT NOT NULL,
    preimage BLOB NOT NULL,
    claim_key BLOB NOT NULL,
    from_hub INTEGER NOT NULL REFERENCES hubs (id),
    address TEXT NOT NULL,
    script BLOB NOT NULL,
    amount INTEGER NOT NULL,
    budget INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    refund_key BLOB,
    timeout INTEGER,
    funding_txid BLOB,
    funding_vout INTEGER,
    funding_height INTEGER,
    value INTEGER,
    claim_tx BLOB,
    revealed_at INTEGER,
    scanned INTEGER,
    spent_height INTEGER,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    failed TEXT
) STRICT;
";

/// Version 4: blocks looked in without tweaks, for fallback addresses and
/// the coins held only. None in a file of an older version: it looked in
/// every block with tweaks.
const UNTWEAKED: &str = "
-- The first of the blocks up to the next looked in without tweaks, to look
-- in again for silent payments once a hub serves them: none if none was.
ALTER TABLE silent_scan ADD COLUMN untweaked INTEGER;
";

/// Version 3: fallback addresses on Bitcoin, for payers of no silent
/// payments.
const FALLBACK: &str = "
-- The highest index of a fallback address handed out in a payment code:
-- none until one is. Those up to it, and a few after, are watched.
CREATE TABLE silent_fallback (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    shown INTEGER NOT NULL
) STRICT;
";

/// Version 2: payments to the silent payment address.
const SILENT: &str = "
-- The next block to look in for silent payments, from a first block the
-- owner chose: none until it chose one.
CREATE TABLE silent_scan (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    next INTEGER NOT NULL
) STRICT;
-- The blocks looked in lately, by height, to notice a reorganization.
CREATE TABLE silent_blocks (
    height INTEGER PRIMARY KEY,
    hash BLOB NOT NULL
) STRICT;
-- Outputs paying the silent payment address, in a block of the best chain,
-- with what spends each (its key's tweak and label); spent_by names the
-- transaction spending it, the wallet's own or seen, and spent_at the
-- block it is in, once in one.
CREATE TABLE silent_coins (
    txid BLOB NOT NULL,
    vout INTEGER NOT NULL,
    value INTEGER NOT NULL,
    output BLOB NOT NULL,
    tweak BLOB NOT NULL,
    label INTEGER,
    height INTEGER NOT NULL,
    spent_by BLOB,
    spent_at INTEGER,
    PRIMARY KEY (txid, vout)
) STRICT;
-- The transactions the wallet made of its coins, to broadcast.
CREATE TABLE silent_spends (
    txid BLOB PRIMARY KEY,
    tx BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
";

/// Version 1, as wallets in the field hold it (`tests/fixtures/wallet-v1.sql`):
/// never change it, add a migration.
const SCHEMA: &str = "
CREATE TABLE meta (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    -- The owner's seed, unless the application keeps it elsewhere.
    seed BLOB,
    -- The owner's address, which the seed opening the wallet must derive.
    address BLOB NOT NULL,
    -- The network of the hubs it trusts, by name: bitcoin, regtest, signet
    -- or mutinynet.
    network TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
-- Hubs the owner trusts, and what each last said of itself (an Info body).
CREATE TABLE hubs (
    id INTEGER PRIMARY KEY,
    info TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
-- The URLs each hub is reached at: one each, for now.
CREATE TABLE hub_urls (
    url TEXT PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id)
) STRICT;
CREATE INDEX hub_urls_hub ON hub_urls (hub);
-- Each hub with the URL the wallet reaches it at.
CREATE VIEW reach (hub, url) AS
    SELECT hub, url FROM hub_urls
    WHERE rowid IN (SELECT MIN(rowid) FROM hub_urls GROUP BY hub);
-- A keyset belongs to the one hub that first advertised it.
CREATE TABLE keysets (
    id BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    bytes BLOB NOT NULL
) STRICT;
-- Payments made, each step recorded before the hub is asked for the next:
-- the offer asked for, then the release once signed. A signed send is
-- given up only once its hub refused it as spent, which it never releases.
CREATE TABLE sends (
    id BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    payee BLOB NOT NULL,
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    offer TEXT NOT NULL,
    release TEXT,
    unlock BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    abandoned TEXT
) STRICT;
-- The owner's notes. Each was made at `position` of `branch` of the
-- proposal with ephemeral key `ephemeral`, where the scan key derives it
-- again.
CREATE TABLE notes (
    y BLOB PRIMARY KEY,
    keyset BLOB NOT NULL REFERENCES keysets (id),
    denom INTEGER NOT NULL,
    note BLOB NOT NULL,
    proposal BLOB NOT NULL,
    ephemeral BLOB NOT NULL,
    branch INTEGER NOT NULL,
    position INTEGER NOT NULL,
    -- The send or lock that spends it, from when it chose it.
    spent_by BLOB,
    -- 1 unlinked, 0 linked, -1 linked for good: its hub unlinks it no more.
    unlinked INTEGER NOT NULL DEFAULT 0,
    -- An unlink under way: its session, to ask the hub again.
    session TEXT,
    -- When its hub showed it spent by a payment this file never made, by a
    -- copy of the wallet: it is never spent here.
    gone_at INTEGER,
    added_at INTEGER NOT NULL
) STRICT;
CREATE INDEX notes_spent_by ON notes (spent_by);
-- Notes locked to a payee on the preimage of `hash` until `expiry`, else
-- refunded: the lock asked for (a Lock body), then the hub's certificate,
-- then the branch its hub settled, once the wallet kept its notes of it.
-- An uncertified lock is given up only once its hub refused it, which
-- proves the hub never recorded it. A lock to the hub may pay a Lightning
-- `invoice` through it, and record why the hub failed or refused to.
CREATE TABLE locks (
    id BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    payee BLOB NOT NULL,
    hash BLOB NOT NULL,
    expiry INTEGER NOT NULL,
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    request TEXT NOT NULL,
    certificate BLOB,
    -- 'pay' or 'refund'.
    settled TEXT,
    preimage BLOB,
    invoice TEXT,
    failure TEXT,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    abandoned TEXT
) STRICT;
-- Payments asked for over Lightning, by their hash: the preimage only the
-- wallet knows and the receipt asked for (a Receive body), recorded before
-- the hub hears of them, then the hub's invoice and, once it was paid, the
-- lock of the hub's notes the wallet claims. Done once claimed; canceled
-- once the hub refused it or canceled the invoice.
CREATE TABLE invoices (
    hash BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    preimage BLOB NOT NULL,
    amount INTEGER NOT NULL,
    body TEXT NOT NULL,
    request TEXT,
    lock BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    canceled TEXT
) STRICT;
-- Deposits on Bitcoin asked for, by their hash: the preimage and the
-- refund key only the wallet knows and the deposit asked for (a Deposit
-- body), recorded before the hub hears of them; then the hub's terms (its
-- claim key, the timeout, what to pay and by which block), the payment
-- once the hub saw it confirm, and the lock of the hub's notes the wallet
-- claims. Done once claimed; canceled once the hub refused or canceled it;
-- a refund, once the wallet made one, takes the coins back.
CREATE TABLE deposits (
    hash BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    preimage BLOB NOT NULL,
    refund_key BLOB NOT NULL,
    amount INTEGER NOT NULL,
    body TEXT NOT NULL,
    claim BLOB,
    timeout INTEGER,
    pays INTEGER,
    address TEXT,
    fund_by INTEGER,
    txid BLOB,
    vout INTEGER,
    height INTEGER,
    lock BLOB,
    refund BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    canceled TEXT
) STRICT;
-- Swaps of notes at one hub for notes at another through a provider, by
-- the hash of a preimage only the wallet knows, recorded before the lock to
-- the provider (in locks, under that hash) is made; then the provider's
-- lock to the wallet at the other hub, which the wallet claims with the
-- preimage. Done once claimed; failed once the provider did not lock.
CREATE TABLE swaps (
    hash BLOB PRIMARY KEY,
    provider TEXT NOT NULL,
    preimage BLOB NOT NULL,
    from_hub INTEGER NOT NULL REFERENCES hubs (id),
    to_hub INTEGER NOT NULL REFERENCES hubs (id),
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    theirs BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    failed TEXT
) STRICT;
-- Payments taken, and locks claimed.
CREATE TABLE receipts (
    id BLOB PRIMARY KEY,
    keyset BLOB NOT NULL REFERENCES keysets (id),
    amount INTEGER NOT NULL,
    received_at INTEGER NOT NULL
) STRICT;
";

/// What a wallet file holds of its owner.
pub(crate) struct Owner {
    /// Unless the application keeps it.
    pub(crate) seed: Option<[u8; 32]>,
    pub(crate) address: Vec<u8>,
    pub(crate) network: String,
}

pub(crate) struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// A new wallet file at `path` of the owner at `address`, on `network`,
    /// holding its `seed` if given, readable by its owner only. Never over
    /// an existing file.
    pub(crate) fn create(
        path: &Path,
        seed: Option<&[u8; 32]>,
        address: &[u8],
        network: &str,
    ) -> Result<Self, Error> {
        let mut file = std::fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
        file.open(path).map_err(|e| setup(path, e))?;
        let store = Self::connect(path)?;
        store.write(|tx| {
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", 1)?;
            migrate(tx)?;
            tx.execute(
                "INSERT INTO meta (one, seed, address, network, created_at)
                 VALUES (1, ?1, ?2, ?3, ?4)",
                rusqlite::params![seed, address, network, now()],
            )?;
            Ok(())
        })?;
        Ok(store)
    }

    /// The wallet file at `path`, and what it holds of its owner.
    pub(crate) fn open(path: &Path) -> Result<(Self, Owner), Error> {
        if !path.exists() {
            return Err(Error::Setup(format!("no wallet at {}", path.display())));
        }
        let store = Self::connect(path)?;
        let version: i64 =
            store.read(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?))?;
        if version != VERSION {
            store.write(migrate)?;
        }
        let owner = store.read(|c| {
            Ok(
                c.query_row("SELECT seed, address, network FROM meta", [], |r| {
                    Ok(Owner {
                        seed: r.get(0)?,
                        address: r.get(1)?,
                        network: r.get(2)?,
                    })
                })?,
            )
        })?;
        Ok((store, owner))
    }

    fn connect(path: &Path) -> Result<Self, Error> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags).map_err(|e| setup(path, e))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn read<T>(
        &self,
        op: impl FnOnce(&Connection) -> Result<T, Error>,
    ) -> Result<T, Error> {
        op(&self.conn())
    }

    /// `op` whole or not at all.
    pub(crate) fn write<T>(
        &self,
        op: impl FnOnce(&Transaction<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = op(&tx)?;
        tx.commit()?;
        Ok(value)
    }
}

/// Bring the schema up to [`VERSION`], in `tx`: from any older version,
/// never from a newer one.
fn migrate(tx: &Transaction<'_>) -> Result<(), Error> {
    let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if !(1..=VERSION).contains(&version) {
        return Err(Error::Setup(format!(
            "wallet schema {version}, this version reads 1 to {VERSION}"
        )));
    }
    if version < VERSION {
        for step in &MIGRATIONS[version as usize - 1..] {
            tx.execute_batch(step)?;
        }
        tx.pragma_update(None, "user_version", VERSION)?;
        tracing::info!(from = version, to = VERSION, "wallet schema migrated");
    }
    Ok(())
}

fn setup(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::Setup(format!("wallet {}: {e}", path.display()))
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A u64 as SQLite's i64, which every amount and denomination fits.
pub(crate) fn sql(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
