//! The hub's two gateways of payments in, its Lightning receipts and its
//! deposits, each the other's peer: a hash backs one own lock of the hub's
//! notes ([`licash_hub::Hub::lock_own`]), by one payment. The wallet that
//! asks for a receipt or a deposit picks its hash, so it may ask both
//! under one, and pay both: each gateway refuses a hash its peer holds,
//! and leaves a lock under it to the payment its peer recorded it for.

use std::sync::{Arc, OnceLock};

use licash::spend::ProposalId;
use licash_api::{Code, Problem};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{deposit, lightning, Failure};

/// What a gateway recorded under a hash: a payment, if any, and the own
/// lock it backs once locked.
pub type Recorded = Option<Option<ProposalId>>;

/// What a gateway asks of its peer, if it has one ([`pair`]).
#[derive(Default)]
pub struct Peer(OnceLock<Paired>);

/// What the peer recorded under a hash.
type Lookup = dyn Fn(&[u8; 32]) -> Result<Recorded, String> + Send + Sync;

struct Paired {
    /// Held by either gateway from checking a hash with the peer to
    /// recording what it holds under it: a payment, or the lock it backs.
    turn: Arc<Mutex<()>>,
    recorded: Box<Lookup>,
}

impl Peer {
    /// Wait for the turn to check a hash with the peer and record what
    /// the gateway holds under it, until dropped: none needed unpaired.
    pub(crate) async fn turn(&self) -> Option<OwnedMutexGuard<()>> {
        let paired = self.0.get()?;
        Some(paired.turn.clone().lock_owned().await)
    }

    /// What the peer recorded under `hash`: nothing unpaired.
    pub(crate) fn recorded(&self, hash: &[u8; 32]) -> Result<Recorded, Failure> {
        let Some(paired) = self.0.get() else {
            return Ok(None);
        };
        (paired.recorded)(hash).map_err(|e| {
            tracing::error!(error = %e, "the hub's other gateway cannot be read");
            Failure(Problem::new(Code::Unavailable, "the hub cannot serve now"))
        })
    }
}

/// Make the hub's Lightning gateway and its deposits peers, once both
/// are open.
pub fn pair(receipts: &Arc<lightning::Gateway>, deposits: &Arc<deposit::Gateway>) {
    let turn = Arc::new(Mutex::new(()));
    let other = Arc::downgrade(deposits);
    let recorded = move |hash: &[u8; 32]| match other.upgrade() {
        Some(deposits) => deposits.recorded(hash),
        None => Err("the deposits stopped".to_owned()),
    };
    let paired = Paired {
        turn: turn.clone(),
        recorded: Box::new(recorded),
    };
    assert!(receipts.peer.0.set(paired).is_ok(), "paired once");
    let other = Arc::downgrade(receipts);
    let recorded = move |hash: &[u8; 32]| match other.upgrade() {
        Some(receipts) => receipts.recorded(hash),
        None => Err("the Lightning gateway stopped".to_owned()),
    };
    let paired = Paired {
        turn,
        recorded: Box::new(recorded),
    };
    assert!(deposits.peer.0.set(paired).is_ok(), "paired once");
}
