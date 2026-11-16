//! Swaps into the provider's own hub: the provider is hub `to`, and rather
//! than lock notes its wallet holds there, it has the hub lock notes it
//! issues itself ([`Hub::lock_own`]), backed by the client's lock at hub
//! `from` as a Lightning receipt's are by its HTLC. The client reveals the
//! preimage to the provider's own hub, which keeps nothing from it: the
//! provider's lock expires [`crate::Config::own_lead`] blocks before the
//! client's, not the window and the refund request a third party needs.
//!
//! What the hub so issues stays backed by the notes of the hubs swapped
//! from, which the provider's wallet claims: [`Issuer::cap`] bounds how
//! many it holds of each, none from a hub without a cap.

use std::sync::Arc;

use licash::{
    address::{Address, Ephemeral},
    crypto::{random_scalar, ScalarBytes},
    spend::{ProposalId, TimedTerms},
};
use licash_api::{Code, Problem};
use licash_hub::{Error as HubError, Hub, Lock, Refusal, Settled};

/// Whether a hash is held by another of the hub's services.
type Taken = dyn Fn(&[u8; 32]) -> Result<bool, String> + Send + Sync;

/// The hub the provider is, issuing the locks of the swaps into it.
pub struct Issuer {
    hub: Arc<Hub>,
    url: String,
    caps: Vec<(String, u64)>,
    taken: Box<Taken>,
    turn: Arc<tokio::sync::Mutex<()>>,
}

impl Issuer {
    /// `hub`, which clients reach at `url`, taking swaps from no hub yet.
    pub fn new(hub: Arc<Hub>, url: impl Into<String>) -> Self {
        Self {
            hub,
            url: url.into(),
            caps: Vec::new(),
            taken: Box::new(|_| Ok(false)),
            turn: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Take swaps from hub `from`, by any URL the provider's wallet knows
    /// it at, while the provider holds, or is owed through such swaps, at
    /// most `most` of its notes, whatever URL they were recorded at. None
    /// are taken from a hub the wallet knows at no such URL.
    pub fn cap(mut self, from: &str, most: u64) -> Self {
        let from = from.trim().trim_end_matches('/').to_owned();
        self.caps.retain(|(url, _)| *url != from);
        self.caps.push((from, most));
        self
    }

    /// Lock no hash `taken` says another of the hub's services holds, such
    /// as its Lightning receipts and deposits: one own lock per hash.
    /// Share `turn` with those services; it covers checking, locking and
    /// recording, including blocking work whose caller was canceled.
    pub fn guard(
        mut self,
        turn: Arc<tokio::sync::Mutex<()>>,
        taken: impl Fn(&[u8; 32]) -> Result<bool, String> + Send + Sync + 'static,
    ) -> Self {
        self.taken = Box::new(taken);
        self.turn = turn;
        self
    }

    pub(crate) async fn turn(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.turn.clone().lock_owned().await
    }

    /// Run `op` off the async runtime: the methods that read or write the
    /// hub's ledger wait for its one connection, which a block scan holds
    /// for as long as the scan takes.
    pub(crate) async fn blocking<R: Send + 'static>(
        self: &Arc<Self>,
        op: impl FnOnce(&Self) -> Result<R, Problem> + Send + 'static,
    ) -> Result<R, Problem> {
        let issuer = Arc::clone(self);
        tokio::task::spawn_blocking(move || op(&issuer))
            .await
            .map_err(unavailable)?
    }

    /// The hub's URL, as its clients reach it.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The hub's address.
    pub(crate) fn address(&self) -> Address {
        self.hub.info().address
    }

    /// The height of the hub's scan of Bitcoin, if known.
    pub(crate) fn height(&self) -> Option<u32> {
        self.hub.scanned().ok().flatten().map(|(height, _)| height)
    }

    /// The most of hub `from`'s notes the provider may hold, if it takes
    /// swaps from there.
    pub(crate) fn cap_of(&self, from: &str) -> Option<u64> {
        self.caps
            .iter()
            .find(|(url, _)| url == from)
            .map(|&(_, most)| most)
    }

    /// The hubs it takes swaps from, by URL.
    pub(crate) fn sources(&self) -> impl Iterator<Item = &str> {
        self.caps.iter().map(|(url, _)| url.as_str())
    }

    /// What the hub says of itself, as at [`licash_api::INFO`].
    pub(crate) fn info(&self) -> Result<licash_api::Info, Problem> {
        self.hub.advertise().map_err(unavailable)
    }

    /// Lock `issue`'s notes, which the hub issues, to its client: the
    /// lock's id. Declined if its hash is the hub's for anything else.
    pub(crate) fn lock(&self, issue: &Issue) -> Result<[u8; 32], Problem> {
        if let Some(lock) = self.hub.own_lock_of(&issue.hash).map_err(unavailable)? {
            return match self.made(issue, &lock) {
                true => Ok(*lock.proposal.id().as_bytes()),
                false => Err(declined("the hash is locked to other notes")),
            };
        }
        if (self.taken)(&issue.hash).map_err(unavailable)? {
            return Err(declined("the hub holds the hash for another payment"));
        }
        let Some(checkpoint) = self.hub.checkpoint() else {
            return Err(unavailable("the hub follows no chain yet"));
        };
        let info = self.hub.info();
        let ephemeral = issue.ephemeral()?;
        let (keyset, pay) = info
            .keysets
            .iter()
            .find_map(|k| {
                let pay = licash_wallet::pay_outputs(k, &ephemeral, &issue.address, issue.amount);
                Some((k, pay.ok()?))
            })
            .ok_or_else(|| declined(format!("no keyset of the hub makes {}", issue.amount)))?;
        let terms = TimedTerms::new(
            checkpoint.profile(),
            checkpoint.commitment(),
            issue.expiry,
            info.policy.timing.min_depth.max(1),
            issue.hash,
        )
        .map_err(|e| Problem::invalid(e.to_string()))?;
        match self
            .hub
            .lock_own(keyset.id(), ephemeral.public(), pay, terms)
        {
            Ok(lock) => Ok(*lock.proposal.id().as_bytes()),
            Err(HubError::Refused(Refusal::Exists)) => {
                Err(declined("the hash is locked to other notes"))
            }
            Err(e @ HubError::Refused(Refusal::Unsynced | Refusal::NoChain)) => Err(unavailable(e)),
            Err(HubError::Refused(refusal)) => Err(declined(refusal.to_string())),
            Err(e) => Err(unavailable(e)),
        }
    }

    /// The lock the hub holds under `issue`'s hash, if `issue` made it:
    /// none if the hub holds none, declined if it holds another.
    pub(crate) fn find(&self, issue: &Issue) -> Result<Option<[u8; 32]>, Problem> {
        match self.hub.own_lock_of(&issue.hash).map_err(unavailable)? {
            None => Ok(None),
            Some(lock) if self.made(issue, &lock) => Ok(Some(*lock.proposal.id().as_bytes())),
            Some(_) => Err(declined("the hash is locked to other notes")),
        }
    }

    /// Whether `issue` made the own `lock`: of its ephemeral key, its
    /// outputs paying the client and its expiry.
    fn made(&self, issue: &Issue, lock: &Lock) -> bool {
        let proposal = &lock.proposal;
        let (Ok(ephemeral), Some(keyset)) =
            (issue.ephemeral(), self.hub.keyset(proposal.keyset_id()))
        else {
            return false;
        };
        let pay = licash_wallet::pay_outputs(keyset, &ephemeral, &issue.address, issue.amount);
        lock.own
            && proposal.ephemeral() == ephemeral.public()
            && pay.is_ok_and(|pay| pay == proposal.outputs())
            && proposal.lock().is_some_and(|l| {
                l.terms().expiry() == issue.expiry && *l.terms().hash() == issue.hash
            })
    }

    /// Where the own lock `id` stands: paid once its client claims it, or
    /// once activated on Bitcoin in time, which publishes the preimage;
    /// refunded once expired at depth otherwise.
    pub(crate) fn follow(&self, id: &[u8; 32]) -> Result<Issued, Problem> {
        let id = ProposalId::from_bytes(*id);
        let lock = self.hub.locked(&id).map_err(unavailable)?;
        match (lock.settled, lock.preimage) {
            (Some(Settled::Refund), _) => return Ok(Issued::Refunded),
            (Some(Settled::Pay), Some(preimage)) => return Ok(Issued::Paid(preimage)),
            (Some(Settled::Pay), None) => return Err(unavailable("paid without its preimage")),
            // Activated, though maybe too late to pay: paid only if the hub
            // pays it, the client's to claim then.
            (None, Some(preimage)) => match self.hub.settle(&id, &preimage) {
                Ok(_) => return Ok(Issued::Paid(preimage)),
                Err(HubError::Refused(Refusal::LockExpired)) => {}
                Err(HubError::Refused(Refusal::Unsynced | Refusal::Settled)) => {
                    return Ok(Issued::Locked)
                }
                Err(e) => return Err(unavailable(e)),
            },
            (None, None) => {}
        }
        let Some(terms) = lock.proposal.lock().map(|l| l.terms().clone()) else {
            return Err(unavailable("not a lock"));
        };
        let tip = self
            .hub
            .scanned()
            .map_err(unavailable)?
            .map_or(0, |(h, _)| h);
        if tip < terms.expiry().saturating_add(terms.depth()) {
            return Ok(Issued::Locked);
        }
        match self.hub.refund(&id) {
            Ok(_) => Ok(Issued::Refunded),
            Err(HubError::Refused(Refusal::Unexpired | Refusal::Activated | Refusal::Settled)) => {
                Ok(Issued::Locked)
            }
            Err(e) => Err(unavailable(e)),
        }
    }
}

/// Where an own lock stands.
pub(crate) enum Issued {
    Locked,
    /// The client claimed it, or activated it on Bitcoin, with the preimage.
    Paid([u8; 32]),
    Refunded,
}

/// The own lock of a swap: of `amount` to the client's `address`, under
/// the client's `hash` until `expiry`, its notes derived from `ephemeral`.
pub(crate) struct Issue {
    pub(crate) address: Address,
    pub(crate) amount: u64,
    pub(crate) hash: [u8; 32],
    pub(crate) expiry: u32,
    pub(crate) ephemeral: [u8; 32],
}

impl Issue {
    /// A new ephemeral secret, to record before the hub locks.
    pub(crate) fn new_ephemeral() -> [u8; 32] {
        ScalarBytes::from_scalar(&random_scalar()).0
    }

    fn ephemeral(&self) -> Result<Ephemeral, Problem> {
        let scalar = ScalarBytes(self.ephemeral).to_scalar()?;
        Ok(Ephemeral::from_scalar(scalar)?)
    }
}

fn declined(message: impl Into<String>) -> Problem {
    Problem::new(Code::Declined, message)
}

fn unavailable(e: impl std::fmt::Display) -> Problem {
    Problem::new(Code::Unavailable, e.to_string())
}
