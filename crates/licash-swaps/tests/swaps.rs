//! Swaps between two hubs in process, through a provider whose clients
//! reach it as they reach the hubs.

#[allow(dead_code)]
#[path = "../../licash-wallet/tests/net/mod.rs"]
mod net;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use licash::spend::{lock_hash, ChainProfile, ProposalId};
use licash_api::{
    swaps::{self, Request},
    Code, Fee, Hex, Problem,
};
use licash_swaps::{Config, Notice, Provider, Stage};
use licash_wallet::{Error, Response, SwapLimits, SwapState, Transport, Wallet};
use net::{Fault, Link, Net};

const A: &str = "http://a.test";
const B: &str = "http://b.test";
const PROVIDER: &str = "http://swaps.test";
const FEE: Fee = Fee { base: 1, ppm: 0 };
const CONFIG: Config = Config {
    fee: Fee {
        base: 1,
        ppm: 10_000,
    },
    min: 5,
    max: 150,
    margin: 6,
    confirm: 4,
};
/// Hub A's response window, longer than B's ([`net::TIMING`]): a provider
/// paying at B needs the client's lock at A to outlive its own by B's
/// window and more.
const WINDOW_A: u32 = 46;
/// Blocks the client's lock outlives the provider's at B at least: the
/// refund request's confirmation, B's window, reading the preimage, the
/// provider's margin and one.
const LEAD_B: u32 = 4 + 20 + 1 + 6 + 1;
/// And at A.
const LEAD_A: u32 = 4 + WINDOW_A + 1 + 6 + 1;

/// The provider, as its clients reach it.
#[derive(Default)]
struct Desk {
    provider: Mutex<Option<Arc<Provider<Link>>>>,
    /// The next request to each path fails so.
    faults: Mutex<Vec<(&'static str, Fault)>>,
    /// Every request fails: the provider is gone.
    gone: AtomicBool,
    /// Answered to the next swap request instead of the provider's answer.
    lie: Mutex<Option<swaps::Swap>>,
}

impl Desk {
    fn provider(&self) -> Arc<Provider<Link>> {
        self.provider.lock().unwrap().clone().unwrap()
    }

    fn fail(&self, path: &'static str, fault: Fault) {
        self.faults.lock().unwrap().push((path, fault));
    }

    async fn serve(&self, path: &str, body: Option<Vec<u8>>) -> Result<Response, String> {
        let fault = {
            let mut faults = self.faults.lock().unwrap();
            let found = faults.iter().position(|(p, _)| *p == path);
            found.map(|i| faults.remove(i).1)
        };
        if self.gone.load(Ordering::Relaxed) || matches!(fault, Some(Fault::Lost)) {
            return Err("connection refused".into());
        }
        let lie = body.as_ref().and(self.lie.lock().unwrap().take());
        let provider = self.provider();
        let answer = match (path, body) {
            (swaps::SWAPS_INFO, None) => Ok(serde_json::to_vec(&provider.info().unwrap()).unwrap()),
            (swaps::SWAPS, Some(body)) => {
                let request: Request = serde_json::from_slice(&body).unwrap();
                match lie {
                    Some(lie) => Ok(serde_json::to_vec(&lie).unwrap()),
                    None => provider
                        .take(&request)
                        .await
                        .map(|swap| serde_json::to_vec(&swap).unwrap()),
                }
            }
            (path, None) => {
                let id = licash_api::lock_id(&path[swaps::SWAPS.len() + 1..]).unwrap();
                match provider.swap(&id).unwrap() {
                    Some(swap) => Ok(serde_json::to_vec(&swap).unwrap()),
                    None => Err(Problem::new(Code::UnknownLock, "no such swap")),
                }
            }
            _ => Err(Problem::invalid("no such route")),
        };
        if let Some(Fault::Dropped) = fault {
            return Err("connection reset".into());
        }
        Ok(match answer {
            Ok(body) => Response { status: 200, body },
            Err(problem) => Response {
                status: problem.code.status(),
                body: serde_json::to_vec(&problem).unwrap(),
            },
        })
    }
}

/// A client's way to the hubs and the provider.
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

struct Eco {
    net: Arc<Net>,
    desk: Arc<Desk>,
    dir: tempfile::TempDir,
    client: Wallet<Client>,
    config: Config,
}

impl Eco {
    /// Hubs A and B at height 3, A's window the longer, a provider holding
    /// `stock` at B, and a client holding 100 at A.
    async fn new(stock: u64) -> Self {
        Self::with(stock, WINDOW_A, CONFIG).await
    }

    /// As [`Eco::new`], A's window `window_a`, the provider's terms
    /// `config`.
    async fn with(stock: u64, window_a: u32, config: Config) -> Self {
        let net = Net::new();
        let timing = licash_hub::Timing {
            window: window_a,
            ..net::TIMING
        };
        net.start_timed(A, FEE, timing);
        net.start(B, FEE);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider.sqlite");
        let wallet = Wallet::create(
            &path,
            &[0x50; 32],
            ChainProfile::Core30DefaultRegtest,
            Link(net.clone()),
        )
        .unwrap();
        for hub in [A, B] {
            wallet.add_hub(hub).await.unwrap();
        }
        wallet
            .receive(&net.pay(B, &wallet.address(), stock))
            .unwrap();
        let provider = Provider::create(&dir.path().join("swaps.sqlite"), wallet, config).unwrap();
        let desk = Arc::new(Desk::default());
        *desk.provider.lock().unwrap() = Some(Arc::new(provider));
        let client = Client(net.clone(), desk.clone());
        let client = Wallet::create(
            &dir.path().join("client.sqlite"),
            &[0xc1; 32],
            ChainProfile::Core30DefaultRegtest,
            client,
        )
        .unwrap();
        for hub in [A, B] {
            client.add_hub(hub).await.unwrap();
        }
        client.receive(&net.pay(A, &client.address(), 100)).unwrap();
        Self {
            net,
            desk,
            dir,
            client,
            config,
        }
    }

    fn provider(&self) -> Arc<Provider<Link>> {
        self.desk.provider()
    }

    /// Stop the provider and start it again on its files.
    fn restart(&self) {
        let old = self.desk.provider.lock().unwrap().take().unwrap();
        assert_eq!(Arc::strong_count(&old), 1, "the provider is in use");
        drop(old);
        let path = self.dir.path().join("provider.sqlite");
        let wallet = Wallet::open(&path, Link(self.net.clone())).unwrap();
        let provider =
            Provider::open(&self.dir.path().join("swaps.sqlite"), wallet, self.config).unwrap();
        *self.desk.provider.lock().unwrap() = Some(Arc::new(provider));
    }

    /// Mine `blocks` on both hubs' chains, as if they followed one.
    fn mine(&self, blocks: u32) {
        self.net.mine(A, blocks, None);
        self.net.mine(B, blocks, None);
    }

    fn kinds(&self) -> Vec<String> {
        let events = self.provider().events(0, 100).unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }
}

/// What `wallet` holds available at the hub at `url`.
fn held<T: Transport>(wallet: &Wallet<T>, url: &str) -> u64 {
    let balance = wallet.balance().unwrap();
    balance
        .iter()
        .find(|b| b.hub == url)
        .map_or(0, |b| b.available)
}

/// The swap's own fee, and the hubs' fee for a lock.
fn costs(amount: u64) -> (u64, u64) {
    (CONFIG.fee.due(amount), FEE.base)
}

impl Eco {
    fn address(&self, url: &str) -> licash::address::Address {
        self.net.hub(url).info().address
    }

    /// A swap of `amount` from A to B whose request never reached the
    /// provider: the client's lock is made.
    async fn pending(&self, amount: u64) -> licash_wallet::Swap {
        self.desk.fail(swaps::SWAPS, Fault::Lost);
        let e = self
            .client
            .swap(PROVIDER, A, B, amount, SwapLimits::usual(amount))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Pending { .. }), "{e}");
        let swap = self.client.swaps().unwrap().remove(0);
        assert_eq!(
            (&swap.state, swap.theirs.as_ref()),
            (&SwapState::Open, None)
        );
        swap
    }

    /// The provider answers `swap` locked by `lock`, whatever it is.
    async fn lie(&self, swap: &licash_wallet::Swap, lock: &str) -> licash_wallet::Swap {
        *self.desk.lie.lock().unwrap() = Some(swaps::Swap {
            id: Hex(hex::decode(swap.lock.as_ref().unwrap()).unwrap()),
            state: swaps::State::Locked,
            amount: swap.amount,
            lock: Some(Hex(hex::decode(lock).unwrap())),
            expiry: 14,
            reason: None,
        });
        self.client.follow_swap(&swap.hash).await.unwrap()
    }
}

fn lock_id(text: &str) -> ProposalId {
    licash_api::lock_id(text).unwrap()
}

fn hash_of(swap: &licash_wallet::Swap) -> [u8; 32] {
    let mut hash = [0; 32];
    hex::decode_to_slice(&swap.hash, &mut hash).unwrap();
    hash
}

#[tokio::test]
async fn a_swap_moves_notes_from_one_hub_to_another_for_the_providers_fee() {
    let eco = Eco::new(200).await;
    let (fee, lock_fee) = costs(20);
    let provider = eco.provider();
    let info = provider.info().unwrap();
    // What it holds at B less the lock fee on it, up to its most; nothing
    // at A.
    assert_eq!(info.hub(&eco.address(B)).unwrap().max, 150);
    assert_eq!(info.hub(&eco.address(A)).unwrap().max, 0);
    assert_eq!(info.address, provider.wallet().address());
    // By how much the client's lock outlives the provider's at each hub,
    // and at every hub, for a client that reads only that.
    let margin = |url| info.hub(&eco.address(url)).unwrap().margin;
    assert_eq!((margin(B), margin(A)), (Some(LEAD_B), Some(LEAD_A)));
    assert_eq!(info.margin, LEAD_A);

    let swap = eco
        .client
        .swap(PROVIDER, A, B, 20, SwapLimits::usual(20))
        .await
        .unwrap();
    assert_eq!(swap.state, SwapState::Done);
    assert_eq!((swap.amount, swap.fee, swap.from.as_str()), (20, fee, A));
    assert!(swap.lock.is_some() && swap.theirs.is_some());
    assert_eq!(held(&eco.client, B), 20);
    assert_eq!(provider.records(10).unwrap()[0].stage, Stage::Locked);

    // The provider follows its lock to the preimage, and claims with it.
    assert!(provider.step().await.unwrap().is_empty());
    let record = &provider.records(10).unwrap()[0];
    assert_eq!(
        (record.stage, record.amount, record.fee, record.paid),
        (Stage::Done, 20, fee, 20 + fee)
    );
    assert_eq!(held(provider.wallet(), A), 20 + fee);
    assert_eq!(held(provider.wallet(), B), 200 - 20 - lock_fee);
    eco.client.resume().await.unwrap();
    assert_eq!(held(&eco.client, A), 100 - 20 - fee - lock_fee);

    assert_eq!(eco.kinds(), ["accepted", "done"]);
    let events = provider.events(0, 10).unwrap();
    assert!(matches!(
        events[1].notice(),
        Some(Notice::Done { amount: 20, fee: f, .. }) if f == fee
    ));
    let stats = provider.stats().unwrap();
    assert_eq!((stats.done, stats.earned, stats.open), (1, fee, 0));
    assert_eq!((stats.events, provider.last_event().unwrap()), (2, 2));
    assert_eq!(
        eco.client.follow_swap(&swap.hash).await.unwrap().state,
        SwapState::Done
    );
    let theirs = provider.swap(&lock_id(swap.lock.as_ref().unwrap()));
    assert_eq!(theirs.unwrap().unwrap().state, swaps::State::Done);
}

#[tokio::test]
async fn a_client_locks_nothing_for_a_swap_the_provider_would_decline() {
    let eco = Eco::new(60).await;
    // Below its least, beyond what it holds at B, the other way, at one hub.
    for (from, to, amount) in [(A, B, 4), (A, B, 60), (B, A, 10), (A, A, 10)] {
        let e = eco
            .client
            .swap(PROVIDER, from, to, amount, SwapLimits::usual(amount))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{e}");
    }
    assert!(matches!(
        eco.client
            .swap(PROVIDER, A, "http://c.test", 10, SwapLimits::usual(10))
            .await,
        Err(Error::UnknownHub(_))
    ));
    assert!(eco.client.swaps().unwrap().is_empty());
    assert_eq!(held(&eco.client, A), 100);
}

#[tokio::test]
async fn a_client_locks_nothing_for_a_swap_costing_more_than_it_allows() {
    let eco = Eco::new(60).await;
    let (fee, _) = costs(20);
    let cheaper = SwapLimits {
        fee: fee - 1,
        margin: LEAD_B,
    };
    let sooner = SwapLimits {
        fee,
        margin: LEAD_B - 1,
    };
    for (limits, says) in [(cheaper, "more than"), (sooner, "locked")] {
        let e = eco.client.swap(PROVIDER, A, B, 20, limits).await;
        match e {
            Err(Error::Invalid(why)) => assert!(why.contains(says), "{why}"),
            other => panic!("{other:?}"),
        }
    }
    assert!(eco.client.swaps().unwrap().is_empty());
    assert_eq!(held(&eco.client, A), 100);
    assert!(eco.provider().records(10).unwrap().is_empty());

    // At its limits exactly: swapped.
    let exact = SwapLimits {
        fee,
        margin: LEAD_B,
    };
    let swap = eco.client.swap(PROVIDER, A, B, 20, exact).await.unwrap();
    assert_eq!((swap.state, swap.fee), (SwapState::Done, fee));
}

#[tokio::test]
async fn a_client_asks_what_a_swap_costs_and_what_it_would_refuse() {
    let eco = Eco::new(60).await;
    let quote =
        |from, to, amount, limits| eco.client.swap_quote(PROVIDER, from, to, amount, limits);
    let (fee, lock_fee) = costs(20);
    let quoted = quote(A, B, 20, SwapLimits::usual(20)).await.unwrap();
    assert_eq!((quoted.amount, quoted.fee), (20, fee));
    // A's fee on the lock that swap makes, of what the client holds now.
    assert_eq!(eco.client.lock_fee(A, 20 + fee).unwrap(), lock_fee);
    // Its margin at B, the hub swapped to.
    assert_eq!(
        (quoted.info.min, quoted.margin, quoted.info.margin),
        (CONFIG.min, LEAD_B, LEAD_A)
    );
    assert!(quoted.info.hub(&eco.address(B)).is_some());
    // Below its least: its least.
    let least = quote(A, B, 1, SwapLimits::usual(1)).await.unwrap();
    assert_eq!((least.amount, least.fee), (CONFIG.min, costs(CONFIG.min).0));

    // What a swap refuses: beyond what it holds at B, the other way, at
    // one hub, nothing, and more than allowed.
    let cheaper = SwapLimits {
        fee: fee - 1,
        margin: LEAD_B,
    };
    let sooner = SwapLimits {
        fee,
        margin: LEAD_B - 1,
    };
    let usual = SwapLimits::usual(10);
    for (from, to, amount, limits) in [
        (A, B, 60, usual),
        (B, A, 10, usual),
        (A, A, 10, usual),
        (A, B, 0, usual),
        (A, B, 20, cheaper),
        (A, B, 20, sooner),
    ] {
        let e = quote(from, to, amount, limits).await.unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{e}");
    }
    let unknown = quote(A, "http://c.test", 10, usual).await;
    assert!(matches!(unknown, Err(Error::UnknownHub(_))), "{unknown:?}");
    // Asking locked and recorded nothing.
    assert!(eco.client.swaps().unwrap().is_empty());
    assert_eq!(held(&eco.client, A), 100);
    assert!(eco.provider().records(10).unwrap().is_empty());
}

#[tokio::test]
async fn the_provider_declines_locks_that_do_not_pay_it_or_leave_it_no_room() {
    let eco = Eco::new(30).await;
    let provider = eco.provider();
    let (a, b, me) = (eco.address(A), eco.address(B), eco.client.address());
    for _ in 0..2 {
        eco.client.receive(&eco.net.pay(A, &me, 64)).unwrap();
    }
    let to = provider.wallet().address();
    let (fee, _) = costs(20);

    let short = eco.client.lock(A, &to, 20, lock_hash(&[1; 32]), Some(20));
    let short = lock_id(&short.await.unwrap().id);
    let declined = provider
        .take(&Request::new(&a, &short, &b, &me, 20))
        .await
        .unwrap_err();
    assert_eq!(declined.code, Code::Declined);
    assert!(declined.message.contains("pays 20"), "{declined:?}");
    for (from, to, payee, amount, code) in [
        (a, b, me, 4, Code::Declined),
        (a, b, me, 151, Code::Declined),
        (a, me, me, 20, Code::Declined),
        (a, a, me, 20, Code::Declined),
        (a, b, b, 20, Code::Invalid),
    ] {
        let request = Request::new(&from, &short, &to, &payee, amount);
        assert_eq!(provider.take(&request).await.unwrap_err().code, code);
    }

    // Expiring at 38: the provider's lock would expire at 6, within 6 blocks.
    let soon = eco
        .client
        .lock(A, &to, 20 + fee, lock_hash(&[2; 32]), Some(LEAD_B + 6));
    let soon = lock_id(&soon.await.unwrap().id);
    let request = Request::new(&a, &soon, &b, &me, 20);
    let declined = provider.take(&request).await.unwrap_err();
    assert!(declined.message.contains("too soon"), "{declined:?}");
    // Refunded once expired at depth.
    eco.mine(LEAD_B + 4);
    eco.client.resume().await.unwrap();
    let declined = provider.take(&request).await.unwrap_err();
    assert!(declined.message.contains("settled"), "{declined:?}");

    // More than it holds at B.
    let big = eco
        .client
        .lock(A, &to, 40 + costs(40).0, lock_hash(&[3; 32]), None);
    let big = lock_id(&big.await.unwrap().id);
    let declined = provider
        .take(&Request::new(&a, &big, &b, &me, 40))
        .await
        .unwrap_err();
    assert!(declined.message.contains("too little"), "{declined:?}");

    assert!(provider.records(10).unwrap().is_empty());
    assert!(eco.kinds().is_empty());
    assert_eq!(held(provider.wallet(), B), 30);
}

#[tokio::test]
async fn a_swap_asked_again_answers_where_it_stands_and_a_hash_serves_once() {
    let eco = Eco::new(200).await;
    let swap = eco
        .client
        .swap(PROVIDER, A, B, 20, SwapLimits::usual(20))
        .await
        .unwrap();
    let provider = eco.provider();
    let (a, b, me) = (eco.address(A), eco.address(B), eco.client.address());
    let id = lock_id(swap.lock.as_ref().unwrap());
    let request = Request::new(&a, &id, &b, &me, 20);
    let again = provider.take(&request).await.unwrap();
    assert_eq!((again.state, again.amount), (swaps::State::Locked, 20));
    assert_eq!(
        hex::encode(&again.lock.unwrap().0),
        swap.theirs.clone().unwrap()
    );
    let other = Request {
        amount: 21,
        ..request.clone()
    };
    let declined = provider.take(&other).await.unwrap_err();
    assert!(declined.message.contains("another swap"), "{declined:?}");

    // Another lock under the hash the client revealed.
    let to = provider.wallet().address();
    let reuse = eco.client.lock(A, &to, 30, hash_of(&swap), None);
    let reuse = lock_id(&reuse.await.unwrap().id);
    let declined = provider
        .take(&Request::new(&a, &reuse, &b, &me, 20))
        .await
        .unwrap_err();
    assert!(declined.message.contains("hash"), "{declined:?}");
    assert_eq!(provider.records(10).unwrap().len(), 1);
}

#[tokio::test]
async fn a_client_whose_provider_never_locks_is_refunded_once_its_lock_expired() {
    let eco = Eco::new(200).await;
    let swap = eco.pending(20).await;
    eco.desk.gone.store(true, Ordering::Relaxed);
    let resumed = eco.client.resume().await.unwrap();
    assert_eq!(resumed.pending.len(), 1, "{resumed:?}");
    // Its lock expires at 46, the latest hub A admitted at 3.
    eco.mine(44);
    let resumed = eco.client.resume().await.unwrap();
    assert!(
        matches!(&resumed.swaps[0].state, SwapState::Failed(why) if why.contains("never locked")),
        "{resumed:?}"
    );
    assert_eq!(resumed.swaps[0].hash, swap.hash);
    assert_eq!(held(&eco.client, A), 100 - FEE.base);
    assert!(eco.provider().records(10).unwrap().is_empty());
}

#[tokio::test]
async fn a_swap_its_provider_could_not_lock_fails_and_the_client_is_refunded() {
    let eco = Eco::new(200).await;
    let swap = eco.pending(20).await;
    eco.net.fail(licash_api::LOCK, Fault::Refused(Code::Timing));
    let failed = eco.client.follow_swap(&swap.hash).await.unwrap();
    assert!(
        matches!(&failed.state, SwapState::Failed(why) if why.contains("not locked")),
        "{failed:?}"
    );
    let provider = eco.provider();
    assert_eq!(provider.records(1).unwrap()[0].stage, Stage::Failed);
    assert_eq!(eco.kinds(), ["accepted", "failed"]);
    assert_eq!(held(provider.wallet(), B), 200);
    assert!(provider.step().await.unwrap().is_empty());
    eco.mine(44);
    eco.client.resume().await.unwrap();
    assert_eq!(held(&eco.client, A), 100 - FEE.base);
    assert_eq!(provider.stats().unwrap().failed, 1);
}

#[tokio::test]
async fn a_lock_its_client_never_claims_returns_to_the_provider() {
    let eco = Eco::new(200).await;
    // The client's claim is lost, and the client gone.
    eco.net.fail(licash_api::SETTLE, Fault::Lost);
    let e = eco
        .client
        .swap(PROVIDER, A, B, 20, SwapLimits::usual(20))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Pending { .. }), "{e}");
    let swap = eco.client.swaps().unwrap().remove(0);
    assert!(swap.theirs.is_some());
    let provider = eco.provider();
    // Its lock expires at 14, 32 before the client's.
    assert_eq!(provider.records(1).unwrap()[0].expiry, 46 - LEAD_B);
    eco.mine(11);
    assert!(provider.step().await.unwrap().is_empty());
    assert_eq!(provider.records(1).unwrap()[0].stage, Stage::Locked);
    eco.mine(1);
    assert!(provider.step().await.unwrap().is_empty());
    assert_eq!(provider.records(1).unwrap()[0].stage, Stage::Refunded);
    assert_eq!(held(provider.wallet(), B), 200 - FEE.base);
    assert_eq!(eco.kinds(), ["accepted", "refunded"]);

    // Back, the client cannot claim the provider's lock any more, and is
    // refunded its own once expired.
    eco.mine(32);
    let resumed = eco.client.resume().await.unwrap();
    assert!(
        matches!(&resumed.swaps[0].state, SwapState::Failed(why) if why.contains("refunded")),
        "{resumed:?}"
    );
    assert_eq!(held(&eco.client, A), 100 - FEE.base);
    assert_eq!(held(&eco.client, B), 0);
}

#[tokio::test]
async fn a_provider_restarted_takes_its_swaps_on() {
    let eco = Eco::new(200).await;
    let swap = eco.pending(20).await;
    // The provider's lock is recorded, but never reaches its hub.
    eco.net.fail(licash_api::LOCK, Fault::Lost);
    let e = eco.client.follow_swap(&swap.hash).await.unwrap_err();
    assert!(
        matches!(&e, Error::Pending { reason, .. } if reason.contains("locking")),
        "{e}"
    );
    assert_eq!(eco.provider().records(1).unwrap()[0].stage, Stage::Accepted);
    assert!(eco.provider().records(1).unwrap()[0].lock.is_some());

    eco.restart();
    assert!(eco.provider().step().await.unwrap().is_empty());
    assert_eq!(eco.provider().records(1).unwrap()[0].stage, Stage::Locked);
    let done = eco.client.follow_swap(&swap.hash).await.unwrap();
    assert_eq!(done.state, SwapState::Done);
    assert_eq!(held(&eco.client, B), 20);

    // Stopped before it saw the preimage: it claims once started again.
    eco.restart();
    assert!(eco.provider().step().await.unwrap().is_empty());
    assert_eq!(eco.provider().records(1).unwrap()[0].stage, Stage::Done);
    assert_eq!(held(eco.provider().wallet(), A), 20 + costs(20).0);
    assert_eq!(eco.kinds(), ["accepted", "done"]);
}

#[tokio::test]
async fn a_provider_that_claims_too_late_loses_the_swap() {
    let eco = Eco::new(200).await;
    let swap = eco
        .client
        .swap(PROVIDER, A, B, 20, SwapLimits::usual(20))
        .await
        .unwrap();
    assert_eq!(swap.state, SwapState::Done);
    // Asleep until the client's lock, expiring at 46, is within 3 blocks.
    eco.mine(40);
    let provider = eco.provider();
    assert!(provider.step().await.unwrap().is_empty());
    let record = &provider.records(1).unwrap()[0];
    assert_eq!(record.stage, Stage::Lost);
    assert!(record.reason.as_ref().unwrap().contains("too late"));
    assert_eq!(eco.kinds(), ["accepted", "lost"]);
    let stats = provider.stats().unwrap();
    assert_eq!((stats.lost, stats.lost_amount, stats.earned), (1, 20, 0));
    // The client keeps both.
    eco.mine(4);
    eco.client.resume().await.unwrap();
    assert_eq!(held(&eco.client, A), 100 - FEE.base);
    assert_eq!(held(&eco.client, B), 20);
}

#[tokio::test]
async fn a_client_reveals_nothing_for_a_lock_that_does_not_pay_it_the_swap() {
    for lie in 0..4 {
        let eco = Eco::new(200).await;
        let provider = eco.provider();
        let payer = provider.wallet();
        let me = eco.client.address();
        let swap = eco.pending(20).await;
        let hash = hash_of(&swap);
        let (lock, why) = match lie {
            0 => (
                payer.lock(B, &me, 19, hash, Some(14)).await,
                "pays 19 of 20",
            ),
            1 => (
                payer.lock(B, &me, 20, lock_hash(&[9; 32]), Some(14)).await,
                "another hash",
            ),
            2 => (
                payer.lock(B, &payer.address(), 20, hash, Some(14)).await,
                "nothing",
            ),
            _ => (Err(Error::PaysNothing), "does not hold"),
        };
        let lock = lock.map_or("00".repeat(32), |l| l.id);
        let lied = eco.lie(&swap, &lock).await;
        assert!(
            matches!(&lied.state, SwapState::Failed(reason) if reason.contains(why)),
            "{lied:?}"
        );
        assert_eq!(lied.theirs, None);

        // No preimage was revealed: every lock is refunded once expired.
        assert!(payer
            .locks()
            .unwrap()
            .iter()
            .all(|l| l.state == licash_wallet::LockState::Locked));
        eco.mine(44);
        eco.client.resume().await.unwrap();
        assert_eq!(held(&eco.client, A), 100 - FEE.base);
    }
}

#[tokio::test]
async fn the_operator_is_alerted_once_of_each_hub_the_provider_cannot_swap_to() {
    let eco = Eco::new(30).await;
    let provider = eco.provider();
    // Nothing at A, 30 at B: swaps to A stop.
    assert_eq!(provider.check_stock().unwrap(), [A]);
    assert_eq!(provider.check_stock().unwrap(), Vec::<String>::new());
    let events = provider.events(0, 10).unwrap();
    assert!(matches!(
        events[0].notice(),
        Some(Notice::Alert { message }) if message.contains(A) && message.contains("least")
    ));

    // Paid at A, it swaps to A again; it holds too little at B now.
    let swap = eco
        .client
        .swap(PROVIDER, A, B, 25, SwapLimits::usual(25))
        .await
        .unwrap();
    assert_eq!(swap.state, SwapState::Done);
    assert!(provider.step().await.unwrap().is_empty());
    assert_eq!(provider.check_stock().unwrap(), [B]);
    assert_eq!(provider.check_stock().unwrap(), Vec::<String>::new());
    assert_eq!(eco.kinds(), ["alert", "accepted", "done", "alert"]);

    // Told again once started again.
    drop(provider);
    eco.restart();
    assert_eq!(eco.provider().check_stock().unwrap(), [B]);
}

#[tokio::test]
async fn no_swap_is_made_to_a_hub_whose_window_the_lock_paying_the_provider_cannot_outlast() {
    // A's window no longer than B's: no lock at A outlives the provider's
    // at B by B's window and more.
    let eco = Eco::with(200, net::TIMING.window, CONFIG).await;
    let provider = eco.provider();
    let info = provider.info().unwrap();
    let at_b = info.hub(&eco.address(B)).unwrap();
    assert_eq!((at_b.max, at_b.margin), (150, Some(LEAD_B)));
    let quoted = eco
        .client
        .swap_quote(PROVIDER, A, B, 20, SwapLimits::usual(20))
        .await;
    let swapped = eco
        .client
        .swap(PROVIDER, A, B, 20, SwapLimits::usual(20))
        .await;
    for e in [quoted.unwrap_err(), swapped.unwrap_err()] {
        assert!(
            matches!(&e, Error::Invalid(why) if why.contains("admits none expiring after 20")),
            "{e}"
        );
    }
    assert!(eco.client.swaps().unwrap().is_empty());
    assert_eq!(held(&eco.client, A), 100);

    // A client that locks anyway, as late as A admits, is declined.
    let (a, b, me) = (eco.address(A), eco.address(B), eco.client.address());
    let to = provider.wallet().address();
    let lock = eco
        .client
        .lock(A, &to, 20 + costs(20).0, lock_hash(&[4; 32]), None);
    let lock = lock_id(&lock.await.unwrap().id);
    let declined = provider
        .take(&Request::new(&a, &lock, &b, &me, 20))
        .await
        .unwrap_err();
    assert_eq!(declined.code, Code::Declined);
    assert!(declined.message.contains("too soon"), "{declined:?}");
    assert!(provider.records(10).unwrap().is_empty());
}

#[tokio::test]
async fn a_provider_locks_nowhere_its_operator_would_hear_of_a_kept_preimage_too_late() {
    // A refund request that confirms within 3 blocks of the lock's expiry
    // is due before the hub, at depth 1 and reaction 2, has to refund.
    let config = Config {
        confirm: 3,
        ..CONFIG
    };
    let eco = Eco::with(200, WINDOW_A, config).await;
    let provider = eco.provider();
    let info = provider.info().unwrap();
    for url in [A, B] {
        let hub = info.hub(&eco.address(url)).unwrap();
        assert_eq!((hub.max, hub.margin), (0, None));
    }
    assert_eq!(info.margin, CONFIG.margin);
    let e = eco
        .client
        .swap(PROVIDER, A, B, 20, SwapLimits::usual(20))
        .await;
    assert!(
        matches!(&e, Err(Error::Invalid(why)) if why.contains("to 0")),
        "{e:?}"
    );

    let (a, b, me) = (eco.address(A), eco.address(B), eco.client.address());
    let to = provider.wallet().address();
    let lock = eco
        .client
        .lock(A, &to, 20 + costs(20).0, lock_hash(&[5; 32]), None);
    let lock = lock_id(&lock.await.unwrap().id);
    let declined = provider
        .take(&Request::new(&a, &lock, &b, &me, 20))
        .await
        .unwrap_err();
    assert_eq!(declined.code, Code::Declined);
    assert!(declined.message.contains("refund request"), "{declined:?}");
    assert!(provider.records(10).unwrap().is_empty());
}

#[tokio::test]
async fn the_operator_is_alerted_once_of_a_hub_that_neither_pays_nor_refunds_its_lock() {
    let eco = Eco::new(200).await;
    // The client's claim is lost, and the client gone.
    eco.net.fail(licash_api::SETTLE, Fault::Lost);
    let e = eco
        .client
        .swap(PROVIDER, A, B, 20, SwapLimits::usual(20))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Pending { .. }), "{e}");
    let provider = eco.provider();
    // The refund of the provider's lock, which its wallet and the swap ask
    // for at each step, is not given.
    let withhold = |fault| {
        for _ in 0..2 {
            eco.net.fail(licash_api::REFUND, fault);
        }
    };
    // Its lock expires at 14, owed back at 15, refunded by 17 at the
    // latest if B answers.
    eco.mine(12);
    withhold(Fault::Refused(Code::Unexpired));
    assert!(provider.step().await.unwrap().is_empty());
    assert_eq!(eco.kinds(), ["accepted"]);
    eco.mine(2);
    withhold(Fault::Lost);
    assert_eq!(provider.step().await.unwrap().len(), 1);
    assert_eq!(eco.kinds(), ["accepted", "alert"]);
    withhold(Fault::Refused(Code::Unexpired));
    assert!(provider.step().await.unwrap().is_empty());
    assert_eq!(eco.kinds(), ["accepted", "alert"]);
    let events = provider.events(0, 10).unwrap();
    let Some(Notice::Alert { message }) = events[1].notice() else {
        panic!("{events:?}");
    };
    // The refund request is due by 14 + 4; the client's lock expires at 46.
    for says in [B, "expired at 14", "by height 18", A, "expires at 46"] {
        assert!(message.contains(says), "{message}");
    }
    assert_eq!(provider.records(1).unwrap()[0].stage, Stage::Locked);

    // Told again once started again, until B refunds.
    drop(provider);
    eco.restart();
    withhold(Fault::Refused(Code::Unexpired));
    assert!(eco.provider().step().await.unwrap().is_empty());
    assert!(eco.provider().step().await.unwrap().is_empty());
    assert_eq!(eco.provider().records(1).unwrap()[0].stage, Stage::Refunded);
    assert_eq!(eco.kinds(), ["accepted", "alert", "alert", "refunded"]);
}
