//! The hub's side of openings: its [`Offer`], each opening kept in
//! `openings.sqlite`, F funded and signed by its node's wallet, and the
//! routes a holder reaches it at ([`router`]).
//!
//! Each opening is one machine, stepped by one task at a time. A step is
//! stored before its effects run, and an effect's result (F funded, F
//! signed) is stored as the step it feeds: the holder sending a message
//! again gets the answer the stored machine gives ([`Hub::pending`]), and
//! effects cut short by a restart run again then.

mod dispute;
mod node;
mod prover;
mod responses;
mod run;
mod store;
pub mod terms;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
pub use dispute::{Evidencer, Played};
/// How the hub tells its operator what needs them ([`Openings::alert_with`]).
pub type Alert = dyn Fn(&str) + Send + Sync;
use futures_util::StreamExt;
use licash::note::Keyset;
use licash::spend::ChainProfile;
use licash_chain_client::Source;
pub use licash_contract::opening::HubPhase;
use licash_contract::opening::{Context, Hub, Trust};

use crate::{Frame, Offer, FOLD, FOLD_SIGN, INSTANCE, MAX_BODY, OPEN, QUOTE, STEP};
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

/// How long a request to the openings' routes may take, its body read.
const REQUEST_TIME: Duration = Duration::from_secs(300);

/// How long an instance's upload may take, its body read and stepped: 25
/// MB over Tor from a phone, at 15 KB/s at worst.
const INSTANCE_TIME: Duration = Duration::from_secs(30 * 60);

/// A hub's openings, with its node, whose wallet funds and signs F.
pub struct Openings {
    data: PathBuf,
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
    /// The responder's caches.
    watch: dispute::Watch,
    /// Where the responder finds its evidence against a filing.
    evidencer: std::sync::OnceLock<Arc<Evidencer>>,
    /// The prover of the hub's statement: none where proofs are the mock
    /// authority's.
    prover: Option<prover::Prover>,
    /// How the operator is told what needs them.
    alert: std::sync::OnceLock<Arc<Alert>>,
    /// The claims given no response this round, for want of the prover.
    withheld: Mutex<Vec<String>>,
    /// Whether more contracts are open than its prover can contest the
    /// exits of at once, as last told.
    over_capacity: std::sync::atomic::AtomicBool,
    /// Each contract's fold offered, until its holder signs or asks again.
    folds: Mutex<HashMap<[u8; 32], dispute::PendingFold>>,
    /// What the node's wallet last said it holds, in sat: the most it
    /// funds an F with ([`terms::fundable`]). None until it says.
    funds: Mutex<Option<u64>>,
}

/// One responder poll. A bad opening never hides the others; errors are
/// also logged with the opening id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResponseRound {
    /// Openings whose exit the hub contested or settled: a response, an
    /// assertion, an answer to a challenge, a reclaimed slot.
    pub answered: usize,
    /// Openings whose payout outputs of the hub's it swept to its wallet.
    pub swept: usize,
    /// Contracts it ended by default at their close.
    pub closed: usize,
    pub failed: usize,
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

/// How many openings in `data` the hub signed started while it charged
/// less, in some fee, than `fees`, what it charges now: their holders
/// agreed to no more, and pay it nothing until they agree to these
/// ([`Openings::charging`]). Those started before what it charged was
/// kept are not counted. Nothing is made in `data`.
pub fn opened_for_less(data: &Path, fees: &licash_api::Fees) -> Result<u64> {
    let charged = Store::charged(data)?;
    Ok(charged.iter().filter(|was| fees.above(was)).count() as u64)
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
        funding: peek.funding.map(|tx| tx.compute_txid()),
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
        match wallet.profile().await {
            Ok(theirs) => ensure!(
                theirs == profile,
                "the contracts' wallet, {wallet}, is on {theirs}, the chain on {profile}"
            ),
            Err(e) => tracing::warn!("the contracts' wallet, {wallet}: {e:#}"),
        }
        let store = Store::open(data)?;
        for (id, format) in store.unreadable_signed()? {
            tracing::warn!(
                id = %crate::hex_id(&id),
                format,
                "a signed contract stored in a format this build doesn't read: its exit isn't played"
            );
        }
        let openings = Arc::new(Self {
            data: data.to_owned(),
            store: Mutex::new(store),
            source,
            wallet,
            keyset,
            trust,
            timing,
            busy: Mutex::new(HashSet::new()),
            heavy: Arc::new(tokio::sync::Semaphore::new(HEAVY)),
            funding: tokio::sync::Mutex::new(()),
            watch: dispute::Watch::default(),
            evidencer: std::sync::OnceLock::new(),
            prover: (!trust.mock)
                .then(|| prover::Prover::start(data, trust.program, prover::spacing(profile))),
            alert: std::sync::OnceLock::new(),
            withheld: Mutex::default(),
            over_capacity: Default::default(),
            folds: Mutex::default(),
            funds: Mutex::new(None),
        });
        if let Err(e) = openings.learn_funds().await {
            tracing::warn!("contracts need the node's wallet, to fund F: {e:#}");
        }
        // A node restarted forgets the coins it locked without persisting.
        let locked = openings.store().locked()?;
        openings.relock_coins(&locked).await;
        Ok(openings)
    }

    /// Keep `fees`, what the hub charges, with each opening that starts
    /// from now on: what its holder agrees to pay at most
    /// ([`opened_for_less`]). Nothing is kept until told.
    pub fn charging(&self, fees: licash_api::Fees) {
        self.store().charging(fees);
    }

    /// The hub's offer now, checkpointed at its tip: its quote of `cap`
    /// sat over `blocks` she chose, if within its terms, else its fixed
    /// one.
    pub async fn offer(&self, chosen: Option<(u32, u32)>) -> Result<Offer, Refused> {
        let (cap, blocks) = chosen.unwrap_or((terms::CAP, terms::LOCK));
        self.timing.allows(cap, blocks).map_err(Refused::bad)?;
        let most = self.most_funded(terms::MAX_CAP);
        if cap > most {
            return Err(Refused::unavailable(format!(
                "the hub's wallet funds caps of {most} sat at most now"
            )));
        }
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
    /// Its range's largest cap, as much as its wallet funds now.
    pub fn advertise(&self) -> licash_api::Contracts {
        let range = self.timing.range().map(|mut r| {
            r.max_cap = self.most_funded(r.max_cap).max(r.min_cap);
            r
        });
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

    /// `most`, or less if the node's wallet, as it last said, funds no F
    /// of a cap so large: none if it funds none. As `most` if not known.
    fn most_funded(&self, most: u32) -> u32 {
        let funds = *self.funds.lock().unwrap_or_else(PoisonError::into_inner);
        match funds {
            Some(balance) => terms::fundable(balance).unwrap_or(0).min(most),
            None => most,
        }
    }

    /// Ask the node's wallet what it holds, confirmed or its own
    /// (`getbalances`, its `mine.trusted`): logged when the largest cap
    /// it funds changes. Its balance, in sat.
    pub async fn learn_funds(&self) -> Result<u64> {
        let balances: serde_json::Value = self
            .wallet
            .rpc("getbalances", serde_json::json!([]))
            .await?;
        let btc = balances["mine"]["trusted"]
            .as_f64()
            .context("a wallet's balances, none trusted")?;
        let balance = bitcoin::Amount::from_btc(btc)?.to_sat();
        let before = self
            .funds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(balance);
        let cap = terms::fundable(balance);
        if before.map(terms::fundable) != Some(cap) {
            match cap {
                Some(cap) => tracing::info!(balance, cap, "the contracts' wallet funds caps up to"),
                None => tracing::warn!(balance, "the contracts' wallet funds no contract"),
            }
        }
        Ok(balance)
    }

    /// Sweep, and learn what the node's wallet holds, every minute, for
    /// as long as the hub runs: never returns.
    pub async fn sweep_every_minute(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if let Err(e) = self.sweep().await {
                tracing::error!("giving up idle openings: {e:#}");
            }
            if let Err(e) = self.learn_funds().await {
                tracing::warn!("what the contracts' wallet holds: {e:#}");
            }
        }
    }

    /// Find the evidence against holders' claims with `f` from now on: the
    /// hub's ledger of the spends it retired notes for and the locks it
    /// settled. Without, a claim is owed what its notes are worth, and
    /// only claims that are none are answered.
    pub fn evidence_with(&self, f: Arc<Evidencer>) {
        let _ = self.evidencer.set(f);
    }

    /// Tell the operator through `f` from now on (the hub's notifications):
    /// its prover is not ready or fails, a claim is answered with no
    /// response for want of it, an assertion's deadline nears unproven.
    pub fn alert_with(&self, f: Arc<Alert>) {
        if let Some(prover) = &self.prover {
            prover.alert_with(f.clone());
        }
        let _ = self.alert.set(f);
    }

    fn alert(&self, message: &str) {
        match self.alert.get() {
            Some(alert) => alert(message),
            None => tracing::error!("alert: {message}"),
        }
    }

    /// `id`'s claim of `b` units, owed `owed`, gets no response: `why`.
    /// Told the operator at the end of the round, with the others.
    fn withheld(&self, id: &[u8; 32], b: u32, owed: u32, why: &str) {
        let message = format!(
            "contract {} (claims {b} units, owed {owed}): {why}",
            crate::hex_id(id)
        );
        tracing::warn!("no response: {message}");
        self.withheld
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message);
    }

    /// Play each signed opening's exit, if any, one step
    /// ([`dispute`]). Each opening's error is reported independently.
    pub async fn answer_requests(&self) -> Result<ResponseRound> {
        let ids = self.store().signed_ids()?;
        self.keep_loaded(&ids);
        let round_open = ids.len();
        let mut round = ResponseRound::default();
        for id in ids {
            match self.play(&id).await {
                Ok(Played::Posted(what)) => {
                    match what {
                        "sweep" => round.swept += 1,
                        "default" => round.closed += 1,
                        _ => round.answered += 1,
                    }
                    tracing::info!(id = %crate::hex_id(&id), what, "contract exit played");
                }
                Ok(Played::Idle) => {}
                Err(e) => {
                    round.failed += 1;
                    tracing::error!(id = %crate::hex_id(&id), "playing a contract exit: {e:#}")
                }
            }
        }
        if let Some(prover) = &self.prover {
            prover.polled();
            let blocks = self.timing.proving_blocks();
            if let Some(defendable) = prover.defendable(blocks) {
                let open = round_open as u64;
                let over = open > defendable;
                if over
                    && !self
                        .over_capacity
                        .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    self.alert(&format!(
                        "{open} contracts are open, and the hub's prover makes about {defendable} \
                         proofs of the smallest in an assert window ({blocks} blocks): exits by \
                         more of their holders at once than it can prove for are paid what they \
                         claim"
                    ));
                }
                if !over {
                    self.over_capacity
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        let withheld =
            std::mem::take(&mut *self.withheld.lock().unwrap_or_else(PoisonError::into_inner));
        if let Some(first) = withheld.first() {
            self.alert(&format!(
                "{} contested claims get no response, for want of the hub's prover (their \
                 timeouts pay what they claim): {first}{}",
                withheld.len(),
                match withheld.len() {
                    1 => String::new(),
                    n => format!(", and {} more", n - 1),
                }
            ));
        }
        Ok(round)
    }

    /// Play the exits every `every` while the hub runs: never returns.
    pub async fn respond_every(self: Arc<Self>, every: Duration) {
        loop {
            if let Err(e) = self.answer_requests().await {
                tracing::error!("automatic contract responder: {e:#}");
            }
            tokio::time::sleep(every).await;
        }
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

/// The routes of `openings`, at [`QUOTE`], [`OPEN`], [`STEP`],
/// [`INSTANCE`], [`FOLD`] and [`FOLD_SIGN`].
pub fn router(openings: Arc<Openings>) -> Router {
    // Separate budgets let existing openings progress while new opens or
    // instance uploads are saturated. Body permits precede extraction and
    // last until the response body is dropped, not merely constructed.
    let budget = |count, time| {
        let limit = Arc::new(tokio::sync::Semaphore::new(count));
        middleware::from_fn_with_state((limit, time), admit)
    };
    Router::new()
        .route(QUOTE, get(quote).layer(budget(2, REQUEST_TIME)))
        .route(OPEN, post(open).layer(budget(2, REQUEST_TIME)))
        .route(STEP, post(step).layer(budget(2, REQUEST_TIME)))
        // A holder uploads three at a time: two holders at once.
        .route(INSTANCE, post(instance).layer(budget(6, INSTANCE_TIME)))
        .route(FOLD, post(offer_fold).layer(budget(2, REQUEST_TIME)))
        .route(FOLD_SIGN, post(sign_fold).layer(budget(2, REQUEST_TIME)))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(openings)
}

async fn admit(
    State((limit, time)): State<(Arc<tokio::sync::Semaphore>, Duration)>,
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
    let response = match tokio::time::timeout(time, next.run(request)).await {
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
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> Result<Answer, Refused> {
    let id = crate::parse_id(&id).map_err(Refused::bad)?;
    if licash_contract::wire::Kind::peek(&body) != Some(licash_contract::wire::Kind::Instance) {
        return Err(Refused::bad(
            "an instance upload requires an Instance envelope",
        ));
    }
    openings.step(id, body.to_vec()).await
}

/// Her nonce for contract `id`'s fold: the hub's, and the fee.
async fn offer_fold(
    State(openings): State<Arc<Openings>>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> Result<Vec<u8>, Refused> {
    let id = crate::parse_id(&id).map_err(Refused::bad)?;
    Ok(openings.offer_fold(&id, &body).await?.encode())
}

/// Her partial signature on contract `id`'s fold: the fold, broadcast.
async fn sign_fold(
    State(openings): State<Arc<Openings>>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> Result<Vec<u8>, Refused> {
    let id = crate::parse_id(&id).map_err(Refused::bad)?;
    let fold = openings.sign_fold(&id, &body).await?;
    Ok(bitcoin::consensus::serialize(&fold))
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
            .layer(middleware::from_fn_with_state(
                (limit.clone(), REQUEST_TIME),
                admit,
            ));
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

    fn funded() -> &'static [u8] {
        &crate::testing::opened().hub
    }

    fn deposit(vout: u32) -> bitcoin::OutPoint {
        use bitcoin::hashes::Hash as _;
        bitcoin::OutPoint {
            txid: bitcoin::Txid::from_byte_array([9; 32]),
            vout,
        }
    }

    fn test_openings(dir: &Path) -> Openings {
        let profile = ChainProfile::Core30DefaultRegtest;
        let keyset = Hub::peek(funded()).unwrap().base.parts().keyset.clone();
        let source: Source = "http://127.0.0.1:1".parse().unwrap();
        Openings {
            data: dir.to_owned(),
            store: Mutex::new(Store::open(dir).unwrap()),
            source: source.clone(),
            wallet: source,
            keyset,
            trust: crate::trust(profile).unwrap(),
            timing: Timing::of(profile, None).unwrap(),
            busy: Mutex::new(HashSet::new()),
            heavy: Arc::new(tokio::sync::Semaphore::new(HEAVY)),
            funding: tokio::sync::Mutex::new(()),
            watch: dispute::Watch::default(),
            evidencer: std::sync::OnceLock::new(),
            prover: None,
            alert: std::sync::OnceLock::new(),
            withheld: Mutex::default(),
            over_capacity: Default::default(),
            folds: Mutex::default(),
            funds: Mutex::new(None),
        }
    }

    #[tokio::test]
    async fn an_instance_waits_for_its_opening_where_a_step_is_answered_202() {
        use licash_contract::wire::{Envelope, Kind};
        let dir = tempfile::tempdir().unwrap();
        let openings = Arc::new(test_openings(dir.path()));
        let id = [9; 32];
        let message = move |kind| {
            Envelope {
                id,
                kind,
                body: vec![],
            }
            .to_bytes()
        };
        openings.busy().insert(id);
        // Another task steps the opening: a step is told to ask again.
        let answer = openings.step(id, message(Kind::Consent)).await.unwrap();
        assert_eq!(answer, Answer::Working);
        // An instance, which she would send whole again, waits its turn.
        let waiting = openings.clone();
        let instance = tokio::spawn(async move { waiting.step(id, message(Kind::Instance)).await });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!instance.is_finished());
        openings.busy().remove(&id);
        // Stepped then, not 202: refused here, of no chain to ask.
        let answer = instance.await.unwrap();
        assert!(answer.is_err(), "{answer:?}");
    }

    #[test]
    fn its_range_is_as_large_as_its_wallet_funds() {
        let dir = tempfile::tempdir().unwrap();
        let mut openings = test_openings(dir.path());
        let range = Range {
            max_cap: 100_000_000,
            max_blocks: 52_560,
            per_coin: 2,
            ppm_year: 10_000,
        };
        openings.timing = Timing::of(ChainProfile::Core30DefaultRegtest, Some(range)).unwrap();
        let max = |o: &Openings| o.advertise().range.unwrap().max_cap;
        // Not known yet: as the operator set it.
        assert_eq!(max(&openings), 100_000_000);
        *openings.funds.lock().unwrap() = Some(50_000_000);
        assert_eq!(max(&openings), 33_554_431);
        // None funded: its least, and opening one is refused.
        *openings.funds.lock().unwrap() = Some(1_000_000);
        assert_eq!(max(&openings), terms::CAP);
        assert_eq!(openings.most_funded(terms::CAP), 0);
    }

    #[test]
    fn sweeping_holds_the_busy_mutex_until_the_abandonment_write_completes() {
        let dir = tempfile::tempdir().unwrap();
        let openings = test_openings(dir.path());
        let id = [5; 32];
        let store = openings.store();
        store
            .insert(&id, 0, true, &deposit(0), &[0; 32], funded())
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
            .insert(&id, 0, true, &deposit(0), &[0; 32], funded())
            .unwrap();
        openings.store().set_coins(&id, &[deposit(1)]).unwrap();
        assert_eq!(openings.store().stale(i64::MAX).unwrap(), vec![id]);
        openings.busy().insert(id);
        assert!(openings.abandon_idle(&id, i64::MAX).unwrap().is_none());
        openings
            .store()
            .save(&id, HubPhase::Funded as u8, true, &[0; 32], funded())
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
            .insert(&id, 0, true, &deposit(0), &[0; 32], funded())
            .unwrap();
        store.set_coins(&id, &[deposit(1)]).unwrap();
        let db = rusqlite::Connection::open(dir.path().join("openings.sqlite")).unwrap();
        db.execute("UPDATE openings SET updated_at = 0", [])
            .unwrap();
        let before = crate::now() - 1;
        assert_eq!(store.stale(before).unwrap(), vec![id]);
        store.save(&id, 0, false, &[1; 32], funded()).unwrap();
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
    fn what_the_hub_charged_is_kept_with_the_openings_started_once_told() {
        let fee = |base, ppm| licash_api::Fee { base, ppm };
        let fees = |base, ppm, output_msat| licash_api::Fees {
            spend: fee(base, ppm),
            lock: fee(base, ppm),
            refund: fee(base, ppm),
            output_msat,
        };
        let dir = tempfile::tempdir().unwrap();
        let now = fees(1, 100, 100);
        assert_eq!(opened_for_less(dir.path(), &now).unwrap(), 0);
        assert!(!dir.path().join("openings.sqlite").exists());
        let openings = test_openings(dir.path());
        let sign = |id: u8| {
            let store = openings.store();
            assert!(store
                .insert(
                    &[id; 32],
                    0,
                    true,
                    &deposit(u32::from(id)),
                    &[0; 32],
                    funded()
                )
                .unwrap());
            store
                .save(&[id; 32], HubPhase::Funded as u8, true, &[0; 32], funded())
                .unwrap();
        };
        // Not told what it charges: nothing kept, and the file as it was.
        sign(1);
        assert_eq!(opened_for_less(dir.path(), &now).unwrap(), 0);
        let tables = || -> i64 {
            let db = rusqlite::Connection::open(dir.path().join("openings.sqlite")).unwrap();
            db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(tables(), 1);

        openings.charging(fees(1, 50, 100));
        sign(2);
        assert_eq!(tables(), 2);
        // One never signed, one whose deposit is another's, one given up.
        assert!(openings
            .store()
            .insert(&[3; 32], 0, true, &deposit(3), &[0; 32], funded())
            .unwrap());
        assert!(!openings
            .store()
            .insert(&[4; 32], 0, true, &deposit(3), &[0; 32], funded())
            .unwrap());
        openings.store().abandon(&[3; 32], i64::MAX).unwrap();
        openings.charging(now);
        sign(5);

        // Only the one signed for less, in one fee, than it charges now.
        assert_eq!(opened_for_less(dir.path(), &now).unwrap(), 1);
        assert_eq!(opened_for_less(dir.path(), &fees(1, 50, 100)).unwrap(), 0);
        assert_eq!(opened_for_less(dir.path(), &fees(1, 100, 101)).unwrap(), 2);
        assert_eq!(opened_for_less(dir.path(), &fees(0, 0, 0)).unwrap(), 0);
        // Read by a hub that reopens them, and still of its version.
        drop(openings);
        let reopened = test_openings(dir.path());
        assert_eq!(reopened.store().signed_ids().unwrap().len(), 3);
        assert_eq!(signed(dir.path()).unwrap(), 3);
    }

    #[test]
    fn openings_read_as_stored_with_their_terms_and_none_are_made() {
        let dir = tempfile::tempdir().unwrap();
        assert!(openings(dir.path()).unwrap().is_empty());
        assert!(!dir.path().join("openings.sqlite").exists());
        let store = Store::open(dir.path()).unwrap();
        assert!(openings(dir.path()).unwrap().is_empty());
        store
            .insert(&[1; 32], 0, true, &deposit(0), &[0; 32], funded())
            .unwrap();
        store
            .save(&[1; 32], HubPhase::Funded as u8, true, &[0; 32], funded())
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
        let peek = Hub::peek(&crate::testing::opened().hub).unwrap();
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
        assert_eq!(terms.funding, peek.funding.map(|tx| tx.compute_txid()));
        assert!(terms.funding.is_some());
        assert_eq!(abandoned.phase, Some(HubPhase::Termed));
        assert!(abandoned.abandoned && !abandoned.signed);
        assert_eq!(abandoned.terms, None);
    }
}
