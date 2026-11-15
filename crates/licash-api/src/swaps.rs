//! The swap provider API, version 1: a provider swaps notes at one hub for
//! notes at another, atomically, with a timed lock at each under one hash.
//!
//! | Path | Body | Answer |
//! |------|------|--------|
//! | `GET /v1/swaps/info` |  | [`Info`] |
//! | `POST /v1/swaps`     | [`Request`] | [`Swap`] |
//! | `GET /v1/swaps/{id}` |  | [`Swap`] |
//! | `POST /v1/swaps/chain` | [`ChainRequest`] | [`ChainSwap`] |
//! | `GET /v1/swaps/chain/{id}` |  | [`ChainSwap`] |
//!
//! The client draws a preimage and locks, at hub `from`, the amount and the
//! provider's fee to [`Info::address`] under its hash, then asks for the
//! swap. Once the provider checked that lock, it locks the amount at hub
//! `to` to the client under the same hash, expiring [`Info::margin`]
//! blocks earlier at least. The client claims that lock with the preimage,
//! which its hub then shows the provider, who claims the client's lock with
//! it. A lock unclaimed is refunded once expired: if the provider never
//! locks, or the client never claims, each gets its own notes back.
//!
//! A provider with [`Info::chain`] pays notes out on Bitcoin the same way:
//! the client locks a coin's value and the provider's fee to it at hub
//! `from`, and the provider locks a coin on chain under the same hash
//! (licash_onchain's `Htlc`: the client's key and the preimage claim it,
//! the provider's key takes it back after its timeout). The client's claim
//! pays its payee, and shows the provider the preimage it claims the
//! client's lock with.
//!
//! Hubs are named by their addresses ([`crate::Info::address`]), as client
//! and provider may reach one hub at different URLs. Anything else is a
//! [`Problem`]; [`crate::Code::Declined`] when the provider takes no such
//! swap.

use licash::{address::Address, spend::ProposalId};
use serde::{Deserialize, Serialize};

use crate::{Fee, Funding, Hex, Problem};

pub const SWAPS: &str = "/v1/swaps";
pub const SWAPS_INFO: &str = "/v1/swaps/info";
/// The path of [`swap_state`], as a route.
pub const SWAP_STATE: &str = "/v1/swaps/{id}";
pub const CHAIN_SWAPS: &str = "/v1/swaps/chain";
/// The path of [`chain_swap_state`], as a route.
pub const CHAIN_SWAP_STATE: &str = "/v1/swaps/chain/{id}";

/// Where the swap of the client's lock `id` stands.
pub fn swap_state(id: &ProposalId) -> String {
    format!("{SWAPS}/{}", hex::encode(id.as_bytes()))
}

/// Where the payout of the client's lock `id` stands.
pub fn chain_swap_state(id: &ProposalId) -> String {
    format!("{CHAIN_SWAPS}/{}", hex::encode(id.as_bytes()))
}

/// What a provider swaps, and on what terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    /// The address the client's lock pays.
    pub address: Address,
    /// Its fee on the amount swapped, which the client's lock pays on top.
    pub fee: Fee,
    /// The least and the most it swaps.
    pub min: u64,
    pub max: u64,
    /// Its lock expires this many blocks before the client's, at least, at
    /// every hub: the most of [`Hub::margin`].
    pub margin: u32,
    /// The hubs it swaps between.
    pub hubs: Vec<Hub>,
    /// The key it announces itself under on Nostr, x-only in hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nostr: Option<String>,
    /// What it pays out on Bitcoin, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<Chain>,
}

impl Info {
    /// The hub at `address`, if the provider swaps there.
    pub fn hub(&self, address: &Address) -> Option<&Hub> {
        self.hubs.iter().find(|h| h.address == *address)
    }

    /// Blocks its lock at `hub` expires before the client's, at least.
    pub fn margin_at(&self, hub: &Hub) -> u32 {
        hub.margin.unwrap_or(self.margin)
    }
}

/// A hub a provider swaps at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hub {
    pub address: Address,
    /// The most it locks to a client there now.
    pub max: u64,
    /// Its lock there expires this many blocks before the client's, at
    /// least; [`Info::margin`] if not given. A provider that is not the
    /// hub learns the preimage from it only by forcing it out on Bitcoin,
    /// so this covers the hub's response window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<u32>,
}

/// What a provider pays out on Bitcoin: coins it locks to clients on
/// chain, for their locks at any of its hubs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chain {
    /// Its fee on a coin's value, which the client's lock pays on top.
    pub fee: Fee,
    /// What the client's lock pays on top for the miners of the coin.
    pub miner: u64,
    /// The fee rate it judges a claim confirms at soon, in sat per 1000
    /// virtual bytes: a hint, which a client budgets its claim at at least,
    /// within the fee it allows.
    pub rate: u64,
    /// The least and the most a coin it locks is worth.
    pub min: u64,
    pub max: u64,
    /// The coin's timeout is this many blocks before the client's lock
    /// expires, at least.
    pub margin: u32,
}

impl Chain {
    /// What a lock pays to have a coin worth `value` locked on chain: the
    /// value, the provider's fee and the miners'.
    pub fn pays(&self, value: u64) -> u64 {
        value
            .saturating_add(self.fee.due(value))
            .saturating_add(self.miner)
    }
}

/// Swap the client's `lock` at hub `from`, which pays the provider, for a
/// lock of `amount` to `address` at hub `to` under the same hash. The same
/// request again answers where the swap stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub from: Address,
    pub lock: Hex,
    pub to: Address,
    pub address: Address,
    pub amount: u64,
}

impl Request {
    pub fn new(
        from: &Address,
        lock: &ProposalId,
        to: &Address,
        address: &Address,
        amount: u64,
    ) -> Self {
        Self {
            from: *from,
            lock: Hex(lock.as_bytes().to_vec()),
            to: *to,
            address: *address,
            amount,
        }
    }

    pub fn lock(&self) -> Result<ProposalId, Problem> {
        Ok(ProposalId::from_bytes(self.lock.array("lock")?))
    }
}

/// Where a swap stands, by the client's lock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swap {
    /// The client's lock at `from`.
    pub id: Hex,
    pub state: State,
    pub amount: u64,
    /// The provider's lock at `to`, once made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<Hex>,
    /// Its expiry.
    pub expiry: u32,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Swap {
    /// The provider's lock, decoded.
    pub fn lock(&self) -> Result<Option<ProposalId>, Problem> {
        self.lock
            .as_ref()
            .map(|l| Ok(ProposalId::from_bytes(l.array("lock")?)))
            .transpose()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Taken: the provider is locking at `to`.
    Accepted,
    /// The provider's lock is certified: the client claims it.
    Locked,
    /// The client claimed it: the provider claims the client's lock.
    Paid,
    /// Both locks paid.
    Done,
    /// The client never claimed: the provider's lock is refunded, and the
    /// client's will be once expired.
    Refunded,
    /// The provider did not lock: the client's lock is refunded once
    /// expired.
    Failed,
}

/// Pay the client's `lock` at hub `from`, which pays the provider, out on
/// Bitcoin: lock a coin worth `value` under the lock's hash, claimed with
/// the client's key `claim`, x-only. The same request again answers where
/// the payout stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainRequest {
    pub from: Address,
    pub lock: Hex,
    pub claim: Hex,
    pub value: u64,
}

impl ChainRequest {
    pub fn new(from: &Address, lock: &ProposalId, claim: [u8; 32], value: u64) -> Self {
        Self {
            from: *from,
            lock: Hex(lock.as_bytes().to_vec()),
            claim: Hex(claim.to_vec()),
            value,
        }
    }

    pub fn lock(&self) -> Result<ProposalId, Problem> {
        Ok(ProposalId::from_bytes(self.lock.array("lock")?))
    }

    pub fn claim(&self) -> Result<[u8; 32], Problem> {
        self.claim.array("claim")
    }
}

/// Where a payout stands, by the client's lock: `Accepted` while its coin
/// is not confirmed, `Locked` once it is, `Paid` once the client claimed
/// it, `Done` once the provider claimed the client's lock, `Refunded` once
/// the provider took the coin back, `Failed` if it never locked one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainSwap {
    /// The client's lock at `from`.
    pub id: Hex,
    pub state: State,
    pub value: u64,
    /// The provider's key, x-only, that takes the coin back after block
    /// `timeout`.
    pub refund: Hex,
    /// A block height, before the client's lock expires: never a time.
    pub timeout: u32,
    /// The coin, once confirmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// The coin's transaction once broadcast, confirmed or not: for the
    /// client to show it before `funding` says where it is. Providers
    /// before it say nothing of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txid: Option<String>,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ChainSwap {
    /// The provider's refund key, decoded.
    pub fn refund(&self) -> Result<[u8; 32], Problem> {
        self.refund.array("refund")
    }
}
