//! The hub service: a [`Hub`] behind the v1 HTTP API of [`licash_api`].

pub mod notify;

use std::sync::Arc;

use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use licash_api::{
    Code, Fee, Fees, Info, Offer, Offered, Problem, Release, Released, Unlink, Unlinked,
};
use licash_hub::{Error, Hub, Refusal};
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

async fn info(State(hub): State<Arc<Hub>>) -> Json<Info> {
    let info = hub.info();
    let spend = info.policy.spend;
    Json(Info::new(
        &info.keysets,
        info.address,
        Fees {
            spend: Fee {
                base: spend.base,
                ppm: spend.ppm,
            },
            ..Fees::default()
        },
        None,
    ))
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
