use licash::request::{PublicKeys, Request, Signer, PUBLIC_KEYS_BYTES, WITNESS_ITEMS};

fn request() -> Request {
    Request {
        amount: 0x0178_ff00,
        claim_digest: std::array::from_fn(|i| (i * 17) as u8),
    }
}
fn fixture() -> (PublicKeys, Vec<Vec<u8>>, Vec<u8>, Vec<u8>) {
    let signer = Signer::derive(&[7; 32], &[11; 32]);
    let public = signer.public();
    let signed = signer.sign(request());
    let leaf = vec![0x51];
    let mut control = vec![0; 33];
    control[0] = 0xc0;
    let witness = signed.witness([17; 64], [19; 64], leaf.clone(), control.clone());
    (public, witness, leaf, control)
}

#[test]
fn strict_hash160_profile_round_trips_without_extra_payload() {
    let (keys, witness, leaf, control) = fixture();
    assert_eq!(keys.amount_elements().len(), 10);
    assert_eq!(keys.claim_elements().len(), 67);
    assert_eq!(keys.to_bytes().len(), PUBLIC_KEYS_BYTES);
    assert_eq!(PUBLIC_KEYS_BYTES, 1_552);
    assert_eq!(PublicKeys::from_bytes(&keys.to_bytes()).unwrap(), keys);
    assert_eq!(witness.len(), WITNESS_ITEMS);
    assert_eq!(
        keys.decode_witness(&witness, &leaf, &control).unwrap(),
        request()
    );
    assert_eq!(
        witness[..154]
            .iter()
            .step_by(2)
            .map(Vec::len)
            .collect::<Vec<_>>(),
        vec![20; 77]
    );
}

#[test]
fn base_context_and_seed_separate_keys_but_signature_bytes_are_not_identity() {
    let (keys, mut witness, leaf, control) = fixture();
    assert_ne!(Signer::derive(&[7; 32], &[12; 32]).public(), keys);
    assert_ne!(Signer::derive(&[8; 32], &[11; 32]).public(), keys);
    witness[154] = vec![27; 64];
    witness[155] = vec![29; 64];
    assert_eq!(
        keys.decode_witness(&witness, &leaf, &control).unwrap(),
        request()
    );
    // This is structural decoding, not signature verification or inclusion.
    assert_eq!(format!("{keys:?}"), "RequestPublicKeys(..)");
    assert_eq!(
        format!("{:?}", Signer::derive(&[7; 32], &[11; 32])),
        "RequestSigner(..)"
    );
}

#[test]
fn every_digit_and_reveal_has_fixed_width_canonical_framing() {
    let (keys, witness, leaf, control) = fixture();
    for i in (1..154).step_by(2) {
        for malformed in [vec![0], vec![0x80], vec![16], vec![0x81], vec![5, 0]] {
            let mut changed = witness.clone();
            changed[i] = malformed;
            assert!(keys.decode_witness(&changed, &leaf, &control).is_err());
        }
        let mut changed = witness.clone();
        changed[i - 1].pop();
        assert!(keys.decode_witness(&changed, &leaf, &control).is_err());
    }
}

#[test]
fn exact_leaf_control_signatures_and_item_count_are_required() {
    let (keys, witness, leaf, control) = fixture();
    for field in 0..7 {
        let mut changed = witness.clone();
        match field {
            0 => changed.push(vec![]),
            1 => {
                changed.remove(0);
            }
            2 => changed[154].push(1),
            3 => {
                changed[155].pop();
            }
            4 => changed[156].push(0),
            5 => changed[157][0] ^= 1,
            _ => changed[0][0] ^= 1,
        }
        assert!(keys.decode_witness(&changed, &leaf, &control).is_err());
    }
    assert!(keys.decode_witness(&witness, &[], &control).is_err());
    assert!(keys.decode_witness(&witness, &leaf, &[]).is_err());
}

#[test]
fn keys_have_no_truncation_trailing_or_other_variant_fallback() {
    let (keys, _, _, _) = fixture();
    let bytes = keys.to_bytes();
    for n in 0..bytes.len() {
        assert!(PublicKeys::from_bytes(&bytes[..n]).is_err());
    }
    let mut changed = bytes.clone();
    changed.push(0);
    assert!(PublicKeys::from_bytes(&changed).is_err());
    let mut changed = bytes;
    changed[0] ^= 1;
    assert!(PublicKeys::from_bytes(&changed).is_err());
}

#[test]
fn native_leaf_requires_distinct_valid_parties_and_binds_optional_block_delay() {
    use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
    let (keys, _, _, _) = fixture();
    let party = |byte| {
        Keypair::from_secret_key(
            &Secp256k1::new(),
            &SecretKey::from_slice(&[byte; 32]).unwrap(),
        )
        .x_only_public_key()
        .0
        .serialize()
    };
    let alice = party(17);
    let hub = party(19);
    assert!(keys.request_leaf(&alice, &alice, 0).is_err());
    assert!(keys.request_leaf(&[255; 32], &hub, 0).is_err());
    let plain = keys.request_leaf(&alice, &hub, 0).unwrap();
    let delayed = keys.request_leaf(&alice, &hub, 2).unwrap();
    assert_ne!(plain, delayed);
    assert!(delayed.ends_with(&plain));
    assert_ne!(plain, keys.request_leaf(&hub, &alice, 0).unwrap());
}

#[test]
fn base_commitment_adds_exactly_one_push_drop_without_changing_the_gadget() {
    use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
    let party = |byte| {
        Keypair::from_secret_key(
            &Secp256k1::new(),
            &SecretKey::from_slice(&[byte; 32]).unwrap(),
        )
        .x_only_public_key()
        .0
        .serialize()
    };
    let (keys, _, _, _) = fixture();
    let alice = party(17);
    let hub = party(19);
    let base = [43; 32];
    let old = keys.request_leaf(&alice, &hub, 2).unwrap();
    let bound = keys.request_leaf_for_base(&base, &alice, &hub, 2).unwrap();
    assert_eq!(bound.len(), old.len() + 34);
    assert_eq!(bound[0], 0x20);
    assert_eq!(&bound[1..33], &base);
    assert_eq!(bound[33], 0x75);
    assert_eq!(&bound[34..], old);
    assert_ne!(
        bound,
        keys.request_leaf_for_base(&[47; 32], &alice, &hub, 2)
            .unwrap()
    );
    assert!(keys
        .request_leaf_for_base(&base, &alice, &alice, 2)
        .is_err());
}
