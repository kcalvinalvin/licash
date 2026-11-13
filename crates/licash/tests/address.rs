use k256::Scalar;
use licash::{
    address::{index, Address, Ephemeral, OwnerKeys, ScanKey, ADDRESS_BYTES, REFUND_INDEX},
    claim::Branch,
    crypto::{self, Point},
    note::{Derivation, HoldRoot, MintLedger, Note},
    spend::{ChainProfile, Output, Proposal, TimedLock, TimedTerms},
};

fn keys(seed: u8) -> OwnerKeys {
    OwnerKeys::from_seed(&[seed; 32]).unwrap()
}

fn note(ledger: &mut MintLedger, owner: &OwnerKeys, denom: u64, position: usize) -> Note {
    let output = Ephemeral::random()
        .derive(&owner.address(), denom, Branch::Pay, position)
        .unwrap();
    ledger
        .mint(denom, output.blinded().unwrap())
        .unwrap()
        .open(ledger.keyset(), &output)
        .unwrap()
}

fn out(e: &Ephemeral, to: &OwnerKeys, denom: u64, branch: Branch, position: usize) -> Output {
    Output::from_derivation(&e.derive(&to.address(), denom, branch, position).unwrap()).unwrap()
}

/// Alice pays Bob 6 of her 8 (outputs 4 and 2 to Bob at positions 0 and 2,
/// change 2 to herself at position 1), refundable to her as 8 under a lock.
fn payment(ledger: &mut MintLedger, alice: &OwnerKeys, bob: &OwnerKeys) -> (Ephemeral, Proposal) {
    let notes = vec![note(ledger, alice, 8, 0)];
    let e = Ephemeral::random();
    let terms = TimedTerms::new(
        ChainProfile::Core30DefaultRegtest,
        [1; 32],
        1_000,
        3,
        [2; 32],
    )
    .unwrap();
    let lock = TimedLock::new(terms, vec![out(&e, alice, 8, Branch::Refund, 0)]).unwrap();
    let outputs = vec![
        out(&e, bob, 4, Branch::Pay, 0),
        out(&e, alice, 2, Branch::Pay, 1),
        out(&e, bob, 2, Branch::Pay, 2),
    ];
    let proposal = Proposal::new(ledger.keyset(), &notes, e.public(), outputs, Some(lock)).unwrap();
    (e, proposal)
}

fn found(scan: &ScanKey, proposal: &Proposal) -> Vec<(Branch, u32, u64)> {
    scan.receive(proposal)
        .unwrap()
        .into_iter()
        .map(|r| (r.branch, r.position, r.derivation.message().denomination()))
        .collect()
}

#[test]
fn payer_and_recipient_derive_the_same_receipt_secret_and_outputs() {
    let bob = keys(2);
    let e = Ephemeral::random();
    assert_eq!(
        e.shared(&bob.address()).unwrap(),
        bob.scan_key().shared(&e.public()).unwrap()
    );
    let paid = e.derive(&bob.address(), 4, Branch::Pay, 3).unwrap();
    let seen = bob
        .scan_key()
        .derive(&e.public(), 4, Branch::Pay, 3)
        .unwrap();
    assert_eq!(paid.message(), seen.message());
    assert_eq!(paid.blinded().unwrap(), seen.blinded().unwrap());
    assert_eq!(paid.unlink_commitment(), seen.unlink_commitment());
}

#[test]
fn the_receipt_secret_is_the_tagged_x_coordinate_of_the_shared_point() {
    let e = Ephemeral::from_scalar(Scalar::from(5u64)).unwrap();
    let scan = Scalar::from(7u64);
    let shared = Point::from_projective(&crypto::generator_mul(&Scalar::from(35u64)));
    let expected = crypto::tagged_hash("licash/ecdh", &[&shared.x_only()]);
    let address = ScanKey::new(scan, keys(1).address().spend(), keys(1).address().hold())
        .unwrap()
        .address();
    assert_eq!(e.shared(&address).unwrap(), expected);
}

#[test]
fn each_party_recognizes_exactly_its_outputs_in_both_branches() {
    let (alice, bob, carol) = (keys(1), keys(2), keys(3));
    let mut ledger = MintLedger::fresh(4).unwrap();
    let (_, proposal) = payment(&mut ledger, &alice, &bob);
    assert_eq!(
        found(bob.scan_key(), &proposal),
        vec![(Branch::Pay, 0, 4), (Branch::Pay, 2, 2)]
    );
    assert_eq!(
        found(alice.scan_key(), &proposal),
        vec![(Branch::Pay, 1, 2), (Branch::Refund, 0, 8)]
    );
    assert!(found(carol.scan_key(), &proposal).is_empty());
}

#[test]
fn an_output_at_another_position_or_branch_is_not_recognized() {
    let (alice, bob) = (keys(1), keys(2));
    let mut ledger = MintLedger::fresh(4).unwrap();
    let notes = vec![note(&mut ledger, &alice, 8, 0)];
    let e = Ephemeral::random();
    // Derived for position 1 and for the refund branch, placed at position 0
    // and 1 of the payment branch.
    let outputs = vec![
        out(&e, &bob, 4, Branch::Pay, 1),
        out(&e, &bob, 4, Branch::Refund, 1),
    ];
    let proposal = Proposal::new(ledger.keyset(), &notes, e.public(), outputs, None).unwrap();
    assert!(found(bob.scan_key(), &proposal).is_empty());
    // Under another ephemeral key nothing is recognized either.
    let other = Ephemeral::random();
    let outputs = vec![
        out(&other, &bob, 4, Branch::Pay, 0),
        out(&other, &bob, 4, Branch::Pay, 1),
    ];
    let proposal = Proposal::new(ledger.keyset(), &notes, e.public(), outputs, None).unwrap();
    assert!(found(bob.scan_key(), &proposal).is_empty());
}

#[test]
fn received_outputs_open_spend_and_hold_under_the_owner_keys() {
    let (alice, bob) = (keys(1), keys(2));
    let mut ledger = MintLedger::fresh(4).unwrap();
    let (_, proposal) = payment(&mut ledger, &alice, &bob);
    for received in bob.scan_key().receive(&proposal).unwrap() {
        let d = &received.derivation;
        let owner = d.ownership().owner_secret(bob.spend_secret()).unwrap();
        assert_eq!(&crypto::x_only_pub(&owner), d.message().owner());
        d.ownership()
            .verify(&bob.address().spend(), d.message())
            .unwrap();
        let hold = d.hold_key(bob.hold_root()).unwrap().to_scalar().unwrap();
        assert_eq!(&crypto::x_only_pub(&hold), d.message().hold());
        assert!(d.hold_key(alice.hold_root()).is_err());
        assert!(d
            .ownership()
            .owner_secret(alice.spend_secret())
            .is_ok_and(|s| { &crypto::x_only_pub(&s) != d.message().owner() }));
    }
}

#[test]
fn derivation_indices_put_refunds_in_their_own_space() {
    assert_eq!(index(Branch::Pay, 0).unwrap(), 0);
    assert_eq!(index(Branch::Pay, 7).unwrap(), 7);
    assert_eq!(index(Branch::Refund, 0).unwrap(), REFUND_INDEX);
    assert_eq!(index(Branch::Refund, 7).unwrap(), REFUND_INDEX + 7);
    assert!(index(Branch::Pay, REFUND_INDEX as usize).is_err());
    assert!(index(Branch::Refund, REFUND_INDEX as usize).is_err());
    // The same as deriving at the index directly.
    let bob = keys(2);
    let e = Ephemeral::random();
    let direct = Derivation::derive(
        4,
        bob.address().spend(),
        bob.address().hold(),
        &e.shared(&bob.address()).unwrap(),
        REFUND_INDEX + 2,
    )
    .unwrap();
    let derived = e.derive(&bob.address(), 4, Branch::Refund, 2).unwrap();
    assert_eq!(direct.message(), derived.message());
}

#[test]
fn seeds_give_distinct_deterministic_keys() {
    let a = keys(1).address();
    assert_eq!(a, keys(1).address());
    assert_ne!(a, keys(2).address());
    assert_ne!(a.spend(), a.scan());
    assert_ne!(a.spend(), a.hold());
    assert_ne!(a.scan(), a.hold());
    assert_eq!(a.hold(), HoldRoot::from_seed(&[1; 32]).unwrap().public());
    assert_eq!(
        crypto::x_only_pub(keys(1).spend_secret()),
        a.spend().x_only()
    );
    assert_eq!(format!("{:?}", keys(1)), "OwnerKeys(..)");
    assert_eq!(format!("{:?}", keys(1).scan_key()), "ScanKey(..)");
}

#[test]
fn addresses_roundtrip_and_refuse_malformed_keys() {
    let a = keys(1).address();
    let bytes = a.to_bytes();
    assert_eq!(bytes.len(), ADDRESS_BYTES);
    assert_eq!(Address::from_bytes(&bytes).unwrap(), a);
    assert!(Address::from_bytes(&bytes[..ADDRESS_BYTES - 1]).is_err());
    assert!(Address::from_bytes(&[bytes.as_slice(), &[0]].concat()).is_err());
    for key in 0..3 {
        for bad in [[0u8; 33], [255; 33]] {
            let mut b = bytes;
            b[33 * key..33 * (key + 1)].copy_from_slice(&bad);
            assert!(Address::from_bytes(&b).is_err());
        }
    }
    let json = serde_json::to_string(&a).unwrap();
    assert_eq!(json, format!("\"{}\"", hex::encode(bytes)));
    assert_eq!(serde_json::from_str::<Address>(&json).unwrap(), a);
    let bin = bincode::serialize(&a).unwrap();
    assert_eq!(bincode::deserialize::<Address>(&bin).unwrap(), a);
    let mut b = bytes;
    b[0] = 0x05;
    assert!(serde_json::from_str::<Address>(&format!("\"{}\"", hex::encode(b))).is_err());
}

#[test]
fn zero_secrets_are_refused() {
    assert!(Ephemeral::from_scalar(Scalar::ZERO).is_err());
    let hold = HoldRoot::from_seed(&[1; 32]).unwrap();
    assert!(OwnerKeys::from_parts(Scalar::ZERO, Scalar::ONE, hold.clone()).is_err());
    assert!(OwnerKeys::from_parts(Scalar::ONE, Scalar::ZERO, hold).is_err());
}
