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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{ensure, Context as _, Result};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use licash::note::Keyset;
use licash::spend::ChainProfile;
use licash_chain_client::Source;
pub use licash_contract::opening::HubPhase;
use licash_contract::opening::{Context, Hub, Trust};

use crate::{Frame, Offer, INSTANCE, MAX_BODY, OPEN, QUOTE, STEP};
use store::Store;
use terms::{Range, Timing};

/// The most openings short of the hub's signatures at once.
pub const MAX_UNDER_WAY: usize = 16;

/// The most steps of openings and instances built at once. Each takes
/// hundreds of MB, and anyone may start an opening (a confirmed coin is all
/// it asks): one at a time bounds the hub's memory, the others waiting
/// their turn (answered 202 meanwhile).
pub const HEAVY: usize = 1;

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
    /// A permit for each step or instance built at once: [`HEAVY`].
    heavy: Arc<tokio::sync::Semaphore>,
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
        Self::with_wallet(data, source.clone(), source, keyset, None).await
    }

    /// The openings in `data` on the chain `source` follows, opened under
    /// `keyset` and funded by the node's wallet `wallet` names (its
    /// JSON-RPC, as `http://host:port/wallet/NAME`), which may be another
    /// node's on the same network. Holders choose their terms within
    /// `range`, if any, else open on its fixed ones. Refused on a network
    /// the hub has no terms or programs for: [`Timing::of`],
    /// [`crate::trust`].
    pub async fn with_wallet(
        data: &Path,
        source: Source,
        wallet: Source,
        keyset: Keyset,
        range: Option<Range>,
    ) -> Result<Arc<Self>> {
        let profile: ChainProfile = source.profile().await?;
        let trust = crate::trust(profile)?;
        let timing = Timing::of(profile, range)?;
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
            heavy: Arc::new(tokio::sync::Semaphore::new(HEAVY)),
            funding: tokio::sync::Mutex::new(()),
        });
        // A node restarted forgets the coins it locked without persisting.
        let locked = openings.store().locked()?;
        openings.lock_coins(&locked).await;
        Ok(openings)
    }

    /// The hub's offer now, checkpointed at its tip: its quote of `cap`
    /// sat over `blocks` she chose, if within its terms, else its fixed
    /// one.
    pub async fn offer(&self, chosen: Option<(u32, u32)>) -> Result<Offer, Refused> {
        let (cap, blocks) = chosen.unwrap_or((terms::CAP, terms::LOCK));
        self.timing.allows(cap, blocks).map_err(Refused::bad)?;
        let tip = self.source.tip().await.map_err(Refused::unavailable)?;
        let checkpoint = self.source.checkpoint(tip.height).await;
        let checkpoint = checkpoint.map_err(Refused::unavailable)?;
        let quote = self
            .timing
            .quote_for(self.keyset.clone(), checkpoint, cap, blocks);
        let bounds = self.timing.bounds(&quote);
        Ok(Offer { quote, bounds })
    }

    /// What the hub's info says of its contracts: its fixed terms, as
    /// apps installed open on, and those holders may choose, if any.
    pub fn advertise(&self) -> licash_api::Contracts {
        let range = self.timing.range();
        licash_api::Contracts {
            cap: terms::CAP,
            price: self.timing.price(terms::CAP, terms::LOCK),
            blocks: terms::LOCK,
            mock: self.trust.mock,
            range,
        }
    }

    /// Give up the openings short of the hub's signatures idle for
    /// [`IDLE`], freeing their coins: how many.
    pub async fn sweep(&self) -> Result<usize> {
        let before = crate::now() - IDLE.as_secs() as i64;
        let stale = self.store().stale(before)?;
        let mut abandoned = 0;
        for id in &stale {
            let coins = self.abandon_idle(id, before)?;
            let Some(coins) = coins else { continue };
            self.unlock_coins(&coins).await;
            abandoned += 1;
            tracing::info!(id = %crate::hex_id(id), "an opening given up, idle");
        }
        Ok(abandoned)
    }

    fn abandon_idle(&self, id: &[u8; 32], before: i64) -> Result<Option<Vec<bitcoin::OutPoint>>> {
        // A step takes this same mark before loading its row. Hold the
        // mutex through the conditional write, never through node I/O.
        let busy = self.busy();
        if busy.contains(id) {
            return Ok(None);
        }
        self.store().abandon(id, before)
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

    /// Its plain answer to `r`, the holder's request on Bitcoin in
    /// opening `id`, to broadcast: her proof decides then. Refused for an
    /// opening not funded, given up or unknown, or a transaction not its
    /// request. No answer is made by the hub on its own yet: an operator
    /// (or a test) asks for it.
    pub async fn respond(
        &self,
        id: &[u8; 32],
        r: &bitcoin::Transaction,
    ) -> Result<bitcoin::Transaction> {
        let ctx = self
            .context()
            .await
            .map_err(|e| anyhow::anyhow!(e.reason))?;
        let row = self.store().row(id)?;
        answer(row, &ctx, id, r)
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

/// The plain answer to `r` in opening `id` of the openings in `data`, as
/// [`Openings::respond`], on the chain `source` follows: read beside the
/// running hub, for its operator. Needs neither the hub's seed nor its
/// wallet, and writes nothing.
pub async fn respond(
    data: &Path,
    source: &Source,
    id: &[u8; 32],
    r: &bitcoin::Transaction,
) -> Result<bitcoin::Transaction> {
    let trust = crate::trust(source.profile().await?)?;
    let tip = source.tip().await?.height;
    answer(Store::read(data, id)?, &Context { tip, trust }, id, r)
}

fn answer(
    row: Option<store::Row>,
    ctx: &Context,
    id: &[u8; 32],
    r: &bitcoin::Transaction,
) -> Result<bitcoin::Transaction> {
    let row = row.with_context(|| format!("no opening {}", crate::hex_id(id)))?;
    ensure!(!row.abandoned, "an opening given up");
    Hub::load(&row.state, ctx)?.respond(r)
}

/// The routes of `openings`, at [`QUOTE`], [`OPEN`], [`STEP`] and
/// [`INSTANCE`].
pub fn router(openings: Arc<Openings>) -> Router {
    // Separate budgets let existing openings progress while new opens or
    // instance downloads are saturated. Four downloads preserve the
    // holder's normal parallel fetch. Body permits precede extraction and
    // last until the response body is dropped, not merely constructed.
    let budget =
        |count| middleware::from_fn_with_state(Arc::new(tokio::sync::Semaphore::new(count)), admit);
    Router::new()
        .route(QUOTE, get(quote).layer(budget(2)))
        .route(OPEN, post(open).layer(budget(2)))
        .route(STEP, post(step).layer(budget(2)))
        .route(INSTANCE, get(instance).layer(budget(4)))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(openings)
}

async fn admit(
    State(limit): State<Arc<tokio::sync::Semaphore>>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(permit) = limit.try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many contract requests: retry later",
        )
            .into_response();
    };
    let response = match tokio::time::timeout(Duration::from_secs(300), next.run(request)).await {
        Ok(response) => response,
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    response.map(|body| {
        Body::from_stream(body.into_data_stream().map(move |chunk| {
            let _held = &permit;
            chunk
        }))
    })
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

/// The hub's quote, of the terms she chose (`?cap=SAT&blocks=N`, both or
/// neither), else its fixed one.
async fn quote(
    State(openings): State<Arc<Openings>>,
    Query(asked): Query<HashMap<String, String>>,
) -> Result<Vec<u8>, Refused> {
    let number = |name: &str| {
        let value = asked.get(name).map(|v| v.parse::<u32>());
        value
            .transpose()
            .map_err(|e| Refused::bad(format!("{name}: {e}")))
    };
    let chosen = match (number("cap")?, number("blocks")?) {
        (Some(cap), Some(blocks)) => Some((cap, blocks)),
        (None, None) => None,
        _ => return Err(Refused::bad("a cap and blocks, both or neither")),
    };
    Ok(openings.offer(chosen).await?.encode())
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
    use tower::ServiceExt;

    #[tokio::test]
    async fn admission_precedes_body_buffering_and_lasts_through_response_delivery() {
        use axum::http::Request as HttpRequest;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let limit = Arc::new(tokio::sync::Semaphore::new(1));
        let app = Router::new()
            .route("/", post(|_: Bytes| async { "ok" }))
            .layer(middleware::from_fn_with_state(limit.clone(), admit));
        let response = app
            .clone()
            .oneshot(HttpRequest::post("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(limit.available_permits(), 0);
        let polled = Arc::new(AtomicUsize::new(0));
        let seen = polled.clone();
        let body = Body::from_stream(futures_util::stream::poll_fn(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::<Option<Result<Bytes, std::io::Error>>>::Pending
        }));
        let refused = app
            .clone()
            .oneshot(HttpRequest::post("/").body(body).unwrap())
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(polled.load(Ordering::SeqCst), 0);
        drop(response);
        assert_eq!(limit.available_permits(), 1);
        let response = app
            .oneshot(HttpRequest::post("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

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

    fn test_openings(dir: &Path) -> Openings {
        let profile = ChainProfile::Core30DefaultRegtest;
        let keyset = Hub::peek(FUNDED).unwrap().base.parts().keyset.clone();
        let source: Source = "http://127.0.0.1:1".parse().unwrap();
        Openings {
            store: Mutex::new(Store::open(dir).unwrap()),
            source: source.clone(),
            wallet: source,
            keyset,
            trust: crate::trust(profile).unwrap(),
            timing: Timing::of(profile, None).unwrap(),
            busy: Mutex::new(HashSet::new()),
            heavy: Arc::new(tokio::sync::Semaphore::new(HEAVY)),
            funding: tokio::sync::Mutex::new(()),
        }
    }

    #[test]
    fn sweeping_holds_the_busy_mutex_until_the_abandonment_write_completes() {
        let dir = tempfile::tempdir().unwrap();
        let openings = test_openings(dir.path());
        let id = [5; 32];
        let store = openings.store();
        store
            .insert(&id, 0, true, &deposit(0), &[0; 32], FUNDED)
            .unwrap();
        store.set_coins(&id, &[deposit(1)]).unwrap();
        std::thread::scope(|scope| {
            let sweeping = scope.spawn(|| openings.abandon_idle(&id, i64::MAX));
            let started = std::time::Instant::now();
            loop {
                match openings.busy.try_lock() {
                    Err(std::sync::TryLockError::WouldBlock) => break,
                    Ok(_) => {}
                    Err(e) => panic!("busy mutex: {e}"),
                }
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "sweep released busy before its write"
                );
                std::thread::yield_now();
            }
            drop(store);
            assert_eq!(sweeping.join().unwrap().unwrap(), Some(vec![deposit(1)]));
        });
    }

    #[test]
    fn sweeping_cannot_abandon_a_busy_opening_or_unlock_one_signed_since_selection() {
        let dir = tempfile::tempdir().unwrap();
        let openings = test_openings(dir.path());
        let id = [3; 32];
        openings
            .store()
            .insert(&id, 0, true, &deposit(0), &[0; 32], FUNDED)
            .unwrap();
        openings.store().set_coins(&id, &[deposit(1)]).unwrap();
        assert_eq!(openings.store().stale(i64::MAX).unwrap(), vec![id]);
        openings.busy().insert(id);
        assert!(openings.abandon_idle(&id, i64::MAX).unwrap().is_none());
        openings
            .store()
            .save(&id, HubPhase::Funded as u8, true, &[0; 32], FUNDED)
            .unwrap();
        openings.busy().remove(&id);
        assert!(openings.abandon_idle(&id, i64::MAX).unwrap().is_none());
        assert_eq!(openings.store().locked().unwrap(), vec![deposit(1)]);
        drop(openings);
        assert_eq!(
            Store::open(dir.path()).unwrap().locked().unwrap(),
            vec![deposit(1)]
        );
    }

    #[test]
    fn abandonment_rechecks_activity_and_returns_only_the_coins_actually_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let id = [4; 32];
        store
            .insert(&id, 0, true, &deposit(0), &[0; 32], FUNDED)
            .unwrap();
        store.set_coins(&id, &[deposit(1)]).unwrap();
        let db = rusqlite::Connection::open(dir.path().join("openings.sqlite")).unwrap();
        db.execute("UPDATE openings SET updated_at = 0", [])
            .unwrap();
        let before = crate::now() - 1;
        assert_eq!(store.stale(before).unwrap(), vec![id]);
        store.save(&id, 0, false, &[1; 32], FUNDED).unwrap();
        assert!(store.abandon(&id, before).unwrap().is_none());
        assert!(!store.row(&id).unwrap().unwrap().abandoned);
        // A candidate's coin snapshot is not an authorization to unlock it.
        store.set_coins(&id, &[deposit(2)]).unwrap();
        assert_eq!(
            store.abandon(&id, i64::MAX).unwrap(),
            Some(vec![deposit(2)])
        );
        assert!(store.abandon(&id, i64::MAX).unwrap().is_none());
        assert!(store.locked().unwrap().is_empty());
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
        store.abandon(&[2; 32], i64::MAX).unwrap();
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
