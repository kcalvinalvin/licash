//! The holder's contracts, in `contracts.sqlite` of the wallet's
//! directory, and the instances each keeps, in `contracts/` beside it:
//! each opening as its machine stored itself at its last step, with what
//! a list shows of it. Neither is made until a first contract opens.
//!
//! The file's version tells which of its tables a build must know to
//! read it right. The `ceilings` are none of them: made with the first
//! kept, whatever the version, and ignored by a build before them, which
//! opens the file all the same. A migration to come must make them
//! `IF NOT EXISTS` too.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context as _, Result};
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::{OutPoint, Txid};
use licash_api::Fees;
use licash_contract::opening::{Fate, Phase};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// The format of the rows: a machine's stored bytes, of
/// `licash_contract`'s format 1.
const FORMAT: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS contracts (
    id BLOB PRIMARY KEY,
    hub TEXT NOT NULL,
    format INTEGER NOT NULL,
    phase INTEGER NOT NULL,
    mock INTEGER NOT NULL,
    irrevocable INTEGER NOT NULL,
    deposit BLOB NOT NULL,
    value INTEGER NOT NULL,
    price INTEGER NOT NULL,
    cap INTEGER NOT NULL,
    assert_height INTEGER NOT NULL,
    close_height INTEGER NOT NULL,
    funding BLOB,
    since INTEGER NOT NULL,
    fate TEXT,
    abandoned TEXT,
    state BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX IF NOT EXISTS contracts_deposit ON contracts (deposit)
    WHERE abandoned IS NULL AND (fate IS NULL OR fate != 'dead');
";

/// Contracts she forced closed, each a recovery's stage and state in a
/// format of `recovery`'s: made at her first, the file of version 2 from
/// then on. A wallet that never forced a contract closed keeps version 1,
/// which builds before readily open.
const RECOVERIES: &str = "
CREATE TABLE recoveries (
    id BLOB PRIMARY KEY REFERENCES contracts (id),
    format INTEGER NOT NULL,
    stage TEXT NOT NULL,
    irrevocable INTEGER NOT NULL,
    state BLOB NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
";

/// The most each contract's hub may charge her, as JSON: its fees as the
/// contract opened, raised since by her only. No row, of a contract opened
/// before these were kept, tells nothing: nor does no table.
const CEILINGS: &str = "
CREATE TABLE IF NOT EXISTS ceilings (
    id BLOB PRIMARY KEY REFERENCES contracts (id),
    fees TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
";

/// A recovery as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RecoveryRow {
    pub id: [u8; 32],
    pub format: i64,
    pub stage: String,
    /// Past giving it up: her request signed, or about to be.
    pub irrevocable: bool,
    pub state: Vec<u8>,
    /// What stopped its last step, if anything did.
    pub error: Option<String>,
    pub created_at: i64,
}

/// Where beside `contracts.sqlite` the instances each opening keeps are,
/// in a directory of its id each.
const KEPT_DIR: &str = "contracts";

/// Beside a contract's kept instances, her statement's input once she
/// forced it closed and the hub answered: what her proof is of.
const STATEMENT: &str = "statement.input";

/// Of the instances opening `id` in `dir` keeps, how many are on disk:
/// those `Store::keep` finished writing. None if it keeps none there.
pub fn kept(dir: &Path, id: &[u8; 32]) -> usize {
    let Ok(entries) = std::fs::read_dir(dir.join(KEPT_DIR).join(crate::hex_id(id))) else {
        return 0;
    };
    let written = |e: &std::fs::DirEntry| {
        let name = e.file_name().to_string_lossy().into_owned();
        !name.ends_with(".new") && name != STATEMENT
    };
    entries.filter_map(Result::ok).filter(written).count()
}

/// A contract as a list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contract {
    pub id: [u8; 32],
    /// The hub's URL.
    pub hub: String,
    pub phase: Phase,
    /// Under the mock authority, for regtest only.
    pub mock: bool,
    /// Her coin F spends, and what it holds.
    pub deposit: OutPoint,
    pub value: u64,
    /// What opening it cost her, from her deposit: her reserve and the
    /// hub's fee.
    pub price: u64,
    /// The most it protects, in sat.
    pub cap: u32,
    /// The last block a recovery can assert in: it protects until then.
    pub assert_height: u32,
    /// The block its collateral returns to the hub from.
    pub close_height: u32,
    /// F's id, once she knows F.
    pub funding: Option<Txid>,
    /// Where F stands, as last seen once she signed it.
    pub fate: Option<Fate>,
    /// Why it was given up, short of her signing F.
    pub abandoned: Option<String>,
    pub created_at: i64,
}

impl Contract {
    /// Whether it protects notes now: F buried, its assertion deadline
    /// ahead of `tip`.
    pub fn live(&self, tip: u32) -> bool {
        self.fate == Some(Fate::Live) && tip < self.assert_height
    }

    /// Whether nothing is left to do for it: given up, F never to
    /// confirm, or past its close.
    pub fn over(&self, tip: u32) -> bool {
        self.abandoned.is_some() || self.fate == Some(Fate::Dead) || tip >= self.close_height
    }
}

/// The new contract's row.
pub(super) struct New<'a> {
    pub id: [u8; 32],
    pub hub: &'a str,
    pub mock: bool,
    pub deposit: OutPoint,
    pub value: u64,
    pub price: u64,
    pub cap: u32,
    pub assert_height: u32,
    pub close_height: u32,
    /// The chain's height as she started: no spend of F's inputs that
    /// matters is in a block before.
    pub since: u32,
    pub state: &'a [u8],
}

pub(super) struct Store {
    c: Connection,
    kept: PathBuf,
}

impl Store {
    /// The contracts in `dir`, if any were ever opened there.
    pub fn existing(dir: &Path) -> Result<Option<Self>> {
        if !dir.join("contracts.sqlite").exists() {
            return Ok(None);
        }
        Self::open(dir).map(Some)
    }

    /// The contracts in `dir`, made if none were.
    pub fn open(dir: &Path) -> Result<Self> {
        let c = crate::private_db(&dir.join("contracts.sqlite"))?;
        let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                c.execute_batch(SCHEMA)?;
                c.pragma_update(None, "user_version", 1)?;
            }
            1 | 2 => {}
            v => bail!("contracts.sqlite is of version {v}, newer than this wallet knows"),
        }
        Ok(Self {
            c,
            kept: dir.join(KEPT_DIR),
        })
    }

    #[cfg(test)]
    pub fn insert(&self, new: &New<'_>) -> Result<()> {
        insert(&self.c, new)
    }

    /// Insert a contract and `fees`, the most its hub may charge her, in
    /// the same write.
    pub fn insert_under(&self, new: &New<'_>, fees: &Fees) -> Result<()> {
        let t = rusqlite::Transaction::new_unchecked(&self.c, TransactionBehavior::Immediate)?;
        t.execute_batch(CEILINGS)?;
        insert(&t, new)?;
        t.execute(
            "INSERT INTO ceilings (id, fees, updated_at) VALUES (?1, ?2, ?3)",
            params![new.id, serde_json::to_string(fees)?, crate::now()],
        )?;
        t.commit()?;
        Ok(())
    }

    /// Whether any contract's ceilings were ever kept here.
    fn keeps_ceilings(&self) -> Result<bool> {
        let made: Option<i64> = self
            .c
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'ceilings'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(made.is_some())
    }

    /// The most the hub of each contract may charge her: none of a
    /// contract opened before these were kept, nor of one whose own
    /// cannot be read.
    pub fn ceilings(&self) -> Result<HashMap<[u8; 32], Fees>> {
        if !self.keeps_ceilings()? {
            return Ok(HashMap::new());
        }
        let mut stmt = self.c.prepare("SELECT id, fees FROM ceilings")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, [u8; 32]>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut all = HashMap::new();
        for row in rows {
            let (id, text) = row?;
            match serde_json::from_str(&text) {
                Ok(fees) => {
                    all.insert(id, fees);
                }
                Err(e) => tracing::warn!(
                    id = %crate::hex_id(&id),
                    "a contract's fee ceilings cannot be read, as if it kept none: {e}"
                ),
            }
        }
        Ok(all)
    }

    /// Raise the ceilings of each of `ids` that keeps some to `fees`, fee
    /// by fee: none lowered, and none made for a contract that keeps
    /// none. How many were raised.
    pub fn raise_ceilings(&self, ids: &[[u8; 32]], fees: &Fees) -> Result<usize> {
        if !self.keeps_ceilings()? {
            return Ok(0);
        }
        let t = rusqlite::Transaction::new_unchecked(&self.c, TransactionBehavior::Immediate)?;
        let mut raised = 0;
        for id in ids {
            let kept: Option<String> = t
                .query_row("SELECT fees FROM ceilings WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?;
            let Some(was) = kept.and_then(|text| serde_json::from_str::<Fees>(&text).ok()) else {
                continue;
            };
            let now = was.most(fees);
            if now == was {
                continue;
            }
            let (from, to) = (serde_json::to_string(&was)?, serde_json::to_string(&now)?);
            t.execute(
                "UPDATE ceilings SET fees = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, to, crate::now()],
            )?;
            tracing::info!(id = %crate::hex_id(id), %from, %to, "a contract's fee ceilings raised");
            raised += 1;
        }
        t.commit()?;
        Ok(raised)
    }

    /// Opening `id` after a step: its phase, F once known, its state.
    /// Irrevocable from [`Phase::Funded`] on, in the same write: her
    /// signature of F leaves only after.
    pub fn save(
        &self,
        id: &[u8; 32],
        phase: Phase,
        funding: Option<Txid>,
        state: &[u8],
    ) -> Result<()> {
        self.c.execute(
            "UPDATE contracts SET phase = ?2, funding = ?3, state = ?4, updated_at = ?5,
                                  irrevocable = max(irrevocable, ?6)
             WHERE id = ?1",
            params![
                id,
                phase as u8,
                funding.map(|t| serialize(&t)),
                state,
                crate::now(),
                phase == Phase::Funded
            ],
        )?;
        Ok(())
    }

    pub fn set_fate(&self, id: &[u8; 32], fate: Fate) -> Result<()> {
        self.c.execute(
            "UPDATE contracts SET fate = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, fate_text(fate), crate::now()],
        )?;
        Ok(())
    }

    /// Give up opening `id` short of her signing F, and why: never one
    /// irrevocable. Its kept instances go: no contract can need them.
    pub fn abandon(&self, id: &[u8; 32], reason: &str) -> Result<()> {
        let given_up = self.c.execute(
            "UPDATE contracts SET abandoned = ?2, updated_at = ?3
             WHERE id = ?1 AND abandoned IS NULL AND irrevocable = 0",
            params![id, reason, crate::now()],
        )?;
        if given_up == 1 {
            self.forget(id)?;
        }
        Ok(())
    }

    /// Remove the instances opening `id` kept, once it can never be a
    /// contract: given up, or its F never to confirm.
    pub fn forget(&self, id: &[u8; 32]) -> Result<()> {
        let dir = self.kept.join(crate::hex_id(id));
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("{}", dir.display()))
            }
            _ => Ok(()),
        }
    }

    /// The stored state of opening `id`, and the height she started at.
    pub fn state(&self, id: &[u8; 32]) -> Result<(Vec<u8>, u32)> {
        let row = self
            .c
            .query_row(
                "SELECT format, state, since FROM contracts WHERE id = ?1",
                [id],
                |r| Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match row {
            Some((FORMAT, state, since)) => Ok((state, since)),
            Some((format, ..)) => bail!("a contract stored in format {format}, unknown"),
            None => bail!("no contract {}", crate::hex_id(id)),
        }
    }

    /// Every contract, the newest first.
    pub fn list(&self) -> Result<Vec<Contract>> {
        let mut stmt = self.c.prepare(
            "SELECT id, hub, phase, mock, deposit, value, price, cap, assert_height,
                    close_height, funding, fate, abandoned, created_at
             FROM contracts ORDER BY created_at DESC, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, [u8; 32]>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u8>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                (
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, u32>(7)?,
                ),
                (r.get::<_, u32>(8)?, r.get::<_, u32>(9)?),
                r.get::<_, Option<Vec<u8>>>(10)?,
                r.get::<_, Option<String>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, i64>(13)?,
            ))
        })?;
        let mut all = Vec::new();
        for row in rows {
            let (
                id,
                hub,
                phase,
                mock,
                deposit,
                (value, price, cap),
                heights,
                funding,
                fate,
                abandoned,
                created_at,
            ) = row?;
            let phase = *Phase::ALL
                .get(usize::from(phase))
                .with_context(|| format!("a contract of phase {phase}, unknown"))?;
            all.push(Contract {
                id,
                hub,
                phase,
                mock,
                deposit: deserialize(&deposit)?,
                value: value as u64,
                price: price as u64,
                cap,
                assert_height: heights.0,
                close_height: heights.1,
                funding: funding.map(|t| deserialize(&t)).transpose()?,
                fate: fate.as_deref().map(text_fate).transpose()?,
                abandoned,
                created_at,
            });
        }
        Ok(all)
    }

    /// Keep instance `index` of assertion `stage` of opening `id`, on disk
    /// before her state names it.
    pub fn keep(&self, id: &[u8; 32], stage: usize, index: usize, bytes: &[u8]) -> Result<()> {
        let dir = self.kept.join(crate::hex_id(id));
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&dir)
            .with_context(|| format!("{}", dir.display()))?;
        let path = dir.join(format!("{stage}-{index}"));
        let aside = dir.join(format!("{stage}-{index}.new"));
        let mut file = std::fs::File::create(&aside)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&aside, &path)?;
        std::fs::File::open(&dir)?.sync_all()?;
        Ok(())
    }

    /// Instance `index` of assertion `stage` of opening `id`, as kept.
    pub fn kept_instance(&self, id: &[u8; 32], stage: usize, index: usize) -> Result<Vec<u8>> {
        let path = self
            .kept
            .join(crate::hex_id(id))
            .join(format!("{stage}-{index}"));
        std::fs::read(&path).with_context(|| format!("{}", path.display()))
    }

    /// Where her statement's input of contract `id` is kept, beside its
    /// instances.
    pub fn statement_path(&self, id: &[u8; 32]) -> std::path::PathBuf {
        self.kept.join(crate::hex_id(id)).join(STATEMENT)
    }

    /// Keep her statement's input of contract `id`, on disk whole before
    /// her state relies on it.
    pub fn keep_statement(&self, id: &[u8; 32], bytes: &[u8]) -> Result<()> {
        let path = self.statement_path(id);
        let aside = path.with_extension("new");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("{}", dir.display()))?;
        }
        let mut file =
            std::fs::File::create(&aside).with_context(|| format!("{}", aside.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&aside, &path)?;
        Ok(())
    }

    fn version(&self) -> Result<i64> {
        Ok(self
            .c
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    /// Start recovering contract `id`, at `stage` with `state`, made
    /// revocable: false if it is recovering already. The file of version 2
    /// from the first on, in the same write.
    pub fn start_recovery(
        &self,
        id: &[u8; 32],
        format: i64,
        stage: &str,
        state: &[u8],
    ) -> Result<bool> {
        let t = rusqlite::Transaction::new_unchecked(&self.c, TransactionBehavior::Immediate)?;
        let version: i64 = t.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            1 => {
                t.execute_batch(RECOVERIES)?;
                t.pragma_update(None, "user_version", 2)?;
            }
            2 => {}
            v => bail!("contracts.sqlite of version {v} keeps no recoveries"),
        }
        let known: Option<i64> = t
            .query_row("SELECT 1 FROM contracts WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        ensure!(known.is_some(), "no contract {}", crate::hex_id(id));
        let now = crate::now();
        let started = t.execute(
            "INSERT OR IGNORE INTO recoveries (id, format, stage, irrevocable, state, created_at,
                                               updated_at)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?5)",
            params![id, format, stage, state, now],
        )?;
        t.commit()?;
        Ok(started == 1)
    }

    /// Recovery `id` at stage `to` with `state`, from stage `from` and
    /// state `was` only; irrevocable from then on if `irrevocable`, in the
    /// same write. False if another step moved it on meanwhile, or it was
    /// given up (and started again) since `was` was read.
    pub fn save_recovery(
        &self,
        id: &[u8; 32],
        (from, was): (&str, &[u8]),
        to: &str,
        irrevocable: bool,
        state: &[u8],
    ) -> Result<bool> {
        let saved = self.c.execute(
            "UPDATE recoveries SET stage = ?4, state = ?5, irrevocable = max(irrevocable, ?6),
                                   error = NULL, updated_at = ?7
             WHERE id = ?1 AND stage = ?2 AND state = ?3",
            params![id, from, was, to, state, irrevocable, crate::now()],
        )?;
        Ok(saved == 1)
    }

    /// Recovery `id` at stage `to` whatever its stage, `error` why, only
    /// while revocable: giving it up, before its notes are given back.
    /// False if it is irrevocable, or none is.
    pub fn revoke_recovery(&self, id: &[u8; 32], to: &str, error: Option<&str>) -> Result<bool> {
        if self.version()? < 2 {
            return Ok(false);
        }
        let revoked = self.c.execute(
            "UPDATE recoveries SET stage = ?2, error = ?3, updated_at = ?4
             WHERE id = ?1 AND irrevocable = 0",
            params![id, to, error, crate::now()],
        )?;
        Ok(revoked == 1)
    }

    /// What stopped recovery `id`'s last step, or none.
    pub fn recovery_error(&self, id: &[u8; 32], error: Option<&str>) -> Result<()> {
        self.c.execute(
            "UPDATE recoveries SET error = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, error, crate::now()],
        )?;
        Ok(())
    }

    /// Forget recovery `id`, given up: never one irrevocable. False if it
    /// is, or none was.
    pub fn drop_recovery(&self, id: &[u8; 32]) -> Result<bool> {
        if self.version()? < 2 {
            return Ok(false);
        }
        let dropped = self.c.execute(
            "DELETE FROM recoveries WHERE id = ?1 AND irrevocable = 0",
            [id],
        )?;
        Ok(dropped == 1)
    }

    /// Recovery `id`, if she forced it closed.
    pub fn recovery(&self, id: &[u8; 32]) -> Result<Option<RecoveryRow>> {
        Ok(self.recoveries()?.into_iter().find(|r| &r.id == id))
    }

    /// Every recovery, the oldest first: none in a file of version 1.
    pub fn recoveries(&self) -> Result<Vec<RecoveryRow>> {
        if self.version()? < 2 {
            return Ok(Vec::new());
        }
        let mut stmt = self.c.prepare(
            "SELECT id, format, stage, irrevocable, state, error, created_at
             FROM recoveries ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RecoveryRow {
                id: r.get(0)?,
                format: r.get(1)?,
                stage: r.get(2)?,
                irrevocable: r.get(3)?,
                state: r.get(4)?,
                error: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

fn insert(c: &Connection, new: &New<'_>) -> Result<()> {
    let now = crate::now();
    c.execute(
        "INSERT INTO contracts (id, hub, format, phase, mock, irrevocable, deposit, value, price, cap,
                                assert_height, close_height, since, state, created_at,
                                updated_at)
         VALUES (?1, ?2, ?3, 0, ?4, 0, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
        params![
            new.id,
            new.hub,
            FORMAT,
            new.mock,
            serialize(&new.deposit),
            new.value as i64,
            new.price as i64,
            new.cap,
            new.assert_height,
            new.close_height,
            new.since,
            new.state,
            now
        ],
    )?;
    Ok(())
}

fn fate_text(fate: Fate) -> &'static str {
    match fate {
        Fate::Live => "live",
        Fate::Dead => "dead",
        Fate::Open => "open",
    }
}

fn text_fate(text: &str) -> Result<Fate> {
    Ok(match text {
        "live" => Fate::Live,
        "dead" => Fate::Dead,
        "open" => Fate::Open,
        other => bail!("a fate {other}, unknown"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(id: u8, deposit: OutPoint) -> New<'static> {
        New {
            id: [id; 32],
            hub: "http://hub.example",
            mock: true,
            deposit,
            value: 60_000,
            price: 23_000,
            cap: 4_096,
            assert_height: 360,
            close_height: 500,
            since: 200,
            state: b"state",
        }
    }

    fn coin(vout: u32) -> OutPoint {
        OutPoint {
            txid: Txid::from_byte_array([9; 32]),
            vout,
        }
    }

    use bitcoin::hashes::Hash as _;

    #[test]
    fn nothing_is_made_until_a_contract_opens() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Store::existing(dir.path()).unwrap().is_none());
        assert!(!dir.path().join("contracts.sqlite").exists());
        let store = Store::open(dir.path()).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(Store::existing(dir.path()).unwrap().is_some());
    }

    #[test]
    fn contracts_list_as_stored_the_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert(&new(1, coin(0))).unwrap();
        store.insert(&new(2, coin(1))).unwrap();
        let f = Txid::from_byte_array([5; 32]);
        store
            .save(&[1; 32], Phase::Funded, Some(f), b"funded")
            .unwrap();
        store.set_fate(&[1; 32], Fate::Live).unwrap();
        let all = store.list().unwrap();
        assert_eq!(all.len(), 2);
        let first = all.iter().find(|c| c.id == [1; 32]).unwrap();
        assert_eq!(first.phase, Phase::Funded);
        assert_eq!((first.funding, first.fate), (Some(f), Some(Fate::Live)));
        assert_eq!(
            (first.deposit, first.value, first.price),
            (coin(0), 60_000, 23_000)
        );
        assert_eq!((first.assert_height, first.close_height), (360, 500));
        assert!(first.live(359) && !first.live(360) && !first.over(499) && first.over(500));
        assert_eq!(store.state(&[1; 32]).unwrap(), (b"funded".to_vec(), 200));
        let second = all.iter().find(|c| c.id == [2; 32]).unwrap();
        assert_eq!((second.phase, second.fate), (Phase::Started, None));
    }

    #[test]
    fn a_coin_is_the_deposit_of_one_live_contract_at_most() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert(&new(1, coin(0))).unwrap();
        assert!(store.insert(&new(2, coin(0))).is_err());
        store.abandon(&[1; 32], "refused").unwrap();
        store.insert(&new(2, coin(0))).unwrap();
        store
            .save(&[2; 32], Phase::Funded, None, b"funded")
            .unwrap();
        store.set_fate(&[2; 32], Fate::Dead).unwrap();
        store.insert(&new(3, coin(0))).unwrap();
    }

    #[test]
    fn a_contract_she_signed_is_never_given_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert(&new(1, coin(0))).unwrap();
        store.keep(&[1; 32], 0, 2, b"instance").unwrap();
        store
            .save(&[1; 32], Phase::Funded, None, b"funded")
            .unwrap();
        // A later step never takes it back.
        store
            .save(&[1; 32], Phase::Signing, None, b"signing")
            .unwrap();
        store.abandon(&[1; 32], "refused").unwrap();
        assert_eq!(store.list().unwrap()[0].abandoned, None);
        let kept = dir.path().join("contracts").join(crate::hex_id(&[1; 32]));
        assert_eq!(std::fs::read(kept.join("0-2")).unwrap(), b"instance");
    }

    #[test]
    fn what_a_contract_given_up_kept_goes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert(&new(1, coin(0))).unwrap();
        store.keep(&[1; 32], 1, 3, b"instance").unwrap();
        let kept = dir.path().join("contracts").join(crate::hex_id(&[1; 32]));
        assert!(kept.join("1-3").exists() && !kept.join("1-3.new").exists());
        store.abandon(&[1; 32], "refused").unwrap();
        assert_eq!(
            store.list().unwrap()[0].abandoned.as_deref(),
            Some("refused")
        );
        assert!(!kept.exists());
        // Again, or with nothing kept: no error.
        store.forget(&[1; 32]).unwrap();
    }

    fn user_version(dir: &Path) -> i64 {
        let c = Connection::open(dir.join("contracts.sqlite")).unwrap();
        c.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_file_stays_of_version_1_until_she_forces_a_contract_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert(&new(1, coin(0))).unwrap();
        store
            .save(&[1; 32], Phase::Funded, None, b"funded")
            .unwrap();
        store.set_fate(&[1; 32], Fate::Live).unwrap();
        let before = store.list().unwrap();
        drop(store);
        // Opened, listed, read: as it was.
        let store = Store::open(dir.path()).unwrap();
        assert!(store.recoveries().unwrap().is_empty());
        assert!(store.recovery(&[1; 32]).unwrap().is_none());
        assert!(!store.drop_recovery(&[1; 32]).unwrap());
        assert_eq!(user_version(dir.path()), 1);

        // Her first: version 2, the contracts as they were.
        assert!(store.start_recovery(&[1; 32], 1, "claiming", b"s").unwrap());
        assert_eq!(user_version(dir.path()), 2);
        assert_eq!(store.list().unwrap(), before);
        assert_eq!(store.state(&[1; 32]).unwrap(), (b"funded".to_vec(), 200));
        let row = store.recovery(&[1; 32]).unwrap().unwrap();
        assert_eq!((row.format, row.stage.as_str()), (1, "claiming"));
        assert!(!row.irrevocable && row.state == b"s" && row.error.is_none());
        // Again: refused, as it was.
        assert!(!store.start_recovery(&[1; 32], 1, "claiming", b"t").unwrap());
        assert_eq!(store.recovery(&[1; 32]).unwrap().unwrap().state, b"s");
        drop(store);

        // Reopened, of version 2.
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.recoveries().unwrap().len(), 1);
        // Of a contract it does not know: refused.
        assert!(store.start_recovery(&[2; 32], 1, "claiming", b"s").is_err());
    }

    fn fees(base: u64, ppm: u64, output_msat: u64) -> Fees {
        let fee = licash_api::Fee { base, ppm };
        Fees {
            spend: fee,
            lock: fee,
            refund: fee,
            output_msat,
        }
    }

    #[test]
    fn ceilings_are_kept_with_a_contract_and_leave_the_file_as_older_builds_open_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        // Opened before they were kept: none, nothing made, none raised.
        store.insert(&new(1, coin(0))).unwrap();
        assert!(store.ceilings().unwrap().is_empty());
        assert!(!store.keeps_ceilings().unwrap());
        assert_eq!(store.raise_ceilings(&[[1; 32]], &fees(9, 9, 9)).unwrap(), 0);
        assert!(!store.keeps_ceilings().unwrap());

        let agreed = fees(1, 100, 100);
        store.insert_under(&new(2, coin(1)), &agreed).unwrap();
        assert_eq!(
            store.ceilings().unwrap(),
            HashMap::from([([2; 32], agreed)])
        );
        // Neither its deposit taken nor its ceilings kept, if it is refused.
        assert!(store
            .insert_under(&new(3, coin(1)), &fees(0, 0, 0))
            .is_err());
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(store.ceilings().unwrap().len(), 1);
        // Of the version it was, and of the next at her first recovery.
        assert_eq!(user_version(dir.path()), 1);
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert!(store.start_recovery(&[2; 32], 1, "claiming", b"s").unwrap());
        assert_eq!(user_version(dir.path()), 2);
        assert_eq!(store.ceilings().unwrap()[&[2; 32]], agreed);
        store.insert_under(&new(4, coin(2)), &agreed).unwrap();
        assert_eq!(store.ceilings().unwrap().len(), 2);
    }

    #[test]
    fn ceilings_are_raised_fee_by_fee_and_never_lowered() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert(&new(1, coin(0))).unwrap();
        store
            .insert_under(&new(2, coin(1)), &fees(1, 100, 100))
            .unwrap();
        store
            .insert_under(&new(3, coin(2)), &fees(5, 50, 0))
            .unwrap();
        store
            .insert_under(&new(4, coin(3)), &fees(0, 0, 0))
            .unwrap();
        // Of those named only, and only those it raises: the one that
        // kept none keeps none.
        let all = [[1; 32], [2; 32], [3; 32]];
        assert_eq!(store.raise_ceilings(&all, &fees(2, 80, 100)).unwrap(), 2);
        let kept = store.ceilings().unwrap();
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[&[2; 32]], fees(2, 100, 100));
        assert_eq!(kept[&[3; 32]], fees(5, 80, 100));
        assert_eq!(kept[&[4; 32]], fees(0, 0, 0));
        assert_eq!(store.raise_ceilings(&all, &fees(2, 80, 100)).unwrap(), 0);
        assert_eq!(store.raise_ceilings(&all, &fees(0, 0, 0)).unwrap(), 0);
        assert_eq!(store.ceilings().unwrap(), kept);
    }

    #[test]
    fn ceilings_that_cannot_be_read_are_as_none_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .insert_under(&new(1, coin(0)), &fees(1, 100, 100))
            .unwrap();
        store
            .insert_under(&new(2, coin(1)), &fees(1, 100, 100))
            .unwrap();
        store
            .c
            .execute(
                "UPDATE ceilings SET fees = 'not fees' WHERE id = ?1",
                [[1u8; 32]],
            )
            .unwrap();
        let kept = store.ceilings().unwrap();
        assert_eq!(kept.len(), 1);
        assert!(kept.contains_key(&[2; 32]));
        // Nor made up by her agreeing to more.
        let both = [[1; 32], [2; 32]];
        assert_eq!(store.raise_ceilings(&both, &fees(2, 100, 100)).unwrap(), 1);
        assert_eq!(store.ceilings().unwrap().len(), 1);
    }

    #[test]
    fn a_file_of_a_version_newer_than_it_knows_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        drop(Store::open(dir.path()).unwrap());
        let c = Connection::open(dir.path().join("contracts.sqlite")).unwrap();
        c.pragma_update(None, "user_version", 3).unwrap();
        drop(c);
        let refused = Store::open(dir.path()).err().unwrap();
        assert!(format!("{refused:#}").contains("version 3"), "{refused:#}");
    }

    #[test]
    fn a_recovery_moves_on_one_stage_at_a_time_and_is_dropped_only_while_revocable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert(&new(1, coin(0))).unwrap();
        store.start_recovery(&[1; 32], 1, "a", b"0").unwrap();
        store.recovery_error(&[1; 32], Some("offline")).unwrap();
        let save = |from: (&str, &[u8]), to: &str, irrevocable: bool, state: &[u8]| {
            store
                .save_recovery(&[1; 32], from, to, irrevocable, state)
                .unwrap()
        };
        assert!(save(("a", b"0"), "b", false, b"1"));
        // From a stage it left, or a state it no longer has: not saved.
        assert!(!save(("a", b"0"), "c", false, b"2"));
        assert!(!save(("b", b"0"), "c", false, b"2"));
        let row = store.recovery(&[1; 32]).unwrap().unwrap();
        assert_eq!((row.stage.as_str(), row.state.as_slice()), ("b", &b"1"[..]));
        assert_eq!(row.error, None);

        assert!(save(("b", b"1"), "c", true, b"2"));
        // Never revocable again.
        assert!(save(("c", b"2"), "d", false, b"3"));
        assert!(store.recovery(&[1; 32]).unwrap().unwrap().irrevocable);
        assert!(!store.revoke_recovery(&[1; 32], "x", None).unwrap());
        assert!(!store.drop_recovery(&[1; 32]).unwrap());
        assert_eq!(store.recovery(&[1; 32]).unwrap().unwrap().stage, "d");

        store.insert(&new(2, coin(1))).unwrap();
        store.start_recovery(&[2; 32], 1, "a", b"0").unwrap();
        let why = Some("why");
        assert!(store.revoke_recovery(&[2; 32], "giving-up", why).unwrap());
        let row = store.recovery(&[2; 32]).unwrap().unwrap();
        assert_eq!(
            (row.stage.as_str(), row.error.as_deref()),
            ("giving-up", why)
        );
        // A step that read it before: not saved.
        assert!(!store
            .save_recovery(&[2; 32], ("a", b"0"), "b", false, b"1")
            .unwrap());
        assert!(store.drop_recovery(&[2; 32]).unwrap());
        assert!(store.recovery(&[2; 32]).unwrap().is_none());
        // And started again.
        assert!(store.start_recovery(&[2; 32], 1, "a", b"0").unwrap());
    }

    #[test]
    fn the_instances_an_opening_kept_are_counted_once_written_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert(&new(1, coin(0))).unwrap();
        assert_eq!(kept(dir.path(), &[1; 32]), 0);
        store.keep(&[1; 32], 0, 1, b"instance").unwrap();
        store.keep(&[1; 32], 1, 3, b"instance").unwrap();
        // One being written, as a stop leaves it: not yet.
        let aside = dir.path().join("contracts").join(crate::hex_id(&[1; 32]));
        std::fs::write(aside.join("1-0.new"), b"inst").unwrap();
        assert_eq!(kept(dir.path(), &[1; 32]), 2);
        assert_eq!(kept(dir.path(), &[2; 32]), 0);
        store.forget(&[1; 32]).unwrap();
        assert_eq!(kept(dir.path(), &[1; 32]), 0);
    }
}
