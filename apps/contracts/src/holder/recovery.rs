//! A holder's hub-proves exit. The filing and request are pinned before
//! one-time signing; each transaction is durable before broadcast. The
//! holder verifies the hub's proof, but never computes a recovery proof.
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use bitcoin::{Amount, FeeRate, OutPoint, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use licash::claim::{self, Branch, Item, UnsignedClaim};
use licash::recovery::{Filing, Publication, Witness as FilingWitness};
use licash::request::Request;
use licash_contract::graph::{link_args, HubProves, PURSE_VOUT};
use licash_contract::{
    dispute::{self, presign, Payout},
    keys::HolderKeys,
    opening::{Context, Holder, Opening},
};
use licash_onchain::deadline;
use licash_wallet::{first_spend, ClaimNote, ClaimSlot, LockClaim, Transport, Wallet};
use lichen_contract::Role;
use serde::{Deserialize, Serialize};

use super::activation;
use super::claims::{self, ClaimTerms};
use super::store::{RecoveryRow, Store};
use super::{Chain, Fate};

/// The format of a recovery's stored state.
const FORMAT: i64 = 2;
const TIMEOUT_FEE_MARGIN: u64 = 2;
const OVERLAP: u32 = 3;
const DUST: Amount = Amount::from_sat(330);
/// About a challenge's virtual size, its revealed secrets among it (20,717
/// vB measured), and its fee child's.
const CHALLENGE_VB: u64 = 21_000;

/// The coin of hers to set aside, at the forced close of a contract, for
/// a challenge's fee child should the hub's assertion be false: for twice
/// the challenge's fee at `rate`, her rate, the rate rising meanwhile.
pub fn challenge_reserve(rate: FeeRate) -> Amount {
    rate.fee_vb(CHALLENGE_VB * 2).unwrap_or(Amount::MAX)
}

/// The txid of no transaction her coin for the challenge of contract `id`
/// is set aside under ([`Wallet::reserve_silent`]), until its fee child
/// takes it or no challenge is left to make: a coin of hers spending
/// nothing else under it is that coin, set aside.
pub fn challenge_marker(id: &[u8; 32]) -> Txid {
    use bitcoin::hashes::{sha256, Hash};
    let hash = sha256::Hash::hash(&[b"licash/challenge-coin/".as_slice(), id].concat());
    Txid::from_byte_array(hash.to_byte_array())
}

/// Whether her coins hold one to set aside for a challenge at `rate`
/// ([`challenge_reserve`]): the least it must hold if not, for her to be
/// told before she forces a close.
pub fn challenge_shortfall<T: Transport>(
    wallet: &Wallet<T>,
    dir: &std::path::Path,
    rate: FeeRate,
) -> Result<Option<Amount>> {
    let need = challenge_reserve(rate);
    let coins = super::coins(wallet, dir)?;
    // Her publication may need one of her coins too, the smallest that
    // pays its fee: one besides it.
    let enough = coins.iter().filter(|c| c.value >= need.to_sat()).count();
    let held = enough >= 2 || (enough == 1 && coins.len() >= 2);
    Ok((!held).then_some(need))
}

fn rescan(from: u32, tip: u32) -> u32 {
    tip.saturating_add(1).saturating_sub(OVERLAP).max(from)
}

/// Whether a transaction mined at `height` is `depth` deep at `tip`, one
/// at least: a reorganization no longer expected to take it out.
fn deep(height: u32, tip: u32, depth: u32) -> bool {
    tip.saturating_add(1).saturating_sub(height) >= depth.max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Claiming,
    Claimed,
    Irrevocable,
    Signing,
    Published,
    Requested,
    TimingOut,
    TimedOut,
    Sweeping,
    Done,
    Answered,
    Lost,
    GivingUp,
    Asserted,
    Succeeding,
    Challenging,
    Challenged,
    Paid,
}
impl Stage {
    const ALL: [Self; 18] = [
        Self::Claiming,
        Self::Claimed,
        Self::Irrevocable,
        Self::Signing,
        Self::Published,
        Self::Requested,
        Self::TimingOut,
        Self::TimedOut,
        Self::Sweeping,
        Self::Done,
        Self::Answered,
        Self::Lost,
        Self::GivingUp,
        Self::Asserted,
        Self::Succeeding,
        Self::Challenging,
        Self::Challenged,
        Self::Paid,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claiming => "claiming",
            Self::Claimed => "claimed",
            Self::Irrevocable => "irrevocable",
            Self::Signing => "signing",
            Self::Published => "published",
            Self::Requested => "requested",
            Self::TimingOut => "timing-out",
            Self::TimedOut => "timed-out",
            Self::Sweeping => "sweeping",
            Self::Done => "done",
            Self::Answered => "answered",
            Self::Lost => "lost",
            Self::GivingUp => "giving-up",
            Self::Asserted => "asserted",
            Self::Succeeding => "succeeding",
            Self::Challenging => "challenging",
            Self::Challenged => "challenged",
            Self::Paid => "paid",
        }
    }
    fn parse(text: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|s| s.as_str() == text)
            .with_context(|| format!("a recovery at stage {text}, unknown"))
    }
    pub fn over(self) -> bool {
        matches!(self, Self::Done | Self::Lost)
    }
    pub fn disputed(self) -> bool {
        matches!(
            self,
            Self::Answered
                | Self::Asserted
                | Self::Succeeding
                | Self::Challenging
                | Self::Challenged
                | Self::Paid
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovery {
    pub id: [u8; 32],
    pub stage: Stage,
    pub irrevocable: bool,
    /// What her claim recovers, in sat (her notes' face value, the cap at
    /// most), and how many notes it holds.
    pub claimed: u64,
    /// The hub's proposed reduced principal, not an entitlement until
    /// verified. `received` is the actual confirmed sweep output.
    pub corrected: Option<u64>,
    pub received: Option<u64>,
    pub notes: usize,
    pub request: Option<Txid>,
    pub publication: Option<Txid>,
    pub requested_at: Option<u32>,
    pub timeout: Option<Txid>,
    pub timed_out_at: Option<u32>,
    pub sweep: Option<Txid>,
    pub swept_at: Option<u32>,
    pub next_at: Option<u32>,
    pub error: Option<String>,
    pub response: Option<Txid>,
    pub deadline: Option<u32>,
    /// Whether the hub's observed assertion verifies.
    pub proved: bool,
    pub assertion: Option<Txid>,
    pub asserted_at: Option<u32>,
    pub challenge: Option<Txid>,
    pub payout: Option<Txid>,
    pub paid_at: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct State {
    notes: Vec<String>,
    amount: u32,
    /// The whole public Filing, not just its signed claim.
    filing: Option<String>,
    #[serde(with = "consensus")]
    r: Option<Transaction>,
    #[serde(with = "consensus")]
    p: Option<Transaction>,
    #[serde(with = "consensus_all")]
    prevouts: Vec<TxOut>,
    f_height: Option<u32>,
    r_height: Option<u32>,
    p_height: Option<u32>,
    /// Her claim's slot taken by the hub's reclaim, P never published:
    /// her terminals' fee coin is her purse, and `success` the hub's to
    /// post.
    reclaimed: bool,
    #[serde(with = "consensus")]
    response: Option<Transaction>,
    response_height: Option<u32>,
    reduced: Option<u32>,
    deadline: Option<u32>,
    #[serde(with = "consensus")]
    assertion: Option<Transaction>,
    assertion_height: Option<u32>,
    verified: Option<bool>,
    #[serde(with = "consensus")]
    challenge: Option<Transaction>,
    challenge_height: Option<u32>,
    /// The challenge's fee child, the last made, its inputs of her wallet
    /// with a placeholder signature: signed again at each broadcast,
    /// which sets its coins aside again.
    #[serde(with = "consensus", skip_serializing_if = "Option::is_none")]
    challenge_child: Option<Transaction>,
    #[serde(with = "consensus")]
    payout: Option<Transaction>,
    payout_name: Option<String>,
    payout_height: Option<u32>,
    #[serde(with = "consensus")]
    sweep: Option<Transaction>,
    swept_height: Option<u32>,
    from: Option<u32>,
    next_at: Option<u32>,
    /// What her publication may pay in fees in all, set by her
    /// ([`allow_publication_fee`]): absent, what its reserve hands it.
    #[serde(skip_serializing_if = "Option::is_none")]
    p_allowance: Option<u64>,
}
impl State {
    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("recovery state")
    }
    fn decode(row: &RecoveryRow) -> Result<Self> {
        ensure!(
            row.format == FORMAT,
            "unsupported recovery format {}",
            row.format
        );
        Stage::parse(&row.stage)?;
        let state: Self = serde_json::from_slice(&row.state)?;
        ensure!(
            state.payout.is_some() == state.payout_name.is_some(),
            "payout provenance missing"
        );
        Ok(state)
    }
}
mod consensus {
    use bitcoin::consensus::{
        encode::{deserialize_hex, serialize_hex},
        Decodable, Encodable,
    };
    use serde::{de::Error, Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<T: Encodable, S: Serializer>(v: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
        v.as_ref().map(serialize_hex).serialize(s)
    }
    pub fn deserialize<'de, T: Decodable, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|h| deserialize_hex(&h).map_err(D::Error::custom))
            .transpose()
    }
}
mod consensus_all {
    use bitcoin::consensus::{
        encode::{deserialize_hex, serialize_hex},
        Decodable, Encodable,
    };
    use serde::{de::Error, Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<T: Encodable, S: Serializer>(v: &[T], s: S) -> Result<S::Ok, S::Error> {
        v.iter().map(serialize_hex).collect::<Vec<_>>().serialize(s)
    }
    pub fn deserialize<'de, T: Decodable, D: Deserializer<'de>>(d: D) -> Result<Vec<T>, D::Error> {
        Vec::<String>::deserialize(d)?
            .iter()
            .map(|h| deserialize_hex(h).map_err(D::Error::custom))
            .collect()
    }
}

pub(super) fn validate_backup(store: &Store) -> Result<()> {
    for row in store.recoveries()? {
        State::decode(&row)?;
    }
    Ok(())
}

pub async fn force_close<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &std::path::Path,
    id: &[u8; 32],
    rate: FeeRate,
) -> Result<Recovery> {
    let store = Store::existing(dir)?.context("no contracts")?;
    let contract = store
        .list()?
        .into_iter()
        .find(|c| &c.id == id)
        .context("no such contract")?;
    if store
        .recovery(id)?
        .is_some_and(|r| r.stage == Stage::GivingUp.as_str())
    {
        finish_giving_up(wallet, &store, id)?;
    }
    if store.recovery(id)?.is_none() {
        let tip = chain.height().await.map_err(|e| anyhow!(e))?;
        ensure!(
            contract.fate == Some(Fate::Live),
            "its F is not buried in the chain yet"
        );
        // Closed early too: a fold that never confirmed leaves it to
        // recover, and one that did makes the recovery give back.
        ensure!(
            tip < contract.assert_height,
            "its protection ended at block {}",
            contract.assert_height
        );
        store.start_recovery(
            id,
            FORMAT,
            Stage::Claiming.as_str(),
            &State::default().encode(),
        )?;
    }
    Run::load(wallet, chain, &store, dir, id, rate)
        .await?
        .steps()
        .await?;
    view(&store.recovery(id)?.context("given up")?)
}
pub async fn advance<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &std::path::Path,
    rate: FeeRate,
) -> Result<Vec<Recovery>> {
    let Some(store) = Store::existing(dir)? else {
        return Ok(vec![]);
    };
    // Before a recovery claims them: as provable as they can be.
    if let Err(e) = activation::keep(wallet, chain, dir, rate).await {
        tracing::warn!("keeping the locks owed to her provable: {e:#}");
    }
    for row in store.recoveries()? {
        if Stage::parse(&row.stage).is_ok_and(Stage::over) {
            continue;
        }
        if row.stage == Stage::GivingUp.as_str() {
            if let Err(e) = finish_giving_up(wallet, &store, &row.id) {
                store.recovery_error(&row.id, Some(&format!("{e:#}")))?;
            }
            continue;
        }
        let result = match Run::load(wallet, chain, &store, dir, &row.id, rate).await {
            Ok(run) => run.steps().await,
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            tracing::warn!(id = %crate::hex_id(&row.id), "recovering a contract: {e:#}");
            store.recovery_error(&row.id, Some(&format!("{e:#}")))?;
        }
    }
    list(&store)
}
pub fn recoveries(dir: &std::path::Path) -> Result<Vec<Recovery>> {
    match Store::existing(dir)? {
        Some(store) => list(&store),
        None => Ok(vec![]),
    }
}
pub fn give_up<T: Transport>(
    wallet: &Wallet<T>,
    dir: &std::path::Path,
    id: &[u8; 32],
) -> Result<()> {
    let store = Store::existing(dir)?.context("no contracts")?;
    if store.recovery(id)?.is_none() {
        // Given up already, asked twice (a second tap waiting behind the
        // first, say): nothing left to give back.
        ensure!(
            store.list()?.iter().any(|c| &c.id == id),
            "no such contract"
        );
        return Ok(());
    }
    ensure!(
        store.revoke_recovery(id, Stage::GivingUp.as_str(), Some("given up"))?,
        "past giving up: her request is signed"
    );
    finish_giving_up(wallet, &store, id)
}
fn finish_giving_up<T: Transport>(wallet: &Wallet<T>, store: &Store, id: &[u8; 32]) -> Result<()> {
    wallet.unclaim_notes(id)?;
    // Nothing set aside under it, or freed already: no error.
    let _ = wallet.abandon_silent(&challenge_marker(id));
    store.drop_recovery(id)?;
    Ok(())
}
fn list(store: &Store) -> Result<Vec<Recovery>> {
    Ok(store
        .recoveries()?
        .iter()
        .filter_map(|r| match view(r) {
            Ok(view) => Some(view),
            Err(e) => {
                tracing::warn!(id = %crate::hex_id(&r.id), "a recovery not read: {e:#}");
                None
            }
        })
        .collect())
}
fn view(row: &RecoveryRow) -> Result<Recovery> {
    let state = State::decode(row)?;
    let stage = Stage::parse(&row.stage)?;
    let txid = |tx: &Option<Transaction>| tx.as_ref().map(Transaction::compute_txid);
    let timed = state
        .payout_name
        .as_deref()
        .is_some_and(|n| matches!(n, "request_timeout" | "respond_timeout" | "failure"));
    Ok(Recovery {
        id: row.id,
        stage,
        irrevocable: row.irrevocable,
        claimed: state.amount as u64,
        corrected: state.reduced.map(u64::from),
        received: if stage == Stage::Done {
            Some(
                state
                    .sweep
                    .as_ref()
                    .and_then(|t| t.output.first())
                    .map_or(0, |o| o.value.to_sat()),
            )
        } else {
            None
        },
        notes: state.notes.len(),
        request: txid(&state.r),
        publication: txid(&state.p),
        requested_at: state.r_height,
        timeout: timed.then(|| txid(&state.payout)).flatten(),
        timed_out_at: timed.then_some(state.payout_height).flatten(),
        sweep: txid(&state.sweep),
        swept_at: state.swept_height,
        next_at: state.next_at,
        error: row.error.clone(),
        response: txid(&state.response),
        deadline: state.deadline,
        proved: state.verified == Some(true),
        assertion: txid(&state.assertion),
        asserted_at: state.assertion_height,
        challenge: txid(&state.challenge),
        payout: txid(&state.payout),
        paid_at: state.payout_height,
    })
}

enum Next {
    Wait,
    Save,
    To(Stage),
}
struct Run<'a, T: Transport, C: Chain> {
    wallet: &'a Wallet<T>,
    chain: &'a C,
    store: &'a Store,
    dir: &'a std::path::Path,
    id: [u8; 32],
    rate: FeeRate,
    o: Opening,
    keys: HolderKeys,
    since: u32,
    took: AtomicBool,
    rebuilt: AtomicBool,
    refused: std::sync::Mutex<Vec<String>>,
}
impl<'a, T: Transport, C: Chain> Run<'a, T, C> {
    async fn load(
        wallet: &'a Wallet<T>,
        chain: &'a C,
        store: &'a Store,
        dir: &'a std::path::Path,
        id: &[u8; 32],
        rate: FeeRate,
    ) -> Result<Self> {
        let (state, since) = store.state(id)?;
        let tip = chain.height().await.map_err(|e| anyhow!(e))?;
        let secrets = wallet.contract_secrets();
        let holder = Holder::load(
            &state,
            &secrets.seed,
            &Context {
                tip,
                trust: crate::trust(wallet.network())?,
            },
        )?;
        let o = holder.signed()?;
        let keys = HolderKeys::derive(&secrets.seed, &o.base.parts().nonce);
        Ok(Self {
            wallet,
            chain,
            store,
            dir,
            id: *id,
            rate,
            o,
            keys,
            since,
            took: AtomicBool::new(false),
            rebuilt: AtomicBool::new(false),
            refused: Default::default(),
        })
    }
    async fn steps(&self) -> Result<()> {
        loop {
            let row = self.store.recovery(&self.id)?.context("given up")?;
            let stage = Stage::parse(&row.stage)?;
            ensure!(stage != Stage::GivingUp, "given up");
            let mut state = State::decode(&row)?;
            let tip = self.chain.height().await.map_err(|e| anyhow!(e))?;
            if let Some(stood) = self.reorganized(stage, tip).await? {
                let to = rewind(stage, &mut state, stood);
                tracing::warn!(
                    id = %crate::hex_id(&self.id), stood, from = stage.as_str(), to = to.as_str(),
                    "blocks she saw left the chain: what she saw in them found again"
                );
                if !self.store.save_recovery(
                    &self.id,
                    (stage.as_str(), &row.state),
                    to.as_str(),
                    true,
                    &state.encode(),
                )? {
                    return self.outrun();
                }
                self.store.forget_recovery_tips(&self.id, stood)?;
                continue;
            }
            self.hold_challenge_coin(stage, &state)?;
            let next = match stage {
                Stage::Claiming => self.claim(&mut state, tip).await?,
                Stage::Claimed => self.commit(&mut state, tip).await?,
                Stage::Irrevocable => self.request(&mut state)?,
                Stage::Signing => self.sign(&mut state)?,
                Stage::Published => self.publish(&mut state, tip).await?,
                Stage::Requested
                | Stage::Answered
                | Stage::Asserted
                | Stage::Challenging
                | Stage::Challenged => self.watch(stage, &mut state, tip).await?,
                Stage::TimingOut | Stage::Succeeding => self.finishing(&mut state, tip).await?,
                Stage::TimedOut | Stage::Paid => self.sweep(&mut state, tip)?,
                Stage::Sweeping => self.swept(&mut state, tip).await?,
                Stage::Done | Stage::Lost | Stage::GivingUp => Next::Wait,
            };
            let to = match next {
                Next::Wait => return self.report(),
                Next::Save => stage,
                Next::To(to) => to,
            };
            let irrevocable = !matches!(to, Stage::Claiming | Stage::Claimed);
            if !self.store.save_recovery(
                &self.id,
                (stage.as_str(), &row.state),
                to.as_str(),
                irrevocable,
                &state.encode(),
            )? {
                return self.outrun();
            }
            if to != stage {
                tracing::info!(id = %crate::hex_id(&self.id), stage = to.as_str(), "recovering a contract");
            }
            if matches!(next, Next::Save) {
                return self.report();
            }
        }
    }
    fn outrun(&self) -> Result<()> {
        if self.took.load(Ordering::SeqCst)
            && self
                .store
                .recovery(&self.id)?
                .is_none_or(|r| r.stage == Stage::GivingUp.as_str())
        {
            self.wallet.unclaim_notes(&self.id)?;
        }
        Ok(())
    }
    /// Whether blocks she saw since she published left the chain: the
    /// height of the newest she saw that stands, if so. Each round the tip
    /// she sees is kept ([`Store::see_recovery_tip`]), the first once she
    /// published ([`watched`]); none kept, as of a build before, tells
    /// nothing. Should none she kept stand, the oldest is taken to be the
    /// last that did. A tip lower than one she saw is no sign yet: looked
    /// at again once the chain is as long.
    async fn reorganized(&self, stage: Stage, tip: u32) -> Result<Option<u32>> {
        if !watched(stage) {
            return Ok(None);
        }
        // Read before the others: a chain replaced meanwhile shows as one
        // of them gone, never as a tip of it kept above them.
        let at_tip = self.chain.hash(tip).await.map_err(|e| anyhow!(e))?;
        let seen = self.store.recovery_tips(&self.id)?;
        let mut gone = false;
        for &(height, hash) in &seen {
            if height > tip {
                continue;
            }
            if self.chain.hash(height).await.map_err(|e| anyhow!(e))? == hash {
                if gone {
                    return Ok(Some(height));
                }
                break;
            }
            gone = true;
        }
        if gone {
            return Ok(seen.last().map(|(height, _)| height.saturating_sub(1)));
        }
        self.store.see_recovery_tip(&self.id, tip, at_tip)?;
        Ok(None)
    }
    fn trouble(&self, why: String) {
        tracing::warn!(id = %crate::hex_id(&self.id), "recovering a contract: {why}");
        self.refused.lock().expect("refusals").push(why);
    }
    fn report(&self) -> Result<()> {
        let refused = self.refused.lock().expect("refusals").join("; ");
        self.store
            .recovery_error(&self.id, (!refused.is_empty()).then_some(refused.as_str()))?;
        Ok(())
    }
    async fn broadcast(&self, tx: &Transaction) {
        if let Err(e) = self.chain.broadcast(tx).await {
            self.trouble(format!("{}: {e}", tx.compute_txid()));
        }
    }
    fn give_back(&self, why: &str) -> Result<()> {
        if self
            .store
            .revoke_recovery(&self.id, Stage::GivingUp.as_str(), Some(why))?
        {
            finish_giving_up(self.wallet, self.store, &self.id)?;
        }
        Ok(())
    }

    /// Her notes of the hub's keyset taken, its share of them with her
    /// other contracts there ([`claims`]), and claimed: the claim signed
    /// by each note's hold key, for their face value, its cap at most.
    /// The branches of her locks there owed on Bitcoin first
    /// ([`LockClaim`]), within its cap: the refunds of those she made that
    /// expired at depth unsettled, and the pay branches of those paying her
    /// whose activation its statement shows ([`activation::shown`]). Should
    /// the claim not be filed with those, it is without them.
    async fn claim(&self, state: &mut State, tip: u32) -> Result<Next> {
        // Before any note of hers is taken.
        self.o.forcible()?;
        let parts = self.o.base.parts();
        let keyset = *parts.keyset.id();
        let mut notes = self.wallet.claimed_notes(&self.id, &keyset)?;
        let mut locks = self.wallet.claimed_locks(&self.id, &keyset)?;
        if notes.is_empty() && locks.is_empty() {
            let shown = activation::shown(self.wallet, self.chain, &self.o.base, tip).await?;
            let pays = |p: &LockClaim| shown.contains(&p.id);
            (notes, locks) =
                self.wallet
                    .claim_with_locks(&self.id, &self.slots(tip)?, tip, Some(&pays))?;
            let held = (notes.iter().map(|n| n.note.message().denomination()))
                .chain(locks.iter().map(|r| r.value))
                .sum::<u64>();
            let amount = parts.cap.min(u32::try_from(held).unwrap_or(u32::MAX));
            let paying = locks.iter().any(|r| r.branch == Branch::Pay);
            if paying {
                if let Err(e) = self.filing(&notes, &locks, amount) {
                    tracing::warn!(id = %crate::hex_id(&self.id), "a claim filed without the locks paying her: {e:#}");
                    self.wallet.unclaim_notes(&self.id)?;
                    (notes, locks) =
                        self.wallet
                            .claim_with_locks(&self.id, &self.slots(tip)?, tip, None)?;
                }
            }
            self.took
                .store(!(notes.is_empty() && locks.is_empty()), Ordering::SeqCst);
        }
        if notes.is_empty() && locks.is_empty() {
            self.give_back("no notes at its hub to claim")?;
            bail!("no notes at its hub to claim");
        }
        // A note beyond what is left of the cap is claimed for that much.
        let held: u64 = (notes.iter().map(|n| n.note.message().denomination()))
            .chain(locks.iter().map(|r| r.value))
            .sum();
        state.amount = parts.cap.min(u32::try_from(held).unwrap_or(u32::MAX));
        let filing = match self.filing(&notes, &locks, state.amount) {
            Ok(filing) => filing,
            Err(e) => {
                self.give_back(&format!("{e:#}"))?;
                return Err(e);
            }
        };
        state.notes = notes.iter().map(|n| hex::encode(n.to_bytes())).collect();
        state.filing = Some(hex::encode(filing.to_bytes()));
        Ok(Next::To(Stage::Claimed))
    }

    /// The slots her notes at its hub are shared out to: its own, and
    /// those of her other contracts there protecting notes at `tip`.
    fn slots(&self, tip: u32) -> Result<Vec<ClaimSlot>> {
        let all = self.store.list()?;
        let this = (all.iter().find(|c| c.id == self.id)).context("a contract not stored")?;
        let urls = self.wallet.hub_urls(&this.hub).unwrap_or_default();
        let others: Vec<_> = (all.iter())
            .filter(|c| c.id != self.id && c.live(tip) && c.at(&urls))
            .collect();
        let mut terms = claims::terms(self.wallet, self.store, &others, tip)?;
        terms.insert(self.id, ClaimTerms::of(&self.o));
        let mine = others.into_iter().chain([this]);
        Ok(claims::claim_slots(mine, &terms))
    }

    fn filing(&self, notes: &[ClaimNote], locks: &[LockClaim], amount: u32) -> Result<Filing> {
        let p = self.o.base.parts();
        let limit = p.limits.max_authorizations as usize;
        // In the claim's order of sources, which their witnesses follow.
        let mut locks = locks.to_vec();
        locks.sort_by_key(|r| r.id);
        let messages: Vec<_> = notes.iter().map(|n| n.note.message().clone()).collect();
        let mut keys = self.wallet.hold_keys(notes)?;
        keys.extend(locks.iter().flat_map(|r| r.keys.iter().cloned()));
        let mut authorizations: Vec<_> = keys.iter().map(|(m, _)| m.clone()).collect();
        authorizations.sort_by_key(|m| m.identifier().ok());
        let items = (messages.into_iter().map(Item::Settled))
            .chain(locks.iter().flat_map(|r| r.items.iter().cloned()))
            .collect();
        let claim = UnsignedClaim::new(
            claim::Context {
                base: self.o.base.digest(),
                deposit: p.deposit,
                cap: p.cap,
            },
            &p.keyset,
            amount,
            claim::Parts {
                items,
                sources: locks.iter().map(|r| r.source.clone()).collect(),
                activations: (locks.iter())
                    .filter_map(|r| r.activation.as_ref().map(|(a, _)| *a))
                    .collect(),
                authorizations,
            },
            limit,
        )?;
        let publication = Publication::sign(claim, &keys, limit)?;
        let filing = Filing::new(
            publication,
            FilingWitness {
                notes: notes.iter().map(|n| n.note.clone()).collect(),
                sources: locks.iter().map(|r| r.witness.clone()).collect(),
                activations: (locks.iter())
                    .filter_map(|r| r.activation.as_ref().map(|(_, w)| w.clone()))
                    .collect(),
            },
        )?;
        ensure!(
            filing.to_bytes().len() <= p.limits.max_publication_bytes as usize,
            "the whole filing exceeds the publication limit"
        );
        Ok(filing)
    }
    fn filing_of(&self, state: &State) -> Result<Filing> {
        let p = self.o.base.parts();
        let bytes = hex::decode(state.filing.as_deref().context("no filing")?)?;
        let filing = Filing::from_bytes(&p.keyset, p.limits.max_authorizations as usize, &bytes)?;
        let claim = filing.publication().claim();
        ensure!(
            claim.context()
                == &claim::Context {
                    base: self.o.base.digest(),
                    deposit: p.deposit,
                    cap: p.cap
                }
                && claim.amount() == state.amount,
            "filing differs from pinned request"
        );
        Ok(filing)
    }
    fn requested(&self, state: &State) -> Result<Request> {
        Ok(Request {
            amount: state.amount,
            claim_digest: *self.filing_of(state)?.publication().claim().digest(),
        })
    }
    fn request_matches(&self, state: &State, tx: &Transaction) -> Result<bool> {
        let r = self.requested(state)?;
        Ok(HubProves::requested_b(tx)? == (r.amount, r.claim_digest))
    }
}

impl<T: Transport, C: Chain> Run<'_, T, C> {
    async fn commit(&self, state: &mut State, tip: u32) -> Result<Next> {
        let p = presign(&self.o.set, "request")?;
        let coin = p.tx.input[0].previous_output;
        let f_height = match state.f_height {
            Some(height) => height,
            None => confirmed_at(
                self.chain,
                coin.txid,
                &p.prevout.script_pubkey,
                self.since..=tip,
            )
            .await?
            .context("F in no block")?,
        };
        state.f_height = Some(f_height);
        let due = f_height
            .checked_add(blocks(p.tx.input[0].sequence)?)
            .context("request delay overflow")?;
        state.next_at = Some(due);
        let from = state.from.unwrap_or(f_height);
        if let Some((height, tx)) =
            first_spend(self.chain, coin, &p.prevout.script_pubkey, from..=tip).await?
        {
            if tx.compute_txid() != p.tx.compute_txid() {
                let why = format!(
                    "its collateral is spent already, by {}: closed otherwise",
                    tx.compute_txid()
                );
                self.give_back(&why)?;
                bail!(why);
            }
            if !self.request_matches(state, &tx)? {
                self.trouble("another copy of this wallet requested it, of other notes".into());
                return Ok(Next::To(Stage::Lost));
            }
            state.r = Some(tx);
            state.r_height = Some(height);
            return Ok(Next::To(Stage::Irrevocable));
        }
        state.from = Some(rescan(from, tip));
        if tip.saturating_add(1) < due {
            return Ok(Next::Save);
        }
        ensure!(
            tip.saturating_add(1) <= self.o.base.parts().graph.request_height,
            "past the request deadline"
        );
        let request = self.requested(state)?;
        // Dry construction is private. No signature leaves before the CAS
        // to Irrevocable pins this filing, amount and E durably.
        let r = self
            .o
            .request(&self.keys, request.amount, &request.claim_digest)?;
        self.build_p(&r, &self.filing_of(state)?, None, state.p_allowance)?;
        Ok(Next::To(Stage::Irrevocable))
    }
    fn request(&self, state: &mut State) -> Result<Next> {
        let request = self.requested(state)?;
        let r = match &state.r {
            // Pinned under an older build of the graph: never valid now.
            Some(r) => {
                self.o.forcible()?;
                r.clone()
            }
            None => self
                .o
                .request(&self.keys, request.amount, &request.claim_digest)?,
        };
        ensure!(
            self.request_matches(state, &r)?,
            "pinned R reads another claim"
        );
        let (p, prevouts) = self.build_p(&r, &self.filing_of(state)?, None, state.p_allowance)?;
        state.r = Some(r);
        state.p = Some(p);
        state.prevouts = prevouts;
        Ok(Next::To(Stage::Signing))
    }
    fn terminal_fee(&self) -> Result<Amount> {
        let dummy = OutPoint::null();
        let coin = TxOut {
            value: DUST,
            script_pubkey: dispute::fee_script(&self.keys.party.payment)?,
        };
        let mut max_vsize = 0;
        for name in ["request_timeout", "respond_timeout", "failure", "success"] {
            let p = presign(&self.o.set, name)?;
            let tx = dispute::add_fee(
                self.o.finish(name, vec![])?,
                p.prevout.clone(),
                (dummy, coin.clone()),
                &self.keys.party.payment,
            )?;
            max_vsize = max_vsize.max(tx.vsize() as u64);
        }
        Ok(self
            .rate
            .fee_vb(max_vsize * TIMEOUT_FEE_MARGIN)
            .context("terminal fee overflow")?
            .max(DUST))
    }
    /// Her publication of `filing`, spending `r`'s slot, its anchor and her
    /// purse, its fee within `allowance` in all, else what those hold
    /// besides the fee coin it pays her terminals (her reserve's, so a
    /// wrong rate or a bug spends no more): a quarter of that at most at
    /// the round's rate, or, `bump`ing one made before, exactly the fee
    /// given, its fee coin kept. One coin of hers too only should an
    /// allowance ask more than they hold.
    fn build_p(
        &self,
        r: &Transaction,
        filing: &Filing,
        bump: Option<(&Transaction, Amount)>,
        allowance: Option<u64>,
    ) -> Result<(Transaction, Vec<TxOut>)> {
        let request = presign(&self.o.set, "request")?;
        let slot = request.tx.output.get(2).context("no claim slot")?.clone();
        let anchor = r.output.get(1).context("no request anchor")?.clone();
        let fee_script = dispute::fee_script(&self.keys.party.payment)?;
        ensure!(
            anchor.script_pubkey == fee_script,
            "request anchor is not hers"
        );
        let held = slot.value + anchor.value + self.o.babe.purse().unwrap_or(Amount::ZERO);
        let fee_coin = match bump {
            Some((old, _)) => {
                let kept = old.output.get(1).context("no fee coin to keep")?;
                ensure!(kept.script_pubkey == fee_script, "a fee coin not hers");
                kept.clone()
            }
            // Half of what R hands her at most, whatever the rate says.
            None => TxOut {
                value: self.terminal_fee()?.min(held / 2).max(DUST),
                script_pubkey: fee_script,
            },
        };
        let limit = match allowance {
            Some(sats) => Amount::from_sat(sats),
            None => held.checked_sub(fee_coin.value).unwrap_or(Amount::ZERO),
        };
        let change = self.wallet.fallback_address()?.script_pubkey();
        let r_total: Amount = r.output.iter().map(|o| o.value).sum();
        let r_fee = request
            .prevout
            .value
            .checked_sub(r_total)
            .context("R creates money")?;
        let bytes = filing.to_bytes();
        let at = |vout| OutPoint {
            txid: r.compute_txid(),
            vout,
        };
        let mut base = vec![(at(2), slot), (at(1), anchor)];
        // Her purse, R's, pays it unless the fees run past it.
        if let Some(value) = self.o.babe.purse() {
            let purse = r.output.get(PURSE_VOUT as usize).context("no purse")?;
            ensure!(
                purse.value == value && purse.script_pubkey == fee_coin.script_pubkey,
                "request purse is not hers"
            );
            base.push((at(PURSE_VOUT), purse.clone()));
        }
        // Else one coin of hers alone too, the smallest that pays: coins
        // are never joined, not to tie them together on Bitcoin.
        let mut coins = super::coins(self.wallet, self.dir)?;
        coins.reverse();
        let most = coins.last().map_or(0, |c| c.value);
        let coins = coins.into_iter().map(|coin| -> Result<_> {
            let output = self
                .wallet
                .silent_coin_output(&coin.outpoint)?
                .context("unknown fee coin")?;
            Ok(Some((coin.outpoint, output)))
        });
        // What one coin more must hold, of R's own paying too little: its
        // shortfall, the coin's input's fee and its change's dust limit.
        let mut needed = None;
        for coin in std::iter::once(Ok(None)).chain(coins) {
            let coin = coin?;
            let alone = coin.is_none();
            let mut inputs = base.clone();
            inputs.extend(coin);
            let prevouts: Vec<_> = inputs.iter().map(|(_, out)| out.clone()).collect();
            let total: Amount = prevouts.iter().map(|out| out.value).sum();
            let mut p = self.o.publication(
                &self.keys,
                &bytes,
                vec![
                    fee_coin.clone(),
                    TxOut {
                        value: Amount::ZERO,
                        script_pubkey: change.clone(),
                    },
                ],
            )?;
            p.input.extend(inputs.iter().skip(1).map(|(op, _)| TxIn {
                previous_output: *op,
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                ..Default::default()
            }));
            let mut weighed = p.clone();
            for i in 1..weighed.input.len() {
                if prevouts[i].script_pubkey == fee_coin.script_pubkey {
                    dispute::sign_fee_input(&self.keys.party.payment, &mut weighed, i, &prevouts)?;
                } else {
                    weighed.input[i].witness = Witness::from_slice(&[[0u8; 64]]);
                }
            }
            let own = self
                .rate
                .fee_vb(weighed.vsize() as u64)
                .context("publication fee")?;
            let package = self
                .rate
                .fee_vb(weighed.vsize() as u64 + r.vsize() as u64)
                .context("request package fee")?;
            let least = FeeRate::from_sat_per_vb_u32(1)
                .fee_vb(weighed.vsize() as u64)
                .context("publication fee")?;
            let fee = match bump {
                Some((_, fee)) => fee,
                None => (own.max(package.checked_sub(r_fee).unwrap_or(Amount::ZERO)))
                    .min(limit / 4)
                    .max(least),
            };
            ensure!(
                fee <= limit,
                "her publication would pay {fee} in fees, more than the {limit} {}: \
                 allow more with `licash-wallet allow-publication-fee {} <sat>`",
                match allowance {
                    Some(_) => "she allowed",
                    None => "its reserve holds",
                },
                crate::hex_id(&self.id)
            );
            let Some(left) = total.checked_sub(fee_coin.value + fee) else {
                if alone {
                    let input = self.rate.fee_vb(58).context("input fee")?;
                    needed = Some(fee_coin.value + fee + input - total + DUST);
                }
                continue;
            };
            if left >= DUST {
                p.output[2].value = left;
            } else {
                p.output.pop();
            }
            // Over its every input and final output: none can be added.
            self.o.sign_publication(&self.keys, &mut p, &prevouts)?;
            return Ok((p, prevouts));
        }
        let needed = needed.map_or(0, |n| n.to_sat());
        bail!(
            "too little of hers on Bitcoin to pay her request's fees: \
             about {needed} sat needed in one coin, her largest {most} sat"
        )
    }
    fn sign(&self, state: &mut State) -> Result<Next> {
        let mut p = state.p.clone().context("no publication transaction")?;
        let fee_script = dispute::fee_script(&self.keys.party.payment)?;
        for i in 1..p.input.len() {
            if state.prevouts[i].script_pubkey == fee_script {
                dispute::sign_fee_input(&self.keys.party.payment, &mut p, i, &state.prevouts)?;
                continue;
            }
            match self.wallet.sign_input(&p, &state.prevouts, i) {
                Ok(signed) => p.input[i].witness = signed.input[i].witness.clone(),
                Err(e @ (licash_wallet::Error::Invalid(_) | licash_wallet::Error::NoteTaken)) => {
                    ensure!(
                        !self.rebuilt.swap(true, Ordering::SeqCst),
                        "a coin of P's again: {e}"
                    );
                    let _ = self.wallet.abandon_silent(&p.compute_txid());
                    state.p = None;
                    state.prevouts.clear();
                    return Ok(Next::To(Stage::Irrevocable));
                }
                Err(e) => return Err(e.into()),
            }
        }
        ensure!(
            bitcoin::consensus::serialize(&p).len()
                <= self.o.base.parts().limits.max_transaction_bytes as usize,
            "publication transaction exceeds contract limit"
        );
        state.p = Some(p);
        Ok(Next::To(Stage::Published))
    }
    /// Her coin paying P, if any, kept from her other spends.
    fn keep_p(&self, p: &Transaction, prevouts: &[TxOut]) {
        let Ok(fee_script) = dispute::fee_script(&self.keys.party.payment) else {
            return;
        };
        for i in 1..p.input.len() {
            if prevouts
                .get(i)
                .is_none_or(|o| o.script_pubkey == fee_script)
            {
                continue;
            }
            if let Err(e) = self.wallet.sign_input(p, prevouts, i) {
                tracing::debug!(id = %crate::hex_id(&self.id), "a fee coin not reserved again: {e}");
            }
        }
    }
    async fn publish(&self, state: &mut State, tip: u32) -> Result<Next> {
        let r = state.r.clone().context("no R")?;
        let p = state.p.clone().context("no P")?;
        if state.r_height.is_none() {
            let request = presign(&self.o.set, "request")?;
            let from = state.from.or(state.f_height).unwrap_or(self.since);
            match first_spend(
                self.chain,
                request.tx.input[0].previous_output,
                &request.prevout.script_pubkey,
                from..=tip,
            )
            .await?
            {
                Some((height, tx)) if tx.compute_txid() == r.compute_txid() => {
                    if !self.request_matches(state, &tx)? {
                        let _ = self.wallet.abandon_silent(&p.compute_txid());
                        self.trouble(
                            "another copy of this wallet requested it, of other notes".into(),
                        );
                        return Ok(Next::To(Stage::Lost));
                    }
                    state.r = Some(tx);
                    state.r_height = Some(height);
                }
                Some((_, tx)) => {
                    let _ = self.wallet.abandon_silent(&p.compute_txid());
                    self.trouble(format!("collateral went to {} instead", tx.compute_txid()));
                    return Ok(Next::To(Stage::Lost));
                }
                None => {
                    state.from = Some(rescan(from, tip));
                }
            }
        }
        let Some(r_height) = state.r_height else {
            if state.next_at.is_none_or(|due| tip.saturating_add(1) >= due) {
                if let Some(next) = self.bump(state, &r, &p, tip)? {
                    return Ok(next);
                }
                self.keep_p(&p, &state.prevouts);
                self.broadcast(&r).await;
                self.broadcast(&p).await;
            }
            return Ok(Next::Save);
        };
        let slot = OutPoint {
            txid: r.compute_txid(),
            vout: 2,
        };
        let spent =
            first_spend(self.chain, slot, &r.output[2].script_pubkey, r_height..=tip).await?;
        // A version of hers, the last or one it took the place of: what she
        // publishes, whichever spends the slot, carries her claim and pays
        // her fee coin alike.
        let hers = match &spent {
            Some((_, tx)) => !self.reclaimed(tx)? && tx.output.get(..2) == p.output.get(..2),
            None => false,
        };
        match spent {
            Some((height, tx)) if hers => {
                if tx.compute_txid() != p.compute_txid() {
                    let _ = self.wallet.abandon_silent(&p.compute_txid());
                    state.p = Some(tx);
                }
                state.p_height = Some(height);
                state.from = Some(r_height);
                state.next_at = Some(
                    r_height
                        + blocks(presign(&self.o.set, "request_timeout")?.tx.input[0].sequence)?,
                );
                Ok(Next::To(Stage::Requested))
            }
            // The hub's reclaim, P too late: her claim reads as empty, owed
            // nothing, and the hub's answer to it pays her reserve back
            // (less her purse), or its silence her request. Followed so.
            Some((_, tx)) if self.reclaimed(&tx)? => {
                let _ = self.wallet.abandon_silent(&p.compute_txid());
                self.trouble(format!(
                    "her claim was not published in time: the hub took its slot ({}), so it reads as empty",
                    tx.compute_txid()
                ));
                state.reclaimed = true;
                state.from = Some(r_height);
                state.next_at = Some(
                    r_height
                        + blocks(presign(&self.o.set, "request_timeout")?.tx.input[0].sequence)?,
                );
                Ok(Next::To(Stage::Requested))
            }
            Some((_, tx)) => bail!("claim slot spent by {}, not P", tx.compute_txid()),
            None => {
                if let Some(next) = self.bump(state, &r, &p, tip)? {
                    return Ok(next);
                }
                self.keep_p(&p, &state.prevouts);
                self.broadcast(&p).await;
                Ok(Next::Save)
            }
        }
    }

    /// Her publication `p`, in no block at `tip`, made again to pay more
    /// as its window runs out, then signed ([`Stage::Signing`]): its fee
    /// the limit [`Self::build_p`] keeps to, a quarter of it once a quarter
    /// of its window passed, half at half, all of it at three quarters,
    /// whatever the chain says fees are (a hub serving the chain could say
    /// anything). Told then, if still in no block, so that she may allow
    /// more ([`allow_publication_fee`]). Its window: until the request
    /// deadline while `r` is in no block, else its publication window from
    /// `r`'s block. None if it pays enough already.
    fn bump(
        &self,
        state: &mut State,
        r: &Transaction,
        p: &Transaction,
        tip: u32,
    ) -> Result<Option<Next>> {
        let graph = &self.o.base.parts().graph;
        let (start, end) = match state.r_height {
            Some(at) => (at, at.saturating_add(u32::from(graph.publication_window))),
            None => (state.next_at.unwrap_or(tip), graph.request_height),
        };
        let paid = (state.prevouts.iter().map(|o| o.value).sum::<Amount>())
            .checked_sub(p.output.iter().map(|o| o.value).sum())
            .context("her publication creates money")?;
        let held: Amount = (p.input.iter().zip(&state.prevouts))
            .filter(|(i, _)| i.previous_output.txid == r.compute_txid())
            .map(|(_, o)| o.value)
            .sum();
        let limit = match state.p_allowance {
            Some(sats) => Amount::from_sat(sats),
            None => held.checked_sub(p.output[1].value).unwrap_or(Amount::ZERO),
        };
        let rung = rung(start, end, tip);
        if rung == 3 {
            // Signed so that others could add inputs to it, by a release
            // before: one of more fee at a lower rate may hold it out.
            let pinnable = p.input[0].witness.nth(0).is_some_and(|s| s.len() == 65);
            self.trouble(format!(
                "her publication {} is in no block, {} blocks left to it, paying {paid} of the \
                 {limit} it may: allow more with `licash-wallet allow-publication-fee {} <sat>`{}",
                p.compute_txid(),
                end.saturating_sub(tip),
                crate::hex_id(&self.id),
                if pinnable {
                    " (signed by an older release, others may hold it out)"
                } else {
                    ""
                },
            ));
        }
        let target = match rung {
            0 => return Ok(None),
            rung => Amount::from_sat(limit.to_sat() >> (3 - rung)),
        };
        if !deadline::replaces(target, p.vsize() as u64, paid) {
            return Ok(None);
        }
        let filing = self.filing_of(state)?;
        let (bumped, prevouts) =
            match self.build_p(r, &filing, Some((p, target)), state.p_allowance) {
                Ok(made) => made,
                Err(e) => {
                    self.trouble(format!("{e:#}"));
                    return Ok(None);
                }
            };
        tracing::info!(id = %crate::hex_id(&self.id), from = %paid, to = %target, "her publication bumped");
        let _ = self.wallet.abandon_silent(&p.compute_txid());
        state.p = Some(bumped);
        state.prevouts = prevouts;
        Ok(Some(Next::To(Stage::Signing)))
    }
}

/// The rung of a window from `start` to `end` at `tip`: 0 in its first
/// quarter, then 1, 2 and 3 in its last, and after.
fn rung(start: u32, end: u32, tip: u32) -> u32 {
    let len = u64::from(end.saturating_sub(start).max(1));
    let gone = u64::from(tip.saturating_sub(start));
    (gone * 4 / len).min(3) as u32
}

/// Her recovery at `stage` taken back to what blocks up to `stood` show,
/// those above it replaced: what she saw in a block above it forgotten
/// (each link the hub or she posted, from the first, and all after it),
/// the stage that watches for the first of them again, and her scan from
/// the block after `stood` at the latest. What she signed or revealed is
/// kept, to be posted again as it was: her request and publication, her
/// challenge and its child, her sweep.
fn rewind(stage: Stage, state: &mut State, stood: u32) -> Stage {
    let gone = |height: Option<u32>| height.is_some_and(|h| h > stood);
    state.from = state.from.map(|from| from.min(stood.saturating_add(1)));
    let (to, first) = if gone(state.r_height) {
        (Stage::Published, 0)
    } else if gone(state.p_height) || state.reclaimed {
        // The hub's reclaim, its height not kept: found again too.
        (Stage::Published, 1)
    } else if gone(state.response_height) {
        (Stage::Requested, 2)
    } else if gone(state.assertion_height) {
        (Stage::Answered, 3)
    } else if gone(state.challenge_height) {
        (Stage::Asserted, 4)
    } else if gone(state.payout_height) {
        let watching = if state.challenge_height.is_some() {
            Stage::Challenged
        } else if state.assertion_height.is_some() {
            Stage::Asserted
        } else if state.response_height.is_some() {
            Stage::Answered
        } else {
            Stage::Requested
        };
        (watching, 5)
    } else {
        return stage;
    };
    if first == 0 {
        state.r_height = None;
    }
    if first <= 1 {
        state.p_height = None;
        state.reclaimed = false;
    }
    if first <= 2 {
        state.response = None;
        state.response_height = None;
        state.reduced = None;
        state.deadline = None;
    }
    if first <= 3 {
        state.assertion = None;
        state.assertion_height = None;
        state.verified = None;
    }
    if first <= 4 {
        state.challenge_height = None;
    }
    state.payout = None;
    state.payout_name = None;
    state.payout_height = None;
    state.swept_height = None;
    state.next_at = None;
    if first == 0 {
        // Her request in no block now, nor any before the next: her
        // publication's fee climbs again from there.
        state.from = Some(stood.saturating_add(1));
        state.next_at = Some(stood.saturating_add(1));
    }
    to
}

/// Whether her recovery at `stage` follows the tips it sees: from her
/// publication's on, while not over, but while it is signed again.
fn watched(stage: Stage) -> bool {
    !matches!(
        stage,
        Stage::Claiming
            | Stage::Claimed
            | Stage::Irrevocable
            | Stage::Signing
            | Stage::GivingUp
            | Stage::Done
            | Stage::Lost
    )
}

/// Let the recovery of contract `id` in `dir` pay `sats` in all for its
/// publication's fees, more than its reserve holds: her call, once told it
/// falls short, or before. Refused above what her contract holds, a typo
/// as like as not.
pub fn allow_publication_fee(dir: &std::path::Path, id: &[u8; 32], sats: u64) -> Result<()> {
    let store = Store::existing(dir)?.context("no contracts")?;
    let contract = (store.list()?.into_iter().find(|c| &c.id == id)).context("no such contract")?;
    ensure!(
        sats <= contract.value,
        "{sats} sat is more than the contract holds ({} sat)",
        contract.value
    );
    for _ in 0..8 {
        let row = store.recovery(id)?.context("not forced closed")?;
        let mut state = State::decode(&row)?;
        state.p_allowance = Some(sats);
        if store.save_recovery(
            id,
            (&row.stage, &row.state),
            &row.stage,
            false,
            &state.encode(),
        )? {
            return Ok(());
        }
    }
    bail!("the recovery kept moving on: try again")
}

impl<T: Transport, C: Chain> Run<'_, T, C> {
    fn parent(&self, stage: Stage, state: &State) -> Result<(Transaction, u32)> {
        let pair = match stage {
            Stage::Requested => (&state.r, state.r_height),
            Stage::Answered => (&state.response, state.response_height),
            Stage::Asserted | Stage::Challenging => (&state.assertion, state.assertion_height),
            Stage::Challenged => (&state.challenge, state.challenge_height),
            _ => bail!("no parent to watch at {}", stage.as_str()),
        };
        Ok((
            pair.0.clone().context("missing watched transaction")?,
            pair.1.context("watched transaction is unconfirmed")?,
        ))
    }
    fn is_link(&self, name: &str, tx: &Transaction) -> Result<bool> {
        Ok(tx.compute_txid() == presign(&self.o.set, name)?.tx.compute_txid())
    }
    /// Whether `tx`, spending her request's claim slot, is the hub's
    /// reclaim of it: its one output paying the hub's payout script.
    fn reclaimed(&self, tx: &Transaction) -> Result<bool> {
        let hub = licash_contract::graph::party_pub(&self.o.base.parts().parties[1])?;
        Ok(matches!(&tx.output[..], [out] if out.script_pubkey == hub.payout_spk))
    }
    fn is_terminal(&self, name: &str, tx: &Transaction) -> Result<bool> {
        let p = presign(&self.o.set, name)?;
        Ok(tx.output == p.tx.output
            && tx
                .input
                .iter()
                .any(|i| i.previous_output == p.tx.input[0].previous_output))
    }
    fn paid(&self, state: &mut State, name: &str, tx: Transaction, height: u32) -> Next {
        state.payout = Some(tx);
        state.payout_name = Some(name.into());
        state.payout_height = Some(height);
        state.from = Some(height);
        state.next_at = None;
        Next::To(
            if matches!(name, "request_timeout" | "respond_timeout" | "failure") {
                Stage::TimedOut
            } else {
                Stage::Paid
            },
        )
    }
    fn observed(
        &self,
        stage: Stage,
        state: &mut State,
        height: u32,
        tx: Transaction,
    ) -> Result<Next> {
        match stage {
            Stage::Requested => {
                if self.is_link("respond", &tx)? {
                    state.reduced = Some(HubProves::responded_b(&tx)?.0);
                    state.response = Some(tx);
                    state.response_height = Some(height);
                    state.payout = None;
                    state.payout_name = None;
                    state.payout_height = None;
                    state.from = Some(height);
                    let due = height
                        + blocks(presign(&self.o.set, "respond_timeout")?.tx.input[0].sequence)?;
                    state.deadline = Some(due);
                    state.next_at = Some(due);
                    return Ok(Next::To(Stage::Answered));
                }
                if self.is_terminal("request_timeout", &tx)? {
                    return Ok(self.paid(state, "request_timeout", tx, height));
                }
            }
            Stage::Answered => {
                if self.is_link("assert", &tx)? {
                    state.verified = Some(self.o.verify_assertion(&tx)?);
                    state.assertion = Some(tx);
                    state.assertion_height = Some(height);
                    state.payout = None;
                    state.payout_name = None;
                    state.payout_height = None;
                    state.from = Some(height);
                    state.next_at = None;
                    return Ok(Next::To(Stage::Asserted));
                }
                if self.is_terminal("respond_timeout", &tx)? {
                    return Ok(self.paid(state, "respond_timeout", tx, height));
                }
            }
            Stage::Asserted | Stage::Challenging => {
                if self.is_link("challenge_assert", &tx)? {
                    state.challenge = Some(tx);
                    state.challenge_height = Some(height);
                    state.payout = None;
                    state.payout_name = None;
                    state.payout_height = None;
                    state.from = Some(height);
                    state.next_at = Some(
                        height + blocks(presign(&self.o.set, "failure")?.tx.input[0].sequence)?,
                    );
                    return Ok(Next::To(Stage::Challenged));
                }
                if self.is_terminal("success", &tx)? {
                    if state.verified == Some(false) {
                        self.trouble(
                            "the hub's invalid assertion paid out before a challenge confirmed"
                                .into(),
                        );
                    }
                    return Ok(self.paid(state, "success", tx, height));
                }
            }
            Stage::Challenged => {
                if self.is_terminal("failure", &tx)? {
                    return Ok(self.paid(state, "failure", tx, height));
                }
                for p in self
                    .o
                    .set
                    .iter()
                    .filter(|p| p.name.starts_with("wrongly_challenged_"))
                {
                    if self.is_terminal(&p.name, &tx)? {
                        return Ok(self.paid(state, &p.name, tx, height));
                    }
                }
            }
            _ => bail!("unexpected observation stage"),
        }
        self.trouble(format!(
            "the dispute output went to an unrecognized transaction {}",
            tx.compute_txid()
        ));
        Ok(Next::To(Stage::Lost))
    }
    fn plan_terminal(&self, state: &mut State, name: &str, due: u32) -> Result<Next> {
        let p = presign(&self.o.set, name)?;
        let tx = dispute::add_fee(
            self.o.finish(name, vec![])?,
            p.prevout.clone(),
            fee_coin(state)?,
            &self.keys.party.payment,
        )?;
        state.payout = Some(tx);
        state.payout_name = Some(name.into());
        state.payout_height = None;
        state.next_at = Some(due);
        // Broadcast happens only in the subsequent, durably stored stage.
        Ok(Next::To(if name == "success" {
            Stage::Succeeding
        } else {
            Stage::TimingOut
        }))
    }
    async fn watch(&self, stage: Stage, state: &mut State, tip: u32) -> Result<Next> {
        let (parent, height) = self.parent(stage, state)?;
        let op = OutPoint {
            txid: parent.compute_txid(),
            vout: 0,
        };
        let from = state.from.unwrap_or(height).max(height);
        if let Some((height, tx)) =
            first_spend(self.chain, op, &parent.output[0].script_pubkey, from..=tip).await?
        {
            return self.observed(stage, state, height, tx);
        }
        state.from = Some(rescan(from, tip));
        if stage == Stage::Challenging {
            if state.next_at.is_none_or(|due| tip.saturating_add(1) >= due) {
                return self.post_challenge(state, height, tip).await;
            }
            return Ok(Next::Save);
        }
        if stage == Stage::Asserted && state.verified == Some(false) {
            // One made before, its assertion's block since replaced: the
            // same again, never revealed anew.
            let challenge = match state.challenge.take() {
                Some(made) => made,
                None => self.o.challenge(
                    &self.keys,
                    state.assertion.as_ref().context("no assertion")?,
                )?,
            };
            state.next_at = Some(height + blocks(challenge.input[0].sequence)?);
            state.challenge = Some(challenge);
            return Ok(Next::To(Stage::Challenging));
        }
        let name = match stage {
            Stage::Requested => "request_timeout",
            Stage::Answered => "respond_timeout",
            Stage::Asserted => {
                ensure!(
                    state.verified == Some(true),
                    "assertion validity is unknown"
                );
                // Her claim never published (the hub reclaimed its slot):
                // nothing of it hers but her reserve's dust, so the hub
                // posts it, its reserve paid by it.
                if state.reclaimed {
                    return Ok(Next::Save);
                }
                "success"
            }
            Stage::Challenged => "failure",
            _ => bail!("no timeout at {}", stage.as_str()),
        };
        let due = height
            .checked_add(blocks(presign(&self.o.set, name)?.tx.input[0].sequence)?)
            .context("deadline overflow")?;
        state.next_at = Some(due);
        if tip.saturating_add(1) < due {
            return Ok(Next::Save);
        }
        self.plan_terminal(state, name, due)
    }
    /// Her coin for a challenge's fee child set aside while a false
    /// assertion may still call for one: from her publication until the
    /// hub's assertion verifies, or the child takes it. Set aside again at
    /// each round, as a reorganization frees it; freed otherwise, and the
    /// child's coins once the challenge is over. Without such a coin, she
    /// is told. Left as it is while her publication is signed, the first
    /// time or bumped: neither set aside nor freed for her publication to
    /// take.
    fn hold_challenge_coin(&self, stage: Stage, state: &State) -> Result<()> {
        if stage == Stage::Signing {
            return Ok(());
        }
        let marker = challenge_marker(&self.id);
        let possible = match stage {
            Stage::Published | Stage::Requested | Stage::Answered => true,
            Stage::Asserted => state.verified != Some(true),
            Stage::Challenging => state.challenge_child.is_none(),
            _ => false,
        };
        if !possible {
            // Nothing under it, or freed already: no error.
            let _ = self.wallet.abandon_silent(&marker);
            if let Some(child) = state.challenge_child.as_ref() {
                if stage != Stage::Challenging {
                    let _ = self.wallet.abandon_silent(&child.compute_txid());
                }
            }
            return Ok(());
        }
        let coins = self.wallet.silent_coins()?;
        if coins.iter().any(|c| c.spending == Some(marker)) {
            return Ok(());
        }
        let need = challenge_reserve(self.rate);
        let free = super::coins(self.wallet, self.dir)?;
        match free
            .iter()
            .filter(|c| c.value >= need.to_sat())
            .min_by_key(|c| c.value)
        {
            // Taken meanwhile by a spend of hers: set aside next round.
            Some(coin) => match self.wallet.reserve_silent(&coin.outpoint, &marker) {
                Ok(()) => tracing::info!(
                    id = %crate::hex_id(&self.id), coin = %coin.outpoint, value = coin.value,
                    "a coin set aside for a challenge's fee"
                ),
                Err(e) => tracing::warn!(
                    id = %crate::hex_id(&self.id), coin = %coin.outpoint,
                    "a coin not set aside for a challenge's fee: {e}"
                ),
            },
            // Not what stops a step: she was told before forcing the close
            // ([`challenge_shortfall`]).
            None => tracing::warn!(
                id = %crate::hex_id(&self.id), need = need.to_sat(),
                "no coin of hers to set aside for a challenge's fee: a challenge pays what her coins hold"
            ),
        }
        Ok(())
    }

    /// What her challenge wins, its fee child's fee at most: her claim
    /// past the hub's response, and the hub's reserve.
    fn challenge_stake(&self, state: &State) -> Result<Amount> {
        let (b, _) = HubProves::requested_b(state.r.as_ref().context("no R")?)?;
        let (responded, _) =
            HubProves::responded_b(state.response.as_ref().context("no response")?)?;
        let unit = Amount::from_sat(self.o.base.parts().graph.unit_sats);
        Ok(unit * u64::from(b.saturating_sub(responded)) + self.o.babe.spec.terms.reserve_hub)
    }

    /// Her challenge, once valid, posted with a fee child as one package:
    /// at the rate [`deadline::rate`] gives for the blocks left until the
    /// hub's `success` is valid, made again at a higher one while it is in
    /// no block, its fee at most what the challenge wins
    /// ([`Self::challenge_stake`]), she told above that. Its coins: the
    /// one set aside, else her smallest that pays; a replacement spends
    /// the last child's, and one more of hers if short (her coins are in
    /// blocks: BIP125 takes them). A child made is kept, its coins freed
    /// of the last, before a signature of them leaves; the next round
    /// signs and broadcasts it.
    async fn post_challenge(&self, state: &mut State, asserted_at: u32, tip: u32) -> Result<Next> {
        let challenge = state.challenge.clone().context("no pinned challenge")?;
        let g = &self.o.base.parts().graph;
        let wait = blocks(presign(&self.o.set, "success")?.tx.input[0].sequence)?;
        let deadline = asserted_at
            .saturating_add(wait)
            .saturating_sub(1)
            .saturating_sub(g.deadline_margin);
        let left = deadline.saturating_sub(tip);
        let opened = state.next_at.unwrap_or(asserted_at);
        if tip >= opened.saturating_add(deadline.saturating_sub(opened) / 2) {
            self.trouble(format!(
                "her challenge is in no block halfway to block {deadline}, when the hub's \
                 payout becomes valid: its fee rises as that nears"
            ));
        }
        let estimate = self
            .chain
            .fee_rate(deadline::target(left))
            .await
            .ok()
            .flatten();
        let one = FeeRate::from_sat_per_vb_u32(1);
        let mut rate = deadline::rate(
            estimate,
            if estimate.is_some() { one } else { self.rate },
            left,
        );
        let old = state.challenge_child.clone();
        if rate <= one && old.is_none() {
            self.broadcast(&challenge).await;
            return Ok(Next::Save);
        }
        let presigned = presign(&self.o.set, "challenge_assert")?;
        let paid: Amount = challenge.output.iter().map(|o| o.value).sum();
        let link_fee = presigned
            .prevout
            .value
            .checked_sub(paid)
            .context("a challenge paying out more")?;
        let fee_script = dispute::fee_script(&self.keys.party.payment)?;
        let anchor = challenge
            .output
            .get(dispute::ANCHOR_VOUT as usize)
            .context("a challenge without an anchor")?;
        ensure!(
            anchor.script_pubkey == fee_script,
            "the challenge's anchor is not hers"
        );
        let output = |op: &OutPoint| -> Result<Option<(OutPoint, TxOut)>> {
            Ok(self.wallet.silent_coin_output(op)?.map(|out| (*op, out)))
        };
        // Its coins: the last child's still hers, else the one set aside.
        let mut coins = Vec::new();
        match &old {
            Some(old) => {
                for input in old.input.iter().skip(1) {
                    coins.extend(output(&input.previous_output)?);
                }
            }
            None => {
                let marker = challenge_marker(&self.id);
                let held = self.wallet.silent_coins()?;
                if let Some(c) = held.iter().find(|c| c.spending == Some(marker)) {
                    coins.extend(output(&c.outpoint)?);
                }
            }
        }
        let change = match &old {
            Some(old) => old.output[0].script_pubkey.clone(),
            None => self.wallet.fallback_address()?.script_pubkey(),
        };
        let key = &self.keys.party.payment;
        let sign = |tx: &mut Transaction, prevouts: &[TxOut]| {
            for i in 0..tx.input.len() {
                if prevouts[i].script_pubkey == fee_script {
                    dispute::sign_fee_input(key, tx, i, prevouts)?;
                } else {
                    // Her wallet's signature, of a key spend, at broadcast.
                    tx.input[i].witness = Witness::from_slice(&[[0u8; 64]]);
                }
            }
            Ok(())
        };
        let pair_vb = |child: &Transaction| challenge.vsize() as u64 + child.vsize() as u64;
        let build = |coins: &[(OutPoint, TxOut)], rate| {
            dispute::link_child(&challenge, link_fee, coins, change.clone(), rate, sign)
        };
        // Short: one coin more of hers, the smallest that pays, else her
        // largest.
        if build(&coins, rate).is_err() {
            let have: Amount = coins.iter().map(|c| c.1.value).sum();
            let need = rate
                .fee_vb(challenge.vsize() as u64 + 200)
                .context("fee overflow")?
                .checked_sub(link_fee + have)
                .unwrap_or(Amount::ZERO)
                + DUST;
            let mut free = super::coins(self.wallet, self.dir)?;
            free.sort_by_key(|c| c.value);
            let pick = (free.iter().find(|c| c.value >= need.to_sat())).or(free.last());
            if let Some(c) = pick {
                coins.extend(output(&c.outpoint)?);
            }
        }
        // Its rate the stake's at most, and what her coins pay at most:
        // weighed at the floor, its size the same at any rate.
        let stake = self.challenge_stake(state)?;
        let child = build(&coins, one).and_then(|sized| {
            let pair = (pair_vb(&sized) * 4).max(1);
            let per_kwu = |sat: Amount| FeeRate::from_sat_per_kwu(sat.to_sat() * 1_000 / pair);
            let most = per_kwu(stake + link_fee);
            if rate > most {
                self.trouble(format!(
                    "her challenge would pay {rate} to be in a block by block {deadline}, \
                     more than the {stake} it wins: it pays {most}"
                ));
                rate = most;
            }
            let have: Amount = coins.iter().map(|c| c.1.value).sum::<Amount>() + anchor.value;
            let dust = change.minimal_non_dust();
            let afford = per_kwu((have + link_fee).checked_sub(dust).unwrap_or(Amount::ZERO));
            if rate > afford {
                self.trouble(format!(
                    "too little of hers on Bitcoin for her challenge to pay {rate} to be in a \
                     block by block {deadline}: it pays {afford}"
                ));
                rate = afford;
            }
            build(&coins, rate)
        });
        let child = match child {
            Ok(child) => child,
            Err(e) => {
                self.trouble(format!(
                    "too little of hers on Bitcoin to pay her challenge's fee: {e:#}"
                ));
                return self.post_child(state, &challenge).await;
            }
        };
        let fee = |tx: &Transaction| {
            let value: Amount = tx
                .input
                .iter()
                .filter_map(|i| {
                    let op = i.previous_output;
                    let anchor = (op.txid == challenge.compute_txid()).then(|| anchor.clone());
                    anchor.or_else(|| coins.iter().find(|c| c.0 == op).map(|c| c.1.clone()))
                })
                .map(|o| o.value)
                .sum();
            let paid: Amount = tx.output.iter().map(|o| o.value).sum();
            value.checked_sub(paid).unwrap_or(Amount::ZERO)
        };
        let child = match &old {
            None => Some(child),
            Some(old) => {
                let (new, before) = (fee(&child), fee(old));
                if new <= before {
                    None
                } else if deadline::replaces(new, child.vsize() as u64, before) {
                    Some(child)
                } else {
                    // The least that replaces it, as a rate of the pair.
                    let least = before.to_sat() + child.vsize() as u64 + link_fee.to_sat();
                    let least =
                        FeeRate::from_sat_per_kwu((least * 1_000).div_ceil(pair_vb(&child) * 4));
                    build(&coins, least).ok().filter(|c| {
                        fee(c) <= stake && deadline::replaces(fee(c), c.vsize() as u64, before)
                    })
                }
            }
        };
        let Some(child) = child else {
            return self.post_child(state, &challenge).await;
        };
        // Its coins freed of the last child, or of being set aside, and the
        // new one kept: signed and broadcast next round.
        if let Some(old) = &old {
            let _ = self.wallet.abandon_silent(&old.compute_txid());
        }
        let _ = self.wallet.abandon_silent(&challenge_marker(&self.id));
        tracing::info!(
            id = %crate::hex_id(&self.id), txid = %child.compute_txid(), %rate,
            "her challenge's fee child made"
        );
        state.challenge_child = Some(child);
        Ok(Next::To(Stage::Challenging))
    }

    /// Her challenge broadcast with its last fee child, if any, as one
    /// package: the child's coins of her wallet signed again, which sets
    /// them aside again. A coin of it taken meanwhile, the child is made
    /// again (once a run).
    async fn post_child(&self, state: &mut State, challenge: &Transaction) -> Result<Next> {
        let Some(mut child) = state.challenge_child.clone() else {
            self.broadcast(challenge).await;
            return Ok(Next::Save);
        };
        let fee_script = dispute::fee_script(&self.keys.party.payment)?;
        let mut prevouts = Vec::new();
        for input in &child.input {
            let op = input.previous_output;
            let out = match op.txid == challenge.compute_txid() {
                true => challenge.output.get(op.vout as usize).cloned(),
                false => self.wallet.silent_coin_output(&op)?,
            };
            prevouts.push(out);
        }
        let prevouts: Option<Vec<TxOut>> = prevouts.into_iter().collect();
        let mut taken = prevouts.is_none();
        if let Some(prevouts) = &prevouts {
            for i in 0..child.input.len() {
                if prevouts[i].script_pubkey == fee_script {
                    continue;
                }
                match self.wallet.sign_input(&child, prevouts, i) {
                    Ok(signed) => child.input[i].witness = signed.input[i].witness.clone(),
                    Err(
                        e @ (licash_wallet::Error::Invalid(_) | licash_wallet::Error::NoteTaken),
                    ) => {
                        tracing::warn!(id = %crate::hex_id(&self.id), "a coin of her challenge's child taken: {e}");
                        taken = true;
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if taken {
            ensure!(
                !self.rebuilt.swap(true, Ordering::SeqCst),
                "a coin of her challenge's child taken again"
            );
            let _ = self.wallet.abandon_silent(&child.compute_txid());
            state.challenge_child = None;
            return Ok(Next::To(Stage::Challenging));
        }
        if let Err(e) = self
            .chain
            .broadcast_package(&[challenge.clone(), child.clone()])
            .await
        {
            self.trouble(format!("{} and its child: {e}", challenge.compute_txid()));
        }
        Ok(Next::Save)
    }

    async fn finishing(&self, state: &mut State, tip: u32) -> Result<Next> {
        let name = state.payout_name.as_deref().context("no pinned terminal")?;
        let stage = match name {
            "request_timeout" => Stage::Requested,
            "respond_timeout" => Stage::Answered,
            "failure" => Stage::Challenged,
            "success" => Stage::Asserted,
            _ => bail!("not a locally posted terminal"),
        };
        let (parent, height) = self.parent(stage, state)?;
        let from = state.from.unwrap_or(height).max(height);
        let op = OutPoint {
            txid: parent.compute_txid(),
            vout: 0,
        };
        if let Some((height, tx)) =
            first_spend(self.chain, op, &parent.output[0].script_pubkey, from..=tip).await?
        {
            return self.observed(stage, state, height, tx);
        }
        state.from = Some(rescan(from, tip));
        if state.next_at.is_none_or(|due| tip.saturating_add(1) >= due) {
            self.broadcast(state.payout.as_ref().context("no pinned payout")?)
                .await;
        }
        Ok(Next::Save)
    }
    fn sweep(&self, state: &mut State, tip: u32) -> Result<Next> {
        let payout = state.payout.as_ref().context("no payout")?;
        let paid = payout.compute_txid();
        let height = state.payout_height.context("payout is unconfirmed")?;
        let name = state
            .payout_name
            .as_deref()
            .context("payout provenance is unknown")?;
        let (amount, wire, value_key, signer) = match name {
            "request_timeout" | "respond_timeout" | "failure" => {
                let r = state.r.as_ref().context("no R")?;
                let (b, _) = HubProves::read_request(&link_args(r))?;
                (
                    state.amount,
                    HubProves::b_wire(&b),
                    &self.o.holder.b_key,
                    Role::User,
                )
            }
            "success" => {
                let (b, wire) =
                    HubProves::responded_b(state.response.as_ref().context("no response")?)?;
                (b, wire, &self.o.hub.b_key, Role::Hub)
            }
            n if n.starts_with("wrongly_challenged_") => {
                presign(&self.o.set, n)?;
                let (b, wire) =
                    HubProves::responded_b(state.response.as_ref().context("no response")?)?;
                (b, wire, &self.o.hub.b_key, Role::Hub)
            }
            _ => bail!("unknown payout provenance"),
        };
        let payout = Payout {
            transaction: payout,
            value_key,
            signer,
            amount,
            wire: &wire,
        };
        let owned = self.o.babe.core().owned(
            payout.transaction,
            payout.value_key,
            payout.signer,
            payout.amount,
            Role::User,
        )?;
        let left = left_in_request(state, self.rate)?;
        if owned.is_empty() && left.is_empty() {
            // Nothing of the payout is hers (a corrected zero): done.
            state.swept_height = Some(height);
            state.next_at = None;
            return Ok(Next::To(Stage::Done));
        }
        let sweep = dispute::sweep_with(
            &self.o.babe,
            &self.keys.party.payment,
            payout,
            Role::User,
            self.wallet.fallback_address()?.script_pubkey(),
            self.rate,
            &left,
        )?;
        let delay = sweep
            .input
            .iter()
            .map(|i| blocks(i.sequence))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        let due = height.checked_add(delay).context("sweep delay overflow")?;
        state.next_at = Some(due);
        if tip.saturating_add(1) < due {
            return Ok(Next::Save);
        }
        // One made before of the same payout, its block since replaced:
        // the same again, as a block may hold it.
        let made = (state.sweep.take())
            .filter(|old| (old.input.iter()).any(|i| i.previous_output.txid == paid));
        state.sweep = Some(made.unwrap_or(sweep));
        state.from = Some(height);
        state.next_at = None;
        Ok(Next::To(Stage::Sweeping))
    }
    async fn swept(&self, state: &mut State, tip: u32) -> Result<Next> {
        let sweep = state.sweep.as_ref().context("no pinned sweep")?;
        let payout = state.payout.as_ref().context("no payout")?;
        let coin = sweep.input.first().context("empty sweep")?.previous_output;
        // A payout's output, or her request's left to her (its purse).
        let r = state.r.as_ref().context("no R")?;
        let from_tx = [payout, r]
            .into_iter()
            .find(|tx| tx.compute_txid() == coin.txid)
            .context("unknown sweep input")?;
        let script = &from_tx
            .output
            .get(coin.vout as usize)
            .context("unknown sweep input")?
            .script_pubkey;
        let from = state
            .from
            .unwrap_or(state.payout_height.context("payout is unconfirmed")?);
        match first_spend(self.chain, coin, script, from..=tip).await? {
            Some((height, tx)) if tx.compute_txid() == sweep.compute_txid() => {
                // Done once as deep as F had to be: looked for again until
                // then, from where it was, and broadcast again if it left
                // the chain.
                if !deep(height, tip, self.o.base.parts().chain.funding_depth) {
                    return Ok(Next::Wait);
                }
                state.swept_height = Some(height);
                state.next_at = None;
                Ok(Next::To(Stage::Done))
            }
            Some((_, tx)) => {
                self.trouble(format!("her payout went to {} instead", tx.compute_txid()));
                Ok(Next::To(Stage::Lost))
            }
            None => {
                state.from = Some(rescan(from, tip));
                self.broadcast(sweep).await;
                Ok(Next::Save)
            }
        }
    }
}

fn blocks(sequence: Sequence) -> Result<u32> {
    match sequence.to_relative_lock_time() {
        Some(bitcoin::relative::LockTime::Blocks(h)) => Ok(h.value() as u32),
        None => Ok(0),
        Some(other) => bail!("a relative lock of {other}, not in blocks"),
    }
}
/// Her request's anchor and purse, if her claim's slot was reclaimed, P
/// never published to spend them, and her payout spent neither as its
/// fee coin: hers alone, swept with her payout. Each worth more than its
/// input's fee at `rate`.
fn left_in_request(state: &State, rate: FeeRate) -> Result<Vec<(OutPoint, TxOut)>> {
    if !state.reclaimed {
        return Ok(Vec::new());
    }
    let r = state.r.as_ref().context("no R")?;
    let payout = state.payout.as_ref().context("no payout")?;
    let spent: Vec<OutPoint> = payout.input.iter().map(|i| i.previous_output).collect();
    Ok([1, PURSE_VOUT]
        .into_iter()
        .filter_map(|vout| {
            let op = OutPoint {
                txid: r.compute_txid(),
                vout,
            };
            let out = r.output.get(vout as usize)?;
            // Its fee leaf spent: a signature, the script and a control
            // block, about 75 vB.
            let worth = rate.fee_vb(75).is_some_and(|fee| out.value > fee);
            (worth && !spent.contains(&op)).then(|| (op, out.clone()))
        })
        .collect())
}

/// Her terminal's fee coin: P's, or, P never published (her claim's slot
/// reclaimed), her purse, R's, whole.
fn fee_coin(state: &State) -> Result<(OutPoint, TxOut)> {
    let (tx, vout) = if state.reclaimed {
        (state.r.as_ref().context("no R")?, PURSE_VOUT)
    } else {
        (state.p.as_ref().context("no P")?, 1)
    };
    Ok((
        OutPoint {
            txid: tx.compute_txid(),
            vout,
        },
        tx.output
            .get(vout as usize)
            .context("no terminal fee coin")?
            .clone(),
    ))
}
async fn confirmed_at(
    chain: &impl Chain,
    txid: Txid,
    script: &bitcoin::Script,
    heights: std::ops::RangeInclusive<u32>,
) -> Result<Option<u32>> {
    for height in heights {
        let hash = chain.hash(height).await.map_err(|e| anyhow!(e))?;
        if let Some(filter) = chain.filter(&hash).await.map_err(|e| anyhow!(e))? {
            if !bitcoin::bip158::BlockFilter::new(&filter)
                .match_any(&hash, std::iter::once(script.as_bytes()))?
            {
                continue;
            }
        }
        let block = chain.block(&hash).await.map_err(|e| anyhow!(e))?;
        ensure!(
            block.block_hash() == hash && block.check_merkle_root(),
            "block {hash} does not match its header"
        );
        if block.txdata.iter().any(|tx| tx.compute_txid() == txid) {
            return Ok(Some(height));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod driver_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_watch_looks_again_at_the_last_blocks_it_saw() {
        assert_eq!(rescan(100, 200), 198);
        assert_eq!(rescan(199, 200), 199);
        assert_eq!(rescan(0, 1), 0);
        assert_eq!(rescan(150, 150), 150);
    }
    #[test]
    fn a_sweep_is_done_only_as_deep_as_f_had_to_be() {
        // Two deep wanted: mined at the tip, not yet; one block on, done.
        assert!(!deep(100, 100, 2));
        assert!(deep(100, 101, 2));
        // One at least, whatever the chain's terms say.
        assert!(deep(100, 100, 0));
        assert!(deep(100, 100, 1));
        // A tip below it, after a reorganization: not deep.
        assert!(!deep(100, 99, 1));
    }
    #[test]
    fn every_stage_reads_back_and_only_done_or_lost_is_over() {
        for stage in Stage::ALL {
            assert_eq!(Stage::parse(stage.as_str()).unwrap(), stage);
            assert_eq!(stage.over(), matches!(stage, Stage::Done | Stage::Lost));
        }
        assert!(Stage::parse("forgotten").is_err());
    }
    #[test]
    fn state_is_whole_versioned_and_requires_payout_provenance() {
        let mut row = RecoveryRow {
            id: [1; 32],
            format: FORMAT,
            stage: Stage::Requested.as_str().into(),
            irrevocable: true,
            error: None,
            state: State::default().encode(),
            created_at: 0,
        };
        assert_eq!(State::decode(&row).unwrap().encode(), row.state);
        row.format = 1;
        assert!(State::decode(&row).is_err());
        row.format = FORMAT;
        row.state = br#"{"later":1}"#.to_vec();
        assert!(State::decode(&row).is_err());
        row.state = vec![];
        assert!(State::decode(&row).is_err());
    }
    #[test]
    fn a_publication_fee_allowed_is_written_only_once_set() {
        let plain = String::from_utf8(State::default().encode()).unwrap();
        assert!(!plain.contains("p_allowance"));
        let state = State {
            p_allowance: Some(40_000),
            ..State::default()
        };
        let row = RecoveryRow {
            id: [1; 32],
            format: FORMAT,
            stage: Stage::Published.as_str().into(),
            irrevocable: true,
            error: None,
            state: state.encode(),
            created_at: 0,
        };
        assert_eq!(State::decode(&row).unwrap().p_allowance, Some(40_000));
    }
    #[test]
    fn a_rewind_forgets_what_she_saw_above_the_block_that_stood_and_keeps_what_she_signed() {
        let tx = |n: u8| Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::from_consensus(u32::from(n)),
            input: vec![],
            output: vec![],
        };
        let paid = State {
            r: Some(tx(1)),
            r_height: Some(100),
            p: Some(tx(2)),
            p_height: Some(101),
            response: Some(tx(3)),
            response_height: Some(105),
            reduced: Some(7),
            deadline: Some(150),
            assertion: Some(tx(4)),
            assertion_height: Some(110),
            verified: Some(false),
            challenge: Some(tx(5)),
            challenge_child: Some(tx(6)),
            challenge_height: Some(115),
            payout: Some(tx(7)),
            payout_name: Some("failure".into()),
            payout_height: Some(120),
            sweep: Some(tx(8)),
            from: Some(125),
            next_at: Some(126),
            ..State::default()
        };
        let signed = |s: &State| {
            assert_eq!((&s.r, &s.p), (&paid.r, &paid.p));
            assert_eq!(
                (&s.challenge, &s.challenge_child),
                (&paid.challenge, &paid.challenge_child)
            );
            assert_eq!(&s.sweep, &paid.sweep);
        };
        // All she saw stands: her scan only looks again from there.
        let mut s = paid.clone();
        assert_eq!(rewind(Stage::Sweeping, &mut s, 122), Stage::Sweeping);
        assert_eq!(
            (s.from, s.payout_height, s.next_at),
            (Some(123), Some(120), Some(126))
        );
        // Its payout's block replaced: watched for again from its challenge.
        let mut s = paid.clone();
        assert_eq!(rewind(Stage::Sweeping, &mut s, 119), Stage::Challenged);
        assert_eq!(
            (&s.payout, &s.payout_name, s.payout_height),
            (&None, &None, None)
        );
        assert_eq!(
            (s.challenge_height, s.from, s.next_at),
            (Some(115), Some(120), None)
        );
        signed(&s);
        // Its challenge's: watched for from the assertion, kept to post.
        let mut s = paid.clone();
        assert_eq!(rewind(Stage::Sweeping, &mut s, 112), Stage::Asserted);
        assert_eq!((s.challenge_height, s.assertion_height), (None, Some(110)));
        signed(&s);
        // The assertion's: what it verified to forgotten too.
        let mut s = paid.clone();
        assert_eq!(rewind(Stage::Challenged, &mut s, 107), Stage::Answered);
        assert_eq!(
            (&s.assertion, s.verified, s.response_height),
            (&None, None, Some(105))
        );
        signed(&s);
        // The answer's: what it said she is owed.
        let mut s = paid.clone();
        assert_eq!(rewind(Stage::Asserted, &mut s, 104), Stage::Requested);
        assert_eq!(
            (&s.response, s.reduced, s.deadline, s.p_height),
            (&None, None, None, Some(101))
        );
        signed(&s);
        // Her publication's, or her request's: published again.
        let mut s = paid.clone();
        assert_eq!(rewind(Stage::Answered, &mut s, 100), Stage::Published);
        assert_eq!((s.r_height, s.p_height), (Some(100), None));
        let mut s = State {
            reclaimed: true,
            ..paid.clone()
        };
        assert_eq!(rewind(Stage::Requested, &mut s, 99), Stage::Published);
        assert_eq!((s.r_height, s.reclaimed, s.from), (None, false, Some(100)));
        assert_eq!(s.next_at, Some(100));
        signed(&s);
        // Her slot reclaimed, at a height not kept: found again however
        // little was replaced.
        let mut s = State {
            reclaimed: true,
            p_height: None,
            ..paid.clone()
        };
        assert_eq!(rewind(Stage::Asserted, &mut s, 130), Stage::Published);
        assert_eq!(
            (s.r_height, s.reclaimed, s.next_at),
            (Some(100), false, None)
        );
        // Her request's own block replaced, her scan not started: from the
        // block after the one that stood, not from F.
        let mut s = State {
            from: None,
            ..paid.clone()
        };
        assert_eq!(rewind(Stage::Requested, &mut s, 90), Stage::Published);
        assert_eq!((s.from, s.next_at), (Some(91), Some(91)));
        // Paid with no answer: watched for from her request.
        let mut s = State {
            r: Some(tx(1)),
            r_height: Some(100),
            p_height: Some(101),
            payout: Some(tx(7)),
            payout_name: Some("request_timeout".into()),
            payout_height: Some(120),
            ..State::default()
        };
        assert_eq!(rewind(Stage::TimedOut, &mut s, 110), Stage::Requested);
        assert_eq!((s.payout_height, s.from), (None, None));
    }
    #[test]
    fn only_a_recovery_published_and_not_over_follows_its_tips() {
        let follows: Vec<_> = (Stage::ALL.into_iter()).filter(|&s| watched(s)).collect();
        assert_eq!(
            follows,
            [
                Stage::Published,
                Stage::Requested,
                Stage::TimingOut,
                Stage::TimedOut,
                Stage::Sweeping,
                Stage::Answered,
                Stage::Asserted,
                Stage::Succeeding,
                Stage::Challenging,
                Stage::Challenged,
                Stage::Paid,
            ]
        );
    }
    #[test]
    fn a_publication_climbs_a_rung_a_quarter_of_its_window() {
        // Window 100..=108: quarters at 102, 104 and 106.
        assert_eq!(rung(100, 108, 100), 0);
        assert_eq!(rung(100, 108, 101), 0);
        assert_eq!(rung(100, 108, 102), 1);
        assert_eq!(rung(100, 108, 104), 2);
        assert_eq!(rung(100, 108, 106), 3);
        assert_eq!(rung(100, 108, 200), 3);
        // A tip before it, or a window of nothing: at once the last rung,
        // or none, never a panic.
        assert_eq!(rung(100, 108, 90), 0);
        assert_eq!(rung(100, 100, 100), 0);
        assert_eq!(rung(100, 100, 101), 3);
        assert_eq!(rung(108, 100, 101), 0);
    }
}
