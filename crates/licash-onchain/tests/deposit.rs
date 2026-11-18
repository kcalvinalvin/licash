//! Deposits: their address, and the claim and refund that spend them,
//! checked against what a node verifies of a taproot script spend.

use bitcoin::{
    hashes::{sha256, Hash},
    key::Secp256k1,
    secp256k1::{schnorr, Message, SecretKey, XOnlyPublicKey},
    sighash::{Prevouts, SighashCache},
    taproot::{ControlBlock, LeafVersion},
    Address, Amount, FeeRate, Network, OutPoint, ScriptBuf, TapLeafHash, TapSighashType,
    Transaction, TxOut, Txid,
};
use licash_onchain::deposit::{Error, Htlc};

const VALUE: Amount = Amount::from_sat(100_000);

fn secret(byte: u8) -> SecretKey {
    SecretKey::from_slice(&[byte; 32]).unwrap()
}

fn key(secret: &SecretKey) -> XOnlyPublicKey {
    secret.x_only_public_key(&Secp256k1::new()).0
}

fn htlc() -> Htlc {
    Htlc {
        hash: sha256::Hash::hash(&[7; 32]).to_byte_array(),
        claim: key(&secret(1)),
        refund: key(&secret(2)),
        timeout: 850_000,
    }
}

fn funding() -> OutPoint {
    OutPoint {
        txid: Txid::from_byte_array([3; 32]),
        vout: 1,
    }
}

fn to() -> ScriptBuf {
    ScriptBuf::new_p2tr_tweaked(bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(
        key(&secret(9)),
    ))
}

/// What a node checks of `tx`'s spend of `htlc` by `script`: that the
/// control block commits `script` to the output key, and that the
/// signature, first in the witness, is of `key` over the spend.
fn verify(htlc: &Htlc, tx: &Transaction, script: &ScriptBuf, key: &XOnlyPublicKey) {
    let secp = Secp256k1::verification_only();
    let witness: Vec<&[u8]> = tx.input[0].witness.iter().collect();
    let (control, leaf) = (witness[witness.len() - 1], witness[witness.len() - 2]);
    assert_eq!(leaf, script.as_bytes());
    let control = ControlBlock::decode(control).unwrap();
    let output = htlc.script_pubkey();
    let output_key = XOnlyPublicKey::from_slice(&output.as_bytes()[2..]).unwrap();
    assert!(control.verify_taproot_commitment(&secp, output_key, script));
    let prevout = TxOut {
        value: VALUE,
        script_pubkey: output,
    };
    let sighash = SighashCache::new(tx)
        .taproot_script_spend_signature_hash(
            0,
            &Prevouts::All(&[prevout]),
            TapLeafHash::from_script(script, LeafVersion::TapScript),
            TapSighashType::Default,
        )
        .unwrap();
    let message = Message::from_digest(sighash.to_byte_array());
    let signature = schnorr::Signature::from_slice(witness[0]).unwrap();
    secp.verify_schnorr(&signature, &message, key).unwrap();
}

#[test]
fn a_deposit_is_a_taproot_output_of_its_terms() {
    let htlc = htlc();
    let address = htlc.address(Network::Regtest);
    assert!(address.to_string().starts_with("bcrt1p"), "{address}");
    assert_eq!(address.script_pubkey(), htlc.script_pubkey());
    let parsed: Address = address
        .to_string()
        .parse::<Address<_>>()
        .unwrap()
        .assume_checked();
    assert_eq!(parsed, address);
    // Each term makes another address.
    let mut others = vec![htlc; 4];
    others[0].hash[0] ^= 1;
    others[1].claim = key(&secret(5));
    others[2].refund = key(&secret(5));
    others[3].timeout += 1;
    for other in others {
        assert_ne!(other.script_pubkey(), htlc.script_pubkey());
    }
    // The timeout, as a script number.
    let refund = htlc.refund_script();
    assert_eq!(&refund.as_bytes()[..4], &[0x03, 0x50, 0xf8, 0x0c]);
}

#[test]
fn the_hub_claims_with_the_preimage_and_its_key() {
    let htlc = htlc();
    let rate = FeeRate::from_sat_per_vb(2).unwrap();
    let tx = htlc
        .claim_tx(funding(), VALUE, to(), rate, &secret(1), &[7; 32])
        .unwrap();
    assert_eq!(tx.input[0].previous_output, funding());
    assert_eq!(tx.lock_time, bitcoin::absolute::LockTime::ZERO);
    let witness: Vec<&[u8]> = tx.input[0].witness.iter().collect();
    assert_eq!(witness.len(), 4);
    assert_eq!(witness[1], [7; 32]);
    verify(&htlc, &tx, &htlc.claim_script(), &htlc.claim);
    // The fee at the rate, of the transaction as signed.
    let fee = VALUE - tx.output[0].value;
    assert_eq!(fee, rate.fee_wu(tx.weight()).unwrap());
    assert_eq!(tx.output[0].script_pubkey, to());

    let wrong = htlc.claim_tx(funding(), VALUE, to(), rate, &secret(1), &[8; 32]);
    assert!(matches!(wrong, Err(Error::Preimage)));
    let wrong = htlc.claim_tx(funding(), VALUE, to(), rate, &secret(2), &[7; 32]);
    assert!(matches!(wrong, Err(Error::Key)));
}

#[test]
fn the_payer_refunds_after_the_timeout_with_its_key() {
    let htlc = htlc();
    let rate = FeeRate::from_sat_per_vb(1).unwrap();
    let tx = htlc
        .refund_tx(funding(), VALUE, to(), rate, &secret(2))
        .unwrap();
    assert_eq!(tx.lock_time.to_consensus_u32(), htlc.timeout);
    assert!(tx.input[0].sequence.enables_absolute_lock_time());
    assert_eq!(tx.input[0].witness.len(), 3);
    verify(&htlc, &tx, &htlc.refund_script(), &htlc.refund);
    let wrong = htlc.refund_tx(funding(), VALUE, to(), rate, &secret(1));
    assert!(matches!(wrong, Err(Error::Key)));
}

#[test]
fn a_deposit_too_small_for_its_fee_is_not_spent() {
    let htlc = htlc();
    let rate = FeeRate::from_sat_per_vb(10).unwrap();
    let small = Amount::from_sat(1_500);
    let spent = htlc.refund_tx(funding(), small, to(), rate, &secret(2));
    assert!(matches!(spent, Err(Error::TooSmall { .. })), "{spent:?}");
    // Paying more than dust after the fee: spent.
    let enough = Amount::from_sat(2_000);
    let tx = htlc
        .refund_tx(funding(), enough, to(), rate, &secret(2))
        .unwrap();
    assert!(tx.output[0].value >= to().minimal_non_dust());
}

#[test]
fn a_spend_racing_its_deadline_pays_all_but_dust_at_most() {
    let htlc = htlc();
    let small = Amount::from_sat(1_500);
    let dust = to().minimal_non_dust();
    let rate = FeeRate::from_sat_per_vb(10).unwrap();
    // As much as the plain spend while the fee leaves more than dust.
    let enough = Amount::from_sat(2_000);
    let plain = htlc
        .refund_tx(funding(), enough, to(), rate, &secret(2))
        .unwrap();
    let capped = htlc
        .refund_capped(funding(), enough, to(), rate, &secret(2))
        .unwrap();
    assert_eq!(capped, plain);
    // Else all of it but dust.
    let tx = htlc
        .refund_capped(funding(), small, to(), rate, &secret(2))
        .unwrap();
    assert_eq!(tx.output[0].value, dust);
    let huge = FeeRate::from_sat_per_vb(VALUE.to_sat()).unwrap();
    let tx = htlc
        .refund_capped(funding(), VALUE, to(), huge, &secret(2))
        .unwrap();
    assert_eq!(tx.output[0].value, dust);
    verify(&htlc, &tx, &htlc.refund_script(), &htlc.refund);
    let claim = htlc
        .claim_capped(funding(), VALUE, to(), huge, &secret(1), &[7; 32])
        .unwrap();
    assert_eq!(claim.output[0].value, dust);
    verify(&htlc, &claim, &htlc.claim_script(), &htlc.claim);
    let wrong = htlc.claim_capped(funding(), VALUE, to(), huge, &secret(1), &[8; 32]);
    assert!(wrong.is_err());
    // A coin that is dust itself is not spent.
    let spent = htlc.refund_capped(funding(), dust - Amount::ONE_SAT, to(), rate, &secret(2));
    assert!(matches!(spent, Err(Error::TooSmall { .. })), "{spent:?}");
}

#[test]
fn a_payout_claims_exactly_its_amount_the_rest_its_fee() {
    let htlc = htlc();
    let amount = Amount::from_sat(99_000);
    let tx = htlc
        .claim_exact(funding(), VALUE, to(), amount, &secret(1), &[7; 32])
        .unwrap();
    assert_eq!(tx.output.len(), 1);
    assert_eq!(tx.output[0].value, amount);
    assert_eq!(tx.output[0].script_pubkey, to());
    assert_eq!(tx.lock_time, bitcoin::absolute::LockTime::ZERO);
    verify(&htlc, &tx, &htlc.claim_script(), &htlc.claim);
    // Its size does not depend on what it pays: a claim of all of it at a
    // rate weighs the same.
    let rate = FeeRate::from_sat_per_vb(2).unwrap();
    let all = htlc
        .claim_tx(funding(), VALUE, to(), rate, &secret(1), &[7; 32])
        .unwrap();
    assert_eq!(tx.weight(), all.weight());

    let wrong = htlc.claim_exact(funding(), VALUE, to(), amount, &secret(1), &[8; 32]);
    assert!(matches!(wrong, Err(Error::Preimage)));
    let wrong = htlc.claim_exact(funding(), VALUE, to(), amount, &secret(2), &[7; 32]);
    assert!(matches!(wrong, Err(Error::Key)));
    let dust = to().minimal_non_dust();
    for amount in [VALUE + Amount::ONE_SAT, dust - Amount::ONE_SAT] {
        let wrong = htlc.claim_exact(funding(), VALUE, to(), amount, &secret(1), &[7; 32]);
        assert!(
            matches!(wrong, Err(Error::Exact { .. })),
            "{amount}: {wrong:?}"
        );
    }
    htlc.claim_exact(funding(), VALUE, to(), dust, &secret(1), &[7; 32])
        .unwrap();
}

#[test]
fn the_preimage_is_read_from_a_claim_and_from_nothing_else() {
    let htlc = htlc();
    let rate = FeeRate::from_sat_per_vb(2).unwrap();
    let amount = Amount::from_sat(99_000);
    let claims = [
        htlc.claim_tx(funding(), VALUE, to(), rate, &secret(1), &[7; 32])
            .unwrap(),
        htlc.claim_exact(funding(), VALUE, to(), amount, &secret(1), &[7; 32])
            .unwrap(),
    ];
    for claim in &claims {
        assert_eq!(htlc.preimage_in(claim, funding()), Some([7; 32]));
        // Not of another coin it does not spend.
        let other = OutPoint {
            vout: 0,
            ..funding()
        };
        assert_eq!(htlc.preimage_in(claim, other), None);
        // Not of other terms, whose hash it is not the preimage of.
        let mut others = htlc;
        others.hash = sha256::Hash::hash(&[8; 32]).to_byte_array();
        assert_eq!(others.preimage_in(claim, funding()), None);
    }
    let refund = htlc
        .refund_tx(funding(), VALUE, to(), rate, &secret(2))
        .unwrap();
    assert_eq!(htlc.preimage_in(&refund, funding()), None);
}

#[test]
fn a_claim_is_told_from_a_refund_whatever_it_pays() {
    let htlc = htlc();
    let rate = FeeRate::from_sat_per_vb(2).unwrap();
    let higher = FeeRate::from_sat_per_vb(20).unwrap();
    for claim in [
        htlc.claim_tx(funding(), VALUE, to(), rate, &secret(1), &[7; 32])
            .unwrap(),
        htlc.claim_capped(funding(), VALUE, to(), higher, &secret(1), &[7; 32])
            .unwrap(),
    ] {
        assert!(htlc.claimed_by(&claim, funding()));
        let other = OutPoint {
            vout: 0,
            ..funding()
        };
        assert!(!htlc.claimed_by(&claim, other));
    }
    let refund = htlc
        .refund_tx(funding(), VALUE, to(), rate, &secret(2))
        .unwrap();
    assert!(!htlc.claimed_by(&refund, funding()));
}
