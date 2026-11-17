//! Payouts on Bitcoin through a provider in process: its coins and its
//! clients' claims on the hub's own chain, which a mempool here holds until
//! the tests mine it.

#[allow(dead_code)]
#[path = "../../licash-wallet/tests/net/mod.rs"]
mod net;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use bitcoin::{
    hashes::Hash, transaction, Amount, BlockHash, FeeRate, OutPoint, Transaction, TxIn, TxOut, Txid,
};
use licash::spend::{lock_hash, ChainProfile};
use licash_api::{
    swaps::{self, ChainRequest},
    Code, Fee, Problem,
};
use licash_hub::Timing;
use licash_swaps::{Chain, Config, Provider, Stage};
use licash_wallet::{Blocks, Error, Payout, PayoutState, Response, Transport, Wallet};
use net::{Link, Net, View, TIMING};

const A: &str = "http://a.test";
const PROVIDER: &str = "http://swaps.test";
const FEE: Fee = Fee { base: 1, ppm: 0 };
/// Locks of up to 141 blocks, room for a payout's coin to time out first.
const TIMED: Timing = Timing {
    window: 144,
    ..TIMING
};
const CONFIG: Config = Config {
    fee: Fee { base: 1, ppm: 0 },
    min: 5,
    max: 150,
    margin: 6,
    confirm: 4,
};
const TERMS: swaps::Chain = swaps::Chain {
    fee: Fee {
        base: 1,
        ppm: 10_000,
    },
    miner: 200,
    rate: 1_000,
    min: 500,
    max: 50_000,
    margin: 36,
};
/// What the client holds at A to start with: notes of up to 64 each.
const HELD: u64 = 3_000;
/// What the provider holds on chain to start with.
const STOCK: u64 = 100_000;
/// A regtest address, P2WPKH: BIP173's.
const PAYEE: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
const MAX_FEE: u64 = 2_000;
const ONE_SAT: FeeRate = FeeRate::from_sat_per_vb_u32(1);

/// The hub's chain and a mempool: what is broadcast waits there until a
/// test mines it.
#[derive(Clone)]
struct Pool {
    net: Arc<Net>,
    view: View,
    txs: Arc<Mutex<Vec<Transaction>>>,
}

impl Pool {
    /// Mine what waits, but what `keep` refuses, in one block.
    fn mine_if(&self, keep: impl Fn(&Transaction) -> bool) -> Vec<Transaction> {
        let txs: Vec<Transaction> = std::mem::take(&mut *self.txs.lock().unwrap());
        let (mined, dropped): (Vec<_>, Vec<_>) = txs.into_iter().partition(|tx| keep(tx));
        self.net.confirm(A, mined.clone());
        drop(dropped);
        mined
    }

    fn mine(&self) -> Vec<Transaction> {
        self.mine_if(|_| true)
    }

    fn waiting(&self) -> Vec<Transaction> {
        self.txs.lock().unwrap().clone()
    }
}

impl Blocks for Pool {
    fn height(&self) -> impl std::future::Future<Output = Result<u32, String>> + Send {
        Blocks::height(&self.view)
    }

    fn hash(
        &self,
        height: u32,
    ) -> impl std::future::Future<Output = Result<BlockHash, String>> + Send {
        self.view.hash(height)
    }

    fn filter(
        &self,
        hash: &BlockHash,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, String>> + Send {
        self.view.filter(hash)
    }

    fn block(
        &self,
        hash: &BlockHash,
    ) -> impl std::future::Future<Output = Result<bitcoin::Block, String>> + Send {
        self.view.block(hash)
    }
}

impl Chain for Pool {
    fn broadcast(
        &self,
        tx: &Transaction,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let mut txs = self.txs.lock().unwrap();
        if !txs.iter().any(|t| t.compute_txid() == tx.compute_txid()) {
            txs.push(tx.clone());
        }
        std::future::ready(Ok(()))
    }
}

/// The provider, as its clients reach it.
struct Desk {
    provider: Mutex<Option<Arc<Provider<Link>>>>,
    pool: Pool,
}

impl Desk {
    fn provider(&self) -> Arc<Provider<Link>> {
        self.provider.lock().unwrap().clone().unwrap()
    }

    async fn serve(&self, path: &str, body: Option<Vec<u8>>) -> Result<Response, String> {
        let provider = self.provider();
        let answer = match (path, body) {
            (swaps::SWAPS_INFO, None) => Ok(serde_json::to_vec(&provider.info().unwrap()).unwrap()),
            (swaps::CHAIN_SWAPS, Some(body)) => {
                let request: ChainRequest = serde_json::from_slice(&body).unwrap();
                provider
                    .take_payout(&request, &self.pool)
                    .await
                    .map(|swap| serde_json::to_vec(&swap).unwrap())
            }
            _ => Err(Problem::invalid("no such route")),
        };
        Ok(match answer {
            Ok(body) => Response { status: 200, body },
            Err(problem) => Response {
                status: problem.code.status(),
                body: serde_json::to_vec(&problem).unwrap(),
            },
        })
    }
}

/// A client's way to the hub and the provider.
#[derive(Clone)]
struct Client(Arc<Net>, Arc<Desk>);

impl Transport for Client {
    fn request(
        &self,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<Response, String>> + Send {
        let (net, desk, url) = (self.0.clone(), self.1.clone(), url.to_owned());
        async move {
            match url.strip_prefix(PROVIDER) {
                Some(path) => desk.serve(path, body).await,
                None => net.serve(&url, body),
            }
        }
    }
}

/// A transaction paying `output` of a coin made up: the hub's chain checks
/// nothing.
fn made_up(output: TxOut) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([0xf0; 32]), 3),
            ..Default::default()
        }],
        output: vec![output],
    }
}

struct Eco {
    net: Arc<Net>,
    desk: Arc<Desk>,
    dir: tempfile::TempDir,
    client: Wallet<Client>,
    pool: Pool,
}

impl Eco {
    /// Hub A at height 3, a provider holding `stock` on chain and paying
    /// out on `terms`, and a client holding 3,000 at A.
    async fn new(stock: u64, terms: swaps::Chain) -> Self {
        let net = Net::new();
        net.start_timed(A, FEE, TIMED);
        let dir = tempfile::tempdir().unwrap();
        let wallet = Wallet::create(
            &dir.path().join("wallet.sqlite"),
            &[0x50; 32],
            ChainProfile::Core30DefaultRegtest,
            Link(net.clone()),
        )
        .unwrap();
        wallet.add_hub(A).await.unwrap();
        wallet.watch_silent(None, 0).unwrap();
        if stock > 0 {
            let to = wallet.fallback_address().unwrap().script_pubkey();
            let output = TxOut {
                value: Amount::from_sat(stock),
                script_pubkey: to,
            };
            net.confirm(A, vec![made_up(output)]);
        }
        let provider = Provider::create(&dir.path().join("swaps.sqlite"), wallet, CONFIG)
            .unwrap()
            .pay_out(terms);
        let pool = Pool {
            net: net.clone(),
            view: net.view(A),
            txs: Arc::default(),
        };
        let desk = Arc::new(Desk {
            provider: Mutex::new(Some(Arc::new(provider))),
            pool: pool.clone(),
        });
        let link = Client(net.clone(), desk.clone());
        let client = Wallet::create(
            &dir.path().join("client.sqlite"),
            &[0xc1; 32],
            ChainProfile::Core30DefaultRegtest,
            link,
        )
        .unwrap();
        client.add_hub(A).await.unwrap();
        client
            .receive(&net.pay(A, &client.address(), HELD))
            .unwrap();
        let eco = Self {
            net,
            desk,
            dir,
            client,
            pool,
        };
        eco.follow().await;
        eco
    }

    fn provider(&self) -> Arc<Provider<Link>> {
        self.desk.provider()
    }

    /// The provider, stopped and opened again.
    fn restart(&self) {
        let provider = self.desk.provider.lock().unwrap().take().unwrap();
        drop(Arc::into_inner(provider).expect("the provider is held once"));
        let wallet = Wallet::open(
            &self.dir.path().join("wallet.sqlite"),
            Link(self.net.clone()),
        )
        .unwrap();
        let provider = Provider::open(&self.dir.path().join("swaps.sqlite"), wallet, CONFIG)
            .unwrap()
            .pay_out(TERMS);
        *self.desk.provider.lock().unwrap() = Some(Arc::new(provider));
    }

    /// The provider follows its payouts: none pending.
    async fn follow(&self) {
        let pending = self.provider().follow_payouts(&self.pool).await.unwrap();
        assert!(pending.is_empty(), "{pending:?}");
        self.provider().step().await.unwrap();
    }

    async fn pay_out(&self, amount: u64) -> Payout {
        let payout = self
            .client
            .payout(PROVIDER, A, PAYEE, amount, MAX_FEE, ONE_SAT)
            .await
            .unwrap();
        assert!(payout.lock.is_some(), "{payout:?}");
        payout
    }

    /// The client follows `payout`, and broadcasts its claims: where it
    /// stands.
    async fn client_follows(&self, payout: &Payout) -> Payout {
        let followed = self
            .client
            .follow_payout(&payout.hash, &self.pool.view)
            .await;
        for claim in self.client.payout_claims().unwrap() {
            self.pool.broadcast(&claim).await.unwrap();
        }
        match followed {
            Ok(payout) => payout,
            Err(Error::Pending { .. }) => self.client.payouts().unwrap().remove(0),
            Err(e) => panic!("{e}"),
        }
    }

    fn stage(&self) -> Stage {
        self.provider().records(1).unwrap()[0].stage
    }

    fn mine_to(&self, height: u32) {
        let tip = self.pool.view.height();
        assert!(tip <= height, "{tip} past {height}");
        self.net.mine(A, height - tip, None);
    }

    /// What the provider holds at A.
    fn provider_held(&self) -> u64 {
        let balance = self.provider().wallet().balance().unwrap();
        balance
            .iter()
            .find(|b| b.hub == A)
            .map_or(0, |b| b.available)
    }

    /// What the provider's coins on chain pay.
    fn provider_coins(&self) -> u64 {
        let coins = self.provider().wallet().silent_coins().unwrap();
        coins.iter().map(|c| c.value).sum()
    }
}

/// The coin of a payout, as its provider says.
fn timeout(eco: &Eco) -> u32 {
    eco.provider().records(1).unwrap()[0].expiry
}

fn is_claim(tx: &Transaction) -> bool {
    tx.output.len() == 1 && tx.output[0].value == Amount::from_sat(1_000)
}

#[tokio::test]
async fn a_payout_claimed_pays_the_provider_its_lock() {
    let eco = Eco::new(STOCK, TERMS).await;
    let info = eco.provider().info().unwrap().chain.unwrap();
    assert_eq!(info, TERMS);
    let payout = eco.pay_out(1_000).await;
    let value = payout.amount + payout.budget;
    assert_eq!(eco.stage(), Stage::Accepted);
    // The coin waits to be mined: nothing claimed before.
    let coin = eco.pool.waiting();
    assert_eq!(coin.len(), 1);
    assert_eq!(eco.client_follows(&payout).await.state, PayoutState::Open);
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Locked);
    let claimed = eco.client_follows(&payout).await;
    assert_eq!(claimed.state, PayoutState::Claimed);
    assert_eq!(claimed.funding.unwrap().txid, coin[0].compute_txid());
    let mined = eco.pool.mine();
    assert!(mined.iter().any(is_claim), "{mined:?}");
    eco.follow().await;
    let record = eco.provider().records(1).unwrap().remove(0);
    assert_eq!(record.stage, Stage::Done);
    assert_eq!(record.amount, value);
    assert_eq!(record.fee, TERMS.pays(value) - value);
    assert_eq!(eco.provider_held(), record.paid);
    let stats = eco.provider().stats().unwrap();
    assert_eq!((stats.done, stats.earned), (1, record.fee));
    // Its change found once mined: what it held less the coin and its
    // miners.
    let change = eco.provider_coins();
    assert!(
        change < STOCK - value && change > STOCK - value - 500,
        "{change}"
    );
    assert_eq!(eco.client_follows(&payout).await.state, PayoutState::Done);
    // The same request again answers where it stands.
    let lock = licash::spend::ProposalId::from_bytes(
        hex::decode(payout.lock.as_ref().unwrap())
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let answer = eco.provider().payout(&lock).unwrap().unwrap();
    assert_eq!(answer.state, swaps::State::Done);
    assert!(eco.provider().swap(&lock).unwrap().is_none());
}

#[tokio::test]
async fn a_payouts_coin_is_told_of_before_it_confirms() {
    let eco = Eco::new(STOCK, TERMS).await;
    let payout = eco.pay_out(1_000).await;
    let coin = eco.pool.waiting();
    assert_eq!(coin.len(), 1);
    let txid = coin[0].compute_txid();
    let lock = licash::spend::ProposalId::from_bytes(
        hex::decode(payout.lock.as_ref().unwrap())
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let answer = eco.provider().payout(&lock).unwrap().unwrap();
    assert_eq!(answer.funding, None);
    assert_eq!(answer.txid, Some(txid.to_string()));
    // The client shows it at once, in no block yet.
    assert_eq!(eco.client_follows(&payout).await.state, PayoutState::Open);
    let progress = eco.client.payout_progress(&payout.hash).unwrap();
    assert_eq!((progress.coin, progress.coin_height), (Some(txid), None));
    eco.pool.mine();
    eco.follow().await;
    let claimed = eco.client_follows(&payout).await;
    assert_eq!(claimed.state, PayoutState::Claimed);
    let progress = eco.client.payout_progress(&payout.hash).unwrap();
    assert_eq!(progress.coin, Some(txid));
    assert_eq!(progress.coin_height, Some(eco.pool.view.height()));
}

#[tokio::test]
async fn a_coin_never_claimed_is_taken_back_once_timed_out() {
    let eco = Eco::new(STOCK, TERMS).await;
    let payout = eco.pay_out(1_000).await;
    let value = payout.amount + payout.budget;
    eco.pool.mine();
    eco.follow().await;
    let held = eco.provider_coins();
    let timeout = timeout(&eco);
    eco.mine_to(timeout - 1);
    eco.follow().await;
    assert!(eco.pool.waiting().is_empty(), "taken back too early");
    eco.mine_to(timeout);
    eco.follow().await;
    let refund = eco.pool.waiting();
    assert_eq!(refund.len(), 1);
    assert_eq!(refund[0].lock_time.to_consensus_u32(), timeout);
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Refunded);
    let back = eco.provider_coins() - held;
    assert!(back < value && back > value - 500, "{back}");
    assert_eq!(eco.provider().stats().unwrap().refunded, 1);
    // The client's lock is the client's again once expired.
    let lock = eco.client.locks().unwrap().remove(0);
    eco.mine_to(lock.expiry + 1);
    eco.client.resume().await.unwrap();
    let left = eco.client.balance().unwrap()[0].available;
    assert_eq!(left, HELD - FEE.base);
}

#[tokio::test]
async fn a_claim_mined_late_still_pays_the_provider_inside_the_claim_margin() {
    let eco = Eco::new(STOCK, TERMS).await;
    let payout = eco.pay_out(1_000).await;
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(
        eco.client_follows(&payout).await.state,
        PayoutState::Claimed
    );
    // The claim stays out of the blocks past the coin's timeout, its
    // refund out of them all.
    let claim = eco.pool.waiting();
    eco.pool.mine_if(|_| false);
    eco.mine_to(timeout(&eco));
    eco.follow().await;
    assert_eq!(eco.pool.waiting().len(), 1, "a refund");
    let lock = eco.client.locks().unwrap().remove(0);
    eco.mine_to(lock.expiry - licash_wallet::CLAIM_MARGIN - 1);
    eco.pool.mine_if(|_| false);
    for tx in &claim {
        eco.pool.broadcast(tx).await.unwrap();
    }
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Done);
    assert!(eco.provider_held() > 1_000);
}

#[tokio::test]
async fn a_provider_stopped_mid_payout_takes_it_on() {
    let eco = Eco::new(STOCK, TERMS).await;
    let payout = eco.pay_out(1_000).await;
    eco.restart();
    eco.pool.txs.lock().unwrap().clear();
    // Broadcast again until mined.
    eco.follow().await;
    assert_eq!(eco.pool.waiting().len(), 1);
    eco.pool.mine();
    eco.restart();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Locked);
    eco.client_follows(&payout).await;
    eco.pool.mine();
    eco.restart();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Done);
}

#[tokio::test]
async fn a_payout_is_declined_beyond_the_providers_terms() {
    let eco = Eco::new(STOCK, TERMS).await;
    let declined = |result: Result<swaps::ChainSwap, Problem>, why: &str| match result {
        Err(problem) => {
            assert_eq!(problem.code, Code::Declined, "{problem:?}");
            assert!(problem.message.contains(why), "{problem:?}");
        }
        Ok(swap) => panic!("{swap:?}"),
    };
    let from = eco.net.hub(A).info().address;
    let provider = eco.provider().wallet().address();
    // A lock expiring too soon for a coin to time out 36 blocks earlier,
    // with room to claim.
    let tip = eco.pool.view.height();
    let preimage = [7; 32];
    let soon = eco
        .client
        .lock(
            A,
            &provider,
            1_300,
            lock_hash(&preimage),
            Some(tip + 36 + 18),
        )
        .await
        .unwrap();
    let id =
        licash::spend::ProposalId::from_bytes(hex::decode(&soon.id).unwrap().try_into().unwrap());
    let request = ChainRequest::new(&from, &id, [2; 32], 1_000);
    let request = ChainRequest {
        claim: licash_api::Hex(
            bitcoin::secp256k1::SecretKey::from_slice(&[3; 32])
                .unwrap()
                .x_only_public_key(&bitcoin::secp256k1::Secp256k1::new())
                .0
                .serialize()
                .to_vec(),
        ),
        ..request
    };
    declined(
        eco.provider().take_payout(&request, &eco.pool).await,
        "too soon",
    );
    let more = ChainRequest {
        value: 1_100,
        ..request.clone()
    };
    declined(
        eco.provider().take_payout(&more, &eco.pool).await,
        "the lock pays 1300",
    );
    let big = ChainRequest {
        value: 60_000,
        ..request.clone()
    };
    declined(
        eco.provider().take_payout(&big, &eco.pool).await,
        "coins of 500 to",
    );
    assert!(eco.pool.waiting().is_empty());
    assert!(eco.provider().records(10).unwrap().is_empty());
    // A notes swap of a payout's lock is another swap.
    let payout = eco.pay_out(1_000).await;
    let lock = payout.lock.unwrap();
    let id = licash::spend::ProposalId::from_bytes(hex::decode(&lock).unwrap().try_into().unwrap());
    let notes = swaps::Request::new(&from, &id, &from, &provider, 1_000);
    match eco.provider().take(&notes).await {
        Err(problem) => assert!(problem.message.contains("another swap"), "{problem:?}"),
        Ok(swap) => panic!("{swap:?}"),
    }
}

#[tokio::test]
async fn a_payout_takes_notes_at_a_hub_only_within_the_providers_limit_there() {
    let eco = Eco::new(STOCK, TERMS).await;
    let provider = eco.provider();
    let payout = eco.pay_out(1_000).await;
    let paid = provider.records(1).unwrap()[0].paid;
    assert!(paid > payout.amount + payout.budget, "{paid}");
    // What its client's lock pays it counts as held at A, before and after
    // it is claimed.
    assert_eq!(provider.exposure(A).unwrap(), paid);
    eco.pool.mine();
    eco.follow().await;
    eco.client_follows(&payout).await;
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Done);
    assert_eq!(eco.provider_held(), paid);
    assert_eq!(provider.exposure(A).unwrap(), paid);

    // Limited to that much of A's notes: no payout taken from there.
    provider.limit_intake(Some(HashMap::from([(A.to_owned(), paid + 1)])));
    let more = eco.net.pay(A, &eco.client.address(), HELD);
    eco.client.receive(&more).unwrap();
    let declined = eco
        .client
        .payout(PROVIDER, A, PAYEE, 1_000, MAX_FEE, ONE_SAT)
        .await
        .unwrap();
    match &declined.state {
        PayoutState::Failed(why) => assert!(
            why.contains("the provider takes no more of hub http://a.test's notes now"),
            "{why}"
        ),
        state => panic!("{state:?}"),
    }
    assert_eq!(provider.records(10).unwrap().len(), 1);
    assert!(eco.pool.waiting().is_empty());
}

#[tokio::test]
async fn a_provider_without_coins_on_chain_pays_nothing_out() {
    let eco = Eco::new(0, TERMS).await;
    assert_eq!(eco.provider().info().unwrap().chain.unwrap().max, 0);
    let quote = eco
        .client
        .payout_quote(PROVIDER, A, PAYEE, 1_000, MAX_FEE, ONE_SAT)
        .await;
    assert!(matches!(quote, Err(Error::Invalid(_))), "{quote:?}");
    // Nor once its coins are all spent.
    let eco = Eco::new(2_500, TERMS).await;
    let max = eco.provider().info().unwrap().chain.unwrap().max;
    assert!(max < 2_500 && max > 2_300, "{max}");
    eco.pay_out(1_000).await;
    eco.pool.mine();
    eco.follow().await;
    let left = eco.provider().info().unwrap().chain.unwrap().max;
    assert!(left < 1_000, "{left}");
}

#[tokio::test]
async fn the_operator_is_alerted_once_when_its_coins_pay_out_too_little() {
    let alerts = |eco: &Eco| {
        let events = eco.provider().events(0, 50).unwrap();
        events
            .iter()
            .filter(|e| matches!(e.notice(), Some(licash_swaps::Notice::Alert { message }) if message.contains("Bitcoin")))
            .count()
    };
    let eco = Eco::new(STOCK, TERMS).await;
    assert!(!eco.provider().check_payouts().unwrap());
    let eco = Eco::new(0, TERMS).await;
    assert!(eco.provider().check_payouts().unwrap());
    assert!(!eco.provider().check_payouts().unwrap());
    assert_eq!(alerts(&eco), 1);
    // Funded, it pays out again; spent down, it is told again.
    let to = eco
        .provider()
        .wallet()
        .fallback_address()
        .unwrap()
        .script_pubkey();
    let coin = TxOut {
        value: Amount::from_sat(2_500),
        script_pubkey: to,
    };
    eco.net.confirm(A, vec![made_up(coin)]);
    eco.follow().await;
    assert!(!eco.provider().check_payouts().unwrap());
    eco.pay_out(1_800).await;
    eco.pool.mine();
    eco.follow().await;
    assert!(eco.provider().check_payouts().unwrap());
    assert_eq!(alerts(&eco), 2);
}

#[tokio::test]
async fn a_swaps_file_of_version_1_gains_payouts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swaps.sqlite");
    let version = |path: &std::path::Path| -> i64 {
        let c = rusqlite::Connection::open(path).unwrap();
        c.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(include_str!("fixtures/swaps-v1.sql"))
        .unwrap();
    assert_eq!(version(&path), 1);
    let net = Net::new();
    net.start_timed(A, FEE, TIMED);
    let wallet = Wallet::create(
        &dir.path().join("wallet.sqlite"),
        &[0x50; 32],
        ChainProfile::Core30DefaultRegtest,
        Link(net.clone()),
    )
    .unwrap();
    let provider = Provider::open(&path, wallet, CONFIG).unwrap();
    assert_eq!(version(&path), 5);
    let records = provider.records(10).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        (records[0].stage, records[0].amount),
        (Stage::Accepted, 1000)
    );
    assert_eq!(provider.last_event().unwrap(), 1);
    let id = licash::spend::ProposalId::from_bytes([0xab; 32]);
    assert!(provider.swap(&id).unwrap().is_some());
    assert!(provider.payout(&id).unwrap().is_none());
    // A swap of notes still, taken on as one: stopped before its lock.
    provider.step().await.unwrap();
    assert_eq!(provider.records(1).unwrap()[0].stage, Stage::Failed);
    drop(provider);
    // A newer file is refused untouched.
    rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 6)
        .unwrap();
    let wallet = Wallet::open(&dir.path().join("wallet.sqlite"), Link(net)).unwrap();
    match Provider::open(&path, wallet, CONFIG) {
        Err(licash_swaps::Error::Setup(why)) => assert!(why.contains("schema 6"), "{why}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("opened"),
    }
    assert_eq!(version(&path), 6);
}

#[tokio::test]
async fn a_swaps_file_of_version_2_gains_the_locks_a_hub_issues() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swaps.sqlite");
    let db = || rusqlite::Connection::open(&path).unwrap();
    db().execute_batch(include_str!("fixtures/swaps-v1.sql"))
        .unwrap();
    let net = Net::new();
    net.start_timed(A, FEE, TIMED);
    let wallet = Wallet::create(
        &dir.path().join("wallet.sqlite"),
        &[0x50; 32],
        ChainProfile::Core30DefaultRegtest,
        Link(net.clone()),
    )
    .unwrap();
    drop(Provider::open(&path, wallet, CONFIG).unwrap());
    // As a provider of version 2 left it: its swaps and payouts, no
    // issues nor invoices nor donations.
    db().execute_batch(
        "DROP TABLE donations; DROP TABLE invoices; DROP TABLE issues;
         PRAGMA user_version = 2;",
    )
    .unwrap();
    let wallet = Wallet::open(&dir.path().join("wallet.sqlite"), Link(net)).unwrap();
    let provider = Provider::open(&path, wallet, CONFIG).unwrap();
    let version: i64 = db()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 5);
    for table in ["issues", "invoices", "donations"] {
        let count: i64 = db()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    let records = provider.records(10).unwrap();
    assert_eq!(
        (records.len(), records[0].stage, records[0].amount),
        (1, Stage::Accepted, 1000)
    );
    assert_eq!(provider.last_event().unwrap(), 1);
}

#[tokio::test]
async fn a_swaps_file_of_version_3_gains_invoices() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swaps.sqlite");
    let db = || rusqlite::Connection::open(&path).unwrap();
    db().execute_batch(include_str!("fixtures/swaps-v1.sql"))
        .unwrap();
    let net = Net::new();
    net.start_timed(A, FEE, TIMED);
    let wallet = Wallet::create(
        &dir.path().join("wallet.sqlite"),
        &[0x50; 32],
        ChainProfile::Core30DefaultRegtest,
        Link(net.clone()),
    )
    .unwrap();
    drop(Provider::open(&path, wallet, CONFIG).unwrap());
    // As a provider of version 3 left it: no invoices nor donations.
    db().execute_batch("DROP TABLE donations; DROP TABLE invoices; PRAGMA user_version = 3;")
        .unwrap();
    let wallet = Wallet::open(&dir.path().join("wallet.sqlite"), Link(net)).unwrap();
    let provider = Provider::open(&path, wallet, CONFIG).unwrap();
    let version: i64 = db()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 5);
    let invoices: i64 = db()
        .query_row("SELECT count(*) FROM invoices", [], |r| r.get(0))
        .unwrap();
    assert_eq!(invoices, 0);
    // Its swap untouched, and nothing paid over Lightning under way.
    let records = provider.records(10).unwrap();
    assert_eq!(
        (records.len(), records[0].stage, records[0].amount),
        (1, Stage::Accepted, 1000)
    );
    assert_eq!(provider.open_invoices().unwrap(), 0);
    assert_eq!(provider.last_event().unwrap(), 1);
}

#[tokio::test]
async fn a_swaps_file_of_version_4_gains_donations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swaps.sqlite");
    let db = || rusqlite::Connection::open(&path).unwrap();
    db().execute_batch(include_str!("fixtures/swaps-v1.sql"))
        .unwrap();
    let net = Net::new();
    net.start_timed(A, FEE, TIMED);
    let wallet = Wallet::create(
        &dir.path().join("wallet.sqlite"),
        &[0x50; 32],
        ChainProfile::Core30DefaultRegtest,
        Link(net.clone()),
    )
    .unwrap();
    drop(Provider::open(&path, wallet, CONFIG).unwrap());
    // As a provider of version 4 left it: its invoices, no donations.
    db().execute_batch("DROP TABLE donations; PRAGMA user_version = 4;")
        .unwrap();
    // Read before it is migrated: its swap's hash reserved, no other.
    assert!(licash_swaps::holds_hash(&path, &[0xcd; 32]).unwrap());
    assert!(!licash_swaps::holds_hash(&path, &[0xef; 32]).unwrap());
    let wallet = Wallet::open(&dir.path().join("wallet.sqlite"), Link(net)).unwrap();
    let provider = Provider::open(&path, wallet, CONFIG).unwrap();
    let version: i64 = db()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 5);
    let donations: i64 = db()
        .query_row("SELECT count(*) FROM donations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(donations, 0);
    // Its swap untouched, and no donation under way nor taken.
    let records = provider.records(10).unwrap();
    assert_eq!(
        (records.len(), records[0].stage, records[0].amount),
        (1, Stage::Accepted, 1000)
    );
    assert!(provider.donation_terms().is_none());
    assert!(provider.follow_donations().await.unwrap().is_empty());
    assert!(licash_swaps::holds_hash(&path, &[0xcd; 32]).unwrap());
    assert!(!licash_swaps::holds_hash(&path, &[0xef; 32]).unwrap());
}
