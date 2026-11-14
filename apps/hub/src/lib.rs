//! The hub service: a [`Hub`] behind the v1 HTTP API of [`licash_api`],
//! and its Lightning gateway if it runs one.

pub mod announce;
pub mod lightning;
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
    Check, Checked, Code, Info, Issued, Lock, LockState, Locked, Offer, Offered, Pay, Paying,
    Problem, Receive, ReceiveStatus, Receiving, Refund, Release, Released, Settle, Unlink,
    Unlinked,
};
use licash_hub::{Error, Hub, Settled};
use lightning::Gateway;
use tower_http::trace::TraceLayer;

/// The largest request body: a spend of the most inputs and outputs the
/// protocol allows fits well within it.
pub const BODY_LIMIT: usize = 1 << 20;

/// `ok`, or 503 once the hub halted.
pub const HEALTH: &str = "/health";

/// The API of `hub`, with the Lightning paths of `gateway` if given.
pub fn router(hub: Arc<Hub>, gateway: Option<Arc<Gateway>>) -> Router {
    let info_of = Router::new()
        .route(licash_api::INFO, get(info))
        .with_state((hub.clone(), gateway.clone()));
    let mut api = Router::new()
        .route(licash_api::OFFER, post(offer))
        .route(licash_api::RELEASE, post(release))
        .route(licash_api::UNLINK, post(unlink))
        .route(licash_api::CHECK, post(check))
        .route(licash_api::LOCK, post(lock))
        .route(licash_api::LOCK_STATE, get(lock_state))
        .route(licash_api::SETTLE, post(settle))
        .route(licash_api::REFUND, post(refund))
        .route(HEALTH, get(health))
        .with_state(hub)
        .merge(info_of);
    if let Some(gateway) = gateway {
        api = api.merge(
            Router::new()
                .route(licash_api::LN_PAY, post(ln_pay))
                .route(licash_api::LN_RECEIVE, post(ln_receive))
                .route(licash_api::LN_RECEIVE_STATE, get(ln_receive_state))
                .with_state(gateway),
        );
    }
    api.layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(TraceLayer::new_for_http())
}

/// A [`Problem`] answered with its status.
#[derive(Debug)]
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
            Error::Refused(refusal) => refusal.code(),
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

async fn info(State((hub, gateway)): State<(Arc<Hub>, Option<Arc<Gateway>>)>) -> Answer<Info> {
    let Json(mut info) = blocking(hub, |hub| Ok(hub.advertise()?)).await?;
    info.lightning = gateway.and_then(|gateway| gateway.advertise());
    Ok(Json(info))
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

async fn check(
    State(hub): State<Arc<Hub>>,
    body: Result<Json<Check>, JsonRejection>,
) -> Answer<Checked> {
    let Json(body) = body?;
    blocking(hub, move |hub| {
        let spent = hub.spent(&body.decode()?)?;
        Ok(Checked { spent })
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

async fn ln_pay(
    State(gateway): State<Arc<Gateway>>,
    body: Result<Json<Pay>, JsonRejection>,
) -> Answer<Paying> {
    let Json(body) = body?;
    Ok(Json(gateway.pay(&body).await?))
}

async fn ln_receive(
    State(gateway): State<Arc<Gateway>>,
    body: Result<Json<Receive>, JsonRejection>,
) -> Answer<Receiving> {
    let Json(body) = body?;
    Ok(Json(gateway.receive(&body).await?))
}

async fn ln_receive_state(
    State(gateway): State<Arc<Gateway>>,
    Path(hash): Path<String>,
) -> Answer<ReceiveStatus> {
    let hash = licash_api::hash_of(&hash)?;
    Ok(Json(gateway.receive_status(&hash)?))
}
