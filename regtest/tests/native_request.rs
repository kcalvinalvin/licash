//! Positive Core compatibility of native request math and the proposed strict
//! leaf, not an integrated opening, publication or recovery statement.

use anyhow::{ensure, Context, Result};
use bitcoincore_rpc::RpcApi;
use licash::{
    chain::{Chain, Checkpoint, WitnessInclusion},
    crypto::tagged_hash,
    opening::{FrozenPrefix, GraphOutput, RequestLeaf},
    request::{PublicKeys, Request, Signer, WITNESS_ITEMS},
    spend::ChainProfile,
};
use lngap_btc::{
    bitcoin::{
        consensus::serialize,
        opcodes::all::*,
        script::Builder,
        secp256k1::{Keypair, Secp256k1, SecretKey},
        Amount, ScriptBuf, TxOut, Witness,
    },
    regtest::Regtest,
    sighash::sign_tapscript,
    taptree::{Leaf, TapTree},
    tx::{build_spend, Timelock},
};
use lngap_lamport::winternitz::{WotsExt, WotsParams, WotsPublic, WotsSecret};

fn key(byte: u8) -> Keypair {
    Keypair::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_slice(&[byte; 32]).unwrap(),
    )
}

/// Connect E and Q authorization to actual issuer-authenticated native notes.
/// The opening context is still a fixture, not final-D/P/guest acceptance.
fn native_cash_claim_request() -> Result<Request> {
    use licash::{
        claim::{Context as ClaimContext, Item, Parts, UnsignedClaim},
        crypto::{self, Point, ScalarBytes},
        hold::Envelope,
        note::Evidence,
        note::{Derivation, HoldRoot, IssuerKeys, Note},
    };
    let issuer = IssuerKeys::random(2)?;
    let keyset = issuer.keyset()?;
    let spend = Point::from_projective(&crypto::generator_mul(&ScalarBytes([17; 32]).to_scalar()?));
    let hold = HoldRoot::from_seed(&[7; 32])?;
    let commit_scalar = ScalarBytes([71; 32]).to_scalar()?;
    let mut items = Vec::new();
    let mut entries = Vec::new();
    for index in 200..232 {
        let d = Derivation::derive(4, spend, hold.public(), &[23; 32], index)?;
        let proof = issuer.prove_blinded(4, d.blinded()?)?;
        let note = Note::from_parts(
            &keyset,
            *keyset.id(),
            d.message().clone(),
            proof.open_point(&keyset, &d)?,
            Evidence::Linked {
                dleq: proof.dleq(),
                blinding: *d.blinding(),
                commit: crypto::dleq_commit(&commit_scalar, &d.blinded()?.to_projective()?),
            },
        )?;
        note.verify(&keyset)?;
        d.ownership().verify(&spend, note.message())?;
        items.push(Item::Settled(note.message().clone()));
        entries.push((note.message().clone(), d.hold_key(&hold)?));
    }
    entries.sort_by_key(|(message, _)| message.identifier().expect("validated native message"));
    let claim = UnsignedClaim::new(
        ClaimContext {
            final_digest: [101; 32],
            deposit: [103; 36],
            cap: 128,
        },
        &keyset,
        128,
        Parts {
            items,
            sources: vec![],
            activations: vec![],
            authorizations: entries.iter().map(|(m, _)| m.clone()).collect(),
        },
        32,
    )?;
    let envelope = Envelope::sign(claim.hold_context(), &entries, 32)?;
    envelope.verify(&claim.hold_context(), claim.hold_root(), 32)?;
    Ok(Request {
        amount: claim.amount(),
        claim_digest: *claim.digest(),
    })
}

/// Fixture-only construction: keep the upstream WOTS gadget unchanged, adding
/// a separate canonical digit preamble. No change to existing funded leaves.
fn leaf(keys: &PublicKeys, alice: &Keypair, hub: &Keypair, delay: u16) -> ScriptBuf {
    let mut b = Builder::new();
    if delay > 0 {
        b = b
            .push_int(i64::from(delay))
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP);
    }
    for party in [alice, hub] {
        b = b
            .push_opcode(OP_SIZE)
            .push_int(64)
            .push_opcode(OP_EQUALVERIFY)
            .push_x_only_key(&party.x_only_public_key().0)
            .push_opcode(OP_CHECKSIGVERIFY);
    }
    for depth in (0..154).step_by(2) {
        b = b
            .push_int(depth)
            .push_opcode(OP_PICK)
            .push_opcode(OP_SIZE)
            .push_int(2)
            .push_opcode(OP_LESSTHAN)
            .push_opcode(OP_VERIFY)
            .push_opcode(OP_DUP)
            .push_opcode(OP_SIZE)
            .push_opcode(OP_IF)
            .push_int(1)
            .push_int(16)
            .push_opcode(OP_WITHIN)
            .push_opcode(OP_VERIFY)
            .push_opcode(OP_ELSE)
            .push_int(0)
            .push_opcode(OP_EQUALVERIFY)
            .push_opcode(OP_ENDIF)
            .push_opcode(OP_DROP);
    }
    b = b.wots_verify(&WotsPublic {
        params: WotsParams::for_bytes(4),
        digits: keys.amount_elements().to_vec(),
    });
    for _ in 0..4 {
        b = b.push_opcode(OP_2DROP);
    }
    b = b.wots_verify(&WotsPublic {
        params: WotsParams::for_bytes(32),
        digits: keys.claim_elements().to_vec(),
    });
    for _ in 0..32 {
        b = b.push_opcode(OP_2DROP);
    }
    b.push_int(1).into_script()
}

#[test]
fn native_request_keys_and_pairs_match_pristine_hash160_math() -> Result<()> {
    let seed = [7; 32];
    let base = [11; 32];
    for request in [
        Request {
            amount: 0,
            claim_digest: [0; 32],
        },
        Request {
            amount: 0x0178_ff00,
            claim_digest: std::array::from_fn(|i| (i * 17) as u8),
        },
        Request {
            amount: u32::MAX,
            claim_digest: [255; 32],
        },
    ] {
        let native = Signer::derive(&seed, &base);
        let keys = native.public();
        let b = WotsSecret::from_entropy(
            WotsParams::for_bytes(4),
            tagged_hash("licash/request-b-key/v2", &[&seed, &base]),
        );
        let e = WotsSecret::from_entropy(
            WotsParams::for_bytes(32),
            tagged_hash("licash/request-e-key/v2", &[&seed, &base]),
        );
        ensure!(keys.amount_elements().as_slice() == b.public().digits);
        ensure!(keys.claim_elements().as_slice() == e.public().digits);
        let mut pairs: Vec<_> = e
            .sign(&request.claim_digest)?
            .consumption_order()
            .into_iter()
            .rev()
            .collect();
        pairs.extend(
            b.sign(&request.amount.to_be_bytes())?
                .consumption_order()
                .into_iter()
                .rev(),
        );
        ensure!(native.sign(request).pairs() == pairs);
    }
    Ok(())
}

#[test]
fn canonical_native_request_confirms_and_decodes_from_bip141_witness() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let alice = key(17);
    let hub = key(19);
    let anchor_hash = rt.rpc.get_best_block_hash()?;
    let anchor = rt.rpc.get_block_header(&anchor_hash)?;
    let anchor_height = rt.height()?;
    let mut times = vec![anchor.time];
    let mut previous = anchor;
    for _ in 1..11 {
        previous = rt.rpc.get_block_header(&previous.prev_blockhash)?;
        times.push(previous.time);
    }
    times.reverse();
    let cp = Checkpoint::new(
        ChainProfile::Core30DefaultRegtest,
        anchor_height,
        serialize(&anchor).try_into().unwrap(),
        times,
        None,
    )?;
    for (index, request) in [
        Request {
            amount: 0,
            claim_digest: [0; 32],
        },
        Request {
            amount: 0x0178_ff00,
            claim_digest: std::array::from_fn(|i| (i * 17) as u8),
        },
        Request {
            amount: u32::MAX,
            claim_digest: [255; 32],
        },
        native_cash_claim_request()?,
    ]
    .into_iter()
    .enumerate()
    {
        // Every funded fixture owns distinct WOTS context, even on this local
        // throwaway chain. The b extremes exercise encoding, not payable value
        // or claim eligibility against the fixture's arbitrary funding amount.
        let base = [index as u8; 32];
        let base_bound = index == 3;
        let signer = Signer::derive(&[7; 32], &base);
        let keys = signer.public();
        let delay = index as u16;
        let timelock = if delay == 0 {
            Timelock::NONE
        } else {
            Timelock::csv(delay)
        };
        let script = ScriptBuf::from_bytes(if base_bound {
            keys.request_leaf_for_base(
                &base,
                &alice.x_only_public_key().0.serialize(),
                &hub.x_only_public_key().0.serialize(),
                delay,
            )?
        } else {
            keys.request_leaf(
                &alice.x_only_public_key().0.serialize(),
                &hub.x_only_public_key().0.serialize(),
                delay,
            )?
        });
        let expected = leaf(&keys, &alice, &hub, delay);
        let expected = if base_bound {
            let mut bytes = Builder::new()
                .push_slice(base)
                .push_opcode(OP_DROP)
                .into_script()
                .into_bytes();
            bytes.extend(expected.as_bytes());
            ScriptBuf::from_bytes(bytes)
        } else {
            expected
        };
        ensure!(
            script == expected,
            "native bytecode matches unchanged pristine WOTS gadget"
        );
        let tree = TapTree::new(vec![Leaf::new("request", script.clone(), timelock)])?;
        let control = tree.control_block("request")?.serialize();
        let (outpoint, prevout) = rt.fund(&tree.script_pubkey(), Amount::from_sat(500_000))?;
        let mut tx = build_spend(
            outpoint,
            &timelock,
            vec![
                TxOut {
                    value: Amount::from_sat(10_000),
                    script_pubkey: tree.script_pubkey(),
                },
                TxOut {
                    value: Amount::from_sat(470_000),
                    script_pubkey: tree.script_pubkey(),
                },
            ],
        );
        let frozen = tx.compute_txid();
        let mut funding = rt.get_tx(&outpoint.txid)?;
        for input in &mut funding.input {
            input.witness.clear();
        }
        let funding = licash::chain::Transaction::parse(&serialize(&funding), 100_000)?;
        let request_body = licash::chain::Transaction::parse(&serialize(&tx), 100_000)?;
        let prefix = FrozenPrefix::new(
            funding,
            request_body,
            GraphOutput {
                index: outpoint.vout,
                output: licash::chain::Output {
                    value: 500_000,
                    script: tree.script_pubkey().to_bytes(),
                },
            },
            GraphOutput {
                index: 1,
                output: licash::chain::Output {
                    value: 470_000,
                    script: tree.script_pubkey().to_bytes(),
                },
            },
            if base_bound {
                RequestLeaf::for_base(
                    base,
                    keys.clone(),
                    alice.x_only_public_key().0.serialize(),
                    hub.x_only_public_key().0.serialize(),
                    delay,
                    control.clone(),
                )?
            } else {
                RequestLeaf::new(
                    keys.clone(),
                    alice.x_only_public_key().0.serialize(),
                    hub.x_only_public_key().0.serialize(),
                    delay,
                    control.clone(),
                )?
            },
        )?;
        if base_bound {
            ensure!(prefix.bound_base()? == &base);
        } else {
            ensure!(prefix.bound_base().is_err());
        }
        let prefix_commitment = prefix.commitment();
        ensure!(
            prefix.funding_outpoint().index == outpoint.vout
                && prefix.request_outpoint().index == 1
        );
        let prevouts = [prevout];
        let alice_sig = sign_tapscript(&alice, &tx, 0, &prevouts, &script)?;
        let hub_sig = sign_tapscript(&hub, &tx, 0, &prevouts, &script)?;
        let signed = signer.sign(request);
        let witness = signed.witness(
            *alice_sig.as_ref(),
            *hub_sig.as_ref(),
            script.to_bytes(),
            control.clone(),
        );
        ensure!(witness.len() == WITNESS_ITEMS);
        tx.input[0].witness = Witness::from_slice(&witness);
        ensure!(
            tx.compute_txid() == frozen,
            "frozen body unchanged by signatures"
        );
        if delay > 0 {
            rt.mine(u64::from(delay))?;
        }
        let (txid, height) = rt.send_and_confirm(&tx)?;
        ensure!(txid == frozen);
        let block_hash = rt.rpc.get_block_hash(u64::from(height))?;
        let block = rt.rpc.get_block(&block_hash)?;
        let position = block
            .txdata
            .iter()
            .position(|t| t.compute_txid() == txid)
            .context("request position")?;
        let headers = (anchor_height + 1..=height)
            .map(|h| -> Result<_> {
                let hash = rt.rpc.get_block_hash(u64::from(h))?;
                Ok(serialize(&rt.rpc.get_block_header(&hash)?)
                    .try_into()
                    .unwrap())
            })
            .collect::<Result<_>>()?;
        let chain = Chain::verify(cp.clone(), &cp.commitment(), headers, 32)?;
        let included = WitnessInclusion::capture(&serialize(&block), position, 4_000_000)?
            .verify(&chain, height, 100_000)?;
        ensure!(included.transaction().inputs().len() == 1);
        ensure!(
            keys.decode_witness(included.input_witness(0)?, script.as_bytes(), &control)?
                == request
        );
        ensure!(prefix.request_from_included(&included)? == request);
        ensure!(
            prefix.commitment() == prefix_commitment,
            "mined witness does not change frozen non-witness identity"
        );
    }
    Ok(())
}
