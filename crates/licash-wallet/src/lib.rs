//! A Licash wallet: an owner's notes at the hubs it trusts, and the
//! payments it makes and takes, in one SQLite file.
//!
//! A payment is recorded before its hub hears of it, and each step before
//! the next is asked for, so a wallet stopped at any point finishes or
//! gives it up on [`Wallet::resume`]: a hub answers the same offer and the
//! same release again. A send its hub refuses before it is signed is
//! given up and its notes freed; a signed one is only retried, unless its
//! hub shows it can never be released.
//!
//! The wallet reaches hubs through the application's [`Transport`]: HTTP
//! over TCP or Tor, or a test. Its seed alone does not restore its notes:
//! back up its file.

mod coins;
mod store;

use std::{cmp::Reverse, future::Future, path::Path};

use licash::{
    address::{Address, Ephemeral, OwnerKeys},
    authorization::{Authorization, Grant, SignedProposal, Unlock},
    claim::Branch,
    crypto::Point,
    note::{Derivation, Keyset, Note},
    spend::{Output, Proposal, ProposalId},
    unlink::Session,
};
use licash_api::{
    Code, Info, Offer, Offered, Payment, Problem, Release, Released, Unlink, Unlinked,
};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{de::DeserializeOwned, Serialize};

pub use coins::MAX_INPUTS;
use store::{now, sql, Store};

/// A hub's answer: its HTTP status and body.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// How the wallet reaches its hubs.
pub trait Transport: Send + Sync {
    /// GET `url`, or POST `body` to it as JSON. `Err` when no answer came,
    /// so that the request may or may not have taken effect.
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl Future<Output = Result<Response, String>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The wallet file is missing, exists already or does not match.
    #[error("{0}")]
    Setup(String),
    #[error("wallet database: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("protocol: {0}")]
    Protocol(#[from] licash::Error),
    /// No answer, or none that makes sense: retry later.
    #[error("hub {hub} is unavailable: {reason}")]
    Unavailable { hub: String, reason: String },
    #[error("hub {hub} refused: {problem}")]
    Refused { hub: String, problem: Problem },
    /// The payment is recorded, its outcome unknown: [`Wallet::resume`]
    /// finishes it.
    #[error("payment {id} is pending ({reason}): resume finishes it")]
    Pending { id: String, reason: String },
    /// Not a hub this wallet added, or one that claims another's keyset.
    #[error("unknown hub {0}: add it first")]
    UnknownHub(String),
    #[error("{needed} needed, {available} available in one payment")]
    Insufficient { needed: u64, available: u64 },
    #[error("{0}")]
    Invalid(String),
}

fn invalid(e: impl std::fmt::Display) -> Error {
    Error::Invalid(e.to_string())
}

/// What the wallet holds at one hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance {
    pub hub: String,
    /// Spendable now.
    pub available: u64,
    /// Held by payments under way.
    pub pending: u64,
}

/// A payment made, and where it stands.
#[derive(Clone, Debug)]
pub struct Sent {
    pub id: String,
    pub hub: String,
    pub payee: Address,
    pub amount: u64,
    pub fee: u64,
    pub state: State,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Not signed yet: its hub may refuse it, which frees its notes.
    Offered,
    /// Signed, waiting for its hub's release.
    Signed,
    Done,
    /// Given up, its notes freed, and why.
    Abandoned(String),
}

/// What [`Wallet::resume`] did with the payments under way.
#[derive(Debug, Default)]
pub struct Resumed {
    /// Finished, by id, with the payment to hand each payee.
    pub paid: Vec<(String, Payment)>,
    /// Refused by their hubs, given up and their notes freed.
    pub abandoned: Vec<(String, Problem)>,
    /// Still under way, and why.
    pub pending: Vec<(String, String)>,
}

pub struct Wallet<T> {
    store: Store,
    owner: OwnerKeys,
    transport: T,
}

/// A note the wallet holds, with where it was made: the scan key derives
/// it again from there.
struct Held {
    note: Note,
    ephemeral: Point,
    branch: Branch,
    position: u32,
}

impl<T: Transport> Wallet<T> {
    /// A new wallet file at `path`, for the owner of `seed`.
    pub fn create(path: &Path, seed: &[u8; 32], transport: T) -> Result<Self, Error> {
        let owner = OwnerKeys::from_seed(seed)?;
        let store = Store::create(path, seed)?;
        Ok(Self {
            store,
            owner,
            transport,
        })
    }

    /// The wallet file at `path`.
    pub fn open(path: &Path, transport: T) -> Result<Self, Error> {
        let (store, seed) = Store::open(path)?;
        let owner = OwnerKeys::from_seed(&seed)?;
        Ok(Self {
            store,
            owner,
            transport,
        })
    }

    /// Where others pay this wallet.
    pub fn address(&self) -> Address {
        self.owner.address()
    }

    /// Trust the hub at `url`, and learn its keysets and fees. Again, to
    /// learn what changed.
    pub async fn add_hub(&self, url: &str) -> Result<Info, Error> {
        self.refresh(&hub_url(url)?).await
    }

    /// The hubs added, and what each last said of itself.
    pub fn hubs(&self) -> Result<Vec<(String, Info)>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare("SELECT url, info FROM hubs ORDER BY url")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?;
            rows.map(|row| {
                let (url, info) = row?;
                Ok((url, serde_json::from_str(&info).map_err(invalid)?))
            })
            .collect()
        })
    }

    pub fn balance(&self) -> Result<Vec<Balance>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT h.url,
                    COALESCE(SUM(CASE WHEN n.spent_by IS NULL THEN n.denom END), 0),
                    COALESCE(SUM(CASE WHEN s.id IS NOT NULL AND s.done_at IS NULL THEN n.denom END), 0)
                 FROM hubs h
                 LEFT JOIN keysets k ON k.hub = h.url
                 LEFT JOIN notes n ON n.keyset = k.id
                 LEFT JOIN sends s ON s.id = n.spent_by
                 GROUP BY h.url ORDER BY h.url",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Balance {
                    hub: r.get(0)?,
                    available: r.get::<_, i64>(1)? as u64,
                    pending: r.get::<_, i64>(2)? as u64,
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    async fn refresh(&self, hub: &str) -> Result<Info, Error> {
        let info: Info = self.get(hub, licash_api::INFO).await?;
        let keysets = info.keysets().map_err(|p| unavailable(hub, p))?;
        let text = serde_json::to_string(&info).map_err(invalid)?;
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO hubs (url, info, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (url) DO UPDATE SET info = ?2, updated_at = ?3",
                params![hub, text, now()],
            )?;
            for keyset in &keysets {
                let known: Option<String> = tx
                    .query_row(
                        "SELECT hub FROM keysets WHERE id = ?1",
                        [keyset.id()],
                        |r| r.get(0),
                    )
                    .optional()?;
                match known {
                    None => {
                        tx.execute(
                            "INSERT INTO keysets (id, hub, bytes) VALUES (?1, ?2, ?3)",
                            params![keyset.id(), hub, keyset.to_bytes()],
                        )?;
                    }
                    Some(other) if other != hub => {
                        return Err(Error::Invalid(format!(
                            "hub {hub} advertises a keyset of hub {other}"
                        )));
                    }
                    Some(_) => {}
                }
            }
            Ok(())
        })?;
        tracing::info!(%hub, keysets = keysets.len(), "hub info");
        Ok(info)
    }

    fn info(&self, hub: &str) -> Result<Info, Error> {
        let info: Option<String> = self.store.read(|c| {
            Ok(
                c.query_row("SELECT info FROM hubs WHERE url = ?1", [hub], |r| r.get(0))
                    .optional()?,
            )
        })?;
        let info = info.ok_or_else(|| Error::UnknownHub(hub.to_owned()))?;
        serde_json::from_str(&info).map_err(invalid)
    }

    fn keysets(&self, hub: &str) -> Result<Vec<Keyset>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare("SELECT bytes FROM keysets WHERE hub = ?1")?;
            let rows = stmt.query_map([hub], |r| r.get::<_, Vec<u8>>(0))?;
            rows.map(|bytes| Ok(Keyset::from_bytes(&bytes?)?)).collect()
        })
    }

    async fn get<A: DeserializeOwned>(&self, hub: &str, path: &str) -> Result<A, Error> {
        self.exchange(hub, path, None).await
    }

    async fn post<A: DeserializeOwned>(
        &self,
        hub: &str,
        path: &str,
        body: &impl Serialize,
    ) -> Result<A, Error> {
        let body = serde_json::to_vec(body).map_err(invalid)?;
        self.exchange(hub, path, Some(body)).await
    }

    async fn exchange<A: DeserializeOwned>(
        &self,
        hub: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<A, Error> {
        let response = self
            .transport
            .request(&format!("{hub}{path}"), body)
            .await
            .map_err(|reason| unavailable(hub, reason))?;
        if (200..300).contains(&response.status) {
            return serde_json::from_slice(&response.body)
                .map_err(|e| unavailable(hub, format!("{path}: {e}")));
        }
        match serde_json::from_slice::<Problem>(&response.body) {
            Ok(problem) if problem.code != Code::Unavailable => Err(Error::Refused {
                hub: hub.to_owned(),
                problem,
            }),
            Ok(problem) => Err(unavailable(hub, problem.message)),
            Err(_) => Err(unavailable(
                hub,
                format!("{path}: status {}", response.status),
            )),
        }
    }
}

/// Payments made.
impl<T: Transport> Wallet<T> {
    /// Pay `amount` to `payee` through the hub at `hub`, the hub's fee on
    /// top, and the payment to hand the payee. A hub that refuses the fee
    /// it advertised is asked again for its fees, and paid them, once.
    pub async fn send(&self, hub: &str, payee: &Address, amount: u64) -> Result<Payment, Error> {
        let hub = hub_url(hub)?;
        if amount == 0 {
            return Err(Error::Invalid("nothing to pay".into()));
        }
        match self.send_once(&hub, payee, amount).await {
            Err(Error::Refused { problem, .. }) if problem.code == Code::Fee => {
                tracing::info!(%hub, %problem, "fees changed, paying the new ones");
                self.refresh(&hub).await?;
                self.send_once(&hub, payee, amount).await
            }
            sent => sent,
        }
    }

    async fn send_once(&self, hub: &str, payee: &Address, amount: u64) -> Result<Payment, Error> {
        let info = self.info(hub)?;
        let (keyset, selection) = self.select(hub, amount, &info.fees.spend)?;
        let inputs = self.store.read(|c| {
            selection
                .inputs
                .iter()
                .map(|y| Ok(held(c, &keyset, y)?.note))
                .collect::<Result<Vec<_>, Error>>()
        })?;
        let me = self.address();
        let mut outputs = Vec::new();
        for (to, value) in [
            (payee, amount),
            (&info.address, selection.fee),
            (&me, selection.change()),
        ] {
            let denoms = coins::split(value, keyset.denominations())
                .ok_or_else(|| Error::Invalid(format!("the keyset cannot make {value}")))?;
            outputs.extend(
                denoms
                    .into_iter()
                    .map(|denom| (OsRng.next_u64(), denom, to)),
            );
        }
        // Largest first, and equal ones in random order, so that their
        // order tells no one whose they are.
        outputs.sort_by_key(|&(order, denom, _)| (Reverse(denom), order));
        let ephemeral = Ephemeral::random();
        let outputs = outputs
            .iter()
            .enumerate()
            .map(|(position, &(_, denom, to))| {
                Output::from_derivation(&ephemeral.derive(to, denom, Branch::Pay, position)?)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let proposal = Proposal::new(&keyset, &inputs, ephemeral.public(), outputs, None)?;
        let id = proposal.id();
        let offer = serde_json::to_string(&Offer::new(&proposal, &inputs)).map_err(invalid)?;
        self.store.write(|tx| {
            tx.execute(
                "INSERT INTO sends (id, hub, payee, amount, fee, offer, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id.as_bytes(),
                    hub,
                    payee.to_bytes(),
                    sql(amount),
                    sql(selection.fee),
                    offer,
                    now()
                ],
            )?;
            for y in &selection.inputs {
                let taken = tx.execute(
                    "UPDATE notes SET spent_by = ?1
                     WHERE y = ?2 AND spent_by IS NULL AND session IS NULL",
                    params![id.as_bytes(), y],
                )?;
                if taken != 1 {
                    return Err(Error::Invalid(
                        "a note was taken meanwhile: try again".into(),
                    ));
                }
            }
            Ok(())
        })?;
        tracing::info!(id = %hex::encode(id.as_bytes()), %hub, amount, fee = selection.fee, "sending");
        self.advance(&id).await
    }

    /// The notes of one keyset of `hub` that pay `amount` and `fee`.
    fn select(
        &self,
        hub: &str,
        amount: u64,
        fee: &licash_api::Fee,
    ) -> Result<(Keyset, coins::Selection<[u8; 33]>), Error> {
        let mut short = Error::Insufficient {
            needed: amount.saturating_add(fee.due(amount)),
            available: 0,
        };
        let mut keysets = self.keysets(hub)?;
        let notes = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT y, keyset, denom FROM notes
                 WHERE spent_by IS NULL AND session IS NULL",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, [u8; 33]>(0)?,
                    r.get::<_, [u8; 32]>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })?;
        let of = |keyset: &Keyset| {
            notes
                .iter()
                .filter(|(_, id, _)| id == keyset.id())
                .map(|&(y, _, denom)| (denom as u64, y))
                .collect::<Vec<_>>()
        };
        keysets.sort_by_cached_key(|k| Reverse(of(k).iter().map(|n| n.0).sum::<u64>()));
        for keyset in keysets {
            let held = of(&keyset);
            match coins::select(&held, amount, fee) {
                Ok(selection) => return Ok((keyset, selection)),
                Err(needed) => {
                    if let Error::Insufficient { available: 0, .. } = short {
                        let available = held.iter().take(MAX_INPUTS).map(|n| n.0).sum();
                        short = Error::Insufficient { needed, available };
                    }
                }
            }
        }
        Err(short)
    }

    /// Take send `id` on from where it stands: ask for its offer, sign it,
    /// ask for its release, then keep its change.
    async fn advance(&self, id: &ProposalId) -> Result<Payment, Error> {
        let pending = |e: Error| match e {
            Error::Refused { .. } | Error::Pending { .. } => e,
            e => Error::Pending {
                id: hex::encode(id.as_bytes()),
                reason: e.to_string(),
            },
        };
        self.advance_from(id).await.map_err(pending)
    }

    async fn advance_from(&self, id: &ProposalId) -> Result<Payment, Error> {
        let row = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT hub, offer, release, unlock, abandoned FROM sends WHERE id = ?1",
                [id.as_bytes()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<[u8; 32]>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )?)
        })?;
        let (hub, offer, release, unlock, abandoned) = row;
        if let Some(reason) = abandoned {
            return Err(Error::Invalid(format!("given up: {reason}")));
        }
        let keysets = self.keysets(&hub)?;
        let lookup = |id: &[u8; 32]| keysets.iter().find(|k| k.id() == id);
        let release: Release = match release {
            Some(release) => serde_json::from_str(&release).map_err(invalid)?,
            None => {
                let offer: Offer = serde_json::from_str(&offer).map_err(invalid)?;
                self.sign(&hub, id, &offer, lookup).await?
            }
        };
        let (signed, notes) = release.decode(lookup).map_err(invalid)?;
        let keyset = lookup(signed.proposal().keyset_id())
            .ok_or_else(|| Error::Invalid("a send under a keyset its hub no longer has".into()))?;
        let Authorization::Granted(grant) = signed.authorization() else {
            return Err(Error::Invalid("a send without a grant".into()));
        };
        let unlock = match unlock {
            Some(unlock) => unlock,
            None => self.release(&hub, id, &release, keyset, grant).await?,
        };
        let change = self
            .owner
            .scan_key()
            .receive(signed.proposal())?
            .into_iter()
            .map(|r| {
                let note = signed.open_granted_output(
                    keyset,
                    &notes,
                    r.position as usize,
                    &r.derivation,
                    &unlock,
                )?;
                Ok((r, note))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let ephemeral = signed.proposal().ephemeral();
        self.store.write(|tx| {
            tx.execute(
                "UPDATE sends SET unlock = ?2, done_at = COALESCE(done_at, ?3) WHERE id = ?1",
                params![id.as_bytes(), unlock, now()],
            )?;
            for (r, note) in &change {
                insert_note(tx, keyset, note, id, &ephemeral, r.branch, r.position)?;
            }
            Ok(())
        })?;
        Ok(Payment::new(&hub, &signed, &notes, &unlock))
    }

    /// The hub's offer for `offer`, signed and recorded: from then on the
    /// send is never given up unasked. Refused, the send is given up.
    async fn sign<'k>(
        &self,
        hub: &str,
        id: &ProposalId,
        offer: &Offer,
        keysets: impl Fn(&[u8; 32]) -> Option<&'k Keyset>,
    ) -> Result<Release, Error> {
        let (proposal, notes) = offer.decode(&keysets).map_err(invalid)?;
        let keyset = keysets(proposal.keyset_id())
            .ok_or_else(|| Error::Invalid("a send under a keyset its hub no longer has".into()))?;
        let grant = match self.post::<Offered>(hub, licash_api::OFFER, offer).await {
            Ok(offered) => offered.decode().map_err(|p| unavailable(hub, p))?,
            Err(Error::Refused { hub, problem }) => {
                self.give_up(id, &problem.to_string(), false)?;
                return Err(Error::Refused { hub, problem });
            }
            Err(e) => return Err(e),
        };
        grant
            .verify(keyset, &proposal)
            .map_err(|e| unavailable(hub, format!("its offer: {e}")))?;
        let owners = self.store.read(|c| {
            notes
                .iter()
                .map(|note| {
                    let y = note.message().identifier()?.0;
                    let held = held(c, keyset, &y)?;
                    Ok(self
                        .derivation(&held)?
                        .ownership()
                        .owner_secret(self.owner.spend_secret())?)
                })
                .collect::<Result<Vec<_>, Error>>()
        })?;
        let signed = SignedProposal::sign(
            keyset,
            proposal,
            &notes,
            Authorization::Granted(Box::new(grant)),
            &owners,
        )?;
        let release = serde_json::to_string(&Release::new(&signed, &notes)).map_err(invalid)?;
        // Another process may have signed it first: its signature stands.
        let (release, abandoned): (String, Option<String>) = self.store.write(|tx| {
            tx.execute(
                "UPDATE sends SET release = ?2
                 WHERE id = ?1 AND release IS NULL AND abandoned IS NULL",
                params![id.as_bytes(), release],
            )?;
            Ok(tx.query_row(
                "SELECT COALESCE(release, ''), abandoned FROM sends WHERE id = ?1",
                [id.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })?;
        if let Some(reason) = abandoned {
            return Err(Error::Invalid(format!("given up: {reason}")));
        }
        serde_json::from_str(&release).map_err(invalid)
    }

    /// The hub's unlock for the signed `release`, checked to open its
    /// grant. A hub that spent an input otherwise will never release it:
    /// the send is given up.
    async fn release(
        &self,
        hub: &str,
        id: &ProposalId,
        release: &Release,
        keyset: &Keyset,
        grant: &Grant,
    ) -> Result<Unlock, Error> {
        let released = match self
            .post::<Released>(hub, licash_api::RELEASE, release)
            .await
        {
            Ok(released) => released,
            Err(Error::Refused { hub, problem }) if problem.code == Code::Spent => {
                self.give_up(id, &problem.to_string(), true)?;
                return Err(Error::Refused { hub, problem });
            }
            Err(e) => return Err(e),
        };
        let unlock = released.decode().map_err(|p| unavailable(hub, p))?;
        grant
            .verify_unlock(keyset, &unlock)
            .map_err(|e| unavailable(hub, format!("its unlock: {e}")))?;
        Ok(unlock)
    }

    /// Give send `id` up and free its notes, unless it is done, or signed
    /// and not `signed_too`.
    fn give_up(&self, id: &ProposalId, reason: &str, signed_too: bool) -> Result<bool, Error> {
        let given_up = self.store.write(|tx| {
            let given_up = tx.execute(
                "UPDATE sends SET abandoned = ?2
                 WHERE id = ?1 AND done_at IS NULL AND abandoned IS NULL
                 AND (release IS NULL OR ?3)",
                params![id.as_bytes(), reason, signed_too],
            )? == 1;
            if given_up {
                tx.execute(
                    "UPDATE notes SET spent_by = NULL WHERE spent_by = ?1",
                    [id.as_bytes()],
                )?;
            }
            Ok(given_up)
        })?;
        if given_up {
            tracing::warn!(id = %hex::encode(id.as_bytes()), reason, "payment given up");
        }
        Ok(given_up)
    }

    /// Finish every payment under way, or give up those their hubs refuse.
    pub async fn resume(&self) -> Result<Resumed, Error> {
        let ids: Vec<[u8; 32]> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM sends WHERE done_at IS NULL AND abandoned IS NULL
                 ORDER BY created_at",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        let mut resumed = Resumed::default();
        for id in ids {
            let key = hex::encode(id);
            match self.advance(&ProposalId::from_bytes(id)).await {
                Ok(payment) => resumed.paid.push((key, payment)),
                Err(Error::Refused { problem, .. }) => resumed.abandoned.push((key, problem)),
                Err(e) => resumed.pending.push((key, e.to_string())),
            }
        }
        Ok(resumed)
    }

    /// Give up the payment `id`, not signed yet, and free its notes: for a
    /// hub gone for good. A signed payment its hub may have released.
    pub fn abandon(&self, id: &str) -> Result<(), Error> {
        let id = send_id(id)?;
        if self.give_up(&id, "abandoned by its owner", false)? {
            return Ok(());
        }
        Err(Error::Invalid(
            "not a payment under way and unsigned: its hub may have released it".into(),
        ))
    }

    /// The payment `id`, once done, to hand its payee again.
    pub fn payment(&self, id: &str) -> Result<Payment, Error> {
        let id = send_id(id)?;
        let (hub, release, unlock): (String, Option<String>, Option<[u8; 32]>) =
            self.store.read(|c| {
                c.query_row(
                    "SELECT hub, release, unlock FROM sends WHERE id = ?1",
                    [id.as_bytes()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| Error::Invalid("no such payment".into()))
            })?;
        let (Some(release), Some(unlock)) = (release, unlock) else {
            return Err(Error::Invalid("the payment is not done".into()));
        };
        let release: Release = serde_json::from_str(&release).map_err(invalid)?;
        let keysets = self.keysets(&hub)?;
        let (signed, notes) = release
            .decode(|id| keysets.iter().find(|k| k.id() == id))
            .map_err(invalid)?;
        Ok(Payment::new(&hub, &signed, &notes, &unlock))
    }

    /// The payments made, newest first.
    pub fn sends(&self) -> Result<Vec<Sent>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, hub, payee, amount, fee, release IS NOT NULL, done_at IS NOT NULL,
                    abandoned, created_at
                 FROM sends ORDER BY created_at DESC, rowid DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                let state = match (r.get::<_, Option<String>>(7)?, r.get(6)?, r.get(5)?) {
                    (Some(reason), _, _) => State::Abandoned(reason),
                    (None, true, _) => State::Done,
                    (None, false, true) => State::Signed,
                    (None, false, false) => State::Offered,
                };
                Ok((
                    r.get::<_, [u8; 32]>(0)?,
                    r.get(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    state,
                    r.get(8)?,
                ))
            })?;
            rows.map(|row| {
                let (id, hub, payee, amount, fee, state, created_at) = row?;
                Ok(Sent {
                    id: hex::encode(id),
                    hub,
                    payee: Address::from_bytes(&payee)?,
                    amount: amount as u64,
                    fee: fee as u64,
                    state,
                    created_at,
                })
            })
            .collect()
        })
    }
}

/// Payments taken, and unlinking.
impl<T: Transport> Wallet<T> {
    /// Take `payment`: keep the notes it pays this wallet, its unlock
    /// checked to open them. It must be under a keyset of a hub added
    /// here, as the wallet takes no hub's notes unasked. The amount.
    pub fn receive(&self, payment: &Payment) -> Result<u64, Error> {
        let id: [u8; 32] = payment
            .release
            .keyset
            .0
            .as_slice()
            .try_into()
            .map_err(invalid)?;
        let keyset: Option<Vec<u8>> = self.store.read(|c| {
            Ok(
                c.query_row("SELECT bytes FROM keysets WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?,
            )
        })?;
        let keyset =
            Keyset::from_bytes(&keyset.ok_or_else(|| Error::UnknownHub(payment.hub.clone()))?)?;
        let (signed, notes, unlock) = payment
            .decode(|id| (id == keyset.id()).then_some(&keyset))
            .map_err(invalid)?;
        let proposal = signed.proposal();
        let ours = self
            .owner
            .scan_key()
            .receive(proposal)?
            .into_iter()
            .map(|r| {
                let note = signed.open_granted_output(
                    &keyset,
                    &notes,
                    r.position as usize,
                    &r.derivation,
                    &unlock,
                )?;
                Ok((r, note))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        if ours.is_empty() {
            return Err(Error::Invalid(
                "the payment pays this wallet nothing".into(),
            ));
        }
        let (pid, ephemeral) = (proposal.id(), proposal.ephemeral());
        let amount = self.store.write(|tx| {
            let mut amount = 0u64;
            for (r, note) in &ours {
                if insert_note(tx, &keyset, note, &pid, &ephemeral, r.branch, r.position)? {
                    amount += note.message().denomination();
                }
            }
            if amount == 0 {
                return Err(Error::Invalid("the payment is received already".into()));
            }
            tx.execute(
                "INSERT INTO receipts (id, keyset, amount, received_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT DO NOTHING",
                params![pid.as_bytes(), keyset.id(), sql(amount), now()],
            )?;
            Ok(amount)
        })?;
        tracing::info!(id = %hex::encode(pid.as_bytes()), amount, "received");
        Ok(amount)
    }

    /// Unlink the notes held linked to their issuance, so that their hub
    /// cannot tell which payment one came from when it is spent. A note
    /// its hub unlinks no more stays linked. The number unlinked; on an
    /// error, those before it are kept and the rest are asked again next
    /// time.
    pub async fn unlink(&self) -> Result<usize, Error> {
        let todo: Vec<([u8; 33], String)> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT n.y, k.hub FROM notes n JOIN keysets k ON k.id = n.keyset
                 WHERE n.unlinked = 0 AND n.spent_by IS NULL
                 ORDER BY n.session IS NULL, n.added_at",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        let mut unlinked = 0;
        for (y, hub) in todo {
            if self.unlink_one(&hub, &y).await? {
                unlinked += 1;
            }
        }
        Ok(unlinked)
    }

    async fn unlink_one(&self, hub: &str, y: &[u8; 33]) -> Result<bool, Error> {
        let row: Option<(Vec<u8>, Option<String>)> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT k.bytes, n.session FROM notes n JOIN keysets k ON k.id = n.keyset
                 WHERE n.y = ?1 AND n.unlinked = 0 AND n.spent_by IS NULL",
                [y],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })?;
        let Some((keyset, session)) = row else {
            return Ok(false);
        };
        let keyset = Keyset::from_bytes(&keyset)?;
        let session: Session = match session {
            Some(session) => serde_json::from_str(&session).map_err(invalid)?,
            None => {
                let held = self.store.read(|c| held(c, &keyset, y))?;
                let session = Session::start(&keyset, &held.note, &self.derivation(&held)?)?;
                let text = serde_json::to_string(&session).map_err(invalid)?;
                let started = self.store.write(|tx| {
                    Ok(tx.execute(
                        "UPDATE notes SET session = ?2
                         WHERE y = ?1 AND session IS NULL AND unlinked = 0 AND spent_by IS NULL",
                        params![y, text],
                    )? == 1)
                })?;
                if !started {
                    return Ok(false);
                }
                session
            }
        };
        let answer = match self
            .post::<Unlinked>(hub, licash_api::UNLINK, &Unlink::new(session.request()))
            .await
        {
            Ok(answer) => answer.decode().map_err(|p| unavailable(hub, p))?,
            Err(Error::Refused { problem, .. })
                if matches!(
                    problem.code,
                    Code::Expired | Code::NotIssued | Code::Unlinked
                ) =>
            {
                tracing::info!(%hub, %problem, "a note stays linked");
                self.store.write(|tx| {
                    tx.execute(
                        "UPDATE notes SET unlinked = -1, session = NULL WHERE y = ?1",
                        [y],
                    )?;
                    Ok(())
                })?;
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        let note = session
            .finish(&keyset, &answer)
            .map_err(|e| unavailable(hub, format!("its unlink answer: {e}")))?;
        if note.message().identifier()?.0 != *y {
            return Err(Error::Invalid("an unlinked note changed identity".into()));
        }
        self.store.write(|tx| {
            tx.execute(
                "UPDATE notes SET note = ?2, unlinked = 1, session = NULL WHERE y = ?1",
                params![y, note.to_bytes()],
            )?;
            Ok(())
        })?;
        Ok(true)
    }

    fn derivation(&self, held: &Held) -> Result<Derivation, Error> {
        Ok(self.owner.scan_key().derive(
            &held.ephemeral,
            held.note.message().denomination(),
            held.branch,
            held.position as usize,
        )?)
    }
}

fn held(c: &Connection, keyset: &Keyset, y: &[u8]) -> Result<Held, Error> {
    let (note, ephemeral, branch, position): (Vec<u8>, [u8; 33], i64, u32) = c.query_row(
        "SELECT note, ephemeral, branch, position FROM notes WHERE y = ?1 AND keyset = ?2",
        params![y, keyset.id()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok(Held {
        note: Note::from_bytes(keyset, &note)?,
        ephemeral: Point(ephemeral),
        branch: match branch {
            0 => Branch::Pay,
            _ => Branch::Refund,
        },
        position,
    })
}

/// Keep `note`, made at `position` of `branch` of proposal `proposal`
/// with ephemeral key `ephemeral`. False if it is kept already.
fn insert_note(
    tx: &Transaction<'_>,
    keyset: &Keyset,
    note: &Note,
    proposal: &ProposalId,
    ephemeral: &Point,
    branch: Branch,
    position: u32,
) -> Result<bool, Error> {
    let branch = match branch {
        Branch::Pay => 0,
        Branch::Refund => 1,
    };
    let inserted = tx.execute(
        "INSERT INTO notes (y, keyset, denom, note, proposal, ephemeral, branch, position, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT DO NOTHING",
        params![
            note.message().identifier()?.0,
            keyset.id(),
            sql(note.message().denomination()),
            note.to_bytes(),
            proposal.as_bytes(),
            ephemeral.0,
            branch,
            position,
            now()
        ],
    )?;
    Ok(inserted == 1)
}

fn send_id(id: &str) -> Result<ProposalId, Error> {
    let mut bytes = [0; 32];
    hex::decode_to_slice(id, &mut bytes)
        .map_err(|_| Error::Invalid(format!("not a payment id: {id}")))?;
    Ok(ProposalId::from_bytes(bytes))
}

fn unavailable(hub: &str, reason: impl std::fmt::Display) -> Error {
    Error::Unavailable {
        hub: hub.to_owned(),
        reason: reason.to_string(),
    }
}

/// A hub's URL as the wallet keys it: http or https, no trailing slash.
fn hub_url(url: &str) -> Result<String, Error> {
    let url = url.trim().trim_end_matches('/');
    if !(url.starts_with("http://") || url.starts_with("https://"))
        || url.contains(char::is_whitespace)
    {
        return Err(Error::Invalid(format!("not a hub URL: {url}")));
    }
    Ok(url.to_owned())
}
