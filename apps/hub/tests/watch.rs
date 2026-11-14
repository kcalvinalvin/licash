//! The hub's scan of Bitcoin through an Esplora server: its first
//! checkpoint, catching up, activations, reorganizations, a backend behind
//! the scan and fresh checkpoints.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use bitcoin::{consensus::serialize, hashes::Hash};
use licash::{
    address::{Ephemeral, OwnerKeys},
    claim::Branch,
    spend::{lock_hash, ChainProfile, Output, ProposalId, TimedTerms},
};
use licash_chain_client::Source;
use licash_hub::{Hub, Policy, Refusal, Timing};
use licash_hub_service::watch::{step, watch};

mod chain;

const SECRET: [u8; 32] = [7; 32];

/// What the Esplora server shows: its chain, up to `shown` if set.
struct Backend {
    chain: chain::Chain,
    shown: Option<u32>,
}

type Shared = Arc<Mutex<Backend>>;

impl Backend {
    fn best(&self) -> &[bitcoin::Block] {
        let height = self.shown.unwrap_or(self.chain.height());
        &self.chain.blocks[..=height as usize]
    }
}

async fn tip(State(backend): State<Shared>) -> String {
    (backend.lock().unwrap().best().len() - 1).to_string()
}

async fn hash(State(backend): State<Shared>, Path(height): Path<usize>) -> Response {
    match backend.lock().unwrap().best().get(height) {
        Some(block) => block.block_hash().to_string().into_response(),
        None => (StatusCode::NOT_FOUND, "Block not found").into_response(),
    }
}

async fn block(
    State(backend): State<Shared>,
    Path((hash, part)): Path<(String, String)>,
) -> Response {
    let backend = backend.lock().unwrap();
    let found = backend
        .chain
        .blocks
        .iter()
        .find(|b| b.block_hash().to_string() == hash);
    match (found, part.as_str()) {
        (Some(block), "header") => hex::encode(serialize(&block.header)).into_response(),
        (Some(block), "raw") => serialize(block).into_response(),
        _ => (StatusCode::NOT_FOUND, "Block not found").into_response(),
    }
}

/// An Esplora server over `backend`.
async fn esplora(backend: Shared) -> Source {
    let app = Router::new()
        .route("/api/blocks/tip/height", get(tip))
        .route("/api/block-height/{height}", get(hash))
        .route("/api/block/{hash}/{part}", get(block))
        .with_state(backend);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("esplora+http://{address}/api").parse().unwrap()
}

struct Setup {
    _dir: tempfile::TempDir,
    hub: Arc<Hub>,
    backend: Shared,
    source: Source,
}

impl Setup {
    /// A hub under `timing` and a backend with `height` blocks.
    async fn new(timing: Timing, height: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
        Hub::init(&seed, &db, 6).unwrap();
        let policy = Policy {
            timing,
            ..Policy::default()
        };
        let hub = Arc::new(Hub::open(&seed, &db, policy).unwrap());
        let mut chain = chain::Chain::new();
        for _ in 0..height {
            chain.mine(None);
        }
        let backend = Arc::new(Mutex::new(Backend { chain, shown: None }));
        let source = esplora(backend.clone()).await;
        Self {
            _dir: dir,
            hub,
            backend,
            source,
        }
    }

    async fn step(&self) -> anyhow::Result<bool> {
        step(&self.hub, &self.source).await
    }

    fn mine(&self, blocks: u32) {
        let chain = &mut self.backend.lock().unwrap().chain;
        for _ in 0..blocks {
            chain.mine(None);
        }
    }

    /// Mine an activation of `SECRET`, at the height it returns.
    fn activate(&self) -> u32 {
        let chain = &mut self.backend.lock().unwrap().chain;
        chain.mine(Some(SECRET));
        chain.height()
    }

    /// Replace the blocks from `height` on with `blocks` others.
    fn reorg(&self, height: u32, blocks: u32) {
        let chain = &mut self.backend.lock().unwrap().chain;
        chain.blocks.truncate(height as usize);
        for _ in 0..blocks {
            chain.mine(None);
        }
    }

    fn show(&self, shown: Option<u32>) {
        self.backend.lock().unwrap().shown = shown;
    }

    /// The backend's best chain's tip.
    fn tip(&self) -> (u32, [u8; 32]) {
        let backend = self.backend.lock().unwrap();
        let best = backend.best();
        let tip = best.last().unwrap().block_hash().to_byte_array();
        (best.len() as u32 - 1, tip)
    }

    fn hash_at(&self, height: u32) -> [u8; 32] {
        let backend = self.backend.lock().unwrap();
        backend.chain.blocks[height as usize]
            .block_hash()
            .to_byte_array()
    }

    fn kinds(&self) -> Vec<String> {
        let events = self.hub.events(0, 1000).unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }

    /// A lock of the hub's own notes to a payee, on `SECRET`'s hash.
    fn lock_own(&self, expiry: u32) -> ProposalId {
        self.try_lock_own(expiry).unwrap().proposal.id()
    }

    fn try_lock_own(&self, expiry: u32) -> Result<licash_hub::Lock, licash_hub::Error> {
        let keyset = self.hub.info().keysets[0].clone();
        let payee = OwnerKeys::from_seed(&[1; 32]).unwrap().address();
        let e = Ephemeral::random();
        let d = e.derive(&payee, 4, Branch::Pay, 0).unwrap();
        let pay = vec![Output::from_derivation(&d).unwrap()];
        let checkpoint = self.hub.checkpoint().unwrap().commitment();
        let terms = TimedTerms::new(
            ChainProfile::Core30DefaultRegtest,
            checkpoint,
            expiry,
            1,
            lock_hash(&SECRET),
        )
        .unwrap();
        self.hub.lock_own(keyset.id(), e.public(), pay, terms)
    }
}

fn timing() -> Timing {
    Timing {
        window: 20,
        reaction: 2,
        min_depth: 1,
        ..Timing::default()
    }
}

fn refused<T>(result: Result<T, licash_hub::Error>) -> Refusal {
    match result {
        Err(licash_hub::Error::Refused(refusal)) => refusal,
        Err(e) => panic!("not a refusal: {e}"),
        Ok(_) => panic!("not refused"),
    }
}

#[tokio::test]
async fn the_first_round_checkpoints_a_buried_block_and_scans_to_the_tip() {
    let s = Setup::new(timing(), 20).await;
    assert!(s.hub.checkpoint().is_none());
    assert!(s.step().await.unwrap());
    // The tip with five blocks on it.
    assert_eq!(s.hub.checkpoint().unwrap().height(), 15);
    assert_eq!(s.hub.scanned().unwrap(), Some(s.tip()));
    s.mine(3);
    assert!(s.step().await.unwrap());
    assert_eq!(s.hub.scanned().unwrap(), Some(s.tip()));
    assert!(s.step().await.unwrap());
    assert_eq!(s.hub.checkpoint().unwrap().height(), 15);
}

#[tokio::test]
async fn an_activation_scanned_in_time_makes_a_lock_pay() {
    let s = Setup::new(timing(), 20).await;
    s.step().await.unwrap();
    let expiry = 25;
    let id = s.lock_own(expiry);
    let height = s.activate();
    assert!(height < expiry);
    assert!(s.step().await.unwrap());
    assert_eq!(s.hub.locked(&id).unwrap().preimage, Some(SECRET));
    assert!(s.kinds().contains(&"activated".to_owned()));
    s.mine(10);
    assert!(s.step().await.unwrap());
    assert_eq!(refused(s.hub.refund(&id)), Refusal::Activated);
}

#[tokio::test]
async fn a_reorganization_above_the_checkpoint_is_rewound_and_rescanned() {
    let s = Setup::new(timing(), 20).await;
    s.step().await.unwrap();
    // A longer chain from block 18.
    s.reorg(18, 4);
    assert!(s.step().await.unwrap());
    assert_eq!(s.hub.scanned().unwrap(), Some(s.tip()));
    assert_eq!(s.hub.scanned_at(18).unwrap(), Some(s.hash_at(18)));
    // One as long as the scan.
    s.reorg(20, 2);
    assert!(s.step().await.unwrap());
    assert_eq!(s.hub.scanned().unwrap(), Some(s.tip()));
    // A shorter one.
    s.reorg(20, 1);
    assert!(s.step().await.unwrap());
    assert_eq!(s.hub.scanned().unwrap(), Some(s.tip()));
    assert_eq!(s.hub.scanned_at(21).unwrap(), None);
    assert!(!s.kinds().contains(&"reorg".to_owned()));
}

#[tokio::test]
async fn a_reorganization_of_the_checkpoint_stops_the_scan_and_alerts_once() {
    let s = Setup::new(timing(), 20).await;
    s.step().await.unwrap();
    let scanned = s.hub.scanned().unwrap();
    s.reorg(15, 7);
    assert!(s.step().await.is_err());
    assert!(s.step().await.is_err());
    assert_eq!(s.hub.scanned().unwrap(), scanned);
    let reorgs = s.kinds().into_iter().filter(|k| k == "reorg").count();
    assert_eq!(reorgs, 1);
    assert_eq!(refused(s.try_lock_own(25)), Refusal::Unsynced);
}

#[tokio::test]
async fn a_backend_behind_the_scan_leaves_the_hub_unsynced() {
    let s = Setup::new(timing(), 20).await;
    assert!(s.step().await.unwrap());
    s.show(Some(18));
    assert!(!s.step().await.unwrap());
    // The backend lags: the scan keeps its blocks.
    assert_eq!(s.hub.scanned().unwrap().unwrap().0, 20);
    assert_eq!(refused(s.try_lock_own(25)), Refusal::Unsynced);
    s.show(None);
    assert!(s.step().await.unwrap());
}

#[tokio::test]
async fn checkpoints_follow_the_tip_within_the_header_budget() {
    // Locks are decided within 30 headers of their checkpoint: a new one
    // every (30 - 10 - 2) / 2 blocks.
    let timing = Timing {
        window: 10,
        bury: 2,
        max_headers: 30,
        ..timing()
    };
    let s = Setup::new(timing, 20).await;
    s.step().await.unwrap();
    let height = || s.hub.checkpoint().unwrap().height();
    assert_eq!(height(), 19);
    s.mine(8);
    s.step().await.unwrap();
    assert_eq!(height(), 19);
    s.mine(1);
    s.step().await.unwrap();
    assert_eq!(height(), 28);
    // A long way behind, a round scans it all and checkpoints once.
    s.mine(100);
    assert!(s.step().await.unwrap());
    assert_eq!(height(), 128);
    assert_eq!(s.hub.scanned().unwrap(), Some(s.tip()));
    s.lock_own(135);
}

#[tokio::test]
async fn the_watch_follows_the_chain_in_rounds() {
    let s = Setup::new(timing(), 20).await;
    let every = Duration::from_millis(20);
    let network = ChainProfile::Core30DefaultRegtest;
    let watching = tokio::spawn(watch(s.hub.clone(), s.source.clone(), network, every));
    let reached = || async {
        for _ in 0..250 {
            if s.hub.scanned().unwrap() == Some(s.tip()) {
                return;
            }
            tokio::time::sleep(every).await;
        }
        panic!("the scan did not reach the tip");
    };
    reached().await;
    s.mine(2);
    reached().await;
    watching.abort();
}

#[tokio::test]
async fn the_watch_scans_no_chain_of_another_network() {
    let s = Setup::new(timing(), 20).await;
    let every = Duration::from_millis(20);
    for network in [ChainProfile::Core30Signet, ChainProfile::Mutinynet] {
        let watching = watch(s.hub.clone(), s.source.clone(), network, every);
        tokio::time::timeout(Duration::from_secs(10), watching)
            .await
            .expect("the watch stops at once");
        assert_eq!(s.hub.scanned().unwrap(), None);
    }
}
