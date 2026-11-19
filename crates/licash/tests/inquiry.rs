use k256::Scalar;
use licash::{
    crypto::{self, Point, ScalarBytes},
    hold::Context,
    inquiry::{self, WINDOW},
    note::{Derivation, HoldRoot, Message},
};

fn note(index: u32) -> (Message, ScalarBytes) {
    let root = HoldRoot::from_seed(&[7; 32]).unwrap();
    let spend = Point::from_projective(&crypto::generator_mul(&Scalar::from(17u64)));
    let d = Derivation::derive(4, spend, root.public(), &[23; 32], index).unwrap();
    (d.message().clone(), d.hold_key(&root).unwrap())
}

const NOW: u64 = 1_800_000_000;

#[test]
fn only_the_hold_key_of_a_note_can_ask_about_it() {
    let keyset = [5; 32];
    let (message, key) = note(0);
    let sig = inquiry::sign(&keyset, NOW, &message, &key).unwrap();
    inquiry::verify(&keyset, NOW, &message, &sig, NOW).unwrap();

    // Another note's key, or an owner key, does not sign for it.
    let (_, other) = note(1);
    assert!(inquiry::sign(&keyset, NOW, &message, &other).is_err());
    let (elsewhere, _) = note(1);
    assert!(inquiry::verify(&keyset, NOW, &elsewhere, &sig, NOW).is_err());
}

#[test]
fn an_inquiry_is_for_its_keyset_its_note_and_its_time_alone() {
    let keyset = [5; 32];
    let (message, key) = note(0);
    let sig = inquiry::sign(&keyset, NOW, &message, &key).unwrap();
    assert!(inquiry::verify(&[6; 32], NOW, &message, &sig, NOW).is_err());
    assert!(inquiry::verify(&keyset, NOW + 1, &message, &sig, NOW).is_err());
    // Within its window either side of now, and not past it.
    inquiry::verify(&keyset, NOW, &message, &sig, NOW + WINDOW).unwrap();
    inquiry::verify(&keyset, NOW, &message, &sig, NOW - WINDOW).unwrap();
    assert!(inquiry::verify(&keyset, NOW, &message, &sig, NOW + WINDOW + 1).is_err());
    assert!(inquiry::verify(&keyset, NOW, &message, &sig, NOW - WINDOW - 1).is_err());
}

#[test]
fn an_inquiry_is_no_authorization_the_hub_could_cite() {
    let keyset = [5; 32];
    let (message, key) = note(0);
    let sig = inquiry::sign(&keyset, NOW, &message, &key).unwrap();
    // Not a hold authorization of any claim: whatever its context says, the
    // hub's `SecondClaim` evidence does not verify with it.
    let context = Context {
        base: keyset,
        deposit: [0; 36],
        amount: u32::try_from(NOW % 1_000_000).unwrap(),
        claim_digest: [0; 32],
    };
    assert!(
        crypto::schnorr_verify(message.hold(), &context.signing_digest(&message), &sig).is_err()
    );
    // Nor is a hold authorization an inquiry.
    let held = crypto::schnorr_sign(
        key.to_nonzero().unwrap().as_ref(),
        &context.signing_digest(&message),
    )
    .unwrap();
    assert!(inquiry::verify(&keyset, NOW, &message, &held, NOW).is_err());
}
