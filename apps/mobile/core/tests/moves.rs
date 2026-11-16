//! Moving notes between the phone's own hubs through swap providers in
//! this process, announced on a relay its hubs name: what it costs, the
//! move itself, and the next provider tried only when one failed before
//! the phone's notes were locked to it.

#[path = "../../../hub/tests/chain/mod.rs"]
mod chain;

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

use axum::{
    extract::Request,
    http::{Method, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    Json,
};
use licash::{
    address::{Address, Ephemeral, OwnerKeys},
    authorization::{Authorization, SignedProposal},
    claim::Branch,
    note::{Derivation, Evidence, IssuerKeys, Keyset, Note},
    spend::{ChainProfile, Output as SpendOutput, Proposal},
    unlink::Nonce,
};
use licash_api::{Code, Fee, Payment, Problem};
use licash_hub::{Hub, Policy, Timing};
use licash_hub_service::Services;
use licash_mobile::{Details, Kind, MoveQuote, Network, State, Wallet};
use licash_nostr::Route;
use licash_swaps::{Config, Provider};
use licash_transport::Http;
use licash_wallet::PaymentCode;
use nostr_sdk::prelude::MockRelay;

const MAX_POW: u32 = 6;

/// Short, as the wallet CLI's tests have it.
const TIMING: Timing = Timing {
    window: 20,
    reaction: 2,
    min_depth: 1,
    max_headers: 4032,
    bury: 6,
    max_lag: 120,
};

/// The response window of the first hub, moved from: a provider's lock at
/// the second, of [`TIMING`]'s window of 20, expires 32 blocks before the
/// phone's with [`TERMS`], and leaves room to claim it.
const SOURCE_WINDOW: u32 = 46;

/// A provider's terms: a fee of 1 and 1%, swapping 5 to 5000, 6 blocks to
/// claim its client's lock and 4 to get a refund request confirmed.
const TERMS: Config = Config {
    fee: Fee {
        base: 1,
        ppm: 10_000,
    },
    min: 5,
    max: 5_000,
    margin: 6,
    confirm: 4,
};

/// Two hubs charging 1 on a payment, a lock and a refund, following the
/// same chain, naming one relay for their wallets to find providers on.
struct Hubs {
    runtime: tokio::runtime::Runtime,
    relay: MockRelay,
    hubs: Vec<Served>,
    dir: tempfile::TempDir,
}

struct Served {
    hub: Arc<Hub>,
    seed: std::path::PathBuf,
    url: String,
}

impl Hubs {
    fn start() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let relay = runtime.block_on(MockRelay::run()).unwrap();
        let relays = vec![runtime.block_on(relay.url()).to_string()];
        let dir = tempfile::tempdir().unwrap();
        let mut chain = chain::Chain::new();
        let mut hubs = Vec::new();
        for (name, window) in [("a", SOURCE_WINDOW), ("b", TIMING.window)] {
            let seed = dir.path().join(format!("{name}.seed"));
            let db = dir.path().join(format!("{name}.sqlite"));
            Hub::init(&seed, &db, MAX_POW).unwrap();
            let fee = Fee { base: 1, ppm: 0 };
            let policy = Policy {
                spend: fee,
                lock: fee,
                refund: fee,
                timing: Timing { window, ..TIMING },
            };
            let hub = Arc::new(Hub::open(&seed, &db, policy).unwrap());
            hub.add_checkpoint(&chain.checkpoint(0)).unwrap();
            let services = Services {
                gateway: None,
                deposits: None,
                tweaks: None,
                inbox: None,
                relays: relays.clone(),
                contracts: None,
                swaps: None,
                urls: Default::default(),
            };
            let app = licash_hub_service::router(hub.clone(), services);
            let listener = runtime
                .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });
            hubs.push(Served { hub, seed, url });
        }
        for _ in 0..3 {
            let block = chain.mine(None).clone();
            for served in &hubs {
                served.hub.observe(chain.height(), &block).unwrap();
            }
        }
        for served in &hubs {
            assert!(served.hub.synced(chain.height(), &chain.tip()).unwrap());
        }
        // Kept synced as a hub's service following Bitcoin keeps it: a
        // loaded runner setting providers up can take longer than the
        // hubs' `max_lag`, past which they would admit no lock.
        let (height, tip) = (chain.height(), chain.tip());
        for served in &hubs {
            let hub = served.hub.clone();
            runtime.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    hub.synced(height, &tip).unwrap();
                }
            });
        }
        Self {
            runtime,
            relay,
            hubs,
            dir,
        }
    }

    fn url(&self, i: usize) -> String {
        self.hubs[i].url.clone()
    }

    fn relays(&self) -> Vec<String> {
        vec![self.runtime.block_on(self.relay.url()).to_string()]
    }

    /// A phone trusting both hubs, holding `amount` at the first.
    fn phone(&self, amount: u64) -> Arc<Wallet> {
        let dir = self.dir.path().join("phone").display().to_string();
        let phone = Wallet::create(dir, Network::Regtest, 0, false).unwrap();
        phone.add_hub(self.url(0)).unwrap();
        phone.add_hub(self.url(1)).unwrap();
        let code: PaymentCode = phone.payment_code(None).unwrap().qr.parse().unwrap();
        let address = code.licash.unwrap().address;
        let payment = self.pay(0, &address, amount);
        phone
            .receive(serde_json::to_string(&payment).unwrap(), false)
            .unwrap();
        phone
    }

    /// Notes of `amount` paid to `to` at hub `i`, minted as its issuer.
    fn pay(&self, i: usize, to: &Address, amount: u64) -> Payment {
        let served = &self.hubs[i];
        let text = std::fs::read_to_string(&served.seed).unwrap();
        let mut seed = [0; 32];
        hex::decode_to_slice(text.trim(), &mut seed).unwrap();
        let keys = IssuerKeys::from_seed(&seed, 0, MAX_POW).unwrap();
        let keyset = keys.keyset().unwrap();
        let info = served.hub.info();
        let mut total = amount;
        while amount + info.policy.spend.due(total) != total {
            total = amount + info.policy.spend.due(total);
        }
        let payer = OwnerKeys::from_seed(&[0xfa; 32]).unwrap();
        let inputs: Vec<(Note, Derivation)> = split(total)
            .into_iter()
            .map(|denom| mint(&keys, &keyset, &payer.address(), denom))
            .collect();
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
            .map(|(i, (d, to))| {
                let derivation = ephemeral.derive(to, d, Branch::Pay, i).unwrap();
                SpendOutput::from_derivation(&derivation).unwrap()
            })
            .collect();
        let proposal = Proposal::new(&keyset, &notes, ephemeral.public(), outputs, None).unwrap();
        let grant = served.hub.offer(&proposal, &notes).unwrap();
        let owners: Vec<_> = inputs
            .iter()
            .map(|(_, d)| d.ownership().owner_secret(payer.spend_secret()).unwrap())
            .collect();
        let grant = Authorization::Granted(Box::new(grant));
        let signed = SignedProposal::sign(&keyset, proposal, &notes, grant, &owners).unwrap();
        let unlock = served.hub.release(&signed, &notes).unwrap();
        Payment::new(&served.url, &signed, &notes, &unlock)
    }
}

fn split(mut amount: u64) -> Vec<u64> {
    let mut denoms = Vec::new();
    for pow in (0..=MAX_POW).rev() {
        while amount >= 1 << pow {
            denoms.push(1 << pow);
            amount -= 1 << pow;
        }
    }
    denoms
}

fn mint(keys: &IssuerKeys, keyset: &Keyset, to: &Address, denom: u64) -> (Note, Derivation) {
    let d = Ephemeral::random()
        .derive(to, denom, Branch::Pay, 0)
        .unwrap();
    let output = SpendOutput::from_derivation(&d).unwrap();
    let proof = keys.prove_blinded(denom, output.blinded()).unwrap();
    let note = Note::from_parts(
        keyset,
        *keyset.id(),
        d.message().clone(),
        proof.open_point(keyset, &d).unwrap(),
        Evidence::Linked {
            dleq: proof.dleq(),
            blinding: *d.blinding(),
            commit: Nonce::new(keyset, &output).unwrap().commit().unwrap(),
        },
    )
    .unwrap();
    (note, d)
}

/// A swap provider on a local port between both hubs, holding stock at
/// the second, announced on their relay, taking its swaps on as the
/// service does, in the runtime of its own thread.
struct Swaps {
    provider: Arc<Provider<Http>>,
    /// Answers nothing but 503 while set: as if unreachable.
    down: Arc<AtomicBool>,
    /// Refuses swap requests while set, as invalid.
    refuse: Arc<AtomicBool>,
    addr: SocketAddr,
    stop: Option<(tokio::sync::oneshot::Sender<()>, JoinHandle<()>)>,
    _dir: tempfile::TempDir,
}

impl Swaps {
    fn start(hubs: &Hubs, seed: u8, terms: Config) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let wallet = licash_wallet::Wallet::create(
            &dir.path().join("wallet.sqlite"),
            &[seed; 32],
            ChainProfile::Core30DefaultRegtest,
            Http::new(None).unwrap(),
        )
        .unwrap();
        let stocked = hubs.pay(1, &wallet.address(), 3_000);
        let urls = vec![hubs.url(0), hubs.url(1)];
        let relays = hubs.relays();
        let swaps = dir.path().join("swaps.sqlite");
        let (down, refuse) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (downed, refusing) = (down.clone(), refuse.clone());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                for url in &urls {
                    wallet.add_hub(url).await.unwrap();
                }
                wallet.receive(&stocked).unwrap();
                let provider = Arc::new(Provider::create(&swaps, wallet, terms).unwrap());
                let nostr = licash_nostr::public_key(&provider.nostr_secret().unwrap()).unwrap();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                ready.send((addr, provider.clone())).unwrap();
                let every = Duration::from_millis(100);
                let stepping = tokio::spawn(licash_swaps_service::step(provider.clone(), every));
                let announcing = tokio::spawn(licash_swaps_service::announce(
                    provider.clone(),
                    relays,
                    Route::Direct,
                    vec![format!("http://{addr}")],
                    Some(format!("provider {seed}")),
                ));
                let failing = move |request: Request, next: Next| {
                    let down = downed.load(Ordering::SeqCst);
                    let refuse =
                        refusing.load(Ordering::SeqCst) && request.method() == Method::POST;
                    async move {
                        if down {
                            return StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        if refuse {
                            let problem = Problem::new(Code::Invalid, "refused as asked");
                            return (StatusCode::BAD_REQUEST, Json(problem)).into_response();
                        }
                        next.run(request).await
                    }
                };
                let router = licash_swaps_service::router(provider, Some(nostr))
                    .layer(middleware::from_fn(failing));
                axum::serve(listener, router)
                    .with_graceful_shutdown(async {
                        stopped.await.ok();
                    })
                    .await
                    .unwrap();
                stepping.abort();
                announcing.abort();
            });
        });
        let (addr, provider) = started.recv().unwrap();
        Self {
            provider,
            down,
            refuse,
            addr,
            stop: Some((stop, thread)),
            _dir: dir,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// How many swaps it took.
    fn swaps(&self) -> usize {
        self.provider.records(100).unwrap().len()
    }
}

impl Drop for Swaps {
    fn drop(&mut self) {
        if let Some((stop, thread)) = self.stop.take() {
            stop.send(()).ok();
            thread.join().ok();
        }
    }
}

/// The offers of moving `amount` from the first hub to the second, once
/// `count` providers are found, within 10 seconds.
fn offers(hubs: &Hubs, phone: &Wallet, amount: Option<u64>, count: usize) -> Vec<MoveQuote> {
    let mut last = None;
    for _ in 0..50 {
        match phone.move_offers(hubs.url(0), hubs.url(1), amount) {
            Ok(offers) if offers.len() >= count => return offers,
            other => last = Some(other),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("{last:?}");
}

/// What the phone holds available at hub `i`.
fn held(hubs: &Hubs, phone: &Wallet, i: usize) -> u64 {
    let url = hubs.url(i);
    let holdings = phone.holdings().unwrap();
    holdings
        .iter()
        .find(|h| h.hub == url)
        .map_or(0, |h| h.available)
}

/// What the phone holds at hub `i` once nothing is pending there: its
/// change of a lock comes back once the lock settles, within 10 seconds.
fn settled(hubs: &Hubs, phone: &Wallet, i: usize) -> u64 {
    let url = hubs.url(i);
    for _ in 0..50 {
        let holdings = phone.holdings().unwrap();
        let holding = holdings.iter().find(|h| h.hub == url);
        if holding.is_none_or(|h| h.pending == 0) {
            return holding.map_or(0, |h| h.available);
        }
        phone.resume().unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("{:?}", phone.holdings().unwrap());
}

/// The phone's move of `id` once done, its history followed meanwhile,
/// within a minute: a loaded runner, running several of these at once,
/// took more than 10 seconds.
fn until_moved(phone: &Wallet, id: &str) {
    let by = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < by {
        let history = phone.history().unwrap();
        let line = history.iter().find(|a| a.id == id && a.kind == Kind::Swap);
        match line.map(|a| a.state) {
            Some(State::Done) => return,
            Some(State::Failed) => panic!("{line:?}"),
            _ => {}
        }
        phone.resume().unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("{:?}", phone.history().unwrap());
}

#[test]
fn a_phone_moves_its_notes_to_another_of_its_hubs_through_a_provider() {
    let hubs = Hubs::start();
    let phone = hubs.phone(1_000);
    let swaps = Swaps::start(&hubs, 0x51, TERMS);
    let quoted = offers(&hubs, &phone, Some(300), 1);
    // The provider's 1 and 1%, and the first hub's 1 on the lock.
    let offer = &quoted[0];
    assert_eq!(
        (offer.amount, offer.lock_fee, offer.fee),
        (300, 1, 5),
        "{offer:?}"
    );
    assert_eq!((&offer.from, &offer.to), (&hubs.url(0), &hubs.url(1)));
    // Its lock at the second hub expires 32 blocks before the phone's: 4 to
    // confirm a refund request, 20, 1 to read the preimage, 6 and 1.
    assert_eq!(offer.margin, 32, "{offer:?}");
    assert_eq!(offer.provider, swaps.url());
    assert_eq!(offer.provider_name.as_deref(), Some("provider 81"));

    let moved = phone.move_notes(quoted.clone()).unwrap();
    assert_eq!((moved.kind, moved.amount, moved.fee), (Kind::Swap, 300, 5));
    until_moved(&phone, &moved.id);
    assert_eq!(held(&hubs, &phone, 1), 300);
    assert_eq!(settled(&hubs, &phone, 0), 1_000 - 305);
    match phone.details(Kind::Swap, moved.id.clone()).unwrap() {
        Details::Swap {
            from,
            to,
            provider,
            lock,
            theirs,
            failure,
            provider_fee,
            lock_fee,
        } => {
            assert_eq!(
                (from, to, provider),
                (hubs.url(0), hubs.url(1), swaps.url())
            );
            assert!(lock.is_some() && theirs.is_some() && failure.is_none());
            // Its fee's parts as recorded: the provider's 4, the hub's 1.
            assert_eq!((provider_fee, lock_fee), (4, 1));
        }
        other => panic!("{other:?}"),
    }
    // Its lock spent notes at the first hub, its claim made them at the
    // second.
    let notes = phone.notes_of(Kind::Swap, moved.id.clone()).unwrap();
    let at = |notes: &[licash_mobile::NoteLine], i: usize| -> u64 {
        let url = hubs.url(i);
        notes
            .iter()
            .filter(|n| n.hub == url)
            .map(|n| n.amount)
            .sum()
    };
    assert_eq!(at(&notes.spent, 1), 0, "{notes:?}");
    assert_eq!(at(&notes.made, 1), 300, "{notes:?}");
    assert_eq!(at(&notes.spent, 0), 305 + at(&notes.made, 0), "{notes:?}");

    // All that is left at the first hub, fitted to the fees.
    let left = held(&hubs, &phone, 0);
    let all = offers(&hubs, &phone, None, 1);
    let spends = all[0].amount + all[0].fee;
    assert!(spends <= left && spends + 5 >= left, "{all:?} of {left}");
    let moved = phone.move_notes(all.clone()).unwrap();
    until_moved(&phone, &moved.id);
    assert_eq!(settled(&hubs, &phone, 0), left - spends);
    assert_eq!(held(&hubs, &phone, 1), 300 + all[0].amount);
    let history = phone.history().unwrap();
    assert_eq!(history.iter().filter(|a| a.kind == Kind::Swap).count(), 2);

    // Between two hubs only.
    assert!(phone.move_offers(hubs.url(0), hubs.url(0), None).is_err());
}

#[test]
fn a_move_tries_the_next_provider_costing_no_more_if_one_fails_before_locking() {
    let hubs = Hubs::start();
    let phone = hubs.phone(1_000);
    let first = Swaps::start(&hubs, 0x51, TERMS);
    let dearer = Config {
        fee: Fee {
            base: 50,
            ppm: 10_000,
        },
        ..TERMS
    };
    let pricier = Swaps::start(&hubs, 0x52, dearer);
    let same = Swaps::start(&hubs, 0x53, TERMS);
    let found = offers(&hubs, &phone, Some(300), 3);
    let by = |swaps: &Swaps| {
        found
            .iter()
            .find(|o| o.provider == swaps.url())
            .unwrap()
            .clone()
    };
    let chosen = vec![by(&first), by(&pricier), by(&same)];
    assert!(chosen[1].fee > chosen[0].fee && chosen[2].fee == chosen[0].fee);

    // The first unreachable: nothing locked to it, so the next costing
    // no more is tried; the dearer one never.
    first.down.store(true, Ordering::SeqCst);
    let moved = phone.move_notes(chosen.clone()).unwrap();
    until_moved(&phone, &moved.id);
    match phone.details(Kind::Swap, moved.id.clone()).unwrap() {
        Details::Swap { provider, .. } => assert_eq!(provider, same.url()),
        other => panic!("{other:?}"),
    }
    assert_eq!((first.swaps(), pricier.swaps(), same.swaps()), (0, 0, 1));
    assert_eq!(held(&hubs, &phone, 1), 300);

    // None costing no more reachable: refused, nothing moved or locked.
    same.down.store(true, Ordering::SeqCst);
    let before = held(&hubs, &phone, 0);
    assert!(phone.move_notes(chosen).is_err());
    assert_eq!(held(&hubs, &phone, 0), before);
    assert_eq!(pricier.swaps(), 0);
    let history = phone.history().unwrap();
    assert_eq!(history.iter().filter(|a| a.kind == Kind::Swap).count(), 1);
}

#[test]
fn a_move_is_never_tried_again_once_its_notes_were_locked() {
    let hubs = Hubs::start();
    let phone = hubs.phone(1_000);
    let first = Swaps::start(&hubs, 0x51, TERMS);
    let next = Swaps::start(&hubs, 0x53, TERMS);
    let found = offers(&hubs, &phone, Some(300), 2);
    let by = |swaps: &Swaps| {
        found
            .iter()
            .find(|o| o.provider == swaps.url())
            .unwrap()
            .clone()
    };

    // The phone locks its notes first, then asks: refused once locked.
    first.refuse.store(true, Ordering::SeqCst);
    let moving = phone.move_notes(vec![by(&first), by(&next)]);
    assert_eq!(next.swaps(), 0, "{moving:?}");
    let history = phone.history().unwrap();
    let swaps: Vec<_> = history.iter().filter(|a| a.kind == Kind::Swap).collect();
    assert_eq!(swaps.len(), 1, "{history:?}");
    match phone.details(Kind::Swap, swaps[0].id.clone()).unwrap() {
        Details::Swap { provider, lock, .. } => {
            assert_eq!(provider, first.url());
            assert!(lock.is_some());
        }
        other => panic!("{other:?}"),
    }
    // Its notes stay locked until the lock expires: none at the other hub.
    assert_eq!(held(&hubs, &phone, 1), 0);
    assert!(held(&hubs, &phone, 0) < 1_000 - 300);
}
