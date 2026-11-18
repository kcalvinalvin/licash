//! Contracts opened between a wallet and the hub service over HTTP, on a
//! regtest node whose wallet funds F: under the mock authority, so they
//! protect nothing. Needs `bitcoind` (`LICHEN_BITCOIND`, as
//! `regtest/run.sh` sets it) and a release build, as the openings build
//! and check their garbled instances: `regtest/run.sh` from the
//! repository root.

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
use lichen_btc::regtest::Regtest;

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
async fn scan<T: licash_wallet::Transport>(wallet: &Wallet<T>, chain: &Node) -> Result<u64> {
    wallet.scan_silent(None, chain, 10_000).await?;
    Ok(wallet.silent_coins()?.iter().map(|c| c.value).sum())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hub_learns_what_its_contracts_wallet_holds() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let (seed, db) = (dir.path().join("seed"), dir.path().join("hub.sqlite"));
    Hub::init(&seed, &db, 12)?;
    let hub = Hub::open(&seed, &db, Policy::default())?;
    let url = format!(
        "{}/wallet/harness?cookie={}",
        rt.rpc_url(),
        rt.cookie().display()
    );
    let source: Source = url.parse()?;
    let keyset = hub.info().keysets[0].clone();
    let range = Range {
        max_cap: licash_contracts::hub::terms::MAX_CAP,
        max_blocks: 5_000,
        per_coin: 2,
        ppm_year: 10_000,
    };
    let openings =
        Openings::with_wallet(dir.path(), source.clone(), source, keyset, Some(range)).await?;
    // Its confirmed balance, as the node's wallet says it now.
    let balance = openings.learn_funds().await?;
    ensure!(balance > 0, "{balance}");
    let fundable = licash_contracts::hub::terms::fundable(balance).context("none funded")?;
    let advertised = openings.advertise().range.context("no range")?.max_cap;
    ensure!(
        advertised == fundable,
        "{advertised} advertised, {fundable} funded"
    );
    Ok(())
}

/// [`Http`], but its second instance upload heard of no answer, once, as
/// one timed out over Tor after the hub had it: and the uploads it sent.
#[derive(Default)]
struct Dropping {
    http: Option<Http>,
    sent: std::sync::Mutex<Vec<[u8; 32]>>,
}

impl licash_wallet::Transport for Dropping {
    async fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> Result<licash_wallet::Response, String> {
        use bitcoin::hashes::Hash as _;
        let http = self.http.as_ref().expect("an Http");
        let instance = url.ends_with("/instances");
        let answer = http.request(url, body.clone()).await;
        if let (true, Some(body)) = (instance, &body) {
            let mut sent = self.sent.lock().unwrap();
            let hash = bitcoin::hashes::sha256::Hash::hash(body);
            sent.push(hash.to_byte_array());
            if sent.len() == 2 {
                return Err("no answer over Tor in 120 s".into());
            }
        }
        answer
    }
}

/// An opening whose instance upload timed out resumes sending only those
/// the hub did not answer as had: each of 25 MB on signet, over Tor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn an_opening_resumed_sends_no_instance_the_hub_answered_again() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let (url, source) = hub(&rt, &dir.path().join("hub")).await?;
    let chain = Node(source);
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let transport = Dropping {
        http: Some(Http::new(None)?),
        ..Default::default()
    };
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(&mine.join("wallet.sqlite"), &[31; 32], regtest, transport)?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    let to = wallet.fallback_address()?.script_pubkey();
    rt.fund(&to, Amount::from_sat(60_000))?;
    rt.mine(1)?;
    ensure!(scan(&wallet, &chain).await? == 60_000);

    // The second of its two instances unanswered: the opening waits.
    let failed = holder::open(&wallet, &chain, &mine, &url, ask()).await;
    ensure!(failed.is_err(), "{failed:?}");
    let listed = holder::list(&mine)?;
    ensure!(
        listed.len() == 1 && listed[0].phase == Phase::Chosen,
        "{listed:?}"
    );

    // Resumed: the first not sent again, the second again, and funded.
    let contracts = holder::resume(&wallet, &chain, &mine).await?;
    ensure!(contracts[0].phase == Phase::Funded, "{contracts:?}");
    let sent = wallet.transport().sent.lock().unwrap().clone();
    ensure!(sent.len() == 3, "{} instance uploads", sent.len());
    ensure!(
        sent[0] != sent[1] && sent[1] == sent[2],
        "the first sent again"
    );
    Ok(())
}

/// An opening whose instance upload dropped, opened as the apps open
/// one: resumed at once, the instance sent again, the one answered not.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn an_opening_carries_on_where_an_instance_upload_dropped() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let (url, source) = hub(&rt, &dir.path().join("hub")).await?;
    let chain = Node(source);
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let transport = Dropping {
        http: Some(Http::new(None)?),
        ..Default::default()
    };
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(&mine.join("wallet.sqlite"), &[32; 32], regtest, transport)?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    let to = wallet.fallback_address()?.script_pubkey();
    rt.fund(&to, Amount::from_sat(60_000))?;
    rt.mine(1)?;
    ensure!(scan(&wallet, &chain).await? == 60_000);

    let opened = holder::open_with(&wallet, &chain, &mine, &url, ask(), Change::Bitcoin).await?;
    ensure!(opened.phase == Phase::Funded, "{opened:?}");
    let sent = wallet.transport().sent.lock().unwrap().clone();
    ensure!(sent.len() == 3, "{} instance uploads", sent.len());
    ensure!(
        sent[0] != sent[1] && sent[1] == sent[2],
        "the first sent again"
    );
    Ok(())
}

/// A quote the hub no longer makes once she sends her Open, its fee
/// surcharged meanwhile as its contracts filled up: refused as such, her
/// opening is given up and made again on a quote asked anew, at the price
/// now, within her ceiling.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_wallet_quoted_before_the_hub_surcharged_opens_on_a_quote_asked_again() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    // Armed, the hub's most contracts falls to 2 once it answered a quote.
    let armed = Arc::new(AtomicBool::new(false));
    let arm = armed.clone();
    let serve = move |app: axum::Router, openings: Arc<Openings>| {
        app.layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let (armed, openings) = (arm.clone(), openings.clone());
                async move {
                    let quote = request.uri().path() == licash_contracts::QUOTE;
                    let response = next.run(request).await;
                    if quote && armed.swap(false, Ordering::SeqCst) {
                        openings.limit_live(2);
                    }
                    response
                }
            },
        ))
    };
    let (url, source, _hub, _) =
        hub_answering_through(&rt, &dir.path().join("hub"), None, serve).await?;
    let chain = Node(source);
    let mine = dir.path().join("wallet");
    std::fs::create_dir(&mine)?;
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &[7; 32],
        regtest,
        Http::new(None)?,
    )?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [100_000, 60_000] {
        let to = wallet.fallback_address()?.script_pubkey();
        rt.fund(&to, Amount::from_sat(sats))?;
    }
    rt.mine(1)?;
    ensure!(scan(&wallet, &chain).await? == 160_000);
    let ask = Ask {
        max_price: 60_000,
        ..ask()
    };
    let first = holder::open(&wallet, &chain, &mine, &url, ask).await?;
    ensure!(
        first.phase == Phase::Funded && first.price == 23_000,
        "{first:?}"
    );

    // One of 2 live once the hub quoted her: its fee doubled since.
    armed.store(true, Ordering::SeqCst);
    let second = holder::open(&wallet, &chain, &mine, &url, ask).await?;
    ensure!(second.phase == Phase::Funded, "{second:?}");
    ensure!(
        second.price > first.price,
        "{} not surcharged",
        second.price
    );
    ensure!(!armed.load(Ordering::SeqCst), "not quoted");
    let contracts = holder::list(&mine)?;
    let given_up = contracts.iter().filter(|c| c.abandoned.is_some()).count();
    ensure!(contracts.len() == 3 && given_up == 1, "{contracts:?}");
    let abandoned = contracts.iter().find(|c| c.abandoned.is_some()).unwrap();
    let why = abandoned.abandoned.as_deref().unwrap_or_default();
    ensure!(why.contains("not the hub's quote"), "{why}");
    Ok(())
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
    ensure!((first.close_height, first.assert_height) == (tip + 300, tip + 140));
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

/// A hub following Bitcoin opens contracts on the newest block its ledger
/// takes as a lock checkpoint, kept as its newest: every lock named after
/// a contract opens is on the chain its statement reads.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hub_opens_contracts_on_its_newest_lock_checkpoint() -> Result<()> {
    let newest = |hub: &Hub| hub.checkpoint().map(|c| c.commitment());
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let (url, source, hub, openings) = hub_answering(&rt, &dir.path().join("hub")).await?;
    licash_hub_service::open_on_lock_checkpoints(&openings, hub.clone());
    let chain = Node(source.clone());
    // Following no chain yet: no offer.
    ensure!(openings.offer(None).await.is_err());
    let seed = [7; 32];
    let (mine, wallet) = holder_wallet(&rt, &chain, dir.path(), &url, seed, &[100_000]).await?;
    while !watch::step(&hub, &source).await? {}
    let first = hub.checkpoint().context("no checkpoint")?;
    // Later blocks, too few for the hub to take another checkpoint.
    rt.mine(20)?;
    while !watch::step(&hub, &source).await? {}
    ensure!(newest(&hub) == Some(first.commitment()));
    let tip = rt.height()?;
    let offer = openings.offer(None).await;
    let offer = offer.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let at = offer.quote.chain.checkpoint.clone();
    let bury = Policy::default().timing.bury;
    ensure!(at.height() == tip + 1 - bury && at.height() > first.height());
    // A quote keeps nothing: an opening does.
    ensure!(newest(&hub) == Some(first.commitment()), "kept on a quote");
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    ensure!(opened.phase == Phase::Funded, "{opened:?}");
    ensure!(newest(&hub) == Some(at.commitment()));
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
    ensure!((first.close_height, first.assert_height) == (tip + 2_000, tip + 1_840));

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
    // No coin of hers left on Bitcoin: her purse pays her forced close.
    empty(&rt, &wallet, &chain).await?;

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
    // Asked again, as a second tap waiting behind the first: done already.
    recovery::give_up(&wallet, &mine, &opened.id)?;
    ensure!(available(&wallet)? == 3_000);
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
    // P's fees paid of R's alone: its claim slot, its anchor and her
    // purse, no coin of hers.
    let p = requested.publication.context("no P")?;
    let r = requested.request.context("no R")?;
    let (r, p) = (rt.get_tx(&r)?, rt.get_tx(&p)?);
    let spent: Vec<_> = p.input.iter().map(|i| i.previous_output).collect();
    let of_r = |vout| bitcoin::OutPoint {
        txid: r.compute_txid(),
        vout,
    };
    ensure!(spent == [of_r(2), of_r(1), of_r(3)], "{p:?}");
    eprintln!(
        "R {} vB, P {} vB, purse {} sat",
        r.vsize(),
        p.vsize(),
        r.output[3].value
    );

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
    // Her claim's bits (3 000 rounded down to 2 560) and all her 20 000,
    // her purse (P's, taken already) refunded, less the sweep's fee (its
    // bits' witnesses are large): the hub, only silent, gets its 100 000
    // back less what the links took, her purse among it (its graph id 8's
    // `request_timeout`).
    ensure!(
        paid > 2_560 + 20_000 - 3_000 && paid < 2_560 + 20_000,
        "{paid} swept"
    );
    let timeout = timed_out.timeout.context("no timeout")?;
    let found: serde_json::Value = chain
        .0
        .rpc(
            "getrawtransaction",
            serde_json::json!([timeout.to_string(), true]),
        )
        .await?;
    let outputs = found["vout"].as_array().context("no outputs")?;
    // After the bits and her reserve, last.
    let last = outputs.last().and_then(|o| o["value"].as_f64());
    let back = (last.context("no output")? * 1e8).round() as u64;
    ensure!((70_000..80_000).contains(&back), "the hub's {back} back");
    eprintln!("swept {paid} sat to her wallet");
    // Nothing more to do.
    let over = recovery::advance(&wallet, &chain, &mine, rate).await?;
    ensure!(over[0] == *done, "{over:?}");
    Ok(())
}

/// Her wallet's coins on Bitcoin, all spent away, mined.
async fn empty(rt: &Regtest, wallet: &Wallet<Http>, chain: &Node) -> Result<()> {
    if scan(wallet, chain).await? > 0 {
        let away =
            bitcoin::ScriptBuf::new_p2wsh(&bitcoin::ScriptBuf::from(vec![0x51]).wscript_hash());
        let rate = bitcoin::FeeRate::from_sat_per_vb(2).context("a rate")?;
        let tx = wallet.spend_silent(away, None, rate)?;
        rt.send_and_confirm(&tx)?;
    }
    ensure!(scan(wallet, chain).await? == 0, "coins left");
    Ok(())
}

/// A forced close at a fee rate her purse falls short of, no coin of hers
/// on Bitcoin: put off, said why, revocable still. Then paid of one coin
/// of hers more, once she has one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_holder_adds_one_coin_to_her_purse_where_fees_run_past_it() -> Result<()> {
    use bitcoin::FeeRate;
    use licash_contracts::holder::recovery::{self, Stage};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
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
    let to = wallet.fallback_address()?.script_pubkey();
    rt.fund(&to, Amount::from_sat(60_000))?;
    rt.mine(1)?;
    ensure!(scan(&wallet, &chain).await? == 60_000);
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 3_000)?;
    wallet.receive(&payment)?;
    rt.mine(2)?;
    holder::resume(&wallet, &chain, &mine).await?;
    empty(&rt, &wallet, &chain).await?;

    // 20 sat/vB: R's package alone is past her purse.
    let rate = FeeRate::from_sat_per_vb(20).context("a rate")?;
    let forced = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    let lock_end = forced.next_at.context("no lock")?;
    rt.mine_to_height(lock_end - 1)?;
    let short = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(
        short.stage == Stage::Claimed && !short.irrevocable,
        "{short:?}"
    );
    let why = short.error.clone().context("no error")?;
    ensure!(why.contains("too little of hers on Bitcoin"), "{why}");
    // How much more, in sats: some, and less than the coin she gets next.
    let needed: u64 = why
        .split("about ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .context("no sats needed said")?;
    ensure!(needed > 0 && needed < 200_000, "{why}");

    // A coin of hers: R's slot, anchor and purse, and that coin alone.
    let (coin, _) = rt.fund(
        &wallet.fallback_address()?.script_pubkey(),
        Amount::from_sat(200_000),
    )?;
    rt.mine(1)?;
    ensure!(scan(&wallet, &chain).await? == 200_000);
    let sent = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(
        sent.stage == Stage::Published && sent.irrevocable && sent.error.is_none(),
        "{sent:?}"
    );
    let p = rt.get_tx(&sent.publication.context("no P")?)?;
    let r = rt.get_tx(&sent.request.context("no R")?)?;
    let spent: Vec<_> = p.input.iter().map(|i| i.previous_output).collect();
    let of_r = |vout| bitcoin::OutPoint {
        txid: r.compute_txid(),
        vout,
    };
    ensure!(spent == [of_r(2), of_r(1), of_r(3), coin], "{p:?}");
    rt.mine(1)?;
    let requested = &recovery::advance(&wallet, &chain, &mine, rate).await?[0];
    ensure!(requested.stage == Stage::Requested, "{requested:?}");
    Ok(())
}

/// An honest forced close at a hub that plays exits: her claim is owed all
/// it asks, so the hub posts nothing and her timeout pays her.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hub_lets_an_honest_claim_time_out_to_its_holder() -> Result<()> {
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
        &[21; 32],
        ChainProfile::Core30DefaultRegtest,
        Http::new(None)?,
    )?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    for sats in [60_000, 50_000] {
        let to = wallet.fallback_address()?.script_pubkey();
        rt.fund(&to, Amount::from_sat(sats))?;
    }
    rt.mine(1)?;
    scan(&wallet, &chain).await?;
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    let rate = FeeRate::from_sat_per_vb(2).context("a rate")?;
    let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 3_000)?;
    wallet.receive(&payment)?;
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;
    recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;

    let mut done = None;
    for _ in 0..400 {
        rt.mine(1)?;
        let round = openings.answer_requests().await?;
        ensure!(round.answered == 0 && round.failed == 0, "{round:?}");
        let all = recovery::advance(&wallet, &chain, &mine, rate).await?;
        ensure!(all[0].stage != Stage::Answered, "{all:?}");
        if all[0].stage == Stage::Done {
            done = Some(all[0].clone());
            break;
        }
    }
    let done = done.context("never paid")?;
    ensure!(done.received.is_some_and(|r| r > 3_000), "{done:?}");
    // The notes she was paid for on Bitcoin are spent at the hub: no
    // spend takes them.
    let claimed: Vec<licash::crypto::Point> =
        rusqlite::Connection::open(mine.join("wallet.sqlite"))?
            .prepare("SELECT y FROM notes WHERE spent_by IS NOT NULL")?
            .query_map([], |r| r.get::<_, [u8; 33]>(0))?
            .map(|y| Ok(licash::crypto::Point(y?)))
            .collect::<Result<_>>()?;
    ensure!(!claimed.is_empty(), "no claimed note");
    ensure!(
        hub.spent(&claimed)?.iter().all(|s| *s),
        "a claimed note unspent"
    );
    Ok(())
}

/// A contract no one exits ends at its close: the hub posts the default,
/// which pays her reserve to her wallet and the rest to the hub's, once.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hub_ends_a_contract_no_one_exited_by_default_at_its_close() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let (url, source, _, openings) = hub_answering(&rt, &dir.path().join("hub")).await?;
    let chain = Node(source);
    let (mine, wallet) = holder_wallet(&rt, &chain, dir.path(), &url, [23; 32], &[60_000]).await?;
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;
    // F's change came back: 60 000 less the price.
    ensure!(scan(&wallet, &chain).await? == 37_000);

    // Not before its close: the default's lock is the close.
    let close = opened.close_height;
    rt.mine(u64::from(close) - u64::from(rt.height()?) - 1)?;
    let round = openings.answer_requests().await?;
    ensure!(round == Default::default(), "{round:?}");
    rt.mine(1)?;
    let round = openings.answer_requests().await?;
    ensure!(round.closed == 1 && round.failed == 0, "{round:?}");
    let again = openings.answer_requests().await?;
    ensure!(again == Default::default(), "posted again: {again:?}");
    rt.mine(1)?;

    // Her reserve at her wallet; the rest, V_max and the hub's reserve, at
    // the hub's.
    ensure!(scan(&wallet, &chain).await? == 37_000 + 20_000);
    let f = opened.funding.context("no F")?;
    let spent: Option<serde_json::Value> = chain
        .0
        .rpc_optional("gettxout", serde_json::json!([f.to_string(), 0, true]))
        .await?;
    ensure!(spent.is_none(), "the contract stands");
    let funding: serde_json::Value = chain
        .0
        .rpc(
            "getrawtransaction",
            serde_json::json!([f.to_string(), true]),
        )
        .await?;
    let value = funding["vout"][0]["value"]
        .as_f64()
        .context("no contract output")?;
    let share = (value * 1e8).round() as u64 - 20_000;
    let ours: serde_json::Value = chain
        .0
        .rpc("listunspent", serde_json::json!([1, 1]))
        .await?;
    let back = ours
        .as_array()
        .context("no coins")?
        .iter()
        .filter_map(|c| c["amount"].as_f64())
        .any(|btc| (btc * 1e8).round() as u64 == share);
    ensure!(back, "the hub's share not in its wallet: {ours}");
    let after = openings.answer_requests().await?;
    ensure!(after == Default::default(), "{after:?}");
    Ok(())
}

/// A holder closes her contract early with the hub, cooperatively: its
/// fold pays her reserve to her wallet at once, the rest to the hub's less
/// the fee, and the contract protects nothing from then on. The hub's
/// watcher takes the fold as the contract's end, and posts no default.
/// Closed early as soon as its F is buried, her wallet not told so
/// before: the close looks at the chain itself.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_holder_closes_a_contract_buried_since_she_last_looked() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let (url, source, _, _openings) = hub_answering(&rt, &dir.path().join("hub")).await?;
    let chain = Node(source);
    let (mine, wallet) = holder_wallet(&rt, &chain, dir.path(), &url, [33; 32], &[60_000]).await?;
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    ensure!(holder::list(&mine)?[0].fate != Some(Fate::Live));
    let early = holder::close(&wallet, &chain, &mine, &opened.id).await;
    ensure!(
        early
            .as_ref()
            .is_err_and(|e| format!("{e:#}").contains("not buried")),
        "{early:?}"
    );
    rt.mine(1)?;
    let txid = holder::close(&wallet, &chain, &mine, &opened.id).await?;
    let listed = holder::list(&mine)?;
    ensure!(listed[0].folded == Some(txid), "{:?}", listed[0]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_holder_closes_her_contract_early_with_the_hub_and_gets_her_reserve_back() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let (url, source, _, openings) = hub_answering(&rt, &dir.path().join("hub")).await?;
    let chain = Node(source);
    let (mine, wallet) = holder_wallet(&rt, &chain, dir.path(), &url, [29; 32], &[60_000]).await?;
    let opened = holder::open(&wallet, &chain, &mine, &url, ask()).await?;
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;
    ensure!(scan(&wallet, &chain).await? == 37_000);
    let tip = rt.height()?;
    let listed = holder::list(&mine)?;
    ensure!(listed[0].live(tip), "not live: {:?}", listed[0]);

    let txid = holder::close(&wallet, &chain, &mine, &opened.id).await?;
    let listed = holder::list(&mine)?;
    ensure!(listed[0].folded == Some(txid), "{:?}", listed[0]);
    ensure!(!listed[0].live(tip) && listed[0].over(tip));
    // Asked again, its fold in the mempool: the hub refuses before she
    // signs another, and it stays closed.
    let again = holder::close(&wallet, &chain, &mine, &opened.id).await;
    ensure!(
        again
            .as_ref()
            .is_err_and(|e| format!("{e:#}").contains("spent")),
        "{again:?}"
    );
    ensure!(holder::list(&mine)?[0].folded == Some(txid));
    rt.mine(1)?;

    // Her reserve at her wallet; the hub's share less the fee at its own.
    ensure!(scan(&wallet, &chain).await? == 37_000 + 20_000);
    let fold: serde_json::Value = chain
        .0
        .rpc(
            "getrawtransaction",
            serde_json::json!([txid.to_string(), true]),
        )
        .await?;
    let f = opened.funding.context("no F")?;
    ensure!(fold["vin"][0]["txid"] == f.to_string(), "{fold}");
    ensure!(fold["confirmations"].as_u64() == Some(1), "{fold}");
    let sats = |v: &serde_json::Value| (v["value"].as_f64().unwrap_or(0.0) * 1e8).round() as u64;
    let outs = fold["vout"].as_array().context("no outputs")?;
    ensure!(outs.len() == 2 && sats(&outs[0]) == 20_000, "{fold}");
    let funding: serde_json::Value = chain
        .0
        .rpc(
            "getrawtransaction",
            serde_json::json!([f.to_string(), true]),
        )
        .await?;
    let value = sats(&funding["vout"][0]);
    let fee = value - 20_000 - sats(&outs[1]);
    ensure!((1..2_000).contains(&fee), "a fee of {fee}: {fold}");

    // The hub posts no default, now or at the close, and ends it.
    let round = openings.answer_requests().await?;
    ensure!(round == Default::default(), "{round:?}");
    let close = opened.close_height;
    rt.mine(u64::from(close) - u64::from(rt.height()?) + 1)?;
    let round = openings.answer_requests().await?;
    ensure!(round == Default::default(), "{round:?}");
    ensure!(scan(&wallet, &chain).await? == 37_000 + 20_000);
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
    hub_answering_through(rt, dir, range, |app, _| app).await
}

/// As [`hub_answering_with`], its service made by `serve` of the hub
/// service's router and its contracts.
async fn hub_answering_through(
    rt: &Regtest,
    dir: &Path,
    range: Option<Range>,
    serve: impl FnOnce(axum::Router, Arc<Openings>) -> axum::Router,
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
    // Its ledger against holders' claims, as the hub service's.
    licash_hub_service::answer_from(&openings, hub.clone());
    let services = Services {
        contracts: Some(openings.clone()),
        ..Default::default()
    };
    let app = serve(
        licash_hub_service::router(hub.clone(), services),
        openings.clone(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((url, source, hub, openings))
}

/// [`Node`], but stalling at its first broadcast, kept: as a holder whose
/// app stops once the hub signed F, before it hands F to the network or
/// tells the hub, which would broadcast it too.
struct Stalling {
    node: Node,
    f: std::sync::Mutex<Option<bitcoin::Transaction>>,
}

impl licash_wallet::Blocks for Stalling {
    async fn height(&self) -> Result<u32, String> {
        self.node.height().await
    }

    async fn hash(&self, height: u32) -> Result<bitcoin::BlockHash, String> {
        self.node.hash(height).await
    }

    async fn filter(&self, hash: &bitcoin::BlockHash) -> Result<Option<Vec<u8>>, String> {
        self.node.filter(hash).await
    }

    async fn block(&self, hash: &bitcoin::BlockHash) -> Result<bitcoin::Block, String> {
        self.node.block(hash).await
    }
}

impl holder::Chain for Stalling {
    async fn checkpoint(&self, height: u32) -> Result<licash::chain::Checkpoint, String> {
        self.node.checkpoint(height).await
    }

    async fn broadcast(&self, tx: &bitcoin::Transaction) -> Result<(), String> {
        self.f.lock().unwrap().get_or_insert_with(|| tx.clone());
        std::future::pending().await
    }

    async fn fee_rate(&self, blocks: u16) -> Result<Option<bitcoin::FeeRate>, String> {
        self.node.fee_rate(blocks).await
    }
}

/// An opening at `url` by `wallet` (of `mine`) stalled once the hub signed
/// F, before F is broadcast: F.
async fn stalled_opening(
    wallet: &Wallet<Http>,
    source: &Source,
    mine: &Path,
    url: &str,
) -> Result<bitcoin::Transaction> {
    let stalling = Stalling {
        node: Node(source.clone()),
        f: Default::default(),
    };
    let reached = async {
        while stalling.f.lock().unwrap().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    tokio::select! {
        opened = holder::open(wallet, &stalling, mine, url, ask()) => {
            anyhow::bail!("opened, never stalled: {opened:?}")
        }
        () = reached => {}
    }
    let f = stalling.f.lock().unwrap().clone();
    f.context("no F")
}

/// The hub frees the coins of an opening it signed whose F never confirms:
/// at once once her deposit is spent elsewhere, deep; else, a day after it
/// first saw F in no block (or once F may no longer fund its contract),
/// by spending one of its own coins F spends back to its wallet.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hub_frees_the_coins_of_an_opening_it_signed_whose_f_never_confirms() -> Result<()> {
    use bitcoin::FeeRate;
    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let hub_dir = dir.path().join("hub");
    let (url, source, _, openings) = hub_answering(&rt, &hub_dir).await?;
    let chain = Node(source.clone());
    let locked = || async {
        let locked: Vec<serde_json::Value> = chain
            .0
            .rpc("listlockunspent", serde_json::json!([]))
            .await?;
        anyhow::Ok(locked.len())
    };
    let gettxout = |coin: bitcoin::OutPoint, mempool: bool| {
        let chain = &chain;
        async move {
            let args = serde_json::json!([coin.txid.to_string(), coin.vout, mempool]);
            let out: Option<serde_json::Value> = chain.0.rpc_optional("gettxout", args).await?;
            anyhow::Ok(out.is_some_and(|out| !out.is_null()))
        }
    };

    // Three holders whose apps stop once the hub signed their F.
    let mut stalled = Vec::new();
    for (n, seed) in [[41; 32], [42; 32], [43; 32]].into_iter().enumerate() {
        let at = dir.path().join(format!("holder{n}"));
        std::fs::create_dir(&at)?;
        let (mine, wallet) = holder_wallet(&rt, &chain, &at, &url, seed, &[60_000]).await?;
        let deposit = wallet.silent_coins()?[0].outpoint;
        let f = stalled_opening(&wallet, &source, &mine, &url).await?;
        ensure!(f.input.iter().any(|i| i.previous_output == deposit));
        stalled.push((wallet, f, deposit));
    }
    ensure!(licash_contracts::hub::signed(&hub_dir)? == 3);
    openings.answer_requests().await?;
    let seen = rt.height()?;

    // One's F broadcast late, once the hub saw it in no block: its
    // contract stands, its coins never spent back.
    let late = stalled.pop().context("three")?.1;
    rt.send_raw(&late)?;
    rt.mine(1)?;
    openings.answer_requests().await?;
    let held = locked().await?;
    ensure!(held >= 2, "{held} coins locked");

    // Her deposit spent elsewhere: given up once that is deep, its coins
    // unlocked.
    let (wallet, f, deposit) = &stalled[1];
    wallet.abandon_silent(&f.compute_txid())?;
    let to = wallet.fallback_address()?.script_pubkey();
    let rate = FeeRate::from_sat_per_vb_u32(2);
    let elsewhere =
        wallet.spend_silent_coin(licash_wallet::Payee::Script(to), *deposit, None, rate)?;
    rt.send_raw(&elsewhere)?;
    rt.mine(5)?;
    openings.answer_requests().await?;
    ensure!(
        licash_contracts::hub::signed(&hub_dir)? == 3,
        "given up 5 deep"
    );
    rt.mine(1)?;
    openings.answer_requests().await?;
    ensure!(
        licash_contracts::hub::signed(&hub_dir)? == 2,
        "not given up 6 deep"
    );
    let mid = locked().await?;
    ensure!(mid < held, "{mid} of {held} coins locked");

    // The other: one of the hub's coins F spends is spent back to its
    // wallet, not at once, a day after F was first seen in no block at
    // the latest.
    let (_, f, deposit) = &stalled[0];
    let hub_coins: Vec<bitcoin::OutPoint> = (f.input.iter())
        .map(|i| i.previous_output)
        .filter(|o| o != deposit)
        .collect();
    let spent_back = || async {
        for coin in &hub_coins {
            if gettxout(*coin, false).await? && !gettxout(*coin, true).await? {
                return anyhow::Ok(true);
            }
        }
        anyhow::Ok(false)
    };
    ensure!(!spent_back().await?, "spent back at once");
    let mut at = None;
    while rt.height()? < seen + 144 {
        rt.mine(1)?;
        openings.answer_requests().await?;
        if spent_back().await? {
            at = Some(rt.height()?);
            break;
        }
    }
    let at = at.context("no coin of the hub's spent back")?;
    ensure!(at > seen + 1, "spent back at {at}, first seen at {seen}");
    ensure!(gettxout(*deposit, true).await?, "her deposit spent");
    // Deep: given up, its coins unlocked. The late one stands.
    rt.mine(6)?;
    openings.answer_requests().await?;
    ensure!(
        licash_contracts::hub::signed(&hub_dir)? == 1,
        "not given up"
    );
    let contract = bitcoin::OutPoint::new(late.compute_txid(), 0);
    ensure!(gettxout(contract, false).await?, "the late contract gone");
    let left = locked().await?;
    ensure!(left < mid, "{left} of {mid} coins locked");
    Ok(())
}

/// `from`'s files copied into `to`, made.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
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

    // A claim of her cap is owed all of it: the hub, playing her exit,
    // posts nothing, and her timeout pays her.
    let mut now = requested.clone();
    for _ in 0..400 {
        if now.stage == Stage::Done {
            break;
        }
        ensure!(now.stage != Stage::Answered, "{now:?}");
        rt.mine(1)?;
        let round = openings.answer_requests().await?;
        ensure!(round.answered == 0 && round.failed == 0, "{round:?}");
        now = recovery::advance(&wallet, &chain, &mine, rate).await?[0].clone();
    }
    ensure!(now.stage == Stage::Done && now.claimed == 5_000, "{now:?}");
    let swept = rt.get_tx(&now.sweep.context("no sweep")?)?.output[0].value;
    let swept = swept.to_sat();
    ensure!(swept > 5_000, "{swept} swept");
    eprintln!("swept {swept} sat to her wallet of a claim of 5000");
    Ok(())
}

/// A stale claim requested at a hub that answers: she restores a copy of
/// her wallet made before she paid a note away and forces her contract
/// closed on it, claiming her cap of 5 000 sat in three notes of 2 048.
struct Stale {
    rt: Regtest,
    _dir: tempfile::TempDir,
    hub: Arc<Hub>,
    openings: Arc<Openings>,
    chain: Node,
    wallet: Wallet<Http>,
    mine: std::path::PathBuf,
    rate: bitcoin::FeeRate,
}

async fn stale_claim(seed: [u8; 32]) -> Result<Stale> {
    stale_claim_scanned(seed, false).await
}

/// As [`stale_claim`], its hub's ledger scanned, if `scanned`, to before
/// her request.
async fn stale_claim_scanned(seed: [u8; 32], scanned: bool) -> Result<Stale> {
    use licash_contracts::holder::recovery;

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
    let chain = Node(source.clone());
    let (mine, wallet) =
        holder_wallet(&rt, &chain, dir.path(), &url, seed, &[60_000, 50_000]).await?;
    let chosen = Ask {
        cap: Some(5_000),
        ..ask()
    };
    let opened = holder::open(&wallet, &chain, &mine, &url, chosen).await?;
    let rate = bitcoin::FeeRate::from_sat_per_vb(2).context("a rate")?;
    for _ in 0..3 {
        let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 2_048)?;
        wallet.receive(&payment)?;
    }
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;
    ensure!(available(&wallet)? == 6_144);

    // Her wallet as it was, copied; then a note of it paid away.
    drop(wallet);
    let stale = dir.path().join("stale");
    copy_dir(&mine, &stale)?;
    let wallet = Wallet::open(&mine.join("wallet.sqlite"), Http::new(None)?)?;
    let bob = licash::address::OwnerKeys::from_seed(&[0xbb; 32])?.address();
    wallet.send(&url, &bob, 2_048).await?;
    let left = available(&wallet)?;
    ensure!(left < 6_144, "{left} left");
    drop(wallet);

    // Restored, the copy claims the note paid away too.
    let wallet = Wallet::open(&stale.join("wallet.sqlite"), Http::new(None)?)?;
    let mine = stale;
    if scanned {
        while !watch::step(&hub, &source).await? {}
    }
    let forced = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    ensure!((forced.claimed, forced.notes) == (5_000, 3), "{forced:?}");
    rt.mine_to_height(forced.next_at.context("no lock")? - 1)?;
    recovery::advance(&wallet, &chain, &mine, rate).await?;
    rt.mine(1)?;
    Ok(Stale {
        rt,
        _dir: dir,
        hub,
        openings,
        chain,
        wallet,
        mine,
        rate,
    })
}

/// The hub's answers to a stale claim and hers, a block at a time while
/// `hub` says the hub plays, until her recovery is done: it.
async fn played(
    s: &Stale,
    mut hub: impl FnMut(&licash_contracts::holder::recovery::Recovery) -> bool,
) -> Result<licash_contracts::holder::recovery::Recovery> {
    use licash_contracts::holder::recovery::{self, Stage};

    let mut last = None;
    for _ in 0..800 {
        s.rt.mine(1)?;
        if last.as_ref().is_none_or(&mut hub) {
            let round = s.openings.answer_requests().await?;
            ensure!(round.failed == 0, "{round:?}");
        }
        let r = recovery::advance(&s.wallet, &s.chain, &s.mine, s.rate).await?[0].clone();
        ensure!(r.stage != Stage::Lost, "{r:?}");
        if r.stage == Stage::Done {
            return Ok(r);
        }
        last = Some(r);
    }
    anyhow::bail!("never paid: {last:?}")
}

/// The hub meets the stale claim with the spend it released, discloses
/// that spend's unlock, responds what is left, proves it and asserts; her
/// payout is that.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hub_meets_a_stale_claim_with_the_spend_it_released_and_pays_what_is_left() -> Result<()>
{
    use licash_contracts::holder::recovery::Stage;

    let s = stale_claim([11; 32]).await?;
    let done = played(&s, |r| {
        assert!(
            !matches!(r.stage, Stage::TimingOut | Stage::TimedOut),
            "{r:?}"
        );
        true
    })
    .await?;
    let x = done.response.context("no response")?;
    ensure!(s.rt.confirmations(&x)?.is_some(), "{done:?}");
    // Its disclose slot spent: the unlock published.
    ensure!(!s.rt.is_unspent(&bitcoin::OutPoint { txid: x, vout: 2 })?);
    // Her notes less the one paid away: two of 2 048.
    let corrected = done.corrected.context("no response")?;
    ensure!(corrected == 4_096, "{done:?}");
    ensure!(done.payout.is_some() && done.timeout.is_none(), "{done:?}");
    let received = done.received.context("nothing received")?;
    eprintln!("a stale claim of 5000 paid {corrected}, {received} swept to her wallet");
    Ok(())
}

/// A hub whose ledger follows the chain, its scan stopped before her
/// request, still meets a claim of no lock with the spend it holds: only
/// a claim of a lock waits for the scan of its block.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hub_meets_a_claim_of_no_lock_while_its_ledgers_scan_lags() -> Result<()> {
    let s = stale_claim_scanned([13; 32], true).await?;
    let done = played(&s, |_| true).await?;
    ensure!(done.corrected == Some(4_096), "{done:?}");
    let scanned = s.hub.scanned()?.context("no scan")?.0;
    // Before her request, so before her publication.
    let requested = done.requested_at.context("no request")?;
    ensure!(
        scanned < requested,
        "scanned to {scanned}, requested at {requested}"
    );
    Ok(())
}

/// The hub responds to the stale claim and stops: its assertion window
/// passes, and her response timeout pays her all she claimed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hub_that_responds_and_never_asserts_pays_her_whole_claim() -> Result<()> {
    let s = stale_claim([12; 32]).await?;
    // The hub plays until she sees its response, then no more.
    let done = played(&s, |r| r.response.is_none()).await?;
    ensure!(done.assertion.is_none(), "{done:?}");
    ensure!(done.corrected == Some(4_096), "{done:?}");
    ensure!(done.timeout.is_some(), "not her timeout: {done:?}");
    let received = done.received.context("nothing received")?;
    ensure!(received > 5_000, "{received} of a claim of 5000: {done:?}");
    Ok(())
}

/// Transaction `txid` of a dispute, confirmed on `rt`'s standard node at
/// the fee its graph presigned: at least 1 sat/vB of its actual size,
/// what a node relays by default, with nothing to bump it.
fn paid_its_way(rt: &Regtest, txid: &bitcoin::Txid) -> Result<()> {
    let tx = rt.get_tx(txid)?;
    let mut spent = 0;
    for input in &tx.input {
        let prev = rt.get_tx(&input.previous_output.txid)?;
        spent += prev.output[input.previous_output.vout as usize]
            .value
            .to_sat();
    }
    let made: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
    let (fee, vsize) = (spent - made, tx.vsize() as u64);
    eprintln!(
        "{txid}: {vsize} vB, {fee} sat, {:.3} sat/vB",
        fee as f64 / vsize as f64
    );
    ensure!(fee >= vsize, "{txid} pays {fee} sat for {vsize} vB");
    ensure!(rt.confirmations(txid)?.is_some(), "{txid} unconfirmed");
    Ok(())
}

/// The fee of `tx`, of transactions the node has.
fn fee_of(rt: &Regtest, tx: &bitcoin::Transaction) -> Result<u64> {
    let mut spent = 0;
    for input in &tx.input {
        let prev = rt.get_tx(&input.previous_output.txid)?;
        spent += prev.output[input.previous_output.vout as usize]
            .value
            .to_sat();
    }
    Ok(spent - tx.output.iter().map(|o| o.value.to_sat()).sum::<u64>())
}

/// That link `txid` confirmed with its poster's fee child, spending its
/// anchor (output 1) in its block, the pair paying `rate` sat/vB at least:
/// as the hub posts a link at 2 sat/vB without an estimate, as on regtest.
fn paid_with_child(rt: &Regtest, txid: &bitcoin::Txid, rate: u64) -> Result<()> {
    let link = rt.get_tx(txid)?;
    let at = rt.confirmations(txid)?.context("unconfirmed")?;
    let anchor = bitcoin::OutPoint {
        txid: *txid,
        vout: 1,
    };
    let child = rt
        .block_txs(at)?
        .into_iter()
        .find(|tx| tx.input.iter().any(|i| i.previous_output == anchor))
        .with_context(|| format!("no fee child of {txid} in block {at}"))?;
    let fee = fee_of(rt, &link)? + fee_of(rt, &child)?;
    let vsize = (link.vsize() + child.vsize()) as u64;
    eprintln!("{txid} and its child: {vsize} vB, {fee} sat");
    ensure!(
        fee >= rate * vsize,
        "{txid} and its child pay {fee} sat for {vsize} vB"
    );
    Ok(())
}

/// The stale claim's request and publication buried, the hub not playing
/// and her timeout not posted: the block her response timeout is valid
/// from, then mined past, her wallet offline.
async fn past_the_response_window(s: &Stale) -> Result<u32> {
    use licash_contracts::holder::recovery::{self, Stage};

    let mut last = None;
    for _ in 0..50 {
        let r = recovery::advance(&s.wallet, &s.chain, &s.mine, s.rate).await?[0].clone();
        if r.stage == Stage::Requested && r.publication.is_some() {
            let window_end = r.next_at.context("no response window")?;
            s.rt.mine_to_height(window_end + 1)?;
            return Ok(window_end);
        }
        last = Some(r);
        s.rt.mine(1)?;
    }
    anyhow::bail!("never requested: {last:?}")
}

/// The response window makes her timeout valid, but does not expire the
/// hub's answer: one confirming after it, before her timeout, still gets
/// a fresh assertion window, and her a fresh challenge window after the
/// assertion. Here the hub proves and asserts, and pays what is left.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hubs_answer_can_confirm_after_its_response_window() -> Result<()> {
    use licash_contracts::holder::recovery;

    let s = stale_claim([15; 32]).await?;
    let window_end = past_the_response_window(&s).await?;
    // The hub plays first: its response confirms strictly after the
    // earliest timeout block.
    let round = s.openings.answer_requests().await?;
    ensure!(round.failed == 0 && round.answered == 1, "{round:?}");
    s.rt.mine(1)?;
    let observed = recovery::advance(&s.wallet, &s.chain, &s.mine, s.rate).await?[0].clone();
    ensure!(observed.error.is_none(), "{observed:?}");
    ensure!(observed.stage.disputed(), "{observed:?}");
    let x = observed.response.context("no response")?;
    let confirmed_at = s.rt.confirmations(&x)?.context("unconfirmed")?;
    ensure!(confirmed_at > window_end, "{confirmed_at} by {window_end}");
    ensure!(observed.timeout.is_none(), "{observed:?}");
    // Its assertion within the fresh window, then her challenge window.
    let done = played(&s, |_| true).await?;
    ensure!(done.assertion.is_some() && done.proved, "{done:?}");
    ensure!(done.corrected == Some(4_096), "{done:?}");
    ensure!(done.payout.is_some() && done.timeout.is_none(), "{done:?}");
    // The hub's response and assertion relayed and confirmed at their
    // presigned fee, with a fee child each.
    let assertion = done.assertion.context("no assertion")?;
    paid_its_way(&s.rt, &assertion)?;
    paid_with_child(&s.rt, &x, 2)?;
    paid_with_child(&s.rt, &assertion, 2)?;
    Ok(())
}

/// Past the response window, her timeout confirms before the hub's late
/// answer: the timeout wins, and pays her all she claimed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn her_timeout_wins_over_a_late_answer_that_confirms_after_it() -> Result<()> {
    use licash_contracts::holder::recovery::{self, Stage};

    let s = stale_claim([16; 32]).await?;
    past_the_response_window(&s).await?;
    // She plays first: her timeout confirms before the hub plays.
    let r = recovery::advance(&s.wallet, &s.chain, &s.mine, s.rate).await?[0].clone();
    ensure!(r.stage == Stage::TimingOut, "{r:?}");
    s.rt.mine(1)?;
    let done = played(&s, |_| true).await?;
    ensure!(done.response.is_none(), "{done:?}");
    ensure!(done.timeout.is_some(), "not her timeout: {done:?}");
    let received = done.received.context("nothing received")?;
    ensure!(received > 5_000, "{received} of a claim of 5000: {done:?}");
    Ok(())
}

/// A dishonest hub asserts a proof of another statement: she challenges
/// it, no kept instance answers, and the assertion's failure pays her all
/// she claimed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_holder_challenges_a_false_assertion_and_its_failure_pays_her_whole_claim() -> Result<()>
{
    let s = stale_claim([13; 32]).await?;
    s.openings.assert_false_proofs();
    // A coin for a challenge's fee, as one she held at her forced close:
    // set aside from her next round.
    let reserve = 100_000;
    let to = s.wallet.fallback_address()?.script_pubkey();
    s.rt.fund(&to, Amount::from_sat(reserve))?;
    s.rt.mine(1)?;
    scan(&s.wallet, &s.chain).await?;
    let coins = s.wallet.silent_coins()?;
    let coin = coins
        .iter()
        .find(|c| c.value == reserve)
        .context("no coin")?;
    let coin = coin.outpoint;
    let done = played(&s, |_| true).await?;
    ensure!(done.assertion.is_some() && !done.proved, "{done:?}");
    ensure!(done.challenge.is_some(), "not challenged: {done:?}");
    // Paid by `failure`, a timed payout of her claim and the hub's reserve.
    ensure!(
        done.payout.is_some() && done.timeout == done.payout,
        "{done:?}"
    );
    let received = done.received.context("nothing received")?;
    ensure!(received > 5_000, "{received} of a claim of 5000: {done:?}");
    // Her challenge and the hub's assertion, each at its presigned fee.
    paid_its_way(&s.rt, &done.assertion.context("no assertion")?)?;
    let challenge = done.challenge.context("no challenge")?;
    paid_its_way(&s.rt, &challenge)?;
    // With a fee child, of the coin set aside, at her 2 sat/vB without an
    // estimate, as on regtest; nothing left set aside.
    paid_with_child(&s.rt, &challenge, 2)?;
    ensure!(!s.rt.is_unspent(&coin)?, "the coin set aside unspent");
    let marker = licash_contracts::holder::recovery::challenge_marker(&done.id);
    let held = s.wallet.silent_coins()?;
    ensure!(held.iter().all(|c| c.spending != Some(marker)), "{held:?}");
    Ok(())
}

/// Her chain, but her claim's publication (P, spending R's claim slot
/// first) never handed to the network: a holder who requests and never
/// publishes.
struct Withholding(Node);

impl licash_wallet::Blocks for Withholding {
    async fn height(&self) -> Result<u32, String> {
        self.0.height().await
    }
    async fn hash(&self, height: u32) -> Result<bitcoin::BlockHash, String> {
        self.0.hash(height).await
    }
    async fn filter(&self, hash: &bitcoin::BlockHash) -> Result<Option<Vec<u8>>, String> {
        self.0.filter(hash).await
    }
    async fn block(&self, hash: &bitcoin::BlockHash) -> Result<bitcoin::Block, String> {
        self.0.block(hash).await
    }
}

impl holder::Chain for Withholding {
    async fn checkpoint(&self, height: u32) -> Result<licash::chain::Checkpoint, String> {
        self.0.checkpoint(height).await
    }
    async fn broadcast(&self, tx: &bitcoin::Transaction) -> Result<(), String> {
        let publication = tx.input.len() >= 3
            && tx.input[0].previous_output.vout == 2
            && tx.input[1].previous_output.vout == 1
            && tx.input[0].previous_output.txid == tx.input[1].previous_output.txid;
        if publication {
            return Ok(());
        }
        self.0.broadcast(tx).await
    }
}

/// She requests and never publishes her claim: the hub reclaims its slot
/// after k, answers her claim, empty, owed nothing, proves it and asserts;
/// her request's timeout never pays her, and her reserve comes back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_hub_reclaims_a_claim_never_published_and_answers_it_owed_nothing() -> Result<()> {
    use licash_contracts::holder::recovery::{self, Stage};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let (url, source, hub, openings) = hub_answering(&rt, &at).await?;
    let node = Node(source.clone());
    let (mine, wallet) =
        holder_wallet(&rt, &node, dir.path(), &url, [14; 32], &[60_000, 50_000]).await?;
    let opened = holder::open(&wallet, &node, &mine, &url, ask()).await?;
    let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 3_000)?;
    wallet.receive(&payment)?;
    rt.mine(2)?;
    holder::resume(&wallet, &node, &mine).await?;

    let chain = Withholding(Node(source));
    let rate = bitcoin::FeeRate::from_sat_per_vb(2).context("a rate")?;
    let forced = recovery::force_close(&wallet, &chain, &mine, &opened.id, rate).await?;
    rt.mine_to_height(forced.next_at.context("no lock")? - 1)?;
    let mut last = None;
    for _ in 0..800 {
        let round = openings.answer_requests().await?;
        ensure!(round.failed == 0, "{round:?}");
        let r = recovery::advance(&wallet, &chain, &mine, rate).await?[0].clone();
        ensure!(r.stage != Stage::Lost, "{r:?}");
        ensure!(
            !matches!(r.stage, Stage::TimingOut | Stage::TimedOut),
            "{r:?}"
        );
        if r.stage == Stage::Done {
            last = Some(r);
            break;
        }
        last = Some(r);
        rt.mine(1)?;
    }
    let done = last.context("no recovery")?;
    ensure!(done.stage == Stage::Done, "never done: {done:?}");
    // Her request confirmed, her publication never; the hub's response,
    // its assertion and `success` did.
    let r = done.request.context("no R")?;
    ensure!(rt.confirmations(&r)?.is_some(), "{done:?}");
    let p = done.publication.context("no P")?;
    // Unknown to the node: never published.
    ensure!(rt.get_tx(&p).is_err(), "P published: {done:?}");
    let slot = bitcoin::OutPoint { txid: r, vout: 2 };
    ensure!(!rt.is_unspent(&slot)?, "slot never reclaimed");
    ensure!(done.corrected == Some(0), "{done:?}");
    ensure!(done.assertion.is_some() && done.proved, "{done:?}");
    ensure!(
        done.payout.is_some() && done.timeout.is_none(),
        "not success: {done:?}"
    );
    // Her purse, R's, never spent by a P: swept back to her.
    let purse = bitcoin::OutPoint { txid: r, vout: 3 };
    ensure!(!rt.is_unspent(&purse)?, "purse stranded: {done:?}");
    ensure!(
        done.sweep.is_some() && done.received.is_some_and(|sat| sat > 10_000),
        "purse not swept: {done:?}"
    );
    eprintln!("reclaimed: {done:?}");
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
    let range = Range {
        max_cap: 50_000_000,
        max_blocks: 5_000,
        per_coin: 2,
        ppm_year: 10_000,
    };
    let openings = Openings::with_wallet(dir, source.clone(), source.clone(), keyset, Some(range));
    let config = deposit::Config {
        max: 2_000_000,
        ..deposit::Config::new(bitcoin::ScriptBuf::new_op_return([]))
    };
    let regtest = ChainProfile::Core30DefaultRegtest;
    let path = dir.join("deposits.sqlite");
    let deposits = deposit::Gateway::open(hub.clone(), source.clone(), regtest, config, &path)?;
    let deposits = Arc::new(deposits);
    let openings = openings.await?;
    // On its lock checkpoints, as the hub service's.
    licash_hub_service::open_on_lock_checkpoints(&openings, hub.clone());
    let services = Services {
        contracts: Some(openings),
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
async fn a_wallet_turns_its_coin_whole_into_a_contract_and_notes_or_opens_none() -> Result<()> {
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

    let range = (info.contracts.and_then(|c| c.range)).context("no range")?;
    let chosen = |coin, cap| Ask {
        blocks: 300,
        max_price: 30_000,
        cap: Some(cap),
        deposit: Some(coin),
    };

    // Opened on the small one, her change into notes it protects: F pays
    // a deposit of hers exactly, the hub's 1000 in it.
    let price = range.price(1_000_000, 300);
    let (pays, amount) = (1_000_000 - price, 1_000_000 - price - 1_000);
    let swept = chosen(small, 1_000_000);
    let first = holder::open_with(&wallet, &chain, &mine, &url, swept, Change::Notes).await?;
    ensure!(
        first.phase == Phase::Funded && first.price == price,
        "{first:?}"
    );
    let asked = wallet.deposits()?;
    ensure!(asked.len() == 1, "{asked:?}");
    let terms = asked[0].terms.clone().context("no terms")?;
    ensure!(asked[0].amount == amount && terms.pays == pays);
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
            && (o["value"].as_f64().unwrap_or(0.0) * 1e8).round() as u64 == pays
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
    ensure!(held == amount, "{held}");
    let contracts = holder::resume(&wallet, &chain, &mine).await?;
    ensure!(contracts[0].fate == Some(Fate::Live), "{contracts:?}");
    ensure!(scan(&wallet, &chain).await? == 3_000_000);
    // Its payment found in F, for a refund had its notes never come.
    let address: bitcoin::Address<_> = terms.address.parse()?;
    let script = address.assume_checked().script_pubkey();
    let tip = rt.height()?;
    let into = holder::paid_into(&wallet, &mine, &script, pays, tip)?;
    ensure!(into.map(|o| o.txid) == Some(f), "{into:?}");
    ensure!(holder::paid_into(&wallet, &mine, &script, pays + 1, tip)?.is_none());

    // On the large one, its change more than the contract protects, or
    // than the hub takes: none opened, no deposit asked, no change left.
    let large = (wallet.silent_coins()?.into_iter())
        .find(|c| c.value == 3_000_000)
        .context("no coin")?
        .outpoint;
    let unprotected = chosen(large, 1_000_000);
    let refused = holder::open_with(&wallet, &chain, &mine, &url, unprotected, Change::Notes).await;
    let refused = refused.expect_err("change unprotected").to_string();
    ensure!(
        refused.contains("more than the 1000000 sat protected"),
        "{refused}"
    );
    let too_much = chosen(large, 3_000_000);
    let refused = holder::open_with(&wallet, &chain, &mine, &url, too_much, Change::Notes).await;
    let refused = format!("{:#}", refused.expect_err("change over the hub's most"));
    ensure!(refused.contains("not to be notes"), "{refused}");
    ensure!(wallet.deposits()?.len() == 1);
    ensure!(holder::list(&mine)?.len() == 1);
    ensure!(scan(&wallet, &chain).await? == 3_000_000);
    Ok(())
}

/// A provider's keeper told of what cap opens the first contract at a hub
/// itself, once: its notes there taken in once it is live.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn a_providers_keeper_opens_the_first_contract_at_a_hub_where_told_to() -> Result<()> {
    use std::collections::HashMap;

    use licash_swaps::{Config as SwapsConfig, Provider};
    use licash_swaps_service::keeper::{Alert, Keeper, Settings};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let (url, source, _hub) = hub_serving(&rt, &at, None).await?;
    let mine = dir.path().join("provider");
    std::fs::create_dir(&mine)?;
    let regtest = ChainProfile::Core30DefaultRegtest;
    let wallet = Wallet::create(
        &mine.join("wallet.sqlite"),
        &[12; 32],
        regtest,
        Http::new(None)?,
    )?;
    wallet.add_hub(&url).await?;
    wallet.watch_silent(None, rt.height()?)?;
    let to = wallet.fallback_address()?.script_pubkey();
    rt.fund(&to, Amount::from_sat(60_000))?;
    rt.mine(1)?;
    let swaps = mine.join("swaps.sqlite");
    let provider = Arc::new(Provider::create(&swaps, wallet, SwapsConfig::default())?);
    let settings = Settings {
        max_price: Some(30_000),
        open_cap: Some(4_096),
        scan: true,
        ..Settings::new(regtest, 0, 0, "open-contract {hub}".into())
    };
    let mut keeper = Keeper::new(provider.clone(), Node(source), mine.clone(), settings);
    let limited = |most: u64| Some(HashMap::from([(url.clone(), most)]));

    // None at the hub: it opens one, taking nothing in meanwhile.
    let plan = keeper.round().await?;
    ensure!(
        plan.alerts == [Alert::NoContract { hub: url.clone() }],
        "{plan:?}"
    );
    ensure!(provider.intake() == limited(0));
    let events = provider.events(0, 1_000)?;
    ensure!(
        (events.iter())
            .any(|e| e.kind == "alert" && e.data.to_string().contains("opening contract")),
        "{events:?}"
    );
    let opened = holder::list(&mine)?;
    ensure!(opened.len() == 1 && opened[0].hub == url, "{opened:?}");
    ensure!(opened[0].cap == 4_096 && opened[0].price <= 30_000);

    // Once: being opened, then live, its cap taken in.
    let plan = keeper.round().await?;
    ensure!(plan.alerts.is_empty(), "{plan:?}");
    rt.mine(1)?;
    keeper.round().await?;
    keeper.round().await?;
    ensure!(holder::list(&mine)?.len() == 1);
    ensure!(
        provider.intake() == limited(4_096),
        "{:?}",
        provider.intake()
    );
    Ok(())
}

/// A provider found by her app that pays nothing out.
struct NoProvider;

impl licash_contracts::holder::exit::CashOut for NoProvider {
    async fn quote_all(&self, _: &str, _: &str) -> Result<licash_wallet::PayoutQuote> {
        anyhow::bail!("no provider pays out now")
    }
}

/// Her exit from a contract ending, notes at its hub no other contract
/// recovers: nothing done without her consent, only told; consented, her
/// notes paid out, which fails here, then forced closed a day before its
/// request would have to confirm.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn her_exit_needs_consent_then_forces_closed_what_it_could_not_pay_out() -> Result<()> {
    use bitcoin::FeeRate;
    use licash_contracts::holder::{exit, recovery, safety};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let range = Range {
        max_cap: 50_000_000,
        max_blocks: 5_000,
        per_coin: 2,
        ppm_year: 10_000,
    };
    let (url, source, hub) = hub_serving(&rt, &at, Some(range)).await?;
    let chain = Node(source);
    let coins = [60_000, 60_000];
    let (mine, wallet) = holder_wallet(&rt, &chain, dir.path(), &url, [12; 32], &coins).await?;
    let address = wallet.fallback_address()?.to_string();
    let (margin, rate) = (10, FeeRate::from_sat_per_vb_u32(2));
    let drive = || exit::drive(&wallet, &chain, &mine, &NoProvider, margin, rate);
    // No contract: nothing to consent to exit, nothing made.
    ensure!(exit::consent_to(&wallet, &mine, &address, margin).is_err());
    ensure!(drive().await?.is_empty());
    ensure!(!mine.join("contracts.sqlite").exists());

    let ask = Ask {
        blocks: 1_000,
        ..ask()
    };
    let contract = holder::open(&wallet, &chain, &mine, &url, ask).await?;
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;
    let live = holder::list(&mine)?;
    ensure!(live[0].live(rt.height()?), "{live:?}");
    let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 3_000)?;
    wallet.receive(&payment)?;
    let tip = rt.height()?;
    let s = safety::of(
        &wallet,
        &mine,
        &contract.id,
        tip,
        margin,
        exit::TIMEOUT_SLACK,
    )?;
    let by = s.request_by.context("no request deadline")?;
    let (cash_at, force_at) = (by - 3 * 144, by - 144);
    ensure!(cash_at > tip, "{s:?} at {tip}");

    let exits = drive().await?;
    ensure!(exits.len() == 1, "{exits:?}");
    ensure!(
        (exits[0].step, exits[0].cash_at, exits[0].force_at)
            == (exit::Step::Waiting, cash_at, force_at),
        "{exits:?}"
    );
    // Due, her notes at risk, and no consent: she is told, nothing done,
    // even past its forced close's block.
    rt.mine_to_height(force_at)?;
    let exits = drive().await?;
    ensure!(exits[0].step == exit::Step::NeedsConsent, "{exits:?}");
    ensure!(exits[0].uncovered == 3_000, "{exits:?}");
    ensure!(recovery::recoveries(&mine)?.is_empty());
    ensure!(exit::consent(&mine)?.is_none());

    // Consented: forced closed at once, its notes not paid out in time.
    let consent = exit::consent_to(&wallet, &mine, &address, margin)?;
    ensure!(consent.address == address && consent.exit_margin == margin);
    let exits = drive().await?;
    ensure!(
        exits[0].step == exit::Step::ForceClosing && exits[0].error.is_none(),
        "{exits:?}"
    );
    let recoveries = recovery::recoveries(&mine)?;
    ensure!(
        recoveries.len() == 1 && recoveries[0].id == contract.id,
        "{recoveries:?}"
    );
    // Under recovery: the exit has nothing more to do of it.
    ensure!(drive().await?.is_empty());
    exit::revoke(&mine)?;
    ensure!(exit::consent(&mine)?.is_none());
    Ok(())
}

/// Her exit, consented, before its forced close's block: her notes paid
/// out, which fails without a provider, told and tried again next round,
/// nothing locked.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs bitcoind and a release build"]
async fn her_exit_tries_to_pay_out_before_forcing_closed() -> Result<()> {
    use bitcoin::FeeRate;
    use licash_contracts::holder::{exit, recovery, safety};

    let rt = Regtest::start_standard()?;
    let dir = tempfile::tempdir()?;
    let at = dir.path().join("hub");
    let range = Range {
        max_cap: 50_000_000,
        max_blocks: 5_000,
        per_coin: 2,
        ppm_year: 10_000,
    };
    let (url, source, hub) = hub_serving(&rt, &at, Some(range)).await?;
    let chain = Node(source);
    let coins = [60_000, 60_000];
    let (mine, wallet) = holder_wallet(&rt, &chain, dir.path(), &url, [13; 32], &coins).await?;
    let address = wallet.fallback_address()?.to_string();
    let (margin, rate) = (10, FeeRate::from_sat_per_vb_u32(2));
    let ask = Ask {
        blocks: 1_000,
        ..ask()
    };
    let contract = holder::open(&wallet, &chain, &mine, &url, ask).await?;
    rt.mine(1)?;
    holder::resume(&wallet, &chain, &mine).await?;
    let payment = pay(&hub, &at.join("seed"), &url, &wallet.address(), 3_000)?;
    wallet.receive(&payment)?;
    exit::consent_to(&wallet, &mine, &address, margin)?;
    let s = safety::of(
        &wallet,
        &mine,
        &contract.id,
        rt.height()?,
        margin,
        exit::TIMEOUT_SLACK,
    )?;
    let by = s.request_by.context("no request deadline")?;
    rt.mine_to_height(by - 3 * 144)?;
    for _ in 0..2 {
        let exits = exit::drive(&wallet, &chain, &mine, &NoProvider, margin, rate).await?;
        ensure!(exits[0].step == exit::Step::CashingOut, "{exits:?}");
        let error = exits[0].error.as_deref().unwrap_or_default();
        ensure!(error.contains("no provider pays out now"), "{exits:?}");
        ensure!(recovery::recoveries(&mine)?.is_empty());
        ensure!(wallet.payouts()?.is_empty() && wallet.locks()?.is_empty());
        rt.mine(1)?;
    }
    // A margin preferred since changes nothing: her consent's is planned.
    let exits = exit::drive(&wallet, &chain, &mine, &NoProvider, 1_000, rate).await?;
    ensure!(exits[0].step == exit::Step::CashingOut, "{exits:?}");
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
        exit_margin: margin,
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
