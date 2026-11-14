//! [`Wallet`]: the SDK's wallet as the apps use it.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use bitcoin::FeeRate;
use licash_nostr::Route;
use licash_transport::Http;
use licash_wallet::{DepositState, PaymentCode, MAX_HUBS};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};

use crate::{
    chain::Chain, Activity, Balance, ChainSource, Code, Hub, Kind, LicashError, Network, Paid,
    Range, Scan, Scanned, State,
};

/// The rate deposits are paid at, in sat/vB: above what nodes relay.
const DEPOSIT_RATE: u64 = 2;

/// The blocks a scan looks in at most, to report progress between.
const SCAN_BATCH: u32 = 144;

/// A wallet, kept in a directory of its own.
#[derive(uniffi::Object)]
pub struct Wallet {
    runtime: tokio::runtime::Runtime,
    wallet: licash_wallet::Wallet<Http>,
    route: Route,
    tor: bool,
    dir: PathBuf,
}

/// What the app keeps of a wallet besides the SDK's file.
#[derive(Default, Serialize, Deserialize)]
struct Kept {
    /// The chain's height when the wallet was made: none of its payments
    /// on Bitcoin can be older. Unknown, everything a hub indexed is
    /// looked in.
    #[serde(default)]
    birthday: u32,
}

fn wallet_file(dir: &Path) -> PathBuf {
    dir.join("wallet.sqlite")
}

fn kept_file(dir: &Path) -> PathBuf {
    dir.join("app.json")
}

/// Whether `dir` holds a wallet.
#[uniffi::export]
pub fn wallet_exists(dir: String) -> bool {
    wallet_file(Path::new(&dir)).exists()
}

#[uniffi::export]
impl Wallet {
    /// A new wallet of `network` in `dir`, which holds none, made at the
    /// chain's height `birthday` (0 if unknown), reaching hubs and relays
    /// through Tor if `tor`.
    #[uniffi::constructor]
    pub fn create(
        dir: String,
        network: Network,
        birthday: u32,
        tor: bool,
    ) -> Result<Arc<Self>, LicashError> {
        let dir = PathBuf::from(dir);
        if wallet_exists(dir.display().to_string()) {
            return Err(LicashError::invalid("a wallet is there already"));
        }
        std::fs::create_dir_all(&dir).map_err(LicashError::failed)?;
        let kept = serde_json::to_vec(&Kept { birthday }).map_err(LicashError::failed)?;
        std::fs::write(kept_file(&dir), kept).map_err(LicashError::failed)?;
        let mut seed = [0; 32];
        OsRng.fill_bytes(&mut seed);
        Self::start(dir, tor, |path, http| {
            licash_wallet::Wallet::create(path, &seed, network.profile(), http)
        })
    }

    /// The wallet in `dir`.
    #[uniffi::constructor]
    pub fn open(dir: String, tor: bool) -> Result<Arc<Self>, LicashError> {
        Self::start(PathBuf::from(dir), tor, licash_wallet::Wallet::open)
    }

    pub fn network(&self) -> Network {
        Network::of(self.wallet.network())
    }

    /// Whether it reaches hubs and relays through Tor.
    pub fn tor(&self) -> bool {
        self.tor
    }
}

impl Wallet {
    fn start(
        dir: PathBuf,
        tor: bool,
        wallet: impl FnOnce(&Path, Http) -> Result<licash_wallet::Wallet<Http>, licash_wallet::Error>,
    ) -> Result<Arc<Self>, LicashError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(LicashError::failed)?;
        let (http, route) = match tor {
            true => {
                let _inside = runtime.enter();
                let tor = licash_tor::Tor::new(&dir.join("tor")).map_err(LicashError::failed)?;
                (Http::tor(tor.clone()), Route::Tor(tor))
            }
            false => (Http::new(None).map_err(LicashError::failed)?, Route::Direct),
        };
        let wallet = wallet(&wallet_file(&dir), http)?;
        Ok(Arc::new(Self {
            runtime,
            wallet,
            route,
            tor,
            dir,
        }))
    }

    fn kept(&self) -> Kept {
        // Absent or unreadable, nothing is known: the conservative default.
        std::fs::read(kept_file(&self.dir))
            .ok()
            .and_then(|kept| serde_json::from_slice(&kept).ok())
            .unwrap_or_default()
    }
}

/// Hubs, and what the wallet holds.
#[uniffi::export]
impl Wallet {
    /// Trust the hub at `url`: take its notes, and pay through it.
    pub fn add_hub(&self, url: String) -> Result<Hub, LicashError> {
        let url = url.trim();
        let host = url.split("://").nth(1).unwrap_or(url);
        let host = host.split(['/', ':']).next().unwrap_or(host);
        if host.ends_with(".onion") && !self.tor {
            return Err(LicashError::NeedsTor);
        }
        let info = self.runtime.block_on(self.wallet.add_hub(url))?;
        // Its URL as the wallet keeps it.
        let mut kept = self.wallet.hubs()?.into_iter();
        let kept = kept
            .find(|(_, i)| i.address == info.address)
            .map(|(url, _)| url);
        Ok(hub(&kept.unwrap_or_else(|| url.to_owned()), &info))
    }

    /// The hubs trusted, by URL.
    pub fn hubs(&self) -> Result<Vec<Hub>, LicashError> {
        let hubs = self.wallet.hubs()?;
        Ok(hubs.iter().map(|(url, info)| hub(url, info)).collect())
    }

    pub fn balance(&self) -> Result<Balance, LicashError> {
        let mut balance = Balance::default();
        for held in self.wallet.balance()? {
            balance.notes += held.available;
            balance.notes_pending += held.pending;
        }
        for coin in self.wallet.silent_coins()? {
            match coin.spending {
                Some(_) => balance.moving += coin.value,
                None => balance.onchain += coin.value,
            }
        }
        Ok(balance)
    }

    /// The code to be paid at, `amount` if given: in notes at the hubs
    /// trusted, and on Bitcoin.
    pub fn payment_code(&self, amount: Option<u64>) -> Result<Code, LicashError> {
        let hubs = self.wallet.hubs()?.into_iter().map(|(url, _)| url);
        let hubs: Vec<String> = hubs.take(MAX_HUBS).collect();
        let mut code = self.wallet.payment_code(&hubs, true)?;
        code.amount = amount.filter(|a| *a > 0);
        Ok(Code {
            text: code.to_string(),
            qr: code.qr(),
        })
    }

    /// What `text`, scanned or pasted, is.
    pub fn scanned(&self, text: String) -> Result<Scanned, LicashError> {
        let text = text.trim();
        if let Ok(code) = text.parse::<PaymentCode>() {
            code.check(self.wallet.network())?;
            let hubs = self.wallet.hubs()?;
            let mut trusted = hubs
                .iter()
                .map(|(_, i)| licash_wallet::HubId::of(&i.address));
            let notes = code
                .licash
                .as_ref()
                .is_some_and(|licash| trusted.any(|id| licash.hubs.contains(&id)));
            return Ok(Scanned::Code {
                amount: code.amount,
                notes,
            });
        }
        if serde_json::from_str::<licash_api::Payment>(text).is_ok() {
            return Ok(Scanned::Payment);
        }
        Err(LicashError::invalid("neither a payment code nor a payment"))
    }

    /// Pay `amount`, or what it asks, to the payment code `code`, in notes:
    /// to the payee's mailbox, or for them to receive by hand.
    pub fn pay(&self, code: String, amount: Option<u64>) -> Result<Paid, LicashError> {
        let code: PaymentCode = code.trim().parse().map_err(LicashError::invalid)?;
        let route = self.route.clone();
        let pay = licash_nostr::wallet::pay(&self.wallet, &code, amount, || Ok(route));
        let paid = self
            .runtime
            .block_on(pay)
            .map_err(|e| LicashError::from_any(e, |reason| LicashError::Invalid { reason }))?;
        let handover = match paid.delivered {
            Some(_) => None,
            None => Some(serde_json::to_string(&paid.paid.payment).map_err(LicashError::failed)?),
        };
        Ok(Paid {
            id: paid.paid.id,
            amount: paid.amount,
            hub: paid.hub,
            delivered: paid.delivered.is_some(),
            handover,
            undelivered: paid.undelivered,
        })
    }

    /// Take the payment `payment` handed over: under a hub trusted, or any
    /// if `trust`, which it then is.
    pub fn receive(&self, payment: String, trust: bool) -> Result<Activity, LicashError> {
        let payment: licash_api::Payment = serde_json::from_str(payment.trim())
            .map_err(|_| LicashError::invalid("not a payment"))?;
        let received = match self.wallet.receive(&payment) {
            Err(licash_wallet::Error::UnknownHub(url)) if trust => {
                self.runtime.block_on(self.wallet.add_hub(&url))?;
                self.wallet.receive(&payment)?
            }
            received => received?,
        };
        self.unlink();
        Ok(received_activity(&received))
    }

    /// Take the payments in the wallet's mailbox, on the relays its hubs
    /// name: each once.
    pub fn collect(&self) -> Result<Vec<Activity>, LicashError> {
        if self
            .wallet
            .hubs()?
            .iter()
            .all(|(_, info)| info.relays.is_empty())
        {
            return Ok(Vec::new());
        }
        let collect = licash_nostr::wallet::collect(&self.wallet, &self.route);
        let collected = self
            .runtime
            .block_on(collect)
            .map_err(|e| LicashError::from_any(e, |reason| LicashError::Unavailable { reason }))?;
        for (id, reason) in &collected.skipped {
            tracing::warn!(%id, %reason, "mail not taken");
        }
        if !collected.received.is_empty() {
            self.unlink();
        }
        Ok(collected.received.iter().map(received_activity).collect())
    }

    /// Finish what was under way: payments, and moves to notes.
    pub fn resume(&self) -> Result<(), LicashError> {
        let resumed = self.runtime.block_on(self.wallet.resume())?;
        for (id, reason) in &resumed.abandoned {
            tracing::warn!(%id, %reason, "given up");
        }
        for (id, reason) in &resumed.pending {
            tracing::info!(%id, %reason, "still pending");
        }
        self.unlink();
        Ok(())
    }
}

/// History, and Bitcoin.
#[uniffi::export]
impl Wallet {
    /// Everything paid and received, coins on Bitcoin not moved yet
    /// first, then the newest first.
    pub fn history(&self) -> Result<Vec<Activity>, LicashError> {
        let mut history = Vec::new();
        for sent in self.wallet.sends()? {
            let (state, detail) = match sent.state {
                licash_wallet::State::Done => (State::Done, sent.hub),
                licash_wallet::State::Abandoned(why) => (State::Failed, why),
                _ => (State::Pending, sent.hub),
            };
            history.push(Activity {
                id: sent.id,
                kind: Kind::Sent,
                state,
                amount: sent.amount,
                fee: sent.fee,
                detail,
                at: Some(sent.created_at),
            });
        }
        let deposits = self.wallet.deposits()?;
        // A move's notes are received under its lock: shown as the move.
        let moved: Vec<&String> = deposits.iter().filter_map(|d| d.lock.as_ref()).collect();
        let receipts = self.wallet.receipts()?.into_iter();
        let receipts = receipts.filter(|r| !moved.contains(&&r.id));
        history.extend(receipts.map(|r| received_activity(&r)));
        history.extend(deposits.iter().map(moved_activity));
        for coin in self.wallet.silent_coins()? {
            let state = match coin.spending {
                Some(_) => State::Pending,
                None => State::Done,
            };
            history.push(Activity {
                id: coin.outpoint.to_string(),
                kind: Kind::Onchain,
                state,
                amount: coin.value,
                fee: 0,
                detail: format!("block {}", coin.height),
                at: None,
            });
        }
        history.sort_by_key(|a| (a.at.is_some(), std::cmp::Reverse(a.at)));
        Ok(history)
    }

    /// Look for payments on Bitcoin in the next blocks of `chain`, with
    /// the tweaks of a hub trusted, and broadcast again the wallet's spends
    /// not in a block yet once caught up. Called until `next` passes
    /// `height`.
    pub fn scan(&self, chain: Arc<dyn ChainSource>) -> Result<Scan, LicashError> {
        let hubs = self.wallet.hubs()?.into_iter();
        let mut tweaks: Vec<(String, u32)> =
            hubs.filter_map(|(u, i)| Some((u, i.tweaks?))).collect();
        tweaks.sort_by_key(|(_, first)| *first);
        let Some((earliest, first)) = tweaks.first().cloned() else {
            return Err(LicashError::invalid(
                "no hub trusted serves what finds payments on Bitcoin",
            ));
        };
        let next = match self.wallet.silent_next()? {
            Some(next) => next,
            None => {
                let from = self.kept().birthday.max(first);
                self.wallet.watch_silent(&earliest, from)?;
                from
            }
        };
        let source = Chain(chain.clone());
        let mut failed = None;
        // Any hub serving the blocks: their tweaks are the chain's.
        for (url, _) in tweaks.iter().filter(|(_, first)| *first <= next) {
            match self
                .runtime
                .block_on(self.wallet.scan_silent(url, &source, SCAN_BATCH))
            {
                Ok(scanned) => {
                    if scanned.next > scanned.height {
                        self.broadcast_again(chain.as_ref());
                    }
                    return Ok(Scan {
                        next: scanned.next,
                        height: scanned.height,
                        found: scanned.found.len() as u32,
                        found_value: scanned.found.iter().map(|c| c.value).sum(),
                    });
                }
                Err(e @ licash_wallet::Error::Unavailable { .. }) => failed = Some(e),
                Err(e) => return Err(e.into()),
            }
        }
        Err(match failed {
            Some(e) => e.into(),
            None => LicashError::invalid(format!("no hub trusted serves block {next} on")),
        })
    }

    /// Move the coins paid on Bitcoin to notes, at a hub trusted that
    /// takes them, broadcasting through `chain`: the move, under way.
    pub fn move_to_licash(&self, chain: Arc<dyn ChainSource>) -> Result<Activity, LicashError> {
        let hubs = self.wallet.hubs()?.into_iter();
        let Some(hub) = hubs
            .filter(|(_, i)| i.deposits.is_some())
            .map(|(u, _)| u)
            .next()
        else {
            return Err(LicashError::invalid(
                "no hub trusted takes coins on Bitcoin",
            ));
        };
        let rate = FeeRate::from_sat_per_vb(DEPOSIT_RATE).expect("a rate");
        let (deposit, tx) = self
            .runtime
            .block_on(self.wallet.deposit_silent(&hub, rate))?;
        if let Err(e) = chain.broadcast(bitcoin::consensus::serialize(&tx)) {
            // Its coins free again: the deposit lapses unpaid.
            self.wallet.abandon_silent(&tx.compute_txid())?;
            return Err(LicashError::Unavailable {
                reason: format!("broadcasting: {e}"),
            });
        }
        tracing::info!(hash = %deposit.hash, txid = %tx.compute_txid(), "moving to notes");
        Ok(moved_activity(&deposit))
    }
}

impl Wallet {
    /// Unlink what can be: what is left stays linked until the next try.
    fn unlink(&self) {
        if let Err(e) = self.runtime.block_on(self.wallet.unlink()) {
            tracing::warn!(%e, "some notes stay linked for now");
        }
    }

    fn broadcast_again(&self, chain: &dyn ChainSource) {
        let spends = match self.wallet.silent_spends() {
            Ok(spends) => spends,
            Err(e) => return tracing::warn!(%e, "the spends to broadcast again"),
        };
        for tx in spends {
            if let Err(e) = chain.broadcast(bitcoin::consensus::serialize(&tx)) {
                tracing::debug!(txid = %tx.compute_txid(), %e, "broadcasting again");
            }
        }
    }
}

fn hub(url: &str, info: &licash_api::Info) -> Hub {
    Hub {
        url: url.to_owned(),
        fee_base: info.fees.spend.base,
        fee_ppm: info.fees.spend.ppm,
        deposits: info.deposits.as_ref().map(|d| Range {
            min: d.min,
            max: d.max,
        }),
        silent: info.tweaks.is_some(),
        mailbox: !info.relays.is_empty(),
    }
}

fn received_activity(received: &licash_wallet::Received) -> Activity {
    Activity {
        id: received.id.clone(),
        kind: Kind::Received,
        state: State::Done,
        amount: received.amount,
        fee: 0,
        detail: received.hub.clone(),
        at: Some(received.received_at),
    }
}

fn moved_activity(deposit: &licash_wallet::Deposit) -> Activity {
    let (state, detail) = match &deposit.state {
        DepositState::Received => (State::Done, deposit.hub.clone()),
        DepositState::Canceled(why) => (State::Failed, why.clone()),
        DepositState::Refunded(txid) => (State::Failed, format!("refunded in {txid}")),
        _ => (State::Pending, deposit.hub.clone()),
    };
    let pays = deposit.terms.as_ref().map_or(deposit.amount, |t| t.pays);
    Activity {
        id: deposit.hash.clone(),
        kind: Kind::Moved,
        state,
        amount: deposit.amount,
        fee: pays.saturating_sub(deposit.amount),
        detail,
        at: Some(deposit.created_at),
    }
}
