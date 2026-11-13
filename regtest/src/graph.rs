//! BABE's transaction graph on ln-gap's skeleton (Lichen's
//! papers/lichen-babe §6).
//!
//! One chain carries the contract's value from the contract output:
//! `request` (Alice signs `b`), `respond` (the hub signs `b` too, and the
//! script checks the two agree), `assert` (π1, Lamport-signed in 2-bit
//! digits), `challenge_assert` (the hub reveals π1's secrets,
//! [`lichen_babe::reveal`]). Each output of the chain also has its terminal,
//! which a party takes when the other doesn't move: `request_timeout`
//! (after `w_H`, Alice paid her `b`), `respond_timeout` (from `T_A`, the
//! hub), `success` (after `Δ2`, Alice), `failure` (after `Δ1`, the hub).
//! Against the challenge Alice posts `wrongly_challenged_k`, `msg` of a
//! kept instance `k`, which pays her `b` in bits. (A terminal can't pay
//! into a pre-signed transaction: its broadcaster adds a fee input, which
//! changes its id.)
//!
//! BABE's own graph (its Appendix A.1) keeps the deposit apart and joins
//! it to the proving phase's connectors, `failure` spending three outputs,
//! because there `assert` starts from an outside coin. Here the dispute
//! starts from the contract output, so the value flows through one chain
//! and every transaction has one input. A challenge spends `success`'s
//! output, so the two can't race, and `Δ2 > Δ1 + 2u` is not needed.
//!
//! With the exit ([`ExitKeys`]),
//! `request` also carries Alice's signature on her claim's fingerprint
//! `E` and the claim as posted (the withdrawal `Posted` encoding, in its
//! first witness items), and `respond` has two leaves, one transaction:
//! `respond` (plain: the hub signs `E` under its plain key) and
//! `respond_counter` (`E` under its counter key, and the counter `K`
//! itself). Both check the hub's `E` digit for digit against Alice's, as
//! `b`'s. Which key signed `E` tells the statement which leaf ran.
//!
//! A corrected exit ([`Corrected`], licash's Corrected contracts) lets a
//! counter cost only some of a claim: `respond_counter` also carries the
//! hub's signature on what the rest pays, `b_corr`, under its own key, and
//! `respond`'s output a second assertion, `assert_corrected`, for a second
//! statement with its own setup (journal `c = 1`). It carries π1 under
//! Alice's second Lamport key and her signature on `b_corr` under her
//! second `b` key, digit by digit the hub's, so she can only claim what the
//! hub signed. Its chain mirrors `assert`'s (`challenge_corrected`,
//! `success_corrected`, `wrongly_challenged_corrected_k`,
//! `failure_corrected`), over the second dealing, and its payouts read the
//! second `b` key. Without the hub's signature on `b_corr` (a plain
//! `respond`) the leaf can't run.
//!
//! `wrongly_challenged` is 2-of-2, as every leaf here, so that its output
//! is fixed; but Alice keeps her signature on it from the hub
//! ([`Babe::withheld`]), for her towers only. The hub, which knows `msg`,
//! could otherwise post it itself, large and at a low fee rate, and pin
//! Alice's own until `failure` (papers/lichen-babe §7). It is TRUC
//! (version 3), which bounds what a tower holding it could pin with.
//!
//! Owned downstream from Lichen d01bb5b plus Licash's committed adapter
//! (MIT). The constants below preserve its transaction and witness contracts.

use anyhow::{ensure, Context, Result};
use lngap_btc::bitcoin::opcodes::all::*;
use lngap_btc::bitcoin::script::Builder;
use lngap_btc::bitcoin::transaction::Version;
use lngap_btc::bitcoin::{Amount, OutPoint, ScriptBuf, TxOut};
use lngap_btc::script::BuilderExt;
use lngap_btc::secp256k1::{Message, XOnlyPublicKey};
use lngap_btc::sighash::tapscript_sighash;
use lngap_btc::taptree::{Leaf, TapTree};
use lngap_btc::tx::Timelock;
use lngap_channel::{ChannelParams, CommitCtx, PartyPubKeys, Role};
use lngap_lamport::winternitz::{
    Wots256Public, Wots256Sig, WotsExt, WotsParams, WotsPublic, WotsSig,
};
use lngap_sessions::session::Terms;
use lngap_sessions::skeleton::{wots_shape, Core, Presign, FLOOR_RATE, POST_CSV};

use lichen_babe::reveal::{LamportPub, Preimage};
use lichen_babe::setup::{Hash, DIGITS, VALUES};

const POSTED_ITEMS: usize = 15;
const POSTED_ITEM: usize = 80;
const K_LEN: usize = 128;
const MAX_NOTES: usize = 8;

fn posted_items(bytes: &[u8]) -> Option<Vec<&[u8]>> {
    (bytes.len() <= POSTED_ITEMS * POSTED_ITEM).then(|| {
        (0..POSTED_ITEMS)
            .map(|i| {
                bytes
                    .get(i * POSTED_ITEM..)
                    .map_or(&[][..], |b| &b[..b.len().min(POSTED_ITEM)])
            })
            .collect()
    })
}

/// The dispute's windows: `w_H` for the hub's `respond`, the absolute
/// deadline `T_A` for Alice's `assert`, `Δ2` for the hub's challenge,
/// `Δ1` for Alice's answer to it. Relative windows in blocks; `T_A` as
/// `T_close` is (a height, or a time from 500,000,000).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Windows {
    pub w_h: u16,
    pub t_a: u32,
    pub delta2: u16,
    pub delta1: u16,
}

/// Five mandatory, setup-bound publication coins. The same coins are consumed
/// by plain and counter responses, preserving one response txid and graph.
#[derive(Clone, Debug)]
pub struct Publication {
    pub coins: [(OutPoint, TxOut); 5],
    pub keys: [Wots256Public; 5],
}

const PUBLICATION_BYTES: [u32; 5] = [224, 224, 224, 224, 128];

/// A BABE contract: what both parties build its graph from.
#[derive(Clone, Debug)]
pub struct Spec {
    pub terms: Terms,
    pub params: ChannelParams,
    /// Alice's keys, then the hub's.
    pub pubs: [PartyPubKeys; 2],
    /// Alice's one-time key for `b` (`request`'s signature; the payout's
    /// bit outputs read it).
    pub b_key: WotsPublic,
    /// The hub's one-time key for `b`, SHA-256 chains: the key `pk_H`
    /// whose digest the statement's journal commits.
    pub hub_b: Wots256Public,
    /// Alice's Lamport key for π1, in 2-bit digits.
    pub lamport: LamportPub,
    /// Alice's one-time adaptor keys, a digit and value.
    pub adaptor: Vec<[XOnlyPublicKey; VALUES]>,
    /// The kept instances' hashlocks, `SHA-256(msg)`.
    pub hmsg: Vec<Hash>,
    pub windows: Windows,
    /// The exit's keys; none for a graph of returns only.
    pub exit: Option<ExitKeys>,
    /// The corrected exit's keys and setup; none but for an exit whose
    /// counters may mask part of a claim.
    pub corrected: Option<Corrected>,
}

/// A corrected exit's keys, a second set of the assertion's: the hub's
/// one-time key for `b_corr` (SHA-256 chains, signed in
/// `respond_counter`), Alice's for it (her second `b`: the corrected
/// payout's bit outputs read it), her Lamport and adaptor keys for the
/// second statement's π1, and its kept instances' hashlocks. Every key is
/// distinct from the first set's: each signs once.
#[derive(Clone, Debug)]
pub struct Corrected {
    pub hub_b: Wots256Public,
    pub b_key: WotsPublic,
    pub lamport: LamportPub,
    pub adaptor: Vec<[XOnlyPublicKey; VALUES]>,
    pub hmsg: Vec<Hash>,
}

/// The exit's keys: Alice's one-time key for `E` (`request`'s second
/// signature, 32 bytes), and the hub's for `respond`, SHA-256 chains (the
/// statement checks them): `E` in the
/// plain leaf, `E` in the counter leaf, and `K` (`K_LEN` bytes).
#[derive(Clone, Debug)]
pub struct ExitKeys {
    pub e_key: WotsPublic,
    pub hub_e_plain: Wots256Public,
    pub hub_e_counter: Wots256Public,
    /// The key for `K`: Lichen's exit signs the counter's seal itself
    /// (`K_LEN` bytes); a statement whose counter is data posts it below
    /// `K` and signs its digest.
    pub hub_k: Wots256Public,
    /// Items of counter data `respond_counter` carries below `K`, each of up
    /// to [`POSTED_ITEM`] bytes, dropped unchecked (the statement reads them
    /// through `K`); none for Lichen's exit.
    pub counter_items: usize,
}

/// What `respond` carries with the exit ([`Babe::read_respond_exit`]).
pub struct RespondExit {
    pub e: Vec<u8>,
    pub hub_e: Vec<[u8; 32]>,
    pub k: Option<Wots256Sig>,
}

/// `assert`'s arguments: the preimages, π2, π3.
pub type Asserted<'a> = (&'a [Vec<u8>], &'a [u8], &'a [u8]);

/// A terminal's outputs, from the value it spends.
type Terminal<'a> = &'a dyn Fn(&Babe, Amount) -> Result<Vec<TxOut>>;

/// A contract's graph.
pub struct Babe {
    pub spec: Spec,
    request_items: usize,
    /// The native request leaf (`request`), which replaces the
    /// claim-carrying one: it anchors Base; P is a separate tx.
    native_request: Option<ScriptBuf>,
}

fn ok_leaf(name: &str, b: Builder, timelock: Timelock) -> Leaf {
    Leaf::new(name, b.push_int(1).into_script(), timelock)
}

/// `Builder` with a relative lock, and the lock.
fn after(blocks: u16) -> (Builder, Timelock) {
    (Builder::new().csv(blocks), Timelock::csv(blocks))
}

/// Drop a word's 8 digits, which `wots_verify` leaves.
fn drop_digits(b: Builder) -> Builder {
    drop_n(b, 8)
}

/// Drop `n` stack items (`n` even, or one `OP_DROP` more).
fn drop_n(b: Builder, n: usize) -> Builder {
    let b = (0..n / 2).fold(b, |b, _| b.push_opcode(OP_2DROP));
    if n % 2 == 1 {
        b.push_opcode(OP_DROP)
    } else {
        b
    }
}

/// `E`'s 64 digits (32 bytes).
const E_DIGITS: usize = 64;

impl Babe {
    pub fn new(spec: Spec) -> Result<Babe> {
        let word = WotsParams::for_bytes(4);
        ensure!(
            spec.b_key.params == word && spec.hub_b.params == word,
            "the keys for b sign a 4-byte word"
        );
        ensure!(
            spec.lamport.0.len() == DIGITS && spec.adaptor.len() == DIGITS,
            "a Lamport and an adaptor key set per digit of π1"
        );
        ensure!(!spec.hmsg.is_empty(), "a kept instance at least");
        let w = spec.windows;
        ensure!(
            w.w_h > POST_CSV && w.delta1 > POST_CSV && w.delta2 > POST_CSV,
            "each window longer than a post's lock"
        );
        ensure!(w.t_a < spec.terms.t_close, "T_A before T_close");
        if let Some(x) = &spec.exit {
            let e = WotsParams::for_bytes(32);
            ensure!(
                x.e_key.params == e && x.hub_e_plain.params == e && x.hub_e_counter.params == e,
                "the keys for E sign 32 bytes"
            );
            ensure!(
                x.hub_k.params == WotsParams::for_bytes(x.hub_k.params.message_digits / 2),
                "the key for K signs whole bytes"
            );
        }
        if let Some(c) = &spec.corrected {
            ensure!(
                spec.exit.is_some(),
                "a corrected exit corrects the exit's counter"
            );
            ensure!(
                c.b_key.params == word && c.hub_b.params == word,
                "the keys for b_corr sign a 4-byte word"
            );
            ensure!(
                c.lamport.0.len() == DIGITS && c.adaptor.len() == DIGITS,
                "a Lamport and an adaptor key set per digit of π1"
            );
            ensure!(!c.hmsg.is_empty(), "a kept corrected instance at least");
            ensure!(
                c.b_key != spec.b_key
                    && c.hub_b != spec.hub_b
                    && c.lamport != spec.lamport
                    && c.adaptor != spec.adaptor,
                "the corrected keys are not the assertion's: each signs once"
            );
        }
        Ok(Babe {
            spec,
            request_items: POSTED_ITEMS,
            native_request: None,
        })
    }

    /// Use the native request leaf. Its b and E keys must be the graph's
    /// `b_key` and exit `e_key` (the same HASH160 chains, digits and
    /// checksum), so `respond` and the payout bits read native signatures.
    pub fn with_native_request(mut self, script: Vec<u8>) -> Result<Self> {
        ensure!(self.spec.exit.is_some(), "a native request needs the exit");
        self.native_request = Some(ScriptBuf::from_bytes(script));
        Ok(self)
    }

    /// Explicit publication-byte budget, separate from application item count,
    /// set before funding and signing the graph.
    pub fn with_request_items(mut self, items: usize) -> Result<Self> {
        ensure!(
            items > 0 && items <= 800,
            "request publication stack budget"
        );
        self.request_items = items;
        Ok(self)
    }

    pub fn request_exit_wire_for(
        &self,
        b: &WotsSig,
        e: &WotsSig,
        posted: &[u8],
    ) -> Result<Vec<Vec<u8>>> {
        ensure!(
            posted.len() <= self.request_items * POSTED_ITEM,
            "claim exceeds agreed request publication budget"
        );
        Ok((0..self.request_items)
            .map(|i| {
                posted
                    .get(i * POSTED_ITEM..)
                    .map_or(vec![], |p| p[..p.len().min(POSTED_ITEM)].to_vec())
            })
            .chain(e.consumption_order().into_iter().rev())
            .chain(Self::request_wire(b))
            .collect())
    }
    pub fn core(&self) -> Core<'_> {
        Core {
            terms: &self.spec.terms,
            params: &self.spec.params,
            pubs: &self.spec.pubs,
        }
    }
    fn ctx(&self) -> CommitCtx<'_> {
        self.core().ctx()
    }
    pub fn value(&self) -> Amount {
        self.core().value()
    }
    /// The 2-of-2 every leaf starts with, after `lock` if any.
    fn signed(&self, (b, tl): (Builder, Timelock)) -> (Builder, Timelock) {
        (self.ctx().two_of_two_verify(b), tl)
    }

    /// The contract output's tree: `fold`, `default`, and `request`, Alice's
    /// signature on `b` (after the channel's `to_self_delay`, as BitVMX's
    /// `escalate`); with the exit, her signature on `E` too, and the claim
    /// posted below them, unchecked (the statement checks it).
    pub fn tree(&self) -> Result<TapTree> {
        let d = self.spec.params.to_self_delay;
        if let Some(script) = &self.native_request {
            return self
                .core()
                .tree(Leaf::new("request", script.clone(), Timelock::csv(d)));
        }
        let (b, tl) = self.signed(if d > 0 {
            after(d)
        } else {
            (Builder::new(), Timelock::NONE)
        });
        let mut b = drop_digits(b.wots_verify(&self.spec.b_key));
        if let Some(x) = &self.spec.exit {
            b = drop_n(
                drop_n(b.wots_verify(&x.e_key), E_DIGITS),
                self.request_items,
            );
        }
        self.core().tree(ok_leaf("request", b, tl))
    }
    /// The corrected exit's keys.
    fn corrected(&self) -> Result<&Corrected> {
        self.spec.corrected.as_ref().context("not a corrected exit")
    }
    /// The hub's signature on a word under `hub`, then Alice's under
    /// `alice`, digit by digit equal: both consumed.
    fn same_word(mut b: Builder, hub: &Wots256Public, alice: &WotsPublic) -> Builder {
        b = b.wots_verify(hub);
        b = (0..8)
            .fold(b, |b, _| b.push_opcode(OP_TOALTSTACK))
            .wots_verify(alice);
        b = (0..8).fold(b, |b, _| b.push_opcode(OP_FROMALTSTACK));
        // Alice's digits a_0..a_7 below the hub's h_0..h_7: a_k is k + 1
        // deep under h_k
        (1..=8).rev().fold(b, |b, k| {
            b.push_int(k)
                .push_opcode(OP_ROLL)
                .push_opcode(OP_NUMEQUALVERIFY)
        })
    }
    /// A `respond` leaf's check of `E`: Alice's signature (from `request`)
    /// leaves its digits, and the hub's under `key` is verified tied to
    /// them, so on the same `E`.
    fn same_e(b: Builder, alice: &WotsPublic, key: &Wots256Public) -> Builder {
        let pos: Vec<usize> = (0..E_DIGITS).collect();
        drop_n(
            b.wots_verify(alice).wots_verify_tied(key, E_DIGITS, &pos),
            E_DIGITS,
        )
    }

    /// Sidecars have no key path. Their positive CLTV reclaim is isolated
    /// from every authorized response by its zero transaction locktime.
    pub fn publication_trees(&self, keys: &[Wots256Public; 5]) -> Result<[TapTree; 5]> {
        let x = self
            .spec
            .exit
            .as_ref()
            .context("publication requires an exit")?;
        ensure!(
            self.spec.terms.t_close > 0,
            "publication needs a positive reclaim deadline"
        );
        let mut trees = vec![];
        for (key, bytes) in keys.iter().zip(PUBLICATION_BYTES) {
            ensure!(
                key.params == WotsParams::for_bytes(bytes),
                "publication key shape"
            );
            ensure!(
                key.digits.len() == key.params.total_digits() as usize,
                "publication key digit count"
            );
            let (b, tl) = self.signed((Builder::new(), Timelock::NONE));
            let publish = ok_leaf(
                "publish",
                drop_n(b.wots_verify(key), bytes as usize * 2),
                tl,
            );
            let (b, tl) = self.signed((Builder::new(), Timelock::NONE));
            let plain = ok_leaf("plain", Self::same_e(b, &x.e_key, &x.hub_e_plain), tl);
            let reclaim = Builder::new()
                .push_int(i64::from(self.spec.terms.t_close))
                .push_opcode(OP_CLTV)
                .push_opcode(OP_DROP)
                .push_x_only_key(&self.spec.pubs[1].payment)
                .push_opcode(OP_CHECKSIG)
                .into_script();
            trees.push(TapTree::new(vec![
                publish,
                plain,
                Leaf::new("reclaim", reclaim, Timelock::cltv(self.spec.terms.t_close)),
            ])?);
        }
        trees
            .try_into()
            .map_err(|_| anyhow::anyhow!("publication tree count"))
    }
    /// `request`'s output: the hub's `respond`, the two signatures on `b`
    /// digit by digit equal; or, after `w_H`, `request_timeout`. With the
    /// exit, `respond` checks `E` the same way under the hub's plain key,
    /// and `respond_counter` under its counter key, with its signature on
    /// `K`; with the corrected exit, `respond_counter` first checks the
    /// hub's signature on `b_corr` (any value: `assert_corrected` ties it
    /// to Alice's, the statement to the claim).
    pub fn requested(&self) -> Result<TapTree> {
        let respond = || {
            let (b, tl) = self.signed(after(POST_CSV));
            (Self::same_word(b, &self.spec.hub_b, &self.spec.b_key), tl)
        };
        let (t, ttl) = self.signed(after(self.spec.windows.w_h));
        let (b, tl) = respond();
        let mut leaves = vec![];
        match &self.spec.exit {
            None => leaves.push(ok_leaf("respond", b, tl)),
            Some(x) => {
                leaves.push(ok_leaf(
                    "respond",
                    Self::same_e(b, &x.e_key, &x.hub_e_plain),
                    tl,
                ));
                let (mut b, tl) = respond();
                if let Some(c) = &self.spec.corrected {
                    b = drop_digits(b.wots_verify(&c.hub_b));
                }
                let b = Self::same_e(b, &x.e_key, &x.hub_e_counter).wots_verify(&x.hub_k);
                let b = drop_n(b, x.hub_k.params.message_digits as usize);
                leaves.push(ok_leaf("respond_counter", drop_n(b, x.counter_items), tl));
            }
        }
        leaves.push(ok_leaf("request_timeout", t, ttl));
        TapTree::new(leaves)
    }
    /// `respond`'s output: Alice's `assert`, π1's digits under her Lamport
    /// key, then π2 and π3, unsigned (so that the hub can check the whole
    /// proof from the chain); with the corrected exit, `assert_corrected`,
    /// the same under her second Lamport key, then the hub's signature on
    /// `b_corr` (from `respond_counter`) and hers, digit by digit equal;
    /// or, from `T_A`, `respond_timeout`.
    pub fn responded(&self) -> Result<TapTree> {
        let (b, tl) = self.signed(after(POST_CSV));
        let t_a = self.spec.windows.t_a;
        let timeout = self.ctx().two_of_two_verify(Builder::new().cltv(t_a));
        let assert = self.spec.lamport.verify(b).push_opcode(OP_2DROP);
        let mut leaves = vec![ok_leaf("assert", assert, tl)];
        if let Some(c) = &self.spec.corrected {
            let (b, tl) = self.signed(after(POST_CSV));
            let b = Self::same_word(
                c.lamport.verify(b).push_opcode(OP_2DROP),
                &c.hub_b,
                &c.b_key,
            );
            leaves.push(ok_leaf("assert_corrected", b, tl));
        }
        leaves.push(ok_leaf("respond_timeout", timeout, Timelock::cltv(t_a)));
        TapTree::new(leaves)
    }
    /// `assert`'s output: the hub's `challenge_assert`, the reveal; or,
    /// after `Δ2`, `success`.
    pub fn asserted(&self) -> Result<TapTree> {
        self.asserted_with(
            &self.spec.lamport,
            &self.spec.adaptor,
            "challenge_assert",
            "success",
        )
    }
    /// `assert_corrected`'s output: the hub's `challenge_corrected`, the
    /// reveal of the second dealing's secrets; or, after `Δ2`,
    /// `success_corrected`.
    pub fn asserted_corrected(&self) -> Result<TapTree> {
        let c = self.corrected()?;
        self.asserted_with(
            &c.lamport,
            &c.adaptor,
            "challenge_corrected",
            "success_corrected",
        )
    }
    fn asserted_with(
        &self,
        lamport: &LamportPub,
        adaptor: &[[XOnlyPublicKey; VALUES]],
        challenge: &str,
        success: &str,
    ) -> Result<TapTree> {
        let (b, tl) = self.signed(after(POST_CSV));
        let b = lichen_babe::reveal::reveal_verify(b, lamport, adaptor);
        let (s, stl) = self.signed(after(self.spec.windows.delta2));
        TapTree::new(vec![ok_leaf(challenge, b, tl), ok_leaf(success, s, stl)])
    }
    /// `challenge_assert`'s output: Alice's `wrongly_challenged_k`, a kept
    /// instance's `msg`; or, after `Δ1`, `failure`.
    pub fn challenged(&self) -> Result<TapTree> {
        self.challenged_with(&self.spec.hmsg, "wrongly_challenged", "failure")
    }
    /// `challenge_corrected`'s output: Alice's
    /// `wrongly_challenged_corrected_k`, a kept corrected instance's `msg`;
    /// or, after `Δ1`, `failure_corrected`.
    pub fn challenged_corrected(&self) -> Result<TapTree> {
        self.challenged_with(
            &self.corrected()?.hmsg,
            "wrongly_challenged_corrected",
            "failure_corrected",
        )
    }
    fn challenged_with(&self, hmsg: &[Hash], wrongly: &str, failure: &str) -> Result<TapTree> {
        let mut leaves: Vec<Leaf> = hmsg
            .iter()
            .enumerate()
            .map(|(k, h)| {
                let (b, tl) = self.signed(after(POST_CSV));
                ok_leaf(
                    &format!("{wrongly}_{k}"),
                    b.push_opcode(OP_SHA256)
                        .push_bytes(h)
                        .push_opcode(OP_EQUALVERIFY),
                    tl,
                )
            })
            .collect();
        let (f, ftl) = self.signed(after(self.spec.windows.delta1));
        leaves.push(ok_leaf(failure, f, ftl));
        TapTree::new(leaves)
    }
    /// The outputs paying `b` in bits and the reserves to Alice
    /// ([`Core::pay_alice`]).
    pub fn pay_alice(&self, available: Amount) -> Result<Vec<TxOut>> {
        self.core().pay_alice(&self.spec.b_key, available)
    }
    /// The corrected payout: `b_corr` in bits, read under Alice's second
    /// `b` key, and the reserves.
    pub fn pay_alice_corrected(&self, available: Amount) -> Result<Vec<TxOut>> {
        self.core().pay_alice(&self.corrected()?.b_key, available)
    }
    /// Who posts link `name`.
    pub fn poster(name: &str) -> Role {
        match name {
            "request" | "assert" | "assert_corrected" => Role::User,
            _ => Role::Hub,
        }
    }
    /// A witness of link `name`'s shape, for its floor: with the exit, its
    /// largest (`respond_counter`'s for `respond`, the largest claim for
    /// `request`).
    fn shape(&self, name: &str) -> Vec<Vec<u8>> {
        let wots256 = |k: &Wots256Public| {
            (0..k.params.total_digits())
                .flat_map(|_| [vec![0u8; 32], vec![15u8]])
                .collect::<Vec<_>>()
        };
        match (name, &self.spec.exit) {
            ("request", None) => wots_shape([&self.spec.b_key]),
            ("request", Some(x)) if self.native_request.is_some() => {
                wots_shape([&x.e_key, &self.spec.b_key])
            }
            ("request", Some(x)) => {
                let posted = if self.request_items == POSTED_ITEMS {
                    8 + 32 + 1 + MAX_NOTES * (1 + 76 + 64)
                } else {
                    self.request_items * POSTED_ITEM
                };
                (0..self.request_items)
                    .map(|i| vec![0u8; posted.saturating_sub(i * POSTED_ITEM).min(POSTED_ITEM)])
                    .chain(wots_shape([&x.e_key, &self.spec.b_key]))
                    .collect()
            }
            ("respond", exit) => {
                let mut w = wots256(&self.spec.hub_b)
                    .into_iter()
                    .chain(wots_shape([&self.spec.b_key]))
                    .collect::<Vec<_>>();
                if let Some(c) = &self.spec.corrected {
                    w.extend(wots256(&c.hub_b));
                }
                if let Some(x) = exit {
                    let e = &x.hub_e_counter.params;
                    w.extend(wots_shape([&x.e_key]));
                    w.extend(
                        (0..e.total_digits())
                            .map(|_| vec![0u8; 32])
                            .chain((0..e.checksum_digits).map(|_| vec![15u8])),
                    );
                    w.extend(wots256(&x.hub_k));
                    w.extend((0..x.counter_items).map(|_| vec![0u8; POSTED_ITEM]));
                }
                w
            }
            ("assert", _) => vec![vec![0u8; 20]; DIGITS]
                .into_iter()
                .chain([vec![0u8; 64], vec![0u8; 32]])
                .collect(),
            ("assert_corrected", _) => {
                let c = self.spec.corrected.as_ref().expect("a corrected exit");
                let mut w = self.shape("assert");
                w.extend(wots256(&c.hub_b).into_iter().chain(wots_shape([&c.b_key])));
                w
            }
            _ => (0..DIGITS)
                .flat_map(|_| [vec![0u8; 20], vec![0u8; 64]])
                .collect(),
        }
    }
    fn link(
        &self,
        name: &str,
        prev: &TapTree,
        op: OutPoint,
        prev_out: &TxOut,
        next: &TapTree,
        publication: Option<&Publication>,
    ) -> Result<Presign> {
        // with the exit, `respond`'s floor is for its counter leaf, the
        // larger, so that both leaves spend into one transaction
        let leaf = if name == "respond" && self.spec.exit.is_some() {
            "respond_counter"
        } else {
            name
        };
        let mut p = self.core().link(
            name,
            Self::poster(name),
            self.shape(name),
            prev,
            leaf,
            op,
            prev_out,
            next.script_pubkey(),
        )?;
        if name == "respond" {
            if let Some(publication) = publication {
                let trees = self.publication_trees(&publication.keys)?;
                let mut inputs = vec![(op, prev_out.clone(), prev.clone(), leaf)];
                let mut total = Amount::ZERO;
                let mut seen = std::collections::BTreeSet::from([op]);
                for ((point, coin), tree) in publication.coins.iter().zip(&trees) {
                    ensure!(seen.insert(*point), "duplicate publication input");
                    ensure!(
                        coin.script_pubkey == tree.script_pubkey(),
                        "publication coin has another script"
                    );
                    inputs.push((*point, coin.clone(), tree.clone(), "publish"));
                    total += coin.value;
                }
                let old_fee = prev_out.value - p.tx.output.iter().map(|o| o.value).sum::<Amount>();
                let mut counter =
                    Presign::new_multi("respond_counter", &inputs, p.tx.output.clone())?;
                let mut shapes = vec![self.shape(name)];
                for key in &publication.keys {
                    shapes.push(
                        (0..key.params.total_digits())
                            .flat_map(|_| [vec![0; 32], vec![15]])
                            .collect(),
                    );
                }
                let sigs = vec![vec![0; 64]; 6];
                let sized = counter.finish_multi(shapes, &sigs, &sigs);
                ensure!(
                    sized.weight().to_wu() <= 400_000,
                    "publication response exceeds standard weight"
                );
                let target = Amount::from_sat((sized.vsize() as u64 + 10) * FLOOR_RATE);
                let extra_fee = target.checked_sub(old_fee).unwrap_or(Amount::ZERO);
                let returned = total
                    .checked_sub(extra_fee)
                    .context("publication coins cannot fund response floor")?;
                counter.tx.output[1].value += returned;
                ensure!(
                    counter.tx.lock_time == lngap_btc::bitcoin::absolute::LockTime::ZERO,
                    "publication response locktime must be zero"
                );
                // Keep the exact body, changing only the authorized script
                // paths for the light plain variant.
                let mut plain = counter.clone();
                plain.name = name.into();
                plain.leaf = prev.leaf(name)?.script.clone();
                plain.control = prev.control_block(name)?;
                for (input, tree) in plain.extra.iter_mut().zip(&trees) {
                    input.1 = tree.leaf("plain")?.script.clone();
                    input.2 = tree.control_block("plain")?;
                }
                p = plain;
            }
        }
        if leaf != name && publication.is_none() {
            let plain = Presign::new(name, prev, name, op, prev_out, p.tx.output.clone(), true)?;
            ensure!(
                plain.tx == p.tx,
                "respond's leaves spend into one transaction"
            );
            (p.leaf, p.control) = (plain.leaf, plain.control);
        }
        Ok(p)
    }

    /// Every transaction the parties pre-sign, the contract output at
    /// `funding`: the default; the chain and each output's terminal; per
    /// kept instance, `wrongly_challenged_k`; with the corrected exit, then
    /// the corrected chain from `respond`'s output, the same way.
    pub fn presign(&self, funding: OutPoint) -> Result<Vec<Presign>> {
        self.presign_with_publication(funding, None)
    }

    pub fn presign_with_publication(
        &self,
        funding: OutPoint,
        publication: Option<&Publication>,
    ) -> Result<Vec<Presign>> {
        let tree = self.tree()?;
        let c_out = TxOut {
            value: self.value(),
            script_pubkey: tree.script_pubkey(),
        };
        let mut out = vec![self.core().default_tx(&tree, funding)?];
        let alice: Terminal = &|b, v| b.pay_alice(v);
        let hub: Terminal = &|b, v| Ok(b.core().pay_hub(v));
        let chain = vec![
            ("request", self.requested()?, "request_timeout", alice),
            ("respond", self.responded()?, "respond_timeout", hub),
            ("assert", self.asserted()?, "success", alice),
            ("challenge_assert", self.challenged()?, "failure", hub),
        ];
        let end = self.chain(&mut out, (tree, funding, c_out), chain, publication)?;
        self.wrongly(
            &mut out,
            end,
            "wrongly_challenged",
            self.spec.hmsg.len(),
            alice,
        )?;
        if let Some(c) = &self.spec.corrected {
            let respond = out
                .iter()
                .find(|p| p.name == "respond")
                .context("respond is a link")?;
            let start = (
                self.responded()?,
                OutPoint {
                    txid: respond.tx.compute_txid(),
                    vout: 0,
                },
                respond.tx.output[0].clone(),
            );
            let alice: Terminal = &|b, v| b.pay_alice_corrected(v);
            let chain = vec![
                (
                    "assert_corrected",
                    self.asserted_corrected()?,
                    "success_corrected",
                    alice,
                ),
                (
                    "challenge_corrected",
                    self.challenged_corrected()?,
                    "failure_corrected",
                    hub,
                ),
            ];
            let end = self.chain(&mut out, start, chain, None)?;
            self.wrongly(
                &mut out,
                end,
                "wrongly_challenged_corrected",
                c.hmsg.len(),
                alice,
            )?;
        }
        Ok(out)
    }
    /// Pre-sign `links` from `prev` on, each with its terminal; the last
    /// output.
    fn chain(
        &self,
        out: &mut Vec<Presign>,
        (mut prev, mut op, mut prev_out): (TapTree, OutPoint, TxOut),
        links: Vec<(&str, TapTree, &str, Terminal<'_>)>,
        publication: Option<&Publication>,
    ) -> Result<(TapTree, OutPoint, TxOut)> {
        for (name, next, terminal, pay) in links {
            let link = self.link(
                name,
                &prev,
                op,
                &prev_out,
                &next,
                if name == "respond" { publication } else { None },
            )?;
            let n_out = link.tx.output[0].clone();
            let counter = if name == "respond" && self.spec.exit.is_some() {
                let mut counter = link.clone();
                counter.name = "respond_counter".into();
                counter.leaf = prev.leaf("respond_counter")?.script.clone();
                counter.control = prev.control_block("respond_counter")?;
                if let Some(publication) = publication {
                    for (input, tree) in counter
                        .extra
                        .iter_mut()
                        .zip(self.publication_trees(&publication.keys)?)
                    {
                        input.1 = tree.leaf("publish")?.script.clone();
                        input.2 = tree.control_block("publish")?;
                    }
                }
                Some(counter)
            } else {
                None
            };
            op = OutPoint {
                txid: link.tx.compute_txid(),
                vout: 0,
            };
            out.push(link);
            out.extend(counter);
            out.push(Presign::new(
                terminal,
                &next,
                terminal,
                op,
                &n_out,
                pay(self, n_out.value)?,
                false,
            )?);
            (prev, prev_out) = (next, n_out);
        }
        Ok((prev, op, prev_out))
    }
    /// Against a challenge: a kept instance's msg, `{name}_k` for each.
    fn wrongly(
        &self,
        out: &mut Vec<Presign>,
        (prev, op, prev_out): (TapTree, OutPoint, TxOut),
        name: &str,
        kept: usize,
        pay: Terminal<'_>,
    ) -> Result<()> {
        for k in 0..kept {
            let name = format!("{name}_{k}");
            let mut w = Presign::new(
                &name,
                &prev,
                &name,
                op,
                &prev_out,
                pay(self, prev_out.value)?,
                false,
            )?;
            w.tx.version = Version::non_standard(3);
            out.push(w);
        }
        Ok(())
    }

    /// Whether Alice keeps her signature on `name` from the hub.
    pub fn withheld(name: &str) -> bool {
        name.starts_with("wrongly_challenged_")
    }

    /// `challenge_assert`'s sighash in `set`, which Alice's adaptor
    /// pre-signatures sign.
    pub fn challenge_sighash(set: &[Presign]) -> Result<Message> {
        Self::sighash(set, "challenge_assert")
    }
    /// `challenge_corrected`'s, which her second adaptor keys pre-sign.
    pub fn challenge_corrected_sighash(set: &[Presign]) -> Result<Message> {
        Self::sighash(set, "challenge_corrected")
    }
    fn sighash(set: &[Presign], name: &str) -> Result<Message> {
        let p = set
            .iter()
            .find(|p| p.name == name)
            .with_context(|| format!("no {name}"))?;
        tapscript_sighash(&p.tx, 0, std::slice::from_ref(&p.prevout), &p.leaf)
    }

    // ----- the links' witnesses, below both signatures -----

    /// `request`'s: Alice's signature on `b`.
    pub fn request_wire(b: &WotsSig) -> Vec<Vec<u8>> {
        b.consumption_order().into_iter().rev().collect()
    }
    /// `request`'s with the exit: the claim as posted (`posted`, in
    /// [`POSTED_ITEMS`] items of up to `POSTED_ITEM` bytes, its first
    /// items), then
    /// Alice's signature on `E`, then on `b`.
    pub fn request_exit_wire(b: &WotsSig, e: &WotsSig, posted: &[u8]) -> Result<Vec<Vec<u8>>> {
        let items = posted_items(posted)
            .with_context(|| format!("a claim of {} bytes posted", posted.len()))?;
        Ok(items
            .into_iter()
            .map(<[u8]>::to_vec)
            .chain(e.consumption_order().into_iter().rev())
            .chain(Self::request_wire(b))
            .collect())
    }
    /// Read `request`'s arguments with the exit (consumption order, below
    /// both signatures): Alice's signatures on `b` and `E`, and the claim
    /// as posted.
    pub fn read_request_exit(args: &[Vec<u8>]) -> Result<(WotsSig, WotsSig, Vec<u8>)> {
        Self::read_request_exit_with_items(args, POSTED_ITEMS)
    }

    pub fn read_request_exit_with_items(
        args: &[Vec<u8>],
        items: usize,
    ) -> Result<(WotsSig, WotsSig, Vec<u8>)> {
        let (nb, ne) = (
            2 * WotsParams::for_bytes(4).total_digits() as usize,
            2 * WotsParams::for_bytes(32).total_digits() as usize,
        );
        ensure!(
            args.len() == nb + ne + items,
            "a request of {} arguments",
            args.len()
        );
        let b = WotsSig::from_consumption_order(WotsParams::for_bytes(4), &args[..nb])?;
        let e = WotsSig::from_consumption_order(WotsParams::for_bytes(32), &args[nb..nb + ne])?;
        Ok((
            b,
            e,
            args[nb + ne..].iter().rev().flatten().copied().collect(),
        ))
    }
    /// `respond`'s: the hub's signature on `b`, consumed first, then
    /// Alice's (from `request`).
    pub fn respond_wire(hub: &Wots256Sig, alice: &WotsSig) -> Vec<Vec<u8>> {
        hub.consumption_order()
            .into_iter()
            .chain(alice.consumption_order())
            .rev()
            .collect()
    }
    /// What `respond` carries with the exit, from its arguments
    /// (consumption order, below both signatures; `counter` for
    /// `respond_counter`'s): the `E` both signed, the hub's signature on it
    /// (its hashes, the statement's), and on `K`.
    pub fn read_respond_exit(args: &[Vec<u8>], counter: bool) -> Result<RespondExit> {
        let k = counter.then(|| WotsParams::for_bytes(K_LEN as u32));
        Ok(Self::read_respond(args, k, 0, 0)?.0)
    }
    /// Read `respond_counter`'s arguments for an exit whose counter is data
    /// below `K` ([`ExitKeys::counter_items`] items): the response, and the
    /// data.
    pub fn read_respond_counter(
        args: &[Vec<u8>],
        k: WotsParams,
        items: usize,
    ) -> Result<(RespondExit, Vec<u8>)> {
        Self::read_respond(args, Some(k), items, 0)
    }
    /// Read a corrected exit's `respond_counter` (`K` under `k`, no data):
    /// the response, and the hub's signature on `b_corr`.
    pub fn read_respond_corrected(
        args: &[Vec<u8>],
        k: WotsParams,
    ) -> Result<(RespondExit, Wots256Sig)> {
        let word = WotsParams::for_bytes(4);
        let b = 2 * word.total_digits() as usize;
        let b_corr = Wots256Sig::from_consumption_order(
            word,
            args.get(2 * b..3 * b)
                .context("respond_counter's arguments are too few")?,
        )?;
        Ok((Self::read_respond(args, Some(k), 0, b)?.0, b_corr))
    }
    /// `skip`: the arguments between Alice's `b` and her `E` (the hub's
    /// `b_corr`, for a corrected exit).
    fn read_respond(
        args: &[Vec<u8>],
        k: Option<WotsParams>,
        items: usize,
        skip: usize,
    ) -> Result<(RespondExit, Vec<u8>)> {
        let (b, e) = (
            2 * WotsParams::for_bytes(4).total_digits() as usize,
            WotsParams::for_bytes(32),
        );
        let args: Vec<Vec<u8>> = args
            .iter()
            .take(2 * b)
            .chain(args.iter().skip(2 * b + skip))
            .cloned()
            .collect();
        let (n, msg) = (e.total_digits() as usize, e.message_digits as usize);
        let alice_e = WotsSig::from_consumption_order(
            e,
            args.get(2 * b..2 * b + 2 * n)
                .context("respond's arguments are too few")?,
        )?;
        // the tied signature, top first: each checksum digit's value and
        // reveal, last first, then the message digits' reveals, last first
        let tied = args
            .get(2 * b + 2 * n..2 * b + 3 * n + (n - msg))
            .context("respond's arguments are too few")?;
        let mut hub_e = vec![[0u8; 32]; n];
        for (k, i) in (msg..n).rev().enumerate() {
            hub_e[i] = tied[2 * k + 1][..]
                .try_into()
                .context("a reveal of 32 bytes")?;
        }
        for (k, i) in (0..msg).rev().enumerate() {
            hub_e[i] = tied[2 * (n - msg) + k][..]
                .try_into()
                .context("a reveal of 32 bytes")?;
        }
        let mut at = 2 * b + 3 * n + (n - msg);
        let k = match k {
            Some(p) => {
                let sig = Wots256Sig::from_consumption_order(
                    p,
                    args.get(at..at + 2 * p.total_digits() as usize)
                        .context("respond_counter's arguments are too few")?,
                )?;
                at += 2 * p.total_digits() as usize;
                Some(sig)
            }
            None => None,
        };
        let data = args
            .get(at..at + items)
            .context("respond_counter's data items are too few")?;
        Ok((
            RespondExit {
                e: alice_e.message(),
                hub_e,
                k,
            },
            data.iter().rev().flatten().copied().collect(),
        ))
    }
    /// `respond`'s with the exit: as [`Babe::respond_wire`], then Alice's
    /// signature on `E` (from `request`) and the hub's, tied to it (its
    /// reveals, and its checksum digits); for `respond_counter`, then the
    /// hub's signature on `K`.
    pub fn respond_exit_wire(
        hub_b: &Wots256Sig,
        alice_b: &WotsSig,
        alice_e: &WotsSig,
        hub_e: &Wots256Sig,
        k: Option<&Wots256Sig>,
    ) -> Vec<Vec<u8>> {
        Self::respond_counter_wire(hub_b, alice_b, alice_e, hub_e, k, &[], 0)
            .expect("no data fits in no items")
    }
    /// `respond_counter`'s for an exit whose counter is data: as
    /// [`Babe::respond_exit_wire`] with `K`, the data below it in `items`
    /// items of up to [`POSTED_ITEM`] bytes (as `request` posts the claim).
    pub fn respond_counter_wire(
        hub_b: &Wots256Sig,
        alice_b: &WotsSig,
        alice_e: &WotsSig,
        hub_e: &Wots256Sig,
        k: Option<&Wots256Sig>,
        data: &[u8],
        items: usize,
    ) -> Result<Vec<Vec<u8>>> {
        Self::respond_wire_with(hub_b, alice_b, alice_e, hub_e, k, None, data, items)
    }
    /// A corrected exit's `respond_counter`: as [`Babe::respond_exit_wire`]
    /// with `K`, and the hub's signature on `b_corr` between Alice's `b`
    /// and her `E`.
    pub fn respond_corrected_wire(
        hub_b: &Wots256Sig,
        alice_b: &WotsSig,
        alice_e: &WotsSig,
        hub_e: &Wots256Sig,
        k: &Wots256Sig,
        b_corr: &Wots256Sig,
    ) -> Vec<Vec<u8>> {
        Self::respond_wire_with(
            hub_b,
            alice_b,
            alice_e,
            hub_e,
            Some(k),
            Some(b_corr),
            &[],
            0,
        )
        .expect("no data fits in no items")
    }
    #[allow(clippy::too_many_arguments)]
    fn respond_wire_with(
        hub_b: &Wots256Sig,
        alice_b: &WotsSig,
        alice_e: &WotsSig,
        hub_e: &Wots256Sig,
        k: Option<&Wots256Sig>,
        b_corr: Option<&Wots256Sig>,
        data: &[u8],
        items: usize,
    ) -> Result<Vec<Vec<u8>>> {
        ensure!(
            data.len() <= items * POSTED_ITEM,
            "counter data of {} bytes in {items} items",
            data.len()
        );
        let data = (0..items).map(|i| {
            data.get(i * POSTED_ITEM..)
                .map_or(vec![], |d| d[..d.len().min(POSTED_ITEM)].to_vec())
        });
        let msg = hub_e.params.message_digits as usize;
        // the tied signature, bottom first: each reveal, a checksum digit's
        // value above its reveal
        let tied = hub_e
            .hashes
            .iter()
            .zip(&hub_e.digits)
            .enumerate()
            .flat_map(|(i, (h, d))| {
                std::iter::once(h.to_vec()).chain((i >= msg).then(|| {
                    if *d == 0 {
                        vec![]
                    } else {
                        vec![*d]
                    }
                }))
            });
        let k = k
            .map(|k| k.consumption_order().into_iter().rev())
            .into_iter()
            .flatten();
        let b_corr = b_corr
            .map(|s| s.consumption_order().into_iter().rev())
            .into_iter()
            .flatten();
        Ok(data
            .chain(k)
            .chain(tied)
            .chain(alice_e.consumption_order().into_iter().rev())
            .chain(b_corr)
            .chain(Self::respond_wire(hub_b, alice_b))
            .collect())
    }
    /// `assert`'s: π1's digits, Lamport-signed, then π2 and π3.
    pub fn assert_wire(preimages: &[Preimage], pi2: &[u8], pi3: &[u8]) -> Vec<Vec<u8>> {
        [pi3.to_vec(), pi2.to_vec()]
            .into_iter()
            .chain(preimages.iter().rev().map(|p| p.to_vec()))
            .collect()
    }
    /// Read `assert`'s arguments (consumption order, below both
    /// signatures): the preimages, π2, π3.
    pub fn read_assert(args: &[Vec<u8>]) -> Option<Asserted<'_>> {
        (args.len() == DIGITS + 2)
            .then(|| (&args[..DIGITS], &args[DIGITS][..], &args[DIGITS + 1][..]))
    }
    /// `assert_corrected`'s: as `assert`'s (π1 under the second Lamport
    /// key), then the hub's signature on `b_corr` (from `respond_counter`)
    /// and Alice's under her second `b` key.
    pub fn assert_corrected_wire(
        preimages: &[Preimage],
        pi2: &[u8],
        pi3: &[u8],
        hub_b_corr: &Wots256Sig,
        alice_b_corr: &WotsSig,
    ) -> Vec<Vec<u8>> {
        Self::respond_wire(hub_b_corr, alice_b_corr)
            .into_iter()
            .chain(Self::assert_wire(preimages, pi2, pi3))
            .collect()
    }
    /// Read `assert_corrected`'s arguments: as [`Babe::read_assert`], and
    /// Alice's signature on `b_corr` (what the corrected payout's bits
    /// read).
    pub fn read_assert_corrected(args: &[Vec<u8>]) -> Result<(Asserted<'_>, WotsSig)> {
        let word = WotsParams::for_bytes(4);
        let b = 2 * word.total_digits() as usize;
        ensure!(
            args.len() == DIGITS + 2 + 2 * b,
            "assert_corrected of {} arguments",
            args.len()
        );
        let asserted =
            Self::read_assert(&args[..DIGITS + 2]).context("assert_corrected's assertion")?;
        Ok((
            asserted,
            WotsSig::from_consumption_order(word, &args[DIGITS + 2 + b..])?,
        ))
    }
    /// `challenge_assert`'s: the reveal ([`lichen_babe::reveal::reveal_args`]).
    pub fn challenge_wire(args: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        args.into_iter().rev().collect()
    }
    /// `wrongly_challenged`'s: `msg`.
    pub fn wrongly_wire(msg: &[u8; 32]) -> Vec<Vec<u8>> {
        vec![msg.to_vec()]
    }
}
