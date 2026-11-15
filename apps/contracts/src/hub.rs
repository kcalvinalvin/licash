//! The hub's side of openings: its [`Offer`], each opening kept in
//! `openings.sqlite`, F funded and signed by its node's wallet, and the
//! routes a holder reaches it at ([`router`]).
//!
//! Each opening is one machine, stepped by one task at a time. A step is
//! stored before its effects run, and an effect's result (F funded, F
//! signed) is stored as the step it feeds: the holder sending a message
//! again gets the answer the stored machine gives ([`Hub::pending`]), and
//! effects cut short by a restart run again then.

mod node;
mod run;
mod store;
pub mod terms;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{ensure, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use licash::note::Keyset;
use licash::spend::ChainProfile;
use licash_chain_client::Source;
pub use licash_contract::opening::HubPhase;
use licash_contract::opening::{Context, Hub, Trust};

use crate::{Frame, Offer, INSTANCE, MAX_BODY, OPEN, QUOTE, STEP};
use store::Store;
use terms::Timing;

/// The most openings short of the hub's signatures at once.
pub const MAX_UNDER_WAY: usize = 16;

/// How long an opening short of the hub's signatures may wait for the
/// holder's next message before it is given up, its coins freed.
pub const IDLE: Duration = Duration::from_secs(3_600);

/// How long a request waits for its step before answering 202.
const WAIT: Duration = Duration::from_secs(20);

/// A hub's openings, with its node, whose wallet funds and signs F.
pub struct Openings {
    store: Mutex<Store>,
    /// The chain followed: its tip, checkpoints and fee rates, and
    /// broadcasting.
    source: Source,
    /// The node whose wallet funds and signs F, and locks its coins.
    wallet: Source,
    keyset: Keyset,
    trust: Trust,
    timing: Timing,
    /// The openings a task is stepping.
    busy: Mutex<HashSet<[u8; 32]>>,
    /// Held from funding an F to locking its coins: no two take the same.
    funding: tokio::sync::Mutex<()>,
}

/// Why a request was not answered with frames.
#[derive(Debug)]
pub struct Refused {
    pub status: StatusCode,
    pub reason: String,
}

impl Refused {
    pub(crate) fn bad(reason: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            reason: format!("{reason:#}"),
        }
    }

    pub(crate) fn unavailable(reason: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            reason: format!("{reason:#}"),
        }
    }
}

/// What a message is answered with.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Frames(Vec<Frame>),
    /// Still working on it: send it again later.
    Working,
}

/// How many openings in `data` the hub signed: a hub run without its
/// contracts warns of them. Nothing is made in `data`.
pub fn signed(data: &Path) -> Result<u64> {
    Store::signed(data)
}

/// An opening as the hub's operator sees it ([`openings`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub id: [u8; 32],
    /// How far it went.
    pub phase: Option<HubPhase>,
    /// Opened under mock proofs: it protects nothing.
    pub mock: bool,
    /// The hub signed F: the contract may stand, and is never given up.
    pub signed: bool,
    /// Given up short of the hub's signatures, its coins freed.
    pub abandoned: bool,
    /// Seconds since 1970.
    pub created_at: u64,
    pub updated_at: u64,
    /// Its terms, unchecked: none if its stored state does not read.
    pub terms: Option<Terms>,
}

/// What an opening's stored state says of it ([`Hub::peek`]), unchecked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terms {
    /// The most of the holder's notes it protects, in sat.
    pub cap: u64,
    /// The height its graph closes at, which ends the contract.
    pub close: u32,
    /// The hub's fee for it, in sat.
    pub fee: u64,
    /// Her reserve, from her deposit, in sat.
    pub reserve: u64,
    /// The value its graph holds, in sat: its collateral, and her reserve.
    pub graph: u64,
    /// F's id, from the hub's commitment to F on.
    pub funding: Option<bitcoin::Txid>,
}

/// The openings in `data`, oldest first, read only: none if no opening
/// was ever stored there. Their secrets stay in their states.
pub fn openings(data: &Path) -> Result<Vec<Summary>> {
    let rows = Store::listed(data)?;
    Ok(rows
        .into_iter()
        .map(|row| Summary {
            id: row.id,
            phase: HubPhase::ALL.get(usize::from(row.phase)).copied(),
            mock: row.mock,
            signed: row.irrevocable,
            abandoned: row.abandoned,
            created_at: row.created_at.max(0) as u64,
            updated_at: row.updated_at.max(0) as u64,
            terms: (row.format == store::FORMAT)
                .then(|| peek(&row.state))
                .flatten(),
        })
        .collect())
}

fn peek(state: &[u8]) -> Option<Terms> {
    let peek = Hub::peek(state).ok()?;
    let p = peek.base.parts();
    Some(Terms {
        cap: u64::from(p.cap),
        close: p.graph.close_height,
        fee: p.fees.opening_fee_quote,
        reserve: p.graph.reserve_alice,
        graph: p.graph.graph_value().ok()?,
        funding: peek.funding,
    })
}

impl Openings {
    /// The openings in `data`, opened under `keyset` and funded by the
    /// wallet of the node `source` names (its JSON-RPC), as
    /// [`Openings::with_wallet`].
    pub async fn new(data: &Path, source: Source, keyset: Keyset) -> Result<Arc<Self>> {
        Self::with_wallet(data, source.clone(), source, keyset).await
    }

    /// The openings in `data` on the chain `source` follows, opened under
    /// `keyset` and funded by the node's wallet `wallet` names (its
    /// JSON-RPC, as `http://host:port/wallet/NAME`), which may be another
    /// node's on the same network. Refused on a network the hub has no
    /// terms or programs for: [`Timing::of`], [`crate::trust`].
    pub async fn with_wallet(
        data: &Path,
        source: Source,
        wallet: Source,
        keyset: Keyset,
    ) -> Result<Arc<Self>> {
        let profile: ChainProfile = source.profile().await?;
        let trust = crate::trust(profile)?;
        let timing = Timing::of(profile)?;
        // Not fatal: a node may load its wallet after the hub starts. F
        // goes unfunded, and its openings given up, until it does. Fatal
        // on another network: no F it funds could be the chain's.
        let info = wallet.rpc::<serde_json::Value>("getwalletinfo", serde_json::json!([]));
        if let Err(e) = info.await {
            tracing::warn!("contracts need the node's wallet, to fund F: {e:#}");
        }
        match wallet.profile().await {
            Ok(theirs) => ensure!(
                theirs == profile,
                "the contracts' wallet, {wallet}, is on {theirs}, the chain on {profile}"
            ),
            Err(e) => tracing::warn!("the contracts' wallet, {wallet}: {e:#}"),
        }
        let store = Store::open(data)?;
        let openings = Arc::new(Self {
            store: Mutex::new(store),
            source,
            wallet,
            keyset,
            trust,
            timing,
            busy: Mutex::new(HashSet::new()),
            funding: tokio::sync::Mutex::new(()),
        });
        // A node restarted forgets the coins it locked without persisting.
        let locked = openings.store().locked()?;
        openings.lock_coins(&locked).await;
        Ok(openings)
    }

    /// The hub's offer now: its quote, checkpointed at its tip.
    pub async fn offer(&self) -> Result<Offer> {
        let tip = self.source.tip().await?.height;
        let checkpoint = self.source.checkpoint(tip).await?;
        Ok(Offer {
            quote: self.timing.quote(self.keyset.clone(), checkpoint),
            bounds: self.timing.bounds,
        })
    }

    /// What the hub's info says of its contracts.
    pub fn advertise(&self) -> licash_api::Contracts {
        licash_api::Contracts {
            cap: terms::CAP,
            price: terms::RESERVE_ALICE + terms::OPENING_FEE,
            blocks: self.timing.bounds.lock,
            mock: self.trust.mock,
        }
    }

    /// Give up the openings short of the hub's signatures idle for
    /// [`IDLE`], freeing their coins: how many.
    pub async fn sweep(&self) -> Result<usize> {
        let before = crate::now() - IDLE.as_secs() as i64;
        let stale = self.store().stale(before)?;
        for (id, coins) in &stale {
            if self.busy().contains(id) {
                continue;
            }
            self.store().abandon(id)?;
            self.unlock_coins(coins).await;
            tracing::info!(id = %crate::hex_id(id), "an opening given up, idle");
        }
        Ok(stale.len())
    }

    /// Sweep every minute, for as long as the hub runs.
    pub fn spawn_sweeper(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if let Err(e) = this.sweep().await {
                    tracing::error!("giving up idle openings: {e:#}");
                }
            }
        });
    }

    async fn context(&self) -> Result<Context, Refused> {
        let tip = self.source.tip().await.map_err(Refused::unavailable)?;
        Ok(Context {
            tip: tip.height,
            trust: self.trust,
        })
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn busy(&self) -> MutexGuard<'_, HashSet<[u8; 32]>> {
        self.busy.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The routes of `openings`, at [`QUOTE`], [`OPEN`], [`STEP`] and
/// [`INSTANCE`].
pub fn router(openings: Arc<Openings>) -> Router {
    Router::new()
        .route(QUOTE, get(quote))
        .route(OPEN, post(open))
        .route(STEP, post(step))
        .route(INSTANCE, get(instance))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(openings)
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        (self.status, self.reason).into_response()
    }
}

impl IntoResponse for Answer {
    fn into_response(self) -> Response {
        match self {
            Answer::Frames(frames) => (StatusCode::OK, Frame::encode_all(&frames)).into_response(),
            Answer::Working => StatusCode::ACCEPTED.into_response(),
        }
    }
}

async fn quote(State(openings): State<Arc<Openings>>) -> Result<Vec<u8>, Refused> {
    let offer = openings.offer().await.map_err(Refused::unavailable)?;
    Ok(offer.encode())
}

async fn open(State(openings): State<Arc<Openings>>, body: Bytes) -> Result<Answer, Refused> {
    openings.open(&body).await
}

async fn step(
    State(openings): State<Arc<Openings>>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> Result<Answer, Refused> {
    let id = crate::parse_id(&id).map_err(Refused::bad)?;
    openings.step(id, body.to_vec()).await
}

async fn instance(
    State(openings): State<Arc<Openings>>,
    UrlPath((id, stage, index)): UrlPath<(String, usize, usize)>,
) -> Result<Vec<u8>, Refused> {
    let id = crate::parse_id(&id).map_err(Refused::bad)?;
    openings.instance(id, stage, index).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hub's opening at Funded, as `licash_contract`'s golden test keeps
    /// it: of a mock opening on regtest.
    const FUNDED: &[u8] =
        include_bytes!("../../../crates/licash-contract/tests/golden/hub-funded.bin");

    fn deposit(vout: u32) -> bitcoin::OutPoint {
        use bitcoin::hashes::Hash as _;
        bitcoin::OutPoint {
            txid: bitcoin::Txid::from_byte_array([9; 32]),
            vout,
        }
    }

    #[test]
    fn openings_read_as_stored_with_their_terms_and_none_are_made() {
        let dir = tempfile::tempdir().unwrap();
        assert!(openings(dir.path()).unwrap().is_empty());
        assert!(!dir.path().join("openings.sqlite").exists());
        let store = Store::open(dir.path()).unwrap();
        assert!(openings(dir.path()).unwrap().is_empty());
        store
            .insert(&[1; 32], 0, true, &deposit(0), &[0; 32], FUNDED)
            .unwrap();
        store
            .save(&[1; 32], HubPhase::Funded as u8, true, &[0; 32], FUNDED)
            .unwrap();
        // A state that does not read, given up.
        store
            .insert(&[2; 32], 0, true, &deposit(1), &[0; 32], b"state")
            .unwrap();
        store.abandon(&[2; 32]).unwrap();
        let all = openings(dir.path()).unwrap();
        let [funded, abandoned] = &all[..] else {
            panic!("two openings, not {all:?}");
        };
        assert_eq!(funded.id, [1; 32]);
        assert_eq!(funded.phase, Some(HubPhase::Funded));
        assert!(funded.mock && funded.signed && !funded.abandoned);
        assert!(funded.updated_at >= funded.created_at && funded.created_at > 0);
        let terms = funded.terms.as_ref().unwrap();
        let peek = Hub::peek(FUNDED).unwrap();
        let parts = peek.base.parts();
        assert_eq!(
            (terms.cap, terms.close, terms.fee, terms.reserve),
            (
                u64::from(parts.cap),
                parts.graph.close_height,
                parts.fees.opening_fee_quote,
                parts.graph.reserve_alice
            )
        );
        // A 21-bit graph, her reserve 20 000 sat and the hub's 100 000.
        assert_eq!(
            (
                terms.cap,
                terms.close,
                terms.fee,
                terms.reserve,
                terms.graph
            ),
            (4_096, 504, 3_000, 20_000, (1 << 21) - 1 + 120_000)
        );
        assert_eq!(terms.funding, peek.funding);
        assert!(terms.funding.is_some());
        assert_eq!(abandoned.phase, Some(HubPhase::Termed));
        assert!(abandoned.abandoned && !abandoned.signed);
        assert_eq!(abandoned.terms, None);
    }
}
