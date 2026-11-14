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
