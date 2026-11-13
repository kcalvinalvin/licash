use k256::Scalar;
use licash::{
    authorization::{Authorization, Grant, PreparedGrant, SignedProposal},
    crypto::{self, Point, Signature},
    note::Evidence,
    note::{Derivation, HoldRoot, IssuerKeys, Keyset, Note},
    spend::{ChainProfile, Output, Proposal, TimedLock, TimedTerms},
    unlink,
};

fn point(key: u64) -> Point {
    Point::from_projective(&crypto::generator_mul(&Scalar::from(key)))
}

fn derive_amount(denom: u64, index: u32) -> Derivation {
    Derivation::derive(
        denom,
        point(17),
        HoldRoot::from_seed(&[7; 32]).unwrap().public(),
        &[23; 32],
        index,
    )
    .unwrap()
}

fn derive(index: u32) -> Derivation {
    derive_amount(4, index)
}

fn owner(index: u32) -> Scalar {
    Scalar::from(17u64)
        + licash::note::derive_output(&point(17), &[23; 32], index)
            .unwrap()
            .tweak
            .to_scalar()
            .unwrap()
}

// Equation fixtures only, no ledger or promised unlink service. The public
// blinded proof is enough to construct a valid issuer-authenticated witness.
fn input(keys: &IssuerKeys, keyset: &Keyset, index: u32) -> Note {
    let d = derive(index);
    let b = d.blinded().unwrap();
    let proof = keys.prove_blinded(4, b).unwrap();
    Note::from_parts(
        keyset,
        *keyset.id(),
        d.message().clone(),
        proof.open_point(keyset, &d).unwrap(),
        Evidence::Linked {
            dleq: proof.dleq(),
            blinding: *d.blinding(),
            commit: crypto::dleq_commit(&Scalar::from(71u64), &b.to_projective().unwrap()),
        },
    )
    .unwrap()
}

fn fixture() -> (IssuerKeys, Keyset, Vec<Note>, Proposal) {
    let keys = IssuerKeys::random(3).unwrap();
    let keyset = keys.keyset().unwrap();
    let notes = vec![input(&keys, &keyset, 0), input(&keys, &keyset, 1)];
    let proposal = Proposal::new(
        &keyset,
        &notes,
        point(19),
        vec![
            Output::from_derivation(&derive(10)).unwrap(),
            Output::from_derivation(&derive(11)).unwrap(),
        ],
        None,
    )
    .unwrap();
    (keys, keyset, notes, proposal)
}

fn owners() -> [Scalar; 2] {
    [owner(0), owner(1)]
}

#[test]
fn plain_owner_authorization_checks_every_input_and_a_distinct_domain() {
    let (_, keyset, notes, proposal) = fixture();
    let signed = SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Plain,
        &owners(),
    )
    .unwrap();
    assert_eq!(signed.verify(&keyset, &notes).unwrap().len(), 2);
    let digest = crypto::tagged_hash("licash/authorize-plain/v2", &[proposal.id().as_bytes()]);
    for (message, sig) in proposal.inputs().iter().zip(signed.signatures()) {
        crypto::schnorr_verify(message.owner(), &digest, sig).unwrap();
        assert!(crypto::schnorr_verify(message.owner(), proposal.id().as_bytes(), sig).is_err());
    }
    assert!(
        SignedProposal::sign(&keyset, proposal.clone(), &notes, Authorization::Plain, &[]).is_err()
    );
    assert!(SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Plain,
        &[owner(1), owner(0)]
    )
    .is_err());
    assert!(SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Plain,
        &[Scalar::ZERO, owner(1)]
    )
    .is_err());
    let hold = HoldRoot::from_seed(&[7; 32]).unwrap();
    let hold_keys = [
        derive(0).hold_key(&hold).unwrap().to_scalar().unwrap(),
        derive(1).hold_key(&hold).unwrap().to_scalar().unwrap(),
    ];
    assert!(SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Plain,
        &hold_keys
    )
    .is_err());
    for sigs in [
        vec![],
        vec![signed.signatures()[0]],
        vec![Signature([0; 64]); 2],
        vec![signed.signatures()[1], signed.signatures()[0]],
    ] {
        assert!(SignedProposal::from_parts(
            &keyset,
            proposal.clone(),
            &notes,
            Authorization::Plain,
            sigs
        )
        .is_err());
    }
    assert_eq!(format!("{signed:?}"), "SignedProposal(..)");
}

#[test]
fn a_complete_grant_opens_each_coin_with_one_shared_unlock_after_owner_authorization() {
    let (keys, keyset, notes, proposal) = fixture();
    let prepared = PreparedGrant::new(&keys, &proposal).unwrap();
    let grant = prepared.offer();
    grant.verify(&keyset, &proposal).unwrap();
    assert_eq!(prepared.clone().offer(), grant);
    let signed = SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Granted(Box::new(grant.clone())),
        &owners(),
    )
    .unwrap();
    assert_eq!(signed.proposal().id(), proposal.id());
    let unlock = prepared.release(&keyset, &notes, &signed).unwrap();
    assert_eq!(
        prepared.clone().release(&keyset, &notes, &signed).unwrap(),
        unlock
    );
    let digest = crypto::tagged_hash(
        "licash/authorize-grant/v2",
        &[proposal.id().as_bytes(), &grant.commitment().unwrap()],
    );
    for (message, sig) in proposal.inputs().iter().zip(signed.signatures()) {
        crypto::schnorr_verify(message.owner(), &digest, sig).unwrap();
    }
    for (index, derived) in [derive(10), derive(11)].iter().enumerate() {
        let note = signed
            .open_granted_output(&keyset, &notes, index, derived, &unlock)
            .unwrap();
        note.verify(&keyset).unwrap();
        assert_eq!(note.message(), derived.message());
        // An opened packet is an ordinary issuance under the plain key.
        assert!(matches!(note.evidence(), Evidence::Linked { .. }));
    }
    grant.verify_unlock(&keyset, &unlock).unwrap();
    assert_eq!(
        grant.lock,
        licash::authorization::unlock_hash(&proposal.id(), &unlock)
    );
    for wrong in [[0; 32], [0xff; 32], [83; 32]] {
        assert!(grant.verify_unlock(&keyset, &wrong).is_err());
        assert!(signed
            .open_granted_output(&keyset, &notes, 0, &derive(10), &wrong)
            .is_err());
    }
    assert!(signed
        .open_granted_output(&keyset, &notes, 0, &derive(11), &unlock)
        .is_err());
    assert!(signed
        .open_granted_output(&keyset, &notes, 2, &derive(10), &unlock)
        .is_err());
    assert_eq!(format!("{prepared:?}"), "PreparedGrant(..)");
    assert_eq!(format!("{grant:?}"), "Grant(..)");
    assert_eq!(format!("{:?}", grant.packets[0]), "GrantPacket(..)");
}

#[test]
fn a_derived_grant_rebuilds_from_its_secrets_and_releases_against_the_stored_offer() {
    let (keys, keyset, notes, proposal) = fixture();
    let (unlock, nonce_secret) = ([41; 32], [43; 32]);
    let prepared = PreparedGrant::derive(&keys, &proposal, unlock, &nonce_secret).unwrap();
    let grant = prepared.offer();
    grant.verify(&keyset, &proposal).unwrap();
    // The secrets fix the lock and the unlink commitments; proofs and the
    // issuer signature are fresh, so an issuer keeps the offer it sent.
    let again = PreparedGrant::derive(&keys, &proposal, unlock, &nonce_secret)
        .unwrap()
        .offer();
    assert_eq!(again.lock, grant.lock);
    for (a, b) in again.packets.iter().zip(&grant.packets) {
        assert_eq!(
            (a.denom, a.blinded, a.commit),
            (b.denom, b.blinded, b.commit)
        );
    }
    assert_ne!(again, grant);
    let other = PreparedGrant::derive(&keys, &proposal, unlock, &[44; 32])
        .unwrap()
        .offer();
    assert_eq!(other.lock, grant.lock);
    assert_ne!(other.packets[0].commit, grant.packets[0].commit);
    for (nonce, output) in prepared.unlink_nonces().iter().zip(proposal.outputs()) {
        let derived = unlink::Nonce::derive(&nonce_secret, &keyset, output).unwrap();
        assert_eq!(nonce.commit().unwrap(), derived.commit().unwrap());
    }
    let signed = SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Granted(Box::new(grant.clone())),
        &owners(),
    )
    .unwrap();
    grant
        .verify_release(&keyset, &notes, &signed, &unlock)
        .unwrap();
    assert_eq!(prepared.release(&keyset, &notes, &signed).unwrap(), unlock);
    let note = signed
        .open_granted_output(&keyset, &notes, 0, &derive(10), &unlock)
        .unwrap();
    note.verify(&keyset).unwrap();
    assert!(grant
        .verify_release(&keyset, &notes, &signed, &[42; 32])
        .is_err());
    assert!(again
        .verify_release(&keyset, &notes, &signed, &unlock)
        .is_err());
    assert!(grant
        .verify_release(&keyset, &notes[..1], &signed, &unlock)
        .is_err());
    let plain = SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Plain,
        &owners(),
    )
    .unwrap();
    assert!(grant
        .verify_release(&keyset, &notes, &plain, &unlock)
        .is_err());
    let foreign = IssuerKeys::random(3).unwrap();
    assert!(PreparedGrant::derive(&foreign, &proposal, unlock, &nonce_secret).is_err());
}

#[test]
fn one_unlock_opens_mixed_denomination_pay_and_change_packets() {
    let (keys, keyset, notes, _) = fixture();
    let outputs = [
        derive_amount(4, 10),
        derive_amount(2, 11),
        derive_amount(2, 12),
    ];
    let proposal = Proposal::new(
        &keyset,
        &notes,
        point(19),
        outputs
            .iter()
            .map(|d| Output::from_derivation(d).unwrap())
            .collect(),
        None,
    )
    .unwrap();
    let prepared = PreparedGrant::new(&keys, &proposal).unwrap();
    let signed = SignedProposal::sign(
        &keyset,
        proposal,
        &notes,
        Authorization::Granted(Box::new(prepared.offer())),
        &owners(),
    )
    .unwrap();
    let unlock = prepared.release(&keyset, &notes, &signed).unwrap();
    for (index, derived) in outputs.iter().enumerate() {
        let note = signed
            .open_granted_output(&keyset, &notes, index, derived, &unlock)
            .unwrap();
        note.verify(&keyset).unwrap();
        assert_eq!(note.message(), derived.message());
    }
    assert!(signed
        .open_granted_output(&keyset, &notes, 0, &derive_amount(2, 10), &unlock)
        .is_err());
}

#[test]
fn grant_stripping_replacement_and_cross_mode_signatures_are_rejected() {
    let (keys, keyset, notes, proposal) = fixture();
    let prepared = PreparedGrant::new(&keys, &proposal).unwrap();
    let grant = prepared.offer();
    let auth = Authorization::Granted(Box::new(grant.clone()));
    let granted =
        SignedProposal::sign(&keyset, proposal.clone(), &notes, auth.clone(), &owners()).unwrap();
    let plain = SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Plain,
        &owners(),
    )
    .unwrap();
    assert!(prepared.release(&keyset, &notes, &plain).is_err());
    assert!(SignedProposal::from_parts(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Plain,
        granted.signatures().to_vec()
    )
    .is_err());
    assert!(SignedProposal::from_parts(
        &keyset,
        proposal.clone(),
        &notes,
        auth,
        plain.signatures().to_vec()
    )
    .is_err());
    let second = PreparedGrant::new(&keys, &proposal).unwrap();
    let other = second.offer();
    assert!(second.release(&keyset, &notes, &granted).is_err());
    let other_signed = SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Granted(Box::new(other.clone())),
        &owners(),
    )
    .unwrap();
    let other_unlock = second.release(&keyset, &notes, &other_signed).unwrap();
    assert!(granted
        .open_granted_output(&keyset, &notes, 0, &derive(10), &other_unlock)
        .is_err());
    assert!(SignedProposal::from_parts(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Granted(Box::new(other)),
        granted.signatures().to_vec()
    )
    .is_err());
    assert!(plain
        .open_granted_output(&keyset, &notes, 0, &derive(10), &other_unlock)
        .is_err());
}

#[test]
fn the_issuer_and_owner_commitments_bind_every_packet_field_order_count_and_signature() {
    let (keys, keyset, notes, proposal) = fixture();
    let original = PreparedGrant::new(&keys, &proposal).unwrap().offer();
    let signed = SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Granted(Box::new(original.clone())),
        &owners(),
    )
    .unwrap();
    for field in 0..13 {
        let mut grant = original.clone();
        match field {
            0 => grant.keyset[0] ^= 1,
            1 => grant.lock[0] ^= 1,
            2 => {
                grant.packets.pop();
            }
            3 => grant.packets.swap(0, 1),
            4 => grant.packets[0].denom = 2,
            5 => grant.packets[0].blinded = point(79),
            6 => grant.packets[0].sealed[0] ^= 1,
            7 => grant.packets[0].sealed[40] ^= 1,
            8 => grant.packets[0].sealed[96] ^= 1,
            9 => grant.packets[0].commit.r1 = point(79),
            10 => grant.packets[0].commit.r2 = point(79),
            11 => grant.sig.0[0] ^= 1,
            _ => {
                grant.proposal = Proposal::new(
                    &keyset,
                    &notes,
                    point(89),
                    proposal.outputs().to_vec(),
                    None,
                )
                .unwrap()
                .id()
            }
        }
        assert_ne!(grant.commitment().unwrap(), original.commitment().unwrap());
        assert!(grant.verify(&keyset, &proposal).is_err(), "field {field}");
        assert!(SignedProposal::from_parts(
            &keyset,
            proposal.clone(),
            &notes,
            Authorization::Granted(Box::new(grant)),
            signed.signatures().to_vec()
        )
        .is_err());
    }
}

#[test]
fn authenticated_keysets_notes_and_the_exact_source_are_checked_again() {
    let (keys, keyset, notes, proposal) = fixture();
    let prepared = PreparedGrant::new(&keys, &proposal).unwrap();
    let signed = SignedProposal::sign(
        &keyset,
        proposal.clone(),
        &notes,
        Authorization::Granted(Box::new(prepared.offer())),
        &owners(),
    )
    .unwrap();
    let other_keys = IssuerKeys::random(3).unwrap();
    let other_set = other_keys.keyset().unwrap();
    let other_notes = vec![
        input(&other_keys, &other_set, 0),
        input(&other_keys, &other_set, 1),
    ];
    assert!(PreparedGrant::new(&other_keys, &proposal).is_err());
    assert!(prepared.offer().verify(&other_set, &proposal).is_err());
    assert!(signed.verify(&other_set, &other_notes).is_err());
    assert!(prepared.release(&other_set, &other_notes, &signed).is_err());
    assert!(signed.verify(&keyset, &other_notes).is_err());
    assert!(signed.verify(&keyset, &notes[..1]).is_err());
    assert!(prepared.release(&keyset, &notes[..1], &signed).is_err());
    let changed = Proposal::new(
        &keyset,
        &notes,
        point(97),
        proposal.outputs().to_vec(),
        None,
    )
    .unwrap();
    assert!(SignedProposal::from_parts(
        &keyset,
        changed,
        &notes,
        signed.authorization().clone(),
        signed.signatures().to_vec()
    )
    .is_err());
}

#[test]
fn output_unlink_authority_is_checked_when_opening_not_inferred_from_a_curve_target() {
    let (keys, keyset, notes, proposal) = fixture();
    let mut outputs = proposal.outputs().to_vec();
    outputs[0] = Output::new(4, outputs[0].blinded(), [101; 32]).unwrap();
    let changed = Proposal::new(&keyset, &notes, point(19), outputs, None).unwrap();
    let prepared = PreparedGrant::new(&keys, &changed).unwrap();
    let signed = SignedProposal::sign(
        &keyset,
        changed,
        &notes,
        Authorization::Granted(Box::new(prepared.offer())),
        &owners(),
    )
    .unwrap();
    assert!(signed
        .open_granted_output(
            &keyset,
            &notes,
            0,
            &derive(10),
            &prepared.release(&keyset, &notes, &signed).unwrap()
        )
        .is_err());
}

#[test]
fn timed_intents_allow_plain_owner_authorization_but_not_untimed_grants() {
    let (keys, keyset, notes, proposal) = fixture();
    let terms = TimedTerms::new(
        ChainProfile::Core30DefaultRegtest,
        [31; 32],
        1_000,
        3,
        [41; 32],
    )
    .unwrap();
    let lock = TimedLock::new(
        terms,
        vec![
            Output::from_derivation(&derive(12)).unwrap(),
            Output::from_derivation(&derive(13)).unwrap(),
        ],
    )
    .unwrap();
    let timed = Proposal::new(
        &keyset,
        &notes,
        point(19),
        proposal.outputs().to_vec(),
        Some(lock),
    )
    .unwrap();
    assert!(PreparedGrant::new(&keys, &timed).is_err());
    let signed = SignedProposal::sign(
        &keyset,
        timed.clone(),
        &notes,
        Authorization::Plain,
        &owners(),
    )
    .unwrap();
    signed.verify(&keyset, &notes).unwrap();
    let grant = PreparedGrant::new(&keys, &proposal).unwrap().offer();
    assert!(SignedProposal::sign(
        &keyset,
        timed,
        &notes,
        Authorization::Granted(Box::new(grant)),
        &owners()
    )
    .is_err());
}

#[test]
fn thirty_two_separate_grants_keep_independent_unlocks_and_valid_coins() {
    let (keys, keyset, notes, _) = fixture();
    let mut unlocks = std::collections::BTreeSet::new();
    for i in 0..32 {
        let outputs = [derive(100 + 2 * i), derive(101 + 2 * i)];
        let proposal = Proposal::new(
            &keyset,
            &notes,
            point(19),
            outputs
                .iter()
                .map(|d| Output::from_derivation(d).unwrap())
                .collect(),
            None,
        )
        .unwrap();
        let prepared = PreparedGrant::new(&keys, &proposal).unwrap();
        assert!(unlocks.insert(prepared.offer().lock));
        let signed = SignedProposal::sign(
            &keyset,
            proposal,
            &notes,
            Authorization::Granted(Box::new(prepared.offer())),
            &owners(),
        )
        .unwrap();
        signed
            .open_granted_output(
                &keyset,
                &notes,
                0,
                &outputs[0],
                &prepared.release(&keyset, &notes, &signed).unwrap(),
            )
            .unwrap()
            .verify(&keyset)
            .unwrap();
    }
    assert_eq!(unlocks.len(), 32);
    // Deliberately no admission/spent ledger: these are independent equation
    // cases, not acceptance of 32 payments consuming the same inputs.
}

#[test]
fn malformed_public_artifacts_are_not_owner_authorization() {
    let (keys, keyset, notes, proposal) = fixture();
    let original = PreparedGrant::new(&keys, &proposal).unwrap().offer();
    let mut empty = original.clone();
    empty.packets.clear();
    assert!(empty.commitment().is_err());
    assert!(empty.verify(&keyset, &proposal).is_err());
    for field in 0..5 {
        let mut grant: Grant = original.clone();
        match field {
            0 => grant.lock = [0; 32],
            1 => grant.packets[0].sealed.clear(),
            2 => grant.packets[0].commit.r1 = Point([0; 33]),
            3 => grant.packets[0].commit.r2 = Point([0; 33]),
            _ => grant.packets[0].sealed.push(0),
        }
        assert!(SignedProposal::sign(
            &keyset,
            proposal.clone(),
            &notes,
            Authorization::Granted(Box::new(grant)),
            &owners()
        )
        .is_err());
    }
}
