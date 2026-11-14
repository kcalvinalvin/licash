//! The swap provider service: a [`Provider`] behind the HTTP API of
//! [`licash_api::swaps`], the steps that take its swaps on, and its
//! announcement on Nostr.

pub mod notify;

use std::{sync::Arc, time::Duration};

use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use licash_api::{
    swaps::{self, Request, Swap},
    Code, Problem,
};
use licash_nostr::{Announcement, Announcer, Role};
use licash_swaps::Provider;
use licash_wallet::Transport;
use tower_http::trace::TraceLayer;

/// The largest request body: a swap request is far smaller.
pub const BODY_LIMIT: usize = 64 * 1024;

/// `ok` while the service serves.
pub const HEALTH: &str = "/health";

struct Served<T: Transport> {
    provider: Arc<Provider<T>>,
    nostr: Option<String>,
}

/// The API of `provider`, whose info names `nostr`, the key it announces
/// itself under, if given.
pub fn router<T: Transport + 'static>(provider: Arc<Provider<T>>, nostr: Option<String>) -> Router {
    Router::new()
        .route(swaps::SWAPS_INFO, get(info::<T>))
        .route(swaps::SWAPS, post(take::<T>))
        .route(swaps::SWAP_STATE, get(state::<T>))
        .route(HEALTH, get(|| async { "ok" }))
        .with_state(Arc::new(Served { provider, nostr }))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(TraceLayer::new_for_http())
}

/// A [`Problem`] answered with its status.
#[derive(Debug)]
struct Failure(Problem);

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

impl From<licash_swaps::Error> for Failure {
    fn from(e: licash_swaps::Error) -> Self {
        tracing::error!(error = %e, "request failed");
        Self(Problem::new(
            Code::Unavailable,
            "the provider cannot serve now",
        ))
    }
}

type Answer<T> = Result<Json<T>, Failure>;

async fn info<T: Transport + 'static>(State(served): State<Arc<Served<T>>>) -> Answer<swaps::Info> {
    let mut info = served.provider.info()?;
    info.nostr = served.nostr.clone();
    Ok(Json(info))
}

async fn take<T: Transport + 'static>(
    State(served): State<Arc<Served<T>>>,
    body: Result<Json<Request>, JsonRejection>,
) -> Answer<Swap> {
    let Json(request) = body?;
    let swap = served.provider.take(&request).await.inspect_err(|p| {
        tracing::debug!(code = ?p.code, message = %p.message, "swap declined");
    })?;
    Ok(Json(swap))
}

async fn state<T: Transport + 'static>(
    State(served): State<Arc<Served<T>>>,
    Path(id): Path<String>,
) -> Answer<Swap> {
    let id = licash_api::lock_id(&id)?;
    match served.provider.swap(&id)? {
        Some(swap) => Ok(Json(swap)),
        None => Err(Problem::new(Code::UnknownLock, "no such swap").into()),
    }
}

/// Take the swaps of `provider` on every `every`, for good.
pub async fn step<T: Transport + 'static>(provider: Arc<Provider<T>>, every: Duration) {
    loop {
        match provider.step().await {
            Ok(pending) => {
                for (id, reason) in pending {
                    tracing::debug!(%id, reason, "swap pending");
                }
            }
            Err(e) => tracing::warn!("taking the swaps on: {e}"),
        }
        tokio::time::sleep(every).await;
    }
}

/// The network the hubs of `provider` follow, once one names it.
pub fn network<T: Transport>(provider: &Provider<T>) -> Option<String> {
    let hubs = provider.wallet().hubs().ok()?;
    let checkpoint = hubs
        .iter()
        .find_map(|(_, info)| info.locks.as_ref()?.checkpoint().ok())?;
    Some(licash_chain_client::network(checkpoint.profile()).to_string())
}

/// Announce `provider` at `urls`, named `name`, on `relays`, for good: once
/// one of its hubs names the network it follows.
pub async fn announce<T: Transport + 'static>(
    provider: Arc<Provider<T>>,
    relays: Vec<String>,
    urls: Vec<String>,
    name: Option<String>,
) {
    let network = loop {
        if let Some(network) = network(&provider) {
            break network;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    let secret = match provider.nostr_secret() {
        Ok(secret) => secret,
        Err(e) => return tracing::error!("announcing on Nostr: {e}"),
    };
    let announcement = Announcement {
        urls,
        network,
        name,
    };
    match Announcer::new(&secret, Role::Swaps, &relays).await {
        Ok(announcer) => announcer.run(announcement).await,
        Err(e) => tracing::error!("announcing on Nostr: {e:#}"),
    }
}
