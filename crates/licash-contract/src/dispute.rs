//! Pure transaction builders and readers for both sides of a hub-proves
//! dispute. Chain observation, durable intent, fee selection and broadcast
//! remain the driver's responsibility.
use anyhow::{ensure, Context, Result};
use ark_bn254::Bn254;
use ark_groth16::Proof;
use lichen_babe::{
    gadget,
    hp::{link_args, Assertion, HubProves},
    opening::{self, Garbler},
    reveal,
    setup::Instance,
    statement,
};
use lichen_btc::{
    bitcoin::{
        absolute::LockTime, key::Keypair, Amount, Block, FeeRate, OutPoint, ScriptBuf, Sequence,
        Transaction, TxIn, TxOut, Txid,
    },
    sighash::{sign_tapscript, sign_tapscript_acp},
    taptree::TapTree,
    tx::{build_spend, build_tx, Timelock},
    winternitz::WotsPublic,
    witness::tapscript_witness,
};
use lichen_contract::{
    skeleton::{anchor_tree_for, Owned, Presign},
    Role,
};
use std::collections::HashMap;

use crate::{
    graph,
    keys::{HolderKeys, HubKeys},
    opening::{HubOpening, Opening},
};

pub fn presign<'a>(set: &'a [Presign], name: &str) -> Result<&'a Presign> {
    set.iter()
        .find(|p| p.name == name)
        .with_context(|| format!("no {name} transaction"))
}
fn check_link(set: &[Presign], name: &str, tx: &Transaction) -> Result<()> {
    let p = presign(set, name)?;
    ensure!(
        !tx.input.is_empty() && tx.compute_txid() == p.tx.compute_txid(),
        "not this contract's {name}"
    );
    Ok(())
}
fn finish(
    set: &[Presign],
    hub: &HashMap<String, Vec<u8>>,
    holder: &HashMap<String, Vec<u8>>,
    name: &str,
    wire: Vec<Vec<u8>>,
) -> Result<Transaction> {
    Ok(presign(set, name)?.finish(
        wire,
        hub.get(name).context("missing hub signature")?,
        holder.get(name).context("missing holder signature")?,
    ))
}
fn carrier_outputs(
    mut tx: Transaction,
    key: &Keypair,
    tree: &TapTree,
    leaf_name: &str,
    prevout: &TxOut,
    outputs: Vec<TxOut>,
) -> Result<Transaction> {
    if outputs.is_empty() {
        return Ok(tx);
    }
    tx.output.extend(outputs);
    let leaf = tree.leaf(leaf_name)?;
    let signature = sign_tapscript_acp(key, &tx, 0, prevout, &leaf.script)?;
    tx.input[0].witness =
        tapscript_witness(&[signature], &leaf.script, &tree.control_block(leaf_name)?);
    Ok(tx)
}
impl Opening {
    pub fn finish(&self, name: &str, wire: Vec<Vec<u8>>) -> Result<Transaction> {
        finish(&self.set, &self.offer.sigs, &self.consent.sigs, name, wire)
    }
    pub fn request(&self, keys: &HolderKeys, b: u32, e: &[u8; 32]) -> Result<Transaction> {
        ensure!(keys.public() == self.holder, "another holder's keys");
        ensure!(
            b <= self.base.parts().cap,
            "claim exceeds contract capacity"
        );
        self.finish(
            "request",
            HubProves::request_wire(&keys.b.sign(&b.to_be_bytes())?, &keys.e.sign(e)?),
        )
    }
    /// Publish the claim with optional fee coins and change outputs, all
    /// fixed before its claim-key signature is made. Fee inputs may follow.
    pub fn publication(
        &self,
        keys: &HolderKeys,
        bytes: &[u8],
        outputs: Vec<TxOut>,
    ) -> Result<Transaction> {
        ensure!(
            keys.claim_key().serialize() == self.base.parts().claim_key,
            "another claim key"
        );
        ensure!(
            bytes.len() <= self.base.parts().limits.max_publication_bytes as usize,
            "publication exceeds limit"
        );
        let request = presign(&self.set, "request")?;
        let slot = OutPoint {
            txid: request.tx.compute_txid(),
            vout: lichen_statement::SLOT_VOUT,
        };
        let prevout = &request.tx.output[slot.vout as usize];
        let tx = self.babe.publication(&keys.claim, slot, prevout, bytes)?;
        carrier_outputs(
            tx,
            &keys.claim,
            &self.babe.slot()?,
            "publish",
            prevout,
            outputs,
        )
    }
    pub fn assertion(&self, tx: &Transaction) -> Result<Assertion> {
        check_link(&self.set, "assert", tx)?;
        self.babe.assertion(tx)
    }
    pub fn verify_assertion(&self, tx: &Transaction) -> Result<bool> {
        let a = self.assertion(tx)?;
        Ok(a.proof.as_ref().is_some_and(|p| {
            graph::kind(&self.base, self.mock).verify(&graph::journal(&self.base), p)
        }))
    }
    pub fn challenge(&self, keys: &HolderKeys, asserted: &Transaction) -> Result<Transaction> {
        ensure!(keys.public() == self.holder, "another holder's keys");
        let a = self.assertion(asserted)?;
        let r = &self.base.parts().recipe;
        let garbler = Garbler::new(
            &keys.garbler,
            r.instances as u32,
            r.kept as u32,
            &graph::kind(&self.base, self.mock),
            &graph::journal(&self.base),
        );
        let secrets = garbler.reveal(&a.digits);
        self.finish(
            "challenge_assert",
            a.challenge_wire(&self.offer.presigs, &secrets),
        )
    }
}
impl HubOpening {
    pub fn finish(&self, name: &str, wire: Vec<Vec<u8>>) -> Result<Transaction> {
        finish(&self.set, &self.sigs, &self.consent.sigs, name, wire)
    }
    pub fn requested_b(&self, request: &Transaction) -> Result<(u32, [u8; 32])> {
        check_link(&self.set, "request", request)?;
        HubProves::requested_b(request)
    }
    pub fn respond(&self, keys: &HubKeys, b_prime: u32) -> Result<Transaction> {
        ensure!(keys.public() == self.hub, "another hub's keys");
        ensure!(
            b_prime <= self.base.parts().cap,
            "response exceeds capacity"
        );
        self.finish(
            "respond",
            HubProves::respond_wire(&keys.b.sign(&b_prime.to_be_bytes())?),
        )
    }
    pub fn disclosure(
        &self,
        keys: &HubKeys,
        bytes: &[u8],
        outputs: Vec<TxOut>,
    ) -> Result<Transaction> {
        ensure!(keys.public() == self.hub, "another hub's keys");
        let respond = presign(&self.set, "respond")?;
        let slot = OutPoint {
            txid: respond.tx.compute_txid(),
            vout: lichen_statement::SLOT_VOUT,
        };
        let prevout = &respond.tx.output[slot.vout as usize];
        let tx = self
            .babe
            .disclosure(&keys.party.payment, slot, prevout, bytes)?;
        carrier_outputs(
            tx,
            &keys.party.payment,
            &self.babe.disclose()?,
            "disclose",
            prevout,
            outputs,
        )
    }
    pub fn assert(&self, keys: &HubKeys, proof: &Proof<Bn254>) -> Result<Transaction> {
        ensure!(keys.public() == self.hub, "another hub's keys");
        let digits = lichen_babe::setup::digits(&gadget::input(&proof.a));
        let (pi2, pi3) = statement::pi23_to_bytes(proof);
        self.finish(
            "assert",
            HubProves::assert_wire(&keys.lamport.sign(&digits), &pi2, &pi3),
        )
    }
    /// Answer with a committed kept instance. Its index in `kept` selects
    /// the graph's wrongly_challenged leaf, not the instance's number.
    pub fn answer(
        &self,
        asserted: &Transaction,
        challenge: &Transaction,
        i: u32,
        bytes: &[u8],
    ) -> Result<Option<Transaction>> {
        let k = self
            .kept
            .iter()
            .position(|n| *n == i)
            .context("instance not kept")?;
        let Some(inst) = Instance::from_bytes(bytes) else {
            return Ok(None);
        };
        if inst.hmsg != self.babe.spec.hmsg[k] {
            return Ok(None);
        }
        check_link(&self.set, "assert", asserted)?;
        check_link(&self.set, "challenge_assert", challenge)?;
        let a = self.babe.assertion(asserted)?;
        ensure!(!challenge.input.is_empty(), "challenge has no input");
        let args = link_args(challenge);
        let (digits, secrets) = reveal::extract(&self.hub.lamport, &self.offer.presigs, &args)
            .context("challenge does not reveal secrets")?;
        ensure!(digits == a.digits, "challenge of another assertion");
        let Some(proof) = &a.proof else {
            return Ok(None);
        };
        let msg = opening::msg(&inst, i, &self.opened.opened, &a.bits, &secrets, proof);
        msg.map(|m| {
            self.finish(
                &format!("wrongly_challenged_{k}"),
                HubProves::wrongly_wire(&m),
            )
        })
        .transpose()
    }
    pub fn reclaim(&self, keys: &HubKeys) -> Result<Transaction> {
        ensure!(keys.public() == self.hub, "another hub's keys");
        let r = presign(&self.set, "request")?;
        let slot = self.babe.slot()?;
        let prevout = &r.tx.output[lichen_statement::SLOT_VOUT as usize];
        let op = OutPoint {
            txid: r.tx.compute_txid(),
            vout: lichen_statement::SLOT_VOUT,
        };
        let mut tx = build_spend(
            op,
            &Timelock::csv(self.babe.spec.windows.k),
            vec![TxOut {
                value: prevout.value,
                script_pubkey: self.keys_payout()?,
            }],
        );
        let leaf = slot.leaf("reclaim")?;
        let sig = sign_tapscript_acp(&keys.party.payment, &tx, 0, prevout, &leaf.script)?;
        tx.input[0].witness =
            tapscript_witness(&[sig], &leaf.script, &slot.control_block("reclaim")?);
        Ok(tx)
    }
    fn keys_payout(&self) -> Result<ScriptBuf> {
        Ok(graph::party_pub(&self.base.parts().parties[1])?.payout_spk)
    }
}

/// An exact fee coin is appended to a terminal (ALL|ANYONECANPAY).
pub fn add_fee(
    mut tx: Transaction,
    prevout: TxOut,
    fee: (OutPoint, TxOut),
    key: &Keypair,
) -> Result<Transaction> {
    ensure!(
        tx.input.len() == 1,
        "fee coin only for a one-input terminal"
    );
    tx.input.push(TxIn {
        previous_output: fee.0,
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        ..Default::default()
    });
    sign_fee_input(key, &mut tx, 1, &[prevout, fee.1])?;
    Ok(tx)
}
pub fn fee_script(key: &Keypair) -> Result<ScriptBuf> {
    Ok(anchor_tree_for(&key.x_only_public_key().0)?.script_pubkey())
}
pub fn sign_fee_input(
    key: &Keypair,
    tx: &mut Transaction,
    i: usize,
    prevouts: &[TxOut],
) -> Result<()> {
    ensure!(
        prevouts.len() == tx.input.len() && i < prevouts.len(),
        "fee prevout count"
    );
    let tree = anchor_tree_for(&key.x_only_public_key().0)?;
    ensure!(
        prevouts[i].script_pubkey == tree.script_pubkey(),
        "another fee key"
    );
    let leaf = tree.leaf("fee")?;
    let sig = sign_tapscript(key, tx, i, prevouts, &leaf.script)?;
    tx.input[i].witness = tapscript_witness(
        &[sig.as_ref().to_vec()],
        &leaf.script,
        &tree.control_block("fee")?,
    );
    Ok(())
}
/// A payout's value and the on-chain signature its bit leaves read.
pub struct Payout<'a> {
    pub transaction: &'a Transaction,
    pub value_key: &'a WotsPublic,
    pub signer: Role,
    pub amount: u32,
    pub wire: &'a [Vec<u8>],
}

/// Collect the graph-owned bits and reserve for either party.
pub fn sweep(
    babe: &HubProves,
    key: &Keypair,
    payout: Payout<'_>,
    party: Role,
    to: ScriptBuf,
    rate: FeeRate,
) -> Result<Transaction> {
    let owned = babe.core().owned(
        payout.transaction,
        payout.value_key,
        payout.signer,
        payout.amount,
        party,
    )?;
    sweep_owned(&owned, key, payout.wire, to, rate)
}
fn sweep_owned(
    owned: &[Owned],
    key: &Keypair,
    wire: &[Vec<u8>],
    to: ScriptBuf,
    rate: FeeRate,
) -> Result<Transaction> {
    ensure!(!owned.is_empty(), "no owned outputs");
    let prevouts: Vec<_> = owned.iter().map(|o| o.out.clone()).collect();
    let total = prevouts.iter().try_fold(Amount::ZERO, |s, o| {
        s.checked_add(o.value).context("sweep overflow")
    })?;
    let mut tx = build_tx(
        &owned.iter().map(|o| (o.op, o.sequence)).collect::<Vec<_>>(),
        vec![TxOut {
            value: total,
            script_pubkey: to,
        }],
        LockTime::ZERO,
    );
    for (i, o) in owned.iter().enumerate() {
        tx.input[i].witness = o.witness(&tx, i, &prevouts, key, wire)?;
    }
    let fee = rate
        .fee_vb(tx.vsize() as u64)
        .context("sweep fee overflow")?;
    tx.output[0].value = total.checked_sub(fee).context("sweep cannot pay fee")?;
    ensure!(
        tx.output[0].value >= tx.output[0].script_pubkey.minimal_non_dust(),
        "dust sweep"
    );
    for (i, o) in owned.iter().enumerate() {
        tx.input[i].witness = o.witness(&tx, i, &prevouts, key, wire)?;
    }
    Ok(tx)
}

pub fn exit(
    headers: Vec<[u8; 80]>,
    funding: &Transaction,
    request: (&Block, &Txid),
    publish: (&Block, &Txid),
    respond: (&Block, &Txid),
    disclosure: Option<(&Block, &Txid)>,
) -> Result<lichen_statement::Exit> {
    Ok(lichen_statement::Exit {
        headers,
        request: lichen_babe::exit::request(funding, request.0, request.1)?,
        publish: lichen_babe::exit::slot_spend(publish.0, publish.1)?,
        respond: lichen_babe::exit::respond(respond.0, respond.1)?,
        disclosure: disclosure
            .map(|(b, t)| lichen_babe::exit::slot_spend(b, t))
            .transpose()?,
    })
}

/// The claim bytes `publication` carries in the request's slot.
pub fn published(request: &Transaction, publication: &Block, txid: &Txid) -> Result<Vec<u8>> {
    let spend = lichen_babe::exit::slot_spend(publication, txid)?;
    let slot = lichen_statement::btc::OutPoint {
        txid: lichen_btc::bitcoin::hashes::Hash::to_byte_array(request.compute_txid()),
        vout: lichen_statement::SLOT_VOUT,
    };
    spend
        .carrying(&slot)
        .context("the publication carries no claim in the request's slot")
}
