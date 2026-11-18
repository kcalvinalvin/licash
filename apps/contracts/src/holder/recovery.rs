//! A holder's hub-proves exit. The filing and request are pinned before
//! one-time signing; each transaction is durable before broadcast. The
//! holder verifies the hub's proof, but never computes a recovery proof.
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use bitcoin::{Amount, FeeRate, OutPoint, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use licash::claim::{self, Item, UnsignedClaim};
use licash::recovery::{Filing, Publication, Witness as FilingWitness};
use licash::request::Request;
use licash_contract::graph::{link_args, HubProves, PURSE_VOUT};
use licash_contract::{
    dispute::{self, presign, Payout},
    keys::HolderKeys,
    opening::{Context, Holder, Opening},
};
use licash_wallet::{first_spend, ClaimNote, ClaimSlot, Transport, Wallet};
use lichen_contract::Role;
use serde::{Deserialize, Serialize};

use super::claims::{self, ClaimTerms};
use super::store::{RecoveryRow, Store};
use super::{Chain, Fate};

/// The format of a recovery's stored state.
const FORMAT: i64 = 2;
const TIMEOUT_FEE_MARGIN: u64 = 2;
const OVERLAP: u32 = 3;
const DUST: Amount = Amount::from_sat(330);

fn rescan(from: u32, tip: u32) -> u32 {
    tip.saturating_add(1).saturating_sub(OVERLAP).max(from)
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
    #[serde(with = "consensus")]
    payout: Option<Transaction>,
    payout_name: Option<String>,
    payout_height: Option<u32>,
    #[serde(with = "consensus")]
    sweep: Option<Transaction>,
    swept_height: Option<u32>,
    from: Option<u32>,
    next_at: Option<u32>,
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
    ensure!(store.recovery(id)?.is_some(), "no recovery of it");
    ensure!(
        store.revoke_recovery(id, Stage::GivingUp.as_str(), Some("given up"))?,
        "past giving up: her request is signed"
    );
    finish_giving_up(wallet, &store, id)
}
fn finish_giving_up<T: Transport>(wallet: &Wallet<T>, store: &Store, id: &[u8; 32]) -> Result<()> {
    wallet.unclaim_notes(id)?;
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
            let next = match stage {
                Stage::Claiming => self.claim(&mut state, tip)?,
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
    fn claim(&self, state: &mut State, tip: u32) -> Result<Next> {
        // Before any note of hers is taken.
        self.o.forcible()?;
        let parts = self.o.base.parts();
        let keyset = *parts.keyset.id();
        let mut notes = self.wallet.claimed_notes(&self.id, &keyset)?;
        if notes.is_empty() {
            notes = self.wallet.claim_notes(&self.id, &self.slots(tip)?)?;
            self.took.store(!notes.is_empty(), Ordering::SeqCst);
        }
        if notes.is_empty() {
            self.give_back("no notes at its hub to claim")?;
            bail!("no notes at its hub to claim");
        }
        // A note beyond what is left of the cap is claimed for that much.
        let held: u64 = notes.iter().map(|n| n.note.message().denomination()).sum();
        state.amount = parts.cap.min(u32::try_from(held).unwrap_or(u32::MAX));
        let filing = match self.filing(&notes, state.amount) {
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

    fn filing(&self, notes: &[ClaimNote], amount: u32) -> Result<Filing> {
        let p = self.o.base.parts();
        let limit = p.limits.max_authorizations as usize;
        let messages: Vec<_> = notes.iter().map(|n| n.note.message().clone()).collect();
        let mut authorizations = messages.clone();
        authorizations.sort_by_key(|m| m.identifier().ok());
        let claim = UnsignedClaim::new(
            claim::Context {
                base: self.o.base.digest(),
                deposit: p.deposit,
                cap: p.cap,
            },
            &p.keyset,
            amount,
            claim::Parts {
                items: messages.into_iter().map(Item::Settled).collect(),
                sources: vec![],
                activations: vec![],
                authorizations,
            },
            limit,
        )?;
        let publication = Publication::sign(claim, &self.wallet.hold_keys(notes)?, limit)?;
        let filing = Filing::new(
            publication,
            FilingWitness {
                notes: notes.iter().map(|n| n.note.clone()).collect(),
                sources: vec![],
                activations: vec![],
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
        self.build_p(&r, &self.filing_of(state)?)?;
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
        let (p, prevouts) = self.build_p(&r, &self.filing_of(state)?)?;
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
    fn build_p(&self, r: &Transaction, filing: &Filing) -> Result<(Transaction, Vec<TxOut>)> {
        let request = presign(&self.o.set, "request")?;
        let slot = request.tx.output.get(2).context("no claim slot")?.clone();
        let anchor = r.output.get(1).context("no request anchor")?.clone();
        let fee_script = dispute::fee_script(&self.keys.party.payment)?;
        ensure!(
            anchor.script_pubkey == fee_script,
            "request anchor is not hers"
        );
        let fee_coin = TxOut {
            value: self.terminal_fee()?,
            script_pubkey: fee_script,
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
        for coin in std::iter::once(Ok(None)).chain(coins) {
            let mut inputs = base.clone();
            inputs.extend(coin?);
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
            let fee = own.max(package.checked_sub(r_fee).unwrap_or(Amount::ZERO));
            let Some(left) = total.checked_sub(fee_coin.value + fee) else {
                continue;
            };
            if left >= DUST {
                p.output[2].value = left;
            } else {
                p.output.pop();
            }
            // ALL|ACP commits outputs, so sign the final output values.
            let final_carrier = self
                .o
                .publication(&self.keys, &bytes, p.output[1..].to_vec())?;
            p.input[0].witness = final_carrier.input[0].witness.clone();
            return Ok((p, prevouts));
        }
        bail!(
            "too little of hers on Bitcoin to pay her request's fees: \
             sats needed in one coin, her largest {most} sat"
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
        match first_spend(self.chain, slot, &r.output[2].script_pubkey, r_height..=tip).await? {
            Some((height, tx)) if tx.compute_txid() == p.compute_txid() => {
                state.p_height = Some(height);
                state.from = Some(r_height);
                state.next_at = Some(
                    r_height
                        + blocks(presign(&self.o.set, "request_timeout")?.tx.input[0].sequence)?,
                );
                Ok(Next::To(Stage::Requested))
            }
            Some((_, tx)) => bail!("claim slot spent by {}, not P", tx.compute_txid()),
            None => {
                self.keep_p(&p, &state.prevouts);
                self.broadcast(&p).await;
                Ok(Next::Save)
            }
        }
    }
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
                self.broadcast(state.challenge.as_ref().context("no pinned challenge")?)
                    .await;
            }
            return Ok(Next::Save);
        }
        if stage == Stage::Asserted && state.verified == Some(false) {
            let challenge = self.o.challenge(
                &self.keys,
                state.assertion.as_ref().context("no assertion")?,
            )?;
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
        if owned.is_empty() {
            // Nothing of the payout is hers (a corrected zero): done.
            state.swept_height = Some(height);
            state.next_at = None;
            return Ok(Next::To(Stage::Done));
        }
        let sweep = dispute::sweep(
            &self.o.babe,
            &self.keys.party.payment,
            payout,
            Role::User,
            self.wallet.fallback_address()?.script_pubkey(),
            self.rate,
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
        state.sweep = Some(sweep);
        state.from = Some(height);
        state.next_at = None;
        Ok(Next::To(Stage::Sweeping))
    }
    async fn swept(&self, state: &mut State, tip: u32) -> Result<Next> {
        let sweep = state.sweep.as_ref().context("no pinned sweep")?;
        let payout = state.payout.as_ref().context("no payout")?;
        let coin = sweep.input.last().context("empty sweep")?.previous_output;
        let script = &payout
            .output
            .get(coin.vout as usize)
            .context("unknown sweep input")?
            .script_pubkey;
        let from = state
            .from
            .unwrap_or(state.payout_height.context("payout is unconfirmed")?);
        match first_spend(self.chain, coin, script, from..=tip).await? {
            Some((height, tx)) if tx.compute_txid() == sweep.compute_txid() => {
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
fn fee_coin(state: &State) -> Result<(OutPoint, TxOut)> {
    let p = state.p.as_ref().context("no P")?;
    Ok((
        OutPoint {
            txid: p.compute_txid(),
            vout: 1,
        },
        p.output.get(1).context("no terminal fee coin")?.clone(),
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
}
