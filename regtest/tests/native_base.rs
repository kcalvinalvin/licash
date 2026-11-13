//! Real Base -> request keys -> frozen F/R -> D -> owner consent -> E -> R.
//! The graph has only the request leaf: no native BABE/guest acceptance or
//! production funding gate is implied by this controlled construction test.

use anyhow::{ensure, Context, Result};
use bitcoincore_rpc::{json::FundRawTransactionOptions, RpcApi};
use k256::Scalar;
use licash::{
    base::{Base, ChainTerms, Fees, GraphTerms, Limits, Parts, Party, Recipe, ResponseKeys},
    chain::{self, Chain, Checkpoint, WitnessInclusion},
    claim::{self, Item, UnsignedClaim},
    crypto::{self, Point},
    hold::Envelope,
    note::Evidence,
    note::{Derivation, HoldRoot, IssuerKeys, Note},
    opening::{FrozenPrefix, GraphOutput, RequestLeaf},
    request::{Request, Signer, WITNESS_ITEMS},
    spend::ChainProfile,
    statement::SecretKeys,
};
use lngap_btc::{
    bitcoin::{
        absolute,
        consensus::serialize,
        hashes::Hash,
        script::Builder,
        secp256k1::{Keypair, Secp256k1, SecretKey},
        transaction, Amount, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Witness,
    },
    regtest::Regtest,
    sighash::sign_tapscript,
    taptree::{Leaf, TapTree},
};

fn point(value: u64) -> Point {
    Point::from_projective(&crypto::generator_mul(&Scalar::from(value)))
}
fn key(value: u64) -> Keypair {
    let secret = crypto::ScalarBytes::from_scalar(&Scalar::from(value));
    Keypair::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_slice(&secret.0).unwrap(),
    )
}
fn party(value: u64) -> Party {
    let payout = Builder::new()
        .push_int(1)
        .push_slice(point(value + 3).x_only())
        .into_script();
    Party {
        funding: point(value).x_only(),
        payment: point(value + 1).x_only(),
        delayed: point(value + 2).x_only(),
        payout_script: payout.into_bytes(),
    }
}

#[test]
fn canonical_base_final_d_and_owner_consent_bind_a_real_thirty_two_note_request() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let anchor_hash = rt.rpc.get_best_block_hash()?;
    let anchor = rt.rpc.get_block_header(&anchor_hash)?;
    let height = rt.height()?;
    let mut times = vec![anchor.time];
    let mut previous = anchor;
    for _ in 1..11 {
        previous = rt.rpc.get_block_header(&previous.prev_blockhash)?;
        times.push(previous.time);
    }
    times.reverse();
    let checkpoint = Checkpoint::new(
        ChainProfile::Core30DefaultRegtest,
        height,
        serialize(&anchor).try_into().unwrap(),
        times,
        None,
    )?;
    let coin = rt
        .rpc
        .list_unspent(Some(1), None, None, None, None)?
        .into_iter()
        .next()
        .context("confirmed named deposit")?;
    let od = OutPoint {
        txid: coin.txid,
        vout: coin.vout,
    };
    let issuer = IssuerKeys::random(2)?;
    let keyset = issuer.keyset()?;
    // Independently selected response keys precede Base, never derive from D.
    let response_keys = ResponseKeys::new(SecretKeys::random().public())?;
    let base = Base::new(Parts {
        nonce: Base::fresh_nonce(),
        spend_root: point(17),
        claim_key: point(19).x_only(),
        keyset: keyset.clone(),
        deposit: serialize(&od).try_into().unwrap(),
        cap: 128,
        response_keys,
        parties: [party(23), party(29)],
        graph: GraphTerms {
            id: 7,
            unit_sats: 1,
            bits: 21,
            low_bits: 9,
            deposit_units: 500_000,
            close_height: height + 1_000,
            reserve_alice: 20_000,
            reserve_hub: 100_000,
            to_self_delay: 6,
            commit_fee: 1_000,
            presign_fee: 1_000,
            dust: 330,
            delta: 6,
            delta_prime: 6,
            deadline_margin: 8,
            response_window: 6,
            assert_height: height + 800,
            hub_challenge: 12,
            holder_answer: 6,
        },
        // Static recipe identities are fixture data, not installed v2 guests.
        recipe: Recipe {
            graph_version: 1,
            holder_program: [11; 32],
            counter_program: [13; 32],
            full_instances: 4,
            full_kept: 2,
            corrected_instances: 4,
            corrected_kept: 2,
        },
        chain: ChainTerms {
            checkpoint,
            funding_depth: 1,
            request_depth: 1,
            publication_depth: 1,
            lookback: 6,
            observation_margin: 1,
            max_headers: 1_000,
        },
        limits: Limits {
            max_transaction_bytes: 100_000,
            max_publication_bytes: 64_000,
            max_authorizations: 128,
            max_source_inputs: 128,
        },
        fees: Fees {
            sweep_fee_max: 1_000,
            external_fee_max: 2_000,
            max_rate_sat_per_kvb: 10_000,
            opening_fee_quote: 3_000,
        },
    })?;
    ensure!(Base::from_bytes(&base.to_bytes())?.digest() == base.digest());
    let p = base.parts();
    let signer = Signer::derive(&[7; 32], &base.digest());
    let request_keys = signer.public();
    let script = ScriptBuf::from_bytes(request_keys.request_leaf_for_base(
        &base.digest(),
        &p.parties[0].payment,
        &p.parties[1].payment,
        p.graph.to_self_delay,
    )?);
    let tree = TapTree::new(vec![Leaf::new(
        "request",
        script.clone(),
        lngap_btc::tx::Timelock::csv(p.graph.to_self_delay),
    )])?;
    let control = tree.control_block("request")?.serialize();
    let graph_value = p.graph.graph_value()?;
    let unsigned = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: od,
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(graph_value),
            script_pubkey: tree.script_pubkey(),
        }],
    };
    let funding = rt
        .rpc
        .fund_raw_transaction(
            &unsigned,
            Some(&FundRawTransactionOptions {
                change_position: Some(0),
                ..Default::default()
            }),
            None,
        )?
        .transaction()?;
    let graph_index = 1;
    ensure!(funding.output[graph_index].value.to_sat() == graph_value);
    for input in &funding.input {
        ensure!(input.script_sig.is_empty());
        let utxo = rt
            .rpc
            .get_tx_out(
                &input.previous_output.txid,
                input.previous_output.vout,
                Some(true),
            )?
            .context("live funding input")?;
        ensure!(utxo.confirmations >= p.chain.funding_depth);
        let previous = rt.get_tx(&input.previous_output.txid)?;
        let spk = &previous.output[input.previous_output.vout as usize].script_pubkey;
        ensure!(spk.is_p2wpkh() || spk.is_p2wsh() || spk.is_p2tr());
    }
    let mut request_tx = lngap_btc::tx::build_spend(
        OutPoint {
            txid: funding.compute_txid(),
            vout: graph_index as u32,
        },
        &lngap_btc::tx::Timelock::csv(p.graph.to_self_delay),
        vec![TxOut {
            value: Amount::from_sat(graph_value - 30_000),
            script_pubkey: ScriptBuf::from_bytes(p.parties[0].payout_script.clone()),
        }],
    );
    let prefix = FrozenPrefix::new(
        chain::Transaction::parse(&serialize(&funding), 100_000)?,
        chain::Transaction::parse(&serialize(&request_tx), 100_000)?,
        GraphOutput {
            index: graph_index as u32,
            output: chain::Output {
                value: graph_value,
                script: tree.script_pubkey().to_bytes(),
            },
        },
        GraphOutput {
            index: 0,
            output: chain::Output {
                value: graph_value - 30_000,
                script: p.parties[0].payout_script.clone(),
            },
        },
        RequestLeaf::for_base(
            base.digest(),
            request_keys.clone(),
            p.parties[0].payment,
            p.parties[1].payment,
            p.graph.to_self_delay,
            control.clone(),
        )?,
    )?;
    let final_digest = base.final_digest(&prefix)?;
    ensure!(final_digest.as_bytes() != &base.digest());
    let owner_signature =
        crypto::schnorr_sign(&Scalar::from(17u64), &base.owner_consent_message(&prefix)?)?;
    base.verify_owner_consent(&prefix, &owner_signature)?;

    let hold = HoldRoot::from_seed(&[7; 32])?;
    let mut items = Vec::new();
    let mut entries = Vec::new();
    for index in 0..32 {
        let d = Derivation::derive(4, p.spend_root, hold.public(), &[23; 32], index)?;
        let proof = issuer.prove_blinded(4, d.blinded()?)?;
        let note = Note::from_parts(
            &keyset,
            *keyset.id(),
            d.message().clone(),
            proof.open_point(&keyset, &d)?,
            Evidence::Linked {
                dleq: proof.dleq(),
                blinding: *d.blinding(),
                commit: crypto::dleq_commit(&Scalar::from(71u64), &d.blinded()?.to_projective()?),
            },
        )?;
        note.verify(&keyset)?;
        d.ownership().verify(&p.spend_root, note.message())?;
        items.push(Item::Settled(note.message().clone()));
        entries.push((note.message().clone(), d.hold_key(&hold)?));
    }
    entries.sort_by_key(|(message, _)| message.identifier().expect("verified message"));
    let claim = UnsignedClaim::new(
        claim::Context {
            final_digest: *final_digest.as_bytes(),
            deposit: p.deposit,
            cap: p.cap,
        },
        &keyset,
        128,
        claim::Parts {
            items,
            sources: vec![],
            activations: vec![],
            authorizations: entries.iter().map(|(message, _)| message.clone()).collect(),
        },
        p.limits.max_authorizations as usize,
    )?;
    Envelope::sign(claim.hold_context(), &entries, 128)?.verify(
        &claim.hold_context(),
        claim.hold_root(),
        128,
    )?;
    let request = Request {
        amount: claim.amount(),
        claim_digest: *claim.digest(),
    };
    let prevouts = [funding.output[graph_index].clone()];
    let alice_sig = sign_tapscript(&key(24), &request_tx, 0, &prevouts, &script)?;
    let hub_sig = sign_tapscript(&key(30), &request_tx, 0, &prevouts, &script)?;
    let signed_request = signer.sign(request);
    request_tx.input[0].witness = Witness::from_slice(&signed_request.witness(
        *alice_sig.as_ref(),
        *hub_sig.as_ref(),
        script.to_bytes(),
        control,
    ));
    ensure!(request_tx.input[0].witness.len() == WITNESS_ITEMS);
    // Controlled primitive fixture only: a production gate must additionally
    // validate the full graph and actual instances before releasing any F sig.
    let signed_funding = rt
        .rpc
        .sign_raw_transaction_with_wallet(&funding, None, None)?;
    ensure!(signed_funding.complete);
    let funding = signed_funding.transaction()?;
    ensure!(funding.compute_txid().to_byte_array() == prefix.funding().txid());
    rt.send_and_confirm(&funding)?;
    rt.mine(u64::from(p.graph.to_self_delay))?;
    let (txid, request_height) = rt.send_and_confirm(&request_tx)?;
    let block = rt
        .rpc
        .get_block(&rt.rpc.get_block_hash(u64::from(request_height))?)?;
    let position = block
        .txdata
        .iter()
        .position(|tx| tx.compute_txid() == txid)
        .context("mined R position")?;
    let headers = (height + 1..=request_height)
        .map(|h| -> Result<_> {
            let hash = rt.rpc.get_block_hash(u64::from(h))?;
            Ok(serialize(&rt.rpc.get_block_header(&hash)?)
                .try_into()
                .unwrap())
        })
        .collect::<Result<_>>()?;
    let chain = Chain::verify(
        p.chain.checkpoint.clone(),
        &p.chain.checkpoint.commitment(),
        headers,
        p.chain.max_headers as usize,
    )?;
    ensure!(
        chain
            .confirmations(request_height)
            .context("request depth")?
            >= u64::from(p.chain.request_depth)
    );
    let included = WitnessInclusion::capture(&serialize(&block), position, 4_000_000)?.verify(
        &chain,
        request_height,
        100_000,
    )?;
    ensure!(prefix.request_from_included(&included)? == request);
    base.verify_owner_consent(&prefix, &owner_signature)?;
    Ok(())
}
