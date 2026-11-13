//! The hub service: a [`Hub`] behind the v1 HTTP API of [`licash_api`].

pub mod notify;
pub mod watch;

use std::sync::Arc;

use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use licash_api::{
    Code, Fee, Fees, Hex, Info, Issued, Lock, LockState, Locked, Locks, Offer, Offered, Problem,
    Refund, Release, Released, Settle, Unlink, Unlinked,
};
use licash_hub::{Error, Hub, Refusal, Settled};
use tower_http::trace::TraceLayer;

/// The largest request body: a spend of the most inputs and outputs the
/// protocol allows fits well within it.
pub const BODY_LIMIT: usize = 1 << 20;

/// `ok`, or 503 once the hub halted.
pub const HEALTH: &str = "/health";

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route(licash_api::INFO, get(info))
        .route(licash_api::OFFER, post(offer))
        .route(licash_api::RELEASE, post(release))
        .route(licash_api::UNLINK, post(unlink))
        .route(licash_api::LOCK, post(lock))
        .route(licash_api::LOCK_STATE, get(lock_state))
        .route(licash_api::SETTLE, post(settle))
        .route(licash_api::REFUND, post(refund))
        .route(HEALTH, get(health))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(TraceLayer::new_for_http())
        .with_state(hub)
}

/// A [`Problem`] answered with its status.
pub struct Failure(pub Problem);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.code.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0)).into_response()
    }
}

impl From<Problem> for Failure {
    fn from(problem: Problem) -> Self {
        Self(problem)
    }
}

impl From<JsonRejection> for Failure {
    fn from(rejection: JsonRejection) -> Self {
        Self(Problem::invalid(rejection.body_text()))
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        let code = match &e {
            Error::Refused(refusal) => match refusal {
                Refusal::UnknownKeyset => Code::UnknownKeyset,
                Refusal::Timed => Code::Timed,
                Refusal::Spent => Code::Spent,
                Refusal::Reissued => Code::Reissued,
                Refusal::Fee { .. } => Code::Fee,
                Refusal::UnknownOffer => Code::UnknownOffer,
                Refusal::NotIssued => Code::NotIssued,
                Refusal::Unlinked => Code::Unlinked,
                Refusal::Expired => Code::Expired,
                Refusal::NotALock => Code::NotALock,
                Refusal::Checkpoint(_) => Code::Checkpoint,
                Refusal::Timing(_) => Code::Timing,
                Refusal::Unsynced | Refusal::NoChain => Code::Unavailable,
                Refusal::UnknownLock => Code::UnknownLock,
                Refusal::Preimage => Code::Preimage,
                Refusal::Settled => Code::Settled,
                Refusal::LockExpired => Code::LockExpired,
                Refusal::Unexpired => Code::Unexpired,
                Refusal::Activated => Code::Activated,
                _ => Code::Invalid,
            },
            Error::Invalid(_) => Code::Invalid,
            _ => {
                tracing::error!(error = %e, "request failed");
                return Self(Problem::new(Code::Unavailable, "the hub cannot serve now"));
            }
        };
        tracing::debug!(error = %e, "request refused");
        Self(Problem::new(code, e.to_string()))
    }
}

type Answer<T> = Result<Json<T>, Failure>;

/// Run `op` off the async workers: it verifies curve proofs and waits on
/// the ledger's fsync.
async fn blocking<T: Send + 'static>(
    hub: Arc<Hub>,
    op: impl FnOnce(&Hub) -> Result<T, Failure> + Send + 'static,
) -> Answer<T> {
    tokio::task::spawn_blocking(move || op(&hub))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "request task failed");
            Failure(Problem::new(Code::Unavailable, "the hub cannot serve now"))
        })?
        .map(Json)
}

async fn health(State(hub): State<Arc<Hub>>) -> Result<&'static str, Failure> {
    if hub.halted() {
        return Err(Failure(Problem::new(Code::Unavailable, "the hub halted")));
    }
    Ok("ok")
}

async fn info(State(hub): State<Arc<Hub>>) -> Answer<Info> {
    blocking(hub, |hub| {
        let info = hub.info();
        let (policy, timing) = (info.policy, info.policy.timing);
        let fee = |fee: licash_hub::Fee| Fee {
            base: fee.base,
            ppm: fee.ppm,
        };
        let fees = Fees {
            spend: fee(policy.spend),
            lock: fee(policy.lock),
            refund: fee(policy.refund),
        };
        let locks = match (hub.checkpoint(), hub.scanned()?) {
            (Some(checkpoint), Some((height, _))) => Some(Locks {
                checkpoint: Hex(checkpoint.to_bytes()),
                height,
                window: timing.window,
                reaction: timing.reaction,
                min_depth: timing.min_depth,
                max_headers: timing.max_headers,
            }),
            _ => None,
        };
        Ok(Info::new(&info.keysets, info.address, fees, locks))
    })
    .await
}

async fn offer(
    State(hub): State<Arc<Hub>>,
    body: Result<Json<Offer>, JsonRejection>,
) -> Answer<Offered> {
    let Json(body) = body?;
    blocking(hub, move |hub| {
        let (proposal, notes) = body.decode(|id| hub.keyset(id))?;
        let grant = hub.offer(&proposal, &notes)?;
        Ok(Offered::new(&grant)?)
    })
    .await
}

async fn release(
    State(hub): State<Arc<Hub>>,
    body: Result<Json<Release>, JsonRejection>,
) -> Answer<Released> {
    let Json(body) = body?;
    blocking(hub, move |hub| {
        let (signed, notes) = body.decode(|id| hub.keyset(id))?;
        Ok(Released::new(&hub.release(&signed, &notes)?))
    })
    .await
}

async fn unlink(
    State(hub): State<Arc<Hub>>,
    body: Result<Json<Unlink>, JsonRejection>,
) -> Answer<Unlinked> {
    let Json(body) = body?;
    blocking(hub, move |hub| {
        Ok(Unlinked::new(&hub.unlink(&body.decode()?)?))
    })
    .await
}

async fn lock(
    State(hub): State<Arc<Hub>>,
    body: Result<Json<Lock>, JsonRejection>,
) -> Answer<Locked> {
    let Json(body) = body?;
    blocking(hub, move |hub| {
        let (signed, notes) = body.decode(|id| hub.keyset(id))?;
        Ok(Locked::new(&hub.lock(&signed, &notes)?))
    })
    .await
}

async fn lock_state(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Answer<LockState> {
    blocking(hub, move |hub| {
        let lock = hub.locked(&licash_api::lock_id(&id)?)?;
        let settled = lock.settled.map(|branch| match branch {
            Settled::Pay => licash_api::Settled::Pay,
            Settled::Refund => licash_api::Settled::Refund,
        });
        Ok(LockState::new(
            &lock.proposal,
            &lock.certificate,
            settled,
            lock.preimage,
        ))
    })
    .await
}

async fn settle(
    State(hub): State<Arc<Hub>>,
    body: Result<Json<Settle>, JsonRejection>,
) -> Answer<Issued> {
    let Json(body) = body?;
    blocking(hub, move |hub| {
        let (id, preimage) = body.decode()?;
        Ok(Issued::new(&hub.settle(&id, &preimage)?))
    })
    .await
}

async fn refund(
    State(hub): State<Arc<Hub>>,
    body: Result<Json<Refund>, JsonRejection>,
) -> Answer<Issued> {
    let Json(body) = body?;
    blocking(hub, move |hub| {
        Ok(Issued::new(&hub.refund(&body.decode()?)?))
    })
    .await
}
