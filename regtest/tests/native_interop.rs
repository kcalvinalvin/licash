//! Native interoperability on Bitcoin Core, mock proof authority. One
//! mechanism, a timed lock under a hash, connects a Hub's notes to anything
//! else that pays under the same hash: a Bitcoin HTLC (swaps), a Lightning
//! invoice (`native_lightning.rs`), or a lock at another Hub (routes).
//!
//! The lock pays its payee if the preimage is known before its expiry and
//! refunds its payer otherwise. On Bitcoin the two sides are enforced by one
//! rule each: a refund claim is voided only by a response that publishes the
//! preimage, and a payee who published the preimage on Bitcoin before the
//! expiry cannot be voided at all.

use anyhow::{ensure, Context, Result};
use licash::{
    authorization::{Authorization, SignedProposal},
    chain::Activation,
    note::{Derivation, HoldRoot, Note},
    recovery::{counter_disclosures, Evidence},
    route::{self, Hop},
    source::Certificate,
    spend::lock_hash,
    spend::{Output, Proposal, TimedLock, TimedTerms},
};
use licash_regtest::{
    htlc::{self, Funded, Htlc},
    native::Native,
    native_fixture::{derive, fixture_with, hold, mint, owner, point, Fixture, Terms, SPEND},
    native_mock_counter_verify,
};
use lngap_btc::{
    bitcoin::{hashes::Hash, Txid},
    keys::{xonly, Seed},
    regtest::Regtest,
    secp256k1::Keypair,
};

/// The preimage, known at first only to whoever is paid for revealing it.
const Z: [u8; 32] = [83; 32];
/// Activation depth of every lock, and blocks a Hub needs to respond.
const DEPTH: u32 = 1;
/// The spending root of a second holder at a Hub: the payee of another
/// holder's lock, with a contract of her own.
const PAYEE: u64 = 19;

/// An output of the lock's other party, a wallet of root 41 at the same Hub.
fn other(denom: u64, index: u32) -> Derivation {
    derive(41, &HoldRoot::from_seed(&[9; 32]).unwrap(), denom, index)
}

fn outputs(ds: &[Derivation]) -> Result<Vec<Output>> {
    Ok(ds
        .iter()
        .map(Output::from_derivation)
        .collect::<Result<_, _>>()?)
}

fn key(name: &str) -> Keypair {
    Seed::from_label(&format!("native-interop/{name}")).keypair("htlc")
}

/// A confirmed Bitcoin HTLC under the lock's hash: `claim` takes it with the
/// preimage, `refund` from height `timeout`.
fn htlc(rt: &Regtest, claim: &Keypair, refund: &Keypair, timeout: u32) -> Result<Funded> {
    htlc::fund(
        rt,
        &Htlc {
            hash: lock_hash(&Z),
            claim: xonly(claim).serialize(),
            refund: xonly(refund).serialize(),
            timeout,
            sats: 1_024,
        },
    )
}

/// The Hub locks a note of its own to `pay` and certifies the lock: its
/// promise to the payee, who needs no note evidence of the Hub's input.
fn certified(
    f: &Fixture,
    pay: &[Derivation],
    terms: TimedTerms,
) -> Result<(SignedProposal, Certificate, Note)> {
    let value = pay.iter().map(|d| d.message().denomination()).sum();
    let note = mint(&f.issuer, f.keyset(), &other(value, 0));
    let lock = TimedLock::new(terms, outputs(&[other(value, 1)])?)?;
    let notes = std::slice::from_ref(&note);
    let proposal = Proposal::new(f.keyset(), notes, point(43), outputs(pay)?, Some(lock))?;
    let signed = SignedProposal::sign(
        f.keyset(),
        proposal,
        notes,
        Authorization::Plain,
        &[owner(41, 0)],
    )?;
    let certificate = Certificate::attest(&f.issuer, &signed, notes)?;
    Ok((signed, certificate, note))
}

/// The preimage an activation published, as anyone reads it from Bitcoin.
fn activated(rt: &Regtest, activation: &Activation) -> Result<[u8; 32]> {
    let reference = &activation.reference;
    let tx = rt.get_tx(&Txid::from_byte_array(reference.txid))?;
    let script = tx.output[reference.output as usize]
        .script_pubkey
        .as_bytes();
    let z: [u8; 32] = script[script.len() - 32..].try_into()?;
    ensure!(lock_hash(&z) == lock_hash(&Z), "the published preimage");
    Ok(z)
}

/// Mine until the lock under `terms` has expired at depth.
fn expire(rt: &Regtest, terms: &TimedTerms) -> Result<()> {
    let decided = u64::from(terms.expiry() + terms.depth());
    let tip = u64::from(rt.height()?);
    if tip < decided {
        rt.mine(decided - tip)?;
    }
    Ok(())
}

/// Swap-out: Alice sells 1024 of her notes to Bob for bitcoin. She locks them
/// to him under her hash, he funds a Bitcoin HTLC under it, and she takes the
/// HTLC, showing the preimage. If she then claims her refund on Bitcoin as
/// well, the Hub voids it with the preimage Bob read from her own spend.
#[test]
fn swap_out_seller_who_took_the_bitcoin_cannot_also_take_her_refund() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = fixture_with(&rt, "native-swap-out", Terms::default())?;
    let (alice, bob) = (key("alice"), key("bob"));
    let now = rt.height()?;
    let terms = f.n.timed(lock_hash(&Z), now + 12, DEPTH)?;
    let refund = [derive(SPEND, &hold(), 1024, 30)];
    let lock = f.lock(&[1], &[other(1024, 0)], &refund, terms)?;
    let id = lock.proposal().id();
    // Bob's HTLC is refundable to him before the lock expires, so Alice
    // must reveal while he can still use the preimage at the Hub.
    let funded = htlc(&rt, &alice, &bob, now + 6)?;
    let taken = htlc::claim_tx(&rt, &funded, &alice, &Z)?;
    rt.send_and_confirm(&taken)?;
    let z = htlc::preimage_of(&rt.get_tx(&taken.compute_txid())?, &funded.htlc)
        .context("Bob reads the preimage from Alice's spend")?;

    let (publication, witness) = f.refund_claim(&[1], &lock, &refund)?;
    let q = f.request_of(publication)?;
    let guess = Evidence::PayPreimage {
        source: id,
        preimage: [0; 32],
    };
    ensure!(f.counter(&q, &[guess], None).is_err(), "only the preimage");
    let evidence = vec![Evidence::PayPreimage {
        source: id,
        preimage: z,
    }];
    let counter = f.counter(&q, &evidence, None)?;
    ensure!(counter.mask.count_ones() == 1 && counter.owed == 2048);
    let response = f.respond_counter(&q, &counter, &counter_disclosures(&evidence)?)?;
    ensure!(f.published()?[0] == z, "the counter published the preimage");
    f.paid_corrected(&q, response, witness, 2048)
}

/// The same swap, but Alice never takes the bitcoin. She asks for her refund
/// on Bitcoin before the lock expires. The Hub admitted the lock because its
/// response window outlasts the expiry, so it waits, sees the lock expire
/// unpaid, and answers plainly. Alice is paid in full and Bob takes his HTLC
/// back.
#[test]
fn swap_out_lock_that_expires_unpaid_is_refunded_in_full() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let wide = Terms {
        w_h: 12,
        ..Terms::default()
    };
    let f = fixture_with(&rt, "native-swap-expired", wide)?;
    let (alice, bob) = (key("alice"), key("bob"));
    let now = rt.height()?;
    let terms = f.n.timed(lock_hash(&Z), now + 5, DEPTH)?;
    ensure!(
        hop(12).admits(now, terms.expiry()),
        "the Hub admits the lock"
    );
    ensure!(
        !hop(6).admits(now, terms.expiry()),
        "a short window does not"
    );
    let refund = [derive(SPEND, &hold(), 1024, 30)];
    let lock = f.lock(&[1], &[other(1024, 0)], &refund, terms.clone())?;
    let funded = htlc(&rt, &alice, &bob, now + 2)?;

    let (publication, witness) = f.refund_claim(&[1], &lock, &refund)?;
    let q = f.request_of(publication)?;
    expire(&rt, &terms)?;
    rt.send_and_confirm(&htlc::refund_tx(&rt, &funded, &bob)?)?;
    let plain = f.respond_plain(&q)?;
    f.paid_in_full(&q, plain, witness)
}

/// Swap-in: Alice buys 1024 of notes from the Hub with bitcoin. She funds an
/// HTLC to the Hub under her hash and the Hub certifies a lock of its own
/// note to her. The Hub then stops answering, so she publishes the preimage
/// on Bitcoin before the expiry. That activates her payment for good: neither
/// the expiry nor another spend of the Hub's note voids it, and the Hub can
/// only take the bitcoin it was promised.
#[test]
fn swap_in_payment_activated_on_bitcoin_cannot_be_voided() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let small = Terms {
        denominations: vec![2048],
        ..Terms::default()
    };
    let f = fixture_with(&rt, "native-swap-in", small)?;
    let (alice, hub) = (key("alice"), key("hub"));
    let now = rt.height()?;
    let terms = f.n.timed(lock_hash(&Z), now + 5, DEPTH)?;
    let pay = [derive(SPEND, &hold(), 1024, 30)];
    let funded = htlc(&rt, &hub, &alice, now + 12)?;
    let (source, certificate, hub_note) = certified(&f, &pay, terms.clone())?;
    let id = source.proposal().id();

    let activation = Native::activate(&rt, &Z)?;
    ensure!(activation.reference.height < terms.expiry());
    let z = activated(&rt, &activation)?;
    rt.send_and_confirm(&htlc::claim_tx(&rt, &funded, &hub, &z)?)?;

    let (publication, witness) = f.pay_claim(&certificate, &source, &pay, Z, Some(activation))?;
    let q = f.request_of(publication)?;
    expire(&rt, &terms)?;
    let expired = [Evidence::Expired { source: id }];
    ensure!(f.counter(&q, &expired, Some(f.n.chain(&rt)?)).is_err());
    let notes = std::slice::from_ref(&hub_note);
    let elsewhere = Proposal::new(
        f.keyset(),
        notes,
        point(47),
        outputs(&[other(1024, 5)])?,
        None,
    )?;
    let elsewhere = SignedProposal::sign(
        f.keyset(),
        elsewhere,
        notes,
        Authorization::Plain,
        &[owner(41, 0)],
    )?;
    let respent = [Evidence::Spend {
        signed: elsewhere,
        notes: vec![hub_note],
        unlock: None,
    }];
    ensure!(f.counter(&q, &respent, None).is_err());
    let plain = f.respond_plain(&q)?;
    f.paid_in_full(&q, plain, witness)
}

/// The same purchase, but Alice never reveals the preimage. The lock expires
/// and she takes her bitcoin back. If she then claims the notes as well, the
/// Hub voids them with the chain: the lock expired with no activation.
#[test]
fn swap_in_payment_left_to_expire_is_voided_by_the_chain() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let small = Terms {
        denominations: vec![2048],
        ..Terms::default()
    };
    let f = fixture_with(&rt, "native-swap-in-expired", small)?;
    let (alice, hub) = (key("alice"), key("hub"));
    let now = rt.height()?;
    let terms = f.n.timed(lock_hash(&Z), now + 3, DEPTH)?;
    let pay = [derive(SPEND, &hold(), 1024, 30)];
    let funded = htlc(&rt, &hub, &alice, now + 3)?;
    let (source, certificate, _) = certified(&f, &pay, terms.clone())?;
    expire(&rt, &terms)?;
    rt.send_and_confirm(&htlc::refund_tx(&rt, &funded, &alice)?)?;

    let (publication, witness) = f.pay_claim(&certificate, &source, &pay, Z, None)?;
    let q = f.request_of(publication)?;
    let evidence = vec![Evidence::Expired {
        source: source.proposal().id(),
    }];
    ensure!(
        f.counter(&q, &evidence, None).is_err(),
        "expiry needs the chain"
    );
    let counter = f.counter(&q, &evidence, Some(f.n.chain(&rt)?))?;
    ensure!(counter.mask.count_ones() == 1 && counter.owed == 2048);
    let response = f.respond_counter(&q, &counter, &counter_disclosures(&evidence)?)?;
    f.paid_corrected(&q, response, witness, 2048)
}

/// A lock is timed on the chain that its terms name by a checkpoint, and both
/// programs of a recovery read Bitcoin from the checkpoint of the claiming
/// contract. Only there can the payee prove an activation and the Hub an
/// expiry, so a payee is protected only in a contract opened on the lock's
/// checkpoint, and the holder's program refuses the claim in any other.
/// Here the lock expired unpaid, and a payee who knows the preimage claims
/// it in a contract opened on a later block.
#[test]
fn payee_cannot_claim_a_lock_timed_on_another_checkpoint() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let payer = fixture_with(&rt, "native-checkpoint-payer", Terms::default())?;
    let hers = Terms {
        denominations: vec![2048],
        root: PAYEE,
        issuer: Some(payer.issuer.clone()),
        ..Terms::default()
    };
    let payee = fixture_with(&rt, "native-checkpoint-payee", hers)?;
    let now = rt.height()?;
    let terms = payer.n.timed(lock_hash(&Z), now + 3, DEPTH)?;
    ensure!(terms != payee.n.timed(lock_hash(&Z), now + 3, DEPTH)?);
    let pay = [derive(PAYEE, &hold(), 1024, 30)];
    let refund = [derive(SPEND, &hold(), 1024, 30)];
    let lock = payer.lock(&[1], &pay, &refund, terms.clone())?;
    let certificate = Certificate::attest(&payer.issuer, &lock, &payer.notes[1..])?;
    expire(&rt, &terms)?;

    let (publication, witness) = payee.pay_claim(&certificate, &lock, &pay, Z, None)?;
    let q = payee.request_of(publication)?;
    // From this contract's checkpoint the Hub cannot show the expiry.
    let expired = [Evidence::Expired {
        source: lock.proposal().id(),
    }];
    let chain = payee.n.chain(&rt)?;
    ensure!(payee.counter(&q, &expired, Some(chain)).is_err());
    let plain = payee.respond_plain(&q)?;
    let refused = payee
        .statement(&q, 0, plain, witness)?
        .run(&mut native_mock_counter_verify)
        .map(|(_, paid)| paid)
        .unwrap_err();
    ensure!(refused.to_string().contains("checkpoint"), "{refused}");
    let recovered = payee.c.timeout(&rt, "respond_timeout")?;
    ensure!(recovered.output == payee.c.set["respond_timeout"].tx.output);
    Ok(())
}

/// A Hub's timing for the locks it issues, at test scale, in blocks.
fn hop(window: u32) -> Hop {
    Hop {
        window,
        confirm: 2,
        detect: 1,
        margin: 1,
        depth: DEPTH,
        reaction: 2,
    }
}

/// Response windows along a route, the recipient's Hub first. Each exceeds
/// the sum of those before it, which is what lets every Hub wait for its
/// own lock to expire inside its window.
const WINDOWS: [u32; 3] = [6, 18, 40];

/// A payment of 1024 across `hubs` Hubs in which every Hub withholds.
///
/// Hub 0 is the recipient's. The payer of the lock at Hub `i` is Hub `i + 1`,
/// a client of Hub `i` with its own recovery contract there, and the payer at
/// the last Hub is the sender. Each lock pays the Hub that issued it, except
/// the first, which pays the recipient under a certificate. Each forwarding
/// Hub keeps 512.
///
/// The recipient reveals the preimage to her Hub and is paid. No Hub passes
/// it on, so each payer asks for its refund on Bitcoin, and each Hub can keep
/// what it was paid only by publishing the preimage in its response. The
/// preimage therefore travels up the route on Bitcoin, each payer reads it
/// before its own incoming lock expires, and the sender ends with it as her
/// receipt. Every payer is paid the 2048 it did not forward.
///
/// With `silent`, the recipient's Hub does not pay her either. She has a
/// contract of her own there, on whose checkpoint the first lock is timed, so
/// she publishes the preimage on Bitcoin before that lock expires and claims
/// her payment with her notes. The rest is unchanged, except that the first
/// payer could have read the preimage from her activation.
fn route(hubs: usize, silent: bool) -> Result<()> {
    let rt = Regtest::start_standard()?;
    let mut fs = vec![];
    for (i, window) in WINDOWS.iter().take(hubs).enumerate() {
        let mut denominations = vec![1024];
        denominations.extend(vec![512; i]);
        denominations.push(2048);
        let terms = Terms {
            denominations,
            t_a: 160,
            w_h: u16::try_from(*window)?,
            ..Terms::default()
        };
        fs.push(fixture_with(
            &rt,
            &format!("native-route-{hubs}-{i}"),
            terms,
        )?);
    }
    let recipient = if silent {
        let hers = Terms {
            denominations: vec![2048],
            t_a: 160,
            root: PAYEE,
            issuer: Some(fs[0].issuer.clone()),
            ..Terms::default()
        };
        Some(fixture_with(&rt, &format!("native-route-{hubs}-to"), hers)?)
    } else {
        None
    };
    let route: Vec<Hop> = WINDOWS.iter().take(hubs).map(|w| hop(*w)).collect();
    let now = rt.height()?;
    let expiries = route::expiries(now + 3, &route).context("route heights")?;
    ensure!(route::admissible(now, &expiries, &route));

    let mut legs = vec![];
    for (i, f) in fs.iter().enumerate() {
        let locked: Vec<usize> = (0..=i).collect();
        let (mut pay, mut refund) = (vec![], vec![]);
        for j in &locked {
            let value = f.ds[*j].message().denomination();
            pay.push(match (&recipient, i) {
                (Some(_), 0) => derive(PAYEE, &hold(), value, 30 + *j as u32),
                _ => other(value, *j as u32),
            });
            refund.push(derive(SPEND, &hold(), value, 30 + *j as u32));
        }
        // A lock is timed on its payee's checkpoint.
        let timed = match (&recipient, i) {
            (Some(r), 0) => &r.n,
            _ => &f.n,
        };
        let terms = timed.timed(lock_hash(&Z), expiries[i], DEPTH)?;
        let lock = f.lock(&locked, &pay, &refund, terms)?;
        legs.push((locked, lock, refund, pay));
    }
    // The recipient holds her Hub's certificate of the first lock, so she
    // can activate it on Bitcoin herself when her Hub does not pay her.
    let certificate = Certificate::attest(&fs[0].issuer, &legs[0].1, &fs[0].notes[..1])?;
    certificate.verify(fs[0].keyset())?;

    let mut z = Z;
    let mut received = None;
    if let Some(r) = &recipient {
        let activation = Native::activate(&rt, &Z)?;
        ensure!(activation.reference.height < expiries[0], "in time");
        // Anyone reads the preimage from her activation, well before the
        // lock one Hub upstream expires.
        z = activated(&rt, &activation)?;
        let height = rt.height()?;
        ensure!(expiries.get(1).is_none_or(|up| height < *up));
        received = Some((r, activation));
    }
    let mut answered = vec![];
    for (i, f) in fs.iter().enumerate() {
        let (locked, lock, refund, _) = &legs[i];
        let (publication, witness) = f.refund_claim(locked, lock, refund)?;
        let q = f.request_of(publication)?;
        let evidence = vec![Evidence::PayPreimage {
            source: lock.proposal().id(),
            preimage: z,
        }];
        let counter = f.counter(&q, &evidence, None)?;
        ensure!(counter.owed == 2048, "hub {i} keeps what it was paid");
        let response = f.respond_counter(&q, &counter, &counter_disclosures(&evidence)?)?;
        // The payer reads the preimage from the response, in time to use it
        // one Hub upstream.
        z = f.published()?[0];
        ensure!(lock_hash(&z) == lock_hash(&Z), "hub {i} published it");
        let height = rt.height()?;
        let bound = route[i].resolved_by(expiries[i]).context("bound")?;
        ensure!(height <= bound, "resolved at {height}, bound {bound}");
        ensure!(expiries.get(i + 1).is_none_or(|up| height < *up));
        answered.push((q, response, witness));
    }
    for (f, (q, response, witness)) in fs.iter().zip(answered) {
        f.paid_corrected(&q, response, witness, 2048)?;
    }
    // The recipient's payment, with her own note. The first lock expired
    // long ago, and her activation keeps the expiry from voiding it.
    if let Some((r, activation)) = received {
        let (lock, pay) = (&legs[0].1, &legs[0].3);
        let (publication, witness) = r.pay_claim(&certificate, lock, pay, Z, Some(activation))?;
        let q = r.request_of(publication)?;
        ensure!(q.request.amount == 3072);
        let expired = [Evidence::Expired {
            source: legs[0].1.proposal().id(),
        }];
        let chain = r.n.chain(&rt)?;
        ensure!(r.counter(&q, &expired, Some(chain)).is_err());
        let plain = r.respond_plain(&q)?;
        r.paid_in_full(&q, plain, witness)?;
    }
    Ok(())
}

/// One Hub: a payment under a hash to another client of the same Hub, or out
/// to Lightning. The payer's receipt is the preimage.
#[test]
fn withheld_receipt_of_a_hash_locked_payment_is_forced_onto_bitcoin() -> Result<()> {
    route(1, false)
}

#[test]
fn two_hub_route_resolves_on_bitcoin_when_every_hub_withholds() -> Result<()> {
    route(2, false)
}

#[test]
fn three_hub_route_resolves_on_bitcoin_when_every_hub_withholds() -> Result<()> {
    route(3, false)
}

/// The recipient's Hub is silent as well: she is paid by activating the
/// first lock on Bitcoin, and every payer upstream still ends with the
/// preimage or its refund.
#[test]
fn route_recipient_whose_hub_is_silent_is_paid_by_activating_on_bitcoin() -> Result<()> {
    route(2, true)
}
