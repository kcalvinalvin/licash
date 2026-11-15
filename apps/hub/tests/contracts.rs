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
use licash_contracts::holder::{self, Ask, Contract, Fate, Node, Phase};
use licash_contracts::hub::terms::Range;
use licash_contracts::hub::Openings;
use licash_hub::{Hub, Policy};
use licash_hub_service::Services;
use licash_transport::Http;
use licash_wallet::{Transport as _, Wallet};
use lngap_btc::regtest::Regtest;

/// A hub service opening contracts, funded by `rt`'s wallet: its URL.
async fn hub(rt: &Regtest, dir: &Path) -> Result<(String, Source)> {
    hub_with(rt, dir, None).await
}

/// As [`hub`], holders choosing their terms within `range`, if any.
async fn hub_with(rt: &Regtest, dir: &Path, range: Option<Range>) -> Result<(String, Source)> {
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
    let app = licash_hub_service::router(hub, services);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((url, source))
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
