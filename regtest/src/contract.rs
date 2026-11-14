//! A BABE exit contract on regtest, as both parties build and play it
//! (after `lichen/crates/babe/tests/graph.rs`), with the hub's `respond`
//! keys for licash's statement: script authenticates the entire counter
//! proof and its prior-claim digest (160 bytes). The garbled
//! instances are real (cut and choose of 4, 2 kept), encrypted under the
//! statement the test supplies, so that a proof opens a kept instance's
//! hashlock and nothing else does. A Corrected contract has a second
//! assertion (`assert_corrected`, journal `c = 1`) with its own keys and
//! setup ([`Stage`]). `native` opens it from licash's Base, both parties
//! through `licash_contract::opening`.

use std::collections::HashMap;

use crate::graph::{Babe, Corrected, ExitKeys};
use anyhow::{anyhow, Context, Result};
use ark_bn254::Bn254;
use ark_groth16::Proof;
use bitcoincore_rpc::json::{AddressType, SignRawTransactionInput};
use bitcoincore_rpc::RpcApi;
use lichen_babe::garble::evaluate;
use lichen_babe::reveal::{complete, extract, reveal_args, AdaptorKeys, LamportKey, LamportPub};
use lichen_babe::vsss::{interpolate, Dealer, Dealing};
use lichen_babe::we::decrypt;
use lngap_btc::bitcoin::{Amount, OutPoint, Transaction, TxIn};
use lngap_btc::keys::Seed;
use lngap_btc::regtest::Regtest;
use lngap_btc::sighash::sign_tapscript;
use lngap_btc::tx::build_tx;
use lngap_btc::witness::{tapscript_witness, witness_args_consumption_order};
use lngap_channel::PartyKeys;
use lngap_lamport::winternitz::{
    Wots256Secret, Wots256Sig, WotsParams, WotsPublic, WotsSecret, WotsSig,
};
use lngap_sessions::session::bit_tree;
use lngap_sessions::skeleton::{Presign, Signed};

use licash_contract::keys::{HolderKeys, HubKeys};
pub use licash_contract::opening::{OpenedShares, Setup, Statement};
pub use licash_contract::{INSTANCES, KEPT};

/// Both parties' secrets for one contract. One struct, as a test holds
/// both sides; which side uses what is noted on each field.
pub struct Parties {
    pub alice: PartyKeys,
    pub hub: PartyKeys,
    /// Alice's one-time keys: `b`, `E`, π1's Lamport key, the adaptors.
    pub b_key: WotsSecret,
    pub e_key: WotsSecret,
    pub lamport: LamportKey,
    pub adaptor: AdaptorKeys,
    /// The hub's one-time keys for `respond`: `b`, `E` in each leaf, `K`,
    /// and what a mask pays (`b_corr`, a Corrected contract's).
    pub hub_b: Wots256Secret,
    pub hub_e_plain: Wots256Secret,
    pub hub_e_counter: Wots256Secret,
    pub hub_k: Wots256Secret,
    pub hub_b_corr: Wots256Secret,
    pub disclosure: [Wots256Secret; 5],
    /// The library signer, derived independently from the same entropy.
    pub sealed_keys: licash::statement::SecretKeys,
    /// The hub's dealing of the instances' input secrets.
    pub dealer: Dealer,
    pub dealing: Dealing,
    /// The corrected assertion's set (used by a Corrected contract only).
    pub corrected: Second,
}

/// The corrected assertion's one-time keys: Alice's second `b` (the
/// corrected payout's bits read it), π1's Lamport key and the adaptors for
/// the second statement, and the hub's dealing of its instances. Each is
/// distinct from the first set's: each signs once.
pub struct Second {
    pub b_key: WotsSecret,
    pub lamport: LamportKey,
    pub adaptor: AdaptorKeys,
    pub dealer: Dealer,
    pub dealing: Dealing,
}

/// Both parties' keys of the contract labelled `label`, as each derives
/// its own ([`licash_contract::keys`]); Alice's `b` and `E` of the graph's
/// own request too (a native opening's come from her request seed).
pub fn parties(label: &str) -> Parties {
    let a = Seed::from_label(&format!("{label}/alice"));
    let alice = HolderKeys::from_label(label);
    let hub = HubKeys::from_label(label);
    Parties {
        alice: alice.party,
        hub: hub.party,
        b_key: WotsSecret::from_entropy(WotsParams::for_bytes(4), a.derive_bytes("b")),
        e_key: WotsSecret::from_entropy(WotsParams::for_bytes(32), a.derive_bytes("e")),
        lamport: alice.lamport,
        adaptor: alice.adaptor,
        hub_b: hub.b,
        hub_e_plain: hub.e_plain,
        hub_e_counter: hub.e_counter,
        hub_k: hub.k,
        hub_b_corr: hub.b_corr,
        disclosure: hub.disclosure,
        sealed_keys: hub.sealed,
        dealer: hub.full.dealer,
        dealing: hub.full.dealing,
        corrected: Second {
            b_key: alice.corrected.b_key,
            lamport: alice.corrected.lamport,
            adaptor: alice.corrected.adaptor,
            dealer: hub.corrected.dealer,
            dealing: hub.corrected.dealing,
        },
    }
}

impl Parties {
    /// The hub's `respond` keys as the graph (and the journal) fix them.
    pub fn exit_keys(&self) -> ExitKeys {
        ExitKeys {
            e_key: self.e_key.public(),
            hub_e_plain: self.hub_e_plain.public(),
            hub_e_counter: self.hub_e_counter.public(),
            hub_k: self.hub_k.public(),
            counter_items: 0,
        }
    }

    /// The same keys as the statement reads them.
    pub fn hub_keys(&self) -> licash::statement::HubKeys {
        crate::hub_keys(
            &self.exit_keys(),
            &self.hub_b.public(),
            &self.hub_b_corr.public(),
            self.sealed_keys.public().disclosure,
        )
    }
}

/// Which assertion a dispute step belongs to: the exit's own (`assert`,
/// journal `c = 0`, paying `b`) or a Corrected contract's
/// (`assert_corrected`, `c = 1`, paying the hub's signed `b_corr`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Full,
    Corrected,
}

impl Stage {
    pub fn assert(self) -> &'static str {
        match self {
            Stage::Full => "assert",
            Stage::Corrected => "assert_corrected",
        }
    }

    pub fn challenge(self) -> &'static str {
        match self {
            Stage::Full => "challenge_assert",
            Stage::Corrected => "challenge_corrected",
        }
    }

    /// The terminal against a challenge, for kept instance `k`.
    pub fn wrongly(self, k: usize) -> String {
        match self {
            Stage::Full => format!("wrongly_challenged_{k}"),
            Stage::Corrected => format!("wrongly_challenged_corrected_{k}"),
        }
    }
}

/// A contract funded and pre-signed, Alice's kept instances built from
/// the hub's shares (the opened ones checked), her adaptor pre-signatures
/// checked by the hub.
pub struct Opened {
    pub p: Parties,
    pub babe: Babe,
    /// Public setup-bound coins and keys, retained for exact reconstruction.
    pub publication: crate::graph::Publication,
    pub set: HashMap<String, Presign>,
    pub signed: Signed,
    pub kept: HashMap<String, Vec<u8>>,
    /// The exit's assertion's setup, and a Corrected contract's second.
    pub full: Setup,
    pub corrected: Option<Setup>,
    /// The deposit coin, the journal's `O_D`.
    pub deposit: OutPoint,
}

/// A separate negotiated byte budget, not an assertion that every possible
/// 32-item claim (with arbitrarily large fallback input tables) fits.
pub const REQUEST_ITEMS: usize = 200;
pub const REQUEST_BYTES: usize = REQUEST_ITEMS * 80;

impl Opened {
    fn setup(&self, stage: Stage) -> Result<&Setup> {
        match stage {
            Stage::Full => Ok(&self.full),
            Stage::Corrected => self.corrected.as_ref().context("not a Corrected contract"),
        }
    }

    fn graph_corrected(&self) -> Result<&Corrected> {
        self.babe
            .spec
            .corrected
            .as_ref()
            .context("not a Corrected contract")
    }

    /// π1's Lamport key of `stage`, as the graph fixes it.
    fn lamport(&self, stage: Stage) -> Result<&LamportPub> {
        match stage {
            Stage::Full => Ok(&self.babe.spec.lamport),
            Stage::Corrected => Ok(&self.graph_corrected()?.lamport),
        }
    }

    /// The `b` key `stage`'s payout bits read.
    fn b_key(&self, stage: Stage) -> Result<&WotsPublic> {
        match stage {
            Stage::Full => Ok(&self.babe.spec.b_key),
            Stage::Corrected => Ok(&self.graph_corrected()?.b_key),
        }
    }

    fn dealer(&self, stage: Stage) -> &Dealer {
        match stage {
            Stage::Full => &self.p.dealer,
            Stage::Corrected => &self.p.corrected.dealer,
        }
    }

    /// The assertion a posted link `tx` belongs to, by its txid (every
    /// witness of a pre-signed link has the same): the stage whose link
    /// `name(stage)` it is.
    fn stage_of(&self, tx: &Transaction, name: fn(Stage) -> &'static str) -> Result<Stage> {
        [Stage::Full, Stage::Corrected]
            .into_iter()
            .find(|s| {
                self.set
                    .get(name(*s))
                    .is_some_and(|p| p.tx.compute_txid() == tx.compute_txid())
            })
            .context("not a link of either assertion")
    }

    fn sigs(&self, name: &str) -> (&[u8], &[u8]) {
        (
            &self.signed.hub[name],
            self.signed.alice.get(name).or(self.kept.get(name)).unwrap(),
        )
    }

    /// Link `name` with its wire and both pre-signatures: built, not posted.
    pub fn build(&self, name: &str, wire: Vec<Vec<u8>>) -> Transaction {
        self.build_publication(name, wire, None)
            .expect("fixture publication wire")
    }

    fn build_publication(
        &self,
        name: &str,
        wire: Vec<Vec<u8>>,
        publication: Option<&licash::disclosure::SignedDisclosures>,
    ) -> Result<Transaction> {
        let p = &self.set[name];
        if p.extra.is_empty() {
            let (h, a) = self.sigs(name);
            return Ok(p.finish(wire, h, a));
        }
        anyhow::ensure!(
            name == "respond" || name == "respond_counter",
            "publication only in responses"
        );
        let mut wires = vec![wire.clone()];
        if name == "respond" {
            // Both variants use the graph's exact tied E encoding. Remove
            // only the main input's b authentication from its bottom-first wire.
            let n = 4 * WotsParams::for_bytes(4).total_digits() as usize;
            let e_wire = wire[..wire.len() - n].to_vec();
            wires.extend((0..5).map(|_| e_wire.clone()));
        } else {
            let values = publication
                .map(|p| p.disclosures.clone())
                .unwrap_or_else(licash::disclosure::Disclosures::empty);
            let bytes = values.to_bytes();
            let mut offset = 0;
            for i in 0..5 {
                let end = offset + licash::disclosure::CHUNK_BYTES[i];
                let key = &self.p.disclosure[i];
                let sig = match publication {
                    Some(p) => Wots256Sig::from_hashes(
                        key.public().params,
                        &bytes[offset..end],
                        p.signatures[i].0.clone(),
                    )?,
                    None => key.sign(&bytes[offset..end])?,
                };
                wires.push(sig.consumption_order().into_iter().rev().collect());
                offset = end;
            }
        }
        let hub = (0..p.inputs())
            .map(|i| self.signed.hub[&lngap_sessions::skeleton::sig_name(name, i)].clone())
            .collect::<Vec<_>>();
        let alice = (0..p.inputs())
            .map(|i| self.signed.alice[&lngap_sessions::skeleton::sig_name(name, i)].clone())
            .collect::<Vec<_>>();
        Ok(p.finish_multi(wires, &hub, &alice))
    }

    /// The Hub's signatures on `values` under its five one-time publication
    /// keys: what `respond_counter` must carry to publish them.
    pub fn sign_disclosures(
        &self,
        values: &licash::disclosure::Disclosures,
    ) -> Result<licash::disclosure::SignedDisclosures> {
        let bytes = values.to_bytes();
        let mut offset = 0;
        let mut signatures = vec![];
        for (i, key) in self.p.disclosure.iter().enumerate() {
            let end = offset + licash::disclosure::CHUNK_BYTES[i];
            signatures.push(crate::sig(&key.sign(&bytes[offset..end])?));
            offset = end;
        }
        Ok(licash::disclosure::SignedDisclosures {
            disclosures: values.clone(),
            signatures: signatures.try_into().unwrap(),
        })
    }

    /// Reconstruct available bytes and native signatures from all five posted
    /// publication inputs. Plain/mixed witnesses require the Full claim route.
    pub fn publication(&self, rt: &Regtest) -> Result<licash::disclosure::SignedDisclosures> {
        let tx = rt.get_tx(&self.txid("respond_counter"))?;
        let mut bytes = vec![];
        let mut signatures = vec![];
        for i in 0..5 {
            let witness = &tx.input[i + 1].witness;
            let n = 2 * self.p.disclosure[i].public().params.total_digits() as usize;
            anyhow::ensure!(witness.len() == n + 4, "publication witness shape");
            let mut args: Vec<_> = witness.iter().take(n).map(<[u8]>::to_vec).collect();
            args.reverse();
            let signature =
                Wots256Sig::from_consumption_order(self.p.disclosure[i].public().params, &args)?;
            bytes.extend(self.p.disclosure[i].public().verify(&signature)?);
            signatures.push(crate::sig(&signature));
        }
        Ok(licash::disclosure::SignedDisclosures {
            disclosures: licash::disclosure::Disclosures::from_bytes(&bytes)?,
            signatures: signatures.try_into().unwrap(),
        })
    }

    /// The txid of link `name` (whatever its witness).
    pub fn txid(&self, name: &str) -> lngap_btc::bitcoin::Txid {
        self.set[name].tx.compute_txid()
    }

    /// Post a built transaction, and mine it.
    pub fn post(&self, rt: &Regtest, tx: &Transaction) -> Result<Transaction> {
        let (txid, _) = rt.send_and_confirm(tx)?;
        rt.get_tx(&txid)
    }

    /// Post link `name` with its wire, and mine it.
    pub fn link(&self, rt: &Regtest, name: &str, wire: Vec<Vec<u8>>) -> Result<Transaction> {
        let tx = self.build(name, wire);
        let posted = self.post(rt, &tx).with_context(|| name.to_string())?;
        println!("{name}: {} vB", tx.vsize());
        Ok(posted)
    }

    /// Whether link `name` with its wire would be accepted now (not posted).
    pub fn accepts(
        &self,
        rt: &Regtest,
        name: &str,
        wire: Vec<Vec<u8>>,
    ) -> std::result::Result<u64, String> {
        rt.test_accept(&self.build(name, wire))
    }

    /// Post terminal `name`, a coin of the node's wallet paying its fee.
    pub fn terminal(&self, rt: &Regtest, name: &str, wire: Vec<Vec<u8>>) -> Result<Transaction> {
        let p = &self.set[name];
        let (h, a) = self.sigs(name);
        let mut tx = p.finish(wire, h, a);
        let fee = Amount::from_sat(2 * (tx.vsize() as u64 + 120));
        // a confirmed coin of exactly the fee (TRUC spends no unconfirmed v2)
        let addr = rt
            .rpc
            .get_new_address(None, Some(AddressType::Bech32m))?
            .assume_checked();
        let txid = rt
            .rpc
            .send_to_address(&addr, fee, None, None, None, None, None, None)?;
        rt.mine(1)?;
        let t = rt.get_tx(&txid)?;
        let v = t
            .output
            .iter()
            .position(|o| o.script_pubkey == addr.script_pubkey())
            .ok_or_else(|| anyhow!("the fee coin"))?;
        let (op, coin) = (
            OutPoint {
                txid,
                vout: v as u32,
            },
            t.output[v].clone(),
        );
        tx.input.push(TxIn {
            previous_output: op,
            ..Default::default()
        });
        let utxos: Vec<SignRawTransactionInput> =
            [(tx.input[0].previous_output, &p.prevout), (op, &coin)]
                .iter()
                .map(|(op, o)| SignRawTransactionInput {
                    txid: op.txid,
                    vout: op.vout,
                    script_pub_key: o.script_pubkey.clone(),
                    redeem_script: None,
                    amount: Some(o.value),
                })
                .collect();
        let tx = rt
            .rpc
            .sign_raw_transaction_with_wallet(&tx, Some(&utxos), None)?
            .transaction()?;
        let (txid, _) = rt.send_and_confirm(&tx).with_context(|| name.to_string())?;
        println!("{name}: {} vB with its fee coin", tx.vsize());
        rt.get_tx(&txid)
    }

    /// A link's script arguments, in consumption order.
    pub fn args(tx: &Transaction) -> Vec<Vec<u8>> {
        witness_args_consumption_order(&tx.input[0].witness)[2..].to_vec()
    }

    /// Alice's `request` of `b` with her claim digest `e`, `posted` beside
    /// them: built, not posted. Every witness has the same txid.
    pub fn request_tx(&self, b: u32, e: &[u8; 32], posted: &[u8]) -> Result<Transaction> {
        anyhow::ensure!(
            posted.len() <= REQUEST_BYTES,
            "claim exceeds request publication byte budget"
        );
        let wire = self.babe.request_exit_wire_for(
            &self.p.b_key.sign(&b.to_be_bytes())?,
            &self.p.e_key.sign(e)?,
            posted,
        )?;
        Ok(self.build("request", wire))
    }

    /// What a request carries, as the hub reads it: Alice's signatures on
    /// `b` and `E`, and the posted claim.
    pub fn read_request(req: &Transaction) -> Result<(WotsSig, WotsSig, Vec<u8>)> {
        Babe::read_request_exit_with_items(&Self::args(req), REQUEST_ITEMS)
    }

    /// Alice's `request` of `b` with her claim digest `e`, `posted` beside
    /// them; her signatures as the hub reads them back.
    pub fn request(
        &self,
        rt: &Regtest,
        b: u32,
        e: &[u8; 32],
        posted: &[u8],
    ) -> Result<(WotsSig, WotsSig)> {
        let req = self
            .post(rt, &self.request_tx(b, e, posted)?)
            .context("request")?;
        let (alice_b, alice_e, read) = Self::read_request(&req)?;
        anyhow::ensure!(
            (alice_b.message(), alice_e.message(), &read[..])
                == (b.to_be_bytes().to_vec(), e.to_vec(), posted),
            "the request reads back"
        );
        Ok((alice_b, alice_e))
    }

    /// The hub's signature on `b_corr` a corrected `respond_counter`
    /// carries (what `assert_corrected` repeats).
    pub fn responded_corrected(respond: &Transaction) -> Result<Wots256Sig> {
        let k = WotsParams::for_bytes(licash::statement::K_LEN as u32);
        Ok(Babe::read_respond_corrected(&Self::args(respond), k)?.1)
    }

    /// Alice's signature on `b_corr` in a posted `assert_corrected`: what
    /// the corrected payout's bits read.
    pub fn asserted_b_corr(asserted: &Transaction) -> Result<WotsSig> {
        Ok(Babe::read_assert_corrected(&Self::args(asserted))?.1)
    }

    /// Spend `stage`'s payout's outputs of bits `bits` by their leaf `kind`
    /// (`alice_bit`, `hub_bit`, or `equiv_b`), `wire` below the spender's
    /// signature, to the node's wallet. Built, not posted.
    pub fn bit_spend(
        &self,
        rt: &Regtest,
        payout: &Transaction,
        stage: Stage,
        kind: &str,
        bits: &[u32],
        wire: &[Vec<u8>],
    ) -> Result<Transaction> {
        use lngap_btc::bitcoin::TxOut;
        let ctx = self.babe.core().ctx();
        let key = if kind == "alice_bit" {
            &self.p.alice.payment
        } else {
            &self.p.hub.payment
        };
        let b_key = self.b_key(stage)?;
        let mut ins = vec![];
        for &i in bits {
            let tree = bit_tree(&ctx, i, b_key)?;
            let vout = payout
                .output
                .iter()
                .position(|o| o.script_pubkey == tree.script_pubkey())
                .with_context(|| format!("the payout has no output for bit {i}"))?;
            let op = OutPoint {
                txid: payout.compute_txid(),
                vout: vout as u32,
            };
            ins.push((op, payout.output[vout].clone(), tree, format!("{kind}_{i}")));
        }
        let prevouts: Vec<TxOut> = ins.iter().map(|x| x.1.clone()).collect();
        let total: Amount = prevouts.iter().map(|o| o.value).sum();
        let to = rt
            .rpc
            .get_new_address(None, Some(AddressType::Bech32m))?
            .assume_checked()
            .script_pubkey();
        let build = |fee: Amount| -> Result<Transaction> {
            let seqs = ins
                .iter()
                .map(|(op, _, tree, name)| Ok((*op, tree.leaf(name)?.timelock.sequence())))
                .collect::<Result<Vec<_>>>()?;
            let out = TxOut {
                value: total
                    .checked_sub(fee)
                    .context("the bits can't pay the fee")?,
                script_pubkey: to.clone(),
            };
            let mut tx = build_tx(
                &seqs,
                vec![out],
                lngap_btc::bitcoin::absolute::LockTime::ZERO,
            );
            for (n, (_, _, tree, name)) in ins.iter().enumerate() {
                let leaf = tree.leaf(name)?;
                let mut w = wire.to_vec();
                w.push(
                    sign_tapscript(key, &tx, n, &prevouts, &leaf.script)?
                        .as_ref()
                        .to_vec(),
                );
                tx.input[n].witness =
                    tapscript_witness(&w, &leaf.script, &tree.control_block(name)?);
            }
            Ok(tx)
        };
        let vsize = build(Amount::ZERO)?.vsize() as u64;
        build(Amount::from_sat(2 * (vsize + 10)))
    }

    /// The hub's signatures on Alice's `b` and `E` for the plain leaf.
    pub fn respond_plain(
        &self,
        rt: &Regtest,
        alice_b: &WotsSig,
        alice_e: &WotsSig,
    ) -> Result<(Wots256Sig, Wots256Sig)> {
        let hub_b = self.p.hub_b.sign(&alice_b.message())?;
        let hub_e = self.p.hub_e_plain.sign(&alice_e.message())?;
        let wire = Babe::respond_exit_wire(&hub_b, alice_b, alice_e, &hub_e, None);
        self.link(rt, "respond", wire)?;
        Ok((hub_b, hub_e))
    }

    /// The hub's counter leaf: script authenticates all 160 bytes of K.
    /// Read the signatures back from the posted response.
    pub fn respond_counter(
        &self,
        rt: &Regtest,
        alice_b: &WotsSig,
        alice_e: &WotsSig,
        k: &[u8],
    ) -> Result<(Wots256Sig, Wots256Sig)> {
        anyhow::ensure!(k.len() == licash::statement::K_LEN, "K length");
        let hub_b = self.p.hub_b.sign(&alice_b.message())?;
        let hub_e = self.p.hub_e_counter.sign(&alice_e.message())?;
        let k_sig = self.p.hub_k.sign(k)?;
        let wire = Babe::respond_exit_wire(&hub_b, alice_b, alice_e, &hub_e, Some(&k_sig));
        let tx = self.link(rt, "respond_counter", wire)?;
        let (r, _) = Babe::read_respond_counter(
            &Self::args(&tx),
            WotsParams::for_bytes(licash::statement::K_LEN as u32),
            0,
        )?;
        anyhow::ensure!(
            r.k.as_ref().map(|k| k.message()) == Some(k.to_vec()),
            "K reads back"
        );
        anyhow::ensure!(
            r.e == alice_e.message() && r.hub_e == hub_e.hashes,
            "E reads back"
        );
        Ok((hub_e, r.k.context("counter signature")?))
    }

    /// The hub's corrected counter leaf, as a hostile hub may sign it: `K`
    /// and `b_corr` of its choice. The signatures read back from the posted
    /// response: on `E`, on `K`, on `b_corr`.
    pub fn respond_masked(
        &self,
        rt: &Regtest,
        alice_b: &WotsSig,
        alice_e: &WotsSig,
        k: &[u8],
        b_corr: u32,
    ) -> Result<(Wots256Sig, Wots256Sig, Wots256Sig)> {
        let nothing = licash::disclosure::Disclosures::empty();
        self.respond_masked_publishing(rt, alice_b, alice_e, k, b_corr, &nothing)
    }

    /// `respond_masked` publishing `values` in the five disclosure chunks
    /// (the secrets a counter must release, or whatever a hostile hub posts).
    pub fn respond_masked_publishing(
        &self,
        rt: &Regtest,
        alice_b: &WotsSig,
        alice_e: &WotsSig,
        k: &[u8],
        b_corr: u32,
        values: &licash::disclosure::Disclosures,
    ) -> Result<(Wots256Sig, Wots256Sig, Wots256Sig)> {
        anyhow::ensure!(k.len() == licash::statement::K_LEN, "K length");
        let hub_b = self.p.hub_b.sign(&alice_b.message())?;
        let hub_e = self.p.hub_e_counter.sign(&alice_e.message())?;
        let wire = Babe::respond_corrected_wire(
            &hub_b,
            alice_b,
            alice_e,
            &hub_e,
            &self.p.hub_k.sign(k)?,
            &self.p.hub_b_corr.sign(&b_corr.to_be_bytes())?,
        );
        let signed = self.sign_disclosures(values)?;
        let tx = self.build_publication("respond_counter", wire, Some(&signed))?;
        println!("respond_counter: {} vB", tx.vsize());
        let tx = self.post(rt, &tx).context("respond_counter")?;
        let (r, corr) = Babe::read_respond_corrected(
            &Self::args(&tx),
            WotsParams::for_bytes(licash::statement::K_LEN as u32),
        )?;
        anyhow::ensure!(
            r.k.as_ref().map(|k| k.message()) == Some(k.to_vec())
                && corr.message() == b_corr.to_be_bytes(),
            "K and b_corr read back"
        );
        Ok((hub_e, r.k.context("counter signature")?, corr))
    }

    /// Alice asserts `proof`: π1 as Lamport-signed digits, π2 and π3 beside.
    pub fn assert(&self, rt: &Regtest, proof: &Proof<Bn254>) -> Result<Transaction> {
        let (digits, pi2, pi3) = Self::pi(proof);
        self.link(
            rt,
            "assert",
            Babe::assert_wire(&self.p.lamport.sign(&digits), &pi2, &pi3),
        )
    }

    /// A Corrected contract's `assert_corrected` of `proof` (journal
    /// `c = 1`): π1 under Alice's second Lamport key, the hub's signature
    /// `hub` on `b_corr` (from `respond_counter`), and hers on the same.
    pub fn assert_corrected(
        &self,
        rt: &Regtest,
        proof: &Proof<Bn254>,
        hub: &Wots256Sig,
    ) -> Result<Transaction> {
        let b_corr = u32::from_be_bytes(hub.message().try_into().map_err(|_| anyhow!("b_corr"))?);
        self.link(
            rt,
            "assert_corrected",
            self.assert_corrected_wire(proof, hub, b_corr)?,
        )
    }

    /// `assert_corrected`'s witness, Alice signing `mine` as `b_corr`
    /// (anything but the hub's is for tests: no script accepts it).
    pub fn assert_corrected_wire(
        &self,
        proof: &Proof<Bn254>,
        hub: &Wots256Sig,
        mine: u32,
    ) -> Result<Vec<Vec<u8>>> {
        let (digits, pi2, pi3) = Self::pi(proof);
        let c = &self.p.corrected;
        Ok(Babe::assert_corrected_wire(
            &c.lamport.sign(&digits),
            &pi2,
            &pi3,
            hub,
            &c.b_key.sign(&mine.to_be_bytes())?,
        ))
    }

    /// π1's digits, π2, π3 of `proof`.
    fn pi(proof: &Proof<Bn254>) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let bits = lichen_babe::gadget::input(&proof.a);
        let (pi2, pi3) = lichen_babe::statement::pi23_to_bytes(proof);
        (
            lichen_babe::setup::digits(&bits),
            pi2.to_vec(),
            pi3.to_vec(),
        )
    }

    /// What a posted assertion (either) carries, as the hub reads it: its
    /// stage, π1's preimages and digits, π2, π3.
    #[allow(clippy::type_complexity)]
    fn read_asserted(
        &self,
        asserted: &Transaction,
    ) -> Result<(Stage, Vec<[u8; 20]>, Vec<u8>, Vec<u8>, Vec<u8>)> {
        let stage = self.stage_of(asserted, Stage::assert)?;
        let args = Self::args(asserted);
        let (signed, pi2, pi3) = match stage {
            Stage::Full => Babe::read_assert(&args).ok_or_else(|| anyhow!("assert's arguments"))?,
            Stage::Corrected => Babe::read_assert_corrected(&args)?.0,
        };
        let preimages: Vec<[u8; 20]> = signed
            .iter()
            .map(|a| a[..].try_into())
            .collect::<std::result::Result<_, _>>()?;
        let lamport = self.lamport(stage)?;
        let digits: Vec<u8> = preimages
            .iter()
            .enumerate()
            .map(|(d, x)| {
                lamport
                    .read(d, x)
                    .ok_or_else(|| anyhow!("a preimage of no digit"))
            })
            .collect::<Result<_>>()?;
        Ok((stage, preimages, digits, pi2.to_vec(), pi3.to_vec()))
    }

    /// The proof an assertion (either) carries, as the hub reads it.
    pub fn asserted(&self, asserted: &Transaction) -> Result<(Vec<bool>, Proof<Bn254>)> {
        let (_, _, digits, pi2, pi3) = self.read_asserted(asserted)?;
        let bits: Vec<bool> = digits
            .iter()
            .flat_map(|v| [v & 1 == 1, v & 2 == 2])
            .collect();
        let proof = lichen_babe::statement::proof_from_chain(&bits, &pi2, &pi3)
            .ok_or_else(|| anyhow!("no proof"))?;
        Ok((bits, proof))
    }

    /// The hub challenges an assertion (either): π1's secrets of its
    /// setup's dealing, from Alice's preimages on chain.
    pub fn challenge(&self, rt: &Regtest, asserted: &Transaction) -> Result<Transaction> {
        let (stage, preimages, digits, _, _) = self.read_asserted(asserted)?;
        let dealer = self.dealer(stage);
        let secrets: Vec<_> = digits
            .iter()
            .enumerate()
            .map(|(d, v)| dealer.reveal(d, *v))
            .collect();
        let wire = Babe::challenge_wire(reveal_args(
            &preimages,
            &complete(&self.setup(stage)?.presigs, &digits, &secrets),
        ));
        self.link(rt, stage.challenge(), wire)
    }

    /// Challenged: Alice reads π1's secrets from the challenge, rebuilds
    /// each kept instance's input secrets (the opened shares and the
    /// revealed ones), evaluates its garbling, and decrypts with `proof`.
    /// A kept instance's `msg` pays her through `wrongly_challenged`; none
    /// opening means the proof is false (or the hub cheated at setup, which
    /// the opened instances would have shown).
    pub fn answer(
        &self,
        rt: &Regtest,
        challenged: &Transaction,
        bits: &[bool],
        proof: &Proof<Bn254>,
    ) -> Result<Option<Transaction>> {
        let stage = self.stage_of(challenged, Stage::challenge)?;
        let setup = self.setup(stage)?;
        let (posted, revealed) = extract(
            self.lamport(stage)?,
            &setup.presigs,
            &Self::args(challenged),
        )
        .ok_or_else(|| anyhow!("the challenge's secrets"))?;
        anyhow::ensure!(
            posted == lichen_babe::setup::digits(bits),
            "the challenge reveals the asserted digits"
        );
        let of_digits: Vec<(u64, Vec<[u8; 32]>)> = setup
            .opened
            .iter()
            .map(|(i, shares)| {
                (
                    *i,
                    shares
                        .iter()
                        .zip(&posted)
                        .map(|(s, v)| s[*v as usize])
                        .collect(),
                )
            })
            .collect();
        for (k, (i, inst)) in setup.instances.iter().enumerate() {
            let Some(secrets) = interpolate(*i, &of_digits, &revealed) else {
                continue;
            };
            let r_pi1 = evaluate(&inst.gc, bits, &inst.labels(bits, &secrets));
            let msg = decrypt(&inst.ct, &r_pi1, proof);
            if inst.is_msg(&msg) {
                println!("kept instance {i} opens");
                return Ok(Some(self.terminal(
                    rt,
                    &stage.wrongly(k),
                    Babe::wrongly_wire(&msg),
                )?));
            }
            println!("kept instance {i} does not open");
        }
        Ok(None)
    }

    /// Mine until `name`'s timeout is final, then post it.
    pub fn timeout(&self, rt: &Regtest, name: &str) -> Result<Transaction> {
        let w = &self.babe.spec.windows;
        match name {
            "respond_timeout" => rt.mine_to_height(w.t_a)?,
            "request_timeout" => drop(rt.mine(u64::from(w.w_h))?),
            "failure" | "failure_corrected" => drop(rt.mine(u64::from(w.delta1))?),
            "success" | "success_corrected" => drop(rt.mine(u64::from(w.delta2))?),
            other => anyhow::bail!("no timeout {other}"),
        }
        self.terminal(rt, name, vec![])
    }
}
