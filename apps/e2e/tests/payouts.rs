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
    assert_eq!(found[0].quote, quote);
    let payout = alice
        .payout(eco.swaps(), hub1, &payee, 10_000, 3_000, rate)
        .await?;
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
        .payout(eco.swaps(), hub1, &payee, max, 3_000, rate)
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
