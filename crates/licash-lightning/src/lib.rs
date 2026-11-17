//! A Lightning node, as the hub's gateway and the swap provider use it:
//! the [`Node`] they pay and are paid through, `lnd::Lnd` over its REST
//! API (feature `lnd`) and, for tests, an in-memory `fake::Fake` (feature
//! `fake`). Beside it, what paying an invoice for a timed lock takes
//! whoever pays it: the invoices a payer
//! takes ([`unpayable`], [`expires`]), the timelock its routes may use
//! ([`cltv_limit`]), and what the node's attempt says of a payment
//! recorded before it ([`judge`]).

#[cfg(feature = "fake")]
pub mod fake;
#[cfg(feature = "lnd")]
pub mod lnd;

use std::{future::Future, pin::Pin, time::Duration};

use lightning_invoice::Bolt11Invoice;
pub use lightning_invoice::Currency;

/// What a node answers, or why it did not.
pub type Call<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// What a node says of itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// Its public key, in hex.
    pub pubkey: String,
    pub height: u32,
    /// The hash of its block at `height`, in internal byte order.
    pub block_hash: [u8; 32],
    pub currency: Currency,
}

/// What a node can pay and be paid over its channels now, in satoshis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Liquidity {
    pub local: u64,
    pub remote: u64,
}

/// The latest attempt of a node to pay a hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub status: Status,
    /// When it started, in Unix nanoseconds.
    pub created_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    InFlight,
    Succeeded([u8; 32]),
    Failed(String),
}

/// A held invoice of the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub state: HeldState,
    pub request: String,
    /// The lowest expiry height of the HTLCs it holds, once accepted.
    pub min_expiry: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldState {
    Open,
    /// Paid, its HTLCs held until it is settled or canceled.
    Accepted,
    Settled,
    Canceled,
}

/// A Lightning node to pay and be paid through, such as LND (`lnd::Lnd`).
pub trait Node: Send + Sync {
    fn info(&self) -> Call<'_, NodeInfo>;
    fn liquidity(&self) -> Call<'_, Liquidity>;
    /// Start paying `request`, for at most `fee_limit` satoshis of routing
    /// fees, along routes whose timelock ends within `cltv_limit` blocks,
    /// trying for at most `timeout` seconds. A payment already under way
    /// or made is no error.
    fn pay(&self, request: String, fee_limit: u64, cltv_limit: u32, timeout: u32) -> Call<'_, ()>;
    /// The node's latest attempt to pay `hash`, if it knows one.
    fn payment(&self, hash: [u8; 32]) -> Call<'_, Option<Attempt>>;
    /// A held invoice of `msat` under `hash`, whose last HTLC expires at
    /// least `cltv_expiry` blocks after it arrives, payable for `expiry`
    /// seconds, committing to the description of hash `description` if
    /// given (as LNURL-pay asks): its request.
    fn hold(
        &self,
        hash: [u8; 32],
        msat: u64,
        cltv_expiry: u32,
        expiry: u64,
        description: Option<[u8; 32]>,
    ) -> Call<'_, String>;
    fn invoice(&self, hash: [u8; 32]) -> Call<'_, Option<Held>>;
    fn settle(&self, preimage: [u8; 32]) -> Call<'_, ()>;
    fn cancel(&self, hash: [u8; 32]) -> Call<'_, ()>;
}

/// Blocks a route's timelock may take beyond the invoice's final delta,
/// at least.
pub const CLTV_MARGIN: u32 = 3;

/// Seconds an invoice must stay payable past the routing timeout.
const EXPIRY_MARGIN: u64 = 60;

/// Whether `invoice` needs features a payer for a lock does not pay:
/// unknown required ones, or AMP (bits 30 and 31), which splits a payment
/// under other hashes than the lock's.
pub fn unpayable(invoice: &Bolt11Invoice) -> bool {
    invoice.features().is_some_and(|f| {
        f.requires_unknown_bits() || f.le_flags().get(3).is_some_and(|b| b & 0xc0 != 0)
    })
}

/// Whether `invoice` expires within `timeout` seconds and a margin.
pub fn expires(invoice: &Bolt11Invoice, timeout: u32) -> bool {
    let at = now().saturating_add(u64::from(timeout) + EXPIRY_MARGIN);
    invoice.would_expire(Duration::from_secs(at))
}

/// The timelock of routes paying `invoice` for a lock expiring at
/// `expiry`, at height `tip`: ending `slack` blocks before, if that leaves
/// the invoice its final delta and [`CLTV_MARGIN`].
pub fn cltv_limit(expiry: u32, tip: u32, slack: u32, invoice: &Bolt11Invoice) -> Option<u32> {
    let delta = u32::try_from(invoice.min_final_cltv_expiry_delta()).ok()?;
    let limit = expiry.checked_sub(tip)?.checked_sub(slack)?;
    (limit >= delta.checked_add(CLTV_MARGIN)?).then_some(limit)
}

/// A payment recorded before the node was asked to make it, as the node's
/// latest attempt at its hash shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Judged {
    /// Made: this preimage was bought.
    Paid([u8; 32]),
    /// Under way.
    InFlight,
    /// Failed, by an attempt started once it was recorded.
    Failed(String),
    /// Not started yet, as far as the node shows: it knows no attempt,
    /// or only a failed one started before the payment was recorded,
    /// which is not this payment's (`known`).
    Unstarted { known: bool },
}

/// What `attempt` says of the payment recorded at Unix second `recorded`.
/// The node keeps one attempt per hash: a failed one started before the
/// payment was recorded is not its own.
pub fn judge(attempt: Option<Attempt>, recorded: u64) -> Judged {
    let recorded_ns = recorded.saturating_mul(1_000_000_000);
    match attempt {
        Some(Attempt {
            status: Status::Succeeded(preimage),
            ..
        }) => Judged::Paid(preimage),
        Some(Attempt {
            status: Status::InFlight,
            ..
        }) => Judged::InFlight,
        Some(Attempt {
            status: Status::Failed(reason),
            created_ns,
        }) if created_ns >= recorded_ns => Judged::Failed(reason),
        known => Judged::Unstarted {
            known: known.is_some(),
        },
    }
}

/// The current Unix time, in seconds.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(status: Status, created_ns: u64) -> Option<Attempt> {
        Some(Attempt { status, created_ns })
    }

    #[test]
    fn a_failure_before_the_payment_was_recorded_is_not_its_own() {
        let at = 1_000;
        let ns = at * 1_000_000_000;
        assert_eq!(judge(None, at), Judged::Unstarted { known: false });
        let stale = attempt(Status::Failed("old".into()), ns - 1);
        assert_eq!(judge(stale, at), Judged::Unstarted { known: true });
        let own = attempt(Status::Failed("no route".into()), ns);
        assert_eq!(judge(own, at), Judged::Failed("no route".into()));
        // Paid or under way, whenever it started: one hash, one payment.
        let paid = attempt(Status::Succeeded([7; 32]), 0);
        assert_eq!(judge(paid, at), Judged::Paid([7; 32]));
        assert_eq!(judge(attempt(Status::InFlight, 0), at), Judged::InFlight);
    }
}
