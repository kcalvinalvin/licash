//! The Hub's statement on an exit as Bitcoin shows it: blocks on regtest's
//! genesis (Base's checkpoint) carrying her request, her publication of a
//! filing, the Hub's response and its disclosure.

use k256::Scalar;
use licash::{
    authorization::{Authorization, PreparedGrant, SignedProposal},
    base::{Base, ChainTerms, Fees, GraphTerms, Limits, Parts, Party, Recipe},
    chain::Checkpoint,
    claim::{self, Branch, Item, Source, UnsignedClaim},
    crypto::{self, Point},
    note::Evidence as Issuance,
    note::{Derivation, HoldRoot, IssuerKeys, Keyset, Note},
    recovery::{disclosures, Evidence, Filing, Publication, SourceWitness, Witness},
    spend::{ChainProfile, Output, Proposal},
};
use licash_hp::{check, contract, decide, HubInput, Refusal, C};
use lichen_statement::{
    btc::{bare_tx, sha256d, Header, OutPoint},
    testing::{headers_on, published, request_on, request_witness, respond_block},
    Exit, Hash, SLOT_VOUT,
};

const SEED: [u8; 32] = [7; 32];
const SPEND: u64 = 17;
const LIMIT: usize = 128;

fn point(value: u64) -> Point {
    Point::from_projective(&crypto::generator_mul(&Scalar::from(value)))
}
fn hold() -> HoldRoot {
    HoldRoot::from_seed(&SEED).unwrap()
}
fn party(value: u64) -> Party {
    let mut script = vec![0x51, 0x20];
    script.extend(point(value + 3).x_only());
    Party {
        funding: point(value).x_only(),
        payment: point(value + 1).x_only(),
        delayed: point(value + 2).x_only(),
        payout_script: script,
    }
}
fn base(keyset: Keyset, nonce: u8) -> Base {
    let mut deposit = [31; 36];
    deposit[32..].copy_from_slice(&7u32.to_le_bytes());
    Base::new(Parts {
        nonce: [nonce; 32],
        claim_key: point(19).x_only(),
        keyset,
        deposit,
        cap: 128,
        parties: [party(23), party(29)],
        graph: GraphTerms {
            id: 7,
            unit_sats: 1,
            bits: 21,
            low_bits: 9,
            deposit_units: 500_000,
            close_height: 1_000,
            reserve_alice: 20_000,
            reserve_hub: 100_000,
            to_self_delay: 6,
            commit_fee: 1_000,
            presign_fee: 1_000,
            dust: 330,
            delta: 6,
            delta_prime: 6,
            deadline_margin: 8,
            request_height: 800,
            response_window: 12,
            publication_window: 3,
            assert_window: 12,
            challenge_window: 24,
            answer_window: 12,
        },
        recipe: Recipe {
            graph_version: 2,
            program: [11; 32],
            instances: 4,
            kept: 2,
        },
        chain: ChainTerms {
            checkpoint: Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap(),
            funding_depth: 1,
            work_depth: 6,
            max_headers: 1_000,
        },
        limits: Limits {
            max_transaction_bytes: 100_000,
            max_publication_bytes: 64_000,
            max_authorizations: LIMIT as u32,
            max_source_inputs: 128,
        },
        fees: Fees {
            max_rate_sat_per_kvb: 10_000,
            opening_fee_quote: 3_000,
        },
    })
    .unwrap()
}

fn derive(root: u64, hold: &HoldRoot, denom: u64, shared: u8, index: u32) -> Derivation {
    Derivation::derive(denom, point(root), hold.public(), &[shared; 32], index).unwrap()
}
fn owner(root: u64, shared: u8, index: u32) -> Scalar {
    Scalar::from(root)
        + licash::note::derive_output(&point(root), &[shared; 32], index)
            .unwrap()
            .tweak
            .to_scalar()
            .unwrap()
}
fn mint(keys: &IssuerKeys, keyset: &Keyset, d: &Derivation) -> Note {
    let blinded = d.blinded().unwrap();
    let proof = keys
        .prove_blinded(d.message().denomination(), blinded)
        .unwrap();
    Note::from_parts(
        keyset,
        *keyset.id(),
        d.message().clone(),
        proof.open_point(keyset, d).unwrap(),
        Issuance::Linked {
            dleq: proof.dleq(),
            blinding: *d.blinding(),
            commit: crypto::dleq_commit(&Scalar::from(71u64), &blinded.to_projective().unwrap()),
        },
    )
    .unwrap()
}
fn hold_keys(ds: &[Derivation]) -> Vec<(licash::note::Message, crypto::ScalarBytes)> {
    ds.iter()
        .map(|d| (d.message().clone(), d.hold_key(&hold()).unwrap()))
        .collect()
}

/// A contract, its issuer, and a filed claim: the filing's bytes and the
/// `(b, E)` her request signs.
struct Filed {
    issuer: IssuerKeys,
    base: Base,
    bytes: Vec<u8>,
    b: u32,
    e: Hash,
}

fn file(
    issuer: IssuerKeys,
    base: Base,
    parts: claim::Parts,
    signers: &[Derivation],
    b: u32,
    witness: Witness,
) -> Filed {
    let p = base.parts();
    let unsigned = UnsignedClaim::new(
        claim::Context {
            base: base.digest(),
            deposit: p.deposit,
            cap: p.cap,
        },
        &p.keyset,
        b,
        parts,
        LIMIT,
    )
    .unwrap();
    let publication = Publication::sign(unsigned, &hold_keys(signers), LIMIT).unwrap();
    let e = *publication.claim().digest();
    let bytes = Filing::new(publication, witness).unwrap().to_bytes();
    Filed {
        issuer,
        base,
        bytes,
        b,
        e,
    }
}

/// 32 notes of 4, of which the holder paid 16 to Bob after her backup;
/// she claims all 32, b = 128. The Hub's evidence: the spend.
fn stale() -> (Filed, Evidence) {
    let issuer = IssuerKeys::random(3).unwrap();
    let keyset = issuer.keyset().unwrap();
    let base = base(keyset.clone(), 53);
    let ds: Vec<_> = (0..32).map(|i| derive(SPEND, &hold(), 4, 23, i)).collect();
    let notes: Vec<_> = ds.iter().map(|d| mint(&issuer, &keyset, d)).collect();
    let owners: Vec<_> = (0..16).map(|i| owner(SPEND, 23, i)).collect();
    let bob = HoldRoot::from_seed(&[9; 32]).unwrap();
    let to = (0..8)
        .map(|i| Output::from_derivation(&derive(41, &bob, 8, 29, i)).unwrap())
        .collect();
    let proposal = Proposal::new(&keyset, &notes[..16], point(43), to, None).unwrap();
    let signed = SignedProposal::sign(
        &keyset,
        proposal,
        &notes[..16],
        Authorization::Plain,
        &owners,
    )
    .unwrap();
    let parts = claim::Parts {
        items: ds
            .iter()
            .map(|d| Item::Settled(d.message().clone()))
            .collect(),
        sources: vec![],
        activations: vec![],
        authorizations: sorted(&ds),
    };
    let witness = Witness {
        notes: notes.clone(),
        sources: vec![],
        activations: vec![],
    };
    let filed = file(issuer, base, parts, &ds, 128, witness);
    let spent = Evidence::Spend {
        signed,
        notes: notes[..16].to_vec(),
        unlock: None,
    };
    (filed, spent)
}

fn sorted(ds: &[Derivation]) -> Vec<licash::note::Message> {
    let mut m: Vec<_> = ds.iter().map(|d| d.message().clone()).collect();
    m.sort_by_key(|m| m.identifier().unwrap());
    m
}

/// The exit on regtest's genesis: her request of `b` and `e` (block 1),
/// her publication carrying `claim` (block 2), six blocks, the Hub's
/// response of `b2` (block 9), six blocks.
fn exit(base: &Base, b: u32, e: &Hash, claim: &[u8], b2: u32) -> Exit {
    let c = contract(base);
    let (rh, request) = request_on(&c.checkpoint.hash, &c.deposit, &request_witness(e, b));
    let slot = OutPoint {
        txid: request.txid().unwrap(),
        vout: SLOT_VOUT,
    };
    let publish = published(&sha256d(&rh), &slot, claim);
    let mut headers = vec![rh, publish.header];
    headers.extend(headers_on(&sha256d(&publish.header), 6));
    let (h, respond) = respond_block(&sha256d(headers.last().unwrap()), &request.tx, b2);
    headers.push(h);
    headers.extend(headers_on(&sha256d(&h), 6));
    Exit {
        headers,
        request,
        publish,
        respond,
        disclosure: None,
    }
}

/// The Hub discloses `bytes` spending the response's slot, in a block on
/// the exit's last; six blocks on it.
fn disclose(exit: &mut Exit, bytes: &[u8]) {
    let slot = OutPoint {
        txid: sha256d(&bare_tx(&exit.respond.tx).unwrap()),
        vout: SLOT_VOUT,
    };
    let d = published(&sha256d(exit.headers.last().unwrap()), &slot, bytes);
    exit.headers.push(d.header);
    exit.headers.extend(headers_on(&sha256d(&d.header), 6));
    exit.disclosure = Some(d);
}

fn input(base: &Base, exit: Exit, evidence: Vec<Evidence>) -> HubInput {
    HubInput {
        base: base.clone(),
        exit,
        evidence,
    }
}

/// The inputs r0/host measures the guest on (`licash-r0 hp-exec`),
/// written to `LICASH_HP_INPUTS` if set: the 32-item claim the Hub
/// answers with a 16-note spend, and the same claim unanswered.
#[test]
fn export_inputs() {
    let Ok(dir) = std::env::var("LICASH_HP_INPUTS") else {
        return;
    };
    let (f, spent) = stale();
    println!("filing: {} bytes", f.bytes.len());
    let answered = input(&f.base, exit(&f.base, f.b, &f.e, &f.bytes, 64), vec![spent]);
    let unanswered = input(&f.base, exit(&f.base, f.b, &f.e, &f.bytes, 127), vec![]);
    for (name, i) in [("answered", answered), ("unanswered", unanswered)] {
        let bytes = i.to_bytes();
        println!("{name}: {} input bytes, {:?}", bytes.len(), check(&i).err());
        std::fs::write(format!("{dir}/{name}.bin"), bytes).unwrap();
    }
}

#[test]
fn the_contract_is_bases() {
    let issuer = IssuerKeys::random(3).unwrap();
    let a = base(issuer.keyset().unwrap(), 53);
    let c = contract(&a);
    let genesis = Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap();
    assert_eq!(c.c, C);
    assert_eq!(c.checkpoint.height, 0);
    assert_eq!(c.checkpoint.hash, Header(*genesis.header()).hash());
    // six regtest blocks of work 2
    assert_eq!(c.work, 12);
    assert_eq!(c.deposit.txid, [31; 32]);
    assert_eq!(c.deposit.vout, 7);
    // Another opening (here, another nonce) is another word.
    let b = base(issuer.keyset().unwrap(), 54);
    assert_ne!(contract(&b).d, c.d);
    assert_ne!(contract(&b).journal(), c.journal());
}

#[test]
fn the_hub_proves_what_its_evidence_leaves_owed() {
    let (f, spent) = stale();
    let journal = contract(&f.base).journal();
    // The spend leaves half owed: the Hub responds 64 and proves it.
    let x = exit(&f.base, f.b, &f.e, &f.bytes, 64);
    let (owed, read) = decide(&f.base, &x, std::slice::from_ref(&spent)).unwrap();
    assert_eq!(
        (owed.owed, owed.mask, read.b, read.b2),
        (64, 0xffff, 128, 64)
    );
    assert_eq!(
        check(&input(&f.base, x.clone(), vec![spent.clone()])).unwrap(),
        journal
    );
    // The statement's input travels as bytes, decoded strictly.
    let bytes = input(&f.base, x.clone(), vec![spent.clone()]).to_bytes();
    assert_eq!(
        check(&HubInput::from_bytes(&bytes).unwrap()).unwrap(),
        journal
    );
    assert_eq!(
        HubInput::from_bytes(&bytes[..bytes.len() - 1]).err(),
        Some(Refusal::Input)
    );
    // It can't prove less than its evidence leaves, nor with no evidence.
    let short = exit(&f.base, f.b, &f.e, &f.bytes, 63);
    assert_eq!(
        check(&input(&f.base, short, vec![spent.clone()])),
        Err(Refusal::Owed)
    );
    assert_eq!(check(&input(&f.base, x, vec![])), Err(Refusal::Owed));
    // Nor under another contract: the claim names this Base.
    let other = base(f.issuer.keyset().unwrap(), 54);
    let x = exit(&other, f.b, &f.e, &f.bytes, 64);
    assert!(matches!(
        check(&input(&other, x, vec![spent])),
        Err(Refusal::Evidence(_))
    ));
}

#[test]
fn a_claim_that_isnt_one_is_owed_nothing() {
    let (f, _) = stale();
    let journal = contract(&f.base).journal();
    // Garbage, an empty slot, or a filing she didn't sign E for.
    for claim in [&b"not a filing"[..], &[], &f.bytes[..f.bytes.len() - 1]] {
        let x = exit(&f.base, f.b, &f.e, claim, 0);
        assert_eq!(check(&input(&f.base, x, vec![])).unwrap(), journal);
    }
    let x = exit(&f.base, f.b, &[0; 32], &f.bytes, 0);
    assert_eq!(check(&input(&f.base, x, vec![])).unwrap(), journal);
    // Bitcoin must show the exit: a response that isn't below her b.
    let x = exit(&f.base, f.b, &f.e, b"not a filing", f.b);
    assert!(matches!(
        check(&input(&f.base, x, vec![])),
        Err(Refusal::Exit(_))
    ));
}

/// The paper's example at half scale: Alice pays Bob 50 from a 128 note
/// under a grant; the Hub withholds the unlock. Her claim of the input
/// (the change as fallbacks) is voided only if the Hub's response
/// discloses the unlock.
#[test]
fn evidence_that_needs_a_disclosure_proves_only_with_it() {
    let issuer = IssuerKeys::random(7).unwrap();
    let keyset = issuer.keyset().unwrap();
    let base = base(keyset.clone(), 53);
    let n = derive(SPEND, &hold(), 128, 23, 0);
    let note = mint(&issuer, &keyset, &n);
    let bob = HoldRoot::from_seed(&[9; 32]).unwrap();
    let paid: Vec<_> = [32, 16, 2]
        .iter()
        .enumerate()
        .map(|(i, d)| derive(41, &bob, *d, 29, i as u32))
        .collect();
    let change: Vec<_> = [64, 8, 4, 2]
        .iter()
        .enumerate()
        .map(|(i, d)| derive(SPEND, &hold(), *d, 71, i as u32))
        .collect();
    let outputs = paid
        .iter()
        .chain(&change)
        .map(|d| Output::from_derivation(d).unwrap())
        .collect();
    let notes = std::slice::from_ref(&note);
    let proposal = Proposal::new(&keyset, notes, point(43), outputs, None).unwrap();
    let prepared = PreparedGrant::new(&issuer, &proposal).unwrap();
    let signed = SignedProposal::sign(
        &keyset,
        proposal,
        notes,
        Authorization::Granted(Box::new(prepared.offer())),
        &[owner(SPEND, 23, 0)],
    )
    .unwrap();
    let unlock = prepared.release(&keyset, notes, &signed).unwrap();
    let id = signed.proposal().id();
    let mut items = vec![Item::Settled(n.message().clone())];
    items.extend(change.iter().enumerate().map(|(i, d)| Item::Pending {
        message: d.message().clone(),
        source: id,
        branch: Branch::Pay,
        index: (paid.len() + i) as u32,
        blinding: *d.blinding(),
        preimage: None,
    }));
    let held: Vec<_> = std::iter::once(n.clone()).chain(change.clone()).collect();
    let parts = claim::Parts {
        items,
        sources: vec![Source::Plain {
            id,
            inputs: vec![n.message().identifier().unwrap()],
        }],
        activations: vec![],
        authorizations: sorted(&held),
    };
    let witness = Witness {
        notes: vec![note.clone()],
        sources: vec![SourceWitness::Signed {
            signed: signed.clone(),
            notes: vec![note.clone()],
        }],
        activations: vec![],
    };
    let f = file(issuer, base, parts, &held, 128, witness);
    let cite = Evidence::Spend {
        signed,
        notes: vec![note],
        unlock: Some(unlock),
    };
    let needed = disclosures(std::slice::from_ref(&cite)).unwrap().to_bytes();
    let mut x = exit(&f.base, f.b, &f.e, &f.bytes, 78);
    assert_eq!(
        check(&input(&f.base, x.clone(), vec![cite.clone()])),
        Err(Refusal::Undisclosed)
    );
    let mut wrong = x.clone();
    disclose(&mut wrong, &[0; 1024]);
    assert_eq!(
        check(&input(&f.base, wrong, vec![cite.clone()])),
        Err(Refusal::Undisclosed)
    );
    disclose(&mut x, &needed);
    assert_eq!(
        check(&input(&f.base, x, vec![cite])).unwrap(),
        contract(&f.base).journal()
    );
}
