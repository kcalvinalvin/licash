//! A Licash wallet: an owner's notes at the hubs it trusts, and the
//! payments it makes and takes, in one SQLite file.
//!
//! A payment is recorded before its hub hears of it, and each step before
//! the next is asked for, so a wallet stopped at any point finishes or
//! gives it up on [`Wallet::resume`]: a hub answers the same offer and the
//! same release again. A send its hub refuses before it is signed is
//! given up and its notes freed; a signed one is only retried, unless its
//! hub shows it can never be released. Notes its hub shows spent by a copy
//! of the wallet are dropped.
//!
//! A timed lock pays a payee on the preimage of a hash, or returns to the
//! wallet once it expired unpaid: see [`Wallet::lock`] and
//! [`Wallet::claim`]. Through a hub's Lightning gateway, the wallet pays
//! invoices with locks and is paid with locks of the hub's notes: see
//! [`Wallet::pay_invoice`] and [`Wallet::invoice`]. Through a swap
//! provider, it swaps notes at one hub for notes at another with a lock at
//! each: see [`Wallet::swap`]. Silent payments to its address are found
//! with a hub's tweaks and the application's light client: see
//! [`Wallet::scan_silent`].
//!
//! The wallet reaches hubs through the application's [`Transport`]: HTTP
//! over TCP or Tor, or a test. Its seed alone does not restore its notes:
//! back up its file.

mod code;
mod coins;
mod deposits;
mod lightning;
mod locks;
mod payouts;
mod silent;
mod store;
mod swaps;

use std::{cmp::Reverse, collections::HashSet, future::Future, path::Path, sync::Mutex};

use licash::{
    address::{Address, Ephemeral, OwnerKeys},
    authorization::{Authorization, Grant, SignedProposal, Unlock},
    claim::Branch,
    crypto::Point,
    note::{Derivation, Keyset, Note},
    spend::{ChainProfile, Output, Proposal, ProposalId},
    unlink::Session,
};
use licash_api::{
    Check, Checked, Code, Info, Offer, Offered, Payment, Problem, Release, Released, Unlink,
    Unlinked, MAX_CHECK,
};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{de::DeserializeOwned, Serialize};

pub use code::{HubId, LicashCode, PaymentCode, MAX_HUBS};
pub use coins::MAX_INPUTS;
pub use deposits::{Deposit, DepositState, DepositTerms};
pub use lightning::{Invoice, InvoiceState, Payable};
pub use locks::{Incoming, Lock, LockState, CLAIM_MARGIN};
pub use payouts::{Payout, PayoutQuote, PayoutState, CLAIM_ROOM};
pub use silent::{Blocks, Payee, Scanned, SilentCoin, SilentSpend};
use store::{now, sql, Owner, Store};
pub use swaps::{Swap, SwapLimits, SwapQuote, SwapState, SWAP_WINDOW};

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

/// Why a wallet call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The wallet file is missing, exists already or does not match.
    #[error("{0}")]
    Setup(String),
    /// The wallet file could not be read or written.
    #[error("wallet database: {0}")]
    Storage(#[from] rusqlite::Error),
    /// Data failed the protocol's checks.
    #[error("protocol: {0}")]
    Protocol(#[from] licash::Error),
    /// No answer, or none that makes sense: retry later.
    #[error("hub {hub} is unavailable: {reason}")]
    Unavailable { hub: String, reason: String },
    /// Nothing was paid: a payment its hub refused is given up, its notes
    /// freed, and may be made again.
    #[error("hub {hub} refused: {problem}")]
    Refused { hub: String, problem: Problem },
    /// The payment is recorded, its outcome unknown: [`Wallet::resume`]
    /// finishes it.
    #[error("payment {id} is pending ({reason}): resume finishes it")]
    Pending { id: String, reason: String },
    /// The payment was given up before, its notes freed: nothing was paid.
    #[error("payment {id} was given up: {reason}")]
    Abandoned { id: String, reason: String },
    /// Not a hub this wallet added. With a payment, the hub its payer
    /// names: add it only if the owner trusts it.
    #[error("unknown hub {0}: add it first")]
    UnknownHub(String),
    #[error("no payment {0}")]
    UnknownPayment(String),
    #[error("{needed} needed, {available} available in one payment")]
    Insufficient { needed: u64, available: u64 },
    /// Another payment took a note meanwhile: nothing was paid.
    #[error("another payment took a note meanwhile: try again")]
    NoteTaken,
    #[error("the payment is received already")]
    AlreadyReceived,
    #[error("the payment pays this wallet nothing")]
    PaysNothing,
    #[error("{0}")]
    Invalid(String),
    /// The application's view of Bitcoin failed, or served a block that
    /// is not the one asked for: retry later.
    #[error("chain source failed: {0}")]
    Chain(String),
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
    /// Held by payments and locks under way.
    pub pending: u64,
}

/// How many notes of one denomination the wallet can spend at one hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notes {
    pub hub: String,
    pub denomination: u64,
    pub count: u64,
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

/// Where a payment stands.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum State {
    /// Not signed yet: its hub may refuse it, which frees its notes.
    Offered,
    /// Signed, waiting for its hub's release.
    Signed,
    /// Released by its hub: paid.
    Done,
    /// Given up, its notes freed, and why.
    Abandoned(String),
}

/// A payment made, by its id, with the payment to hand its payee.
#[derive(Clone, Debug)]
pub struct Paid {
    pub id: String,
    pub payment: Payment,
}

/// A payment taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub id: String,
    pub hub: String,
    pub amount: u64,
    pub received_at: i64,
}

/// What [`Wallet::resume`] did with the payments and locks under way.
#[derive(Debug, Default)]
pub struct Resumed {
    pub paid: Vec<Paid>,
    /// Locks taken on, where each stands now.
    pub locks: Vec<Lock>,
    /// Invoices taken on, where each stands now.
    pub invoices: Vec<Invoice>,
    /// Swaps taken on, where each stands now.
    pub swaps: Vec<Swap>,
    /// Deposits taken on, where each stands now.
    pub deposits: Vec<Deposit>,
    /// Given up, their notes freed, by id and why.
    pub abandoned: Vec<(String, String)>,
    /// Still under way, by id and why.
    pub pending: Vec<(String, String)>,
}

/// The step of a payment its hub refused, past which it is not given up:
/// a send unsigned, a send signed and refused as spent, a lock its hub has
/// not certified.
#[derive(Clone, Copy)]
pub(crate) enum Step {
    Offer,
    Release,
    Lock,
}

impl Step {
    /// Give the payment `?1` up for reason `?2`.
    fn give_up(self) -> &'static str {
        match self {
            Self::Offer => {
                "UPDATE sends SET abandoned = ?2
                 WHERE id = ?1 AND done_at IS NULL AND abandoned IS NULL AND release IS NULL"
            }
            Self::Release => {
                "UPDATE sends SET abandoned = ?2
                 WHERE id = ?1 AND done_at IS NULL AND abandoned IS NULL"
            }
            Self::Lock => {
                "UPDATE locks SET abandoned = ?2
                 WHERE id = ?1 AND certificate IS NULL AND abandoned IS NULL"
            }
        }
    }

    /// Why the payment `?1` was given up, if it was.
    fn abandoned(self) -> &'static str {
        match self {
            Self::Offer | Self::Release => "SELECT abandoned FROM sends WHERE id = ?1",
            Self::Lock => "SELECT abandoned FROM locks WHERE id = ?1",
        }
    }
}

/// An owner's wallet: its keys, derived from its seed, and its notes and
/// payments, in one SQLite file. It reaches hubs and swap providers
/// through `T`, the application's [`Transport`].
pub struct Wallet<T> {
    store: Store,
    owner: OwnerKeys,
    keys: code::CodeKeys,
    network: ChainProfile,
    transport: T,
    /// The hubs reached over plain HTTP asked over HTTPS since opened.
    asked_secure: Mutex<HashSet<i64>>,
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
    /// A new wallet file at `path`, holding `seed`, its owner's, for hubs
    /// on `network`.
    pub fn create(
        path: &Path,
        seed: &[u8; 32],
        network: ChainProfile,
        transport: T,
    ) -> Result<Self, Error> {
        Self::created(path, seed, true, network, transport)
    }

    /// A new wallet file at `path` for the owner of `seed`, which the
    /// application keeps, as in a phone's keystore: it opens the wallet
    /// with [`Wallet::open_with_seed`].
    pub fn create_without_seed(
        path: &Path,
        seed: &[u8; 32],
        network: ChainProfile,
        transport: T,
    ) -> Result<Self, Error> {
        Self::created(path, seed, false, network, transport)
    }

    fn created(
        path: &Path,
        seed: &[u8; 32],
        keep: bool,
        network: ChainProfile,
        transport: T,
    ) -> Result<Self, Error> {
        let owner = OwnerKeys::from_seed(seed)?;
        let address = owner.address().to_bytes();
        let keys = code::CodeKeys::from_seed(seed, network)?;
        let store = Store::create(path, keep.then_some(seed), &address, network.name())?;
        Ok(Self {
            store,
            owner,
            keys,
            network,
            transport,
            asked_secure: Mutex::default(),
        })
    }

    /// The wallet file at `path`, which holds its seed.
    pub fn open(path: &Path, transport: T) -> Result<Self, Error> {
        let (store, owner) = Store::open(path)?;
        let seed = owner.seed.ok_or_else(|| {
            Error::Setup("the wallet's seed is kept elsewhere: open it with its seed".into())
        })?;
        Self::opened(store, &seed, &owner, transport)
    }

    /// The wallet file at `path`, of the owner of `seed`.
    pub fn open_with_seed(path: &Path, seed: &[u8; 32], transport: T) -> Result<Self, Error> {
        let (store, owner) = Store::open(path)?;
        Self::opened(store, seed, &owner, transport)
    }

    fn opened(store: Store, seed: &[u8; 32], kept: &Owner, transport: T) -> Result<Self, Error> {
        let owner = OwnerKeys::from_seed(seed)?;
        if owner.address().to_bytes()[..] != *kept.address {
            return Err(Error::Setup("not this wallet's seed".into()));
        }
        let network = kept
            .network
            .parse()
            .map_err(|_| Error::Setup(format!("the wallet's network {}", kept.network)))?;
        Ok(Self {
            store,
            keys: code::CodeKeys::from_seed(seed, network)?,
            owner,
            network,
            transport,
            asked_secure: Mutex::default(),
        })
    }

    /// The network of the hubs the wallet trusts: a hub following another
    /// chain is refused.
    pub fn network(&self) -> ChainProfile {
        self.network
    }

    /// When the wallet file was made, in seconds since the Unix epoch: of
    /// a new seed, no payment to it is older.
    pub fn created_at(&self) -> Result<i64, Error> {
        self.store
            .read(|c| Ok(c.query_row("SELECT created_at FROM meta", [], |r| r.get(0))?))
    }

    /// Where others pay this wallet.
    pub fn address(&self) -> Address {
        self.owner.address()
    }

    /// Trust the hub at `url`, and learn its keysets and fees. Again, to
    /// learn what changed. Refused if it follows another network than the
    /// wallet's. A hub reached over plain HTTP is asked over HTTPS too,
    /// once a wallet is opened, even if unavailable over plain HTTP: the
    /// same hub there is reached there from then on, and still known at
    /// the other URL.
    pub async fn add_hub(&self, url: &str) -> Result<Info, Error> {
        let url = hub_url(url)?;
        let refreshed = self.refresh(&url).await;
        let hub = match &refreshed {
            Ok((hub, _)) => Some(*hub),
            Err(Error::Unavailable { .. }) => self.hub(&url).ok().map(|(hub, _)| hub),
            Err(_) => None,
        };
        if let Some(info) = match hub {
            Some(hub) => self.secure(hub).await,
            None => None,
        } {
            return Ok(info);
        }
        refreshed.map(|(_, info)| info)
    }

    /// The hubs added, by URL, and what each last said of itself.
    pub fn hubs(&self) -> Result<Vec<(String, Info)>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT r.url, h.info FROM hubs h JOIN reach r ON r.hub = h.id ORDER BY r.url",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?;
            rows.map(|row| {
                let (url, info) = row?;
                Ok((url, serde_json::from_str(&info).map_err(invalid)?))
            })
            .collect()
        })
    }

    /// What the wallet holds at each hub it added, by URL.
    pub fn balance(&self) -> Result<Vec<Balance>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT r.url,
                    COALESCE(SUM(CASE WHEN n.spent_by IS NULL AND n.gone_at IS NULL
                        THEN n.denom END), 0),
                    COALESCE(SUM(CASE WHEN (s.id IS NOT NULL AND s.done_at IS NULL)
                        OR (l.id IS NOT NULL AND l.done_at IS NULL) THEN n.denom END), 0)
                 FROM hubs h
                 JOIN reach r ON r.hub = h.id
                 LEFT JOIN keysets k ON k.hub = h.id
                 LEFT JOIN notes n ON n.keyset = k.id
                 LEFT JOIN sends s ON s.id = n.spent_by
                 LEFT JOIN locks l ON l.id = n.spent_by
                 GROUP BY h.id ORDER BY r.url",
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

    /// How many notes the wallet can spend now of each denomination, at
    /// each hub it added: by URL, then largest denomination first.
    pub fn notes(&self) -> Result<Vec<Notes>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT r.url, n.denom, COUNT(*)
                 FROM hubs h
                 JOIN reach r ON r.hub = h.id
                 JOIN keysets k ON k.hub = h.id
                 JOIN notes n ON n.keyset = k.id
                 WHERE n.spent_by IS NULL AND n.gone_at IS NULL
                 GROUP BY h.id, n.denom ORDER BY r.url, n.denom DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Notes {
                    hub: r.get(0)?,
                    denomination: r.get::<_, i64>(1)? as u64,
                    count: r.get::<_, i64>(2)? as u64,
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    /// The fee of the hub at `url`, added, on the notes a payment of
    /// `amount` there spends now, as it charges on all they hold: refused
    /// as [`Error::Insufficient`] if it holds not that much in notes it
    /// can spend together.
    pub fn send_fee(&self, url: &str, amount: u64) -> Result<u64, Error> {
        let (hub, info) = self.hub(&hub_url(url)?)?;
        Ok(self.select(hub, amount, &info.fees.spend)?.1.fee)
    }

    /// [`Wallet::send_fee`] of a lock of `amount` to another than the hub.
    pub fn lock_fee(&self, url: &str, amount: u64) -> Result<u64, Error> {
        let (hub, info) = self.hub(&hub_url(url)?)?;
        Ok(self.select(hub, amount, &info.fees.lock)?.1.fee)
    }

    /// What more the wallet needs at the hub at `url`, added, to pay
    /// `amount` there, the hub's fee on what it spends on top: none if it
    /// holds that much in notes it can spend together.
    pub fn lacks(&self, url: &str, amount: u64) -> Result<u64, Error> {
        let (hub, info) = self.hub(&hub_url(url)?)?;
        let fee = info.fees.spend;
        let available = match self.select(hub, amount, &fee) {
            Ok(_) => return Ok(0),
            Err(Error::Insufficient { available, .. }) => available,
            Err(e) => return Err(e),
        };
        // The fee grows with what is spent: more until it is covered.
        let mut more = 0u64;
        for _ in 0..64 {
            let held = available.saturating_add(more);
            let needed = amount.saturating_add(fee.due(held));
            if held >= needed {
                break;
            }
            more = needed - available;
        }
        Ok(more)
    }

    /// What the hub at `url` says of itself, kept: its id and info. A hub
    /// known at `url` is asked where the wallet reaches it.
    async fn refresh(&self, url: &str) -> Result<(i64, Info), Error> {
        let reached: Option<String> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT r.url FROM hub_urls u JOIN reach r ON r.hub = u.hub WHERE u.url = ?1",
                [url],
                |r| r.get(0),
            )
            .optional()?)
        })?;
        let url = reached.as_deref().unwrap_or(url);
        let info = self.info(url).await?;
        Ok((self.keep_info(url, &info)?, info))
    }

    /// The hub `hub` asked over HTTPS, at most once since the wallet was
    /// opened, if reached over plain HTTP: what it says there, if it is
    /// the same hub, reached there from now on.
    async fn secure(&self, hub: i64) -> Option<Info> {
        let reached = self.reached(hub).ok()?;
        let url = secure_url(&reached)?;
        if !self.asked_secure.lock().ok()?.insert(hub) {
            return None;
        }
        let info = match self.info(&url).await {
            Ok(info) => info,
            Err(e) => {
                tracing::debug!(%url, "hub not reached over HTTPS: {e}");
                return None;
            }
        };
        // Not keysets all new to the wallet: another hub, or one it would
        // take as a hub of its own.
        let known = self.keysets(hub).ok()?;
        let theirs = info.keysets().ok()?;
        if !theirs
            .iter()
            .any(|k| known.iter().any(|o| o.id() == k.id()))
        {
            tracing::warn!(%url, http = %reached, "another hub over HTTPS than over HTTP");
            return None;
        }
        match self.keep_info(&url, &info) {
            Ok(kept) if kept == hub => Some(info),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(%url, "hub not reached over HTTPS: {e}");
                None
            }
        }
    }

    /// The URL the wallet reaches the hub `hub` at.
    fn reached(&self, hub: i64) -> Result<String, Error> {
        self.store
            .read(|c| Ok(c.query_row("SELECT url FROM reach WHERE hub = ?1", [hub], |r| r.get(0))?))
    }

    /// What the hub at `url` says of itself, if it follows the wallet's
    /// network.
    async fn info(&self, url: &str) -> Result<Info, Error> {
        let info: Info = self.get(url, licash_api::INFO).await?;
        if let Some(locks) = &info.locks {
            let profile = locks
                .checkpoint()
                .map_err(|p| unavailable(url, p))?
                .profile();
            if profile != self.network {
                return Err(Error::Invalid(format!(
                    "hub {url} follows {profile}, not this wallet's network, {}",
                    self.network
                )));
            }
        }
        // A proof of another identity: not the hub's info, whoever made it.
        info.proved(self.network)
            .map_err(|p| Error::Invalid(format!("hub {url}: {}", p.message)))?;
        Ok(info)
    }

    /// Keep `info`, what the hub at `url` says of itself: its id. A keyset
    /// belongs to the hub that first advertised it: another URL advertising
    /// it is not that hub, unless it is the URL the hub is reached at, over
    /// HTTPS rather than plain HTTP. The hub is reached there from then on,
    /// and still known at the other URL.
    fn keep_info(&self, url: &str, info: &Info) -> Result<i64, Error> {
        let keysets = info.keysets().map_err(|p| unavailable(url, p))?;
        let text = serde_json::to_string(info).map_err(invalid)?;
        let hub = self.store.write(|tx| {
            let mut known: Option<i64> = tx
                .query_row("SELECT hub FROM hub_urls WHERE url = ?1", [url], |r| {
                    r.get(0)
                })
                .optional()?;
            let mut others: Vec<(i64, String)> = Vec::new();
            for keyset in &keysets {
                let other = tx
                    .query_row(
                        "SELECT r.hub, r.url FROM keysets k JOIN reach r ON r.hub = k.hub
                         WHERE k.id = ?1 AND k.hub IS NOT ?2",
                        params![keyset.id(), known],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some(other) = other.filter(|o| !others.contains(o)) {
                    others.push(other);
                }
            }
            match (known, others.as_slice()) {
                (_, []) => {}
                (None, [(hub, other)]) if secure_url(other).as_deref() == Some(url) => {
                    // The row's id keeps the URL first, the one reached.
                    tx.execute(
                        "UPDATE hub_urls SET url = ?1 WHERE url = ?2",
                        params![url, other],
                    )?;
                    tx.execute(
                        "INSERT INTO hub_urls (url, hub) VALUES (?1, ?2)",
                        params![other, hub],
                    )?;
                    tracing::info!(%url, http = %other, "hub reached over HTTPS from now on");
                    known = Some(*hub);
                }
                (_, [(_, other), ..]) => {
                    return Err(Error::Invalid(format!(
                        "hub {url} advertises a keyset of hub {other}"
                    )));
                }
            }
            let hub = match known {
                Some(hub) => {
                    tx.execute(
                        "UPDATE hubs SET info = ?2, updated_at = ?3 WHERE id = ?1",
                        params![hub, text, now()],
                    )?;
                    hub
                }
                None => {
                    tx.execute(
                        "INSERT INTO hubs (info, updated_at) VALUES (?1, ?2)",
                        params![text, now()],
                    )?;
                    let hub = tx.last_insert_rowid();
                    tx.execute(
                        "INSERT INTO hub_urls (url, hub) VALUES (?1, ?2)",
                        params![url, hub],
                    )?;
                    hub
                }
            };
            for keyset in &keysets {
                tx.execute(
                    "INSERT INTO keysets (id, hub, bytes) VALUES (?1, ?2, ?3)
                     ON CONFLICT DO NOTHING",
                    params![keyset.id(), hub, keyset.to_bytes()],
                )?;
            }
            Ok(hub)
        })?;
        tracing::info!(%url, keysets = keysets.len(), "hub info");
        Ok(hub)
    }

    /// The hub at `url`: its id and what it last said of itself.
    fn hub(&self, url: &str) -> Result<(i64, Info), Error> {
        let row: Option<(i64, String)> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT h.id, h.info FROM hub_urls u JOIN hubs h ON h.id = u.hub
                 WHERE u.url = ?1",
                [url],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })?;
        let (hub, info) = row.ok_or_else(|| Error::UnknownHub(url.to_owned()))?;
        Ok((hub, serde_json::from_str(&info).map_err(invalid)?))
    }

    fn keysets(&self, hub: i64) -> Result<Vec<Keyset>, Error> {
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
    /// Pay `amount` to `payee` through the hub at `url`, the hub's fee on
    /// top. A hub that refuses the fee it advertised is asked again for its
    /// fees, and paid them, once.
    pub async fn send(&self, url: &str, payee: &Address, amount: u64) -> Result<Paid, Error> {
        let url = hub_url(url)?;
        if amount == 0 {
            return Err(Error::Invalid("nothing to pay".into()));
        }
        match self.send_once(&url, payee, amount).await {
            Err(Error::Refused { problem, .. }) if problem.code == Code::Fee => {
                tracing::info!(%url, %problem, "fees changed, paying the new ones");
                self.refresh(&url).await?;
                self.send_once(&url, payee, amount).await
            }
            sent => sent,
        }
    }

    async fn send_once(&self, url: &str, payee: &Address, amount: u64) -> Result<Paid, Error> {
        let (hub, info) = self.hub(url)?;
        let (keyset, selection) = self.select(hub, amount, &info.fees.spend)?;
        let inputs = self.notes_of(&keyset, &selection.inputs)?;
        let me = self.address();
        let ephemeral = Ephemeral::random();
        let parts = [
            (payee, amount),
            (&info.address, selection.fee),
            (&me, selection.change()),
        ];
        let outputs = outputs(&keyset, &ephemeral, Branch::Pay, &parts)?;
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
            take(tx, &id, &selection.inputs)
        })?;
        let key = hex::encode(id.as_bytes());
        tracing::info!(id = %key, %url, amount, fee = selection.fee, "sending");
        let payment = self.advance(&id).await?;
        Ok(Paid { id: key, payment })
    }

    /// The wallet's notes `ys` of `keyset`.
    fn notes_of(&self, keyset: &Keyset, ys: &[[u8; 33]]) -> Result<Vec<Note>, Error> {
        self.store
            .read(|c| ys.iter().map(|y| Ok(held(c, keyset, y)?.note)).collect())
    }

    /// The notes of one keyset of `hub` that pay `amount` and `fee`.
    fn select(
        &self,
        hub: i64,
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
                 WHERE spent_by IS NULL AND session IS NULL AND gone_at IS NULL",
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
    /// ask for its release, then keep its change. Refused only if given up
    /// here; any other failure leaves it pending.
    async fn advance(&self, id: &ProposalId) -> Result<Payment, Error> {
        let pending = |e: Error| match e {
            Error::Refused { .. } | Error::Pending { .. } | Error::Abandoned { .. } => e,
            e => Error::Pending {
                id: hex::encode(id.as_bytes()),
                reason: e.to_string(),
            },
        };
        self.advance_from(id).await.map_err(pending)
    }

    async fn advance_from(&self, id: &ProposalId) -> Result<Payment, Error> {
        type Row = (
            i64,
            String,
            String,
            Option<String>,
            Option<[u8; 32]>,
            Option<String>,
        );
        let row: Row = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT s.hub, r.url, s.offer, s.release, s.unlock, s.abandoned
                 FROM sends s JOIN reach r ON r.hub = s.hub WHERE s.id = ?1",
                [id.as_bytes()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )?)
        })?;
        let (hub, url, offer, release, unlock, abandoned) = row;
        if let Some(reason) = abandoned {
            return Err(Error::Abandoned {
                id: hex::encode(id.as_bytes()),
                reason,
            });
        }
        let keysets = self.keysets(hub)?;
        let lookup = |id: &[u8; 32]| keysets.iter().find(|k| k.id() == id);
        let release: Release = match release {
            Some(release) => serde_json::from_str(&release).map_err(invalid)?,
            None => {
                let offer: Offer = serde_json::from_str(&offer).map_err(invalid)?;
                self.sign(&url, id, &offer, lookup).await?
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
            None => self.release(&url, id, &release, keyset, grant).await?,
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
        Ok(Payment::new(&url, &signed, &notes, &unlock))
    }

    /// The hub's offer for `offer`, signed and recorded: from then on the
    /// send is given up only once its hub refuses it as spent. Refused, the
    /// send is given up.
    async fn sign<'k>(
        &self,
        url: &str,
        id: &ProposalId,
        offer: &Offer,
        keysets: impl Fn(&[u8; 32]) -> Option<&'k Keyset>,
    ) -> Result<Release, Error> {
        let (proposal, notes) = offer.decode(&keysets).map_err(invalid)?;
        let keyset = keysets(proposal.keyset_id())
            .ok_or_else(|| Error::Invalid("a send under a keyset its hub no longer has".into()))?;
        let grant = match self.post::<Offered>(url, licash_api::OFFER, offer).await {
            Ok(offered) => offered.decode().map_err(|p| unavailable(url, p))?,
            Err(Error::Refused { problem, .. }) => {
                return Err(self.refused(url, id, problem, Step::Offer).await?);
            }
            Err(e) => return Err(e),
        };
        grant
            .verify(keyset, &proposal)
            .map_err(|e| unavailable(url, format!("its offer: {e}")))?;
        let signed = self.sign_as_owner(
            keyset,
            proposal,
            &notes,
            Authorization::Granted(Box::new(grant)),
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
            return Err(Error::Abandoned {
                id: hex::encode(id.as_bytes()),
                reason,
            });
        }
        serde_json::from_str(&release).map_err(invalid)
    }

    /// The hub's unlock for the signed `release`, checked to open its
    /// grant. A hub that spent an input otherwise will never release it:
    /// the send is given up. Any other refusal leaves it pending.
    async fn release(
        &self,
        url: &str,
        id: &ProposalId,
        release: &Release,
        keyset: &Keyset,
        grant: &Grant,
    ) -> Result<Unlock, Error> {
        let released = match self
            .post::<Released>(url, licash_api::RELEASE, release)
            .await
        {
            Ok(released) => released,
            Err(Error::Refused { problem, .. }) if problem.code == Code::Spent => {
                return Err(self.refused(url, id, problem, Step::Release).await?);
            }
            Err(Error::Refused { problem, .. }) => {
                return Err(Error::Pending {
                    id: hex::encode(id.as_bytes()),
                    reason: format!("hub {url} refused its release: {problem}"),
                });
            }
            Err(e) => return Err(e),
        };
        let unlock = released.decode().map_err(|p| unavailable(url, p))?;
        grant
            .verify_unlock(keyset, &unlock)
            .map_err(|e| unavailable(url, format!("its unlock: {e}")))?;
        Ok(unlock)
    }

    /// Give payment `id` up on its hub's refusal `problem` of `step`, unless
    /// it went past it: its notes freed, but those the hub shows spent
    /// dropped. The error to answer with.
    pub(crate) async fn refused(
        &self,
        url: &str,
        id: &ProposalId,
        problem: Problem,
        step: Step,
    ) -> Result<Error, Error> {
        let gone = if problem.code == Code::Spent {
            self.spent_inputs(url, id).await
        } else {
            Vec::new()
        };
        let reason = problem.to_string();
        if self.give_up(step, id, &reason, &gone)? {
            return Ok(Error::Refused {
                hub: url.to_owned(),
                problem,
            });
        }
        // Another process took it on, or gave it up, meanwhile.
        let abandoned: Option<String> = self
            .store
            .read(|c| Ok(c.query_row(step.abandoned(), [id.as_bytes()], |r| r.get(0))?))?;
        let id = hex::encode(id.as_bytes());
        Ok(match abandoned {
            Some(reason) => Error::Abandoned { id, reason },
            None => Error::Pending { id, reason },
        })
    }

    /// The inputs of payment `id` its hub shows spent: none if it cannot
    /// tell.
    async fn spent_inputs(&self, url: &str, id: &ProposalId) -> Vec<[u8; 33]> {
        let inputs = self.store.read(|c| {
            let mut stmt = c.prepare("SELECT y FROM notes WHERE spent_by = ?1")?;
            let rows = stmt.query_map([id.as_bytes()], |r| r.get(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        });
        match inputs {
            Ok(inputs) => self.spent(url, &inputs).await,
            Err(e) => Err(e),
        }
        .unwrap_or_else(|e| {
            tracing::warn!(%url, error = %e, "cannot tell which notes are spent");
            Vec::new()
        })
    }

    /// Those of the notes `ys` the hub at `url` shows spent.
    async fn spent(&self, url: &str, ys: &[[u8; 33]]) -> Result<Vec<[u8; 33]>, Error> {
        let mut spent = Vec::new();
        for ys in ys.chunks(MAX_CHECK) {
            let check = Check::new(&ys.iter().map(|y| Point(*y)).collect::<Vec<_>>());
            let checked: Checked = self.post(url, licash_api::CHECK, &check).await?;
            if checked.spent.len() != ys.len() {
                return Err(unavailable(url, "it checked other notes than asked"));
            }
            spent.extend(ys.iter().zip(checked.spent).filter(|s| s.1).map(|s| *s.0));
        }
        Ok(spent)
    }

    /// Give payment `id` up at `step`, unless it went past it, and free its
    /// notes but drop those `gone`.
    fn give_up(
        &self,
        step: Step,
        id: &ProposalId,
        reason: &str,
        gone: &[[u8; 33]],
    ) -> Result<bool, Error> {
        let given_up = self.store.write(|tx| {
            let given_up = tx.execute(step.give_up(), params![id.as_bytes(), reason])? == 1;
            if given_up {
                for y in gone {
                    tx.execute(
                        "UPDATE notes SET gone_at = ?3 WHERE y = ?1 AND spent_by = ?2",
                        params![y, id.as_bytes(), now()],
                    )?;
                }
                tx.execute(
                    "UPDATE notes SET spent_by = NULL WHERE spent_by = ?1",
                    [id.as_bytes()],
                )?;
            }
            Ok(given_up)
        })?;
        if given_up {
            let id = hex::encode(id.as_bytes());
            tracing::warn!(%id, reason, gone = gone.len(), "payment given up");
        }
        Ok(given_up)
    }

    /// Finish every payment under way, or give up those their hubs refuse,
    /// and take every lock, invoice and swap under way on.
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
                Ok(payment) => resumed.paid.push(Paid { id: key, payment }),
                Err(Error::Refused { problem, .. }) => {
                    resumed.abandoned.push((key, problem.to_string()));
                }
                Err(Error::Abandoned { reason, .. }) => resumed.abandoned.push((key, reason)),
                Err(Error::Pending { reason, .. }) => resumed.pending.push((key, reason)),
                Err(e) => resumed.pending.push((key, e.to_string())),
            }
        }
        let ids: Vec<[u8; 32]> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM locks WHERE done_at IS NULL AND abandoned IS NULL
                 ORDER BY created_at",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        for id in ids {
            let key = hex::encode(id);
            match self.advance_lock(&ProposalId::from_bytes(id)).await {
                Ok(lock) => resumed.locks.push(lock),
                Err(Error::Refused { problem, .. }) => {
                    resumed.abandoned.push((key, problem.to_string()));
                }
                Err(Error::Abandoned { reason, .. }) => resumed.abandoned.push((key, reason)),
                Err(Error::Pending { reason, .. }) => resumed.pending.push((key, reason)),
                Err(e) => resumed.pending.push((key, e.to_string())),
            }
        }
        for hash in self.open_invoices()? {
            match self.advance_invoice(&hash).await {
                Ok(invoice) => resumed.invoices.push(invoice),
                Err(Error::Pending { reason, .. }) => {
                    resumed.pending.push((hex::encode(hash), reason));
                }
                Err(e) => resumed.pending.push((hex::encode(hash), e.to_string())),
            }
        }
        for hash in self.open_deposits()? {
            match self.advance_deposit(&hash).await {
                Ok(deposit) => resumed.deposits.push(deposit),
                Err(Error::Pending { reason, .. }) => {
                    resumed.pending.push((hex::encode(hash), reason));
                }
                Err(e) => resumed.pending.push((hex::encode(hash), e.to_string())),
            }
        }
        for hash in self.open_swaps()? {
            match self.advance_swap(&hash).await {
                Ok(swap) => resumed.swaps.push(swap),
                Err(Error::Pending { reason, .. }) => {
                    resumed.pending.push((hex::encode(hash), reason));
                }
                Err(e) => resumed.pending.push((hex::encode(hash), e.to_string())),
            }
        }
        Ok(resumed)
    }

    /// Give up the payment `id`, not signed yet, and free its notes: for a
    /// hub gone for good. A signed payment its hub may have released.
    pub fn abandon(&self, id: &str) -> Result<(), Error> {
        let id = payment_id(id)?;
        if self.give_up(Step::Offer, &id, "abandoned by its owner", &[])? {
            return Ok(());
        }
        Err(Error::Invalid(
            "not a payment under way and unsigned: its hub may have released it".into(),
        ))
    }

    /// The payment `id`, once done, to hand its payee again.
    pub fn payment(&self, id: &str) -> Result<Payment, Error> {
        let key = id;
        let id = payment_id(id)?;
        type Row = (
            i64,
            String,
            Option<String>,
            Option<[u8; 32]>,
            Option<String>,
        );
        let row: Option<Row> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT s.hub, r.url, s.release, s.unlock, s.abandoned
                 FROM sends s JOIN reach r ON r.hub = s.hub WHERE s.id = ?1",
                [id.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?)
        })?;
        let (hub, url, release, unlock, abandoned) =
            row.ok_or_else(|| Error::UnknownPayment(key.to_owned()))?;
        let id = hex::encode(id.as_bytes());
        let (release, unlock) = match (release, unlock, abandoned) {
            (_, _, Some(reason)) => return Err(Error::Abandoned { id, reason }),
            (Some(release), Some(unlock), None) => (release, unlock),
            _ => {
                let reason = "under way".into();
                return Err(Error::Pending { id, reason });
            }
        };
        let release: Release = serde_json::from_str(&release).map_err(invalid)?;
        let keysets = self.keysets(hub)?;
        let (signed, notes) = release
            .decode(|id| keysets.iter().find(|k| k.id() == id))
            .map_err(invalid)?;
        Ok(Payment::new(&url, &signed, &notes, &unlock))
    }

    /// The payments made, newest first.
    pub fn sends(&self) -> Result<Vec<Sent>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT s.id, r.url, s.payee, s.amount, s.fee, s.release IS NOT NULL,
                    s.done_at IS NOT NULL, s.abandoned, s.created_at
                 FROM sends s JOIN reach r ON r.hub = s.hub
                 ORDER BY s.created_at DESC, s.rowid DESC",
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

/// Payments taken, unlinking and checking.
impl<T: Transport> Wallet<T> {
    /// Take `payment`: keep the notes it pays this wallet, its unlock
    /// checked to open them. It must be under a keyset of a hub added
    /// here, as the wallet takes no hub's notes unasked.
    pub fn receive(&self, payment: &Payment) -> Result<Received, Error> {
        let id: [u8; 32] = payment
            .release
            .keyset
            .0
            .as_slice()
            .try_into()
            .map_err(invalid)?;
        let known: Option<(Vec<u8>, String)> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT k.bytes, r.url FROM keysets k JOIN reach r ON r.hub = k.hub
                 WHERE k.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })?;
        let (keyset, url) = known.ok_or_else(|| Error::UnknownHub(payment.hub.clone()))?;
        let keyset = Keyset::from_bytes(&keyset)?;
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
        self.keep(&keyset, url, proposal, &ours)
    }

    /// Keep the notes `ours` of `proposal` paid this wallet, with a
    /// receipt: unless they are kept already.
    fn keep(
        &self,
        keyset: &Keyset,
        url: String,
        proposal: &Proposal,
        ours: &[(licash::address::Received, Note)],
    ) -> Result<Received, Error> {
        if ours.is_empty() {
            return Err(Error::PaysNothing);
        }
        let (pid, ephemeral) = (proposal.id(), proposal.ephemeral());
        let received_at = now();
        let amount = self.store.write(|tx| {
            let mut amount = 0u64;
            for (r, note) in ours {
                if insert_note(tx, keyset, note, &pid, &ephemeral, r.branch, r.position)? {
                    amount += note.message().denomination();
                }
            }
            if amount == 0 {
                return Err(Error::AlreadyReceived);
            }
            tx.execute(
                "INSERT INTO receipts (id, keyset, amount, received_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT DO NOTHING",
                params![pid.as_bytes(), keyset.id(), sql(amount), received_at],
            )?;
            Ok(amount)
        })?;
        let id = hex::encode(pid.as_bytes());
        tracing::info!(%id, amount, "received");
        Ok(Received {
            id,
            hub: url,
            amount,
            received_at,
        })
    }

    /// The payments taken, newest first.
    pub fn receipts(&self) -> Result<Vec<Received>, Error> {
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT p.id, r.url, p.amount, p.received_at
                 FROM receipts p JOIN keysets k ON k.id = p.keyset JOIN reach r ON r.hub = k.hub
                 ORDER BY p.received_at DESC, p.rowid DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Received {
                    id: hex::encode(r.get::<_, [u8; 32]>(0)?),
                    hub: r.get(1)?,
                    amount: r.get::<_, i64>(2)? as u64,
                    received_at: r.get(3)?,
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    /// Ask each hub which notes held are spent, and drop those: after
    /// restoring an old copy of the wallet file, whose notes a newer copy
    /// may have spent. A hub learns that one party holds the notes asked
    /// about. The amount dropped.
    pub async fn check(&self) -> Result<u64, Error> {
        let held: Vec<(String, [u8; 33])> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT r.url, n.y FROM notes n
                 JOIN keysets k ON k.id = n.keyset JOIN reach r ON r.hub = k.hub
                 WHERE n.spent_by IS NULL AND n.gone_at IS NULL ORDER BY r.url",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        let mut dropped = 0;
        for at in held.chunk_by(|a, b| a.0 == b.0) {
            let url = &at[0].0;
            let ys: Vec<[u8; 33]> = at.iter().map(|(_, y)| *y).collect();
            let spent = self.spent(url, &ys).await?;
            dropped += self.store.write(|tx| {
                let mut dropped = 0;
                for y in &spent {
                    let denom: Option<i64> = tx
                        .query_row(
                            "UPDATE notes SET gone_at = ?2, session = NULL
                             WHERE y = ?1 AND spent_by IS NULL AND gone_at IS NULL
                             RETURNING denom",
                            params![y, now()],
                            |r| r.get(0),
                        )
                        .optional()?;
                    dropped += denom.map_or(0, |d| d as u64);
                }
                Ok(dropped)
            })?;
            if !spent.is_empty() {
                tracing::warn!(%url, notes = spent.len(), "notes spent by a copy of the wallet dropped");
            }
        }
        Ok(dropped)
    }

    /// Unlink the notes held linked to their issuance, so that their hub
    /// cannot tell which payment one came from when it is spent. A note
    /// its hub refuses to unlink stays linked. The number unlinked; on an
    /// error, those before it are kept and the rest are asked again next
    /// time.
    pub async fn unlink(&self) -> Result<usize, Error> {
        let todo: Vec<([u8; 33], String)> = self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT n.y, r.url FROM notes n
                 JOIN keysets k ON k.id = n.keyset JOIN reach r ON r.hub = k.hub
                 WHERE n.unlinked = 0 AND n.spent_by IS NULL AND n.gone_at IS NULL
                 ORDER BY n.session IS NULL, n.added_at",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        let mut unlinked = 0;
        for (y, url) in todo {
            if self.unlink_one(&url, &y).await? {
                unlinked += 1;
            }
        }
        Ok(unlinked)
    }

    async fn unlink_one(&self, url: &str, y: &[u8; 33]) -> Result<bool, Error> {
        let row: Option<(Vec<u8>, Option<String>)> = self.store.read(|c| {
            Ok(c.query_row(
                "SELECT k.bytes, n.session FROM notes n JOIN keysets k ON k.id = n.keyset
                 WHERE n.y = ?1 AND n.unlinked = 0 AND n.spent_by IS NULL AND n.gone_at IS NULL",
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
            .post::<Unlinked>(url, licash_api::UNLINK, &Unlink::new(session.request()))
            .await
        {
            Ok(answer) => answer.decode().map_err(|p| unavailable(url, p))?,
            // It answers the same request the same: it will never unlink it.
            Err(Error::Refused { problem, .. }) => {
                tracing::info!(%url, %problem, "a note stays linked");
                self.stay_linked(y)?;
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        let note = match session.finish(&keyset, &answer) {
            Ok(note) if note.message().identifier()?.0 == *y => note,
            _ => {
                tracing::warn!(%url, "its unlink answer does not open: a note stays linked");
                self.stay_linked(y)?;
                return Ok(false);
            }
        };
        self.store.write(|tx| {
            tx.execute(
                "UPDATE notes SET note = ?2, unlinked = 1, session = NULL WHERE y = ?1",
                params![y, note.to_bytes()],
            )?;
            Ok(())
        })?;
        Ok(true)
    }

    fn stay_linked(&self, y: &[u8; 33]) -> Result<(), Error> {
        self.store.write(|tx| {
            tx.execute(
                "UPDATE notes SET unlinked = -1, session = NULL WHERE y = ?1",
                [y],
            )?;
            Ok(())
        })
    }

    /// `proposal`, spending the wallet's `notes`, signed by their owner.
    fn sign_as_owner(
        &self,
        keyset: &Keyset,
        proposal: Proposal,
        notes: &[Note],
        authorization: Authorization,
    ) -> Result<SignedProposal, Error> {
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
        Ok(SignedProposal::sign(
            keyset,
            proposal,
            notes,
            authorization,
            &owners,
        )?)
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

/// Take the notes `ys` for payment `id`: none taken if another payment
/// took one meanwhile.
fn take(tx: &Transaction<'_>, id: &ProposalId, ys: &[[u8; 33]]) -> Result<(), Error> {
    for y in ys {
        let taken = tx.execute(
            "UPDATE notes SET spent_by = ?1
             WHERE y = ?2 AND spent_by IS NULL AND session IS NULL AND gone_at IS NULL",
            params![id.as_bytes(), y],
        )?;
        if taken != 1 {
            return Err(Error::NoteTaken);
        }
    }
    Ok(())
}

/// The outputs of `branch` paying each of `parts` its value, under
/// `keyset`, derived from `ephemeral`: largest first, and equal ones in
/// random order, so that their order tells no one whose they are.
fn outputs(
    keyset: &Keyset,
    ephemeral: &Ephemeral,
    branch: Branch,
    parts: &[(&Address, u64)],
) -> Result<Vec<Output>, Error> {
    let mut outputs = Vec::new();
    for &(to, value) in parts {
        let denoms = coins::split(value, keyset.denominations())
            .ok_or_else(|| Error::Invalid(format!("the keyset cannot make {value}")))?;
        outputs.extend(
            denoms
                .into_iter()
                .map(|denom| (OsRng.next_u64(), denom, to)),
        );
    }
    outputs.sort_by_key(|&(order, denom, _)| (Reverse(denom), order));
    Ok(outputs
        .iter()
        .enumerate()
        .map(|(position, &(_, denom, to))| {
            Output::from_derivation(&ephemeral.derive(to, denom, branch, position)?)
        })
        .collect::<Result<Vec<_>, _>>()?)
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

fn payment_id(id: &str) -> Result<ProposalId, Error> {
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

/// The hub URL `url`, of plain HTTP, over HTTPS instead: not of an onion
/// service, which Tor authenticates already.
fn secure_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let host = rest.split(['/', ':']).next().unwrap_or(rest);
    (!host.ends_with(".onion")).then(|| format!("https://{rest}"))
}
