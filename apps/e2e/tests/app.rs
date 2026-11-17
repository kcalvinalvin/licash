//! The Android app among the stack of `apps/e2e/run.sh`, on the emulator or
//! phone adb picks, as its user sees it: made, paid on Bitcoin and in
//! Licash, paying, told what went wrong, and started again. Skipped unless
//! `LICASH_E2E_APP` names the app's APK. What it showed and logged on a
//! failure is kept beside the stack's logs.

use std::{path::PathBuf, time::Duration};

use anyhow::{ensure, Context};
use licash_e2e::{
    phone::{amount, Phone, View},
    until, Eco, OUTSIDE,
};
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

/// A hub as the app names it: its host and port.
fn name(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.trim_end_matches('/')
}

/// Whatever waits in the mempool, mined.
async fn mine(eco: &Eco) -> anyhow::Result<()> {
    let waiting = eco.core("getmempoolinfo", json!([])).await?["size"].as_u64();
    if waiting.unwrap_or(0) > 0 {
        eco.mine(1).await?;
    }
    Ok(())
}

/// The history, opened from Home: what it shows.
async fn history(phone: &Phone) -> anyhow::Result<Vec<View>> {
    phone.tap_id("activity").await?;
    // Home says "Activity" too: the history's own list, then.
    phone.by_id("history", 30).await?;
    phone.views().await
}

/// The newest line of the history titled `title`, opened from Home: where
/// it stands is shown first.
async fn opened(phone: &Phone, title: &str) -> anyhow::Result<()> {
    phone.tap_id("activity").await?;
    let line = until(&format!("a line {title:?} in the history"), 30, || async {
        Ok(phone.views().await?.into_iter().find(|v| v.text == title))
    })
    .await?;
    phone.tap_view(&line).await?;
    phone.by_id("state", 30).await?;
    Ok(())
}

/// What a payment to confirm must say before it is agreed to: what it
/// costs, and when it arrives.
async fn reviewed(phone: &Phone, secs: u64) -> anyhow::Result<()> {
    let fee = phone.by_id("fee", secs).await?;
    ensure!(!fee.text.is_empty(), "no fee said");
    let arrives = phone.by_id("arrives", 10).await?;
    ensure!(!arrives.text.is_empty(), "no time of arrival said");
    Ok(())
}

/// Mine whatever waits in the mempool until Home says nothing is under
/// way, and the history shows `moves` payments on Bitcoin moved into the
/// balance.
async fn settle(eco: &Eco, phone: &Phone, moves: usize) -> anyhow::Result<()> {
    let what = format!("{moves} payments on Bitcoin moved into the balance");
    until(&what, 300, || async {
        mine(eco).await?;
        // Home's one line of what is going on: gone once nothing is.
        if phone.has("status").await? {
            return Ok(None);
        }
        let lines = history(phone).await?;
        let moved = lines.iter().filter(|v| v.text == "From Bitcoin").count();
        // On its way, or found on Bitcoin and not moved in yet.
        let busy = lines
            .iter()
            .any(|v| v.id == "pending" || v.text == "Arriving" || v.text == "On Bitcoin");
        phone.back_to("receive").await?;
        Ok((moved == moves && !busy).then_some(()))
    })
    .await
}

/// What was paid on Bitcoin, once found there, added to the balance with
/// no protection, as its owner asks once warned: nothing moves by itself.
async fn move_in(eco: &Eco, phone: &Phone) -> anyhow::Result<()> {
    on_bitcoin(phone).await?;
    until("the payment found on Bitcoin", 300, || async {
        mine(eco).await?;
        Ok(phone.views().await?.into_iter().find(|v| v.id == "move-in"))
    })
    .await?;
    ensure!(
        !phone.has("moving").await?,
        "what was paid on Bitcoin moved in by itself"
    );
    phone.tap_id("move-in").await?;
    phone.wait("Not protected", 10).await?;
    phone.tap_id("move-in-anyway").await?;
    phone.gone("move-in-anyway", 30).await?;
    phone.back_to("onchain").await?;
    phone.back_to("receive").await?;
    Ok(())
}

/// Wait for the balance Home shows to change from `from`: the new one.
async fn changed(phone: &Phone, from: u64, secs: u64) -> anyhow::Result<u64> {
    until("the balance to change", secs, || async {
        let now = phone.balance().await?;
        Ok((now != from).then_some(now))
    })
    .await
}

/// Its silent payment address `silent` alone, one switch away on Receive,
/// for any amount.
async fn silently(phone: &Phone, silent: &str) -> anyhow::Result<()> {
    phone.tap_id("receive").await?;
    phone.tap_id("segment Silent").await?;
    phone.tagged("silent", silent, 30).await?;
    phone.tagged("asking", "Any amount, on Bitcoin", 10).await?;
    phone.tap("Back").await?;
    phone.home(30).await
}

/// Bob paid again, picked by name among the contacts rather than scanned.
async fn contact(phone: &Phone, bob: &licash_e2e::Wallet) -> anyhow::Result<()> {
    let before = phone.balance().await?;
    phone.tap_id("send").await?;
    phone.tap_id("contact Bob").await?;
    until("the amount to type in", 10, || async {
        let views = phone.views().await?;
        Ok(views
            .iter()
            .any(|v| v.id == "amount" && v.focused)
            .then_some(()))
    })
    .await?;
    phone.type_text("1000").await?;
    phone.tagged("arrives", "Instantly", 10).await?;
    phone.tap_id("confirm").await?;
    phone.tagged("result", "Sent", 60).await?;
    phone.tagged("contact", "To Bob", 10).await?;
    phone.tap_id("done").await?;
    ensure!(
        collected(bob, 60).await? == 1_000,
        "Bob got other than 1000"
    );
    changed(phone, before, 30).await?;
    Ok(())
}

/// Paid over Lightning: an invoice its hub makes asking the amount it
/// enters, the hub's fee in it, shown as a QR code, which a node outside
/// pays. Its notes are taken while shown, and the payment is in the
/// history.
async fn lightning(eco: &Eco, phone: &Phone) -> anyhow::Result<()> {
    let before = phone.balance().await?;
    // What the wallet gets of an invoice asking 4000: the most that fits
    // with the hub's fee on it.
    let info = eco.info(eco.hub(1)).await?;
    let receive = info.lightning.context("hub 1 takes no Lightning")?.receive;
    let gets = (0..=4_000u64)
        .rev()
        .find(|n| n + receive.due(*n) <= 4_000)
        .context("no amount fits 4000 with its fee")?;
    phone.tap_id("receive").await?;
    phone.tap_id("segment Lightning").await?;
    phone.tap_id("amount").await?;
    phone.type_text("4000").await?;
    phone.tap_id("done").await?;
    phone.tagged("asking", "4,000 sat", 10).await?;
    // In upper case, as a QR code holds it best.
    let shown = phone.qr(60).await?;
    let request = shown.to_lowercase();
    ensure!(
        request.starts_with("lnbcrt"),
        "{shown} shown for an invoice"
    );
    let paying = eco.pay(OUTSIDE, &request);
    let says = format!("Received {},{:03} sat", gets / 1_000, gets % 1_000);
    phone.tagged("received", &says, 120).await?;
    ensure!(paying.await??, "the node outside did not pay {request}");
    phone.tap("Back").await?;
    phone.home(30).await?;
    let now = changed(phone, before, 30).await?;
    ensure!(
        now == before + gets,
        "{now} held after {gets} of 4000 over Lightning"
    );
    opened(phone, "Received over Lightning").await?;
    phone.tagged("state", "In your balance", 10).await?;
    phone.back().await?;
    phone.back_to("receive").await?;
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
    // Paid at the fallback address: Home says it is on its way, then it
    // is found in a block and moved into the balance once asked, the next
    // address shown from then on.
    eco.send_to(&fallback, 30_000).await?;
    phone.tagged("status", "Receiving 30,000 sat", 60).await?;
    eco.mine(1).await?;
    move_in(eco, phone).await?;
    settle(eco, phone, 1).await?;
    let moved = phone.balance().await?;
    // Less the hub's 1000 and the fees of the move on Bitcoin.
    ensure!(
        (25_000..29_000).contains(&moved),
        "{moved} of 30000 moved in"
    );
    // Opened, the move shows what it cost: the hub's 1000 and the miners'.
    opened(phone, "From Bitcoin").await?;
    phone.tagged("state", "In your balance", 10).await?;
    let hubs = phone.by_id("fee-hub", 10).await?;
    ensure!(hubs.text == "1,000 sat", "a hub fee of {:?}", hubs.text);
    phone.by_id("fee-network", 10).await?;
    phone.back().await?;
    phone.back_to("receive").await?;
    let next: PaymentCode = phone.code().await?.parse()?;
    ensure!(next.address != code.address, "paid at, and shown again");
    ensure!(next.silent == code.silent, "another silent payment address");

    // Paid at its silent payment address: found and moved in too.
    eco.pay_silent(&silent.to_string(), 20_000).await?;
    eco.mine(1).await?;
    move_in(eco, phone).await?;
    settle(eco, phone, 2).await?;
    let held = phone.balance().await?;
    ensure!(
        (15_000..19_000).contains(&(held - moved)),
        "{} moved in",
        held - moved
    );
    silently(phone, &silent.to_string()).await?;

    // It pays Bob at the hub, an amount it enters, to his mailbox.
    let bob = eco.wallet("bob")?;
    bob.add_hub(hub).await?;
    let bobs = bob.payment_code(&[hub.to_owned()], false)?;
    phone.link(&bobs.to_string()).await?;
    // Typed straight in: the field has the focus, untouched.
    until("the amount to type in", 10, || async {
        let views = phone.views().await?;
        Ok(views
            .iter()
            .any(|v| v.id == "amount" && v.focused)
            .then_some(()))
    })
    .await?;
    phone.type_text("5000").await?;
    // What it costs, as the wallet would pay it, and when it arrives,
    // before it is paid.
    let fee = until("the fee of 5000", 10, || async {
        let views = phone.views().await?;
        let fee = views.into_iter().find(|v| v.id == "fee");
        Ok(fee.and_then(|v| match v.text.as_str() {
            "None" => Some(0),
            text => text
                .strip_suffix(" sat")?
                .replace(',', "")
                .parse::<u64>()
                .ok(),
        }))
    })
    .await?;
    eprintln!("paying 5000, fee: {fee}");
    phone.tagged("arrives", "Instantly", 10).await?;
    phone.tap_id("confirm").await?;
    phone.tagged("result", "Sent", 60).await?;
    // Bob, kept as a contact once paid.
    phone.tap_id("save-contact").await?;
    phone.fill_id("contact-name", "Bob").await?;
    phone.tap_id("save-contact-name").await?;
    phone.tagged("contact", "To Bob", 10).await?;
    phone.tap_id("done").await?;
    ensure!(
        collected(&bob, 60).await? == 5_000,
        "Bob got other than 5000"
    );
    let after = changed(phone, held, 30).await?;
    // At most what was shown: the wallet pays no more than it quoted, the
    // notes it makes counted.
    ensure!(
        (5_000..=5_000 + fee).contains(&(held - after)),
        "{} spent paying 5000",
        held - after
    );

    // Bob pays its code back, kept at the hub for the app, which takes it
    // there by itself: nothing mailed.
    let code: PaymentCode = phone.code().await?.parse()?;
    let paid = wallet::pay(&bob, &code, Some(3_000)).await?;
    ensure!(paid.paid.kept, "not kept at the hub: {paid:?}");
    let now = changed(phone, after, 90).await?;
    ensure!(now == after + 3_000, "{now} held after 3000 received");
    let lines = history(phone).await?;
    ensure!(
        lines.iter().any(|v| v.text == "Received"),
        "no payment received in the history"
    );
    phone.back_to("receive").await?;
    contact(phone, &bob).await?;

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
    logs(phone).await?;
    restored(phone, hub, &silent.to_string()).await
}

/// Backed up from Settings, deleted and brought back from the first
/// screen: from its backup file, all it held; from its 24 words, its keys,
/// the same silent payment address.
async fn restored(phone: &Phone, hub: &str, silent: &str) -> anyhow::Result<()> {
    phone.launch().await?;
    let held = phone.balance().await?;
    phone.tap("Settings").await?;
    phone.tap_id("backup").await?;
    phone.tap_id("backup-words").await?;
    phone.tap_id("show-words").await?;
    phone.by_id("word 24", 30).await?;
    let views = phone.views().await?;
    let words = (1..=24)
        .map(|i| {
            let word = views.iter().find(|v| v.id == format!("word {i}"));
            Ok(word.with_context(|| format!("no word {i}"))?.text.clone())
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    phone.tap_id("words-done").await?;
    phone.tap_id("backup-file").await?;
    phone.tap_id("save-backup").await?;
    // The system's file picker, the file's name suggested.
    picked(phone, |text| text.eq_ignore_ascii_case("save")).await?;
    phone.tap_id("backup-done").await?;
    deleted(phone).await?;

    phone.tap_id("restore-wallet").await?;
    phone.tap_id("restore-file").await?;
    picked(phone, |text| text.starts_with("licash-regtest-backup-")).await?;
    phone.home(120).await?;
    phone.wait_balance(held, 60).await?;
    deleted(phone).await?;

    phone.tap_id("restore-wallet").await?;
    phone.tap_id("restore-words").await?;
    // A few words at a time: typed all at once, the last ones can be lost.
    phone.tap_id("phrase").await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    for few in words.chunks(6) {
        phone.type_text(&format!("{} ", few.join(" "))).await?;
    }
    phone.hide_keyboard().await?;
    phone.wait("24 of 24", 10).await?;
    phone.tap_id("restore").await?;
    // Its keys only: no hub until one is added again.
    phone.by_id("choose-hub", 120).await?;
    phone.connect(hub).await?;
    silently(phone, silent).await
}

/// In the system's file picker, tap the first view whose text `wanted`.
async fn picked(phone: &Phone, wanted: impl Fn(&str) -> bool) -> anyhow::Result<()> {
    let view = until("the file picker", 60, || async {
        Ok(phone.views().await?.into_iter().find(|v| wanted(&v.text)))
    })
    .await?;
    phone.tap_view(&view).await
}

/// The wallet deleted from Settings' advanced page, from Home or a page
/// of Settings: the first screen shown again.
async fn deleted(phone: &Phone) -> anyhow::Result<()> {
    phone.launch().await?;
    phone.tap("Settings").await?;
    phone.tap_id("advanced").await?;
    let delete = phone.scroll_to_id("delete-wallet").await?;
    phone.tap_view(&delete).await?;
    phone.tap_id("delete").await?;
    phone.by_id("create", 60).await?;
    Ok(())
}

/// The question the app asks, or what it says went wrong, starting with
/// `text`, waiting up to `secs` for it.
async fn asked(phone: &Phone, text: &str, secs: u64) -> anyhow::Result<()> {
    phone.tagged("ask", text, secs).await?;
    Ok(())
}

/// What it cannot pay: told why, nothing spent.
async fn refused(eco: &Eco, phone: &Phone, bobs: &PaymentCode) -> anyhow::Result<()> {
    let held = phone.balance().await?;
    let mut asking = bobs.clone();
    asking.amount = Some(10_000_000);
    phone.link(&asking.to_string()).await?;
    phone.tap_id("confirm").await?;
    asked(phone, "Not enough", 60).await?;
    phone.tap_id("cancel").await?;

    phone.link("bitcoin:?licash=nonsense").await?;
    asked(phone, "Neither a payment code", 30).await?;
    phone.tap_id("cancel").await?;

    // A code naming only a hub it does not use.
    let carol = eco.wallet("carol")?;
    carol.add_hub(eco.hub(2)).await?;
    let carols = carol.payment_code(&[eco.hub(2).to_owned()], false)?;
    phone.link(&carols.to_string()).await?;
    asked(phone, "Their hub is not one of yours", 30).await?;
    phone.tap_id("cancel").await?;
    phone.home(10).await?;
    ensure!(phone.balance().await? == held, "spent refusing");
    Ok(())
}

/// Where the money is, from Home, then the page of the hub at `url`.
async fn at_hub(phone: &Phone, url: &str) -> anyhow::Result<()> {
    phone.tap_id("balance").await?;
    phone.by_id("onchain", 30).await?;
    phone.tap(name(url)).await?;
    phone.by_id("cash-out", 30).await?;
    Ok(())
}

/// Where the money is, from Home, then what is on Bitcoin.
async fn on_bitcoin(phone: &Phone) -> anyhow::Result<()> {
    phone.tap_id("balance").await?;
    phone.tap_id("onchain").await?;
    phone.wait("On Bitcoin", 30).await?;
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
    asked(phone, "Their hub is not one of yours", 30).await?;
    phone.tap_id("confirm").await?;
    // At a hub not added, notes moved there first: both said.
    phone.by_id("not-added", 120).await?;
    phone.by_id("moves", 10).await?;
    reviewed(phone, 10).await?;
    phone.tap_id("confirm").await?;
    phone.tagged("result", "Sent", 180).await?;
    phone.tap_id("done").await?;
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

    // What is left there, apart: added with a tap and a word of warning.
    phone.tap_id("balance").await?;
    phone.tap_id("visited").await?;
    phone
        .wait("Left over from paying someone at this hub", 10)
        .await?;
    phone.tap_id("add-visited").await?;
    // The sheet closed, then its hub among those used.
    phone.gone("add-visited", 30).await?;
    phone.by_id("onchain", 30).await?;
    phone.gone("visited", 60).await?;
    phone.back_to("receive").await?;
    let now = changed(phone, after, 60).await?;
    ensure!(now > after, "{now} held once the hub was added");
    Ok(())
}

/// Cashed out to itself: all its notes paid out on Bitcoin to its own
/// address, and kept there rather than moved back into the balance, until
/// it is asked to.
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
    at_hub(phone, eco.hub(1)).await?;
    phone.tap_id("cash-out").await?;
    phone.tap_id("to-own").await?;
    phone.tagged("to", "This wallet", 120).await?;
    reviewed(phone, 10).await?;
    // In advanced mode (since onboarding), every provider's offer: the
    // stack's only one, the one picked.
    phone.tap_id("providers").await?;
    phone.wait("The only provider", 120).await?;
    phone.wait("Chosen", 10).await?;
    phone.tap_id("confirm").await?;
    phone.tagged("result", "Sending", 60).await?;
    phone.tap_id("done").await?;
    // Under way, watched in a foreground service: finished out of sight too.
    until("the watching notification", 30, || async {
        Ok(phone.notifying("watching").await?.then_some(()))
    })
    .await?;
    // Home tells it is under way, no block mined yet; opened from there,
    // where it stands, at one of its first steps.
    phone.back_to("onchain").await?;
    phone.back_to("receive").await?;
    let going = phone.tagged("status", "Sending", 30).await?;
    phone.tap_view(&going).await?;
    let stands = phone.tagged("state", "On its way", 30).await?;
    eprintln!("cashing out, on Home: {:?}, {:?}", going.text, stands.text);
    phone.tap_id("more").await?;
    phone.scroll_to_id("step locked").await?;
    phone.back_to("receive").await?;
    on_bitcoin(phone).await?;
    // Claimed once the provider's coin confirms, then found on Bitcoin:
    // kept there.
    let kept = until("the payout kept on Bitcoin", 300, || async {
        mine(eco).await?;
        Ok(phone.views().await?.into_iter().find(|v| v.id == "kept"))
    })
    .await?;
    eprintln!("cashed out, kept: {:?}", kept.text);
    // Rounds later, not moved back in by itself.
    tokio::time::sleep(Duration::from_secs(20)).await;
    mine(eco).await?;
    tokio::time::sleep(Duration::from_secs(10)).await;
    ensure!(
        !phone.has("moving").await?,
        "what was cashed out moved back in by itself"
    );
    phone.by_id("kept", 10).await?;
    // Asked to, once warned: moved in, none left on Bitcoin.
    phone.tap_id("move-in").await?;
    phone.tap_id("move-in-anyway").await?;
    until("all moved back into the balance", 300, || async {
        mine(eco).await?;
        let views = phone.views().await?;
        let busy = views
            .iter()
            .any(|v| ["kept", "moving", "coming"].contains(&v.id.as_str()));
        Ok((!busy).then_some(()))
    })
    .await?;
    phone.back_to("onchain").await?;
    phone.back_to("receive").await?;
    let back = phone.balance().await?;
    ensure!(
        back < held && back > held * 8 / 10,
        "{back} held once {held} cashed out and moved back in"
    );
    // In the history, arrived, each of its steps there once asked for.
    opened(phone, "Sent on Bitcoin").await?;
    phone.tagged("state", "Arrived", 60).await?;
    phone.tap_id("more").await?;
    for step in [
        "step locked",
        "step coin",
        "step coin-confirmed",
        "step claimed",
        "step arrived",
    ] {
        phone.scroll_to_id(step).await?;
    }
    phone.back().await?;
    phone.back_to("receive").await?;
    Ok(())
}

/// Protection at its hub, a recovery contract, set up as its user would:
/// with no coin on Bitcoin to pay its price, notes cashed out to itself
/// for it first, as the quote offers; then of the cap and length it
/// chooses, signed, confirmed and protecting its cap of the notes there,
/// on credit no longer. Its price paid, its change kept on Bitcoin. Ending it, it is told to cash out
/// instead.
async fn contracted(eco: &Eco, phone: &Phone) -> anyhow::Result<()> {
    /// The most keeping its notes few costs while it is set up, in sat.
    const NOTES_UPKEEP: u64 = 50;
    // hub1's fixed price, and over it 1% of the cap a year for terms its
    // holders choose (licash-hub's --contracts-rate by default).
    const FIXED: u64 = 23_000;
    let price = |cap: u64, blocks: u32| {
        FIXED + licash_api::rate(10_000, u32::try_from(cap).unwrap_or(u32::MAX), blocks)
    };
    /// What a mock contract is worth, said wherever one shows.
    const MOCK: &str = "Test only: protects nothing";
    // Enough in notes to pay for it, and to pay with after.
    let code: PaymentCode = phone.code().await?.parse()?;
    let fallback = code.address.context("a fallback address")?;
    let held = phone.balance().await?;
    eco.send_to(&fallback.assume_checked().to_string(), 100_000)
        .await?;
    // Too little for protection worth its price: none offered, moved in
    // with none.
    move_in(eco, phone).await?;
    ensure!(!phone.has("offer").await?, "protection offered for 100000");
    until("100000 moved into the balance", 300, || async {
        mine(eco).await?;
        // Home's one line of what is going on: gone once nothing is.
        let busy = phone.has("status").await?;
        Ok((!busy && phone.balance().await? > held + 90_000).then_some(()))
    })
    .await?;

    // No coin on Bitcoin pays its price: cashed out to itself first.
    at_hub(phone, eco.hub(1)).await?;
    phone.tap_id("open-contract").await?;
    // Its terms chosen all the same: at first covering the notes there,
    // here the least.
    let all = amount(&phone.by_id("quote-cap", 60).await?.text);
    ensure!(all > 90_000, "a cap of {all} offered on the notes held");
    phone.slide_id("contract-cap", 0.0).await?;
    phone.tagged("quote-cap", "4,096 sat", 10).await?;
    phone.tagged("quote-price", "23,000 sat", 10).await?;
    phone.tagged("worth", MOCK, 10).await?;
    phone.tap_id("cash-out-first").await?;
    phone.tagged("to", "This wallet", 120).await?;
    phone.tagged("paying", "33,000 sat", 10).await?;
    reviewed(phone, 10).await?;
    phone.tap_id("confirm").await?;
    phone.tagged("result", "Sending", 60).await?;
    phone.tap_id("done").await?;
    phone.back_to("onchain").await?;
    phone.tap_id("onchain").await?;
    let coin = until("the payout kept on Bitcoin", 300, || async {
        mine(eco).await?;
        Ok(phone.views().await?.into_iter().find(|v| v.id == "kept"))
    })
    .await?;
    let coin = amount(&coin.text);
    phone.back_to("onchain").await?;
    let credit = amount(&phone.by_id("credit", 10).await?.text);
    phone.back_to("receive").await?;
    let before = phone.balance().await?;
    eprintln!("a coin of {coin} kept, {before} held, {credit} on credit");

    // Paid from it now, its change back, of terms chosen within hub1's
    // range: at first a cap of half again the coin, over the longest hub1
    // allows, priced as hub1 prices them as they change.
    at_hub(phone, eco.hub(1)).await?;
    phone.tap_id("open-contract").await?;
    let cap = amount(&phone.by_id("quote-cap", 60).await?.text);
    ensure!(cap == coin + coin / 2, "a cap of {cap} offered on {coin}");
    let costs = |blocks: u32| async move {
        let want = price(cap, blocks);
        until(&format!("{want} for {blocks} blocks"), 10, || async {
            let views = phone.views().await?;
            let shown = views.into_iter().find(|v| v.id == "quote-price");
            Ok(shown.filter(|v| amount(&v.text) == want).map(|_| ()))
        })
        .await
    };
    costs(1_000).await?;
    // Its shortest costs less, at once; the longest again, to protect.
    phone.slide_id("contract-blocks", 0.0).await?;
    costs(200).await?;
    phone.slide_id("contract-blocks", 1.0).await?;
    costs(1_000).await?;
    let paid = price(cap, 1_000);
    ensure!(paid > price(cap, 200) && paid > FIXED, "{paid} for {cap}");
    phone.tagged("worth", MOCK, 10).await?;
    phone.tap_id("protect").await?;
    // Being set up: said at once, then the step it is at.
    until("protection being set up", 30, || async {
        let views = phone.views().await?;
        Ok(views
            .into_iter()
            .find(|v| v.id == "opening" || v.id == "contract-step"))
    })
    .await?;
    let protecting = until("the contract protecting", 900, || async {
        mine(eco).await?;
        let views = phone.views().await?;
        let state = views.into_iter().find(|v| v.id == "contract-state");
        Ok(state.filter(|v| v.text == "Protecting"))
    })
    .await?;
    eprintln!("contract {:?}", protecting.text);
    // What it is worth, in sight without a tap.
    phone.tagged("worth", MOCK, 10).await?;
    // Unfolded; in advanced mode, since onboarding: forcing it, not yet.
    phone.tap_view(&protecting).await?;
    // Below its details: scrolled to, as often as the page moves.
    phone.scroll_to_id("force-close").await?;
    let end = phone.scroll_to_id("close-contract").await?;
    phone.tap_view(&end).await?;
    // It ends by itself: to stop relying on the hub, cash out.
    phone.by_id("end-cash-out", 10).await?;
    phone.back().await?;
    phone.gone("end-cash-out", 10).await?;
    phone.scroll_to_id("close-contract").await?;
    phone.back_to("onchain").await?;

    // Its cap of the notes protected, past what is on Bitcoin, which is.
    until("its cap protected", 30, || async {
        let views = phone.views().await?;
        let of = |id: &str| views.iter().find(|v| v.id == id).map(|v| amount(&v.text));
        let (protected, onchain) = (of("protected"), of("onchain"));
        Ok((protected.is_some() && protected == onchain.map(|o| o + cap)).then_some(()))
    })
    .await?;
    let now = amount(&phone.by_id("credit", 10).await?.text);
    phone.tap_id("onchain").await?;
    // Less than hub1 takes, its change stays on Bitcoin, moved in or not.
    let change = amount(&phone.by_id("kept", 30).await?.text);
    ensure!(change + paid == coin, "{change} back of {coin}");
    phone.back_to("onchain").await?;
    phone.back_to("receive").await?;
    let after = phone.balance().await?;
    // Less its price, and what keeping its notes few may have cost meanwhile
    // (a few sat, at times): off its credit too, with its cap.
    let upkeep = (before - paid).checked_sub(after);
    ensure!(
        upkeep.is_some_and(|fee| fee <= NOTES_UPKEEP),
        "{after} held, {before} before"
    );
    let upkeep = upkeep.unwrap_or_default();
    ensure!(
        now + cap + upkeep == credit,
        "{now} on credit, {credit} before, {upkeep} on notes"
    );
    // Home says some of it is protected.
    let share = phone.by_id("protection", 10).await?;
    ensure!(
        share.text.ends_with("% protected"),
        "{:?} on Home with a contract protecting",
        share.text
    );
    // Its price in the history, as paid once its funding confirmed.
    opened(phone, "Protection").await?;
    let shown = amount(&phone.by_id("amount", 10).await?.text);
    ensure!(shown == paid, "{shown} paid for it in the history");
    phone.tagged("state", "Set up", 30).await?;
    phone.by_id("at", 10).await?;
    phone.tagged("paid-from", "Your Bitcoin", 10).await?;
    phone.back().await?;
    phone.back_to("receive").await?;
    Ok(())
}

/// The other hub, announced on the relay its hub names: found in Settings,
/// offered to add with a tap and a word of warning.
async fn found_hubs(phone: &Phone) -> anyhow::Result<()> {
    phone.tap("Settings").await?;
    phone.tap_id("hubs").await?;
    let find = phone.scroll_to_id("find-hubs").await?;
    phone.tap_view(&find).await?;
    phone.wait("hub2", 60).await?;
    phone.tap_id("add-found").await?;
    // Which it is, and what adding it means, before it is added.
    let url = phone.by_id("found-url", 10).await?;
    ensure!(url.text.contains("://"), "{:?} for its address", url.text);
    phone
        .wait("Add it only if you trust who runs it", 10)
        .await?;
    phone.by_id("add-hub", 10).await?;
    // Not now: paid there first, below.
    phone.back().await?;
    phone.gone("add-hub", 10).await?;
    phone.tap("Back").await?;
    // In advanced mode, since onboarding: its page listed.
    phone.scroll_to_id("toggle Advanced mode").await?;
    phone.scroll_to_id("advanced").await?;
    phone.back_to("receive").await?;
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
        phone.tap_id("confirm").await?;
        // Said to be under way, or sent, or asked to try again.
        until("what became of paying", 120, || async {
            let views = phone.views().await?;
            Ok(views
                .into_iter()
                .find(|v| v.id == "result" || v.text == "Try again"))
        })
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
    phone.tap_id("done").await?;
    phone.home(10).await?;

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

/// The app's log and the light client's, and both exported from About in
/// Settings, which shows what the app keeps on the phone.
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
    phone.tap_id("about").await?;
    // What the app keeps on the phone, its light client's and its own
    // code among it, in a sheet.
    phone.tap_id("storage").await?;
    phone.by_id("storage-total", 30).await?;
    phone.scroll_to_id("storage-chain").await?;
    phone.scroll_to_id("storage-app").await?;
    phone.back().await?;
    let export = phone.scroll_to_id("export-logs").await?;
    phone.tap_view(&export).await?;
    phone.wait("licash-logs.txt", 30).await?;
    phone.back().await?;
    Ok(())
}
