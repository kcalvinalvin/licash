//! The Licash hub engine: a hub's operations over its durable ledger.
//!
//! A hub holds one secret, a 32-byte seed in a file of its own, and derives
//! from it its issuer keysets ([`IssuerKeys::from_seed`]), the unlock of
//! each grant it offers and its own address, at which spends pay its fees.
//! The database holds no secret: what the hub offered, which inputs it
//! retired, which outputs it issued and what it answered. Every operation
//! commits one transaction before it replies.
//!
//! Unlink nonces derive from a key drawn at each start and never stored,
//! so no restored or copied database can make the hub answer twice under
//! one nonce. Outputs issued before a restart can therefore no longer be
//! unlinked ([`Refusal::Expired`]); their holders keep the value and can
//! swap them for fresh outputs.

mod store;

use std::{collections::BTreeMap, io::Write, path::Path};

use licash::{
    address::{Address, OwnerKeys},
    authorization::{Grant, PreparedGrant, SignedProposal, Unlock},
    crypto::{tagged_hash, Point},
    note::{IssuerKeys, Keyset, Note},
    spend::{Output, Proposal, ProposalId},
    unlink::{self, Answer, Answered, Request},
};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use store::{now, setup_io, Store};

/// The largest denomination exponent a hub issues: its ledger keeps
/// denominations as SQLite integers.
pub const MAX_POW: u32 = 62;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request is valid but the hub will not serve it.
    #[error("refused: {0}")]
    Refused(#[from] Refusal),
    /// The request does not verify.
    #[error("invalid: {0}")]
    Invalid(#[from] licash::Error),
    /// The hub's files are missing, exist already or do not match.
    #[error("setup: {0}")]
    Setup(String),
    /// The database changed under the running hub, which serves nothing
    /// more. Restart it once the database is the one it should be.
    #[error("halted: the hub database changed under it")]
    Halted,
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("unknown keyset")]
    UnknownKeyset,
    #[error("timed spends are not served")]
    Timed,
    #[error("an input is already spent")]
    Spent,
    #[error("an output is already issued")]
    Reissued,
    #[error("fee {paid} below {due}")]
    Fee { due: u64, paid: u64 },
    #[error("no offer for this proposal")]
    UnknownOffer,
    #[error("not issued by this hub")]
    NotIssued,
    #[error("already unlinked under another request")]
    Unlinked,
    #[error("unlinking closed by a hub restart: swap the note instead")]
    Expired,
}

/// A fee: `base` plus `ppm` parts per million of the amount moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fee {
    pub base: u64,
    pub ppm: u64,
}

impl Fee {
    /// The fee on moving `amount`, rounded up.
    pub fn due(&self, amount: u64) -> u64 {
        let rate = (u128::from(amount) * u128::from(self.ppm)).div_ceil(1_000_000);
        u64::try_from(rate).map_or(u64::MAX, |rate| rate.saturating_add(self.base))
    }
}

/// What the hub charges. A spend pays its fee in outputs to the hub's
/// address ([`Info::address`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub spend: Fee,
}

#[derive(Clone, Debug)]
pub struct Info {
    pub keysets: Vec<Keyset>,
    pub address: Address,
    pub policy: Policy,
}

/// Something a hub operator may want to hear about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    /// The hub started. Outputs issued before can no longer be unlinked.
    Started { boot: String },
    /// A spend was released: `amount` moved, `fee` of it to the hub.
    Released {
        proposal: String,
        amount: u64,
        fee: u64,
    },
    /// The owners of inputs another spend retired signed `proposal`.
    DoubleSpend { proposal: String, spent_by: String },
}

impl Notice {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Started { .. } => "started",
            Self::Released { .. } => "released",
            Self::DoubleSpend { .. } => "double_spend",
        }
    }
}

/// A [`Notice`] in the hub's outbox, in order of `seq`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    /// Unix seconds.
    pub at: u64,
    pub notice: Notice,
}

pub struct Hub {
    store: Store,
    keysets: BTreeMap<[u8; 32], (IssuerKeys, Keyset)>,
    owner: OwnerKeys,
    unlock_secret: [u8; 32],
    /// This process's public boot id and its key, never stored.
    boot: [u8; 16],
    boot_key: [u8; 32],
    policy: Policy,
}

impl Hub {
    /// Create a hub: a fresh seed at `seed` and its ledger at `db`, with
    /// keyset 0 of denominations 1 to `2^max_pow`. Neither may exist: a
    /// ledger is never made again for a seed, whose spent notes it would
    /// accept as unspent.
    pub fn init(seed: &Path, db: &Path, max_pow: u32) -> Result<(), Error> {
        if max_pow > MAX_POW {
            return Err(Error::Setup(format!("max_pow above {MAX_POW}")));
        }
        if db.exists() {
            return Err(Error::Setup(format!("{} exists", db.display())));
        }
        let mut secret = [0; 32];
        OsRng.fill_bytes(&mut secret);
        let keys = IssuerKeys::from_seed(&secret, 0, max_pow)?;
        write_seed(seed, &secret)?;
        let created = Store::create(db, |tx| {
            tx.execute(
                "INSERT INTO keysets (id, idx, max_pow, created_at) VALUES (?1, 0, ?2, ?3)",
                params![keys.id(), max_pow, now()],
            )?;
            Ok(())
        });
        if created.is_err() {
            // Nothing was issued under this seed.
            let _ = std::fs::remove_file(seed);
        }
        created
    }

    /// The hub that [`Hub::init`] created at `seed` and `db`.
    pub fn open(seed: &Path, db: &Path, policy: Policy) -> Result<Self, Error> {
        let secret = read_seed(seed)?;
        let store = Store::open(db)?;
        let rows: Vec<([u8; 32], u32, u32)> = store.read(|c| {
            let mut rows = c.prepare("SELECT id, idx, max_pow FROM keysets ORDER BY idx")?;
            let rows = rows
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        })?;
        let mut keysets = BTreeMap::new();
        for (id, index, max_pow) in rows {
            let keys = IssuerKeys::from_seed(&secret, index, max_pow)?;
            if keys.id() != &id {
                return Err(Error::Setup("the seed does not match the database".into()));
            }
            let keyset = keys.keyset()?;
            keysets.insert(id, (keys, keyset));
        }
        if keysets.is_empty() {
            return Err(Error::Setup("the database has no keyset".into()));
        }
        let (mut boot, mut drawn) = ([0; 16], [0; 32]);
        OsRng.fill_bytes(&mut boot);
        OsRng.fill_bytes(&mut drawn);
        let hub = Self {
            store,
            keysets,
            owner: OwnerKeys::from_seed(&tagged_hash("licash-hub/owner", &[&secret]))?,
            unlock_secret: tagged_hash("licash-hub/unlock", &[&secret]),
            boot,
            boot_key: tagged_hash("licash-hub/boot", &[&secret, &drawn]),
            policy,
        };
        let boot = hex::encode(boot);
        tracing::info!(keysets = hub.keysets.len(), %boot, "hub started");
        hub.store
            .write(|tx| notify(tx, &Notice::Started { boot }))?;
        Ok(hub)
    }

    pub fn info(&self) -> Info {
        Info {
            keysets: self.keysets.values().map(|(_, k)| k.clone()).collect(),
            address: self.owner.address(),
            policy: self.policy,
        }
    }

    /// The hub's keyset `id`, to decode requests under.
    pub fn keyset(&self, id: &[u8; 32]) -> Option<&Keyset> {
        self.keysets.get(id).map(|(_, keyset)| keyset)
    }

    /// The events after `seq`, oldest first, at most `limit`.
    pub fn events(&self, after: u64, limit: u32) -> Result<Vec<Event>, Error> {
        self.store.read(|c| {
            let mut rows =
                c.prepare("SELECT seq, at, json FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
            let rows = rows.query_map(params![sql(after), limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            rows.map(|row| {
                let (seq, at, json) = row?;
                let notice = serde_json::from_str(&json)
                    .map_err(|e| Error::Setup(format!("event {seq}: {e}")))?;
                Ok(Event {
                    seq: seq as u64,
                    at: at as u64,
                    notice,
                })
            })
            .collect()
        })
    }

    fn keys(&self, id: &[u8; 32]) -> Result<(&IssuerKeys, &Keyset), Error> {
        let (keys, keyset) = self.keysets.get(id).ok_or(Refusal::UnknownKeyset)?;
        Ok((keys, keyset))
    }

    fn unlock(&self, keyset: &[u8; 32], proposal: &ProposalId, salt: &[u8; 32]) -> Unlock {
        tagged_hash(
            "licash-hub/unlock",
            &[&self.unlock_secret, keyset, proposal.as_bytes(), salt],
        )
    }

    /// The secret of the unlink nonces of `proposal`'s outputs, for this
    /// process only.
    fn nonce_secret(&self, proposal: &[u8; 32]) -> [u8; 32] {
        tagged_hash("licash-hub/unlink-nonce", &[&self.boot_key, proposal])
    }
}

fn notify(tx: &Transaction<'_>, notice: &Notice) -> Result<(), Error> {
    let json = serde_json::to_string(notice).expect("notices serialize");
    tx.execute(
        "INSERT INTO events (at, kind, json) VALUES (?1, ?2, ?3)",
        params![now(), notice.kind(), json],
    )?;
    Ok(())
}

/// `value` as an SQLite integer. Denominations fit by [`MAX_POW`].
fn sql(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn write_seed(path: &Path, seed: &[u8; 32]) -> Result<(), Error> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(|e| setup_io(path, e))?;
    file.write_all(format!("{}\n", hex::encode(seed)).as_bytes())?;
    file.sync_all()?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

fn read_seed(path: &Path) -> Result<[u8; 32], Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Setup(format!("seed {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            tracing::warn!(path = %path.display(), "the seed is readable by others than its owner");
        }
    }
    let mut seed = [0; 32];
    hex::decode_to_slice(text.trim(), &mut seed)
        .map_err(|_| Error::Setup(format!("seed {}: not 32 hex bytes", path.display())))?;
    Ok(seed)
}

/// Spends and unlinking.
impl Hub {
    /// The grant for `proposal`, spending `notes`: the hub's offer to issue
    /// its outputs once their owners authorize it ([`Hub::release`]). The
    /// same proposal gets the same offer.
    pub fn offer(&self, proposal: &Proposal, notes: &[Note]) -> Result<Grant, Error> {
        let (keys, keyset) = self.keys(proposal.keyset_id())?;
        if proposal.lock().is_some() {
            return Err(Refusal::Timed.into());
        }
        let ys = proposal.verify(keyset, notes)?;
        let (amount, fee) = self.charge(proposal)?;
        let due = self.policy.spend.due(amount);
        if fee < due {
            return Err(Refusal::Fee { due, paid: fee }.into());
        }
        let id = proposal.id();
        let mut salt = [0; 32];
        OsRng.fill_bytes(&mut salt);
        let unlock = self.unlock(keyset.id(), &id, &salt);
        let grant =
            PreparedGrant::derive(keys, proposal, unlock, &self.nonce_secret(id.as_bytes()))?
                .offer();
        let offer = grant.to_bytes()?;
        self.store.write(|tx| {
            for y in &ys {
                let spent: Option<[u8; 32]> = tx
                    .query_row("SELECT proposal FROM spent WHERE y = ?1", [&y.0], |r| {
                        r.get(0)
                    })
                    .optional()?;
                if spent.is_some_and(|by| &by != id.as_bytes()) {
                    return Err(Refusal::Spent.into());
                }
            }
            let offered: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT offer FROM proposals WHERE id = ?1",
                    [id.as_bytes()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(offered) = offered {
                return Ok(Grant::from_bytes(&offered)?);
            }
            for output in proposal.outputs() {
                if issued(tx, keyset.id(), output)? {
                    return Err(Refusal::Reissued.into());
                }
            }
            tx.execute(
                "INSERT INTO proposals (id, keyset, offer, salt, boot, offered_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id.as_bytes(), keyset.id(), offer, salt, self.boot, now()],
            )?;
            Ok(grant)
        })
    }

    /// The unlock of the offer `signed` authorizes, once the owners of
    /// `notes` signed it: their inputs are retired and its outputs issued
    /// first. A released spend gets its unlock again.
    pub fn release(&self, signed: &SignedProposal, notes: &[Note]) -> Result<Unlock, Error> {
        let proposal = signed.proposal();
        let (_, keyset) = self.keys(proposal.keyset_id())?;
        let id = proposal.id();
        let offered: Option<(Vec<u8>, [u8; 32], [u8; 16])> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT offer, salt, boot FROM proposals WHERE id = ?1",
                [id.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
        })?;
        let (offer, salt, boot) = offered.ok_or(Refusal::UnknownOffer)?;
        let unlock = self.unlock(keyset.id(), &id, &salt);
        Grant::from_bytes(&offer)?.verify_release(keyset, notes, signed, &unlock)?;
        let ys = proposal
            .inputs()
            .iter()
            .map(|message| message.identifier())
            .collect::<Result<Vec<Point>, _>>()?;
        let (amount, fee) = self.charge(proposal)?;
        let income = self
            .owner
            .scan_key()
            .receive(proposal)?
            .iter()
            .map(|r| {
                let index = r.position as usize;
                let note =
                    signed.open_granted_output(keyset, notes, index, &r.derivation, &unlock)?;
                Ok((r.position, note))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let mut spent_by = None;
        let released = self.store.write(|tx| {
            let released: Option<i64> = tx.query_row(
                "SELECT released_at FROM proposals WHERE id = ?1",
                [id.as_bytes()],
                |r| r.get(0),
            )?;
            if released.is_some() {
                return Ok(false);
            }
            for (position, (y, note)) in (0i64..).zip(ys.iter().zip(notes)) {
                let inserted = tx.execute(
                    "INSERT INTO spent (y, proposal, position, note) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT DO NOTHING",
                    params![&y.0, id.as_bytes(), position, note.to_bytes()],
                )?;
                if inserted == 0 {
                    spent_by = Some(tx.query_row(
                        "SELECT proposal FROM spent WHERE y = ?1",
                        [&y.0],
                        |r| r.get::<_, [u8; 32]>(0),
                    )?);
                    return Err(Refusal::Spent.into());
                }
            }
            for (position, output) in (0i64..).zip(proposal.outputs()) {
                let inserted = tx.execute(
                    "INSERT INTO issued (keyset, denom, blinded, commitment, proposal, position, boot)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT DO NOTHING",
                    params![
                        keyset.id(),
                        sql(output.denomination()),
                        &output.blinded().0,
                        output.unlink_commitment(),
                        id.as_bytes(),
                        position,
                        boot,
                    ],
                )?;
                if inserted == 0 {
                    return Err(Refusal::Reissued.into());
                }
            }
            for (position, note) in &income {
                tx.execute(
                    "INSERT INTO income (proposal, position, denom, note) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        id.as_bytes(),
                        position,
                        sql(note.message().denomination()),
                        note.to_bytes()
                    ],
                )?;
            }
            tx.execute(
                "UPDATE proposals SET signed = ?2, released_at = ?3 WHERE id = ?1",
                params![id.as_bytes(), signed.to_bytes(), now()],
            )?;
            let proposal = hex::encode(id.as_bytes());
            notify(tx, &Notice::Released { proposal, amount, fee })?;
            Ok(true)
        });
        if let Some(by) = spent_by {
            let (proposal, spent_by) = (hex::encode(id.as_bytes()), hex::encode(by));
            tracing::warn!(%proposal, %spent_by, "double spend refused");
            self.store
                .write(|tx| notify(tx, &Notice::DoubleSpend { proposal, spent_by }))?;
        }
        if released? {
            tracing::info!(proposal = %hex::encode(id.as_bytes()), amount, fee, "released");
        }
        Ok(unlock)
    }

    /// The answer to an unlink request for an output this hub issued since
    /// it started, or the same answer again for the same request.
    pub fn unlink(&self, request: &Request) -> Result<Answer, Error> {
        let (keys, keyset) = self.keys(request.keyset())?;
        keyset.key(request.denomination())?;
        let key = params![
            keyset.id(),
            sql(request.denomination()),
            &request.blinded().0
        ];
        self.store.write(|tx| {
            type Row = ([u8; 32], [u8; 32], [u8; 16], Option<Vec<u8>>);
            let row: Option<Row> = tx
                .query_row(
                    "SELECT commitment, proposal, boot, answered FROM issued
                     WHERE keyset = ?1 AND denom = ?2 AND blinded = ?3",
                    key,
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let (commitment, proposal, boot, answered) = row.ok_or(Refusal::NotIssued)?;
            if let Some(answered) = answered {
                return Answered::from_bytes(&answered)?
                    .replay(request)
                    .map_err(|_| Refusal::Unlinked.into());
            }
            if boot != self.boot {
                return Err(Refusal::Expired.into());
            }
            let output = Output::new(request.denomination(), request.blinded(), commitment)?;
            let answered = unlink::Nonce::derive(&self.nonce_secret(&proposal), keyset, &output)?
                .answer(keys, request)?;
            let updated = tx.execute(
                "UPDATE issued SET answered = ?4
                 WHERE keyset = ?1 AND denom = ?2 AND blinded = ?3 AND answered IS NULL",
                params![
                    keyset.id(),
                    sql(request.denomination()),
                    &request.blinded().0,
                    answered.to_bytes()
                ],
            )?;
            if updated != 1 {
                return Err(Error::Halted);
            }
            Ok(answered.answer())
        })
    }

    /// The amount `proposal` moves and the part of it paid to the hub.
    fn charge(&self, proposal: &Proposal) -> Result<(u64, u64), Error> {
        let amount = proposal
            .inputs()
            .iter()
            .fold(0u64, |sum, m| sum.saturating_add(m.denomination()));
        let fee = self
            .owner
            .scan_key()
            .receive(proposal)?
            .iter()
            .fold(0u64, |sum, r| {
                sum.saturating_add(r.derivation.message().denomination())
            });
        Ok((amount, fee))
    }
}

fn issued(tx: &Transaction<'_>, keyset: &[u8; 32], output: &Output) -> Result<bool, Error> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM issued WHERE keyset = ?1 AND denom = ?2 AND blinded = ?3",
            params![keyset, sql(output.denomination()), &output.blinded().0],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}
