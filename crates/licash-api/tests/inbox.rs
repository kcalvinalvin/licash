use licash::{
    address::{Ephemeral, OwnerKeys},
    spend::ProposalId,
};
use licash_api::{
    inbox::{self, Keep, Receipt},
    Code, Hex, Release, Released,
};

fn release() -> Release {
    Release {
        keyset: Hex(vec![1; 32]),
        signed: Hex(vec![2; 40]),
        notes: vec![Hex(vec![3; 10])],
    }
}

fn keep() -> Keep {
    Keep {
        tag: Hex(vec![4; 4]),
        key: Hex(vec![5; 32]),
        commitment: Hex(vec![6; 32]),
    }
}

fn text(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap()
}

#[test]
fn a_release_keeping_its_payment_is_a_plain_release_to_hubs_keeping_none() {
    let plain = text(&release());
    let keeping = inbox::Release {
        release: release(),
        keep: Some(keep()),
    };
    // A hub keeping none reads the release in it.
    let read: Release = serde_json::from_str(&text(&keeping)).unwrap();
    assert_eq!(text(&read), plain);
    // Asking nothing kept, it is the plain release.
    let unkept = inbox::Release {
        release: release(),
        keep: None,
    };
    assert_eq!(text(&unkept), plain);
    // A plain release, from an older wallet, asks nothing kept.
    let asked: inbox::Release = serde_json::from_str(&plain).unwrap();
    assert_eq!(asked.keep, None);
    let asked: inbox::Release = serde_json::from_str(&text(&keeping)).unwrap();
    assert_eq!(asked.keep, Some(keep()));
}

#[test]
fn an_answer_says_kept_only_if_the_hub_keeps_the_payment() {
    let released = Released {
        unlock: Hex(vec![7; 32]),
    };
    let plain = text(&released);
    let answer: inbox::Released = serde_json::from_str(&plain).unwrap();
    assert!(!answer.kept, "an older hub keeps nothing");
    let kept = inbox::Released {
        released: released.clone(),
        kept: true,
    };
    let read: Released = serde_json::from_str(&text(&kept)).unwrap();
    assert_eq!(read.unlock, released.unlock);
    assert!(
        serde_json::from_str::<inbox::Released>(&text(&kept))
            .unwrap()
            .kept
    );
    let unkept = inbox::Released {
        released,
        kept: false,
    };
    assert_eq!(text(&unkept), plain);
}

#[test]
fn payer_and_payee_derive_the_same_tag_key_and_ack_and_the_hub_only_a_commitment() {
    let payee = OwnerKeys::from_seed(&[9; 32]).unwrap();
    let e = Ephemeral::random();
    let payer = e.shared(&payee.address()).unwrap();
    let shared = payee.scan_key().shared(&e.public()).unwrap();
    assert_eq!(payer, shared);
    let id = ProposalId::from_bytes([8; 32]);
    let (keep, ack) = Keep::new(&payer, &id);
    let (tag, key, commitment) = keep.decode().unwrap();
    assert_eq!(tag, inbox::tag(&shared));
    assert_eq!(key, inbox::key(&shared));
    assert_eq!(ack, inbox::ack(&shared, &id));
    assert_eq!(commitment, inbox::commitment(&ack));
    // Each its own: the key the hub holds tells neither ack nor secret.
    assert_ne!(key, ack);
    assert_ne!(key, shared);
    assert_ne!(commitment, ack);
    assert_ne!(tag, key[..4]);
    // An ack is of one payment.
    let other = ProposalId::from_bytes([7; 32]);
    assert_ne!(inbox::ack(&shared, &other), ack);
    // And of one payee.
    let stranger = OwnerKeys::from_seed(&[10; 32]).unwrap();
    let theirs = stranger.scan_key().shared(&e.public()).unwrap();
    assert_ne!(inbox::key(&theirs), key);
}

#[test]
fn malformed_asks_are_invalid() {
    let mut short = keep();
    short.tag.0.pop();
    assert_eq!(short.decode().unwrap_err().code, Code::Invalid);
    let mut long = keep();
    long.key.0.push(0);
    assert_eq!(long.decode().unwrap_err().code, Code::Invalid);
    let keys = vec![Hex(vec![1; 32]); inbox::MAX_ASKED];
    assert_eq!(inbox::arrays::<32>(keys.iter(), "key").unwrap().len(), 100);
    let many = vec![Hex(vec![1; 32]); inbox::MAX_ASKED + 1];
    let problem = inbox::arrays::<32>(many.iter(), "key").unwrap_err();
    assert_eq!(problem.code, Code::Invalid);
    let short = [Hex(vec![1; 31])];
    let problem = inbox::arrays::<32>(short.iter(), "key").unwrap_err();
    assert_eq!(problem.code, Code::Invalid);
    let entry = inbox::Entry {
        seq: 1,
        ephemeral: Hex(vec![2; 32]),
        tag: Hex(vec![1; 4]),
    };
    assert_eq!(entry.decode().unwrap_err().code, Code::Invalid);
}

#[test]
fn a_payment_kept_is_one_at_the_hub_its_payee_takes_it_at() {
    let receipt = Receipt {
        release: release(),
        unlock: Hex(vec![7; 32]),
    };
    let read: Receipt = serde_json::from_str(&text(&receipt)).unwrap();
    let payment = read.at("http://hub.example");
    assert_eq!(payment.hub, "http://hub.example");
    assert_eq!(text(&payment.release), text(&release()));
    assert_eq!(payment.unlock, Hex(vec![7; 32]));
    assert_eq!(inbox::feed(42), "/v1/inbox?after=42");
}
