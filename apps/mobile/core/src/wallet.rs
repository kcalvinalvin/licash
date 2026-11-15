//! [`Wallet`]: the SDK's wallet as the apps use it.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use bitcoin::{FeeRate, OutPoint};
use licash_nostr::Route;
use licash_transport::Http;
use licash_wallet::{DepositState, PaymentCode, MAX_HUBS};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};

use crate::{
    chain::Chain,
    deliveries::{self, Delivery},
    Activity, Balance, ChainError, ChainSource, Code, Details, Holding, Hub, InvoiceQuote, Kind,
    LicashError, LightningInvoice, LightningTerms, Network, Notes, Onchain, Paid, PayQuote, Range,
    Rate, Scan, Scanned, State,
};

/// The rate deposits are paid at, in sat/vB: above what nodes relay.
const DEPOSIT_RATE: u64 = 2;

/// The blocks a scan looks in at most, to report progress between.
const SCAN_BATCH: u32 = 144;

/// A day, in seconds.
const DAY: i64 = 86_400;

mod payouts;

/// A wallet, kept in a directory of its own.
#[derive(uniffi::Object)]
pub struct Wallet {
    runtime: tokio::runtime::Runtime,
    wallet: licash_wallet::Wallet<Http>,
    route: Route,
    tor: bool,
    dir: PathBuf,
    /// Held while paying and finishing what is under way: the payments to
    /// send to mailboxes change only under it.
    delivering: Mutex<()>,
    /// The plan of the last [`Wallet::pay_quote`], for [`Wallet::pay`]:
    /// for the code, by its text, and the amount it pays.
    planned: Mutex<Option<(String, licash_nostr::wallet::Plan)>>,
}

/// What the app keeps of a wallet besides the SDK's file.
#[derive(Default, Serialize, Deserialize)]
struct Kept {
    /// The chain's height when the wallet was made: none of its payments
    /// on Bitcoin can be older. Unknown (0), from the blocks of the day
    /// before its file was made.
    #[serde(default)]
    birthday: u32,
}

fn holding(b: licash_wallet::Balance) -> Holding {
    Holding {
        hub: b.hub,
        available: b.available,
        pending: b.pending,
    }
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
            delivering: Mutex::new(()),
            planned: Mutex::new(None),
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
    /// Trust the hub at `url`: take its notes, and pay through it. A host
    /// alone is reached over HTTPS, or HTTP for an onion service.
    pub fn add_hub(&self, url: String) -> Result<Hub, LicashError> {
        let url = url.trim();
        let host = url.split("://").nth(1).unwrap_or(url);
        let host = host.split(['/', ':']).next().unwrap_or(host);
        let onion = host.ends_with(".onion");
        if onion && !self.tor {
            return Err(LicashError::NeedsTor);
        }
        let url = match url.contains("://") {
            true => url.to_owned(),
            false if onion => format!("http://{url}"),
            false => format!("https://{url}"),
        };
        let url = url.as_str();
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
        let trusted = self.wallet.hubs()?;
        let hubs: Vec<String> = trusted
            .iter()
            .map(|(url, _)| url.clone())
            .take(MAX_HUBS)
            .collect();
        let mut code = self.wallet.payment_code(&hubs, true)?;
        code.amount = amount.filter(|a| *a > 0);
        // Payers pay a silent payment address rather than the fallback:
        // only if a hub's tweaks find those payments.
        if !trusted.iter().any(|(_, info)| info.tweaks.is_some()) {
            code.silent = None;
        }
        Ok(Code {
            text: code.to_string(),
            qr: code.qr(),
            address: code
                .address
                .as_ref()
                .map(|a| a.assume_checked_ref().to_string()),
            silent: code.silent.as_ref().map(|s| s.to_string()),
        })
    }

    /// What `text`, scanned or pasted, is: a code to pay in notes if it
    /// can be, else a Lightning invoice if it is or names one, else an
    /// address on Bitcoin to pay out to if it is or names one.
    pub fn scanned(&self, text: String) -> Result<Scanned, LicashError> {
        let text = text.trim();
        let code = text.parse::<PaymentCode>();
        if let Ok(code) = &code {
            code.check(self.wallet.network())?;
            let hubs = self.wallet.hubs()?;
            let mut trusted = hubs
                .iter()
                .map(|(_, i)| licash_wallet::HubId::of(&i.address));
            let notes = code
                .licash
                .as_ref()
                .is_some_and(|licash| trusted.any(|id| licash.hubs.contains(&id)));
            if notes {
                return Ok(Scanned::Code {
                    amount: code.amount,
                    notes,
                });
            }
        }
        match self.wallet.read_invoice(text) {
            Ok(invoice) => {
                return Ok(Scanned::Invoice {
                    amount: invoice.amount,
                    description: invoice.description,
                })
            }
            // An invoice this wallet cannot pay: say why.
            Err(e) if code.is_err() && licash_wallet::Payable::read(text).is_ok() => {
                return Err(e.into())
            }
            Err(_) => {}
        }
        if let Ok(code) = code {
            // Its hubs not trusted, reached where it says, proving who
            // they are.
            if code
                .licash
                .as_ref()
                .is_some_and(|l| !l.locations.is_empty())
            {
                return Ok(Scanned::Visit {
                    amount: code.amount,
                    address: code
                        .address
                        .as_ref()
                        .map(|a| a.assume_checked_ref().to_string()),
                });
            }
            if let Some(address) = &code.address {
                return Ok(Scanned::Bitcoin {
                    address: address.assume_checked_ref().to_string(),
                    amount: code.amount,
                });
            }
            return Ok(Scanned::Code {
                amount: code.amount,
                notes: false,
            });
        }
        if serde_json::from_str::<licash_api::Payment>(text).is_ok() {
            return Ok(Scanned::Payment);
        }
        Err(LicashError::invalid(
            "neither a payment code, a Lightning invoice nor a payment",
        ))
    }

    /// What paying the Lightning invoice `text` costs through the hub
    /// trusted paying it for least, with the notes held now.
    pub fn invoice_quote(&self, text: String) -> Result<InvoiceQuote, LicashError> {
        let invoice = self.wallet.read_invoice(&text)?;
        let (mut best, mut refused): (Option<InvoiceQuote>, Option<licash_wallet::Error>) =
            (None, None);
        for (url, info) in self.wallet.hubs()? {
            if info.lightning.is_none() {
                continue;
            }
            match self.wallet.invoice_fee(&url, &invoice.request) {
                Ok(fee) if best.as_ref().is_none_or(|b| fee < b.fee) => {
                    best = Some(InvoiceQuote {
                        hub: url,
                        amount: invoice.amount,
                        fee,
                        description: invoice.description.clone(),
                    })
                }
                Ok(_) => {}
                // Lacking notes says the most: what is needed.
                Err(e)
                    if refused.is_none()
                        || matches!(e, licash_wallet::Error::Insufficient { .. }) =>
                {
                    refused = Some(e)
                }
                Err(_) => {}
            }
        }
        best.ok_or_else(|| match refused {
            Some(e) => e.into(),
            None => LicashError::invalid("no hub you trust pays Lightning invoices"),
        })
    }

    /// Pay the Lightning invoice `text` through `hub`: done, or under way
    /// until [`Wallet::resume`] finds it paid or refunded.
    pub fn pay_invoice(&self, text: String, hub: String) -> Result<Activity, LicashError> {
        let invoice = self.wallet.read_invoice(&text)?;
        let pay = self.wallet.pay_invoice(&hub, &invoice.request);
        let lock = self.runtime.block_on(pay)?;
        Ok(lightning_activity(&lock, &invoice.request))
    }

    /// Ask the hub trusted taking `amount` over Lightning for least for an
    /// invoice paying the wallet `amount` in notes, its fee on top for the
    /// payer. Followed by [`Wallet::follow_invoice`] while shown, and by
    /// [`Wallet::resume`]: its notes are taken once it is paid.
    pub fn ask_invoice(&self, amount: u64) -> Result<LightningInvoice, LicashError> {
        let (mut best, mut any): (Option<(u64, String)>, bool) = (None, false);
        for (url, info) in self.wallet.hubs()? {
            let Some(lightning) = &info.lightning else {
                continue;
            };
            any = true;
            if !(lightning.min_receive..=lightning.max_receive).contains(&amount) {
                continue;
            }
            let fee = lightning.receive.due(amount);
            if best.as_ref().is_none_or(|(least, _)| fee < *least) {
                best = Some((fee, url));
            }
        }
        let (_, url) = best.ok_or_else(|| {
            LicashError::invalid(match any {
                true => format!("no hub you trust takes {amount} sat over Lightning"),
                false => "no hub you trust takes payments over Lightning".into(),
            })
        })?;
        let asked = self.runtime.block_on(self.wallet.invoice(&url, amount));
        self.invoice_now(asked)
    }

    /// Take on the invoice of `hash`, as [`Wallet::ask_invoice`] asked for
    /// it: have its hub make it, and take its notes once it is paid. Where
    /// it stands.
    pub fn follow_invoice(&self, hash: String) -> Result<LightningInvoice, LicashError> {
        let followed = self.runtime.block_on(self.wallet.follow_invoice(&hash));
        self.invoice_now(followed)
    }

    /// What the wallet holds at each hub it trusts.
    pub fn holdings(&self) -> Result<Vec<Holding>, LicashError> {
        Ok(self.wallet.balance()?.into_iter().map(holding).collect())
    }

    /// What the wallet holds at hubs it only visited to pay a payee there,
    /// left over from a swap: none of its balance until the owner adds the
    /// hub ([`Wallet::add_hub`]). Hubs holding nothing are left out.
    pub fn visited(&self) -> Result<Vec<Holding>, LicashError> {
        let held = self.wallet.visited_balance()?.into_iter();
        let held = held.filter(|b| b.available > 0 || b.pending > 0);
        Ok(held.map(holding).collect())
    }

    /// How many notes of each denomination it can pay with, at each hub.
    pub fn notes(&self) -> Result<Vec<Notes>, LicashError> {
        let notes = self.wallet.notes()?.into_iter();
        Ok(notes
            .map(|n| Notes {
                hub: n.hub,
                denomination: n.denomination,
                count: n.count,
            })
            .collect())
    }

    /// How paying `amount`, or what it asks, to the payment code `code` in
    /// notes goes, and all it costs: at a hub it names that the wallet
    /// trusts, else once a swap from another brings what it lacks there,
    /// else so at a hub the code says where to reach, visited, proving it
    /// is the one the code names. [`Wallet::pay`] pays it so.
    pub fn pay_quote(&self, code: String, amount: Option<u64>) -> Result<PayQuote, LicashError> {
        let text = code.trim().to_owned();
        let code: PaymentCode = text.parse().map_err(LicashError::invalid)?;
        code.check(self.wallet.network())?;
        let plan = licash_nostr::wallet::plan(&self.wallet, &code, amount, &self.route);
        let plan = match self.runtime.block_on(plan) {
            Ok(plan) => plan,
            Err(e) => {
                // Lacking notes says the most: what is needed.
                self.enough(&code, amount)?;
                return Err(LicashError::from_any(e, |reason| LicashError::Invalid {
                    reason,
                }));
            }
        };
        let quote = PayQuote {
            hub: plan.hub.clone(),
            amount: plan.amount,
            fee: plan.fees(),
            swap_from: plan.swap.as_ref().map(|s| s.from.clone()),
            swapped: plan.swap.as_ref().map_or(0, |s| s.provider.quote.amount),
            trusted: plan.trusted,
        };
        *self.planned.lock().unwrap_or_else(PoisonError::into_inner) = Some((text, plan));
        Ok(quote)
    }

    /// Pay `amount`, or what it asks, to the payment code `code`, in notes:
    /// as [`Wallet::pay_quote`] quoted it last, else at a hub it names that
    /// the wallet trusts and holds enough at. To the payee's mailbox, or
    /// for them to receive by hand.
    pub fn pay(&self, code: String, amount: Option<u64>) -> Result<Paid, LicashError> {
        let text = code.trim().to_owned();
        let code: PaymentCode = text.parse().map_err(LicashError::invalid)?;
        let planned = self
            .planned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let planned = planned
            .filter(|(quoted, plan)| *quoted == text && Some(plan.amount) == amount.or(code.amount))
            .map(|(_, plan)| plan);
        if planned.is_none() {
            self.enough(&code, amount)?;
        }
        let _delivering = self.delivering();
        // Kept before paying: a payment its hub does not answer is made
        // later, and sent to the mailbox then.
        let delivery = code.licash.as_ref().and_then(|licash| {
            Some(Delivery {
                payee: licash.address,
                amount: amount.or(code.amount)?,
                mailbox: licash.mailbox?,
                at: deliveries::now(),
                send: None,
            })
        });
        if let Some(delivery) = &delivery {
            let mut all = deliveries::load(&self.dir);
            all.push(delivery.clone());
            deliveries::save(&self.dir, &all).map_err(LicashError::failed)?;
        }
        let route = self.route.clone();
        let paid = match &planned {
            Some(plan) => self.runtime.block_on(licash_nostr::wallet::pay_planned(
                &self.wallet,
                &code,
                plan,
                || Ok(route),
            )),
            None => self.runtime.block_on(licash_nostr::wallet::pay(
                &self.wallet,
                &code,
                amount,
                || Ok(route),
            )),
        };
        if let Some(delivery) = &delivery {
            self.settle(delivery, &paid);
        }
        let paid =
            paid.map_err(|e| LicashError::from_any(e, |reason| LicashError::Invalid { reason }))?;
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
        let _delivering = self.delivering();
        // Moves kept before their hub answered, by earlier versions: none
        // paid them, as no address was named, and a move asks anew.
        for deposit in self.wallet.deposits()? {
            if deposit.terms.is_none() && deposit.state == DepositState::Open {
                self.wallet.abandon_deposit(&deposit.hash)?;
            }
        }
        let resumed = self.runtime.block_on(self.wallet.resume())?;
        for (id, reason) in &resumed.abandoned {
            tracing::warn!(%id, %reason, "given up");
        }
        for (id, reason) in &resumed.pending {
            tracing::info!(%id, %reason, "still pending");
        }
        self.unlink();
        self.deliver();
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
        let me = self.wallet.address();
        // A payment to itself only makes notes of other denominations.
        let sends = self.wallet.sends()?.into_iter().filter(|s| s.payee != me);
        for sent in sends {
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
        let invoices = self.wallet.invoices()?;
        // A move's notes, and an invoice's, are received under its lock:
        // shown as the move or the invoice.
        let locks = deposits.iter().filter_map(|d| d.lock.as_ref());
        let locks = locks.chain(invoices.iter().filter_map(|i| i.lock.as_ref()));
        let locks: Vec<&String> = locks.collect();
        let receipts = self.wallet.receipts()?.into_iter();
        let receipts = receipts.filter(|r| !locks.contains(&&r.id));
        history.extend(receipts.map(|r| received_activity(&r)));
        // An invoice is a payment once paid: its hub locked notes to it.
        let paid = invoices.iter().filter(|i| i.lock.is_some());
        history.extend(paid.map(invoice_activity));
        // A move its hub never answered moved nothing.
        let answered = deposits
            .iter()
            .filter(|d| d.terms.is_some() || d.state == DepositState::Open);
        history.extend(answered.map(moved_activity));
        for lock in self.wallet.locks()? {
            if let Some(invoice) = &lock.invoice {
                history.push(lightning_activity(&lock, invoice));
            }
        }
        history.extend(payouts::activities(&self.wallet)?);
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

    /// More of the line of the history of `kind` and `id`: what it cost
    /// and where it stands on Bitcoin, as far as the wallet looked. A coin
    /// the wallet does not hold is one on its way, in no block yet.
    pub fn details(&self, kind: Kind, id: String) -> Result<Details, LicashError> {
        let unknown = || LicashError::invalid(format!("nothing in the history is {id}"));
        match kind {
            Kind::Sent => {
                let sent = self.wallet.sends()?.into_iter().find(|s| s.id == id);
                let hub = sent.ok_or_else(unknown)?.hub;
                Ok(Details::Notes { hub })
            }
            Kind::Received => {
                let got = self.wallet.receipts()?.into_iter().find(|r| r.id == id);
                let hub = got.ok_or_else(unknown)?.hub;
                Ok(Details::Notes { hub })
            }
            Kind::Lightning => {
                let lock = self.wallet.locks()?.into_iter().find(|l| l.id == id);
                let lock = lock.filter(|l| l.invoice.is_some()).ok_or_else(unknown)?;
                let (preimage, failure) = match &lock.state {
                    licash_wallet::LockState::Paid(p) => (Some(hex::encode(p)), None),
                    licash_wallet::LockState::Abandoned(why) => (None, Some(why.clone())),
                    _ => (None, lock.failure.clone()),
                };
                Ok(Details::Lightning {
                    hub: lock.hub,
                    invoice: lock.invoice.unwrap_or_default(),
                    preimage,
                    failure,
                })
            }
            Kind::Moved => {
                let deposit = self.wallet.deposits()?.into_iter().find(|d| d.hash == id);
                self.moved_details(deposit.ok_or_else(unknown)?)
            }
            Kind::Invoice => {
                let invoice = self.wallet.invoices()?.into_iter().find(|i| i.hash == id);
                let invoice = lightning_invoice(&invoice.ok_or_else(unknown)?, None);
                Ok(Details::Invoice {
                    hub: invoice.hub,
                    invoice: invoice.request,
                    asks: invoice.asks,
                    failure: invoice.reason,
                })
            }
            Kind::Payout => payouts::details(&self.wallet, &id),
            Kind::Onchain => {
                let outpoint: OutPoint = id.parse().map_err(|_| unknown())?;
                let mut coins = self.wallet.silent_coins()?.into_iter();
                let coin = coins.find(|c| c.outpoint == outpoint);
                let height = coin.as_ref().map(|c| c.height);
                let tx = Onchain {
                    txid: outpoint.txid.to_string(),
                    fee: None,
                    height,
                    confirmations: confirmations(height, self.wallet.silent_next()?),
                };
                Ok(Details::Coin {
                    tx,
                    vout: outpoint.vout,
                    change: coin.as_ref().is_some_and(|c| c.change),
                    moving: coin.and_then(|c| c.spending).map(|t| t.to_string()),
                })
            }
        }
    }

    /// Look for payments on Bitcoin in the next blocks of `chain`, and
    /// broadcast again the wallet's spends not in a block yet once caught
    /// up. Called until `next` passes `height`, once a hub is trusted. With
    /// the tweaks of a hub trusted serving them; without, of blocks no hub
    /// trusted serves tweaks of, for the fallback addresses and the coins
    /// held only.
    pub fn scan(&self, chain: Arc<dyn ChainSource>) -> Result<Scan, LicashError> {
        let hubs = self.wallet.hubs()?;
        if hubs.is_empty() {
            return Err(LicashError::invalid(
                "no hub trusted serves what finds payments on Bitcoin",
            ));
        }
        let hubs = hubs.into_iter();
        let mut tweaks: Vec<(String, u32)> =
            hubs.filter_map(|(u, i)| Some((u, i.tweaks?))).collect();
        tweaks.sort_by_key(|(_, first)| *first);
        let earliest = tweaks.first().map(|(_, first)| *first);
        let next = match self.wallet.silent_next()? {
            Some(next) => next,
            None => {
                // Recorded before any block is looked in.
                let from = self.first_block(chain.as_ref())?;
                self.wallet.watch_silent(None, from)?;
                tracing::info!(from, "looking for payments on Bitcoin");
                from
            }
        };
        let source = Chain(chain.clone());
        let mut failed = None;
        let mut scanned = None;
        // Any hub serving the blocks: their tweaks are the chain's.
        for (url, _) in tweaks.iter().filter(|(_, first)| *first <= next) {
            let scan = self.wallet.scan_silent(url.as_str(), &source, SCAN_BATCH);
            match self.runtime.block_on(scan) {
                Ok(done) => {
                    scanned = Some(done);
                    break;
                }
                Err(e @ licash_wallet::Error::Unavailable { .. }) => {
                    tracing::warn!(hub = %url, "no tweaks: {e}");
                    failed = Some(e);
                }
                Err(e) => return Err(e.into()),
            }
        }
        let scanned = match (scanned, failed) {
            (Some(scanned), _) => scanned,
            // Those serving them are unreachable: wait for one.
            (None, Some(e)) => return Err(e.into()),
            // None serves them: without tweaks, up to the first one does.
            (None, None) => {
                let max = earliest.map_or(SCAN_BATCH, |first| SCAN_BATCH.min(first - next));
                let scan = self.wallet.scan_silent(None, &source, max);
                self.runtime.block_on(scan)?
            }
        };
        if scanned.next > scanned.height {
            self.broadcast_again(chain.as_ref());
        }
        Ok(Scan {
            next: scanned.next,
            height: scanned.height,
            found: scanned.found.len() as u32,
            found_value: scanned.found.iter().map(|c| c.value).sum(),
        })
    }

    /// Payments on Bitcoin on their way to the fallback addresses handed
    /// out, as `chain`'s peers relay them: in no block the wallet looked
    /// in yet. Shown, never spent: only a scan finds coins.
    pub fn incoming(&self, chain: Arc<dyn ChainSource>) -> Result<Vec<Activity>, LicashError> {
        let mut txs = Vec::new();
        for script in self.wallet.fallback_scripts()? {
            let raw = chain
                .unconfirmed(script.into_bytes())
                .map_err(|e| LicashError::failed(format!("the light client: {e}")))?;
            let mut rest = raw.as_slice();
            while !rest.is_empty() {
                let tx: bitcoin::Transaction =
                    bitcoin::consensus::Decodable::consensus_decode(&mut rest)
                        .map_err(|e| LicashError::failed(format!("a relayed transaction: {e}")))?;
                txs.push(tx);
            }
        }
        let paying = self.wallet.unconfirmed_payments(&txs)?;
        Ok(paying
            .into_iter()
            .map(|(outpoint, value)| Activity {
                id: outpoint.to_string(),
                kind: Kind::Onchain,
                state: State::Pending,
                amount: value,
                fee: 0,
                detail: "waiting for a block".into(),
                at: None,
            })
            .collect())
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
    /// Where an invoice taken on stands: as kept, with why, while its hub
    /// is yet to answer, which it is asked again until it does.
    fn invoice_now(
        &self,
        followed: Result<licash_wallet::Invoice, licash_wallet::Error>,
    ) -> Result<LightningInvoice, LicashError> {
        match followed {
            Ok(invoice) => Ok(lightning_invoice(&invoice, None)),
            Err(licash_wallet::Error::Pending { id, reason }) => {
                let mut invoices = self.wallet.invoices()?.into_iter();
                match invoices.find(|i| i.hash == id) {
                    Some(invoice) => Ok(lightning_invoice(&invoice, Some(reason))),
                    None => Err(LicashError::Pending { reason }),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// A move's terms, and its payment: the wallet's own transaction paying
    /// its address, unless its hub saw another pay it.
    fn moved_details(&self, deposit: licash_wallet::Deposit) -> Result<Details, LicashError> {
        let next = self.wallet.silent_next()?;
        let hubs = self.wallet.hubs()?.into_iter();
        let mut hubs = hubs.filter(|(url, _)| *url == deposit.hub);
        let depth = hubs.next().and_then(|(_, i)| i.deposits.map(|d| d.depth));
        let terms = deposit.terms.as_ref();
        // Checked against the network when the hub answered.
        let script = terms
            .and_then(|t| t.address.parse::<bitcoin::Address<_>>().ok())
            .map(|a| a.assume_checked().script_pubkey());
        let spends = match &script {
            Some(script) => self.wallet.silent_spends_paying(script)?,
            None => Vec::new(),
        };
        let funding = deposit.funding;
        let spend = spends
            .into_iter()
            .rev()
            .find(|s| funding.is_none_or(|(outpoint, _)| outpoint.txid == s.tx.compute_txid()));
        let payment = match (spend, funding) {
            (Some(spend), _) => {
                // Its hub may have seen it confirm before the wallet looked.
                let height = spend.height.or(funding.map(|(_, height)| height));
                Some(Onchain {
                    txid: spend.tx.compute_txid().to_string(),
                    fee: spend.fee,
                    height,
                    confirmations: confirmations(height, next),
                })
            }
            (None, Some((outpoint, height))) => Some(Onchain {
                txid: outpoint.txid.to_string(),
                fee: None,
                height: Some(height),
                confirmations: confirmations(Some(height), next),
            }),
            (None, None) => None,
        };
        let refund = match &deposit.state {
            DepositState::Refunded(txid) => Some(txid.to_string()),
            _ => None,
        };
        Ok(Details::Moved {
            pays: terms.map(|t| t.pays),
            address: terms.map(|t| t.address.clone()),
            fund_by: terms.map(|t| t.fund_by),
            timeout: terms.map(|t| t.timeout),
            hub: deposit.hub,
            hash: deposit.hash,
            payment,
            depth,
            refund,
        })
    }

    fn delivering(&self) -> MutexGuard<'_, ()> {
        self.delivering
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// After paying to the mailbox of `delivery`: forgotten once sent
    /// there, else kept naming its payment if known, for [`Wallet::deliver`]
    /// to send once made, or again.
    fn settle(&self, delivery: &Delivery, paid: &anyhow::Result<licash_nostr::wallet::CodePaid>) {
        let send = match paid {
            Ok(paid) if paid.delivered.is_some() => None,
            Ok(paid) => Some(Some(paid.paid.id.clone())),
            Err(e) => match e.downcast_ref() {
                Some(licash_wallet::Error::Pending { id, .. }) => Some(Some(id.clone())),
                // Never made, refused or given up, unless it failed after:
                // what became of it is looked up.
                _ => Some(None),
            },
        };
        let mut all = deliveries::load(&self.dir);
        deliveries::forget(&mut all, delivery);
        if let Some(send) = send {
            all.push(Delivery {
                send,
                ..delivery.clone()
            });
        }
        if let Err(e) = deliveries::save(&self.dir, &all) {
            tracing::warn!(%e, "the payments to send to mailboxes");
        }
    }

    /// Send the payments kept for mailboxes that are made now to them, from
    /// what the wallet records: kept while under way or not sent, forgotten
    /// once sent, given up or never made, or not sent two weeks on.
    fn deliver(&self) {
        let all = deliveries::load(&self.dir);
        if all.is_empty() {
            return;
        }
        let (sends, hubs) = match (self.wallet.sends(), self.wallet.hubs()) {
            (Ok(sends), Ok(hubs)) => (sends, hubs),
            (Err(e), _) | (_, Err(e)) => return tracing::warn!(%e, "the payments made"),
        };
        let mut named: Vec<String> = all.iter().filter_map(|d| d.send.clone()).collect();
        let old = deliveries::now() - licash_nostr::mailbox::KEEP.as_secs() as i64;
        let mut kept = Vec::new();
        for mut delivery in all {
            if delivery.send.is_none() {
                // The app stopped while paying, or it failed: the oldest
                // payment to the payee since not given up, if any, no other
                // one's. Newest first, the sends are looked at from the end.
                let made = sends.iter().rev().find(|s| {
                    s.payee == delivery.payee
                        && s.amount == delivery.amount
                        && s.created_at >= delivery.at
                        && !matches!(s.state, licash_wallet::State::Abandoned(_))
                        && !named.contains(&s.id)
                });
                let Some(made) = made else { continue };
                named.push(made.id.clone());
                delivery.send = Some(made.id.clone());
            }
            let Some(send) = sends.iter().find(|s| delivery.send.as_ref() == Some(&s.id)) else {
                continue;
            };
            match send.state {
                licash_wallet::State::Offered | licash_wallet::State::Signed => {
                    kept.push(delivery);
                    continue;
                }
                licash_wallet::State::Done => {}
                _ => continue,
            }
            let relays = hubs.iter().find(|(url, _)| *url == send.hub);
            let relays = relays.map_or(&[][..], |(_, info)| info.relays.as_slice());
            match self.mail(&delivery.mailbox, &send.id, relays) {
                Ok(()) => {
                    let amount = delivery.amount;
                    tracing::info!(id = %send.id, amount, "a payment sent to its payee's mailbox");
                }
                Err(e) if delivery.at < old => {
                    let why = format!("{e:#}");
                    tracing::warn!(id = %send.id, %why, "a payment never sent to its payee's mailbox");
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    tracing::warn!(id = %send.id, %why, "a payment to send to its payee's mailbox, again later");
                    kept.push(delivery);
                }
            }
        }
        if let Err(e) = deliveries::save(&self.dir, &kept) {
            tracing::warn!(%e, "the payments to send to mailboxes");
        }
    }

    /// Send the payment `id` made to `mailbox`, on `relays`.
    fn mail(&self, mailbox: &[u8; 32], id: &str, relays: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(!relays.is_empty(), "its hub names no mailbox relays");
        let payment = serde_json::to_string(&self.wallet.payment(id)?)?;
        let mail = licash_nostr::mailbox::send(mailbox, &payment, relays, &self.route);
        let taken = self.runtime.block_on(mail)?;
        anyhow::ensure!(taken > 0, "no relay took it");
        Ok(())
    }

    /// Refused as more than held if the code names hubs trusted, some
    /// holding notes, none enough to pay `amount` or what it asks and the
    /// fee: what is needed and held at the one holding most. Holding none,
    /// paying it says where to pay on Bitcoin instead.
    fn enough(&self, code: &PaymentCode, amount: Option<u64>) -> Result<(), LicashError> {
        code.check(self.wallet.network())?;
        let (Some(licash), Some(amount)) = (&code.licash, amount.or(code.amount)) else {
            return Ok(());
        };
        if code.amount.is_some_and(|asked| asked != amount)
            || self.wallet.hub_to_pay(licash, amount)?.is_some()
        {
            return Ok(());
        }
        let held = self.wallet.balance()?;
        let mut most: Option<(u64, u64)> = None;
        for (url, info) in self.wallet.hubs()? {
            if !licash
                .hubs
                .contains(&licash_wallet::HubId::of(&info.address))
            {
                continue;
            }
            let available = held
                .iter()
                .find(|b| b.hub == url)
                .map_or(0, |b| b.available);
            let needed = amount.saturating_add(info.fees.spend.due(amount));
            if most.is_none_or(|(_, a)| available > a) {
                most = Some((needed, available));
            }
        }
        match most {
            Some((needed, available)) if available > 0 => {
                Err(LicashError::Insufficient { needed, available })
            }
            _ => Ok(()),
        }
    }

    /// Unlink what can be: what is left stays linked until the next try.
    fn unlink(&self) {
        if let Err(e) = self.runtime.block_on(self.wallet.unlink()) {
            tracing::warn!(%e, "some notes stay linked for now");
        }
    }

    /// The first block to look in for payments on Bitcoin: the chain's
    /// height when the wallet was made, if the app knew it. Else the first
    /// block claiming a time from a day before the wallet file was made
    /// on, a margin for block times and clocks: the app makes new seeds
    /// only, never paid before.
    fn first_block(&self, chain: &dyn ChainSource) -> Result<u32, LicashError> {
        let birthday = self.kept().birthday;
        if birthday > 0 {
            return Ok(birthday);
        }
        let since = self.wallet.created_at()? - DAY;
        let light = |e: ChainError| LicashError::failed(format!("the light client: {e}"));
        let height = chain.height().map_err(light)?;
        let (mut low, mut high) = (1, height + 1);
        while low < high {
            let mid = low + (high - low) / 2;
            if chain.time(mid).map_err(light)? < since {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(low)
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
        lightning_pay: info.lightning.as_ref().map(|l| l.max_pay),
        lock_fee: rate(info.fees.lock),
        refund_fee: rate(info.fees.refund),
        deposit_fee: info.deposits.as_ref().map(|d| rate(d.fee)),
        lightning: info.lightning.as_ref().map(|l| LightningTerms {
            pay: rate(l.pay),
            receive: rate(l.receive),
            receive_range: Range {
                min: l.min_receive,
                max: l.max_receive,
            },
        }),
    }
}

fn rate(fee: licash_api::Fee) -> Rate {
    Rate {
        base: fee.base,
        ppm: fee.ppm,
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

/// The line of the history of `lock`, paying `invoice`: what the invoice
/// asks, and the fee: the gateway's on top once paid, or while under way;
/// the hub's refund fee once refunded; none if given up.
fn lightning_activity(lock: &licash_wallet::Lock, invoice: &str) -> Activity {
    use licash_wallet::LockState;
    let asked = licash_wallet::Payable::read(invoice).map_or(lock.amount, |p| p.amount);
    let asked = asked.min(lock.amount);
    let expected = lock.amount - asked + lock.fee;
    let (state, detail, fee) = match &lock.state {
        LockState::Paid(_) => (
            State::Done,
            lock.hub.clone(),
            lock.cost.map_or(expected, |c| c.saturating_sub(asked)),
        ),
        LockState::Refunded => (
            State::Failed,
            lock.failure.clone().unwrap_or_else(|| "refunded".into()),
            lock.cost.unwrap_or(0),
        ),
        LockState::Abandoned(why) => (State::Failed, why.clone(), 0),
        _ => (State::Pending, lock.hub.clone(), expected),
    };
    Activity {
        id: lock.id.clone(),
        kind: Kind::Lightning,
        state,
        amount: asked,
        fee,
        detail,
        at: Some(lock.created_at),
    }
}

/// An invoice the wallet asked for, as it stands; `waiting`, why its hub
/// is yet to answer, if it is.
fn lightning_invoice(
    invoice: &licash_wallet::Invoice,
    waiting: Option<String>,
) -> LightningInvoice {
    use licash_wallet::InvoiceState;
    let (state, reason) = match &invoice.state {
        InvoiceState::Received => (State::Done, None),
        InvoiceState::Canceled(why) => (State::Failed, Some(why.clone())),
        _ => (State::Pending, waiting),
    };
    let request = invoice.request.clone();
    let asks = request.as_deref().map(licash_wallet::Payable::read);
    LightningInvoice {
        hash: invoice.hash.clone(),
        hub: invoice.hub.clone(),
        amount: invoice.amount,
        asks: asks.and_then(Result::ok).map(|p| p.amount),
        request,
        state,
        reason,
    }
}

/// The line of the history of an invoice paid to the wallet: its notes,
/// when they came, and no fee, which the payer pays.
fn invoice_activity(invoice: &licash_wallet::Invoice) -> Activity {
    let shown = lightning_invoice(invoice, None);
    Activity {
        id: shown.hash,
        kind: Kind::Invoice,
        state: shown.state,
        amount: shown.amount,
        fee: 0,
        detail: shown.reason.unwrap_or(shown.hub),
        at: Some(invoice.received_at.unwrap_or(invoice.created_at)),
    }
}

/// The blocks confirming a transaction in block `height`, the wallet
/// having looked in those before `next`.
fn confirmations(height: Option<u32>, next: Option<u32>) -> u32 {
    match (height, next) {
        (Some(height), Some(next)) => next.saturating_sub(height).max(1),
        (Some(_), None) => 1,
        (None, _) => 0,
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
