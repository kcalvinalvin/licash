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

/// The Hub locks notes of its own, one for each output of `pay`, and
/// certifies the lock: its promise to the payee, who needs no note evidence
/// of the Hub's inputs.
fn certified(
    f: &Fixture,
    pay: &[Derivation],
    terms: TimedTerms,
) -> Result<(SignedProposal, Certificate, Vec<Note>)> {
    let mut notes = vec![];
    let (mut refund, mut keys) = (vec![], vec![]);
    for (i, d) in (0u32..).zip(pay) {
        let value = d.message().denomination();
        notes.push(mint(&f.issuer, f.keyset(), &other(value, i)));
        refund.push(other(value, 50 + i));
        keys.push(owner(41, i));
    }
    let lock = TimedLock::new(terms, outputs(&refund)?)?;
    let proposal = Proposal::new(f.keyset(), &notes, point(43), outputs(pay)?, Some(lock))?;
    let signed = SignedProposal::sign(f.keyset(), proposal, &notes, Authorization::Plain, &keys)?;
    let certificate = Certificate::attest(&f.issuer, &signed, &notes)?;
    Ok((signed, certificate, notes))
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
    let (source, certificate, hub_notes) = certified(&f, &pay, terms.clone())?;
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
    let notes = &hub_notes[..];
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
        notes: hub_notes,
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

/// Response window of every Hub along a route. It only has to contain the
/// Hub's own lock: windows do not nest.
const WINDOW: u32 = 18;

/// The spending root of a Hub's wallet at the Hub upstream of it, where it
/// is a client with a contract of its own.
const CLIENT: u64 = 23;

/// A payment across `hubs` Hubs, the recipient's first, in which every Hub
/// upstream of hers is silent.
///
/// The lock at Hub 0 pays the recipient, and the lock at Hub `i` pays Hub
/// `i - 1`, a client of Hub `i`. Every payee has a contract of its own at
/// the Hub of its lock, on whose checkpoint the lock is timed, and holds
/// that Hub's certificate of it. The payer of the lock at the last Hub is
/// the sender. Every other Hub pays its lock itself, with notes it issued to
/// itself, so it learns the preimage as their issuer: its payee settles the
/// lock only by revealing the preimage to it or by publishing it on
/// Bitcoin. The lock at Hub `i` carries 1024 and 512 more per Hub, so each
/// forwarding Hub keeps 512. Every payee also holds a note of 1024 there,
/// which keeps each claim within the contract's cap.
///
/// The recipient reveals the preimage to her Hub in the last block before
/// its lock expires, and is paid. No Hub upstream pays its payee, so her Hub
/// publishes the preimage on Bitcoin once its own lock has expired, in time
/// for the lock one Hub upstream. That activation activates every lock
/// upstream, which all expire later, so each forwarding Hub is paid in its
/// own contract upstream and no expiry voids it. The sender's Hub keeps her
/// notes only by publishing the preimage in its response to her refund
/// request: her receipt. She is paid the 2048 she did not send.
///
/// With `silent`, the recipient's Hub does not pay her either. She
/// publishes the preimage on Bitcoin before her lock expires and claims her
/// payment with her notes. Her Hub, which read it from her activation, is
/// paid upstream with the same activation.
fn route(hubs: usize, silent: bool) -> Result<()> {
    let rt = Regtest::start_standard()?;
    let w_h = u16::try_from(WINDOW)?;
    let last = hubs - 1;
    let mut sent = vec![1024];
    sent.extend(vec![512; hubs - 1]);
    let locked: Vec<usize> = (0..sent.len()).collect();
    let mut denominations = sent.clone();
    denominations.push(2048);
    let sender = fixture_with(
        &rt,
        &format!("native-route-{hubs}-from"),
        Terms {
            denominations,
            t_a: 160,
            w_h,
            ..Terms::default()
        },
    )?;
    let mut payees = vec![];
    for i in 0..hubs {
        let root = if i == 0 { PAYEE } else { CLIENT };
        let issuer = (i == last).then(|| sender.issuer.clone());
        let theirs = Terms {
            denominations: vec![1024],
            t_a: 160,
            w_h,
            root,
            issuer,
        };
        payees.push(fixture_with(
            &rt,
            &format!("native-route-{hubs}-{i}"),
            theirs,
        )?);
    }
    let route = vec![hop(WINDOW); hubs];
    let now = rt.height()?;
    let expiries = route::expiries(now + 3, &route).context("route heights")?;
    ensure!(route::admissible(now, &expiries, &route));

    let refund: Vec<_> = (0u32..)
        .zip(&sent)
        .map(|(j, value)| derive(SPEND, &hold(), *value, 30 + j))
        .collect();
    // Recipient's Hub first: each lock with its terms, certificate and pay
    // outputs.
    let mut legs = vec![];
    for (i, payee) in payees.iter().enumerate() {
        let pay: Vec<_> = (0u32..)
            .zip(&sent[..=i])
            .map(|(j, value)| derive(payee.root, &hold(), *value, 30 + j))
            .collect();
        let terms = payee.n.timed(lock_hash(&Z), expiries[i], DEPTH)?;
        let (lock, certificate) = if i == last {
            let lock = sender.lock(&locked, &pay, &refund, terms.clone())?;
            let notes = &sender.notes[..sent.len()];
            let certificate = Certificate::attest(&sender.issuer, &lock, notes)?;
            (lock, certificate)
        } else {
            let (lock, certificate, _) = certified(payee, &pay, terms.clone())?;
            (lock, certificate)
        };
        certificate.verify(payee.keyset())?;
        legs.push((lock, terms, certificate, pay));
    }

    let activation = if silent {
        let activation = Native::activate(&rt, &Z)?;
        ensure!(activation.reference.height < expiries[0], "in time");
        Some(activation)
    } else if hubs > 1 {
        // The recipient's Hub learned the preimage at the last moment, and
        // publishes it once its own lock has expired.
        let tip = rt.height()?;
        if tip < expiries[0] {
            rt.mine(u64::from(expiries[0] - tip))?;
        }
        let activation = Native::activate(&rt, &Z)?;
        let height = activation.reference.height;
        ensure!(height >= expiries[0], "after its own lock expired");
        let bound = route[0].activated_by(expiries[0]).context("bound")?;
        ensure!(height <= bound, "activated at {height}, bound {bound}");
        Some(activation)
    } else {
        None
    };
    // Every lock upstream expires later: that one activation is theirs too.
    let z = match &activation {
        Some(a) => {
            ensure!(expiries[1..].iter().all(|e| a.reference.height < *e));
            activated(&rt, a)?
        }
        None => Z,
    };

    // The sender asks for her refund, and her Hub counters it with the
    // preimage it learned from its payee.
    let lock = &legs[last].0;
    let (publication, witness) = sender.refund_claim(&locked, lock, &refund)?;
    let q = sender.request_of(publication)?;
    let evidence = vec![Evidence::PayPreimage {
        source: lock.proposal().id(),
        preimage: z,
    }];
    let counter = sender.counter(&q, &evidence, None)?;
    ensure!(counter.owed == 2048, "her Hub keeps what she sent");
    let response = sender.respond_counter(&q, &counter, &counter_disclosures(&evidence)?)?;
    ensure!(sender.published()?[0] == z, "her receipt");
    sender.paid_corrected(&q, response, witness, 2048)?;

    // Every payee paid on Bitcoin claims its payment with its notes. The
    // lock expired long ago, and the activation keeps the expiry from
    // voiding it.
    let Some(activation) = activation else {
        return Ok(());
    };
    let first = usize::from(!silent);
    for (payee, (lock, terms, certificate, pay)) in payees.iter().zip(&legs).skip(first) {
        let a = Some(activation.clone());
        let (publication, witness) = payee.pay_claim(certificate, lock, pay, Z, a)?;
        let q = payee.request_of(publication)?;
        let value: u64 = pay.iter().map(|d| d.message().denomination()).sum();
        ensure!(u64::from(q.request.amount) == 1024 + value);
        expire(&rt, terms)?;
        let expired = [Evidence::Expired {
            source: lock.proposal().id(),
        }];
        let chain = payee.n.chain(&rt)?;
        ensure!(payee.counter(&q, &expired, Some(chain)).is_err());
        let plain = payee.respond_plain(&q)?;
        payee.paid_in_full(&q, plain, witness)?;
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
fn two_hub_route_pays_the_forwarding_hub_when_the_hub_upstream_is_silent() -> Result<()> {
    route(2, false)
}

#[test]
fn three_hub_route_pays_every_forwarding_hub_when_the_hubs_upstream_are_silent() -> Result<()> {
    route(3, false)
}

/// The recipient's Hub is silent as well: she is paid by activating the
/// first lock on Bitcoin, and her Hub is paid upstream with her activation.
#[test]
fn route_recipient_whose_hub_is_silent_is_paid_by_activating_on_bitcoin() -> Result<()> {
    route(2, true)
}

/// A provider swapping between two Hubs pays the recipient at her Hub with
/// notes that Hub issued to it, and is paid by the sender at hers. It learns
/// the preimage only from the recipient's Hub, which can keep it. So the
/// provider asks for its refund on Bitcoin once its lock has expired, and
/// that Hub keeps the provider's notes only by publishing the preimage in
/// its response, within its window. The lock that pays the provider must
/// outlive that window: the windows nest, and a Hub with the same window as
/// the recipient's could not admit it.
///
/// The provider reads the preimage in time and, with the sender's Hub
/// silent too, activates the lock there and is paid on Bitcoin.
#[test]
fn provider_paying_at_a_hub_that_withholds_the_preimage_forces_it_in_time() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let (to, from) = (hop(6), hop(WINDOW));
    let contract = |w: &Hop, denominations, root, issuer| -> Result<Terms> {
        Ok(Terms {
            denominations,
            t_a: 160,
            w_h: u16::try_from(w.window)?,
            root,
            issuer,
        })
    };
    let at_to = contract(&to, vec![1024, 2048], SPEND, None)?;
    let provider = fixture_with(&rt, "native-forced-provider", at_to)?;
    let sender = fixture_with(
        &rt,
        "native-forced-sender",
        contract(&from, vec![1024, 2048], SPEND, None)?,
    )?;
    // The provider is a client of the sender's Hub as well.
    let at_from = contract(&from, vec![2048], CLIENT, Some(sender.issuer.clone()))?;
    let paid = fixture_with(&rt, "native-forced-paid", at_from)?;

    let now = rt.height()?;
    let e_to = now + 3;
    let e_from = to.forced_upstream_expiry(e_to).context("height")?;
    ensure!(to.admits(now, e_to) && from.admits(now, e_from));
    ensure!(!to.admits(now, e_from), "the windows nest");

    let refund = [derive(SPEND, &hold(), 1024, 30)];
    let to_terms = provider.n.timed(lock_hash(&Z), e_to, DEPTH)?;
    let to_lock = provider.lock(
        &[0],
        &[derive(PAYEE, &hold(), 1024, 30)],
        &refund,
        to_terms.clone(),
    )?;
    let from_terms = paid.n.timed(lock_hash(&Z), e_from, DEPTH)?;
    let from_pay = [derive(CLIENT, &hold(), 1024, 30)];
    let from_lock = sender.lock(&[0], &from_pay, &refund, from_terms.clone())?;
    let certificate = Certificate::attest(&sender.issuer, &from_lock, &sender.notes[..1])?;
    certificate.verify(paid.keyset())?;

    // The recipient reveals the preimage to her Hub, which pays her and
    // keeps it.
    expire(&rt, &to_terms)?;
    let (publication, witness) = provider.refund_claim(&[0], &to_lock, &refund)?;
    let q = provider.request_of(publication)?;
    let evidence = vec![Evidence::PayPreimage {
        source: to_lock.proposal().id(),
        preimage: Z,
    }];
    let counter = provider.counter(&q, &evidence, None)?;
    ensure!(counter.owed == 2048, "the Hub keeps what the provider paid");
    let response = provider.respond_counter(&q, &counter, &counter_disclosures(&evidence)?)?;
    let z = provider.published()?[0];
    ensure!(lock_hash(&z) == lock_hash(&Z), "published in the response");
    let height = rt.height()?;
    let bound = to.forced_by(e_to).context("bound")?;
    ensure!(height <= bound, "read at {height}, bound {bound}");
    let activation = Native::activate(&rt, &z)?;
    ensure!(activation.reference.height < e_from, "in time");
    provider.paid_corrected(&q, response, witness, 2048)?;

    let (publication, witness) =
        paid.pay_claim(&certificate, &from_lock, &from_pay, z, Some(activation))?;
    let q = paid.request_of(publication)?;
    ensure!(q.request.amount == 3072);
    expire(&rt, &from_terms)?;
    let expired = [Evidence::Expired {
        source: from_lock.proposal().id(),
    }];
    ensure!(paid
        .counter(&q, &expired, Some(paid.n.chain(&rt)?))
        .is_err());
    let plain = paid.respond_plain(&q)?;
    paid.paid_in_full(&q, plain, witness)
}
