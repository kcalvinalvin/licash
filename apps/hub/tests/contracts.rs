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
use licash_contracts::hub::Openings;
use licash_hub::{Hub, Policy};
use licash_hub_service::Services;
use licash_transport::Http;
use licash_wallet::Wallet;
use lngap_btc::regtest::Regtest;

/// A hub service opening contracts, funded by `rt`'s wallet: its URL.
async fn hub(rt: &Regtest, dir: &Path) -> Result<(String, Source)> {
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
    let openings = Openings::new(dir, source.clone(), keyset).await?;
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
