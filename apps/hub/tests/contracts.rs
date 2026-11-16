//! Contracts opened between a wallet and the hub service over HTTP, on a
//! regtest node whose wallet funds F: under the mock authority, so they
//! protect nothing. Needs `bitcoind` (`LNGAP_BITCOIND`, as
//! `regtest/run.sh` sets it) and a release build, as the openings build
//! and check their garbled instances:
//!
//! ```sh
//! LNGAP_BITCOIND=$PWD/../regtest/bitcoind-docker.sh \
//!   cargo test --release -p licash-hub-service --test contracts -- --ignored
//! ```

use std::path::Path;
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use bitcoin::Amount;
use licash::spend::ChainProfile;
use licash_chain_client::Source;
use licash_contracts::holder::{self, Ask, Change, Contract, Fate, Node, Phase};
use licash_contracts::hub::terms::Range;
use licash_contracts::hub::Openings;
use licash_hub::{Hub, Policy};
use licash_hub_service::{deposit, watch, Services};
use licash_transport::Http;
use licash_wallet::{DepositState, Transport as _, Wallet};
use lngap_btc::regtest::Regtest;

/// A hub service opening contracts, funded by `rt`'s wallet: its URL.
async fn hub(rt: &Regtest, dir: &Path) -> Result<(String, Source)> {
    hub_with(rt, dir, None).await
}

/// As [`hub`], holders choosing their terms within `range`, if any.
async fn hub_with(rt: &Regtest, dir: &Path, range: Option<Range>) -> Result<(String, Source)> {
    let (url, source, _) = hub_serving(rt, dir, range).await?;
    Ok((url, source))
}

/// As [`hub_with`], and the hub it serves, for a test to pay with.
async fn hub_serving(
    rt: &Regtest,
    dir: &Path,
    range: Option<Range>,
) -> Result<(String, Source, Arc<Hub>)> {
    std::fs::create_dir_all(dir)?;
    let (seed, db) = (dir.join("seed"), dir.join("hub.sqlite"));
    Hub::init(&seed, &db, 12)?;
    let hub = Arc::new(Hub::open(&seed, &db, Policy::default())?);
    let url = format!(
        "{}/wallet/harness?cookie={}",
        rt.rpc_url(),
        rt.cookie().display()
    );
    let source: Source = url.parse()?;
    let keyset = hub.info().keysets[0].clone();
    let openings = Openings::with_wallet(dir, source.clone(), source.clone(), keyset, range);
    let openings = openings.await?;
    let services = Services {
        contracts: Some(openings),
        ..Default::default()
    };
    let app = licash_hub_service::router(hub.clone(), services);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((url, source, hub))
}

/// What the wallet finds on the chain to the tip: its coins' total.
async fn scan(wallet: &Wallet<Http>, chain: &Node) -> Result<u64> {
    wallet.scan_silent(None, chain, 10_000).await?;
    Ok(wallet.silent_coins()?.iter().map(|c| c.value).sum())
}

fn ask() -> Ask {
    Ask {
        blocks: 300,
        max_price: 30_000,
        cap: None,
        deposit: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_wallet_opens_contracts_at_a_hub_and_sees_them_confirm() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let (url, source) = hub(&rt, &dir.path().join("hub")).await?;
    let chain = Node(source);

    // A wallet trusting the hub, which says it opens mock contracts.
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let seed = [7; 32];
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &seed,
        regtest,
        Http::new(None)?,
    )?;
    let info = wallet.add_hub(&url).await?;
    let offered = info.contracts.context("no contracts in the hub's info")?;
    ensure!(offered.mock && offered.cap == 4_096 && offered.price == 23_000);
    ensure!(holder::list(&mine)?.is_empty());
    ensure!(!mine.join("contracts.sqlite").exists(), "made by a list");

    // Two coins of hers on Bitcoin, at her fallback addresses.
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [100_000, 60_000] {
        let to = wallet.fallback_address()?.script_pubkey();
        rt.fund(&to, Amount::from_sat(sats))?;
    }
    rt.mine(1)?;
    ensure!(scan(&wallet, &chain).await? == 160_000);

    // Asked to pay more than she would: refused, nothing kept.
    let cheap = Ask {
        max_price: 22_999,
        ..ask()
    };
    let refused = holder::open(&wallet, &chain, &mine, &url, cheap).await;
    ensure!(format!("{:#}", refused.unwrap_err()).contains("23000 sat to open"));

    // Opened on the smaller coin: F signed whole, broadcast, her coin
    // reserved for it.
    let tip = rt.height()?;
    let first = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    ensure!(first.phase == Phase::Funded, "{first:?}");
    ensure!(first.mock && first.cap == 4_096 && first.price == 23_000);
    ensure!(first.value == 60_000 && first.abandoned.is_none());
    ensure!((first.close_height, first.assert_height) == (tip + 300, tip + 160));
    let f = first.funding.context("no F")?;
    ensure!(first.fate == Some(Fate::Open), "{first:?}");
    let reserved = wallet.silent_coins()?;
    ensure!(reserved
        .iter()
        .any(|c| c.outpoint == first.deposit && c.spending == Some(f)));

    // A second contract at the same hub, on her other coin.
    let second = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    ensure!(second.phase == Phase::Funded && second.value == 100_000);
    ensure!(second.id != first.id && second.deposit != first.deposit);

    // Her coin's reservation lost, as a wallet rewound past it loses it:
    // kept for F again once the contracts resume.
    wallet.abandon_silent(&f)?;
    let kept = |coins: Vec<licash_wallet::SilentCoin>| {
        coins
            .iter()
            .any(|c| c.outpoint == first.deposit && c.spending == Some(f))
    };
    ensure!(!kept(wallet.silent_coins()?));
    holder::resume(&wallet, &chain, &mine).await?;
    ensure!(kept(wallet.silent_coins()?), "not kept again");

    // Both F in a block: live, her change back at her fallback address.
    rt.mine(1)?;
    let contracts = holder::resume(&wallet, &chain, &mine).await?;
    let tip = rt.height()?;
    let live = |c: &Contract| c.fate == Some(Fate::Live) && c.live(tip);
    ensure!(
        contracts.len() == 2 && contracts.iter().all(live),
        "{contracts:?}"
    );
    ensure!(contracts[0].id == second.id, "not the newest first");
    let found: serde_json::Value = chain
        .0
        .rpc(
            "getrawtransaction",
            serde_json::json!([f.to_string(), true]),
        )
        .await?;
    ensure!(found["confirmations"] == 1, "{found}");
    ensure!(scan(&wallet, &chain).await? == 77_000 + 37_000);

    // Nothing more to do for them, as stored.
    let again = holder::resume(&wallet, &chain, &mine).await?;
    ensure!(again == contracts);
    ensure!(holder::list(&mine)? == contracts);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_wallet_opens_a_contract_of_terms_it_chose_on_its_large_coin() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let range = Range {
        max_cap: 50_000_000,
        max_blocks: 5_000,
        per_coin: 2,
        ppm_year: 10_000,
    };
    let (url, source) = hub_with(&rt, &dir.path().join("hub"), Some(range)).await?;
    let chain = Node(source);
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &[8; 32],
        regtest,
        Http::new(None)?,
    )?;

    // Its fixed terms, as apps installed open on, and the range beside.
    let info = wallet.add_hub(&url).await?;
    let offered = info.contracts.context("no contracts in the hub's info")?;
    ensure!(offered.cap == 4_096 && offered.price == 23_000 && offered.blocks == 1_000);
    let chosen = offered.range.context("no range in the hub's info")?;
    ensure!((chosen.min_blocks, chosen.max_blocks, chosen.per_coin) == (200, 5_000, 2));
    // 15 M sat over 2 000 blocks: 1% a year of it over the fixed price.
    let price = chosen.price(15_000_000, 2_000);
    ensure!(price == 23_000 + 5_707, "{price}");

    // Asked of terms she chose: those exactly, a graph as wide as them.
    let offer = holder::offer(&wallet, &url, Some((15_000_000, 2_000))).await?;
    ensure!(offer.quote.cap == 15_000_000 && offer.bounds.lock == 2_000);
    ensure!(offer.quote.graph.bits == 24 && offer.price() == price);
    // Out of its range, or half asked: refused.
    let refused = holder::offer(&wallet, &url, Some((4_096, 199))).await;
    let refused = refused.err().context("offered 199 blocks")?;
    ensure!(format!("{refused:#}").contains("status 400"), "{refused:#}");
    let half = format!("{url}{}?cap=5000", licash_contracts::QUOTE);
    let half = wallet.transport().request(&half, None).await;
    let half = half.map_err(anyhow::Error::msg)?;
    ensure!(half.status == 400, "{}", half.status);

    // Her 10 M sat coin and a small one.
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [10_000_000, 60_000] {
        let to = wallet.fallback_address()?.script_pubkey();
        rt.fund(&to, Amount::from_sat(sats))?;
    }
    rt.mine(1)?;
    ensure!(scan(&wallet, &chain).await? == 10_060_000);
    let small = wallet.silent_coins()?;
    let small = small
        .iter()
        .find(|c| c.value == 60_000)
        .context("no coin")?;

    // 15 M sat on her small coin: more than twice it, refused before
    // asking the hub; nothing kept.
    let ask = Ask {
        blocks: 2_000,
        max_price: price,
        cap: Some(15_000_000),
        deposit: Some(small.outpoint),
    };
    let refused = holder::open(&wallet, &chain, &mine, &url, ask).await;
    let refused = format!("{:#}", refused.unwrap_err());
    ensure!(refused.contains("7500000 at least"), "{refused}");
    ensure!(holder::list(&mine)?.is_empty());

    // On the coin large enough, the smallest that is: F funded whole.
    let tip = rt.height()?;
    let ask = Ask {
        deposit: None,
        ..ask
    };
    let first = holder::open(&wallet, &chain, &mine, &url, ask).await?;
    ensure!(first.phase == Phase::Funded, "{first:?}");
    ensure!(first.cap == 15_000_000 && first.price == price && first.value == 10_000_000);
    ensure!((first.close_height, first.assert_height) == (tip + 2_000, tip + 1_860));

    // Of the fixed terms beside it, on her small coin, as an app
    // installed asks.
    let fixed = Ask {
        blocks: 300,
        max_price: 23_000,
        cap: None,
        deposit: None,
    };
    let second = holder::open(&wallet, &chain, &mine, &url, fixed).await?;
    ensure!(second.cap == 4_096 && second.price == 23_000 && second.value == 60_000);

    // Both live; the hub's collateral for hers 2^24 - 1 sat at least.
    rt.mine(1)?;
    let contracts = holder::resume(&wallet, &chain, &mine).await?;
    let tip = rt.height()?;
    ensure!(contracts
        .iter()
        .all(|c| c.fate == Some(Fate::Live) && c.live(tip)));
    let f = first.funding.context("no F")?;
    let found: serde_json::Value = chain
        .0
        .rpc(
            "getrawtransaction",
            serde_json::json!([f.to_string(), true]),
        )
        .await?;
    let outputs = found["vout"].as_array().context("no outputs")?;
    let sats = |o: &serde_json::Value| (o["value"].as_f64().unwrap_or(0.0) * 1e8).round() as u64;
    let most = outputs.iter().map(sats).max().unwrap_or(0);
    ensure!(most >= (1 << 24) - 1, "{found}");
    Ok(())
}

/// A payment of `amount` to `to` at `hub`, of seed file `seed`, its fee
/// paid, from notes its keys made for someone outside any wallet.
fn pay(
    hub: &Hub,
    seed: &Path,
    url: &str,
    to: &licash::address::Address,
    amount: u64,
) -> Result<licash_api::Payment> {
    use licash::address::{Ephemeral, OwnerKeys};
    use licash::authorization::{Authorization, SignedProposal};
    use licash::claim::Branch;
    use licash::note::{Evidence, IssuerKeys, Note};
    use licash::spend::{Output, Proposal};
    use licash::unlink::Nonce;

    let split = |mut amount: u64| {
        let mut denoms = Vec::new();
        while amount > 0 {
            let d = 1u64 << (63 - amount.leading_zeros());
            denoms.push(d);
            amount -= d;
        }
        denoms
    };
    let text = std::fs::read_to_string(seed)?;
    let mut bytes = [0; 32];
    hex::decode_to_slice(text.trim(), &mut bytes)?;
    let keys = IssuerKeys::from_seed(&bytes, 0, 12)?;
    let keyset = keys.keyset()?;
    let info = hub.info();
    let mut total = amount;
    while amount + info.policy.spend.due(total) != total {
        total = amount + info.policy.spend.due(total);
    }
    let payer = OwnerKeys::from_seed(&[0xfa; 32])?;
    let mut inputs = Vec::new();
    for denom in split(total) {
        let d = Ephemeral::random().derive(&payer.address(), denom, Branch::Pay, 0)?;
        let output = Output::from_derivation(&d)?;
        let proof = keys.prove_blinded(denom, output.blinded())?;
        let note = Note::from_parts(
            &keyset,
            *keyset.id(),
            d.message().clone(),
            proof.open_point(&keyset, &d)?,
            Evidence::Linked {
                dleq: proof.dleq(),
                blinding: *d.blinding(),
                commit: Nonce::new(&keyset, &output)?.commit()?,
            },
        )?;
        inputs.push((note, d));
    }
    let notes: Vec<Note> = inputs.iter().map(|(note, _)| note.clone()).collect();
    let ephemeral = Ephemeral::random();
    let to_hub = split(total - amount)
        .into_iter()
        .map(|d| (d, &info.address));
    let outputs = split(amount)
        .into_iter()
        .map(|d| (d, to))
        .chain(to_hub)
        .enumerate()
        .map(|(i, (d, to))| Output::from_derivation(&ephemeral.derive(to, d, Branch::Pay, i)?))
        .collect::<Result<Vec<_>, _>>()?;
    let proposal = Proposal::new(&keyset, &notes, ephemeral.public(), outputs, None)?;
    let grant = hub.offer(&proposal, &notes)?;
    let owners = inputs
        .iter()
        .map(|(_, d)| d.ownership().owner_secret(payer.spend_secret()))
        .collect::<Result<Vec<_>, _>>()?;
    let signed = SignedProposal::sign(
        &keyset,
        proposal,
        &notes,
        Authorization::Granted(Box::new(grant)),
        &owners,
    )?;
    let unlock = hub.release(&signed, &notes)?;
    Ok(licash_api::Payment::new(url, &signed, &notes, &unlock))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_holder_forces_her_contract_closed_on_a_silent_hub_and_takes_her_notes_value(
) -> Result<()> {
    use bitcoin::FeeRate;
    use licash_contracts::holder::recovery::{self, Stage};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    // A hub service that answers no request: silent.
    let (url, source, hub) = hub_serving(&rt, &at, None).await?;
    let chain = Node(source);
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &[9; 32],
        regtest,
        Http::new(None)?,
    )?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [60_000, 50_000] {
        let to = wallet.fallback_address()?.script_pubkey();
        rt.fund(&to, Amount::from_sat(sats))?;
    }
    rt.mine(1)?;
    ensure!(scan(&wallet, &chain).await? == 110_000);
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    let rate = FeeRate::from_sat_per_vb(2).context("a rate")?;

    // 3 000 sat of notes at the hub: 7 of them.
    let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 3_000)?;
    wallet.receive(&payment)?;
    let available =
        |w: &Wallet<Http>| -> Result<u64> { Ok(w.balance()?.iter().map(|b| b.available).sum()) };
    ensure!(available(&wallet)? == 3_000);

    // Not before F is buried: nothing taken.
    let early = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await;
    let early = format!("{:#}", early.unwrap_err());
    ensure!(early.contains("not buried"), "{early}");
    ensure!(available(&wallet)? == 3_000 && recovery::recoveries(&mine)?.is_empty());
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;

    // Forced closed: her notes claimed, out of her balance; revocable
    // until R may be in the next block, after its lock from F.
    let forced = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    ensure!(
        forced.stage == Stage::Claimed && !forced.irrevocable,
        "{forced:?}"
    );
    ensure!((forced.claimed, forced.notes) == (3_000, 7), "{forced:?}");
    ensure!(forced.error.is_none(), "{forced:?}");
    ensure!(available(&wallet)? == 0);
    let lock_end = forced.next_at.context("no lock")?;
    ensure!(lock_end > rt.height()? + 1, "{forced:?}");
    // Given up: her notes back. Then forced closed again.
    recovery::give_up(&wallet, &mine, &opened.id)?;
    ensure!(available(&wallet)? == 3_000 && recovery::recoveries(&mine)?.is_empty());
    let forced = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    ensure!(
        forced.stage == Stage::Claimed && forced.claimed == 3_000,
        "{forced:?}"
    );
    ensure!(available(&wallet)? == 0);
    // Again: the same one, carried on.
    let again = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    ensure!(again == forced, "{again:?}");

    rt.mine_to_height(lock_end - 2)?;
    let held = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(
        held[0].stage == Stage::Claimed && held[0].error.is_none(),
        "{held:?}"
    );
    // R may be in the next block: irrevocable, R and P broadcast.
    rt.mine(1)?;
    let sent = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(
        sent[0].stage == Stage::Published && sent[0].irrevocable,
        "{sent:?}"
    );
    ensure!(sent[0].error.is_none(), "{sent:?}");
    let given_up = recovery::give_up(&wallet, &mine, &opened.id);
    ensure!(format!("{:#}", given_up.unwrap_err()).contains("past giving up"));

    // R and P in a block: the hub's window runs.
    rt.mine(1)?;
    let tip = rt.height()?;
    let all = recovery::advance(&wallet, &chain, &mine, rate).await?;
    let requested = &all[0];
    ensure!(
        all.len() == 1 && requested.stage == Stage::Requested,
        "{all:?}"
    );
    ensure!(requested.requested_at == Some(tip), "{requested:?}");
    let window_end = requested.next_at.context("no window")?;
    ensure!(window_end > tip + 1, "{requested:?}");

    // Nothing to do until it passes; then request_timeout, paid of P's
    // fee coin.
    rt.mine_to_height(window_end - 2)?;
    let waiting = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(waiting[0].stage == Stage::Requested, "{waiting:?}");
    rt.mine(1)?;
    let timing_out = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(timing_out[0].stage == Stage::TimingOut, "{timing_out:?}");
    ensure!(timing_out[0].error.is_none(), "{timing_out:?}");
    rt.mine(1)?;
    let timed_out = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(timed_out.stage == Stage::TimedOut, "{timed_out:?}");
    ensure!(
        timed_out.timed_out_at == Some(rt.height()?),
        "{timed_out:?}"
    );

    // Her bits and reserve, swept to her wallet once their delay passed.
    let delay_end = timed_out.next_at.context("no delay")?;
    rt.mine_to_height(delay_end - 2)?;
    let held = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(held[0].stage == Stage::TimedOut, "{held:?}");
    let before = scan(&wallet, &chain).await?;
    rt.mine(1)?;
    let sweeping = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(sweeping.stage == Stage::Sweeping, "{sweeping:?}");
    let sweep = sweeping.sweep.context("no sweep")?;
    rt.mine(1)?;
    let done = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(
        done.stage == Stage::Done && done.swept_at == Some(rt.height()?),
        "{done:?}"
    );
    let found: serde_json::Value = chain
        .0
        .rpc(
            "getrawtransaction",
            serde_json::json!([sweep.to_string(), true]),
        )
        .await?;
    let paid = found["vout"][0]["value"].as_f64().context("no output")?;
    let paid = (paid * 1e8).round() as u64;
    let after = scan(&wallet, &chain).await?;
    ensure!(
        after == before + paid,
        "{before} then {after}, {paid} swept"
    );
    // Her claim's bits and her reserve, less the sweep's fee.
    ensure!(paid > 3_000, "{paid} swept");
    eprintln!("swept {paid} sat to her wallet");
    // Nothing more to do.
    let over = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(over[0] == *done, "{over:?}");
    Ok(())
}

/// The response window makes a timeout eligible, but does not expire the
/// hub's answer. A holder must still be ready to prove after that window.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hubs_answer_can_confirm_after_its_response_window() -> Result<()> {
    use bitcoin::FeeRate;
    use licash_contracts::holder::recovery::{self, Stage};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let (url, source, hub, openings) = hub_answering(&rt, &at).await?;
    let chain = Node(source);
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &[19; 32],
        ChainProfile::Core30DefaultRegtest,
        Http::new(None)?,
    )?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [60_000, 50_000] {
        rt.fund(
            &wallet.fallback_address()?.script_pubkey(),
            Amount::from_sat(sats),
        )?;
    }
    rt.mine(1)?;
    scan(&wallet, &chain).await?;
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    wallet.receive(&pay(
        &hub,
        &at.join("seed"),
        &url,
        &wallet.address(),
        3_000,
    )?)?;
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;
    let rate = FeeRate::from_sat_per_vb(2).context("a rate")?;
    let forced = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    rt.mine_to_height(forced.next_at.context("no lock")? - 1)?;
    recovery::advance(&wallet, &chain, &mine, rate).await?;
    rt.mine(1)?;
    let requested = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(requested[0].stage == Stage::Requested, "{requested:?}");
    let window_end = requested[0].next_at.context("no window")?;
    let r = rt.get_tx(&requested[0].request.context("no request")?)?;

    // No timeout is broadcast while the holder is offline. The response
    // confirms strictly after the earliest timeout block.
    rt.mine_to_height(window_end + 1)?;
    let answer = openings.respond(&opened.id, &r).await?;
    rt.send_raw(&answer)?;
    rt.mine(1)?;
    let observed = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(observed[0].error.is_none(), "{observed:?}");
    ensure!(observed[0].stage.disputed(), "{observed:?}");
    ensure!(
        observed[0].response == Some(answer.compute_txid()),
        "{observed:?}"
    );
    ensure!(observed[0].timeout.is_none(), "{observed:?}");
    Ok(())
}

/// The daemon needs no scan cursor and an upgraded opening needs no
/// rewrite. A late first poll and a restart still use just one answer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hub_automatically_answers_and_restarts_with_the_same_one_time_answer() -> Result<()> {
    use bitcoin::FeeRate;
    use licash_contracts::holder::recovery::{self, Stage};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let (url, source, hub, openings) = hub_answering(&rt, &at).await?;
    let chain = Node(source.clone());
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &[29; 32],
        ChainProfile::Core30DefaultRegtest,
        Http::new(None)?,
    )?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [60_000, 50_000] {
        rt.fund(
            &wallet.fallback_address()?.script_pubkey(),
            Amount::from_sat(sats),
        )?;
    }
    rt.mine(1)?;
    scan(&wallet, &chain).await?;
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    wallet.receive(&pay(
        &hub,
        &at.join("seed"),
        &url,
        &wallet.address(),
        3_000,
    )?)?;
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;
    let rate = FeeRate::from_sat_per_vb(2).context("a rate")?;
    let forced = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    rt.mine_to_height(forced.next_at.context("no lock")? - 1)?;
    recovery::advance(&wallet, &chain, &mine, rate).await?;
    // No signature for R while it is only in the mempool (depth 0).
    ensure!(
        openings.answer_requests().await?
            == licash_contracts::hub::ResponseRound {
                answered: 0,
                failed: 0
            }
    );
    ensure!(!at.join("responses.sqlite").exists());
    rt.mine(1)?;
    let requested = recovery::advance(&wallet, &chain, &mine, rate).await?;
    let r = rt.get_tx(&requested[0].request.context("no request")?)?;
    rt.mine_to_height(requested[0].next_at.context("no window")? + 1)?;

    // A broken unrelated row must not hide the actual request.
    let db = rusqlite::Connection::open(at.join("openings.sqlite"))?;
    db.execute("INSERT INTO openings SELECT ?1, format, phase, mock, irrevocable, ?2, coins, last, x'00', abandoned, created_at, updated_at FROM openings WHERE id = ?3", rusqlite::params![[99_u8; 32], vec![99_u8; 36], opened.id])?;
    ensure!(
        openings.answer_requests().await?
            == licash_contracts::hub::ResponseRound {
                answered: 1,
                failed: 1
            }
    );
    let answer = openings.respond(&opened.id, &r).await?;
    // Manual fallback shares and returns the exact same stored answer.
    let manual = licash_contracts::hub::respond(&at, &source, &opened.id, &r).await?;
    ensure!(manual == answer);
    let restarted = Openings::with_wallet(
        &at,
        source.clone(),
        source,
        hub.info().keysets[0].clone(),
        None,
    )
    .await?;
    ensure!(
        restarted.answer_requests().await?
            == licash_contracts::hub::ResponseRound {
                answered: 1,
                failed: 1
            }
    );
    ensure!(restarted.respond(&opened.id, &r).await? == answer);
    rt.mine(1)?;
    ensure!(
        restarted.answer_requests().await?
            == licash_contracts::hub::ResponseRound {
                answered: 0,
                failed: 1
            }
    );
    let observed = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(observed[0].error.is_none(), "{observed:?}");
    ensure!(observed[0].stage.disputed(), "{observed:?}");
    ensure!(
        observed[0].response == Some(answer.compute_txid()),
        "{observed:?}"
    );
    ensure!(observed[0].stage != Stage::Lost);
    Ok(())
}

/// As [`hub_serving`], and its openings, for a test to answer a holder's
/// request with.
async fn hub_answering(
    rt: &Regtest,
    dir: &Path,
) -> Result<(String, Source, Arc<Hub>, Arc<Openings>)> {
    hub_answering_with(rt, dir, None).await
}

/// As [`hub_answering`], holders choosing their terms within `range`, if
/// any.
async fn hub_answering_with(
    rt: &Regtest,
    dir: &Path,
    range: Option<Range>,
) -> Result<(String, Source, Arc<Hub>, Arc<Openings>)> {
    std::fs::create_dir_all(dir)?;
    let (seed, db) = (dir.join("seed"), dir.join("hub.sqlite"));
    Hub::init(&seed, &db, 12)?;
    let hub = Arc::new(Hub::open(&seed, &db, Policy::default())?);
    let url = format!(
        "{}/wallet/harness?cookie={}",
        rt.rpc_url(),
        rt.cookie().display()
    );
    let source: Source = url.parse()?;
    let keyset = hub.info().keysets[0].clone();
    let openings = Openings::with_wallet(dir, source.clone(), source.clone(), keyset, range);
    let openings = openings.await?;
    let services = Services {
        contracts: Some(openings.clone()),
        ..Default::default()
    };
    let app = licash_hub_service::router(hub.clone(), services);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((url, source, hub, openings))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_holder_proves_her_claim_against_a_hub_that_answered_and_takes_her_notes_value(
) -> Result<()> {
    use bitcoin::FeeRate;
    use licash_contracts::holder::recovery::{self, Stage};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let (url, source, hub, openings) = hub_answering(&rt, &at).await?;
    let chain = Node(source);
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &[9; 32],
        regtest,
        Http::new(None)?,
    )?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [60_000, 50_000] {
        let to = wallet.fallback_address()?.script_pubkey();
        rt.fund(&to, Amount::from_sat(sats))?;
    }
    rt.mine(1)?;
    ensure!(scan(&wallet, &chain).await? == 110_000);
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    let rate = FeeRate::from_sat_per_vb(2).context("a rate")?;
    let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 3_000)?;
    wallet.receive(&payment)?;
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;

    // Forced closed; R and P in a block once revocable no more.
    let forced = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    ensure!(forced.stage == Stage::Claimed, "{forced:?}");
    rt.mine_to_height(forced.next_at.context("no lock")? - 1)?;
    let sent = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(sent[0].stage == Stage::Published, "{sent:?}");
    rt.mine(1)?;
    let requested = recovery::advance(&wallet, &chain, &mine, rate).await?;
    let requested = &requested[0];
    ensure!(requested.stage == Stage::Requested, "{requested:?}");

    // No statement to prove, nor proof taken, before the hub answers.
    ensure!(recovery::statement(&mine, &opened.id).is_err());
    let early = recovery::import_proof(&wallet, &chain, &mine, &opened.id, b"{}", rate).await;
    ensure!(early.is_err(), "{early:?}");

    // The hub answers: her proof decides.
    let r = rt.get_tx(&requested.request.context("no R")?)?;
    let answer = openings.respond(&opened.id, &r).await?;
    let wrong = openings.respond(&opened.id, &answer).await;
    ensure!(wrong.is_err(), "an answer to its own answer");
    rt.send_raw(&answer)?;
    rt.mine(1)?;
    let answered = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(answered.error.is_none(), "{answered:?}");
    ensure!(
        answered.response == Some(answer.compute_txid()) && answered.timeout.is_none(),
        "{answered:?}"
    );
    ensure!(
        matches!(answered.stage, Stage::Answered | Stage::Asserting),
        "{answered:?}"
    );
    let deadline = answered.deadline;

    // Her statement built once R and P are deep enough, her proof made
    // under the mock authority, her assert broadcast, then in a block.
    let mut asserted = answered.clone();
    for _ in 0..20 {
        if asserted.stage == Stage::Asserted {
            break;
        }
        ensure!(asserted.error.is_none(), "{asserted:?}");
        rt.mine(1)?;
        asserted = recovery::advance(&wallet, &chain, &mine, rate).await?[0].clone();
    }
    ensure!(asserted.stage == Stage::Asserted, "{asserted:?}");
    ensure!(asserted.error.is_none(), "{asserted:?}");
    ensure!(
        asserted.proved && asserted.assertion.is_some(),
        "{asserted:?}"
    );
    let deadline = deadline.or(asserted.deadline).context("no deadline")?;
    ensure!(
        asserted.asserted_at.is_some_and(|h| h <= deadline),
        "{asserted:?}"
    );
    let kept = mine
        .join("contracts")
        .join(hex::encode(opened.id))
        .join("statement.input");
    ensure!(kept.exists(), "no statement kept at {}", kept.display());
    // What another machine proves: her statement's input, whole.
    let statement = recovery::statement(&mine, &opened.id)?;
    ensure!(statement == std::fs::read(&kept)?);
    licash::statement::StatementInput::from_bytes(&statement)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Another proof is refused: not an export, of another program.
    let refused = |e: Result<recovery::Recovery>| format!("{:#}", e.unwrap_err());
    let garbage = recovery::import_proof(&wallet, &chain, &mine, &opened.id, b"[", rate).await;
    ensure!(refused(garbage).contains("not a proof's export"));
    let other = serde_json::json!({
        "image_id": hex::encode([0u8; 32]),
        "journal": "",
        "proof": hex::encode([0u8; 128]),
    });
    let other = serde_json::to_vec(&other)?;
    let other = recovery::import_proof(&wallet, &chain, &mine, &opened.id, &other, rate).await;
    ensure!(refused(other).contains("another program"));

    // The hub's chance to challenge passes: success, of P's fee coin.
    let chance_end = asserted.next_at.context("no chance")?;
    rt.mine_to_height(chance_end - 2)?;
    let held = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(held[0].stage == Stage::Asserted, "{held:?}");
    rt.mine(1)?;
    let succeeding = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(succeeding.stage == Stage::Succeeding, "{succeeding:?}");
    ensure!(succeeding.error.is_none(), "{succeeding:?}");
    ensure!(succeeding.payout.is_some(), "{succeeding:?}");
    rt.mine(1)?;
    let paid = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(
        paid.stage == Stage::Paid && paid.paid_at == Some(rt.height()?),
        "{paid:?}"
    );

    // Her bits and reserve, swept to her wallet once their delay passed.
    let delay_end = paid.next_at.context("no delay")?;
    rt.mine_to_height(delay_end - 2)?;
    let held = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(held[0].stage == Stage::Paid, "{held:?}");
    let before = scan(&wallet, &chain).await?;
    rt.mine(1)?;
    let sweeping = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(sweeping.stage == Stage::Sweeping, "{sweeping:?}");
    let sweep = sweeping.sweep.context("no sweep")?;
    rt.mine(1)?;
    let done = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(
        done.stage == Stage::Done && done.swept_at == Some(rt.height()?),
        "{done:?}"
    );
    let swept = rt.get_tx(&sweep)?.output[0].value.to_sat();
    let after = scan(&wallet, &chain).await?;
    ensure!(
        after == before + swept,
        "{before} then {after}, {swept} swept"
    );
    ensure!(swept > 3_000, "{swept} swept");
    eprintln!("swept {swept} sat to her wallet after her proof");
    let over = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(over[0] == *done, "{over:?}");
    Ok(())
}

/// A wallet of `seed` in `dir`'s `wallet`, trusting the hub at `url`,
/// with coins of `coins` sat on Bitcoin, in a block: its directory too.
async fn holder_wallet(
    rt: &Regtest,
    chain: &Node,
    dir: &Path,
    url: &str,
    seed: [u8; 32],
    coins: &[u64],
) -> Result<(std::path::PathBuf, Wallet<Http>)> {
    let mine = dir.join("wallet");
    std::fs::create_dir(&mine)?;
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &seed,
        regtest,
        Http::new(None)?,
    )?;
    wallet.add_hub(url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    for sats in coins {
        let to = wallet.fallback_address()?.script_pubkey();
        rt.fund(&to, Amount::from_sat(*sats))?;
    }
    rt.mine(1)?;
    ensure!(scan(&wallet, chain).await? == coins.iter().sum::<u64>());
    Ok((mine, wallet))
}

fn available(wallet: &Wallet<Http>) -> Result<u64> {
    Ok(wallet.balance()?.iter().map(|b| b.available).sum())
}

/// What each contract in `mine` protecting notes at `tip` would claim of
/// `wallet`'s notes now, by id ([`holder::coverage`]): none of one under
/// recovery.
fn recoverable(
    wallet: &Wallet<Http>,
    mine: &Path,
    tip: u32,
) -> Result<std::collections::HashMap<[u8; 32], u64>> {
    holder::coverage(wallet, mine, &holder::list(mine)?, tip)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_holder_recovers_her_cap_of_notes_beyond_it_from_a_hub_that_answered() -> Result<()> {
    use bitcoin::FeeRate;
    use licash_contracts::holder::recovery::{self, Stage};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let range = Range {
        max_cap: 50_000_000,
        max_blocks: 5_000,
        per_coin: 2,
        ppm_year: 10_000,
    };
    let (url, source, hub, openings) = hub_answering_with(&rt, &at, Some(range)).await?;
    let chain = Node(source);
    let (mine, wallet) =
        holder_wallet(&rt, &chain, dir.path(), &url, [10; 32], &[60_000, 50_000]).await?;
    let chosen = Ask {
        cap: Some(5_000),
        ..ask()
    };
    let opened = holder::open(&wallet, &chain, &mine, &url, chosen).await?;
    ensure!(opened.cap == 5_000, "{opened:?}");
    let rate = FeeRate::from_sat_per_vb(2).context("a rate")?;
    // Three notes of 2 048 sat, the hub's largest: two recover 4 096 of
    // her cap, three all of it.
    for _ in 0..3 {
        let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 2_048)?;
        wallet.receive(&payment)?;
    }
    ensure!(available(&wallet)? == 6_144);
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;
    // What it protects: its cap, of the three.
    let each = recoverable(&wallet, &mine, rt.height()?)?;
    ensure!(each.get(&opened.id) == Some(&5_000), "{each:?}");

    // Forced closed: the notes claimed for her cap, their excess lost.
    let forced = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    ensure!(forced.stage == Stage::Claimed, "{forced:?}");
    ensure!((forced.claimed, forced.notes) == (5_000, 3), "{forced:?}");
    ensure!(available(&wallet)? == 0);
    rt.mine_to_height(forced.next_at.context("no lock")? - 1)?;
    let sent = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(sent[0].stage == Stage::Published, "{sent:?}");
    rt.mine(1)?;
    let requested = recovery::advance(&wallet, &chain, &mine, rate).await?;
    let requested = &requested[0];
    ensure!(requested.stage == Stage::Requested, "{requested:?}");

    // The hub answers a claim of less than her notes' face: her proof
    // decides, and is taken.
    let r = rt.get_tx(&requested.request.context("no R")?)?;
    let answer = openings.respond(&opened.id, &r).await?;
    rt.send_raw(&answer)?;
    rt.mine(1)?;
    let mut now = recovery::advance(&wallet, &chain, &mine, rate).await?[0].clone();
    ensure!(now.response == Some(answer.compute_txid()), "{now:?}");
    for _ in 0..20 {
        if now.stage == Stage::Asserted {
            break;
        }
        ensure!(now.error.is_none(), "{now:?}");
        rt.mine(1)?;
        now = recovery::advance(&wallet, &chain, &mine, rate).await?[0].clone();
    }
    ensure!(now.stage == Stage::Asserted && now.proved, "{now:?}");
    ensure!(now.error.is_none(), "{now:?}");

    // Paid once the hub's chance to challenge passes, then swept.
    rt.mine_to_height(now.next_at.context("no chance")? - 1)?;
    let succeeding = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(succeeding.stage == Stage::Succeeding, "{succeeding:?}");
    ensure!(succeeding.error.is_none(), "{succeeding:?}");
    rt.mine(1)?;
    let paid = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(
        paid.stage == Stage::Paid && paid.claimed == 5_000,
        "{paid:?}"
    );
    rt.mine_to_height(paid.next_at.context("no delay")? - 1)?;
    let sweeping = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(sweeping.stage == Stage::Sweeping, "{sweeping:?}");
    rt.mine(1)?;
    let swept = rt.get_tx(&sweeping.sweep.context("no sweep")?)?.output[0].value;
    let swept = swept.to_sat();
    ensure!(swept > 5_000, "{swept} swept");
    eprintln!("swept {swept} sat to her wallet of a claim of 5000");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn her_contracts_at_a_hub_each_claim_their_share_of_her_notes() -> Result<()> {
    use bitcoin::FeeRate;
    use licash_contracts::holder::recovery::{self, Stage};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let (url, source, hub) = hub_serving(&rt, &at, None).await?;
    let chain = Node(source);
    let coins = [100_000, 60_000];
    let (mine, wallet) = holder_wallet(&rt, &chain, dir.path(), &url, [11; 32], &coins).await?;
    let mut both = [
        holder::open(&wallet, &chain, &mine, &url, ask()).await?,
        holder::open(&wallet, &chain, &mine, &url, ask()).await?,
    ];
    // The one asserting first served first: by id, of the same deadline.
    both.sort_by_key(|c| (c.assert_height, c.id));
    let [first, later] = both;
    ensure!(first.cap == 4_096 && later.cap == 4_096);
    let rate = FeeRate::from_sat_per_vb(2).context("a rate")?;
    // 4 096 + 2 048 + 1 024: within their caps together.
    let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 7_168)?;
    wallet.receive(&payment)?;
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;
    let tip = rt.height()?;
    let each = recoverable(&wallet, &mine, tip)?;
    let shares = (each.get(&first.id), each.get(&later.id));
    ensure!(shares == (Some(&4_096), Some(&3_072)), "{each:?}");

    // The later forced closed first: its share only, not the largest
    // note within its cap, which the first would then miss.
    let forced = recovery::force_close(&wallet, &chain, &mine, &later.id, rate).await?;
    ensure!(forced.stage == Stage::Claimed, "{forced:?}");
    ensure!((forced.claimed, forced.notes) == (3_072, 2), "{forced:?}");
    ensure!(available(&wallet)? == 4_096);
    let each = recoverable(&wallet, &mine, tip)?;
    let shares = (each.get(&first.id), each.get(&later.id));
    ensure!(shares == (Some(&4_096), None), "{each:?}");
    let forced = recovery::force_close(&wallet, &chain, &mine, &first.id, rate).await?;
    ensure!((forced.claimed, forced.notes) == (4_096, 1), "{forced:?}");
    ensure!(available(&wallet)? == 0);

    // Both given up: her notes back.
    recovery::give_up(&wallet, &mine, &later.id)?;
    recovery::give_up(&wallet, &mine, &first.id)?;
    ensure!(available(&wallet)? == 7_168);
    Ok(())
}

/// A hub service opening contracts as [`hub_serving`], of notes up to
/// 2^30 sat, taking deposits of up to 2 M sat for 1000 sat each, scanning
/// `rt` once [`followed`]: its URL, chain, it and its deposits.
async fn hub_taking_deposits(
    rt: &Regtest,
    dir: &Path,
) -> Result<(String, Source, Arc<Hub>, Arc<deposit::Gateway>)> {
    std::fs::create_dir_all(dir)?;
    let (seed, db) = (dir.join("seed"), dir.join("hub.sqlite"));
    Hub::init(&seed, &db, 30)?;
    let hub = Arc::new(Hub::open(&seed, &db, Policy::default())?);
    let url = format!(
        "{}/wallet/harness?cookie={}",
        rt.rpc_url(),
        rt.cookie().display()
    );
    let source: Source = url.parse()?;
    let keyset = hub.info().keysets[0].clone();
    let openings = Openings::with_wallet(dir, source.clone(), source.clone(), keyset, None);
    let config = deposit::Config {
        max: 2_000_000,
        ..deposit::Config::new(bitcoin::ScriptBuf::new_op_return([]))
    };
    let regtest = ChainProfile::Core30DefaultRegtest;
    let path = dir.join("deposits.sqlite");
    let deposits = deposit::Gateway::open(hub.clone(), source.clone(), regtest, config, &path)?;
    let deposits = Arc::new(deposits);
    let services = Services {
        contracts: Some(openings.await?),
        deposits: Some(deposits.clone()),
        ..Default::default()
    };
    let app = licash_hub_service::router(hub.clone(), services);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((url, source, hub, deposits))
}

/// `hub` scanned to the tip of `source`, then its `deposits`.
async fn followed(hub: &Arc<Hub>, source: &Source, deposits: &deposit::Gateway) -> Result<()> {
    for _ in 0..1_000 {
        if watch::step(hub, source).await? {
            deposits.round().await;
            return Ok(());
        }
    }
    anyhow::bail!("the hub never reached the tip")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_wallet_sweeps_its_coin_into_a_contract_and_notes_its_change_on_bitcoin_if_too_much(
) -> Result<()> {
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let (url, source, hub, deposits) = hub_taking_deposits(&rt, &dir.path().join("hub")).await?;
    let chain = Node(source.clone());
    followed(&hub, &source, &deposits).await?;
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &[9; 32],
        regtest,
        Http::new(None)?,
    )?;
    let info = wallet.add_hub(&url).await?;
    let taken = info.deposits.context("no deposits in the hub's info")?;
    ensure!(taken.max == 2_000_000 && taken.fee.base == 1_000 && taken.fee.ppm == 0);

    // Her 1 M sat coin and a 3 M sat one.
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [1_000_000, 3_000_000] {
        let to = wallet.fallback_address()?.script_pubkey();
        rt.fund(&to, Amount::from_sat(sats))?;
    }
    rt.mine(1)?;
    followed(&hub, &source, &deposits).await?;
    ensure!(scan(&wallet, &chain).await? == 4_000_000);
    let coins = wallet.silent_coins()?;
    let small = coins.iter().find(|c| c.value == 1_000_000);
    let small = small.context("no coin")?.outpoint;

    // Opened on the small one, her change into notes: F pays a deposit
    // of hers exactly, 977 000 sat, the hub's 1000 in it.
    let swept = Ask {
        deposit: Some(small),
        ..ask()
    };
    let first = holder::open_with(&wallet, &chain, &mine, &url, swept, Change::Notes).await?;
    ensure!(
        first.phase == Phase::Funded && first.price == 23_000,
        "{first:?}"
    );
    let asked = wallet.deposits()?;
    ensure!(asked.len() == 1, "{asked:?}");
    let terms = asked[0].terms.clone().context("no terms")?;
    ensure!(asked[0].amount == 976_000 && terms.pays == 977_000);
    ensure!(asked[0].state == DepositState::Open, "{asked:?}");
    let f = first.funding.context("no F")?;
    let found: serde_json::Value = chain
        .0
        .rpc(
            "getrawtransaction",
            serde_json::json!([f.to_string(), true]),
        )
        .await?;
    let outputs = found["vout"].as_array().context("no outputs")?;
    let paid = outputs.iter().any(|o| {
        o["scriptPubKey"]["address"] == terms.address.as_str()
            && (o["value"].as_f64().unwrap_or(0.0) * 1e8).round() as u64 == 977_000
    });
    ensure!(paid, "{found}");

    // F confirmed at depth: her notes locked, then claimed; no coin of
    // hers left on Bitcoin but the other.
    let mut state = DepositState::Open;
    for _ in 0..12 {
        rt.mine(1)?;
        followed(&hub, &source, &deposits).await?;
        state = wallet.follow_deposit(&asked[0].hash).await?.state;
        if state == DepositState::Received {
            break;
        }
    }
    ensure!(state == DepositState::Received, "{state:?}");
    let held: u64 = wallet.balance()?.iter().map(|b| b.available).sum();
    ensure!(held == 976_000, "{held}");
    let contracts = holder::resume(&wallet, &chain, &mine).await?;
    ensure!(contracts[0].fate == Some(Fate::Live), "{contracts:?}");
    ensure!(scan(&wallet, &chain).await? == 3_000_000);
    // Its payment found in F, for a refund had its notes never come.
    let address: bitcoin::Address<_> = terms.address.parse()?;
    let script = address.assume_checked().script_pubkey();
    let tip = rt.height()?;
    let into = holder::paid_into(&wallet, &mine, &script, 977_000, tip)?;
    ensure!(into.map(|o| o.txid) == Some(f), "{into:?}");
    ensure!(holder::paid_into(&wallet, &mine, &script, 977_001, tip)?.is_none());

    // On the large one, its change more than the hub takes: on Bitcoin,
    // as asked otherwise, no deposit asked for.
    let second = holder::open_with(&wallet, &chain, &mine, &url, ask(), Change::Notes).await?;
    ensure!(second.phase == Phase::Funded && second.value == 3_000_000);
    ensure!(wallet.deposits()?.len() == 1);
    rt.mine(1)?;
    let contracts = holder::resume(&wallet, &chain, &mine).await?;
    ensure!(contracts.iter().all(|c| c.fate == Some(Fate::Live)));
    ensure!(scan(&wallet, &chain).await? == 2_977_000);
    Ok(())
}

/// `licash-hub respond`, beside the running hub: the operator answers her
/// request from the openings alone, and she sees its answer on Bitcoin.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn an_operator_answers_a_holders_request_beside_the_running_hub() -> Result<()> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    use bitcoin::FeeRate;
    use licash_contracts::holder::recovery::{self, Stage};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let (url, source, hub, _openings) = hub_answering(&rt, &at).await?;
    let chain = Node(source);
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &[9; 32],
        regtest,
        Http::new(None)?,
    )?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [60_000, 50_000] {
        let to = wallet.fallback_address()?.script_pubkey();
        rt.fund(&to, Amount::from_sat(sats))?;
    }
    rt.mine(1)?;
    ensure!(scan(&wallet, &chain).await? == 110_000);
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    let rate = FeeRate::from_sat_per_vb(2).context("a rate")?;
    let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 3_000)?;
    wallet.receive(&payment)?;
    rt.mine(1)?;
    let live = holder::resume(&wallet, &chain, &mine).await?;
    let funding = live[0].funding.context("no F")?;

    let forced = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    rt.mine_to_height(forced.next_at.context("no lock")? - 1)?;
    recovery::advance(&wallet, &chain, &mine, rate).await?;
    rt.mine(1)?;
    let requested = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(requested[0].stage == Stage::Requested, "{requested:?}");
    let r = rt.get_tx(&requested[0].request.context("no R")?)?;
    let r_hex = bitcoin::consensus::encode::serialize_hex(&r);

    let rpc = format!(
        "{}/wallet/harness?cookie={}",
        rt.rpc_url(),
        rt.cookie().display()
    );
    let respond = |id: &str, request: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_licash-hub"));
        command
            .arg("--data")
            .arg(&at)
            .args(["respond", id, request, "--chain", &rpc])
            .env_remove("LICASH_HUB_SEED")
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    };

    // Refused for an opening it does not have, or what is not R.
    let file = dir.path().join("r.hex");
    std::fs::write(&file, &r_hex)?;
    let unknown = respond(&"00".repeat(32), &file.to_string_lossy()).output()?;
    ensure!(!unknown.status.success(), "{unknown:?}");
    let id = licash_contracts::hex_id(&opened.id);
    let other = dir.path().join("f.hex");
    let f = rt.get_tx(&funding)?;
    std::fs::write(&other, bitcoin::consensus::encode::serialize_hex(&f))?;
    let wrong = respond(&id, &other.to_string_lossy()).output()?;
    ensure!(!wrong.status.success(), "{wrong:?}");

    // Its answer to R, read from stdin, broadcast and printed.
    let mut child = respond(&id, "-").spawn()?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(r_hex.as_bytes())?;
    let out = child.wait_with_output()?;
    ensure!(out.status.success(), "{out:?}");
    let txid: bitcoin::Txid = String::from_utf8(out.stdout)?.trim().parse()?;
    rt.mine(1)?;
    let answered = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(answered.error.is_none(), "{answered:?}");
    ensure!(
        answered.response == Some(txid) && answered.timeout.is_none(),
        "{answered:?}"
    );
    ensure!(
        matches!(answered.stage, Stage::Answered | Stage::Asserting),
        "{answered:?}"
    );
    Ok(())
}

/// A provider's keeper, a round at a time: what the provider takes in at
/// the hub held to its contracts there, the one ending replaced in time,
/// and forced closed at its safe deadline only once its successor cannot
/// recover every note held there.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_providers_keeper_holds_its_intake_to_its_contracts_and_replaces_them() -> Result<()> {
    use std::collections::HashMap;

    use licash_contracts::holder::{recovery, safety};
    use licash_swaps::{Config as SwapsConfig, Provider};
    use licash_swaps_service::keeper::{Alert, Deadlines, Keeper, Settings};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let (url, source, hub) = hub_serving(&rt, &at, None).await?;
    let mine = dir.path().join("provider");
    std::fs::create_dir(&mine)?;
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &[11; 32],
        regtest,
        Http::new(None)?,
    )?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [60_000, 60_000] {
        let to = wallet.fallback_address()?.script_pubkey();
        rt.fund(&to, Amount::from_sat(sats))?;
    }
    rt.mine(1)?;
    let swaps = mine.join("swaps.sqlite");
    let provider = Arc::new(Provider::create(&swaps, wallet, SwapsConfig::default())?);
    let (day, lead, margin, slack) = (2, 20, 10, 6);
    let settings = Settings {
        proof_margin: margin,
        timeout_slack: slack,
        day,
        successor_lead: lead,
        max_price: Some(30_000),
        scan: true,
        ..Settings::new(regtest, 0, 0, "open-contract {hub}".into())
    };
    let chain = Node(source.clone());
    let mut keeper = Keeper::new(provider.clone(), Node(source), mine.clone(), settings);
    let limited = |most: u64| Some(HashMap::from([(url.clone(), most)]));
    let alerted = |says: &str| -> Result<bool> {
        let events = provider.events(0, 1_000)?;
        Ok((events.iter()).any(|e| e.kind == "alert" && e.data.to_string().contains(says)))
    };
    // Nothing taken in until it knows what protects the notes.
    ensure!(provider.intake() == Some(HashMap::new()));

    // No contract at the hub: nothing taken in there, the operator told
    // how to open one.
    let plan = keeper.round().await?;
    ensure!(
        plan.alerts == [Alert::NoContract { hub: url.clone() }],
        "{plan:?}"
    );
    ensure!(provider.intake() == limited(0));
    ensure!(alerted(&format!("open-contract {url}"))?);

    // Opened by the operator: nothing yet while it opens, then its cap.
    let first = holder::open(provider.wallet(), &chain, &mine, &url, ask()).await?;
    let plan = keeper.round().await?;
    ensure!(
        plan.alerts.is_empty() && plan.successors.is_empty(),
        "{plan:?}"
    );
    ensure!(provider.intake() == limited(0));
    rt.mine(1)?;
    keeper.round().await?;
    keeper.round().await?;
    ensure!(
        provider.intake() == limited(4_096),
        "{:?}",
        provider.intake()
    );

    let tip = rt.height()?;
    let safety = safety::of(provider.wallet(), &mine, &first.id, tip, margin, slack)?;
    let Deadlines { renew_at, claim_at } = Deadlines::new(safety.request_by, day, lead);
    ensure!(renew_at > tip + 1, "{safety:?}");
    rt.mine_to_height(renew_at - 1)?;
    let plan = keeper.round().await?;
    ensure!(plan.successors.is_empty(), "{plan:?}");
    ensure!(provider.intake() == limited(4_096));

    // Due to be replaced: a successor of its cap opened, counted in its
    // stead while it opens.
    rt.mine(1)?;
    let plan = keeper.round().await?;
    ensure!(
        plan.successors.len() == 1 && plan.successors[0].replaces == first.id,
        "{plan:?}"
    );
    let all = holder::list(&mine)?;
    let second = all
        .iter()
        .find(|c| c.id != first.id)
        .context("no successor")?;
    ensure!(
        second.since >= renew_at && second.cap == 4_096 && second.price <= 30_000,
        "{second:?}"
    );
    let plan = keeper.round().await?;
    ensure!(plan.successors.is_empty(), "{plan:?}");
    ensure!(provider.intake() == limited(4_096));
    // Live: it counts, and nothing more is opened.
    rt.mine(1)?;
    keeper.round().await?;
    let plan = keeper.round().await?;
    ensure!(
        plan.successors.is_empty() && plan.force_close.is_empty(),
        "{plan:?}"
    );
    ensure!(provider.intake() == limited(4_096));
    ensure!(holder::list(&mine)?.len() == 2);

    // At its safe deadline, its successor recovers every note there: not
    // claimed.
    let payment = pay(
        &hub,
        &at.join("seed"),
        &url,
        &provider.wallet().address(),
        3_000,
    )?;
    provider.wallet().receive(&payment)?;
    rt.mine_to_height(claim_at)?;
    let plan = keeper.round().await?;
    ensure!(
        plan.force_close.is_empty() && plan.drains.is_empty(),
        "{plan:?}"
    );
    ensure!(recovery::recoveries(&mine)?.is_empty());
    // More than it recovers: claimed, whatever the hub says, and what the
    // successor cannot protect to be drained.
    let payment = pay(
        &hub,
        &at.join("seed"),
        &url,
        &provider.wallet().address(),
        2_000,
    )?;
    provider.wallet().receive(&payment)?;
    let plan = keeper.round().await?;
    ensure!(plan.force_close == [first.id], "{plan:?}");
    ensure!(plan.drains == [(url.clone(), 5_000 - 4_096)], "{plan:?}");
    let recoveries = recovery::recoveries(&mine)?;
    ensure!(
        recoveries.len() == 1 && recoveries[0].id == first.id,
        "{recoveries:?}"
    );
    ensure!(alerted("forced contract")?);
    // Never forced again, nor replaced.
    let plan = keeper.round().await?;
    ensure!(
        plan.force_close.is_empty() && plan.successors.is_empty(),
        "{plan:?}"
    );
    Ok(())
}
