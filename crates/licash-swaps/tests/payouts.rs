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
use licash_onchain::deposit::Htlc;
use licash_swaps::{Chain, Config, PayoutLimits, Provider, Stage, NO_SHOW_VSIZE};
use licash_wallet::{Blocks, Error, Payee, Payout, PayoutState, Response, Transport, Wallet};
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
    /// The fee rate the chain estimates, for any blocks.
    estimate: Arc<Mutex<Option<FeeRate>>>,
    /// Blocks the chain's height is said to be behind its tip by, as a
    /// lagging chain source says.
    lag: Arc<Mutex<u32>>,
}

impl Pool {
    fn estimate(&self, rate: Option<FeeRate>) {
        *self.estimate.lock().unwrap() = rate;
    }

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
        let lag = *self.lag.lock().unwrap();
        let height = Blocks::height(&self.view);
        async move { height.await.map(|h| h.saturating_sub(lag)) }
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
            // In place of those spending a coin it spends.
            let spends = |t: &Transaction| {
                t.input.iter().any(|i| {
                    tx.input
                        .iter()
                        .any(|o| o.previous_output == i.previous_output)
                })
            };
            txs.retain(|t| !spends(t));
            txs.push(tx.clone());
        }
        std::future::ready(Ok(()))
    }

    fn fee_rate(
        &self,
        _blocks: u16,
    ) -> impl std::future::Future<Output = Result<Option<FeeRate>, String>> + Send {
        std::future::ready(Ok(*self.estimate.lock().unwrap()))
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
        let provider = Provider::create(wallet, CONFIG).unwrap().pay_out(terms);
        let pool = Pool {
            net: net.clone(),
            view: net.view(A),
            txs: Arc::default(),
            estimate: Arc::new(Mutex::new(Some(ONE_SAT))),
            lag: Arc::default(),
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
        let provider = Provider::open(wallet, CONFIG).unwrap().pay_out(TERMS);
        *self.desk.provider.lock().unwrap() = Some(Arc::new(provider));
    }

    /// The provider, opened again with its payouts bounded by `limits`.
    fn limit(&self, limits: PayoutLimits) {
        let provider = self.desk.provider.lock().unwrap().take().unwrap();
        drop(Arc::into_inner(provider).expect("the provider is held once"));
        let wallet = Wallet::open(
            &self.dir.path().join("wallet.sqlite"),
            Link(self.net.clone()),
        )
        .unwrap();
        let provider = Provider::open(wallet, CONFIG)
            .unwrap()
            .pay_out(TERMS)
            .limit_payouts(limits);
        *self.desk.provider.lock().unwrap() = Some(Arc::new(provider));
    }

    /// The provider follows its payouts: none pending.
    async fn follow(&self) {
        let pending = self.provider().follow_payouts(&self.pool).await.unwrap();
        assert!(pending.is_empty(), "{pending:?}");
        self.provider().step().await.unwrap();
    }

    async fn pay_out(&self, amount: u64) -> Payout {
        let quote = self
            .client
            .payout_quote(PROVIDER, A, PAYEE, amount, MAX_FEE, ONE_SAT)
            .await
            .unwrap();
        let payout = self.client.payout(&quote).await.unwrap();
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
    // By a claim leaf taking a preimage of 32 bytes only.
    let claim = mined.iter().find(|tx| is_claim(tx)).unwrap();
    let leaf = claim.input[0].witness.taproot_leaf_script().unwrap();
    assert_eq!(leaf.script.as_bytes()[..4], [0x82, 0x01, 0x20, 0x88]);
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

fn rate(vb: u32) -> FeeRate {
    FeeRate::from_sat_per_vb_u32(vb)
}

/// What `tx` pays, all its outputs.
fn pays(tx: &Transaction) -> u64 {
    tx.output.iter().map(|o| o.value.to_sat()).sum()
}

#[tokio::test]
async fn a_payouts_coin_pays_the_chains_estimate_or_else_the_operators_rate() {
    let eco = Eco::new(STOCK, TERMS).await;
    eco.pool.estimate(Some(rate(3)));
    eco.pay_out(1_000).await;
    let coin = eco.pool.waiting().remove(0);
    let (fee, vsize) = (STOCK - pays(&coin), coin.vsize() as u64);
    assert!(fee >= 3 * vsize && fee < 4 * vsize, "{fee} for {vsize}");
    // No estimate, on regtest: the operator's.
    let eco = Eco::new(STOCK, TERMS).await;
    eco.pool.estimate(None);
    eco.pay_out(1_000).await;
    let coin = eco.pool.waiting().remove(0);
    let (fee, vsize) = (STOCK - pays(&coin), coin.vsize() as u64);
    assert!(fee >= vsize && fee < 2 * vsize, "{fee} for {vsize}");
}

#[tokio::test]
async fn a_coin_left_out_of_the_blocks_is_canceled_before_it_times_out() {
    let eco = Eco::new(STOCK, TERMS).await;
    let payout = eco.pay_out(1_000).await;
    let coin = eco.pool.waiting().remove(0);
    let timeout = timeout(&eco);
    // Broadcast as it is until 12 blocks before its timeout.
    eco.mine_to(timeout - 13);
    eco.follow().await;
    assert_eq!(eco.pool.waiting(), std::slice::from_ref(&coin));
    // Then its coins paid back to the provider in its place, paying more.
    eco.mine_to(timeout - 12);
    eco.follow().await;
    let cancel = eco.pool.waiting().remove(0);
    assert_eq!(eco.pool.waiting().len(), 1);
    let spent = |tx: &Transaction| -> Vec<OutPoint> {
        let mut spent: Vec<OutPoint> = tx.input.iter().map(|i| i.previous_output).collect();
        spent.sort();
        spent
    };
    assert_eq!(spent(&cancel), spent(&coin));
    assert_eq!(cancel.output.len(), 1);
    let fee = STOCK - pays(&cancel);
    assert!(fee >= STOCK - pays(&coin) + cancel.vsize() as u64, "{fee}");
    // Not again within the block, nor while its rate stands.
    eco.follow().await;
    eco.mine_to(timeout - 11);
    eco.follow().await;
    assert_eq!(eco.pool.waiting(), std::slice::from_ref(&cancel));
    // Made again as the blocks left run out, at a rising rate: four times
    // the estimate 8 blocks before.
    eco.mine_to(timeout - 8);
    eco.follow().await;
    let higher = eco.pool.waiting().remove(0);
    let higher_fee = STOCK - pays(&higher);
    assert!(higher_fee >= 4 * higher.vsize() as u64, "{higher_fee}");
    assert!(higher_fee > fee, "{higher_fee}");
    assert_eq!(eco.stage(), Stage::Accepted);
    // Mined: the payout failed, the provider's coins back less the fee.
    eco.pool.mine();
    eco.follow().await;
    let record = eco.provider().records(1).unwrap().remove(0);
    assert_eq!(record.stage, Stage::Failed);
    assert!(
        record.reason.as_deref().unwrap_or("").contains("canceled"),
        "{record:?}"
    );
    assert_eq!(eco.provider_coins(), STOCK - higher_fee);
    assert!(eco.pool.waiting().is_empty());
    // The client's lock is the client's again once expired.
    assert!(matches!(
        eco.client_follows(&payout).await.state,
        PayoutState::Failed(_)
    ));
}

/// A cancel the provider's wallet found in a block a chain source behind it
/// does not reach yet: not counted buried, so the payout waits, and fails
/// once the source catches up.
#[tokio::test]
async fn a_cancel_beyond_a_lagging_chains_tip_is_not_counted_buried() {
    let eco = Eco::new(STOCK, TERMS).await;
    eco.pay_out(1_000).await;
    eco.pool.txs.lock().unwrap().clear();
    eco.mine_to(timeout(&eco) - 12);
    eco.follow().await;
    assert_eq!(eco.pool.waiting().len(), 1);
    eco.pool.mine();
    let wallet = eco.provider();
    let wallet = wallet.wallet();
    while {
        let scanned = wallet.scan_silent(None, &eco.pool, 100).await.unwrap();
        scanned.next <= scanned.height
    } {}
    *eco.pool.lag.lock().unwrap() = 2;
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Accepted);
    *eco.pool.lag.lock().unwrap() = 0;
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Failed);
}

#[tokio::test]
async fn a_coin_mined_after_all_while_canceled_is_followed_as_ever() {
    let eco = Eco::new(STOCK, TERMS).await;
    let payout = eco.pay_out(1_000).await;
    let coin = eco.pool.waiting().remove(0);
    eco.mine_to(timeout(&eco) - 12);
    eco.follow().await;
    assert_ne!(eco.pool.waiting(), std::slice::from_ref(&coin));
    eco.pool.txs.lock().unwrap().clear();
    eco.net.confirm(A, vec![coin]);
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Locked);
    // Its cancel given up, broadcast no more.
    assert!(eco.pool.waiting().is_empty(), "{:?}", eco.pool.waiting());
    // Too late for its client to claim, it is taken back once timed out.
    let state = eco.client_follows(&payout).await.state;
    assert!(matches!(state, PayoutState::Failed(_)), "{state:?}");
    eco.mine_to(timeout(&eco));
    eco.follow().await;
    assert_eq!(eco.pool.waiting().len(), 1);
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Refunded);
}

#[tokio::test]
async fn a_refund_left_out_of_the_blocks_is_made_again_at_a_rising_rate() {
    let eco = Eco::new(STOCK, TERMS).await;
    eco.pay_out(1_000).await;
    let coin = eco.pool.mine().remove(0);
    eco.follow().await;
    let timeout = timeout(&eco);
    eco.mine_to(timeout);
    eco.follow().await;
    let refund = eco.pool.waiting().remove(0);
    let value = coin.output[refund.input[0].previous_output.vout as usize]
        .value
        .to_sat();
    let fee = value - pays(&refund);
    assert!(fee >= refund.vsize() as u64 && fee < 2 * refund.vsize() as u64);
    // Not again while the rate stands.
    eco.follow().await;
    assert_eq!(eco.pool.waiting(), std::slice::from_ref(&refund));
    // The chain's estimate risen: made again at it.
    eco.pool.estimate(Some(rate(2)));
    eco.follow().await;
    let again = eco.pool.waiting().remove(0);
    assert_eq!(again.lock_time, refund.lock_time);
    let again_fee = value - pays(&again);
    assert!(again_fee >= 2 * again.vsize() as u64, "{again_fee}");
    assert!(again_fee >= fee + again.vsize() as u64, "{again_fee}");
    // Close to the client's lock's expiry, past which the client's claim
    // would pay the provider nothing: all of it but dust, if the rate
    // takes more.
    let lock = eco.client.locks().unwrap().remove(0);
    eco.mine_to(lock.expiry - licash_wallet::CLAIM_MARGIN - 2);
    eco.follow().await;
    let last = eco.pool.waiting().remove(0);
    let dust = last.output[0].script_pubkey.minimal_non_dust().to_sat();
    assert_eq!(pays(&last), dust);
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Refunded);
}

/// A payout's coin looked through to three blocks past its own, which
/// Bitcoin then replaces with others, its client's claim in the first:
/// the provider takes the claim and is paid. `legacy`: the file holds no
/// hash of the block looked through to, as a provider of a version before
/// left it.
async fn claimed_in_blocks_replacing_those_looked_through(legacy: bool) {
    let eco = Eco::new(STOCK, TERMS).await;
    let payout = eco.pay_out(1_000).await;
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Locked);
    let coin_at = eco.pool.view.height();
    assert_eq!(
        eco.client_follows(&payout).await.state,
        PayoutState::Claimed
    );
    let claim = eco.pool.waiting();
    eco.pool.mine_if(|_| false);
    eco.mine_to(coin_at + 3);
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Locked);
    if legacy {
        rusqlite::Connection::open(eco.dir.path().join("wallet.sqlite"))
            .unwrap()
            .execute("UPDATE provider_payout_scans SET hash = NULL", [])
            .unwrap();
    }
    eco.net.reorg(A, coin_at);
    for tx in &claim {
        eco.pool.broadcast(tx).await.unwrap();
    }
    assert!(eco.pool.mine().iter().any(is_claim));
    eco.mine_to(coin_at + 4);
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Done);
    assert!(eco.provider_held() > 1_000);
}

#[tokio::test]
async fn a_claim_mined_in_blocks_replacing_those_looked_through_pays_the_provider() {
    claimed_in_blocks_replacing_those_looked_through(false).await;
}

#[tokio::test]
async fn a_claim_mined_in_blocks_replacing_those_a_provider_before_looked_through_pays_it() {
    claimed_in_blocks_replacing_those_looked_through(true).await;
}

/// The payout under way made again as a provider before payout coins were
/// sized left it: its coin unsized, the only one paid, its funding recorded
/// if `recorded`, else not, as a provider stopped between paying the coin
/// and recording it left it; then the provider opened again, upgraded. The
/// coin's terms, and its transaction, broadcast.
async fn made_unsized(eco: &Eco, recorded: bool) -> (Htlc, Transaction) {
    use bitcoin::{
        consensus::{deserialize, serialize},
        secp256k1::{Secp256k1, SecretKey, XOnlyPublicKey},
    };
    use rusqlite::params;

    let db = rusqlite::Connection::open(eco.dir.path().join("wallet.sqlite")).unwrap();
    type Row = (Vec<u8>, [u8; 32], u32, [u8; 32], [u8; 32], Vec<u8>, u32);
    let (id, hash, timeout, claim, refund, sized, vout): Row = db
        .query_row(
            "SELECT s.id, s.hash, s.expiry_height, p.claim_key, p.refund_key, f.tx, f.vout
             FROM provider_swaps s JOIN provider_payouts p ON p.id = s.id
             JOIN provider_payout_fundings f ON f.id = s.id",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap();
    let sized: Transaction = deserialize(&sized).unwrap();
    let refund = SecretKey::from_slice(&refund).unwrap();
    let legacy = Htlc {
        hash,
        claim: XOnlyPublicKey::from_slice(&claim).unwrap(),
        refund: refund.x_only_public_key(&Secp256k1::new()).0,
        timeout,
        wait: None,
        sized: false,
    };
    let output = &sized.output[vout as usize];
    let script = legacy.script_pubkey();
    assert_eq!(
        output.script_pubkey,
        Htlc {
            sized: true,
            ..legacy
        }
        .script_pubkey()
    );
    // The sized coin never paid: its coins the provider's again.
    eco.pool.txs.lock().unwrap().clear();
    let txid = sized.compute_txid().to_byte_array();
    db.execute(
        "UPDATE silent_coins SET spent_by = NULL WHERE spent_by = ?1",
        [txid],
    )
    .unwrap();
    db.execute("DELETE FROM silent_spends WHERE txid = ?1", [txid])
        .unwrap();
    eco.restart();
    let paid = eco
        .provider()
        .wallet()
        .spend_silent(
            Payee::Script(script.clone()),
            Some(output.value.to_sat()),
            ONE_SAT,
        )
        .unwrap();
    let at = paid.output.iter().position(|o| o.script_pubkey == script);
    db.execute(
        "UPDATE provider_swaps SET address = ?2 WHERE id = ?1",
        params![id, script.as_bytes()],
    )
    .unwrap();
    match recorded {
        true => db.execute(
            "UPDATE provider_payout_fundings SET tx = ?2, vout = ?3 WHERE id = ?1",
            params![id, serialize(&paid), at.unwrap() as u32],
        ),
        false => db.execute("DELETE FROM provider_payout_fundings WHERE id = ?1", [&id]),
    }
    .unwrap();
    drop(db);
    eco.restart();
    eco.pool.broadcast(&paid).await.unwrap();
    (legacy, paid)
}

/// A coin made unsized before the provider was upgraded, claimed by its
/// client after: the provider reads the preimage off the claim, by the
/// unsized claim leaf, and is paid its lock.
#[tokio::test]
async fn a_coin_made_unsized_before_an_upgrade_is_followed_to_its_claim() {
    let eco = Eco::new(STOCK, TERMS).await;
    let payout = eco.pay_out(1_000).await;
    let (legacy, coin) = made_unsized(&eco, true).await;
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Locked);
    let claimed = eco.client_follows(&payout).await;
    assert_eq!(claimed.state, PayoutState::Claimed);
    assert_eq!(claimed.funding.unwrap().txid, coin.compute_txid());
    let mined = eco.pool.mine();
    let claim = mined.iter().find(|tx| is_claim(tx)).expect("its claim");
    let leaf = claim.input[0].witness.taproot_leaf_script().unwrap();
    assert_eq!(leaf.script, legacy.claim_script().as_script());
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Done);
    assert!(eco.provider_held() > 1_000);
    assert_eq!(eco.client_follows(&payout).await.state, PayoutState::Done);
}

/// A payout whose recorded coin pays neither its terms nor them unsized, as
/// payouts made before a coin's keys were aggregated: said pending each
/// follow, never failed nor taken back on a guess, and followed again once
/// its coin is as recorded.
#[tokio::test]
async fn a_coin_of_other_terms_than_its_own_is_left_as_it_stands() {
    let eco = Eco::new(STOCK, TERMS).await;
    eco.pay_out(1_000).await;
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Locked);
    let db = rusqlite::Connection::open(eco.dir.path().join("wallet.sqlite")).unwrap();
    let address: Vec<u8> = db
        .query_row("SELECT address FROM provider_swaps", [], |r| r.get(0))
        .unwrap();
    let other = bitcoin::ScriptBuf::new_p2tr_tweaked(
        bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(
            bitcoin::XOnlyPublicKey::from_slice(&[0x02; 32]).unwrap(),
        ),
    );
    db.execute("UPDATE provider_swaps SET address = ?1", [other.as_bytes()])
        .unwrap();
    eco.mine_to(timeout(&eco));
    for _ in 0..2 {
        let pending = eco.provider().follow_payouts(&eco.pool).await.unwrap();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert!(pending[0].1.contains("other terms"), "{pending:?}");
        assert_eq!(eco.stage(), Stage::Locked);
        assert!(eco.pool.waiting().is_empty());
        eco.net.mine(A, 1, None);
    }
    db.execute("UPDATE provider_swaps SET address = ?1", [address])
        .unwrap();
    drop(db);
    eco.follow().await;
    assert_eq!(eco.pool.waiting().len(), 1, "its refund");
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Refunded);
}

/// A coin made unsized before the provider was upgraded, its funding not
/// recorded, never claimed: the provider finds it among its own spends and
/// takes it back once timed out, by the unsized coin's refund leaf.
#[tokio::test]
async fn a_coin_made_unsized_before_an_upgrade_is_taken_back_once_timed_out() {
    use bitcoin::{secp256k1::Secp256k1, taproot::ControlBlock, XOnlyPublicKey};

    let eco = Eco::new(STOCK, TERMS).await;
    eco.pay_out(1_000).await;
    let (legacy, coin) = made_unsized(&eco, false).await;
    eco.follow().await;
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Locked);
    eco.mine_to(timeout(&eco));
    eco.follow().await;
    let refund = eco.pool.waiting();
    assert_eq!(refund.len(), 1);
    let input = &refund[0].input[0];
    assert_eq!(input.previous_output.txid, coin.compute_txid());
    let leaf = input.witness.taproot_leaf_script().unwrap();
    assert_eq!(leaf.script, legacy.refund_script().as_script());
    let witness: Vec<&[u8]> = input.witness.iter().collect();
    let control = ControlBlock::decode(witness[witness.len() - 1]).unwrap();
    let output = legacy.script_pubkey();
    let key = XOnlyPublicKey::from_slice(&output.as_bytes()[2..]).unwrap();
    let secp = Secp256k1::verification_only();
    assert!(control.verify_taproot_commitment(&secp, key, leaf.script));
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Refunded);
}

/// The operator's alerts of refunds that left the chain.
fn refunds_undone(eco: &Eco) -> usize {
    let events = eco.provider().events(0, 50).unwrap();
    events
        .iter()
        .filter(|e| matches!(e.notice(), Some(licash_swaps::Notice::Alert { message }) if message.contains("left the chain")))
        .count()
}

/// A payout's coin, never claimed but by `claim` if given, refunded once
/// timed out: its refund, mined at the tip.
async fn refunded(eco: &Eco, claim: bool) -> Vec<Transaction> {
    let payout = eco.pay_out(1_000).await;
    eco.pool.mine();
    eco.follow().await;
    let mut held = Vec::new();
    if claim {
        eco.client_follows(&payout).await;
        held = eco.pool.waiting();
        assert!(held.iter().any(is_claim));
        eco.pool.mine_if(|_| false);
    }
    eco.mine_to(timeout(eco));
    eco.follow().await;
    let refund = eco.pool.waiting();
    assert_eq!(refund.len(), 1);
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Refunded);
    if claim {
        held
    } else {
        refund
    }
}

#[tokio::test]
async fn a_refund_replaced_out_of_the_chain_is_made_again() {
    let eco = Eco::new(STOCK, TERMS).await;
    let refund = refunded(&eco, false).await;
    let at = eco.pool.view.height();
    // Mined again in the block replacing its own: refunded still.
    eco.net.reorg(A, at - 1);
    for tx in &refund {
        eco.pool.broadcast(tx).await.unwrap();
    }
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Refunded);
    assert_eq!(refunds_undone(&eco), 0);
    // Left out of those replacing its own: taken on again, and refunded.
    eco.net.reorg(A, at - 1);
    eco.mine_to(at + 1);
    eco.follow().await;
    assert_eq!(refunds_undone(&eco), 1);
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Locked);
    assert_eq!(eco.pool.waiting().len(), 1);
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Refunded);
    // Buried, never looked at again.
    eco.mine_to(eco.pool.view.height() + 6);
    eco.follow().await;
    eco.net.reorg(A, at);
    eco.mine_to(at + 10);
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Refunded);
    assert_eq!(refunds_undone(&eco), 1);
}

#[tokio::test]
async fn a_claim_mined_in_place_of_a_refund_still_pays_the_provider() {
    let eco = Eco::new(STOCK, TERMS).await;
    let claim = refunded(&eco, true).await;
    let at = eco.pool.view.height();
    eco.net.reorg(A, at - 1);
    for tx in &claim {
        eco.pool.broadcast(tx).await.unwrap();
    }
    assert!(eco.pool.mine().iter().any(is_claim));
    eco.follow().await;
    assert_eq!(refunds_undone(&eco), 1);
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
    let quote = eco
        .client
        .payout_quote(PROVIDER, A, PAYEE, 1_000, MAX_FEE, ONE_SAT)
        .await
        .unwrap();
    let declined = eco.client.payout(&quote).await.unwrap();
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

/// Bounds as wide as the terms: each test narrows one.
const UNBOUNDED: PayoutLimits = PayoutLimits {
    no_shows: u32::MAX,
    open: u64::MAX,
    max_blocks: u32::MAX,
    fee_share: 0,
};

/// A payout whose client never claims costs the provider its coin's fees
/// and the refund's, at no cost to the client: the provider bounds how
/// many are under way, and declines a hub's once too many of its clients
/// never claimed lately.
#[tokio::test]
async fn payouts_never_claimed_are_bounded_at_once_and_by_their_hub() {
    let eco = Eco::new(STOCK, TERMS).await;
    eco.limit(PayoutLimits {
        no_shows: 1,
        open: 1,
        ..UNBOUNDED
    });
    eco.pay_out(1_000).await;
    // One under way: no more paid out, said before a lock is made.
    assert!(eco.provider().info().unwrap().chain.is_none());
    let quote = eco
        .client
        .payout_quote(PROVIDER, A, PAYEE, 500, MAX_FEE, ONE_SAT)
        .await;
    assert!(
        matches!(&quote, Err(Error::Invalid(why)) if why.contains("pays nothing out")),
        "{quote:?}"
    );
    // Never claimed: taken back once timed out.
    eco.pool.mine();
    eco.follow().await;
    eco.mine_to(timeout(&eco));
    eco.follow().await;
    eco.pool.mine();
    eco.follow().await;
    assert_eq!(eco.stage(), Stage::Refunded);
    let lock = eco.client.locks().unwrap().remove(0);
    eco.mine_to(lock.expiry + 1);
    eco.client.resume().await.unwrap();
    // Its hub's clients never claimed once lately: no more from there.
    assert!(eco.provider().info().unwrap().chain.is_some());
    let quote = eco
        .client
        .payout_quote(PROVIDER, A, PAYEE, 500, MAX_FEE, ONE_SAT)
        .await
        .unwrap();
    let declined = eco.client.payout(&quote).await;
    let why = format!("{declined:?}");
    assert!(why.contains("went unclaimed lately"), "{why}");
    assert!(eco.pool.waiting().is_empty());
}

/// A payout's coin is held no longer than the provider bounds, and worth
/// some times what it and its refund pay in fees at least, as the rate
/// it is paid at rises.
#[tokio::test]
async fn a_payout_is_declined_held_too_long_or_worth_too_little_for_its_fees() {
    let eco = Eco::new(STOCK, TERMS).await;
    eco.limit(PayoutLimits {
        max_blocks: 30,
        ..UNBOUNDED
    });
    let quote = eco
        .client
        .payout_quote(PROVIDER, A, PAYEE, 1_000, MAX_FEE, ONE_SAT)
        .await
        .unwrap();
    let declined = format!("{:?}", eco.client.payout(&quote).await);
    assert!(declined.contains("more than 30 blocks"), "{declined}");

    let eco = Eco::new(STOCK, TERMS).await;
    eco.limit(PayoutLimits {
        fee_share: 5,
        ..UNBOUNDED
    });
    // At 1 sat/vB: 320 sat of fees, 1,600 at least.
    let least = |eco: &Eco| eco.provider().info().unwrap().chain.unwrap().min;
    assert_eq!(least(&eco), 5 * NO_SHOW_VSIZE);
    let quote = eco
        .client
        .payout_quote(PROVIDER, A, PAYEE, 1_000, MAX_FEE, ONE_SAT)
        .await;
    assert!(
        matches!(&quote, Err(Error::Invalid(why)) if why.contains("coins of 1600")),
        "{quote:?}"
    );
    // The rate risen since its quote: declined once its lock is made, and
    // told of to the next.
    eco.pool.estimate(Some(rate(2)));
    let quote = eco
        .client
        .payout_quote(PROVIDER, A, PAYEE, 2_000, MAX_FEE, ONE_SAT)
        .await
        .unwrap();
    let declined = format!("{:?}", eco.client.payout(&quote).await);
    assert!(declined.contains("coins of 3200"), "{declined}");
    assert_eq!(least(&eco), 10 * NO_SHOW_VSIZE);
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

/// A payout to a silent payment address through the provider: asked its
/// share of the payee's scan key blinded, it answers one proven of its
/// refund key, the same each time asked; the client's claim pays the
/// output its payee finds, and the provider claims its lock as any.
#[tokio::test]
async fn a_payout_to_a_silent_payment_address_pays_what_its_payee_finds() {
    use bitcoin::{secp256k1::SecretKey, NetworkKind};
    use licash_onchain::silent::{self, Keys};
    let eco = Eco::new(STOCK, TERMS).await;
    let secret = |b: u8| SecretKey::from_slice(&[b; 32]).unwrap();
    let payee = Keys::new(secret(0x71), secret(0x72), NetworkKind::Test);
    let to = payee.address().to_string();
    let quote = eco
        .client
        .payout_quote(PROVIDER, A, &to, 1_000, MAX_FEE, ONE_SAT)
        .await
        .unwrap();
    let payout = eco.client.payout(&quote).await.unwrap();
    assert_eq!(eco.client_follows(&payout).await.state, PayoutState::Open);
    let coin_tx = eco.pool.waiting().remove(0);
    eco.pool.mine();
    eco.follow().await;
    let claimed = eco.client_follows(&payout).await;
    assert_eq!(claimed.state, PayoutState::Claimed);
    let mined = eco.pool.mine();
    let claim = mined
        .iter()
        .find(|tx| is_claim(tx))
        .expect("the claim mined");
    assert_eq!(claim.output.len(), 1);
    assert_eq!(claim.output[0].value.to_sat(), 1_000);
    let coin = claimed.funding.unwrap();
    assert_eq!(coin.txid, coin_tx.compute_txid());
    let funding = coin_tx.output[coin.vout as usize].script_pubkey.clone();
    let tweak = silent::tweak(claim, &[funding.as_script()]).expect("a claim counted");
    assert_eq!(
        payee
            .scanner([])
            .scan(&tweak, &silent::taproot_outputs(claim))
            .len(),
        1
    );
    eco.follow().await;
    assert_eq!(eco.provider().records(1).unwrap()[0].stage, Stage::Done);
    assert_eq!(eco.client_follows(&payout).await.state, PayoutState::Done);
}
