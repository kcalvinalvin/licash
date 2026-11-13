//! Native protocol end to end on Bitcoin Core, mock proof authority: Base,
//! gates, F, native R, P, the Hub's observation and response, the holder
//! statement, assert, challenge and the payout bits, for every ending.
//! Garbled instances are genuine.

use anyhow::{ensure, Context, Result};
use licash::{
    authorization::{Authorization, PreparedGrant, SignedProposal, Unlock},
    claim::{Branch, Item, Source},
    disclosure::Disclosures,
    note::{Derivation, HoldRoot},
    recovery::{
        counter_disclosures, prove_counter, settle, Evidence, HolderWitness, ItemWitness,
        Publication, Response, SourceWitness,
    },
    request::Request,
    spend::{Output, Proposal},
};
use licash_regtest::{
    contract::Stage,
    graph::Babe,
    mock_proof, mock_verify,
    native::journal,
    native_fixture::{derive, fixture, hold, mint, owner, point, Fixture, Requested, SPEND},
    native_mock_counter, native_mock_counter_verify,
};
use lngap_btc::regtest::Regtest;

/// An honest holder claims 3072: the Hub has no counter, answers plainly,
/// and she proves the Full statement (c = 0) and is paid b in bits.
#[test]
fn honest_native_claim_is_answered_proven_and_paid_on_bitcoin() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = fixture(&rt, "native-honest")?;
    let q = f.request()?;
    // The Hub's own counter search finds nothing to void.
    let unrelated = derive(SPEND, &hold(), 1024, 9);
    let unrelated_note = mint(&f.issuer, f.keyset(), &unrelated);
    let proposal = Proposal::new(
        f.keyset(),
        std::slice::from_ref(&unrelated_note),
        point(43),
        vec![Output::from_derivation(&derive(41, &hold(), 1024, 40))?],
        None,
    )?;
    let spend = SignedProposal::sign(
        f.keyset(),
        proposal,
        std::slice::from_ref(&unrelated_note),
        Authorization::Plain,
        &[owner(SPEND, 9)],
    )?;
    let evidence = Evidence::Spend {
        signed: spend,
        notes: vec![unrelated_note],
        unlock: None,
    };
    let (base, d) = (&f.n.base, &f.n.d);
    ensure!(prove_counter(base, d, &q.request, &q.publication, &[evidence], None).is_err());
    let plain = f.respond_plain(&q)?;
    let corrected = f.statement(&q, 1, plain.clone(), f.witness())?;
    let refused = corrected.run(&mut native_mock_counter_verify).unwrap_err();
    ensure!(
        refused.to_string().contains("no proven counter"),
        "{refused}"
    );
    f.paid_in_full(&q, plain, f.witness())
}

/// A stale backup claims 2048 + 1024 after the 1024 was paid away. The Hub
/// proves a Corrected counter (owed 2048), signs it in `respond_counter`,
/// and the holder's Full statement fails while the corrected one pays 2048.
#[test]
fn stale_native_claim_is_countered_and_paid_its_unspent_part_on_bitcoin() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = fixture(&rt, "native-stale")?;
    let q = f.request()?;
    let (base, d) = (&f.n.base, &f.n.d);
    // The Hub's counter, from the spend it recorded and the observed P.
    let evidence = f.paid_away()?;
    let counter = f.counter(&q, &evidence, None)?;
    ensure!((counter.mask, counter.owed) == (0b10, 2048));
    // A plain spend has no secret to release: the chunks are empty.
    let posted = counter_disclosures(&evidence)?;
    ensure!(posted == Disclosures::empty());
    let response = f.respond_counter(&q, &counter, &posted)?;

    // Same value as the library's settlement arithmetic (no penalty).
    let holder = licash::recovery::prove_holder(base, d, &q.request, &q.publication, &f.witness())?;
    let settled = settle(
        base,
        d,
        &q.request,
        Response::Counter(counter),
        Some(&holder),
    )?;
    ensure!(settled.owed == 2048);
    f.paid_corrected(&q, response, f.witness(), 2048)
}

/// The Hub never answers: after its window the request times out and the
/// payout's bits read the holder's native signature on b. No proof needed.
#[test]
fn silent_hub_pays_the_native_request_on_bitcoin() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = fixture(&rt, "native-silent")?;
    let q = f.request()?;
    let timeout = f.c.timeout(f.rt, "request_timeout")?;
    ensure!(timeout.output == f.c.set["request_timeout"].tx.output);
    f.collect(&timeout, Stage::Full, 3072, &Babe::request_wire(&q.alice_b))
}

/// The paper's payment example on the fixture's wallet. Alice pays Bob 512
/// from her 2048 note under a grant, with 1536 change; the Hub records her
/// signed spend and withholds the unlock. She claims her input with the
/// change as fallbacks, so one claim is right whichever way the payment ends.
struct Granted {
    signed: SignedProposal,
    unlock: Unlock,
    bob: Derivation,
    change: Vec<Derivation>,
    publication: Publication,
    witness: HolderWitness,
}

impl Granted {
    /// The Hub's evidence, citing the spend with `unlock` or without.
    fn cite(&self, f: &Fixture, unlock: Option<Unlock>) -> Vec<Evidence> {
        vec![Evidence::Spend {
            signed: self.signed.clone(),
            notes: f.notes[..1].to_vec(),
            unlock,
        }]
    }

    /// Open packet `index` with the unlock found in posted `slots`.
    fn open(&self, f: &Fixture, slots: &[[u8; 32]], index: usize, d: &Derivation) -> Result<u64> {
        let Authorization::Granted(grant) = self.signed.authorization() else {
            anyhow::bail!("granted fixture");
        };
        let unlock = grant.unlock_in(slots).context("no unlock was published")?;
        let note = self
            .signed
            .open_granted_output(f.keyset(), &f.notes[..1], index, d, &unlock)?;
        note.verify(f.keyset())?;
        Ok(note.message().denomination())
    }
}

fn granted(f: &Fixture) -> Result<Granted> {
    let bob = derive(41, &HoldRoot::from_seed(&[9; 32])?, 512, 0);
    let change = vec![
        derive(SPEND, &hold(), 1024, 20),
        derive(SPEND, &hold(), 512, 21),
    ];
    let outputs = std::iter::once(&bob)
        .chain(&change)
        .map(Output::from_derivation)
        .collect::<Result<Vec<_>, _>>()?;
    let proposal = Proposal::new(f.keyset(), &f.notes[..1], point(43), outputs, None)?;
    let prepared = PreparedGrant::new(&f.issuer, &proposal)?;
    let signed = SignedProposal::sign(
        f.keyset(),
        proposal,
        &f.notes[..1],
        Authorization::Granted(Box::new(prepared.offer())),
        &[owner(SPEND, 0)],
    )?;
    // The Hub holds the owner-signed spend, so it can release the unlock.
    let unlock = prepared.release(f.keyset(), &f.notes[..1], &signed)?;
    let id = signed.proposal().id();
    let mut items: Vec<_> =
        f.ds.iter()
            .map(|d| Item::Settled(d.message().clone()))
            .collect();
    items.extend(change.iter().enumerate().map(|(i, d)| Item::Pending {
        message: d.message().clone(),
        source: id,
        branch: Branch::Pay,
        index: 1 + i as u32,
        blinding: *d.blinding(),
        preimage: None,
    }));
    let source = Source::Plain {
        id,
        inputs: vec![f.ds[0].message().identifier()?],
    };
    let held: Vec<_> = f.ds.iter().chain(&change).cloned().collect();
    let publication = f.claim_of(items, vec![source], vec![], &held)?;
    let mut witness = f.witness();
    witness
        .items
        .extend(change.iter().map(|d| ItemWitness::Pending {
            ownership: d.ownership(),
        }));
    witness.sources = vec![SourceWitness::Signed {
        signed: signed.clone(),
        notes: f.notes[..1].to_vec(),
    }];
    Ok(Granted {
        signed,
        unlock,
        bob,
        change,
        publication,
        witness,
    })
}

/// The Hub can retire Alice's input only by publishing the grant's unlock in
/// `respond_counter`. Then Bob opens his 512 and Alice her change from the
/// chain, and she is paid her untouched 1024 plus the 1536 change.
#[test]
fn withheld_grant_is_countered_by_publishing_its_unlock_on_bitcoin() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = fixture(&rt, "native-granted")?;
    let g = granted(&f)?;
    let q = f.request_of(g.publication.clone())?;
    // No counter keeps the unlock private.
    ensure!(f.counter(&q, &g.cite(&f, None), None).is_err());
    let evidence = g.cite(&f, Some(g.unlock));
    let counter = f.counter(&q, &evidence, None)?;
    ensure!(counter.mask.count_ones() == 1 && counter.owed == 2560);
    let posted = counter_disclosures(&evidence)?;
    ensure!(posted.values[0] == g.unlock && counter.disclosure == posted.digest());
    let response = f.respond_counter(&q, &counter, &posted)?;

    // Anyone reads the unlock from the response transaction.
    let slots = f.published()?;
    ensure!(g.open(&f, &slots, 0, &g.bob)? == 512, "Bob is paid");
    let mut change = 0;
    for (i, d) in g.change.iter().enumerate() {
        change += g.open(&f, &slots, 1 + i, d)?;
    }
    ensure!(change == 1536, "Alice's change opens");

    f.paid_corrected(&q, response, g.witness.clone(), 2560)
}

/// A hostile Hub seals the same counter but posts empty chunks, keeping the
/// unlock. The counter is then unproven: Alice proves the Full statement.
/// The Hub challenges anyway; she answers from a kept instance and is paid b.
#[test]
fn counter_withholding_its_unlock_is_unproven_and_a_wrong_challenge_pays_in_full() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = fixture(&rt, "native-hostile")?;
    let g = granted(&f)?;
    let q = f.request_of(g.publication.clone())?;
    let (rt, c) = (f.rt, &f.c);
    let (base, d) = (&f.n.base, &f.n.d);
    let counter = f.counter(&q, &g.cite(&f, Some(g.unlock)), None)?;
    let response = f.respond_counter(&q, &counter, &Disclosures::empty())?;
    let slots = f.published()?;
    ensure!(g.open(&f, &slots, 0, &g.bob).is_err(), "Bob was not paid");

    let corrected = f.statement(&q, 1, response.clone(), g.witness.clone())?;
    let refused = corrected.run(&mut native_mock_counter_verify).unwrap_err();
    ensure!(
        refused.to_string().contains("no proven counter"),
        "{refused}"
    );
    let (j, paid) = f
        .statement(&q, 0, response, g.witness.clone())?
        .run(&mut native_mock_counter_verify)?;
    ensure!(j == journal(base, d, 0) && paid == 3072);

    let asserted = c.assert(rt, &mock_proof(&j))?;
    let (bits, proof) = c.asserted(&asserted)?;
    ensure!(mock_verify(&j, &proof));
    let challenged = c.challenge(rt, &asserted)?;
    let recovered = c
        .answer(rt, &challenged, &bits, &proof)?
        .context("a valid proof opens a kept instance")?;
    ensure!(recovered.output == c.set["wrongly_challenged_0"].tx.output);
    f.collect(
        &recovered,
        Stage::Full,
        3072,
        &Babe::request_wire(&q.alice_b),
    )
}

/// A hostile Hub signs a counter whose `K` is not a journal: a reserved byte
/// is set. Nothing was proven, so it is a Plain response, and Alice proves
/// the Full statement and is paid b. Were a malformed `K` an error, she could
/// prove neither statement and the Hub would take the contract at `T_A`.
#[test]
fn counter_with_a_malformed_journal_is_unproven_and_the_claim_is_paid_in_full() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = fixture(&rt, "native-malformed")?;
    let q = f.request()?;
    let counter = f.counter(&q, &f.paid_away()?, None)?;
    let mut k = native_mock_counter(&counter)?;
    k[20] = 1;
    let response = f.respond_sealed(&q, &counter, &Disclosures::empty(), k)?;
    f.paid_in_full(&q, response, f.witness())
}

/// A stale claim is countered, and the holder asserts the Full statement
/// anyway. She has no proof of it, the Hub challenges, no instance opens, and
/// after her answer window the Hub takes the contract.
#[test]
fn false_native_assertion_is_challenged_and_forfeits_on_bitcoin() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = fixture(&rt, "native-false")?;
    let q = f.request()?;
    let (rt, c) = (f.rt, &f.c);
    let (base, d) = (&f.n.base, &f.n.d);
    let evidence = f.paid_away()?;
    let counter = f.counter(&q, &evidence, None)?;
    let response = f.respond_counter(&q, &counter, &Disclosures::empty())?;
    ensure!(f
        .statement(&q, 0, response, f.witness())?
        .run(&mut native_mock_counter_verify)
        .is_err());
    // What she posts is not a proof of the Full journal.
    let full = journal(base, d, 0);
    let asserted = c.assert(rt, &mock_proof(&journal(base, d, 1)))?;
    let (bits, read) = c.asserted(&asserted)?;
    ensure!(!mock_verify(&full, &read), "the Hub sees a false assertion");
    let challenged = c.challenge(rt, &asserted)?;
    ensure!(c.answer(rt, &challenged, &bits, &read)?.is_none());
    let failure = c.timeout(rt, "failure")?;
    ensure!(failure.output == c.set["failure"].tx.output);
    Ok(())
}

/// The holder posts R but her claim body P confirms too late for the Hub to
/// counter. The Hub answers plainly, no statement accepts the late P, and
/// after the assertion deadline the Hub recovers the contract.
#[test]
fn late_claim_body_cannot_be_proven_and_the_hub_recovers_on_bitcoin() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = fixture(&rt, "native-late")?;
    let (rt, c) = (f.rt, &f.c);
    let publication = f.claim()?;
    let request = Request {
        amount: publication.claim().amount(),
        claim_digest: *publication.claim().digest(),
    };
    let r = f.n.request(c, rt, request)?;
    let (read, alice_b, alice_e) = f.n.read_request(&r)?;
    // The Hub finds no P in its window and has nothing to counter.
    let start = f.n.base.parts().chain.checkpoint.height();
    rt.mine(4)?;
    ensure!(f.n.observe_publication(rt, &read, start).is_err());
    let q = Requested {
        request,
        publication: publication.clone(),
        r: r.compute_txid(),
        p: r.compute_txid(),
        alice_b,
        alice_e,
    };
    let plain = f.respond_plain(&q)?;
    let q = Requested {
        p: f.n.publish(rt, &publication)?,
        ..q
    };
    let late = f
        .statement(&q, 0, plain, f.witness())?
        .run(&mut native_mock_counter_verify)
        .unwrap_err();
    ensure!(late.to_string().contains("not observable"), "{late}");
    let recovered = c.timeout(rt, "respond_timeout")?;
    ensure!(recovered.output == c.set["respond_timeout"].tx.output);
    Ok(())
}
