//! The hub's side of a holder's exit, played from the chain on each poll.
//!
//! Her request names a claim `b`; her publication carries the filing. The
//! hub decides what the filing is owed against its evidence
//! ([`licash::recovery::owed`]): owed all of `b`, it lets her timeout pay
//! her; owed less, it responds `b′`, proves its statement
//! ([`licash_hp::check`]) and asserts the proof. Challenged, it answers
//! with a kept instance whose circuit opens on the proof. Its one-time
//! signatures (`b′`, the proof's digits) are pinned in `responses.sqlite`
//! before they are made. Outputs of a payout that are the hub's are swept
//! to its wallet.
//!
//! Every step reads the chain again: nothing here is state but caches and
//! the pins. Missing evidence, a stopped prover or a failed broadcast are
//! retried on the next poll.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use bitcoin::{Amount, Block, FeeRate, OutPoint, Transaction, TxOut};
use licash::recovery::Evidence;
use licash_contract::dispute::{self, presign, Payout};
use licash_contract::graph;
use licash_contract::opening::{Hub, HubOpening};
use licash_wallet::first_spend;
use lichen_babe::hp::{link_args, HubProves};
use lichen_contract::Role;
use sha2::{Digest, Sha256};

use super::{responses::Responses, Openings};
use crate::holder::node::Node;

/// What the hub knows against a filing: its evidence. None known is no
/// error: the filing is then owed what its notes are worth.
pub type Evidencer = dyn Fn(&HubOpening, &[u8]) -> Result<Vec<Evidence>> + Send + Sync;

/// The caches of the responder: each coin's first spend once seen, and
/// how far its blocks were looked through; the proofs being made; the
/// fee coins made for terminals.
/// A coin's first spend, once found: its block's height and it.
type Spend = Option<(u32, Transaction)>;

/// A fee coin made for a terminal: it and what it pays.
type FeeCoin = (OutPoint, TxOut);

#[derive(Default)]
pub(super) struct Watch {
    spends: Mutex<HashMap<OutPoint, (u32, Spend)>>,
    proving: Arc<Mutex<HashSet<[u8; 32]>>>,
    fee_coins: Mutex<HashMap<([u8; 32], String), FeeCoin>>,
    noted: Mutex<HashSet<([u8; 32], &'static str)>>,
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
    pub(super) async fn play(&self, id: &[u8; 32]) -> Result<Played> {
        let ctx = self.context().await.map_err(|e| anyhow!(e.reason))?;
        let row = self.store().row(id)?.context("no signed opening")?;
        ensure!(!row.abandoned, "an opening given up");
        let hub = Hub::load(&row.state, &ctx)?;
        let o = hub.signed()?;
        let tip = ctx.tip;
        let funding = &o.funding;
        let contract = OutPoint {
            txid: funding.compute_txid(),
            vout: 0,
        };
        let from = o.base.parts().chain.checkpoint.height();
        // The cheap check first: her contract stands.
        if self.unspent(contract).await? {
            return Ok(Played::Idle);
        }
        let Some((r_at, r)) = self.spend(contract, &funding.output[0], from, tip).await? else {
            return Ok(Played::Idle);
        };
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
            return self.decide(id, &hub, &o, &r, r_at, tip).await;
        };
        let respond = presign(&o.set, "respond")?;
        if x.compute_txid() != respond.tx.compute_txid() {
            return self.collect(id, &hub, &o, &r, None, &x).await;
        }
        let Some((y_at, y)) = self.spend(main(&x), &x.output[0], x_at, tip).await? else {
            return self.assert(id, &hub, &o, &r, r_at, &x, x_at, tip).await;
        };
        let assert = presign(&o.set, "assert")?;
        if y.compute_txid() != assert.tx.compute_txid() {
            return self.collect(id, &hub, &o, &r, Some(&x), &y).await;
        }
        let Some((z_at, z)) = self.spend(main(&y), &y.output[0], y_at, tip).await? else {
            return Ok(Played::Idle);
        };
        let challenge = presign(&o.set, "challenge_assert")?;
        if z.compute_txid() != challenge.tx.compute_txid() {
            return self.collect(id, &hub, &o, &r, Some(&x), &z).await;
        }
        match self.spend(main(&z), &z.output[0], z_at, tip).await? {
            None => self.answer(id, &hub, &o, &y, &z).await,
            Some((_, end)) => self.collect(id, &hub, &o, &r, Some(&x), &end).await,
        }
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
                let tx = o.reclaim(hub.keys())?;
                if self.post(&tx).await {
                    return Ok(Played::Posted("reclaim"));
                }
            }
            return Ok(Played::Idle);
        };
        let request = presign(&o.set, "request")?;
        if p.input.iter().all(|i| i.previous_output != slot)
            || p.compute_txid() == o.reclaim(hub.keys())?.compute_txid()
        {
            return Ok(Played::Idle);
        }
        let depth = o.base.parts().chain.funding_depth.max(1);
        if tip.saturating_add(1) < p_at.saturating_add(depth) {
            return Ok(Played::Idle);
        }
        let block = self.block_at(p_at).await?;
        let filed = dispute::published(&request.tx, &block, &p.compute_txid())?;
        let (b, e) = o.requested_b(r)?;
        let headers = self.headers(o, p_at).await?;
        let chain = licash_hp::chain(&o.base, &headers).map_err(|e| anyhow!("{e}"))?;
        let evidence = self.evidence(o, &filed)?;
        let owed = licash::recovery::owed(
            &o.base,
            &licash::request::Request {
                amount: b,
                claim_digest: e,
            },
            &filed,
            &evidence,
            &chain,
        )
        .map_err(|e| anyhow!("{e}"))?;
        if owed.owed >= b {
            self.note_once(
                id,
                "owed",
                "her claim is owed all she asks: her timeout pays her",
            );
            return Ok(Played::Idle);
        }
        ensure!(
            owed.disclosures == licash::disclosure::Disclosures::empty(),
            "the evidence needs a disclosure, which this hub does not post yet"
        );
        tracing::warn!(
            id = %crate::hex_id(id),
            claimed = b,
            owed = owed.owed,
            "a contract claim is owed less than it asks: responding"
        );
        let keys = hub.keys();
        let response = Responses::open(&self.data)?.answer(id, r, || o.respond(keys, owed.owed))?;
        self.post(&response).await;
        Ok(Played::Posted("respond"))
    }

    /// The hub responded: its proof that her claim is owed no more, once
    /// the response is deep enough, then its assertion.
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
        let depth = o.base.parts().chain.work_depth;
        if tip < x_at.saturating_add(depth) {
            return Ok(Played::Idle);
        }
        let slot = OutPoint {
            txid: r.compute_txid(),
            vout: SLOT,
        };
        let (p_at, p) = self
            .spend(slot, &r.output[slot.vout as usize], r_at, tip)
            .await?
            .context("responded with no publication")?;
        let (r_block, p_block, x_block) = (
            self.block_at(r_at).await?,
            self.block_at(p_at).await?,
            self.block_at(x_at).await?,
        );
        let request = presign(&o.set, "request")?;
        let filed = dispute::published(&request.tx, &p_block, &p.compute_txid())?;
        let headers = self.headers(o, tip).await?;
        let exit = dispute::exit(
            headers,
            &o.funding,
            (&r_block, &r.compute_txid()),
            (&p_block, &p.compute_txid()),
            (&x_block, &x.compute_txid()),
            None,
        )?;
        let input = licash_hp::HubInput {
            base: o.base.clone(),
            exit,
            evidence: self.evidence(o, &filed)?,
        };
        let journal = graph::journal(&o.base);
        match licash_hp::check(&input) {
            Ok(j) if j == journal => {}
            Ok(_) => bail!("the statement's journal is not the contract's"),
            Err(e) => bail!("the hub's statement does not hold: {e}"),
        }
        let kind = graph::kind(&o.base, o.mock);
        let proof = if o.mock {
            lichen_babe::statement::mock_prove(&journal)
        } else {
            match self.proof(id, &input)? {
                Some(p) => p,
                None => return Ok(Played::Idle),
            }
        };
        ensure!(
            kind.verify(&journal, &proof),
            "the proof does not verify for this contract"
        );
        let tag = tagged(id, b"assert");
        let tx = Responses::open(&self.data)?.answer(&tag, r, || o.assert(hub.keys(), &proof))?;
        self.post(&tx).await;
        Ok(Played::Posted("assert"))
    }

    /// The proof of the hub's statement on `input`, once the prover made
    /// it: `LICASH_HP_PROVER` (default `licash-r0`) `hp-prove`, run in the
    /// background, its files in `<data>/hp-proofs/<id>/`.
    fn proof(
        &self,
        id: &[u8; 32],
        input: &licash_hp::HubInput,
    ) -> Result<Option<ark_groth16::Proof<ark_bn254::Bn254>>> {
        let dir = self.data.join("hp-proofs").join(crate::hex_id(id));
        let out = dir.join("proof.json");
        if out.exists() {
            let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&out)?)?;
            let seal = hex::decode(json["seal"].as_str().context("no seal")?)?;
            return Ok(Some(lichen_babe::risc0::proof_from_seal(&seal)?));
        }
        let mut proving = self
            .watch
            .proving
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !proving.insert(*id) {
            return Ok(None);
        }
        drop(proving);
        std::fs::create_dir_all(&dir)?;
        let input_path = dir.join("input.bin");
        std::fs::write(&input_path, input.to_bytes())?;
        let prover = std::env::var("LICASH_HP_PROVER").unwrap_or_else(|_| "licash-r0".into());
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("prover.log"))?;
        let tmp = dir.join("proof.json.tmp");
        let mut cmd = std::process::Command::new(&prover);
        cmd.arg("hp-prove")
            .arg(&input_path)
            .arg(&tmp)
            .stdout(log.try_clone()?)
            .stderr(log);
        let proving = self.watch.proving.clone();
        let (id, hex) = (*id, crate::hex_id(id));
        tracing::warn!(id = %hex, %prover, "proving the hub's statement");
        std::thread::spawn(move || {
            let ok = cmd.status().is_ok_and(|s| s.success()) && std::fs::rename(&tmp, &out).is_ok();
            if !ok {
                tracing::error!(id = %hex, "the prover failed (see its prover.log); trying again");
            }
            proving
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
        });
        Ok(None)
    }

    /// She challenged the assertion: answered with the first kept
    /// instance whose circuit opens on the proof.
    async fn answer(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        asserted: &Transaction,
        challenge: &Transaction,
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
                    let tx = self.with_fee(id, hub, o, &name, tx).await?;
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

    /// The terminal `tx` named `name` with an exact fee coin of the hub's
    /// wallet appended: made once and kept for the poll after.
    async fn with_fee(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        name: &str,
        tx: Transaction,
    ) -> Result<Transaction> {
        let key = &hub.keys().party.payment;
        let script = dispute::fee_script(key)?;
        let prevout = presign(&o.set, name)?.prevout.clone();
        let cached = self
            .watch
            .fee_coins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(*id, name.to_string()))
            .cloned();
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
                let network = self.source.network().await?;
                let address = bitcoin::Address::from_script(&script, network)?;
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
                    .position(|o| o.script_pubkey == script)
                    .context("no fee coin output")?;
                let coin = (
                    OutPoint {
                        txid: raw.compute_txid(),
                        vout: vout as u32,
                    },
                    raw.output[vout].clone(),
                );
                self.watch
                    .fee_coins
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert((*id, name.to_string()), coin.clone());
                coin
            }
        };
        dispute::add_fee(tx, prevout, coin, key)
    }

    /// A payout ended the exit: the hub's outputs of it, swept to its
    /// wallet once their waits are over.
    async fn collect(
        &self,
        id: &[u8; 32],
        hub: &Hub,
        o: &HubOpening,
        r: &Transaction,
        response: Option<&Transaction>,
        payout: &Transaction,
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
            return Ok(Played::Idle);
        }
        let address: String = self
            .wallet
            .rpc("getnewaddress", serde_json::json!(["", "bech32m"]))
            .await?;
        let to = address
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()?
            .assume_checked()
            .script_pubkey();
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

    fn evidence(&self, o: &HubOpening, filed: &[u8]) -> Result<Vec<Evidence>> {
        match self.evidencer.get() {
            Some(f) => f(o, filed),
            None => Ok(Vec::new()),
        }
    }

    async fn unspent(&self, coin: OutPoint) -> Result<bool> {
        let out: Option<serde_json::Value> = self
            .wallet
            .rpc_optional(
                "gettxout",
                serde_json::json!([coin.txid.to_string(), coin.vout, true]),
            )
            .await?;
        Ok(out.is_some())
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

    /// The headers after `o`'s checkpoint through block `to`.
    async fn headers(&self, o: &HubOpening, to: u32) -> Result<Vec<[u8; 80]>> {
        let parts = o.base.parts();
        let from = parts.chain.checkpoint.height().saturating_add(1);
        let to = to.min(
            from.saturating_add(parts.chain.max_headers)
                .saturating_sub(1),
        );
        let mut headers = Vec::new();
        for h in from..=to {
            let hash = self.source.hash(h).await?;
            let header = self.source.header(&hash).await?;
            let bytes = bitcoin::consensus::serialize(&header);
            headers.push(
                bytes
                    .try_into()
                    .map_err(|_| anyhow!("a header of 80 bytes"))?,
            );
        }
        Ok(headers)
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
        let mut noted = self
            .watch
            .noted
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if noted.insert((*id, what)) {
            tracing::info!(id = %crate::hex_id(id), "{message}");
        }
    }
}

fn tagged(id: &[u8; 32], tag: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"licash/contracts/hub-pin");
    h.update(id);
    h.update(tag);
    h.finalize().into()
}

/// The output of her request her publication spends.
const SLOT: u32 = 2;
