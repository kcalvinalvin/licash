//! The hub's side of a holder's exit, played from the chain on each poll.
//!
//! Her request names a claim `b`; her publication carries the filing. The
//! hub decides what the filing is owed against its evidence
//! ([`licash::recovery::owed`]), what its ledger holds against the claim
//! ([`Evidencer`]) that the statement accepts: owed all of `b`, it lets her
//! timeout pay her; owed less, it pins that evidence, responds `b′`,
//! discloses what the evidence needs published (a granted spend's unlock,
//! a preimage) in the response's disclose slot, proves its statement
//! ([`licash_hp::check`]) and asserts the proof. Challenged, it answers
//! with a kept instance whose circuit opens on the proof. Its evidence and
//! one-time signatures (`b′`, the proof's digits) are pinned in
//! `responses.sqlite` before they are made. Outputs of a payout that are
//! the hub's are swept to its wallet.
//!
//! Every step reads the chain again: nothing here is state but caches and
//! the pins. A stopped prover or a failed broadcast is retried on the next
//! poll.
//!
//! Before any of it, she may close her contract early with the hub: its
//! fold, on the contract output's key path ([`Openings::offer_fold`],
//! [`Openings::sign_fold`]). Only both parties sign a key-path spend: the
//! watcher takes any as the fold, and the contract as ended.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use bitcoin::block::Header;
use bitcoin::{Amount, Block, BlockHash, FeeRate, OutPoint, ScriptBuf, Transaction, TxOut};
use licash::claim::UnsignedClaim;
use licash::disclosure::Disclosures;
use licash::recovery::{self, Evidence, Filing};
use licash::request::Request;
use licash_contract::dispute::{self, presign, Payout};
use licash_contract::graph;
use licash_contract::graph::{link_args, HubProves};
use licash_contract::opening::{Hub, HubOpening};
use licash_onchain::deadline;
use licash_wallet::first_spend;
use lichen_contract::Role;
use sha2::{Digest, Sha256};

use super::prover::{self, Admission};
use super::{responses::Responses, Openings, Refused};
use crate::holder::node::Node;
use crate::FoldOffer;

/// What the hub's ledger holds against a claim: candidates, of which the
/// responder cites those the statement accepts. None known is no error:
/// the claim is then owed what its notes are worth.
pub type Evidencer = dyn Fn(&UnsignedClaim) -> Result<Vec<Evidence>> + Send + Sync;

/// A coin's first spend, once found: its block's height and it.
type Spend = Option<(u32, Transaction)>;

/// A signed opening as loaded, checked and its graph built: by the digest
/// of the state it was loaded from.
type Loaded = ([u8; 32], Arc<(Hub, HubOpening)>);

/// The caches of the responder: each signed opening as loaded, while its
/// state is unchanged; each coin's first spend once seen, and how far its
/// blocks were looked through; each header by its hash; what was noted
/// once.
#[derive(Default)]
pub(super) struct Watch {
    loaded: Mutex<HashMap<[u8; 32], Loaded>>,
    spends: Mutex<HashMap<OutPoint, (u32, Spend)>>,
    headers: Mutex<HashMap<BlockHash, Header>>,
    noted: Mutex<HashSet<([u8; 32], &'static str)>>,
    /// A dishonest hub's, for tests: it asserts proofs of another
    /// statement ([`Openings::assert_false_proofs`]).
    #[cfg(feature = "faults")]
    false_proofs: std::sync::atomic::AtomicBool,
}

/// A fold offered a holder: the hub's secret nonce, its holder's public
/// one then the hub's, and the fee. Kept in memory only, and taken out
/// before the hub signs: no nonce of it ever signs twice.
pub(super) struct PendingFold {
    sec: lichen_btc::musig::SecNonce,
    nonces: [lichen_btc::musig::PubNonce; 2],
    fee: Amount,
}

/// What a poll of one opening did.
#[derive(Debug, PartialEq, Eq)]
pub enum Played {
    /// Nothing to do yet.
    Idle,
    /// It broadcast `name`.
    Posted(&'static str),
}

impl Openings {
    /// Opening `id` loaded from `state`: checked, its graph built and
    /// signed, which takes seconds, once for each state it is in rather
    /// than every poll. Its checks see nothing but `state` and the trusted
    /// program.
    fn loaded(
        &self,
        id: &[u8; 32],
        state: &[u8],
        ctx: &licash_contract::opening::Context,
    ) -> Result<Arc<(Hub, HubOpening)>> {
        let digest: [u8; 32] = Sha256::digest(state).into();
        let cache = || {
            self.watch
                .loaded
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        if let Some((seen, loaded)) = cache().get(id) {
            if *seen == digest {
                return Ok(loaded.clone());
            }
        }
        let hub = Hub::load(state, ctx)?;
        let o = hub.signed()?;
        let loaded = Arc::new((hub, o));
        cache().insert(*id, (digest, loaded.clone()));
        Ok(loaded)
    }

    /// Forget the openings loaded but those of `ids`: played no more.
    pub(super) fn keep_loaded(&self, ids: &[[u8; 32]]) {
        let mut cache = self
            .watch
            .loaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache.retain(|id, _| ids.contains(id));
    }

    pub(super) async fn play(&self, id: &[u8; 32]) -> Result<Played> {
        let ctx = self.context().await.map_err(|e| anyhow!(e.reason))?;
        let row = self.store().row(id)?.context("no signed opening")?;
        ensure!(!row.abandoned, "an opening given up");
        let loaded = self.loaded(id, &row.state, &ctx)?;
        let (hub, o) = (&loaded.0, &loaded.1);
        let tip = ctx.tip;
        let funding = &o.funding;
        let contract = OutPoint {
            txid: funding.compute_txid(),
            vout: 0,
        };
        let from = o.base.parts().chain.checkpoint.height();
        // The cheap check first: her contract stands, and past its close
        // the default ends it.
        if self.unspent(contract, true).await? {
            let was = self.store().was_unfunded(id)?;
            if was && self.unspent(contract, false).await? {
                self.store().funded(id)?;
            }
            return self.close(id, hub, o, tip).await;
        }
        let Some((r_at, r)) = self.spend(contract, &funding.output[0], from, tip).await? else {
            return self.unfunded(id, hub, o, contract, tip).await;
        };
        self.store().funded(id)?;
        let default = presign(&o.set, "default")?;
        let folded = r
            .input
            .iter()
            .any(|i| i.previous_output == contract && i.witness.len() == 1);
        if folded
            || r.input.first().map(|i| i.previous_output) == Some(contract)
                && r.output == default.tx.output
        {
            // Each party's share went to its wallet: nothing left to play.
            self.end_if_deep(id, r_at, tip)?;
            return Ok(Played::Idle);
        }
        let request = presign(&o.set, "request")?;
        if r.compute_txid() != request.tx.compute_txid() {
            self.note_once(id, "unknown", "her contract went to an unknown transaction");
            return Ok(Played::Idle);
        }
        let main = |tx: &Transaction| OutPoint {
            txid: tx.compute_txid(),
            vout: 0,
        };
        // Whatever followed her request, then what follows that.
        let Some((x_at, x)) = self.spend(main(&r), &r.output[0], r_at, tip).await? else {
            return self.decide(id, hub, o, &r, r_at, tip).await;
        };
        let respond = presign(&o.set, "respond")?;
        if x.compute_txid() != respond.tx.compute_txid() {
            return self.collect(id, hub, o, &r, None, (x_at, &x), tip).await;
        }
        let Some((y_at, y)) = self.spend(main(&x), &x.output[0], x_at, tip).await? else {
            return self.assert(id, hub, o, &r, r_at, &x, x_at, tip).await;
        };
        let assert = presign(&o.set, "assert")?;
        if y.compute_txid() != assert.tx.compute_txid() {
            return self
                .collect(id, hub, o, &r, Some(&x), (y_at, &y), tip)
                .await;
        }
        let Some((z_at, z)) = self.spend(main(&y), &y.output[0], y_at, tip).await? else {
            return self.succeed(id, hub, o, y_at, tip).await;
        };
        let challenge = presign(&o.set, "challenge_assert")?;
        if z.compute_txid() != challenge.tx.compute_txid() {
            return self
                .collect(id, hub, o, &r, Some(&x), (z_at, &z), tip)
                .await;
        }
        match self.spend(main(&z), &z.output[0], z_at, tip).await? {
            None => {
                // Missing its deadline pays her claim and the hub's reserve.
                let unit = Amount::from_sat(o.base.parts().graph.unit_sats);
                let (b, _) = o.requested_b(&r)?;
                let (responded, _) = HubProves::responded_b(&x)?;
                let stake =
                    unit * u64::from(b.saturating_sub(responded)) + o.babe.spec.terms.reserve_hub;
                let g = &o.base.parts().graph;
                let deadline = z_at
                    .saturating_add(u32::from(g.answer_window))
                    .saturating_sub(1)
                    .saturating_sub(g.deadline_margin);
                self.answer(id, hub, o, &y, &z, (deadline, stake), tip)
                    .await
            }
            Some((end_at, end)) => {
                self.collect(id, hub, o, &r, Some(&x), (end_at, &end), tip)
                    .await
            }
        }
    }

    /// The first round of contract `id`'s fold: her public nonce `nonce`
    /// taken, the hub's made, and the fee it takes from its share at the
    /// rate of the day. Kept until she signs, or asks again.
    pub(super) async fn offer_fold(
        &self,
        id: &[u8; 32],
        nonce: &[u8],
    ) -> Result<FoldOffer, Refused> {
        let hers = lichen_btc::musig::PubNonce::from_slice(nonce).map_err(Refused::bad)?;
        let (hub, o) = self.standing(id).await?;
        let fee = fold_fee(&o, self.fee_rate().await).map_err(Refused::unavailable)?;
        let fold = crate::fold(&o.babe, &o.funding, fee).map_err(Refused::bad)?;
        // Its share above dust, or the fee would take it whole.
        if fold.tx.output.len() != 2 {
            return Err(Refused::unavailable("fees too high to fold now: later"));
        }
        let (sec, mine) = fold
            .nonce(&hub.keys().party.payment, id)
            .map_err(Refused::unavailable)?;
        let pending = PendingFold {
            sec,
            nonces: [hers, mine],
            fee,
        };
        self.folds().insert(*id, pending);
        tracing::info!(id = %crate::hex_id(id), %fee, "a fold offered");
        Ok(FoldOffer {
            nonce: mine.0,
            fee: fee.to_sat(),
        })
    }

    /// The second round of contract `id`'s fold: her partial signature
    /// on the fold last offered checked, the hub's added, the fold
    /// broadcast. Its nonce is taken out first, whatever follows: asked
    /// again, it is offered anew.
    pub(super) async fn sign_fold(
        &self,
        id: &[u8; 32],
        partial: &[u8],
    ) -> Result<Transaction, Refused> {
        let partial: lichen_btc::musig::Partial = partial
            .try_into()
            .map_err(|_| Refused::bad("a partial signature is 32 bytes"))?;
        let (hub, o) = self.standing(id).await?;
        let Some(pending) = self.folds().remove(id) else {
            return Err(Refused::bad("no fold offered: POST a nonce first"));
        };
        let fold = crate::fold(&o.babe, &o.funding, pending.fee).map_err(Refused::bad)?;
        let holder = &o.babe.core().pubs[0].payment;
        let nonces = &pending.nonces;
        fold.verify_partial(holder, &nonces[0], nonces, &partial)
            .map_err(|e| Refused::bad(format!("her partial signature: {e:#}")))?;
        let signed = fold
            .sign(&hub.keys().party.payment, pending.sec, nonces)
            .and_then(|mine| fold.finish(nonces, &[partial, mine]))
            .map_err(Refused::unavailable)?;
        // Hers to broadcast too: a failure here is no reason to keep it.
        if self.post(&signed).await {
            tracing::info!(id = %crate::hex_id(id), txid = %signed.compute_txid(), "contract folded");
        }
        Ok(signed)
    }

    /// Opening `id`, signed, its contract output unspent, even by the
    /// mempool: one to fold.
    async fn standing(&self, id: &[u8; 32]) -> Result<(Hub, HubOpening), Refused> {
        let ctx = self.context().await?;
        let row = self.store().row(id).map_err(Refused::unavailable)?;
        let row = row.ok_or_else(|| Refused::bad("no such contract"))?;
        if row.abandoned {
            return Err(Refused::bad("an opening given up"));
        }
        let hub = Hub::load(&row.state, &ctx).map_err(Refused::unavailable)?;
        let o = hub
            .signed()
            .map_err(|_| Refused::bad("not a signed contract"))?;
        let contract = OutPoint {
            txid: o.funding.compute_txid(),
            vout: 0,
        };
        if !self
            .unspent(contract, true)
            .await
            .map_err(Refused::unavailable)?
        {
            return Err(Refused::bad(
                "its contract output is spent, or not yet funded",
            ));
        }
        Ok((hub, o))
    }

    fn folds(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], PendingFold>> {
        self.folds.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Assert proofs of another statement from now on, as a dishonest hub
    /// would: for tests of the holder's challenge.
    #[cfg(feature = "faults")]
    pub fn assert_false_proofs(&self) {
        self.watch
            .false_proofs
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Her contract stands: past its close, the default, with a fee coin,
    /// pays each party its share (the hub `V_max` and its reserve).
    async fn close(&self, id: &[u8; 32], hub: &Hub, o: &HubOpening, tip: u32) -> Result<Played> {
        if tip < o.base.parts().graph.close_height {
            return Ok(Played::Idle);
        }
        let prevout = presign(&o.set, "default")?.prevout.clone();
        let tx = self
            .with_fee(id, hub, "default", o.default_close()?, prevout)
            .await?;
        if self.post(&tx).await && self.once(id, "default") {
            return Ok(Played::Posted("default"));
        }
        Ok(Played::Idle)
    }

    /// Her request stands: what her publication is owed, and the response
    /// if less than she claims.
    async fn decide(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        r: &Transaction,
        r_at: u32,
        tip: u32,
    ) -> Result<Played> {
        let slot = OutPoint {
            txid: r.compute_txid(),
            vout: SLOT,
        };
        let Some((p_at, p)) = self
            .spend(slot, &r.output[slot.vout as usize], r_at, tip)
            .await?
        else {
            // She never published: the slot is the hub's after k.
            let k = u32::from(o.babe.spec.windows.k);
            if tip.saturating_add(1) >= r_at.saturating_add(k) {
                // Its slot's value alone pays no fee: a fee coin with it.
                let prevout = r.output[slot.vout as usize].clone();
                let tx = o.reclaim(hub.keys())?;
                let tx = self.with_fee(id, hub, "reclaim", tx, prevout).await?;
                if self.post(&tx).await {
                    return Ok(Played::Posted("reclaim"));
                }
            }
            return Ok(Played::Idle);
        };
        let request = presign(&o.set, "request")?;
        if p.input.iter().all(|i| i.previous_output != slot) {
            return Ok(Played::Idle);
        }
        // Her slot spent by her publication, or by the hub's reclaim: then
        // her claim reads as empty, owed nothing, and is answered so.
        let depth = o.base.parts().chain.funding_depth.max(1);
        if tip.saturating_add(1) < p_at.saturating_add(depth) {
            return Ok(Played::Idle);
        }
        let block = self.block_at(p_at).await?;
        let filed = dispute::published(&request.tx, &block, &p.compute_txid())?;
        let (b, e) = o.requested_b(r)?;
        let headers = self.headers(o, p_at).await?;
        let chain = licash_hp::chain(&o.base, &headers).map_err(|e| anyhow!("{e}"))?;
        let request = Request {
            amount: b,
            claim_digest: e,
        };
        let owed = |evidence: &[Evidence]| {
            recovery::owed(&o.base, &request, &filed, evidence, &chain).map_err(|e| anyhow!("{e}"))
        };
        let pins = Responses::open(&self.data)?;
        let evidence = match pins.evidence(id)? {
            Some(pinned) => pinned_evidence(o, &pinned)?,
            None => self.select(o, &filed, owed)?,
        };
        let mut decided = owed(&evidence)?;
        if decided.owed >= b {
            self.note_once(
                id,
                "owed",
                "her claim is owed all she asks: her timeout pays her",
            );
            return Ok(Played::Idle);
        }
        // A response commits the hub to a proof in time: none unless its
        // prover has room for it (once signed, it is posted again whatever).
        let prover = self.prover.as_ref().filter(|_| !o.mock);
        let responded = pins.response(id)?.is_some();
        // At the earliest, its response confirms in the next block.
        let job = |evidence: &[Evidence]| {
            let x_at = tip.saturating_add(1);
            let release = x_at.saturating_add(o.base.parts().chain.work_depth);
            let bytes = recovery::evidence_to_bytes(evidence).len();
            (
                assert_deadline(o, x_at),
                release,
                estimated_cycles(o, bytes, release),
            )
        };
        if let Some(prover) = prover.filter(|_| !responded) {
            let (deadline, release, cycles) = job(&evidence);
            // In sats: what responding saves the hub.
            let unit = o.base.parts().graph.unit_sats;
            let stake = u64::from(b.saturating_sub(decided.owed)) * unit;
            match prover.admit(id, stake, cycles, deadline, release, tip) {
                Admission::Admit => {}
                Admission::Wait => return Ok(Played::Idle),
                Admission::Withhold(why) => {
                    if self.once(id, "withheld") {
                        self.withheld(id, b, decided.owed, &why);
                    }
                    return Ok(Played::Idle);
                }
            }
        }
        // What is pinned first is what the hub answers on.
        let pinned = pins.pin(id, r, &recovery::evidence_to_bytes(&evidence))?;
        let evidence = pinned_evidence(o, &pinned)?;
        decided = owed(&evidence)?;
        ensure!(
            decided.owed < b,
            "the pinned evidence leaves her claim owed"
        );
        tracing::warn!(
            id = %crate::hex_id(id),
            claimed = b,
            owed = decided.owed,
            cited = evidence.len(),
            "a contract claim is owed less than it asks: responding"
        );
        let keys = hub.keys();
        let response = pins.answer(id, r, || o.respond(keys, decided.owed))?;
        if let Some(prover) = prover {
            let (deadline, release, cycles) = job(&evidence);
            let program = o.base.parts().recipe.program;
            prover.commit(id, program, deadline, release, cycles, false, false, tip);
        }
        let window = (r_at, respond_deadline(o, r_at));
        let unit = Amount::from_sat(o.base.parts().graph.unit_sats);
        let stake = unit * u64::from(b.saturating_sub(decided.owed));
        let prevout = &r.output[0];
        self.post_link(id, hub, "respond", &response, prevout, window, stake, tip)
            .await?;
        // Its disclosure with it: the statement wants the contract's work
        // on top of both, within the assertion window.
        self.disclose(id, hub, o, &decided.disclosures).await?;
        Ok(Played::Posted("respond"))
    }

    /// Of the ledger's candidates against her filing, those the statement
    /// accepts, in order: each must void something the ones before did not.
    /// None against a claim ineligible or owed nothing.
    fn select(
        &self,
        o: &HubOpening,
        filed: &[u8],
        owed: impl Fn(&[Evidence]) -> Result<recovery::Owed>,
    ) -> Result<Vec<Evidence>> {
        let Some(ledger) = self.evidencer.get() else {
            return Ok(Vec::new());
        };
        let none = owed(&[])?;
        if none.ineligible.is_some() || none.owed == 0 {
            return Ok(Vec::new());
        }
        let p = o.base.parts();
        let filing = Filing::from_bytes(&p.keyset, p.limits.max_authorizations as usize, filed)
            .map_err(|e| anyhow!("{e}"))?;
        let mut kept = Vec::new();
        for candidate in ledger(filing.publication().claim())? {
            kept.push(candidate);
            if owed(&kept).is_err() {
                kept.pop();
            }
        }
        Ok(kept)
    }

    /// Post the disclosure of `disclosures` in the response's disclose
    /// slot, with a fee coin of the hub's: nothing if there is none to
    /// make. Whether it was broadcast.
    async fn disclose(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        disclosures: &Disclosures,
    ) -> Result<bool> {
        if *disclosures == Disclosures::empty() {
            return Ok(false);
        }
        let respond = presign(&o.set, "respond")?;
        let prevout = respond.tx.output[SLOT as usize].clone();
        let tx = o.disclosure(hub.keys(), &disclosures.to_bytes(), vec![])?;
        let tx = self.with_fee(id, hub, "disclose", tx, prevout).await?;
        Ok(self.post(&tx).await)
    }

    /// The hub responded: its proof that her claim is owed no more, once
    /// the response is deep enough, then its assertion. What it decides
    /// on, first found first: the assertion signed, the proof pinned for
    /// it, the proof made of the input frozen for it.
    #[allow(clippy::too_many_arguments)]
    async fn assert(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        r: &Transaction,
        r_at: u32,
        x: &Transaction,
        x_at: u32,
        tip: u32,
    ) -> Result<Played> {
        let tag = tagged(id, b"assert");
        let pins = Responses::open(&self.data)?;
        // Missing its deadline pays her claim and the hub's reserve.
        let (b, _) = o.requested_b(r)?;
        let (responded, _) = HubProves::responded_b(x)?;
        let unit = Amount::from_sat(o.base.parts().graph.unit_sats);
        let stake = unit * u64::from(b.saturating_sub(responded)) + o.babe.spec.terms.reserve_hub;
        let window = (x_at, assert_deadline(o, x_at));
        let post = |tx: Transaction| async move {
            self.post_link(id, hub, "assert", &tx, &x.output[0], window, stake, tip)
                .await
        };
        if let Some(tx) = pins.response(&tag)? {
            post(tx).await?;
            return Ok(Played::Posted("assert"));
        }
        let journal = graph::journal(&o.base);
        let kind = graph::kind(&o.base, o.mock);
        let proof = match pins.evidence(&tag)?.filter(|p| !p.is_empty()) {
            Some(pinned) => lichen_babe::statement::proof_from_bytes(&pinned)
                .context("the pinned proof does not decode")?,
            None => match self.prove(id, o, r, r_at, x, x_at, tip, hub).await? {
                Ok(proof) => proof,
                Err(played) => return Ok(played),
            },
        };
        ensure!(
            kind.verify(&journal, &proof) || self.false_proofs(),
            "the proof does not verify for this contract"
        );
        let tx = pins.answer(&tag, r, || o.assert(hub.keys(), &proof))?;
        post(tx).await?;
        Ok(Played::Posted("assert"))
    }

    /// Whether this hub asserts proofs of another statement (tests).
    fn false_proofs(&self) -> bool {
        #[cfg(feature = "faults")]
        return self
            .watch
            .false_proofs
            .load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(feature = "faults"))]
        false
    }

    /// The proof of the hub's statement for its assertion, pinned: once its
    /// response is deep enough, its disclosure too (else what it played
    /// instead), its input frozen, and its prover made it (the mock
    /// authority's at once).
    #[allow(clippy::too_many_arguments)]
    async fn prove(
        &self,
        id: &[u8; 32],
        o: &HubOpening,
        r: &Transaction,
        r_at: u32,
        x: &Transaction,
        x_at: u32,
        tip: u32,
        hub: &Hub,
    ) -> Result<std::result::Result<ark_groth16::Proof<ark_bn254::Bn254>, Played>> {
        let parts = o.base.parts();
        let depth = parts.chain.work_depth;
        let prover = self.prover.as_ref().filter(|_| !o.mock);
        let deadline = assert_deadline(o, x_at);
        let program = parts.recipe.program;
        let commit = |release: u32, cycles: u64, frozen: bool, proven: bool| {
            let Some(prover) = prover else { return };
            let fits = prover.commit(id, program, deadline, release, cycles, frozen, proven, tip);
            if proven {
                return;
            }
            if !fits && self.once(id, "late") {
                self.alert(&format!(
                    "contract {}: its proof may not be made before its assertion's deadline, \
                     block {deadline} (now {tip}): her timeout would pay her what she claims",
                    crate::hex_id(id)
                ));
            }
            if tip.saturating_add(NEAR) >= deadline && self.once(id, "near") {
                self.alert(&format!(
                    "contract {}: its assertion's deadline, block {deadline}, is near (now \
                     {tip}), and its proof not made",
                    crate::hex_id(id)
                ));
            }
        };
        let frozen = prover.and_then(|p| p.frozen(id));
        let bytes = match frozen {
            Some(bytes) => bytes,
            None => {
                let pinned = Responses::open(&self.data)?
                    .evidence(id)?
                    .context("responded on no pinned evidence")?;
                let release = x_at.saturating_add(depth);
                commit(
                    release,
                    estimated_cycles(o, pinned.len(), release),
                    false,
                    false,
                );
                if tip < release {
                    return Ok(Err(Played::Idle));
                }
                let input = match self
                    .input(id, hub, o, r, r_at, x, x_at, tip, &pinned)
                    .await?
                {
                    Ok(input) => input,
                    Err(played) => return Ok(Err(played)),
                };
                let bytes = input.to_bytes();
                match prover {
                    Some(prover) => prover.freeze(id, &bytes)?,
                    None => bytes,
                }
            }
        };
        let journal = graph::journal(&o.base);
        let kind = graph::kind(&o.base, o.mock);
        let proof = match prover {
            None => lichen_babe::statement::mock_prove(&journal),
            Some(prover) => {
                let seal = prover.proof(id, &program)?;
                let proof = match seal {
                    Some(seal) => lichen_babe::risc0::proof_from_seal(&seal)?,
                    None => {
                        commit(tip, prover::input_cycles(&bytes), true, false);
                        return Ok(Err(Played::Idle));
                    }
                };
                if !kind.verify(&journal, &proof) {
                    prover.reject(id, "it does not verify for this contract")?;
                    return Ok(Err(Played::Idle));
                }
                commit(tip, prover::input_cycles(&bytes), true, true);
                proof
            }
        };
        let proof = match self.false_proofs() {
            true => lichen_babe::statement::mock_prove(b"another statement"),
            false => proof,
        };
        let tag = tagged(id, b"assert");
        let pinned = Responses::open(&self.data)?
            .pin_proof(&tag, r, &lichen_babe::statement::proof_to_bytes(&proof))?
            .context("an assertion signed before its proof was pinned")?;
        let proof = lichen_babe::statement::proof_from_bytes(&pinned)
            .context("the pinned proof does not decode")?;
        Ok(Ok(proof))
    }

    /// The statement's input for the hub's assertion on `pinned` evidence,
    /// checked: once its disclosure, if the evidence needs one, is deep
    /// enough (else what it played instead).
    #[allow(clippy::too_many_arguments)]
    async fn input(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        r: &Transaction,
        r_at: u32,
        x: &Transaction,
        x_at: u32,
        tip: u32,
        pinned: &[u8],
    ) -> Result<std::result::Result<licash_hp::HubInput, Played>> {
        let depth = o.base.parts().chain.work_depth;
        let slot = OutPoint {
            txid: r.compute_txid(),
            vout: SLOT,
        };
        let (p_at, p) = self
            .spend(slot, &r.output[slot.vout as usize], r_at, tip)
            .await?
            .context("responded with no publication")?;
        let evidence = pinned_evidence(o, pinned)?;
        let disclosures = recovery::disclosures(&evidence).map_err(|e| anyhow!("{e}"))?;
        let disclosure = if disclosures == Disclosures::empty() {
            None
        } else {
            let slot = OutPoint {
                txid: x.compute_txid(),
                vout: SLOT,
            };
            match self
                .spend(slot, &x.output[SLOT as usize], x_at, tip)
                .await?
            {
                None => {
                    return Ok(Err(if self.disclose(id, hub, o, &disclosures).await? {
                        Played::Posted("disclose")
                    } else {
                        Played::Idle
                    }));
                }
                Some((d_at, _)) if tip < d_at.saturating_add(depth) => {
                    return Ok(Err(Played::Idle))
                }
                Some((d_at, d)) => Some((self.block_at(d_at).await?, d.compute_txid())),
            }
        };
        let (r_block, p_block, x_block) = (
            self.block_at(r_at).await?,
            self.block_at(p_at).await?,
            self.block_at(x_at).await?,
        );
        let headers = self.headers(o, tip).await?;
        let exit = dispute::exit(
            headers,
            &o.funding,
            (&r_block, &r.compute_txid()),
            (&p_block, &p.compute_txid()),
            (&x_block, &x.compute_txid()),
            disclosure.as_ref().map(|(block, txid)| (block, txid)),
        )?;
        let input = licash_hp::HubInput {
            base: o.base.clone(),
            exit,
            evidence,
        };
        match licash_hp::check(&input) {
            Ok(j) if j == graph::journal(&o.base) => Ok(Ok(input)),
            Ok(_) => bail!("the statement's journal is not the contract's"),
            Err(e) => bail!("the hub's statement does not hold: {e}"),
        }
    }

    /// Its assertion unchallenged past `success`'s wait: `success`, with a
    /// fee coin, paying the hub its reserve. She may post it too, but need
    /// not: with no claim published (the slot reclaimed), she has no fee
    /// coin of her own for it.
    async fn succeed(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        asserted_at: u32,
        tip: u32,
    ) -> Result<Played> {
        let success = presign(&o.set, "success")?;
        let wait = match success.tx.input[0].sequence.to_relative_lock_time() {
            Some(bitcoin::relative::LockTime::Blocks(h)) => u32::from(h.value()),
            _ => bail!("success waits for no number of blocks"),
        };
        if tip.saturating_add(1) < asserted_at.saturating_add(wait) {
            return Ok(Played::Idle);
        }
        let prevout = success.prevout.clone();
        let tx = o.finish("success", vec![])?;
        let tx = self.with_fee(id, hub, "success", tx, prevout).await?;
        if self.post(&tx).await && self.once(id, "success") {
            return Ok(Played::Posted("success"));
        }
        Ok(Played::Idle)
    }

    /// She challenged the assertion: answered with the first kept
    /// instance whose circuit opens on the proof, in a block by `by`'s
    /// deadline, its fee `by`'s stake at most ([`Self::with_fee_by`]).
    #[allow(clippy::too_many_arguments)]
    async fn answer(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        asserted: &Transaction,
        challenge: &Transaction,
        by: (u32, Amount),
        tip: u32,
    ) -> Result<Played> {
        for &i in &o.kept {
            let bytes = match self.store().kept_instance(id, i) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(id = %crate::hex_id(id), i, "a kept instance: {e:#}");
                    continue;
                }
            };
            match o.answer(asserted, challenge, i, &bytes) {
                Ok(Some(tx)) => {
                    let name = format!(
                        "wrongly_challenged_{}",
                        o.kept.iter().position(|n| *n == i).unwrap_or(0)
                    );
                    let prevout = presign(&o.set, &name)?.prevout.clone();
                    let tx = self
                        .with_fee_by(id, hub, &name, tx, prevout, by, tip)
                        .await?;
                    self.post(&tx).await;
                    return Ok(Played::Posted("wrongly_challenged"));
                }
                Ok(None) => continue,
                Err(e) => {
                    tracing::error!(id = %crate::hex_id(id), i, "answering with an instance: {e:#}");
                    continue;
                }
            }
        }
        self.note_once(id, "unanswered", "no kept instance answers her challenge");
        Ok(Played::Idle)
    }

    /// The one-input `tx` named `name`, spending `prevout`, with an exact
    /// fee coin of the hub's wallet appended: made once, kept in
    /// `openings.sqlite` before it is used.
    async fn with_fee(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        name: &str,
        tx: Transaction,
        prevout: TxOut,
    ) -> Result<Transaction> {
        let key = &hub.keys().party.payment;
        let script = dispute::fee_script(key)?;
        let cached = self.store().fee_coin(id, name)?;
        let coin = match cached {
            Some(c) => c,
            None => {
                let dummy = TxOut {
                    value: Amount::from_sat(330),
                    script_pubkey: script.clone(),
                };
                let size =
                    dispute::add_fee(tx.clone(), prevout.clone(), (OutPoint::null(), dummy), key)?
                        .vsize() as u64;
                let rate = self.fee_rate().await;
                let value = rate
                    .fee_vb(size * 2)
                    .context("fee overflow")?
                    .max(Amount::from_sat(330));
                let coin = self.fund_coin(&script, value).await?;
                // Kept at once: a restart pays with this one, not another.
                self.store().keep_fee_coin(id, name, &coin)?
            }
        };
        dispute::add_fee(tx, prevout, coin, key)
    }

    /// [`Self::with_fee`], for a terminal to be in a block by `deadline`,
    /// the last tip it confirms in time from: while it is in none, once the
    /// rate [`deadline::rate`] gives for the blocks left pays more than its
    /// coin, a larger coin is funded, `stake` at most, what missing the
    /// deadline costs the hub, and taken in place of the first once it is
    /// in a block (a replacement spends no other unconfirmed coin,
    /// BIP125). Kept beside the first, which stays valid.
    #[allow(clippy::too_many_arguments)]
    async fn with_fee_by(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        name: &str,
        tx: Transaction,
        prevout: TxOut,
        (deadline, stake): (u32, Amount),
        tip: u32,
    ) -> Result<Transaction> {
        let first = self
            .with_fee(id, hub, name, tx.clone(), prevout.clone())
            .await?;
        let first_coin = self.store().fee_coin(id, name)?.context("no fee coin")?;
        let mut extra = Vec::new();
        let kept = self.store().extra_coins(id, name)?;
        for c in kept {
            // In a block and unspent, else in the mempool, unspent.
            let state = match self.unspent(c.0, false).await? {
                true => Some(true),
                false => self.unspent(c.0, true).await?.then_some(false),
            };
            extra.push((c, state));
        }
        let left = deadline.saturating_sub(tip);
        let estimate = self
            .source
            .fee_rate(deadline::target(left))
            .await
            .ok()
            .flatten();
        let floor = FeeRate::from_sat_per_vb_u32(if estimate.is_some() { 1 } else { 2 });
        let rate = deadline::rate(estimate, floor, left);
        let needed = rate.fee_vb(first.vsize() as u64).context("fee overflow")?;
        let (coin, fund) = terminal_coin(first_coin, &extra, needed, stake);
        if let Some(value) = fund {
            let key = &hub.keys().party.payment;
            let more = self.fund_coin(&dispute::fee_script(key)?, value).await?;
            self.store().keep_extra_coin(id, name, &more)?;
            tracing::warn!(
                id = %crate::hex_id(id), %rate, %value, deadline, tip,
                "{name} pays too little to be in a block in time: a larger fee coin funded"
            );
        }
        dispute::add_fee(tx, prevout, coin, &hub.keys().party.payment)
    }

    /// A coin of `value` paying `script`, from the hub's wallet.
    async fn fund_coin(&self, script: &ScriptBuf, value: Amount) -> Result<(OutPoint, TxOut)> {
        let network = self.source.network().await?;
        let address = bitcoin::Address::from_script(script, network)?;
        let txid: String = self
            .wallet
            .rpc(
                "sendtoaddress",
                serde_json::json!([address.to_string(), value.to_btc()]),
            )
            .await
            .context("fund a fee coin")?;
        let funded: serde_json::Value = self
            .wallet
            .rpc("gettransaction", serde_json::json!([txid, false, true]))
            .await?;
        let raw: Transaction = bitcoin::consensus::encode::deserialize_hex(
            funded["hex"].as_str().context("no fee coin transaction")?,
        )?;
        let vout = raw
            .output
            .iter()
            .position(|o| &o.script_pubkey == script)
            .context("no fee coin output")?;
        Ok((
            OutPoint {
                txid: raw.compute_txid(),
                vout: vout as u32,
            },
            raw.output[vout].clone(),
        ))
    }

    /// Post link `name` of contract `id`, presigned at its floor, spending
    /// `prevout`, to be in a block by `deadline`, the last tip it confirms
    /// in time from, its window open since `opened`: with its fee child
    /// ([`dispute::link_child`]) while the rate [`deadline::rate`] gives
    /// for the blocks left is above its floor, made again at a higher one,
    /// in place of the one before, as the deadline nears. The pair pays
    /// `stake` at most, what missing the deadline costs the hub: past it,
    /// the operator is told, as when the link is in no block halfway
    /// through its window. The child's fee coins come from the hub's
    /// wallet, each kept before it is used; the child is kept before it is
    /// broadcast. Whether the node took the link.
    #[allow(clippy::too_many_arguments)]
    async fn post_link(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        name: &'static str,
        link: &Transaction,
        prevout: &TxOut,
        (opened, deadline): (u32, u32),
        stake: Amount,
        tip: u32,
    ) -> Result<bool> {
        if tip >= opened.saturating_add(deadline.saturating_sub(opened) / 2) && self.once(id, name)
        {
            self.alert(&format!(
                "contract {}: its {name} is in no block halfway to its deadline, block \
                 {deadline} (now {tip}): its fee is raised as the deadline nears",
                crate::hex_id(id)
            ));
        }
        let left = deadline.saturating_sub(tip);
        let estimate = self
            .source
            .fee_rate(deadline::target(left))
            .await
            .ok()
            .flatten();
        // Without an estimate, as `fee_rate`'s: 2 sat/vB.
        let floor = FeeRate::from_sat_per_vb_u32(if estimate.is_some() { 1 } else { 2 });
        let mut rate = deadline::rate(estimate, floor, left);
        let paid: Amount = link.output.iter().map(|o| o.value).sum();
        let link_fee = prevout
            .value
            .checked_sub(paid)
            .context("a link paying out more")?;
        let old = self.store().child(id, name)?;
        let one = FeeRate::from_sat_per_vb_u32(1);
        if rate <= one && old.is_none() {
            return Ok(self.post(link).await);
        }
        // The coins a child may spend: any while none was made; then those
        // the last one spent and those in a block (a replacement spends no
        // other unconfirmed coin, BIP125).
        let mut usable = Vec::new();
        let kept = self.store().extra_coins(id, name)?;
        let waiting = kept.len();
        for coin in kept {
            let spent_before = old
                .as_ref()
                .is_some_and(|old| old.input.iter().any(|i| i.previous_output == coin.0));
            if old.is_none() || spent_before || self.unspent(coin.0, false).await? {
                usable.push(coin);
            }
        }
        let link_vb = link.vsize() as u64;
        let pair = |inputs: usize, rate: FeeRate| {
            let fee = rate
                .fee_vb(link_vb + child_vb(inputs))
                .unwrap_or(Amount::MAX);
            fee.checked_sub(link_fee).unwrap_or(Amount::ZERO)
        };
        // At most the stake, the link's fee among it.
        let inputs = usable.len().max(1) + 1;
        let most = stake.to_sat().saturating_add(link_fee.to_sat()) * 1_000
            / ((link_vb + child_vb(inputs)) * 4).max(1);
        let most = FeeRate::from_sat_per_kwu(most);
        if rate > most {
            let capped = match name {
                "respond" => "respond capped",
                _ => "assert capped",
            };
            if self.once(id, capped) {
                self.alert(&format!(
                    "contract {}: its {name} would pay {rate} to be in a block by block \
                     {deadline} (now {tip}), more than the {stake} at stake: it pays {most}",
                    crate::hex_id(id)
                ));
            }
            rate = most;
        }
        let have: Amount = usable.iter().map(|c| c.1.value).sum();
        // One coin more, for twice the rate too, the stake at most: unless
        // one funded already waits for a block.
        let waiting = waiting > usable.len();
        if have < pair(inputs, rate) && !waiting {
            let value = pair(
                inputs + 1,
                FeeRate::from_sat_per_kwu(rate.to_sat_per_kwu() * 2),
            )
            .checked_sub(have)
            .unwrap_or(Amount::ZERO)
            .clamp(Amount::from_sat(1_000), stake.max(Amount::from_sat(1_000)));
            let key = &hub.keys().party.payment;
            let coin = self.fund_coin(&dispute::fee_script(key)?, value).await?;
            self.store().keep_extra_coin(id, name, &coin)?;
            if old.is_none() {
                usable.push(coin);
            }
        }
        let change = match &old {
            Some(old) => old.output[0].script_pubkey.clone(),
            None => self.payout_script().await?,
        };
        let key = &hub.keys().party.payment;
        let sign = |tx: &mut Transaction, prevouts: &[TxOut]| {
            (0..tx.input.len()).try_for_each(|i| dispute::sign_fee_input(key, tx, i, prevouts))
        };
        let build = |rate| dispute::link_child(link, link_fee, &usable, change.clone(), rate, sign);
        let child = match (build(rate), &old) {
            (Ok(child), None) => Some(child),
            (Ok(child), Some(old)) => {
                let coins = self.store().extra_coins(id, name)?;
                let fee = |tx: &Transaction| child_fee(tx, &link.output, &coins);
                let (new, before) = (fee(&child), fee(old));
                if new <= before {
                    None
                } else if deadline::replaces(new, child.vsize() as u64, before) {
                    Some(child)
                } else {
                    // The least that replaces it, as a rate of the pair.
                    let least = before.to_sat() + child.vsize() as u64 + link_fee.to_sat();
                    let weight = (link_vb + child.vsize() as u64) * 4;
                    let least = FeeRate::from_sat_per_kwu((least * 1_000).div_ceil(weight));
                    build(least)
                        .ok()
                        .filter(|c| deadline::replaces(fee(c), c.vsize() as u64, before))
                }
            }
            // Its coins short until the one funded is in a block.
            (Err(e), _) => {
                tracing::debug!(id = %crate::hex_id(id), "{name}'s child waits: {e:#}");
                None
            }
        };
        let child = match child {
            Some(child) => {
                self.store().keep_child(id, name, &child)?;
                tracing::info!(
                    id = %crate::hex_id(id), txid = %child.compute_txid(), %rate,
                    "{name}'s fee child made"
                );
                child
            }
            None => match old {
                Some(old) => old,
                None => return Ok(self.post(link).await),
            },
        };
        match self.source.broadcast_package(&[link.clone(), child]).await {
            Ok(_) => Ok(true),
            Err(e) => {
                tracing::debug!(txid = %link.compute_txid(), "not broadcast yet: {e:#}");
                Ok(false)
            }
        }
    }

    /// A payout ended the exit: the hub's outputs of it, swept to its
    /// wallet once their waits are over.
    #[allow(clippy::too_many_arguments)]
    async fn collect(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        r: &Transaction,
        response: Option<&Transaction>,
        (paid_at, payout): (u32, &Transaction),
        tip: u32,
    ) -> Result<Played> {
        let spent = payout
            .input
            .first()
            .context("an empty payout")?
            .previous_output;
        let terminal = o
            .set
            .iter()
            .find(|p| {
                p.tx.input
                    .first()
                    .is_some_and(|i| i.previous_output == spent)
                    && p.tx.output == payout.output
            })
            .map(|p| p.name.clone())
            .context("the exit went to an unknown transaction")?;
        let (amount, wire, value_key, signer) = match terminal.as_str() {
            "request_timeout" | "respond_timeout" | "failure" => {
                let (b, _) = HubProves::read_request(&link_args(r))?;
                let (amount, _) = o.requested_b(r)?;
                (amount, HubProves::b_wire(&b), &o.holder.b_key, Role::User)
            }
            n if n == "success" || n.starts_with("wrongly_challenged_") => {
                let (b, wire) = HubProves::responded_b(response.context("no response")?)?;
                (b, wire, &o.hub.b_key, Role::Hub)
            }
            _ => bail!("not a payout: {terminal}"),
        };
        let owned = o
            .babe
            .core()
            .owned(payout, value_key, signer, amount, Role::Hub)?;
        if owned.is_empty() {
            self.note_once(id, "nothing", "nothing of the payout is the hub's");
            self.end_if_deep(id, paid_at, tip)?;
            return Ok(Played::Idle);
        }
        // Swept, in a block: ended once the payout is deep. The sweep pays
        // the hub's wallet, whose node posts it again should a
        // reorganization drop it.
        let mut swept = true;
        for out in &owned {
            swept &= !self.unspent(out.op, false).await?;
        }
        if swept {
            self.end_if_deep(id, paid_at, tip)?;
            return Ok(Played::Idle);
        }
        let to = self.payout_script().await?;
        let rate = self.fee_rate().await;
        let sweep = dispute::sweep(
            &o.babe,
            &hub.keys().party.payment,
            Payout {
                transaction: payout,
                value_key,
                signer,
                amount,
                wire: &wire,
            },
            Role::Hub,
            to,
            rate,
        )?;
        // Its inputs' waits may not be over: the node says so, and the next
        // poll tries again. Swept once, the outputs are spent and this
        // payout is never looked at again.
        if self.post(&sweep).await {
            return Ok(Played::Posted("sweep"));
        }
        Ok(Played::Idle)
    }

    /// Signed opening `id`, `hub` and `o`, whose F is in no block at `tip`
    /// (its `contract` output neither unspent nor spent in one). Given up,
    /// its coins unlocked, once a coin F spends is spent in a block by
    /// another transaction [`ENDED_DEPTH`] deep (her funding depth, if
    /// deeper): F can never confirm then. Short of it, F is made so
    /// [`UNFUNDED_FOR`] blocks after it was first seen in no block, or once
    /// it may no longer fund its contract: one of the hub's coins F spends
    /// is spent back to its wallet, while nothing spends any, in the
    /// mempool either. F confirming first, the contract stands. Its coins
    /// are never unlocked before: F, signed by the hub, binds them until
    /// it may never confirm.
    async fn unfunded(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        contract: OutPoint,
        tip: u32,
    ) -> Result<Played> {
        // Unspent in the blocks, spent in the mempool: F is in a block,
        // her request not yet. Played once it is.
        if self.unspent(contract, false).await? {
            return Ok(Played::Idle);
        }
        let since = self.store().unfunded(id, tip)?;
        let Some(f) = hub.funding() else {
            return Ok(Played::Idle);
        };
        let txid = f.tx.compute_txid();
        let from = o.base.parts().chain.checkpoint.height();
        let deep = o.base.parts().chain.funding_depth.max(ENDED_DEPTH);
        let mut untouched = true;
        for (input, coin) in f.tx.input.iter().zip(f.prevouts) {
            let outpoint = input.previous_output;
            if self.unspent(outpoint, false).await? {
                untouched &= self.unspent(outpoint, true).await?;
                continue;
            }
            untouched = false;
            let Some((at, by)) = self.spend(outpoint, coin, from, tip).await? else {
                continue;
            };
            let by = by.compute_txid();
            if by == txid || tip.saturating_sub(at) + 1 < deep {
                continue;
            }
            // Read again, as cached spends are kept: a reorganization may
            // have taken it out of its block.
            let block = self.block_at(at).await?;
            if !block.txdata.iter().any(|tx| tx.compute_txid() == by) {
                self.forget_spend(outpoint);
                continue;
            }
            return self.give_up_unfunded(id, outpoint, by).await;
        }
        let due = tip >= since.saturating_add(UNFUNDED_FOR) || hub.lapsed(tip);
        if !due || !untouched {
            return Ok(Played::Idle);
        }
        let Some((outpoint, coin)) = f.hub_coins().into_iter().next() else {
            return Ok(Played::Idle);
        };
        let spent = self.spend_back(outpoint, coin).await?;
        tracing::warn!(
            id = %crate::hex_id(id), %txid, since, coin = %outpoint, spent = %spent,
            "a signed opening's F in no block: a coin of the hub's it spends spent back, so that it never confirms"
        );
        Ok(Played::Idle)
    }

    /// Spend the hub's `coin` at `outpoint` whole back to its wallet, at
    /// the node's rate: the transaction, broadcast.
    async fn spend_back(&self, outpoint: OutPoint, coin: &TxOut) -> Result<bitcoin::Txid> {
        let to = self.payout_script().await?;
        let make = |fee: Amount| -> Result<Transaction> {
            let value = coin
                .value
                .checked_sub(fee)
                .context("a coin smaller than its fee")?;
            ensure!(
                value >= to.minimal_non_dust(),
                "a coin worth no more than its fee"
            );
            Ok(Transaction {
                version: bitcoin::transaction::Version::TWO,
                lock_time: bitcoin::absolute::LockTime::ZERO,
                input: vec![bitcoin::TxIn {
                    previous_output: outpoint,
                    sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
                    ..Default::default()
                }],
                output: vec![TxOut {
                    value,
                    script_pubkey: to.clone(),
                }],
            })
        };
        // Signed once to weigh it, its size the same at any fee.
        let weighed = self.sign(&make(Amount::ZERO)?, &[]).await?;
        let rate = self.fee_rate().await;
        let fee = rate
            .fee_vb(weighed.vsize() as u64)
            .context("a fee too large")?;
        let tx = self.sign(&make(fee)?, &[]).await?;
        ensure!(self.post(&tx).await, "{} not broadcast", tx.compute_txid());
        Ok(tx.compute_txid())
    }

    /// Give up signed opening `id`, its F never to confirm, `outpoint`
    /// spent by `by` deep in a block: its coins unlocked. Not while a step
    /// of its holder's runs: tried again next poll.
    async fn give_up_unfunded(
        &self,
        id: &[u8; 32],
        outpoint: OutPoint,
        by: bitcoin::Txid,
    ) -> Result<Played> {
        let coins = {
            let busy = self.busy();
            if busy.contains(id) {
                return Ok(Played::Idle);
            }
            self.store().give_up_unfunded(id)?
        };
        if let Some(coins) = coins {
            self.unlock_coins(&coins).await;
            tracing::warn!(
                id = %crate::hex_id(id), coin = %outpoint, %by,
                "a signed opening given up, its F never to confirm: its coins unlocked"
            );
        }
        Ok(Played::Idle)
    }

    /// Look for `coin`'s spend again from the start: the one found left
    /// its block.
    fn forget_spend(&self, coin: OutPoint) {
        (self
            .watch
            .spends
            .lock()
            .unwrap_or_else(PoisonError::into_inner))
        .remove(&coin);
    }

    /// A new script of the hub's wallet: what its openings and sweeps pay
    /// the hub to.
    pub(super) async fn payout_script(&self) -> Result<ScriptBuf> {
        let address: String = self
            .wallet
            .rpc("getnewaddress", serde_json::json!(["", "bech32m"]))
            .await
            .context("a new address of the hub's wallet")?;
        Ok(address
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()?
            .assume_checked()
            .script_pubkey())
    }

    /// Whether `coin` is unspent: by the blocks, and by the mempool too
    /// if `mempool`.
    async fn unspent(&self, coin: OutPoint, mempool: bool) -> Result<bool> {
        let out: Option<serde_json::Value> = self
            .wallet
            .rpc_optional(
                "gettxout",
                serde_json::json!([coin.txid.to_string(), coin.vout, mempool]),
            )
            .await?;
        Ok(out.is_some())
    }

    /// Opening `id` ended at block `at`: not looked at again once that
    /// block is [`ENDED_DEPTH`] deep at `tip`.
    fn end_if_deep(&self, id: &[u8; 32], at: u32, tip: u32) -> Result<()> {
        if tip.saturating_sub(at) + 1 >= ENDED_DEPTH {
            self.store().end(id, at)?;
            tracing::info!(id = %crate::hex_id(id), at, "contract ended");
        }
        Ok(())
    }

    /// The first spend of `coin` (paying `out`) in blocks `from..=tip`,
    /// looked up once each block.
    async fn spend(
        &self,
        coin: OutPoint,
        out: &TxOut,
        from: u32,
        tip: u32,
    ) -> Result<Option<(u32, Transaction)>> {
        let seen = self
            .watch
            .spends
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&coin)
            .cloned();
        let start = match seen {
            Some((_, Some(found))) => return Ok(Some(found)),
            // Look again at the last blocks seen: a reorganization.
            Some((through, None)) => through.saturating_sub(3).max(from),
            None => from,
        };
        if start > tip {
            return Ok(None);
        }
        let node = Node(self.source.clone());
        let found = first_spend(&node, coin, &out.script_pubkey, start..=tip)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        self.watch
            .spends
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(coin, (tip, found.clone()));
        Ok(found)
    }

    async fn block_at(&self, height: u32) -> Result<Block> {
        let hash = self.source.hash(height).await?;
        let block = self.source.block(&hash).await?;
        ensure!(
            block.block_hash() == hash && block.check_merkle_root(),
            "block {hash}"
        );
        Ok(block)
    }

    /// The headers after `o`'s checkpoint through block `to`: back from
    /// `to`'s, each its parent's child, those seen before not fetched again
    /// (a header is its hash's).
    async fn headers(&self, o: &HubOpening, to: u32) -> Result<Vec<[u8; 80]>> {
        let parts = o.base.parts();
        let from = parts.chain.checkpoint.height().saturating_add(1);
        let to = to.min(
            from.saturating_add(parts.chain.max_headers)
                .saturating_sub(1),
        );
        if to < from {
            return Ok(Vec::new());
        }
        let mut hash = self.source.hash(to).await?;
        let mut headers = Vec::with_capacity((to - from + 1) as usize);
        for _ in from..=to {
            let cached = self.header_cache().get(&hash).copied();
            let header = match cached {
                Some(header) => header,
                None => {
                    let header = self.source.header(&hash).await?;
                    ensure!(header.block_hash() == hash, "header {hash}");
                    self.header_cache().insert(hash, header);
                    header
                }
            };
            let bytes = bitcoin::consensus::serialize(&header);
            headers.push(
                bytes
                    .try_into()
                    .map_err(|_| anyhow!("a header of 80 bytes"))?,
            );
            hash = header.prev_blockhash;
        }
        headers.reverse();
        Ok(headers)
    }

    fn header_cache(&self) -> std::sync::MutexGuard<'_, HashMap<BlockHash, Header>> {
        self.watch
            .headers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    async fn fee_rate(&self) -> FeeRate {
        match self.source.fee_rate(6).await {
            Ok(Some(rate)) => rate.max(FeeRate::from_sat_per_vb_u32(1)),
            _ => FeeRate::from_sat_per_vb_u32(2),
        }
    }

    /// Broadcast `tx`: whether the node took it.
    async fn post(&self, tx: &Transaction) -> bool {
        match self.source.broadcast(tx).await {
            Ok(txid) => {
                tracing::info!(%txid, "contract transaction broadcast");
                true
            }
            Err(e) => {
                tracing::debug!(txid = %tx.compute_txid(), "not broadcast yet: {e:#}");
                false
            }
        }
    }

    fn note_once(&self, id: &[u8; 32], what: &'static str, message: &str) {
        if self.once(id, what) {
            tracing::info!(id = %crate::hex_id(id), "{message}");
        }
    }

    /// Whether this is the first time `what` happened to `id` since the
    /// hub started.
    fn once(&self, id: &[u8; 32], what: &'static str) -> bool {
        self.watch
            .noted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((*id, what))
    }
}

/// How many blocks before an assertion's deadline its operator is told
/// its proof is not made.
const NEAR: u32 = 6;

/// The fee coin a terminal pays with, needing `needed` to be in a block in
/// time, of `stake` at most: the largest of `first` and the `extra` ones in
/// a block (`Some(true)`), and the value of a larger one to fund, unless
/// one is in the mempool only (`Some(false)`) or pays enough already (none
/// are spent ones).
fn terminal_coin(
    first: (OutPoint, TxOut),
    extra: &[((OutPoint, TxOut), Option<bool>)],
    needed: Amount,
    stake: Amount,
) -> ((OutPoint, TxOut), Option<Amount>) {
    let needed = needed.min(stake);
    let confirmed = extra
        .iter()
        .filter(|(_, s)| *s == Some(true))
        .map(|(c, _)| c);
    let coin = confirmed.fold(first, |best, c| match c.1.value > best.1.value {
        true => c.clone(),
        false => best,
    });
    let waiting = extra
        .iter()
        .any(|(c, s)| *s == Some(false) && c.1.value >= needed);
    let fund = (coin.1.value < needed && !waiting).then(|| (needed * 2).min(stake.max(needed)));
    (coin, fund)
}

/// About the virtual size of a link's fee child of `inputs` inputs, each
/// a script spend of the hub's fee key, and one output.
fn child_vb(inputs: usize) -> u64 {
    54 + 76 * inputs as u64
}

/// The fee of `child`, spending the anchor of a link of `outputs` and some
/// of `coins`.
fn child_fee(child: &Transaction, outputs: &[TxOut], coins: &[(OutPoint, TxOut)]) -> Amount {
    let anchor = outputs
        .get(dispute::ANCHOR_VOUT as usize)
        .map_or(Amount::ZERO, |o| o.value);
    let spent: Amount = coins
        .iter()
        .filter(|c| child.input.iter().any(|i| i.previous_output == c.0))
        .map(|c| c.1.value)
        .sum();
    let paid: Amount = child.output.iter().map(|o| o.value).sum();
    (anchor + spent).checked_sub(paid).unwrap_or(Amount::ZERO)
}

/// The last tip at which the hub's response, broadcast, still confirms
/// before her `request_timeout`, `w_h` blocks after her request at `r_at`:
/// `deadline_margin` blocks to confirm in.
fn respond_deadline(o: &HubOpening, r_at: u32) -> u32 {
    let g = &o.base.parts().graph;
    r_at.saturating_add(u32::from(g.response_window))
        .saturating_sub(1)
        .saturating_sub(g.deadline_margin)
}

/// The last tip at which the hub's assertion, broadcast, still confirms
/// before her `respond_timeout`, `w_a` blocks after its response at
/// `x_at`: `deadline_margin` blocks to confirm in.
fn assert_deadline(o: &HubOpening, x_at: u32) -> u32 {
    let g = &o.base.parts().graph;
    x_at.saturating_add(u32::from(g.assert_window))
        .saturating_sub(1)
        .saturating_sub(g.deadline_margin)
}

/// The cycles of the proof of `o`'s statement on `evidence` bytes of
/// evidence, its input made at `at`: until its input is, from its headers
/// and its size (the exit's transactions and proofs about 8 kB).
fn estimated_cycles(o: &HubOpening, evidence: usize, at: u32) -> u64 {
    let chain = &o.base.parts().chain;
    let from = chain.checkpoint.height().saturating_add(1);
    let headers = at
        .saturating_sub(from)
        .saturating_add(1)
        .min(chain.max_headers);
    let other = o.base.to_bytes().len() + evidence + 8_192;
    prover::cycles(u64::from(headers), other as u64)
}

/// The evidence pinned as `bytes`: none for empty bytes (a response pinned
/// before evidence was).
fn pinned_evidence(o: &HubOpening, bytes: &[u8]) -> Result<Vec<Evidence>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let p = o.base.parts();
    recovery::evidence_from_bytes(&p.keyset, p.limits.max_authorizations as usize, bytes)
        .map_err(|e| anyhow!("the pinned evidence: {e}"))
}

fn tagged(id: &[u8; 32], tag: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"licash/contracts/hub-pin");
    h.update(id);
    h.update(tag);
    h.finalize().into()
}

/// How many confirmations an end of a contract takes to be final for the
/// responder.
const ENDED_DEPTH: u32 = 6;

/// Blocks after a signed opening's F was first seen in no block from which
/// the hub makes sure it never confirms: a day. Its holder broadcasts it
/// once signed.
pub(super) const UNFUNDED_FOR: u32 = 144;

/// The slot output of a request, her publication spending it, and of a
/// response, the hub's disclosure spending it (Lichen's `SLOT_VOUT`).
const SLOT: u32 = 2;

/// The fee of `o`'s fold at `rate`: of its size signed, one key-path
/// signature its witness.
fn fold_fee(o: &HubOpening, rate: FeeRate) -> Result<Amount> {
    let mut tx = crate::fold(&o.babe, &o.funding, Amount::ZERO)?.tx;
    tx.input[0].witness = bitcoin::Witness::from_slice(&[[0u8; 64]]);
    rate.fee_vb(tx.vsize() as u64).context("fee overflow")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coin(n: u8, sat: u64) -> (OutPoint, TxOut) {
        let txid = bitcoin::hashes::Hash::from_byte_array([n; 32]);
        let out = TxOut {
            value: Amount::from_sat(sat),
            script_pubkey: ScriptBuf::new(),
        };
        (OutPoint { txid, vout: 0 }, out)
    }

    #[test]
    fn a_terminal_pays_with_its_largest_coin_in_a_block_and_funds_a_larger_one_once() {
        let sat = Amount::from_sat;
        let (first, stake) = (coin(1, 1_000), sat(50_000));
        // Its first pays enough: nothing funded.
        assert_eq!(
            terminal_coin(first.clone(), &[], sat(800), stake),
            (first.clone(), None)
        );
        // Too little: twice what it needs, the stake at most.
        assert_eq!(
            terminal_coin(first.clone(), &[], sat(3_000), stake).1,
            Some(sat(6_000))
        );
        assert_eq!(
            terminal_coin(first.clone(), &[], sat(30_000), stake).1,
            Some(stake)
        );
        assert_eq!(
            terminal_coin(first.clone(), &[], sat(90_000), stake).1,
            Some(stake)
        );
        // One funded, in the mempool only: waited for, the first paid with.
        let pending = [(coin(2, 6_000), Some(false))];
        assert_eq!(
            terminal_coin(first.clone(), &pending, sat(3_000), stake),
            (first.clone(), None)
        );
        // The rate rose past it meanwhile: a larger one more.
        assert_eq!(
            terminal_coin(first.clone(), &pending, sat(8_000), stake).1,
            Some(sat(16_000))
        );
        // In a block: the largest paid with; a spent one never.
        let extra = [
            (coin(2, 6_000), Some(true)),
            (coin(3, 4_000), Some(true)),
            (coin(4, 9_000), None),
        ];
        assert_eq!(
            terminal_coin(first, &extra, sat(3_000), stake),
            (coin(2, 6_000), None)
        );
    }
}
