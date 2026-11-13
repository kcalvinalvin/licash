//! Lightning gateway against two pinned LND nodes, on the Core that
//! carries the funded recovery contracts. A Lightning payment and a timed
//! lock of notes share one hash, so a Hub that runs a Lightning node moves
//! value between the two without being trusted by its client. Lightning is
//! real; recovery proofs are mock. Run with `regtest/lightning.sh`.

use anyhow::{ensure, Context, Result};
use licash::{
    authorization::{Authorization, SignedProposal},
    note::{Derivation, HoldRoot},
    recovery::{counter_disclosures, Evidence},
    source::Certificate,
    spend::lock_hash,
    spend::{Output, Proposal, TimedLock},
};
use licash_regtest::{
    lnd::{connect_channel, Lnd, IMAGE},
    native::Native,
    native_fixture::{derive, fixture_with, hold, mint, owner, point, Terms, SPEND},
};
use lngap_btc::{
    bitcoin::{hashes::Hash, Txid},
    regtest::Regtest,
};
use serde_json::Value;

const SATS: u64 = 1024;

/// A Hub-side output, a wallet of root 41.
fn hub_output(index: u32) -> Derivation {
    derive(41, &HoldRoot::from_seed(&[9; 32]).unwrap(), SATS, index)
}

fn hash32(hex_hash: &str) -> Result<[u8; 32]> {
    hex::decode(hex_hash)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("a 32-byte hash"))
}

/// The record of the payment of `hash` once `node` sees it succeed.
fn succeeded(node: &Lnd, hash: &str) -> Result<Value> {
    let done = |p: &Value| p["payment_hash"] == hash && p["status"] == "SUCCEEDED";
    let all = node.wait(&["listpayments", "--include_incomplete"], |v| {
        v["payments"]
            .as_array()
            .is_some_and(|ps| ps.iter().any(done))
    })?;
    let payments = all["payments"].as_array().context("payments")?;
    payments
        .iter()
        .find(|p| done(p))
        .cloned()
        .context("payment")
}

/// Alice pays a Lightning invoice with notes. She locks 1024 to the Hub
/// under the invoice's hash, and the Hub pays the invoice from its node and
/// learns the preimage. The Hub keeps it to itself, so Alice has paid without
/// a receipt. She asks for her refund on Bitcoin, and the Hub can keep the
/// notes only by publishing the preimage: her proof of payment.
fn pay_out(rt: &Regtest, hub: &Lnd, recipient: &Lnd) -> Result<()> {
    let f = fixture_with(rt, "native-ln-out", Terms::default())?;
    let invoice = recipient.cli(&["addinvoice", &format!("--amt={SATS}")])?;
    let invoice = invoice["payment_request"].as_str().context("invoice")?;
    let decoded = hub.cli(&["decodepayreq", invoice])?;
    let hash = decoded["payment_hash"].as_str().context("payment hash")?;
    ensure!(decoded["num_satoshis"] == SATS.to_string());

    let terms = f.n.timed(hash32(hash)?, rt.height()? + 12, 1)?;
    let refund = [derive(SPEND, &hold(), SATS, 30)];
    let lock = f.lock(&[1], &[hub_output(0)], &refund, terms)?;
    let mut payment = hub.pay(invoice, "native-ln-out")?;
    let paid = succeeded(hub, hash)?;
    let _ = payment.0.wait()?;
    let z = hash32(paid["payment_preimage"].as_str().context("preimage")?)?;

    let (publication, witness) = f.refund_claim(&[1], &lock, &refund)?;
    let q = f.request_of(publication)?;
    let evidence = vec![Evidence::PayPreimage {
        source: lock.proposal().id(),
        preimage: z,
    }];
    let counter = f.counter(&q, &evidence, None)?;
    ensure!(counter.owed == 2048);
    let response = f.respond_counter(&q, &counter, &counter_disclosures(&evidence)?)?;
    let receipt = f.published()?[0];
    ensure!(
        hex::encode(lock_hash(&receipt)) == hash,
        "Alice reads her proof of payment from Bitcoin"
    );
    f.paid_corrected(&q, response, witness, 2048)
}

/// Alice is paid over Lightning in notes. She picks the preimage, the Hub
/// holds an invoice under its hash, and once the payer's HTLC arrives the Hub
/// certifies a lock of its own note to her, expiring well before that HTLC.
/// The Hub then stops answering. Alice publishes the preimage on Bitcoin
/// before the expiry, which makes her payment unconditional, and the Hub
/// settles the invoice with what she published.
fn pay_in(rt: &Regtest, payer: &Lnd, hub: &Lnd) -> Result<()> {
    let small = Terms {
        denominations: vec![2048],
        ..Terms::default()
    };
    let f = fixture_with(rt, "native-ln-in", small)?;
    let z = [83; 32];
    let hash = hex::encode(lock_hash(&z));
    let invoice = hub.cli(&[
        "addholdinvoice",
        "--cltv_expiry_delta=40",
        &hash,
        &SATS.to_string(),
    ])?;
    let invoice = invoice["payment_request"].as_str().context("invoice")?;
    let mut payment = payer.pay(invoice, "native-ln-in")?;
    hub.wait(&["lookupinvoice", &hash], |v| v["state"] == "ACCEPTED")?;
    let channels = hub.cli(&["listchannels"])?;
    let htlc_expiry = channels["channels"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|c| c["pending_htlcs"].as_array().into_iter().flatten())
        .filter(|h| h["hash_lock"] == hash)
        .filter_map(|h| h["expiration_height"].as_u64())
        .min()
        .context("the incoming HTLC")?;

    let terms = f.n.timed(lock_hash(&z), rt.height()? + 5, 1)?;
    ensure!(
        u64::from(terms.expiry()) + 8 < htlc_expiry,
        "the Hub can still settle after the last activation"
    );
    let pay = [derive(SPEND, &hold(), SATS, 30)];
    let note = mint(&f.issuer, f.keyset(), &hub_output(0));
    let notes = std::slice::from_ref(&note);
    let lock = TimedLock::new(terms, vec![Output::from_derivation(&hub_output(1))?])?;
    let outputs = vec![Output::from_derivation(&pay[0])?];
    let proposal = Proposal::new(f.keyset(), notes, point(43), outputs, Some(lock))?;
    let source = SignedProposal::sign(
        f.keyset(),
        proposal,
        notes,
        Authorization::Plain,
        &[owner(41, 0)],
    )?;
    let certificate = Certificate::attest(&f.issuer, &source, notes)?;

    let activation = Native::activate(rt, &z)?;
    // The Hub reads the preimage from Bitcoin and settles on Lightning.
    let reference = &activation.reference;
    let tx = rt.get_tx(&Txid::from_byte_array(reference.txid))?;
    let script = tx.output[reference.output as usize]
        .script_pubkey
        .as_bytes();
    let read: [u8; 32] = script[script.len() - 32..].try_into()?;
    hub.cli(&["settleinvoice", &hex::encode(read)])?;
    hub.wait(&["lookupinvoice", &hash], |v| v["state"] == "SETTLED")?;
    ensure!(succeeded(payer, &hash)?["payment_preimage"] == hex::encode(z));
    let _ = payment.0.wait()?;

    let (publication, witness) = f.pay_claim(&certificate, &source, &pay, z, Some(activation))?;
    let q = f.request_of(publication)?;
    let plain = f.respond_plain(&q)?;
    f.paid_in_full(&q, plain, witness)
}

#[test]
#[ignore = "explicit pinned two-node runtime gate: run regtest/lightning.sh"]
fn native_lightning_payments_out_and_in_settle_against_pinned_lnd() -> Result<()> {
    ensure!(
        std::env::var("LICASH_LND_IMAGE")? == IMAGE,
        "unexpected LND image pin"
    );
    let rt = Regtest::start_standard()?;
    let payer = Lnd::start(&rt, "payer")?;
    let receiver = Lnd::start(&rt, "receiver")?;
    connect_channel(&rt, &payer, &receiver)?;
    // The channel's funds start on `payer`'s side: it is the Hub's node when
    // Alice pays out, and the outside payer when she is paid.
    pay_out(&rt, &payer, &receiver)?;
    pay_in(&rt, &payer, &receiver)
}
