//! The holder's contracts, in `contracts.sqlite` of the wallet's
//! directory, and the instances each keeps, in `contracts/` beside it:
//! each opening as its machine stored itself at its last step, with what
//! a list shows of it. Neither is made until a first contract opens.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::{OutPoint, Txid};
use licash_contract::opening::{Fate, Phase};
use rusqlite::{params, Connection, OptionalExtension};

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
            1 => {}
            v => bail!("contracts.sqlite is of version {v}, newer than this wallet knows"),
        }
        Ok(Self {
            c,
            kept: dir.join("contracts"),
        })
    }

    pub fn insert(&self, new: &New<'_>) -> Result<()> {
        let now = crate::now();
        self.c.execute(
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
}
