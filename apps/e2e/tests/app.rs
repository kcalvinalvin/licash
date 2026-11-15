//! The Android app among the stack of `apps/e2e/run.sh`, on the emulator or
//! phone adb picks, as its user sees it: made, paid on Bitcoin and in
//! Licash, paying, told what went wrong, and started again. Skipped unless
//! `LICASH_E2E_APP` names the app's APK. What it showed and logged on a
//! failure is kept beside the stack's logs.

use std::{path::PathBuf, time::Duration};

use anyhow::{ensure, Context};
use licash_e2e::{phone::Phone, until, Eco, OUTSIDE};
use licash_nostr::{wallet, Route};
use licash_wallet::PaymentCode;
use serde_json::json;

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh and an Android device"]
async fn the_app_is_paid_and_pays_on_bitcoin_and_in_licash() -> anyhow::Result<()> {
    let Some(phone) = Phone::from_env() else {
        eprintln!("skipped: LICASH_E2E_APP names no APK of the app");
        return Ok(());
    };
    let eco = Eco::up().await?;
    phone.fresh(&eco).await?;
    let used = used(&eco, &phone).await;
    if used.is_err() {
        keep(&phone).await;
    }
    used
}

/// What the app shows and logged, beside the stack's logs.
async fn keep(phone: &Phone) {
    let dir = std::env::var_os("LICASH_E2E_LOGS").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/e2e"),
        PathBuf::from,
    );
    let _ = std::fs::create_dir_all(&dir);
    let _ = phone.screenshot(&dir.join("app.png")).await;
    for log in ["app.log", "chain.log"] {
        if let Ok(text) = phone.file(&format!("files/logs/{log}")).await {
            let _ = std::fs::write(dir.join(format!("app-{log}")), text);
        }
    }
}

/// Mine whatever waits in the mempool until Home shows `moves` payments
/// on Bitcoin moved into Licash, and none under way.
async fn settle(eco: &Eco, phone: &Phone, moves: usize) -> anyhow::Result<()> {
    let what = format!("{moves} payments on Bitcoin moved into Licash");
    until(&what, 300, || async {
        let waiting = eco.core("getmempoolinfo", json!([])).await?["size"].as_u64();
        if waiting.unwrap_or(0) > 0 {
            eco.mine(1).await?;
        }
        let views = phone.views().await?;
        let moved = views.iter().filter(|v| v.text == "From Bitcoin").count();
        let busy = views.iter().any(|v| {
            [
                "Under way",
                "Waiting for a block",
                "Coming on Bitcoin",
                "On Bitcoin",
            ]
            .contains(&v.text.as_str())
        });
        Ok((moved == moves && !busy).then_some(()))
    })
    .await
}

/// Wait for the balance Home shows to change from `from`: the new one.
async fn changed(phone: &Phone, from: u64, secs: u64) -> anyhow::Result<u64> {
    until("the balance to change", secs, || async {
        let now = phone.balance().await?;
        Ok((now != from).then_some(now))
    })
    .await
}

/// Paid over Lightning: an invoice its hub makes for an amount it enters,
/// shown as a QR code, which a node outside pays. Its notes are taken while
/// shown, and the payment is in the history.
async fn lightning(eco: &Eco, phone: &Phone) -> anyhow::Result<()> {
    let before = phone.balance().await?;
    phone.tap("Receive").await?;
    phone.tap("Receive over Lightning").await?;
    phone.tap_id("amount").await?;
    phone.type_text("4000").await?;
    phone.tap("Done").await?;
    phone
        .wait("Asking for 4\u{2009}000 sat over Lightning", 10)
        .await?;
    // In upper case, as a QR code holds it best.
    let shown = phone.qr(60).await?;
    let request = shown.to_lowercase();
    ensure!(
        request.starts_with("lnbcrt"),
        "{shown} shown for an invoice"
    );
    let paying = eco.pay(OUTSIDE, &request);
    phone.wait("Received 4\u{2009}000 sat", 120).await?;
    ensure!(paying.await??, "the node outside did not pay {request}");
    phone.tap("Back").await?;
    phone.wait("Send", 30).await?;
    let now = changed(phone, before, 30).await?;
    ensure!(
        now == before + 4_000,
        "{now} held after 4000 over Lightning"
    );
    phone.tap("Received over Lightning").await?;
    phone.wait("In your Licash", 10).await?;
    phone.back().await?;
    phone.wait("Send", 30).await?;
    Ok(())
}

/// Collect Bob's mailbox, for up to `secs`, until it holds payments: their
/// sum.
async fn collected(bob: &licash_e2e::Wallet, secs: u64) -> anyhow::Result<u64> {
    until("a payment in Bob's mailbox", secs, || async {
        let collected = wallet::collect(bob, &Route::Direct).await?;
        let sum: u64 = collected.received.iter().map(|r| r.amount).sum();
        Ok((sum > 0).then_some(sum))
    })
    .await
}

async fn used(eco: &Eco, phone: &Phone) -> anyhow::Result<()> {
    let hub = eco.hub(1);
    let first = phone.launch().await?;
    eprintln!("first start in {first:?}");
    phone.onboard().await?;
    phone.connect(hub).await?;

    // Its code names the hub and its mailbox, a silent payment address,
    // the hub serving tweaks, and an address to fall back on.
    let code: PaymentCode = phone.code().await?.parse()?;
    let fallback = code.address.clone().context("a fallback address")?;
    let fallback = fallback.assume_checked().to_string();
    let silent = code.silent.context("a silent payment address")?;
    let licash = code.licash.clone().context("a Licash code")?;
    ensure!(licash.mailbox.is_some(), "no mailbox in {code}");
    // Paid at the fallback address: shown on its way, then found in a
    // block and moved into Licash, the next address shown from then on.
    eco.send_to(&fallback, 30_000).await?;
    phone.wait("Coming on Bitcoin", 60).await?;
    eco.mine(1).await?;
    settle(eco, phone, 1).await?;
    let moved = phone.balance().await?;
    // Less the hub's 1000 and the fees of the move on Bitcoin.
    ensure!(
        (25_000..29_000).contains(&moved),
        "{moved} of 30000 moved in"
    );
    // Tapped, the move shows what it cost: the hub's 1000 and the miners'.
    phone.tap("From Bitcoin").await?;
    phone.wait("In your Licash", 10).await?;
    phone.wait("Hub fee", 10).await?;
    phone.wait("1\u{2009}000 sat", 10).await?;
    phone.wait("Network fee", 10).await?;
    phone.back().await?;
    phone.wait("Receive", 10).await?;
    let next: PaymentCode = phone.code().await?.parse()?;
    ensure!(next.address != code.address, "paid at, and shown again");
    ensure!(next.silent == code.silent, "another silent payment address");

    // Paid at its silent payment address: found and moved in too.
    eco.pay_silent(&silent.to_string(), 20_000).await?;
    eco.mine(1).await?;
    settle(eco, phone, 2).await?;
    let held = phone.balance().await?;
    ensure!(
        (15_000..19_000).contains(&(held - moved)),
        "{} moved in",
        held - moved
    );

    // It pays Bob at the hub, an amount it enters, to his mailbox.
    let info = eco.info(hub).await?;
    let bob = eco.wallet("bob")?;
    bob.add_hub(hub).await?;
    let bobs = bob.payment_code(&[hub.to_owned()], false)?;
    phone.link(&bobs.to_string()).await?;
    phone.tap("Enter amount").await?;
    phone.tap_id("amount").await?;
    phone.type_text("5000").await?;
    phone.tap("Done").await?;
    phone.wait_any(&["Fee up to", "No fee"], 10).await?;
    phone.tap("Pay").await?;
    phone.wait("Sent", 60).await?;
    phone.tap("Done").await?;
    ensure!(
        collected(&bob, 60).await? == 5_000,
        "Bob got other than 5000"
    );
    let after = changed(phone, held, 30).await?;
    let fee = info.fees.spend.due(held);
    ensure!(
        (5_000..=5_000 + fee).contains(&(held - after)),
        "{} spent paying 5000",
        held - after
    );

    // Bob pays its code back, to its mailbox: the app takes it by itself.
    let code: PaymentCode = phone.code().await?.parse()?;
    let paid = wallet::pay(&bob, &code, Some(3_000), || Ok(Route::Direct)).await?;
    ensure!(paid.delivered == Some(1), "{:?}", paid.undelivered);
    let now = changed(phone, after, 90).await?;
    ensure!(now == after + 3_000, "{now} held after 3000 received");
    phone.wait("Received", 10).await?;

    lightning(eco, phone).await?;
    refused(eco, phone, &bobs).await?;
    hub_down(eco, phone, &bob, &bobs).await?;
    found_hubs(phone).await?;
    cashed_out(eco, phone).await?;
    visited(eco, phone).await?;
    contracted(eco, phone).await?;

    // Started again: at once, holding the same.
    let held = phone.balance().await?;
    let again = phone.launch().await?;
    ensure!(again < Duration::from_secs(10), "{again:?} to start again");
    phone.wait_balance(held, 30).await?;
    logs(phone).await
}

/// What it cannot pay: told why, nothing spent.
async fn refused(eco: &Eco, phone: &Phone, bobs: &PaymentCode) -> anyhow::Result<()> {
    let held = phone.balance().await?;
    let mut asking = bobs.clone();
    asking.amount = Some(10_000_000);
    phone.link(&asking.to_string()).await?;
    phone.tap("Pay").await?;
    phone.wait("Not enough", 60).await?;
    phone.tap("Cancel").await?;

    phone.link("bitcoin:?licash=nonsense").await?;
    phone.wait("Neither a payment code", 30).await?;
    phone.tap("Cancel").await?;

    // A code naming only a hub it does not use.
    let carol = eco.wallet("carol")?;
    carol.add_hub(eco.hub(2)).await?;
    let carols = carol.payment_code(&[eco.hub(2).to_owned()], false)?;
    phone.link(&carols.to_string()).await?;
    phone.wait("This code names no hub you use", 30).await?;
    phone.tap("Cancel").await?;
    phone.wait("Receive", 10).await?;
    ensure!(phone.balance().await? == held, "spent refusing");
    Ok(())
}

/// A code naming only a hub it does not use, saying where to reach it:
/// paid there once asked and reviewed, a swap bringing notes there from
/// its hub. What the swap brought past the payment shows apart until the
/// hub is added.
async fn visited(eco: &Eco, phone: &Phone) -> anyhow::Result<()> {
    eco.stock(5_000).await?;
    let held = phone.balance().await?;
    let dave = eco.wallet("dave")?;
    dave.add_hub(eco.hub(2)).await?;
    let mut daves = dave.payment_code(&[eco.hub(2).to_owned()], false)?;
    // Less than the provider swaps at least: some is left there.
    daves.amount = Some(800);
    phone.link(&daves.to_string()).await?;
    // Its amount asked: straight to what it costs.
    phone.tap("Pay there").await?;
    phone.wait("Paid at", 120).await?;
    phone.wait("Moves", 10).await?;
    phone.tap("Pay").await?;
    phone.wait("Sent", 180).await?;
    phone.tap("Done").await?;
    ensure!(
        collected(&dave, 60).await? == 800,
        "Dave got other than 800"
    );
    let after = changed(phone, held, 30).await?;
    ensure!(
        held - after > 800,
        "{} spent swapping for 800",
        held - after
    );

    phone.tap("Where it is").await?;
    phone.wait("At hubs you have not added", 30).await?;
    phone.tap("Add this hub").await?;
    phone.wait("Add this hub?", 10).await?;
    phone.tap("Add").await?;
    phone.tap("Back").await?;
    let now = changed(phone, after, 60).await?;
    ensure!(now > after, "{now} held once the hub was added");
    Ok(())
}

/// Cashed out to itself: all its notes paid out on Bitcoin to its own
/// address, and kept there rather than moved back into Licash, until it is
/// asked to.
async fn cashed_out(eco: &Eco, phone: &Phone) -> anyhow::Result<()> {
    let pays = || async {
        let chain = eco.swaps_info().await?.chain;
        anyhow::Ok(
            chain
                .context("the provider pays nothing out on Bitcoin")?
                .max,
        )
    };
    if pays().await? < 100_000 {
        let address = eco.exec("swaps", &["licash-swaps", "address"]).await?;
        eco.send_to(String::from_utf8(address)?.trim(), 300_000)
            .await?;
        eco.mine(1).await?;
        until("the provider's coins found", 120, || async {
            Ok((pays().await? >= 100_000).then_some(()))
        })
        .await?;
    }
    let held = phone.balance().await?;
    phone.tap("Where it is").await?;
    phone.tap("Cash out to Bitcoin").await?;
    phone.tap("To this wallet").await?;
    phone.wait("To this wallet, on Bitcoin", 120).await?;
    // In advanced mode (since onboarding), every provider's offer: the
    // stack's only one, the one picked.
    phone.tap_id("providers").await?;
    phone.wait("The only provider", 120).await?;
    phone.wait("Chosen", 10).await?;
    phone.tap("Send").await?;
    phone.wait("Sending", 60).await?;
    phone.tap("Done").await?;
    // Claimed once the provider's coin confirms, then found on Bitcoin:
    // kept there.
    let mine = || async {
        let waiting = eco.core("getmempoolinfo", json!([])).await?["size"].as_u64();
        if waiting.unwrap_or(0) > 0 {
            eco.mine(1).await?;
        }
        anyhow::Ok(())
    };
    let kept = until("the payout kept on Bitcoin", 300, || async {
        mine().await?;
        phone.find("Cashed out, kept on Bitcoin").await
    })
    .await?;
    eprintln!("cashed out, kept: {:?}", kept.text);
    // Rounds later, not moved back in by itself.
    tokio::time::sleep(Duration::from_secs(20)).await;
    mine().await?;
    tokio::time::sleep(Duration::from_secs(10)).await;
    ensure!(
        phone.find("Moving into Licash").await?.is_none(),
        "what was cashed out moved back in by itself"
    );
    phone.wait("Cashed out, kept on Bitcoin", 10).await?;
    // Asked to: moved in, none left on Bitcoin.
    phone.tap("Move into Licash").await?;
    until("all moved back into Licash", 300, || async {
        mine().await?;
        let views = phone.views().await?;
        let busy = views.iter().any(|v| {
            [
                "Cashed out, kept on Bitcoin",
                "Moving into Licash",
                "Coming, waiting for a block",
            ]
            .contains(&v.text.as_str())
        });
        Ok((!busy).then_some(()))
    })
    .await?;
    phone.back().await?;
    let back = phone.balance().await?;
    ensure!(
        back < held && back > held * 8 / 10,
        "{back} held once {held} cashed out and moved back in"
    );
    Ok(())
}

/// A recovery contract at its hub, opened as its user would: with no coin
/// on Bitcoin to pay its price, notes cashed out to itself for it first,
/// as the quote offers; then signed, confirmed and protecting its cap of
/// the notes there, on credit no longer. Its price paid, its change kept
/// on Bitcoin. Closing it, it is told to cash out instead.
async fn contracted(eco: &Eco, phone: &Phone) -> anyhow::Result<()> {
    const PRICE: u64 = 23_000;
    const CAP: u64 = 4_096;
    let mine = || async {
        let waiting = eco.core("getmempoolinfo", json!([])).await?["size"].as_u64();
        if waiting.unwrap_or(0) > 0 {
            eco.mine(1).await?;
        }
        anyhow::Ok(())
    };
    // A note on screen holding `text`, as Home's start with an amount.
    let note = |text: &'static str| async move {
        until(text, 30, || async {
            let views = phone.views().await?;
            Ok(views.into_iter().find(|v| v.text.contains(text)))
        })
        .await
    };
    let on_home = |texts: &'static [&'static str]| async move {
        let views = phone.views().await?;
        anyhow::Ok(
            views
                .iter()
                .any(|v| texts.iter().any(|t| v.text.contains(t))),
        )
    };
    // Enough in notes to pay for it, and to pay with after.
    let code: PaymentCode = phone.code().await?.parse()?;
    let fallback = code.address.context("a fallback address")?;
    let held = phone.balance().await?;
    eco.send_to(&fallback.assume_checked().to_string(), 100_000)
        .await?;
    until("100000 moved into Licash", 300, || async {
        mine().await?;
        let busy = on_home(&[
            "on its way from Bitcoin",
            "moved in once enough",
            "coming on Bitcoin",
        ])
        .await?;
        Ok((!busy && phone.balance().await? > held + 90_000).then_some(()))
    })
    .await?;

    // No coin on Bitcoin pays its price: cashed out to itself first.
    phone.tap("Where it is").await?;
    phone.tap_id("open-contract").await?;
    phone.wait("Open a contract at", 60).await?;
    phone.wait("A mock contract, for testing", 10).await?;
    // A dialog's views carry no ids: by their text.
    phone.tap("Cash out to this wallet").await?;
    phone.wait("To this wallet, on Bitcoin", 120).await?;
    phone.wait("33\u{2009}000 sat", 10).await?;
    phone.tap("Send").await?;
    phone.wait("Sending", 60).await?;
    phone.tap("Done").await?;
    until("the payout kept on Bitcoin", 300, || async {
        mine().await?;
        phone.find("Cashed out, kept on Bitcoin").await
    })
    .await?;
    phone.back().await?;
    let coin = note("cashed out, kept on Bitcoin").await?;
    let coin = licash_e2e::phone::amount(&coin.text);
    let before = phone.balance().await?;
    let credit = licash_e2e::phone::amount(&phone.by_id("credit", 10).await?.text);
    eprintln!("a coin of {coin} kept, {before} held, {credit} on credit");

    // Paid from it now, its change back.
    phone.tap("Where it is").await?;
    phone.tap_id("open-contract").await?;
    phone
        .wait("It costs 23\u{2009}000 sat, paid from a coin of yours", 60)
        .await?;
    phone.tap("Open for 23\u{2009}000 sat").await?;
    phone.wait("Opening a contract", 30).await?;
    let protecting = until("the contract protecting", 900, || async {
        mine().await?;
        let views = phone.views().await?;
        let state = views.into_iter().find(|v| v.id == "contract-state");
        Ok(state.filter(|v| v.text == "Protecting"))
    })
    .await?;
    eprintln!("contract {:?}", protecting.text);
    phone.wait("Mock, for testing", 10).await?;
    // In advanced mode, since onboarding: forcing it, not yet.
    phone.wait("Force close", 10).await?;
    phone.tap_id("close-contract").await?;
    phone.wait("Close this contract?", 10).await?;
    phone
        .wait("A contract closes by itself at block", 10)
        .await?;
    phone.tap("Cancel").await?;
    phone.wait_gone("Close this contract?", 10).await?;
    phone.back().await?;

    phone
        .wait("4\u{2009}096 sat in notes protected by contracts", 30)
        .await?;
    let now = licash_e2e::phone::amount(&phone.by_id("credit", 10).await?.text);
    ensure!(now + CAP == credit, "{now} on credit, {credit} before");
    let after = phone.balance().await?;
    ensure!(after + PRICE == before, "{after} held, {before} before");
    let change = note("cashed out, kept on Bitcoin").await?;
    let change = licash_e2e::phone::amount(&change.text);
    ensure!(change + PRICE == coin, "{change} back of {coin}");
    // Its price in the history, as paid once its funding confirmed.
    phone.tap("Contract opened").await?;
    phone.wait("Opened at", 30).await?;
    phone.wait("On Bitcoin, from a coin of yours", 10).await?;
    phone.back().await?;
    phone.wait("Receive", 10).await?;
    Ok(())
}

/// The other hub, announced on the relay its hub names: found in Settings,
/// offered to add with a tap and a word of warning.
async fn found_hubs(phone: &Phone) -> anyhow::Result<()> {
    phone.tap("Settings").await?;
    phone.tap("Hubs").await?;
    let find = phone.scroll_to("Find hubs").await?;
    phone.tap_view(&find).await?;
    phone.wait("hub2", 60).await?;
    phone.tap_id("add-found").await?;
    phone.wait("Add this hub?", 10).await?;
    // Not now: paid there first, below.
    phone.tap("Cancel").await?;
    phone.wait_gone("Add this hub?", 10).await?;
    phone.tap("Back").await?;
    // In advanced mode, since onboarding: its page listed.
    phone.scroll_to("Advanced mode").await?;
    phone
        .scroll_to("Notes held, the log, deleting the wallet")
        .await?;
    phone.back().await?;
    phone.wait("Receive", 10).await?;
    Ok(())
}

/// Its hub down: paying is under way, and once the hub is back up the app
/// finishes it by itself, to Bob's mailbox, paid once.
async fn hub_down(
    eco: &Eco,
    phone: &Phone,
    bob: &licash_e2e::Wallet,
    bobs: &PaymentCode,
) -> anyhow::Result<()> {
    let held = phone.balance().await?;
    let fee = eco.info(eco.hub(1)).await?.fees.spend.due(held);
    let mut asking = bobs.clone();
    asking.amount = Some(1_000);
    eco.stop("hub1").await?;
    let told = async {
        phone.link(&asking.to_string()).await?;
        phone.tap("Pay").await?;
        phone
            .wait_any(&["Under way", "Sent", "Try again"], 120)
            .await
    }
    .await;
    eco.start("hub1").await?;
    let told = told?;
    ensure!(
        told.text.starts_with("Under way"),
        "told {:?} with its hub down",
        told.text
    );
    phone.tap("Done").await?;
    phone.wait("Receive", 10).await?;

    // Back up: finished and sent on within a few rounds of the app.
    ensure!(
        collected(bob, 180).await? == 1_000,
        "Bob got other than 1000"
    );
    let spent = until("the payment done", 60, || async {
        let spent = held - phone.balance().await?;
        Ok((1_000..=1_000 + fee).contains(&spent).then_some(spent))
    })
    .await?;
    eprintln!("{spent} spent paying 1000 while its hub was down");
    // Once: nothing more for Bob a round later.
    tokio::time::sleep(Duration::from_secs(30)).await;
    let again = wallet::collect(bob, &Route::Direct).await?;
    ensure!(
        again.received.is_empty(),
        "paid twice: {:?}",
        again.received
    );
    Ok(())
}

/// The app's log and the light client's, and both exported from Settings,
/// which shows what the app keeps on the phone.
async fn logs(phone: &Phone) -> anyhow::Result<()> {
    let app = phone.file("files/logs/app.log").await?;
    for want in ["light client of regtest started", "moving to notes"] {
        ensure!(app.contains(want), "no {want:?} in the app's log");
    }
    let chain = phone.file("files/logs/chain.log").await?;
    ensure!(
        chain.contains("LCHN"),
        "no neutrino in the light client's log"
    );
    phone.tap("Settings").await?;
    // What the app keeps on the phone, its light client's and its own
    // code among it.
    phone.tap("Storage").await?;
    phone.scroll_to("Bitcoin light client").await?;
    phone.scroll_to("The app itself").await?;
    phone.back().await?;
    let export = phone.scroll_to("Export logs").await?;
    phone.tap_view(&export).await?;
    phone.wait("licash-logs.txt", 30).await?;
    phone.back().await?;
    Ok(())
}
