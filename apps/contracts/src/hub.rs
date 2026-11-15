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

use anyhow::{Context as _, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use licash::note::Keyset;
use licash::spend::ChainProfile;
use licash_chain_client::Source;
use licash_contract::opening::{Context, Trust};

use crate::{Frame, Offer, INSTANCE, MAX_BODY, OPEN, QUOTE, STEP};
use store::Store;

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
    source: Source,
    keyset: Keyset,
    trust: Trust,
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

impl Openings {
    /// The openings in `data`, opened under `keyset` and funded by the
    /// wallet of the node `source` names (its JSON-RPC). Refused off
    /// regtest until the genuine proof programs are named: [`crate::trust`].
    pub async fn new(data: &Path, source: Source, keyset: Keyset) -> Result<Arc<Self>> {
        let profile: ChainProfile = source.profile().await?;
        let trust = crate::trust(profile)?;
        let _: serde_json::Value = source
            .rpc("getwalletinfo", serde_json::json!([]))
            .await
            .context("contracts need the node's wallet, to fund F")?;
        let store = Store::open(data)?;
        let openings = Arc::new(Self {
            store: Mutex::new(store),
            source,
            keyset,
            trust,
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
            quote: terms::quote(self.keyset.clone(), checkpoint),
            bounds: terms::BOUNDS,
        })
    }

    /// What the hub's info says of its contracts.
    pub fn advertise(&self) -> licash_api::Contracts {
        licash_api::Contracts {
            cap: terms::CAP,
            price: terms::RESERVE_ALICE + terms::OPENING_FEE,
            blocks: terms::BOUNDS.lock,
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
