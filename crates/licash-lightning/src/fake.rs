//! An in-memory Lightning node, for tests: it holds the invoices others
//! pay it, and pays invoices of other nodes, ending as the test says.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bitcoin::{
    hashes::{sha256, Hash},
    secp256k1::{PublicKey, Secp256k1, SecretKey},
};
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder, PaymentSecret};

use crate::{Attempt, Call, Held, HeldState, Liquidity, Node, NodeInfo, Status};

/// How a payment the node makes ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Paid, buying this preimage.
    Pay([u8; 32]),
    Fail(String),
    /// In flight until resolved ([`Fake::resolve`]).
    Hang,
}

/// A payment the node was asked to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub request: String,
    pub fee_limit: u64,
    pub cltv_limit: u32,
}

pub struct Fake {
    state: Mutex<State>,
    key: SecretKey,
    /// The key of the other nodes' invoices.
    payee: SecretKey,
}

#[derive(Default)]
struct State {
    height: u32,
    block_hash: [u8; 32],
    held: HashMap<[u8; 32], (Held, u64)>,
    attempts: HashMap<[u8; 32], Attempt>,
    outcomes: HashMap<[u8; 32], Outcome>,
    sent: Vec<Sent>,
    down: bool,
    /// Ample unless set.
    liquidity: Option<Liquidity>,
}

impl Default for Fake {
    fn default() -> Self {
        Self::new()
    }
}

impl Fake {
    pub fn new() -> Self {
        Self {
            state: Mutex::default(),
            key: SecretKey::from_slice(&[1; 32]).expect("a key"),
            payee: SecretKey::from_slice(&[2; 32]).expect("a key"),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn pubkey(&self) -> String {
        hex::encode(PublicKey::from_secret_key(&Secp256k1::new(), &self.key).serialize())
    }

    /// Follow the chain to `height`, block `hash` in internal byte order.
    pub fn tip(&self, height: u32, hash: [u8; 32]) {
        let mut state = self.state();
        (state.height, state.block_hash) = (height, hash);
    }

    /// Pay `local` and be paid `remote` over its channels at most.
    pub fn channels(&self, local: u64, remote: u64) {
        self.state().liquidity = Some(Liquidity { local, remote });
    }

    /// Answer nothing while `down`.
    pub fn down(&self, down: bool) {
        self.state().down = down;
    }

    /// An invoice of another node for `msat` under `hash`, of final delta
    /// `cltv`, whose payment ends as `outcome`.
    pub fn invoice(&self, msat: u64, hash: [u8; 32], cltv: u64, outcome: Outcome) -> String {
        self.state().outcomes.insert(hash, outcome);
        bolt11(&self.payee, msat, hash, cltv, 3600, None)
    }

    /// The payment under way of `hash` ends as `outcome`.
    pub fn resolve(&self, hash: [u8; 32], outcome: Outcome) {
        let mut state = self.state();
        if let Some(attempt) = state.attempts.get_mut(&hash) {
            attempt.status = status(&outcome);
        }
        state.outcomes.insert(hash, outcome);
    }

    /// Forget the attempts at `hash`, as a node restored from a backup.
    pub fn forget(&self, hash: [u8; 32]) {
        self.state().attempts.remove(&hash);
    }

    /// The payments the node was asked to start.
    pub fn sent(&self) -> Vec<Sent> {
        self.state().sent.clone()
    }

    /// Another node pays the held invoice under `hash`, its HTLCs expiring
    /// at `expiry`.
    pub fn pay_held(&self, hash: [u8; 32], expiry: u32) {
        assert!(self.try_pay_held(hash, expiry), "an open invoice");
    }

    /// Another node tries to pay the held invoice under `hash`, its HTLCs
    /// expiring at `expiry`: whether the node accepted them, which it does
    /// for an open invoice only, failing them back otherwise.
    pub fn try_pay_held(&self, hash: [u8; 32], expiry: u32) -> bool {
        let mut state = self.state();
        let (held, _) = state.held.get_mut(&hash).expect("a held invoice");
        if held.state != HeldState::Open {
            return false;
        }
        (held.state, held.min_expiry) = (HeldState::Accepted, Some(expiry));
        true
    }

    /// The held invoice under `hash`: its state and amount.
    pub fn held(&self, hash: [u8; 32]) -> Option<(HeldState, u64)> {
        let state = self.state();
        state
            .held
            .get(&hash)
            .map(|(held, msat)| (held.state, *msat))
    }

    /// The final delta of the held invoice under `hash`.
    pub fn held_cltv(&self, hash: [u8; 32]) -> Option<u64> {
        let state = self.state();
        let (held, _) = state.held.get(&hash)?;
        let invoice: Bolt11Invoice = held.request.parse().ok()?;
        Some(invoice.min_final_cltv_expiry_delta())
    }

    fn up(&self) -> Result<MutexGuard<'_, State>, String> {
        let state = self.state();
        if state.down {
            return Err("the fake node is down".into());
        }
        Ok(state)
    }
}

fn status(outcome: &Outcome) -> Status {
    match outcome {
        Outcome::Pay(preimage) => Status::Succeeded(*preimage),
        Outcome::Fail(reason) => Status::Failed(reason.clone()),
        Outcome::Hang => Status::InFlight,
    }
}

fn since_epoch() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// A regtest invoice by `key`.
fn bolt11(
    key: &SecretKey,
    msat: u64,
    hash: [u8; 32],
    cltv: u64,
    expiry: u64,
    description: Option<[u8; 32]>,
) -> String {
    let secp = Secp256k1::new();
    let builder = InvoiceBuilder::new(Currency::Regtest);
    let builder = match description {
        Some(d) => builder.description_hash(sha256::Hash::from_byte_array(d)),
        None => builder.description(String::new()),
    };
    builder
        .payment_hash(sha256::Hash::from_byte_array(hash))
        .payment_secret(PaymentSecret([42; 32]))
        .duration_since_epoch(since_epoch())
        .min_final_cltv_expiry_delta(cltv)
        .amount_milli_satoshis(msat)
        .expiry_time(Duration::from_secs(expiry))
        .build_signed(|message| secp.sign_ecdsa_recoverable(message, key))
        .expect("an invoice")
        .to_string()
}

impl Node for Fake {
    fn info(&self) -> Call<'_, NodeInfo> {
        let info = self.up().map(|state| NodeInfo {
            pubkey: self.pubkey(),
            height: state.height,
            block_hash: state.block_hash,
            currency: Currency::Regtest,
        });
        Box::pin(async move { info })
    }

    fn liquidity(&self) -> Call<'_, Liquidity> {
        let liquidity = self.up().map(|state| {
            state.liquidity.unwrap_or(Liquidity {
                local: u64::MAX,
                remote: u64::MAX,
            })
        });
        Box::pin(async move { liquidity })
    }

    fn pay(&self, request: String, fee_limit: u64, cltv_limit: u32, _timeout: u32) -> Call<'_, ()> {
        let paid = self.up().and_then(|mut state| {
            let invoice: Bolt11Invoice = request.parse().map_err(|e| format!("{e}"))?;
            let hash = *invoice.payment_hash().as_ref();
            state.sent.push(Sent {
                request,
                fee_limit,
                cltv_limit,
            });
            // A failed payment may be tried again, as LND does; one under
            // way or made is no error.
            if let Some(Status::InFlight | Status::Succeeded(_)) =
                state.attempts.get(&hash).map(|a| &a.status)
            {
                return Ok(());
            }
            let outcome = match state.outcomes.get(&hash) {
                _ if invoice.min_final_cltv_expiry_delta() > u64::from(cltv_limit) => {
                    Outcome::Fail("FAILURE_REASON_NO_ROUTE".into())
                }
                Some(outcome) => outcome.clone(),
                None => Outcome::Fail("FAILURE_REASON_NO_ROUTE".into()),
            };
            let created_ns = u64::try_from(since_epoch().as_nanos()).unwrap_or(u64::MAX);
            let status = status(&outcome);
            state.attempts.insert(hash, Attempt { status, created_ns });
            Ok(())
        });
        Box::pin(async move { paid })
    }

    fn payment(&self, hash: [u8; 32]) -> Call<'_, Option<Attempt>> {
        let attempt = self.up().map(|state| state.attempts.get(&hash).cloned());
        Box::pin(async move { attempt })
    }

    fn hold(
        &self,
        hash: [u8; 32],
        msat: u64,
        cltv_expiry: u32,
        expiry: u64,
        description: Option<[u8; 32]>,
    ) -> Call<'_, String> {
        let held = self.up().and_then(|mut state| {
            if state.held.contains_key(&hash) {
                return Err("invoice with payment hash already exists".into());
            }
            let request = bolt11(
                &self.key,
                msat,
                hash,
                cltv_expiry.into(),
                expiry,
                description,
            );
            let held = Held {
                state: HeldState::Open,
                request: request.clone(),
                min_expiry: None,
            };
            state.held.insert(hash, (held, msat));
            Ok(request)
        });
        Box::pin(async move { held })
    }

    fn invoice(&self, hash: [u8; 32]) -> Call<'_, Option<Held>> {
        let held = self
            .up()
            .map(|state| state.held.get(&hash).map(|(h, _)| h.clone()));
        Box::pin(async move { held })
    }

    fn settle(&self, preimage: [u8; 32]) -> Call<'_, ()> {
        let hash = sha256::Hash::hash(&preimage).to_byte_array();
        let settled = self
            .up()
            .and_then(|mut state| match state.held.get_mut(&hash) {
                Some((held, _)) if held.state == HeldState::Accepted => {
                    held.state = HeldState::Settled;
                    Ok(())
                }
                _ => Err("invoice is not accepted".into()),
            });
        Box::pin(async move { settled })
    }

    fn cancel(&self, hash: [u8; 32]) -> Call<'_, ()> {
        let canceled = self
            .up()
            .and_then(|mut state| match state.held.get_mut(&hash) {
                Some((held, _)) if held.state != HeldState::Settled => {
                    held.state = HeldState::Canceled;
                    Ok(())
                }
                _ => Err("invoice already settled".into()),
            });
        Box::pin(async move { canceled })
    }
}
