//! Deposits on Bitcoin at the first hub, paid and refunded on Bitcoin Core,
//! on the stack of `apps/e2e/run.sh`, which runs these: Core's mempool and
//! blocks take the hub's claim and the payer's refund as valid spends.

use anyhow::{bail, ensure, Context};
use bitcoin::{consensus::encode::serialize_hex, Amount, FeeRate, OutPoint, ScriptBuf, Txid};
use licash_e2e::{until, Eco, Wallet};
use licash_wallet::{Deposit, DepositState};
use serde_json::json;

/// Where the first hub claims deposits to (docker-compose.yml).
const SWEEP: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";

/// The output of transaction `txid` paying `address`, and its value.
async fn paying(eco: &Eco, txid: &str, address: &str) -> anyhow::Result<(OutPoint, Amount)> {
    let tx = eco.core("getrawtransaction", json!([txid, 1])).await?;
    let outputs = tx["vout"].as_array().context("outputs")?;
    let output = outputs
        .iter()
        .find(|o| o["scriptPubKey"]["address"] == address)
        .context("an output paying the deposit")?;
    let vout = output["n"].as_u64().context("its index")? as u32;
    let value = Amount::from_btc(output["value"].as_f64().context("its value")?)?;
    Ok((OutPoint::new(txid.parse()?, vout), value))
}

/// The deposit of `hash` once in a state `done` accepts, followed on.
async fn followed(
    wallet: &Wallet,
    hash: &str,
    what: &str,
    done: impl Fn(&DepositState) -> bool,
) -> anyhow::Result<Deposit> {
    until(what, 60, || async {
        let deposit = wallet.follow_deposit(hash).await?;
        Ok(done(&deposit.state).then_some(deposit))
    })
    .await
}

/// Whether Bitcoin Core's chain leaves `outpoint` unspent.
async fn unspent(eco: &Eco, outpoint: &OutPoint) -> anyhow::Result<bool> {
    let txid = outpoint.txid.to_string();
    let out = eco
        .core("gettxout", json!([txid, outpoint.vout, false]))
        .await?;
    Ok(!out.is_null())
}

/// Mine until the tip is `height`.
async fn mine_to(eco: &Eco, height: u32) -> anyhow::Result<()> {
    let tip = eco.height().await?;
    ensure!(tip <= height, "the tip {tip} is past {height}");
    if tip < height {
        eco.mine(height - tip).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_deposit_paid_on_bitcoin_is_received_and_its_coins_claimed() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let hub = eco.hub(1);
    let alice = eco.wallet("alice")?;
    let deposit = alice.deposit(hub, 20_000).await?;
    let terms = deposit.terms.clone().context("its terms")?;
    // The hub's default fee of 1000 on top.
    ensure!(terms.pays == 21_000, "pays {}", terms.pays);
    let txid = eco.send_to(&terms.address, terms.pays).await?;
    let (funding, _) = paying(&eco, &txid, &terms.address).await?;
    eco.mine(1).await?;

    // At depth, the hub locks its notes, which the wallet claims.
    let received = followed(&alice, &deposit.hash, "the deposit received", |s| {
        *s == DepositState::Received
    })
    .await?;
    ensure!(
        received.funding.map(|(o, _)| o) == Some(funding),
        "{received:?}"
    );
    let balance: u64 = alice.balance()?.iter().map(|b| b.available).sum();
    ensure!(balance == 20_000, "a balance of {balance}");

    // The preimage the wallet claimed with lets the hub claim the coins,
    // which Core takes into its mempool, then a block.
    let prevout = json!([{ "txid": funding.txid.to_string(), "vout": funding.vout }]);
    let claim = until("the hub's claim", 60, || async {
        let spending = eco.core("gettxspendingprevout", json!([prevout])).await?;
        Ok(spending[0]["spendingtxid"].as_str().map(str::to_owned))
    })
    .await?;
    let height = eco.mine(1).await?;
    ensure!(
        !unspent(&eco, &funding).await?,
        "the deposit is still unspent"
    );
    let block = eco.core("getblockhash", json!([height])).await?;
    let tx = eco
        .core("getrawtransaction", json!([claim, 1, block]))
        .await?;
    ensure!(tx["confirmations"].as_u64() >= Some(1), "{tx}");
    ensure!(tx["vout"][0]["scriptPubKey"]["address"] == SWEEP, "{tx}");
    let refund = alice.refund_deposit(
        &deposit.hash,
        funding,
        Amount::from_sat(terms.pays),
        ScriptBuf::new(),
        FeeRate::from_sat_per_vb_u32(2),
    );
    ensure!(
        refund.is_err(),
        "a refund of a deposit received: {refund:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn a_deposit_paid_wrong_returns_to_its_payer_after_its_timeout() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let hub = eco.hub(1);
    let alice = eco.wallet("alice")?;
    let deposit = alice.deposit(hub, 15_000).await?;
    let terms = deposit.terms.clone().context("its terms")?;
    // A satoshi short: never locked, canceled once its window passed.
    let txid = eco.send_to(&terms.address, terms.pays - 1).await?;
    let (funding, value) = paying(&eco, &txid, &terms.address).await?;
    eco.mine(1).await?;
    mine_to(&eco, terms.fund_by + 1).await?;
    followed(&alice, &deposit.hash, "the deposit canceled", |s| {
        matches!(s, DepositState::Canceled(_))
    })
    .await?;
    let balance: u64 = alice.balance()?.iter().map(|b| b.available).sum();
    ensure!(balance == 0, "a balance of {balance}");

    // Its refund is not final before the timeout: Core refuses it.
    let to = eco.miner_address("bech32m").await?;
    let to = to
        .parse::<bitcoin::Address<_>>()?
        .assume_checked()
        .script_pubkey();
    let rate = FeeRate::from_sat_per_vb_u32(2);
    let refund = alice.refund_deposit(&deposit.hash, funding, value, to.clone(), rate)?;
    ensure!(refund.lock_time.to_consensus_u32() == terms.timeout);
    let early = eco
        .core("sendrawtransaction", json!([serialize_hex(&refund)]))
        .await;
    match early {
        Err(e) if format!("{e:#}").contains("non-final") => {}
        other => bail!("a refund before the timeout: {other:?}"),
    }
    // From the timeout on, it is.
    mine_to(&eco, terms.timeout).await?;
    let sent = eco
        .core("sendrawtransaction", json!([serialize_hex(&refund)]))
        .await?;
    let sent: Txid = sent.as_str().context("a txid")?.parse()?;
    ensure!(sent == refund.compute_txid());
    eco.mine(1).await?;
    ensure!(
        !unspent(&eco, &funding).await?,
        "the deposit is still unspent"
    );
    let state = alice.deposit_of(&deposit.hash)?.state;
    ensure!(state == DepositState::Refunded(sent), "{state:?}");
    Ok(())
}
