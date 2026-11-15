//! The hub's Lightning gateway, over a node of its own.
//!
//! Out: a wallet locks notes to the hub under an invoice's hash, paying the
//! hub alone the invoice's amount and the gateway's fee. The gateway pays
//! the invoice along routes whose timelock ends [`Config::pay_slack`]
//! blocks before the lock expires, and the preimage it buys settles the
//! lock; a failed payment refunds it ([`Hub::forward`]). Only the node's
//! own word that a payment succeeded or failed resolves it.
//!
//! In: a wallet asks for a held invoice under a hash whose preimage only it
//! knows. Once the invoice is paid, the gateway locks the hub's own notes
//! to the wallet until well before the payment's HTLCs expire. The
//! preimage the wallet reveals to claim them settles the invoice; a lock
//! refunded unclaimed cancels it. Receipts are recorded in the gateway's
//! own SQLite file before their invoice exists.

#[cfg(feature = "fake")]
pub mod fake;
pub mod lnd;
mod pay;
mod receive;

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use licash_api::{Code, Fee, Lightning, Problem};
use licash_hub::{Error, Hub, Timing};
pub use lightning_invoice::Currency;

use crate::Failure;

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

/// A Lightning node the gateway pays and is paid through, such as
/// [`lnd::Lnd`].
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
    /// seconds: its request.
    fn hold(&self, hash: [u8; 32], msat: u64, cltv_expiry: u32, expiry: u64) -> Call<'_, String>;
    fn invoice(&self, hash: [u8; 32]) -> Call<'_, Option<Held>>;
    fn settle(&self, preimage: [u8; 32]) -> Call<'_, ()>;
    fn cancel(&self, hash: [u8; 32]) -> Call<'_, ()>;
}

/// What the gateway charges, the amounts it takes and its timing, in
/// satoshis, blocks and seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// On top of an invoice paid, routing fees included.
    pub pay: Fee,
    /// On top of the notes a receipt locks.
    pub receive: Fee,
    /// The most an invoice paid may ask.
    pub max_pay: u64,
    /// The notes a receipt locks: at least and at most.
    pub min_receive: u64,
    pub max_receive: u64,
    /// Blocks between the end of a payment's route timelock and the
    /// expiry of its lock: the time to settle the lock once paid.
    pub pay_slack: u32,
    /// Blocks before an HTLC's expiry at which the node cancels it when
    /// held: at least LND's `holdexpirydelta`.
    pub hold_delta: u32,
    /// Blocks the gateway may be down while a receipt is locked.
    pub downtime: u32,
    /// Blocks a receipt's lock lasts.
    pub receive_blocks: u32,
    /// Seconds a receipt's invoice may be paid in.
    pub invoice_expiry: u64,
    /// Seconds the node tries to route a payment.
    pub pay_timeout: u32,
    /// Seconds after which a payment the node does not know is given up.
    pub grace: u64,
    /// Between rounds of following payments and receipts.
    pub poll: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pay: Fee { base: 2, ppm: 5000 },
            receive: Fee::default(),
            max_pay: 1_000_000,
            min_receive: 1,
            max_receive: 1_000_000,
            pay_slack: 12,
            hold_delta: 12,
            downtime: 6,
            receive_blocks: 12,
            invoice_expiry: 3600,
            pay_timeout: 60,
            grace: 600,
            poll: Duration::from_secs(2),
        }
    }
}

impl Config {
    /// Whether the hub, of `timing`, admits the locks of this gateway.
    pub fn check(&self, timing: &Timing) -> Result<(), String> {
        let receipt = [self.receive_blocks, timing.min_depth, timing.reaction]
            .iter()
            .try_fold(0u32, |sum, b| sum.checked_add(*b));
        if self.receive_blocks == 0 || receipt.is_none_or(|r| r > timing.window) {
            return Err(format!(
                "receive blocks must be positive and, with the depth and reaction of the hub's \
                 locks, within their window of {}",
                timing.window
            ));
        }
        if self.pay_slack == 0 || self.hold_delta == 0 || self.pay_timeout == 0 {
            return Err("the pay slack, hold delta and pay timeout must be positive".into());
        }
        if self.min_receive == 0 || self.min_receive > self.max_receive {
            return Err("the least receipt must be positive and at most the largest".into());
        }
        Ok(())
    }

    /// What the hub's info says of the gateway, of `node`.
    fn lightning(&self, node: String) -> Lightning {
        Lightning {
            node,
            pay: self.pay,
            receive: self.receive,
            pay_slack: self.pay_slack,
            max_pay: self.max_pay,
            min_receive: self.min_receive,
            max_receive: self.max_receive,
        }
    }

    /// The final CLTV delta of a receipt's invoice: the lock's life, its
    /// depth, the gateway's downtime and the node's hold delta, and 4.
    fn receive_cltv(&self, depth: u32) -> u32 {
        self.receive_blocks
            .saturating_add(depth)
            .saturating_add(self.downtime)
            .saturating_add(self.hold_delta)
            .saturating_add(4)
    }
}

/// The hub's Lightning gateway.
pub struct Gateway {
    hub: Arc<Hub>,
    node: Arc<dyn Node>,
    config: Config,
    receipts: receive::Store,
    /// Held while acting on payments out, one at a time: how often in a
    /// row the node did not know each.
    paying: tokio::sync::Mutex<HashMap<[u8; 32], u32>>,
    /// The node's public key, once it answered.
    identity: Mutex<Option<String>>,
    /// What the operator was alerted of, once each.
    alerted: Mutex<HashSet<String>>,
}

/// The node and the hub on the same chain: the node's view, and the
/// height of the hub's scan.
struct View {
    info: NodeInfo,
    tip: u32,
}

impl Gateway {
    /// The gateway of `hub` over `node`, keeping its receipts at `path`.
    pub fn open(
        hub: Arc<Hub>,
        node: Arc<dyn Node>,
        config: Config,
        path: &Path,
    ) -> anyhow::Result<Self> {
        config
            .check(&hub.info().policy.timing)
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            receipts: receive::Store::open(path)?,
            hub,
            node,
            config,
            paying: tokio::sync::Mutex::default(),
            identity: Mutex::default(),
            alerted: Mutex::default(),
        })
    }

    /// What the hub's info says of the gateway: nothing until its node
    /// answered.
    pub fn advertise(&self) -> Option<Lightning> {
        let node = self
            .identity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        Some(self.config.lightning(node))
    }

    /// Follow payments and receipts until the task is dropped.
    pub async fn run(self: Arc<Self>) {
        loop {
            self.round().await;
            tokio::time::sleep(self.config.poll).await;
        }
    }

    /// One round: resolve the payments out the node finished, lock the
    /// receipts paid and settle or cancel those decided.
    pub async fn round(&self) {
        match self.node.info().await {
            Ok(info) => {
                *self.identity.lock().unwrap_or_else(|e| e.into_inner()) = Some(info.pubkey)
            }
            Err(e) => tracing::warn!(error = %e, "the Lightning node does not answer"),
        }
        self.check_liquidity().await;
        if let Err(e) = self.follow_payments().await {
            tracing::warn!(error = %e.0.message, "cannot follow Lightning payments");
        }
        if let Err(e) = self.follow_receipts().await {
            tracing::warn!(error = %e.0.message, "cannot follow Lightning receipts");
        }
    }

    /// Run `op` on the hub off the async workers.
    async fn on_hub<T: Send + 'static>(
        &self,
        op: impl FnOnce(&Hub) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let hub = self.hub.clone();
        tokio::task::spawn_blocking(move || op(&hub))
            .await
            .map_err(|e| Error::Io(std::io::Error::other(e)))?
    }

    /// The node's view, checked to be on the hub's chain and not ahead of
    /// its scan.
    async fn view(&self) -> Result<View, Failure> {
        let info = self.node.info().await.map_err(node_down)?;
        let height = info.height;
        let (scanned, at) = self
            .on_hub(move |hub| Ok((hub.scanned()?, hub.scanned_at(height)?)))
            .await?;
        let (tip, _) = scanned.ok_or_else(|| unavailable("the hub follows no chain yet"))?;
        if height > tip {
            return Err(unavailable("the Lightning node is ahead of the hub's scan"));
        }
        if at != Some(info.block_hash) {
            return Err(unavailable("the Lightning node follows another chain"));
        }
        Ok(View { info, tip })
    }

    /// Alert the operator of each way the node can move less than the most
    /// the gateway takes: once, until it can again.
    async fn check_liquidity(&self) {
        let liquidity = match self.node.liquidity().await {
            Ok(liquidity) => liquidity,
            Err(e) => return tracing::warn!(error = %e, "cannot read the Lightning channels"),
        };
        let ways = [
            ("pay", liquidity.local, self.config.max_pay, "--ln-max-pay"),
            (
                "receive",
                liquidity.remote,
                self.config.max_receive,
                "--ln-max-receive",
            ),
        ];
        for (way, can, most, flag) in ways {
            let key = format!("liquidity {way}");
            if can >= most {
                self.alerted
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key);
                continue;
            }
            let message = format!(
                "The Lightning node can {way} {} over its channels, less than the {} the \
                 gateway allows ({flag}): larger payments fail. Open or rebalance channels.",
                licash_notify::sat(can),
                licash_notify::sat(most)
            );
            self.alert(key, message).await;
        }
    }

    /// Tell the operator `message` once for `key`.
    async fn alert(&self, key: String, message: String) {
        if !self
            .alerted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key)
        {
            return;
        }
        if let Err(e) = self.on_hub(move |hub| hub.alert(&message)).await {
            tracing::error!(error = %e, "cannot alert the operator");
        }
    }
}

fn unavailable(detail: &str) -> Failure {
    Failure(Problem::new(Code::Unavailable, detail))
}

fn node_down(e: String) -> Failure {
    tracing::warn!(error = %e, "the Lightning node does not answer");
    unavailable("the Lightning node does not answer")
}

/// The gateway refuses a payment for `detail`.
fn refused(detail: &str) -> Failure {
    Failure(Problem::new(Code::Forward, detail))
}

/// The current Unix time, in seconds.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_lock_within_the_hubs_window() {
        let timing = Timing::default();
        let config = Config::default();
        assert_eq!(config.check(&timing), Ok(()));
        let fits = timing.window - timing.min_depth - timing.reaction;
        let at = |receive_blocks| Config {
            receive_blocks,
            ..Config::default()
        };
        assert_eq!(at(fits).check(&timing), Ok(()));
        assert!(at(fits + 1).check(&timing).is_err());
        assert!(at(0).check(&timing).is_err());
        assert!(at(u32::MAX).check(&timing).is_err());
        let empty = Config {
            min_receive: 0,
            ..Config::default()
        };
        assert!(empty.check(&timing).is_err());
        let inverted = Config {
            min_receive: 10,
            max_receive: 9,
            ..Config::default()
        };
        assert!(inverted.check(&timing).is_err());
        let slackless = Config {
            pay_slack: 0,
            ..Config::default()
        };
        assert!(slackless.check(&timing).is_err());
    }

    #[test]
    fn an_invoice_outlasts_its_lock_by_the_depth_downtime_and_hold_delta() {
        let config = Config::default();
        let cltv = config.receive_blocks + 3 + config.downtime + config.hold_delta + 4;
        assert_eq!(config.receive_cltv(3), cltv);
    }
}
