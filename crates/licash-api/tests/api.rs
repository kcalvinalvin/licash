use licash::{
    address::{Ephemeral, OwnerKeys},
    authorization::{Authorization, PreparedGrant, SignedProposal},
    claim::Branch,
    note::{Derivation, Evidence, IssuerKeys, Keyset, Note},
    source::Certificate,
    spend::{lock_hash, ChainProfile, Output, Proposal, ProposalId, TimedLock, TimedTerms},
    unlink::{Nonce, Session},
};
use licash_api::{
    Check, Code, Fee, Fees, Hex, Info, Issued, Lock, LockState, Locked, Offer, Offered, Payment,
    Problem, Refund, Release, Released, Settle, Settled, Unlink, Unlinked,
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
        ..Fees::default()
    };
    let info = json(&Info::new(
        std::slice::from_ref(&f.keyset),
        f.owner.address(),
        fees,
        None,
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

    // The payee's view of it.
    let payment = json(&Payment::new(
        "http://hub.example",
        &signed,
        &notes,
        &unlock,
    ));
    assert_eq!(payment.hub, "http://hub.example");
    let (paid, paid_notes, paid_unlock) = payment.decode(lookup).unwrap();
    assert_eq!(paid.to_bytes(), signed.to_bytes());
    assert_eq!((paid_notes, paid_unlock), (notes.clone(), unlock));
    let mut wrong = payment.clone();
    wrong.unlock = Hex(vec![0; 32]);
    assert_eq!(wrong.decode(lookup).unwrap_err().code, Code::Invalid);
    let none = |_: &[u8; 32]| None;
    assert_eq!(payment.decode(none).unwrap_err().code, Code::UnknownKeyset);
    let fields = serde_json::to_value(&payment).unwrap();
    let mut keys: Vec<_> = fields.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["hub", "keyset", "notes", "signed", "unlock"]);

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

    let y = f.input.0.message().identifier().unwrap();
    let check = json(&Check::new(&[y]));
    assert_eq!(
        serde_json::to_string(&check).unwrap(),
        format!(r#"{{"identifiers":["{}"]}}"#, hex::encode(y.0))
    );
    assert_eq!(check.decode().unwrap(), vec![y]);
}

#[test]
fn a_check_asks_of_whole_identifiers_and_a_bounded_number() {
    let y = Hex(vec![2; 33]);
    let check = Check {
        identifiers: vec![y.clone(); licash_api::MAX_CHECK],
    };
    assert_eq!(check.decode().unwrap().len(), licash_api::MAX_CHECK);
    let many = Check {
        identifiers: vec![y; licash_api::MAX_CHECK + 1],
    };
    assert_eq!(many.decode().unwrap_err().code, Code::Invalid);
    let short = Check {
        identifiers: vec![Hex(vec![2; 32])],
    };
    assert_eq!(short.decode().unwrap_err().code, Code::Invalid);
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
        (Code::UnknownLock, "unknown_lock", 404),
        (Code::NotALock, "not_a_lock", 409),
        (Code::Checkpoint, "checkpoint", 409),
        (Code::Timing, "timing", 409),
        (Code::Preimage, "preimage", 409),
        (Code::Settled, "settled", 409),
        (Code::LockExpired, "lock_expired", 409),
        (Code::Unexpired, "unexpired", 409),
        (Code::Activated, "activated", 409),
        (Code::Unavailable, "unavailable", 503),
        (Code::Forward, "forward", 409),
        (Code::Declined, "declined", 409),
    ] {
        assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{name}\""));
        assert_eq!(code.status(), status);
    }
    assert_eq!(licash_api::OFFER, "/v1/offer");
    let id = ProposalId::from_bytes([0xab; 32]);
    let path = licash_api::lock_state(&id);
    assert_eq!(path, format!("/v1/lock/{}", "ab".repeat(32)));
    assert_eq!(licash_api::lock_id(&path[9..]).unwrap(), id);
    assert_eq!(licash_api::lock_id("ab").unwrap_err().code, Code::Invalid);
    assert_eq!(licash_api::lock_id("zz").unwrap_err().code, Code::Invalid);
    // Info and fees of a hub that predates locks.
    let fees: Fees = serde_json::from_str(r#"{"spend":{"base":1,"ppm":0}}"#).unwrap();
    assert_eq!((fees.lock, fees.refund), (Fee::default(), Fee::default()));
    assert_eq!(format!("{:?}", Hex(vec![1, 2])), "Hex(2 bytes)");
}

#[test]
fn lock_bodies_carry_a_lock_and_its_settlement() {
    let f = fixture();
    let lookup = |id: &[u8; 32]| (id == f.keyset.id()).then_some(&f.keyset);
    let notes = vec![f.input.0.clone()];
    let preimage = [7; 32];
    let terms = TimedTerms::new(
        ChainProfile::Core30DefaultRegtest,
        [3; 32],
        100,
        1,
        lock_hash(&preimage),
    )
    .unwrap();
    let secret = f
        .input
        .1
        .ownership()
        .owner_secret(f.owner.spend_secret())
        .unwrap();
    // A lock of the fixture's outputs, refunding to a fresh output.
    let lock = || {
        let back = Ephemeral::random()
            .derive(&f.owner.address(), 8, Branch::Refund, 0)
            .unwrap();
        let refund = vec![Output::from_derivation(&back).unwrap()];
        let proposal = Proposal::new(
            &f.keyset,
            &notes,
            f.proposal.ephemeral(),
            f.proposal.outputs().to_vec(),
            Some(TimedLock::new(terms.clone(), refund).unwrap()),
        )
        .unwrap();
        SignedProposal::sign(&f.keyset, proposal, &notes, Authorization::Plain, &[secret]).unwrap()
    };
    let signed = lock();
    // A plain spend pays no one through a payment.
    let plain = Payment::new("h", &signed, &notes, &[0; 32]);
    assert_eq!(plain.decode(lookup).unwrap_err().code, Code::Invalid);
    let (decoded, decoded_notes) = json(&Lock::new(&signed, &notes)).decode(lookup).unwrap();
    assert_eq!(decoded.to_bytes(), signed.to_bytes());
    assert_eq!(decoded_notes, notes);

    let certificate = Certificate::attest(&f.keys, &signed, &notes).unwrap();
    let locked = json(&Locked::new(&certificate));
    let checked = locked.decode(&f.keyset, &signed, &notes).unwrap();
    assert_eq!(checked.to_bytes(), certificate.to_bytes());
    // Another lock's certificate is refused.
    let foreign = Certificate::attest(&f.keys, &lock(), &notes).unwrap();
    let wrong = Locked::new(&foreign).decode(&f.keyset, &signed, &notes);
    assert_eq!(wrong.unwrap_err().code, Code::Invalid);

    let id = signed.proposal().id();
    let open = LockState::new(signed.proposal(), &certificate, None, None);
    let view = json(&open).decode(lookup).unwrap();
    assert_eq!(view.proposal.id(), id);
    assert_eq!((view.settled, view.preimage), (None, None));
    let paid = LockState::new(
        signed.proposal(),
        &certificate,
        Some(Settled::Pay),
        Some(preimage),
    );
    let view = json(&paid).decode(lookup).unwrap();
    assert_eq!(
        (view.settled, view.preimage),
        (Some(Settled::Pay), Some(preimage))
    );
    let lying = LockState::new(signed.proposal(), &certificate, None, Some([8; 32]));
    assert_eq!(lying.decode(lookup).unwrap_err().code, Code::Invalid);
    let swapped = LockState::new(signed.proposal(), &foreign, None, None);
    assert_eq!(swapped.decode(lookup).unwrap_err().code, Code::Invalid);
    let plain = LockState::new(&f.proposal, &foreign, None, None);
    assert_eq!(plain.decode(lookup).unwrap_err().code, Code::Invalid);
    let none = |_: &[u8; 32]| None;
    assert_eq!(open.decode(none).unwrap_err().code, Code::UnknownKeyset);
    assert_eq!(
        serde_json::to_value(&paid).unwrap()["settled"],
        serde_json::json!("pay")
    );

    assert_eq!(
        json(&Settle::new(&id, &preimage)).decode().unwrap(),
        (id, preimage)
    );
    assert_eq!(json(&Refund::new(&id)).decode().unwrap(), id);
    let short = Settle {
        lock: Hex(vec![0; 32]),
        preimage: Hex(vec![0; 31]),
    };
    assert_eq!(short.decode().unwrap_err().code, Code::Invalid);

    let issuances: Vec<_> = signed
        .proposal()
        .outputs()
        .iter()
        .map(|o| Nonce::new(&f.keyset, o).unwrap().issue(&f.keys).unwrap())
        .collect();
    let decoded = json(&Issued::new(&issuances)).decode(&f.keyset).unwrap();
    for (issuance, d) in decoded.iter().zip(&f.outputs) {
        issuance.open(&f.keyset, d).unwrap();
    }
    let mut broken = Issued::new(&issuances);
    broken.issuances[0].0.pop();
    assert_eq!(broken.decode(&f.keyset).unwrap_err().code, Code::Invalid);
}

#[test]
fn swap_bodies_name_hubs_by_address_and_locks_by_id() {
    use licash_api::swaps::{self, Hub, Request, State, Swap};

    let (hub_a, hub_b) = (
        OwnerKeys::from_seed(&[1; 32]).unwrap(),
        OwnerKeys::from_seed(&[2; 32]).unwrap(),
    );
    let (provider, client) = (
        OwnerKeys::from_seed(&[3; 32]).unwrap(),
        OwnerKeys::from_seed(&[4; 32]).unwrap(),
    );
    let info = swaps::Info {
        address: provider.address(),
        fee: Fee { base: 1, ppm: 1000 },
        min: 100,
        max: 50_000,
        margin: 12,
        hubs: vec![
            Hub {
                address: hub_a.address(),
                max: 20_000,
            },
            Hub {
                address: hub_b.address(),
                max: 0,
            },
        ],
        nostr: None,
    };
    assert_eq!(json(&info), info);
    assert_eq!(info.hub(&hub_a.address()).unwrap().max, 20_000);
    assert!(info.hub(&client.address()).is_none());
    let text = serde_json::to_string(&info).unwrap();
    assert!(!text.contains("nostr"), "{text}");

    let id = ProposalId::from_bytes([0xcd; 32]);
    let request = Request::new(
        &hub_a.address(),
        &id,
        &hub_b.address(),
        &client.address(),
        5000,
    );
    assert_eq!(json(&request), request);
    assert_eq!(request.lock().unwrap(), id);
    let short = Request {
        lock: Hex(vec![1; 31]),
        ..request
    };
    assert_eq!(short.lock().unwrap_err().code, Code::Invalid);

    let swap = Swap {
        id: Hex(id.as_bytes().to_vec()),
        state: State::Locked,
        amount: 5000,
        lock: Some(Hex(vec![0xef; 32])),
        expiry: 140,
        reason: None,
    };
    assert_eq!(json(&swap), swap);
    assert_eq!(
        swap.lock().unwrap(),
        Some(ProposalId::from_bytes([0xef; 32]))
    );
    assert!(serde_json::to_string(&swap)
        .unwrap()
        .contains(r#""state":"locked""#));
    let path = swaps::swap_state(&id);
    assert_eq!(path, format!("/v1/swaps/{}", "cd".repeat(32)));
    assert_eq!(licash_api::lock_id(&path[10..]).unwrap(), id);
    assert_eq!(
        (swaps::SWAPS, swaps::SWAPS_INFO),
        ("/v1/swaps", "/v1/swaps/info")
    );
}

#[test]
fn deposit_bodies_are_receipts_with_a_refund_key_and_a_payment() {
    use licash_api::{
        Deposit, DepositStatus, Depositing, Deposits, Funding, Receive, ReceiveState, ReceiveStatus,
    };
    let f = fixture();
    let outputs: Vec<Output> = f
        .outputs
        .iter()
        .map(|d| Output::from_derivation(d).unwrap())
        .collect();
    let ephemeral = Ephemeral::random().public();
    let receive = Receive::new(f.keyset.id(), &ephemeral, &outputs, &[5; 32]);
    let deposit = Deposit {
        receive: receive.clone(),
        refund: Hex(vec![2; 32]),
    };
    // The receipt's fields, and the refund key beside them.
    let wire = serde_json::to_value(&deposit).unwrap();
    let mut fields: Vec<&str> = wire
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    fields.sort();
    assert_eq!(fields, ["ephemeral", "hash", "keyset", "outputs", "refund"]);
    let back = json(&deposit);
    assert_eq!(back.receive.terms().unwrap(), receive.terms().unwrap());
    assert_eq!(back.receive.outputs().unwrap(), outputs);
    assert_eq!(back.refund, deposit.refund);

    let depositing = Depositing {
        address: "bcrt1p".into(),
        amount: 1_000,
        claim: Hex(vec![3; 32]),
        timeout: 300,
        fund_by: 150,
    };
    assert_eq!(json(&depositing), depositing);
    // Open and unpaid: no lock, no payment, on the wire too.
    let open = DepositStatus {
        status: ReceiveStatus {
            state: ReceiveState::Open,
            lock: None,
        },
        funding: None,
    };
    assert_eq!(serde_json::to_string(&open).unwrap(), r#"{"state":"open"}"#);
    let locked = DepositStatus {
        status: ReceiveStatus {
            state: ReceiveState::Locked,
            lock: Some(Hex(vec![4; 32])),
        },
        funding: Some(Funding {
            txid: "ab".repeat(32),
            vout: 1,
            height: 120,
        }),
    };
    assert_eq!(json(&locked), locked);
    let deposits = Deposits {
        fee: Fee {
            base: 500,
            ppm: 1_000,
        },
        min: 10_000,
        max: 1_000_000,
        depth: 3,
        window: 144,
        timeout: 288,
    };
    assert_eq!(deposits.pays(100_000), Some(100_600));
    assert_eq!(deposits.pays(u64::MAX), None);
    let mut info = Info::new(
        std::slice::from_ref(&f.keyset),
        f.owner.address(),
        Fees::default(),
        None,
    );
    info.deposits = Some(deposits);
    assert_eq!(json(&info).deposits, Some(deposits));
}
