use licash::{
    address::{Ephemeral, OwnerKeys},
    authorization::{Authorization, PreparedGrant, SignedProposal},
    claim::Branch,
    note::{Derivation, Evidence, IssuerKeys, Keyset, Note},
    spend::{Output, Proposal},
    unlink::{Nonce, Session},
};
use licash_api::{
    Code, Fee, Fees, Hex, Info, Offer, Offered, Problem, Release, Released, Unlink, Unlinked,
};
use serde::{de::DeserializeOwned, Serialize};

fn json<T: Serialize + DeserializeOwned>(value: &T) -> T {
    serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
}

struct Fixture {
    keys: IssuerKeys,
    keyset: Keyset,
    owner: OwnerKeys,
    input: (Note, Derivation),
    proposal: Proposal,
    outputs: Vec<Derivation>,
}

fn note(keys: &IssuerKeys, keyset: &Keyset, d: &Derivation) -> Note {
    let output = Output::from_derivation(d).unwrap();
    let proof = keys
        .prove_blinded(d.message().denomination(), output.blinded())
        .unwrap();
    Note::from_parts(
        keyset,
        *keyset.id(),
        d.message().clone(),
        proof.open_point(keyset, d).unwrap(),
        Evidence::Linked {
            dleq: proof.dleq(),
            blinding: *d.blinding(),
            commit: Nonce::new(keyset, &output).unwrap().commit().unwrap(),
        },
    )
    .unwrap()
}

fn fixture() -> Fixture {
    let keys = IssuerKeys::random(3).unwrap();
    let keyset = keys.keyset().unwrap();
    let owner = OwnerKeys::from_seed(&[1; 32]).unwrap();
    let d = Ephemeral::random()
        .derive(&owner.address(), 8, Branch::Pay, 0)
        .unwrap();
    let input = (note(&keys, &keyset, &d), d);
    let e = Ephemeral::random();
    let outputs: Vec<_> = (0..2)
        .map(|i| e.derive(&owner.address(), 4, Branch::Pay, i).unwrap())
        .collect();
    let proposal = Proposal::new(
        &keyset,
        std::slice::from_ref(&input.0),
        e.public(),
        outputs
            .iter()
            .map(|d| Output::from_derivation(d).unwrap())
            .collect(),
        None,
    )
    .unwrap();
    Fixture {
        keys,
        keyset,
        owner,
        input,
        proposal,
        outputs,
    }
}

#[test]
fn bodies_carry_the_canonical_bytes_through_json() {
    let f = fixture();
    let lookup = |id: &[u8; 32]| (id == f.keyset.id()).then_some(&f.keyset);
    let notes = vec![f.input.0.clone()];

    let fees = Fees {
        spend: Fee { base: 1, ppm: 2 },
    };
    let info = json(&Info::new(
        std::slice::from_ref(&f.keyset),
        f.owner.address(),
        fees,
    ));
    assert_eq!(info.keysets().unwrap(), vec![f.keyset.clone()]);
    assert_eq!(info.address, f.owner.address());
    assert_eq!(info.fees, fees);

    let offer = json(&Offer::new(&f.proposal, &notes));
    let (proposal, decoded) = offer.decode(lookup).unwrap();
    assert_eq!(proposal.id(), f.proposal.id());
    assert_eq!(decoded, notes);

    let prepared = PreparedGrant::new(&f.keys, &f.proposal).unwrap();
    let grant = prepared.offer();
    assert_eq!(
        json(&Offered::new(&grant).unwrap()).decode().unwrap(),
        grant
    );

    let secret = f
        .input
        .1
        .ownership()
        .owner_secret(f.owner.spend_secret())
        .unwrap();
    let signed = SignedProposal::sign(
        &f.keyset,
        f.proposal.clone(),
        &notes,
        Authorization::Granted(Box::new(grant)),
        &[secret],
    )
    .unwrap();
    let (back, decoded) = json(&Release::new(&signed, &notes)).decode(lookup).unwrap();
    assert_eq!(back.to_bytes(), signed.to_bytes());
    assert_eq!(decoded, notes);
    let unlock = prepared.release(&f.keyset, &notes, &signed).unwrap();
    assert_eq!(json(&Released::new(&unlock)).decode().unwrap(), unlock);

    let paid = signed
        .open_granted_output(&f.keyset, &notes, 0, &f.outputs[0], &unlock)
        .unwrap();
    let session = Session::start(&f.keyset, &paid, &f.outputs[0]).unwrap();
    let request = json(&Unlink::new(session.request())).decode().unwrap();
    assert_eq!(&request, session.request());
    let nonce = prepared.unlink_nonces().remove(0);
    let answer = nonce.answer(&f.keys, &request).unwrap().answer();
    let answer = json(&Unlinked::new(&answer)).decode().unwrap();
    session.finish(&f.keyset, &answer).unwrap();
}

#[test]
fn malformed_and_foreign_bodies_are_problems() {
    let f = fixture();
    let notes = vec![f.input.0.clone()];
    let none = |_: &[u8; 32]| None;
    assert_eq!(
        Offer::new(&f.proposal, &notes)
            .decode(none)
            .unwrap_err()
            .code,
        Code::UnknownKeyset
    );
    let lookup = |id: &[u8; 32]| (id == f.keyset.id()).then_some(&f.keyset);
    let mut offer = Offer::new(&f.proposal, &notes);
    offer.proposal.0.pop();
    assert_eq!(offer.decode(lookup).unwrap_err().code, Code::Invalid);
    let mut offer = Offer::new(&f.proposal, &notes);
    offer.notes[0].0[20] ^= 1;
    assert_eq!(offer.decode(lookup).unwrap_err().code, Code::Invalid);
    assert!(serde_json::from_str::<Unlink>(r#"{"request":"0g"}"#).is_err());
    assert!(serde_json::from_str::<Unlink>(r#"{"request":7}"#).is_err());
    let short = Released {
        unlock: Hex(vec![0; 31]),
    };
    assert_eq!(short.decode().unwrap_err().code, Code::Invalid);
    let release = Release {
        keyset: Hex(vec![0; 31]),
        signed: Hex(vec![]),
        notes: vec![],
    };
    assert_eq!(release.decode(lookup).unwrap_err().code, Code::Invalid);
    let unlinked = Unlinked {
        answer: Hex(vec![0xff; 32]),
    };
    assert_eq!(unlinked.decode().unwrap_err().code, Code::Invalid);
}

#[test]
fn problems_and_their_statuses_are_the_wire_contract() {
    let problem = Problem::new(Code::Spent, "an input is already spent");
    assert_eq!(
        serde_json::to_string(&problem).unwrap(),
        r#"{"code":"spent","message":"an input is already spent"}"#
    );
    assert_eq!(json(&problem), problem);
    for (code, name, status) in [
        (Code::Invalid, "invalid", 400),
        (Code::UnknownKeyset, "unknown_keyset", 404),
        (Code::UnknownOffer, "unknown_offer", 404),
        (Code::NotIssued, "not_issued", 404),
        (Code::Timed, "timed", 409),
        (Code::Spent, "spent", 409),
        (Code::Reissued, "reissued", 409),
        (Code::Fee, "fee", 409),
        (Code::Unlinked, "unlinked", 409),
        (Code::Expired, "expired", 409),
        (Code::Unavailable, "unavailable", 503),
    ] {
        assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{name}\""));
        assert_eq!(code.status(), status);
    }
    assert_eq!(licash_api::OFFER, "/v1/offer");
    assert_eq!(format!("{:?}", Hex(vec![1, 2])), "Hex(2 bytes)");
}
