//! Notes paid out to an address on Bitcoin through the stack's swap
//! provider, which pays its coins on Bitcoin Core, on the stack of
//! `apps/e2e/run.sh`, which runs these: Core's mempool and blocks take the
//! provider's coin, the wallet's claim and the provider's claim of the lock.

use anyhow::Context;
use bitcoin::FeeRate;
use licash_api::swaps::State;
use licash_e2e::{until, Eco};
use licash_nostr::{wallet::payout_providers, Route};
use licash_wallet::{LockState, PayoutState};
use serde_json::json;

/// What the provider holds on chain for the tests.
const COINS: u64 = 300_000;

/// The provider funded on chain with `COINS`, once: what it pays out now.
async fn funded(eco: &Eco, amount: u64) -> anyhow::Result<()> {
    let max = || async {
        let info = eco.swaps_info().await?;
        let chain = info
            .chain
            .context("the provider pays nothing out on Bitcoin")?;
        anyhow::Ok(chain.max)
    };
    if max().await? >= amount {
        return Ok(());
    }
    let address = eco.exec("swaps", &["licash-swaps", "address"]).await?;
    let address = String::from_utf8(address)?;
    eco.send_to(address.trim(), COINS).await?;
    eco.mine(1).await?;
    until("the provider's coins found", 120, || async {
        Ok((max().await? >= amount).then_some(()))
    })
    .await
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn notes_are_paid_out_to_a_bitcoin_address_through_the_provider() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let hub1 = eco.hub(1);
    funded(&eco, 20_000).await?;
    let info = eco.swaps_info().await?.chain.unwrap();
    // As docker-compose.yml sets it.
    assert_eq!((info.min, info.margin), (1_000, 36), "{info:?}");

    let alice = eco.wallet("payer")?;
    eco.pay_in(&alice, hub1, 30_000).await?;
    let payee = eco.miner_address("bech32m").await?;
    let rate = FeeRate::from_sat_per_vb_u32(1);
    // Found on the relay it announces itself on, as the app finds it.
    let relays = [eco.relay().to_owned()];
    let found = payout_providers(
        &alice,
        &relays,
        &Route::Direct,
        (hub1, &payee),
        10_000,
        (3_000, rate),
    )
    .await?;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (found[0].url.as_str(), found[0].known),
        (eco.swaps(), false)
    );
    let quote = alice
        .payout_quote(eco.swaps(), hub1, &payee, 10_000, 3_000, rate)
        .await?;
    // As asked, each under a ticket of its own.
    let asked =
        |q: &licash_wallet::PayoutQuote| (q.info.clone(), q.amount, q.budget, q.fee, q.lock_fee);
    assert_eq!(asked(&found[0].quote), asked(&quote));
    assert_ne!(found[0].quote.ticket, quote.ticket);
    let payout = alice.payout(&quote).await?;
    assert_eq!(payout.hash, quote.ticket);
    assert_eq!(payout.state, PayoutState::Open, "{payout:?}");
    assert_eq!((payout.budget, payout.fee), (quote.budget, quote.fee));
    let lock = payout.lock.clone().context("a lock")?;
    let theirs = eco.payout_state(&lock).await?;
    assert_eq!(theirs.value, 10_000 + payout.budget);

    // Claimed once the provider's coin confirmed, the claim broadcast.
    eco.mine(1).await?;
    let claimed = until("the payout claimed", 120, || async {
        let followed = alice.follow_payout(&payout.hash, &eco).await;
        for claim in alice.payout_claims()? {
            eco.broadcast(&claim).await?;
        }
        match followed {
            Ok(p) if p.state == PayoutState::Claimed => Ok(Some(p)),
            Ok(p) if p.state == PayoutState::Open => Ok(None),
            Ok(p) => anyhow::bail!("{p:?}"),
            Err(licash_wallet::Error::Pending { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await?;
    let funding = claimed.funding.context("the provider's coin")?;
    let claim = claimed.claim.context("the claim")?;
    assert_eq!(theirs.refund.0.len(), 32);

    // The payee gets exactly the amount once the claim confirms.
    let height = eco.mine(1).await?;
    let done = alice.follow_payout(&payout.hash, &eco).await?;
    assert_eq!(done.state, PayoutState::Done, "{done:?}");
    let block = eco.core("getblockhash", json!([height])).await?;
    let tx = eco
        .core("getrawtransaction", json!([claim.to_string(), 1, block]))
        .await?;
    assert_eq!(tx["vin"][0]["txid"], funding.txid.to_string());
    let outputs = tx["vout"].as_array().context("outputs")?;
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0]["scriptPubKey"]["address"], payee.as_str());
    assert_eq!(outputs[0]["value"].as_f64(), Some(10_000.0 / 1e8));

    // The provider read the preimage from the claim and claimed the lock.
    until("the provider paid", 120, || async {
        Ok((eco.payout_state(&lock).await?.state == State::Done).then_some(()))
    })
    .await?;
    alice.resume().await?;
    let ours = alice.locks()?.into_iter().find(|l| l.id == lock).unwrap();
    assert!(matches!(ours.state, LockState::Paid(_)), "{ours:?}");
    let status = eco
        .exec("swaps", &["licash-swaps", "status", "--json"])
        .await?;
    let status: serde_json::Value = serde_json::from_slice(&status)?;
    let swaps = status["swaps"].as_array().context("swaps")?;
    let ours = swaps
        .iter()
        .find(|s| s["id"] == lock.as_str())
        .context("ours")?;
    assert_eq!(
        (&ours["stage"], &ours["to"]),
        (&json!("done"), &json!("bitcoin:regtest"))
    );
    assert!(status["onchain"]["free"].as_u64() > Some(0), "{status}");

    // Beyond what it pays out: Alice locks nothing.
    let max = eco.swaps_info().await?.chain.unwrap().max;
    let e = alice
        .payout_quote(eco.swaps(), hub1, &payee, max, 3_000, rate)
        .await
        .unwrap_err();
    assert!(matches!(e, licash_wallet::Error::Invalid(_)), "{e}");
    assert_eq!(alice.payouts()?.len(), 1);
    // Paid out through before: found first from now on.
    let found = payout_providers(
        &alice,
        &relays,
        &Route::Direct,
        (hub1, &payee),
        10_000,
        (3_000, rate),
    )
    .await?;
    assert!(found[0].known, "{found:?}");
    Ok(())
}

/// Notes paid out to a silent payment address: the provider's coin claimed
/// to the output only its payee finds, by the tweak its hub serves of the
/// claim as of any spend.
#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn notes_are_paid_out_to_a_silent_payment_address() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let hub1 = eco.hub(1);
    funded(&eco, 20_000).await?;
    let alice = eco.wallet("silent-payer")?;
    eco.pay_in(&alice, hub1, 30_000).await?;
    let bob = eco.wallet("silent-payee")?;
    bob.add_hub(hub1).await?;
    let from = eco.height().await?;
    bob.watch_silent(hub1, from)?;
    let to = bob.silent_keys().address().to_string();
    let rate = FeeRate::from_sat_per_vb_u32(1);
    let quote = alice
        .payout_quote(eco.swaps(), hub1, &to, 10_000, 3_000, rate)
        .await?;
    let payout = alice.payout(&quote).await?;
    assert_eq!(payout.address, to);
    let lock = payout.lock.clone().context("a lock")?;

    eco.mine(1).await?;
    until("the payout claimed", 120, || async {
        let followed = alice.follow_payout(&payout.hash, &eco).await;
        for claim in alice.payout_claims()? {
            eco.broadcast(&claim).await?;
        }
        match followed {
            Ok(p) if p.state == PayoutState::Claimed => Ok(Some(p)),
            Ok(p) if p.state == PayoutState::Open => Ok(None),
            Ok(p) => anyhow::bail!("{p:?}"),
            Err(licash_wallet::Error::Pending { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
    .await?;
    eco.mine(1).await?;
    let done = alice.follow_payout(&payout.hash, &eco).await?;
    assert_eq!(done.state, PayoutState::Done, "{done:?}");

    // Bob finds exactly the amount, of his hub's tweaks.
    let found = until("the payee's coin found", 120, || async {
        let scanned = bob.scan_silent(hub1, &eco, u32::MAX).await?;
        Ok((!scanned.found.is_empty()).then_some(scanned.found))
    })
    .await?;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].value, 10_000);
    until("the provider paid", 120, || async {
        Ok((eco.payout_state(&lock).await?.state == State::Done).then_some(()))
    })
    .await?;
    Ok(())
}

/// Whether Bitcoin Core's mempool takes a claim of a coin locked to the
/// claim leaf of a payout's HTLC, `sized` or not, by a preimage of `len`
/// bytes: the coin paid, mined, and the claim signed by the claim key.
async fn claim_taken(eco: &Eco, sized: bool, len: usize) -> anyhow::Result<bool> {
    use bitcoin::{
        consensus::encode::serialize_hex,
        hashes::{sha256, Hash},
        key::{Keypair, Secp256k1},
        secp256k1::{Message, SecretKey},
        sighash::{Prevouts, SighashCache},
        taproot::LeafVersion,
        transaction, Amount, Network, OutPoint, ScriptBuf, Sequence, TapLeafHash, TapSighashType,
        Transaction, TxIn, TxOut, Txid, Witness,
    };
    use licash_onchain::deposit::Htlc;

    let secp = Secp256k1::new();
    let claim = SecretKey::from_slice(&[0x31; 32])?;
    let refund = SecretKey::from_slice(&[0x32; 32])?;
    let preimage = vec![7; len];
    let htlc = Htlc {
        hash: sha256::Hash::hash(&preimage).to_byte_array(),
        claim: claim.x_only_public_key(&secp).0,
        refund: refund.x_only_public_key(&secp).0,
        timeout: 100_000,
        wait: None,
        sized,
    };
    let value = Amount::from_sat(100_000);
    let address = htlc.address(Network::Regtest).to_string();
    let txid = eco.send_to(&address, value.to_sat()).await?;
    let paid = eco.core("getrawtransaction", json!([txid, true])).await?;
    let vout = paid["vout"]
        .as_array()
        .context("outputs")?
        .iter()
        .find(|o| o["scriptPubKey"]["address"] == address.as_str())
        .and_then(|o| o["n"].as_u64())
        .context("the coin's output")?;
    eco.mine(1).await?;
    let script = htlc.claim_script();
    let control = htlc
        .spend_info()
        .control_block(&(script.clone(), LeafVersion::TapScript))
        .context("the claim leaf")?;
    let mut tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(txid.parse::<Txid>()?, vout as u32),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: value - Amount::from_sat(1_000),
            script_pubkey: htlc.script_pubkey(),
        }],
    };
    let prevout = TxOut {
        value,
        script_pubkey: htlc.script_pubkey(),
    };
    let sighash = SighashCache::new(&tx).taproot_script_spend_signature_hash(
        0,
        &Prevouts::All(&[prevout]),
        TapLeafHash::from_script(&script, LeafVersion::TapScript),
        TapSighashType::Default,
    )?;
    let keypair = Keypair::from_secret_key(&secp, &claim);
    let signature =
        secp.sign_schnorr_no_aux_rand(&Message::from_digest(sighash.to_byte_array()), &keypair);
    let mut witness = Witness::new();
    witness.push(signature.as_ref());
    witness.push(&preimage);
    witness.push(script.as_bytes());
    witness.push(control.serialize());
    tx.input[0].witness = witness;
    let tested = eco
        .core("testmempoolaccept", json!([[serialize_hex(&tx)]]))
        .await?;
    tested[0]["allowed"].as_bool().context("a verdict")
}

/// A payout's coin is claimed with a preimage of 32 bytes only once its
/// claim is sized: an unsized claim leaf lets a client who chose the hash
/// claim with a longer preimage, one no lock takes.
#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_sized_claim_takes_a_preimage_of_32_bytes_only() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    assert!(claim_taken(&eco, false, 33).await?);
    assert!(!claim_taken(&eco, true, 33).await?);
    assert!(!claim_taken(&eco, true, 31).await?);
    assert!(claim_taken(&eco, true, 32).await?);
    Ok(())
}
