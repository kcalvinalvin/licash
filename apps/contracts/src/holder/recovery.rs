//! A contract its holder forces closed, her hub silent: her notes at the
//! hub claimed in R, her one-time request, and P, its publication; the
//! hub's window let pass, `request_timeout` paying her the claim's bits
//! and both reserves; those swept to her wallet once the hub's chance to
//! show she signed another `b` is past.
//!
//! Each step is stored before the next starts, so that one stopped at any
//! point carries on where it was. Until her request may be in the next
//! block the recovery may be given up, her notes given back; from the
//! write that makes it irrevocable on, never. Neither P nor the timeout's
//! fee coin is bumped: their fees are those of the rate she forced it at.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use bitcoin::{Amount, FeeRate, OutPoint, ScriptBuf, Transaction, TxOut, Txid, Witness};
use licash::chain::{Inclusion, WitnessInclusion};
use licash::claim::{self, Item, UnsignedClaim};
use licash::recovery::{HolderWitness, ItemWitness, Publication};
use licash::request::{Request, Signer};
use licash::statement::{HubResponse, Observed, StatementInput};
use licash_contract::close;
use licash_contract::dispute::{self, Response};
use licash_contract::keys::HolderKeys;
use licash_contract::opening::{Context, Holder, Opening, Trust};
use licash_wallet::{first_spend, ClaimNote, Transport, Wallet};
use serde::{Deserialize, Serialize};

use super::store::{RecoveryRow, Store};
use super::{Chain, Fate};

/// The format of a recovery's stored state.
const FORMAT: i64 = 1;

/// The most notes a claim takes, as many as a claim's authorizations at
/// most.
const MAX_NOTES: usize = 32;

/// The timeout's fee coin pays this many times its fee at the rate she
/// forced the contract closed at: the rate may rise over the hub's window.
const TIMEOUT_FEE_MARGIN: u64 = 2;

/// The largest block her statement reads R or P in, in bytes.
const MAX_BLOCK: usize = 4_000_000;

/// A taproot output's least value to relay.
const DUST: Amount = Amount::from_sat(330);

/// Where a recovery stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Her notes being claimed: revocable.
    Claiming,
    /// Her notes claimed, the claim signed: revocable.
    Claimed,
    /// Past giving up: her request to sign.
    Irrevocable,
    /// R signed, P built: P to sign.
    Signing,
    /// R and P signed and broadcast, until R is in a block.
    Published,
    /// R in a block: the hub's window running.
    Requested,
    /// The hub's window passed: `request_timeout` broadcast.
    TimingOut,
    /// `request_timeout` in a block: her payout waiting out its delay.
    TimedOut,
    /// Her payout swept to her wallet, until in a block.
    Sweeping,
    /// Her payout in her wallet.
    Done,
    /// The hub answered her request: her statement's input to build from
    /// her notes claimed and the chain's evidence of R and P. A counter,
    /// which this build does not answer yet, stays here.
    Answered,
    /// The contract's output went otherwise than to her request.
    Lost,
    /// Given up, her notes being given back.
    GivingUp,
    /// Her proof of her claim needed before the hub's deadline: made, or
    /// brought from elsewhere, at her word ([`import_proof`]).
    Proving,
    /// Her proof kept: her `assert` of it broadcast until in a block.
    Asserting,
    /// Her `assert` in a block: the hub's chance to challenge it running.
    Asserted,
    /// The hub's chance passed: `success` broadcast until in a block.
    Succeeding,
    /// The hub challenged her `assert`: her answer, from a kept instance
    /// her proof opens, broadcast until in a block.
    Answering,
    /// Her payout after the hub's answer (`success`, or her answer to its
    /// challenge) in a block: waiting out its delay.
    Paid,
}

impl Stage {
    const ALL: [Stage; 19] = [
        Stage::Claiming,
        Stage::Claimed,
        Stage::Irrevocable,
        Stage::Signing,
        Stage::Published,
        Stage::Requested,
        Stage::TimingOut,
        Stage::TimedOut,
        Stage::Sweeping,
        Stage::Done,
        Stage::Answered,
        Stage::Lost,
        Stage::GivingUp,
        Stage::Proving,
        Stage::Asserting,
        Stage::Asserted,
        Stage::Succeeding,
        Stage::Answering,
        Stage::Paid,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Claiming => "claiming",
            Stage::Claimed => "claimed",
            Stage::Irrevocable => "irrevocable",
            Stage::Signing => "signing",
            Stage::Published => "published",
            Stage::Requested => "requested",
            Stage::TimingOut => "timing-out",
            Stage::TimedOut => "timed-out",
            Stage::Sweeping => "sweeping",
            Stage::Done => "done",
            Stage::Answered => "answered",
            Stage::Lost => "lost",
            Stage::GivingUp => "giving-up",
            Stage::Proving => "proving",
            Stage::Asserting => "asserting",
            Stage::Asserted => "asserted",
            Stage::Succeeding => "succeeding",
            Stage::Answering => "answering",
            Stage::Paid => "paid",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|s| s.as_str() == text)
            .with_context(|| format!("a recovery at stage {text}, unknown"))
    }

    /// Whether nothing is left to do for it.
    pub fn over(self) -> bool {
        matches!(self, Stage::Done | Stage::Lost)
    }

    /// Whether the hub answered her request, so that her proof decides.
    pub fn disputed(self) -> bool {
        matches!(
            self,
            Stage::Answered
                | Stage::Proving
                | Stage::Asserting
                | Stage::Asserted
                | Stage::Succeeding
                | Stage::Answering
                | Stage::Paid
        )
    }
}

/// A recovery as a list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovery {
    /// The contract's id.
    pub id: [u8; 32],
    pub stage: Stage,
    /// Past giving it up.
    pub irrevocable: bool,
    /// What her notes claimed come to, in sat, and how many they are.
    pub claimed: u64,
    pub notes: usize,
    /// Her request R and its publication P, once signed.
    pub request: Option<Txid>,
    pub publication: Option<Txid>,
    /// The block R is in, once it is.
    pub requested_at: Option<u32>,
    /// `request_timeout`, once built, and the block it is in.
    pub timeout: Option<Txid>,
    pub timed_out_at: Option<u32>,
    /// Her sweep of her payout, once built, and the block it is in.
    pub sweep: Option<Txid>,
    pub swept_at: Option<u32>,
    /// The first block the next step may be in, if it waits for one.
    pub next_at: Option<u32>,
    /// What stopped its last step, if anything did.
    pub error: Option<String>,
    /// The hub's answer to her request, once in a block, and the last
    /// block her `assert` may be in then.
    pub response: Option<Txid>,
    pub deadline: Option<u32>,
    /// Whether her proof is kept.
    pub proved: bool,
    /// Her `assert`, once built, and the block it is in.
    pub assertion: Option<Txid>,
    pub asserted_at: Option<u32>,
    /// The hub's challenge of it, if any.
    pub challenge: Option<Txid>,
    /// Her payout after the hub's answer, once built, and the block it is
    /// in: `success`, or her answer to its challenge.
    pub payout: Option<Txid>,
    pub paid_at: Option<u32>,
}

/// What a recovery keeps between its steps, its transactions and outputs
/// in consensus hex, the encoding Bitcoin fixes. A field of a later build
/// is refused, not dropped on this one's next save.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct State {
    /// Her notes claimed, a copy of each ([`ClaimNote::to_bytes`]), in
    /// hex, and their total.
    notes: Vec<String>,
    amount: u32,
    /// The signed claim P publishes, in hex.
    publication: Option<String>,
    #[serde(with = "consensus")]
    r: Option<Transaction>,
    /// P, unsigned until `Published`, and the outputs its inputs spend.
    #[serde(with = "consensus")]
    p: Option<Transaction>,
    #[serde(with = "consensus_all")]
    prevouts: Vec<TxOut>,
    /// The blocks F, R, P, the spend of R's output and her sweep are in.
    f_height: Option<u32>,
    r_height: Option<u32>,
    p_height: Option<u32>,
    spent_height: Option<u32>,
    swept_height: Option<u32>,
    /// The spend of R's output: `request_timeout` as she built it, then
    /// as it is in a block, or the hub's answer.
    #[serde(with = "consensus")]
    spend: Option<Transaction>,
    #[serde(with = "consensus")]
    sweep: Option<Transaction>,
    /// The next block to look for what is awaited from.
    from: Option<u32>,
    /// The first block the next step may be in, if it waits for one.
    next_at: Option<u32>,
    // Once the hub answered (none written before, so that a state of its
    // silence reads as the build before this one wrote it): the last block
    // her `assert` may be in; her proof, its points compressed, in hex;
    // her `assert`, the hub's challenge and her payout, each as she built
    // it, then as in a block, and the block.
    #[serde(skip_serializing_if = "Option::is_none")]
    deadline: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proof: Option<String>,
    #[serde(with = "consensus", skip_serializing_if = "Option::is_none")]
    assertion: Option<Transaction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assertion_height: Option<u32>,
    #[serde(with = "consensus", skip_serializing_if = "Option::is_none")]
    challenge: Option<Transaction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    challenge_height: Option<u32>,
    #[serde(with = "consensus", skip_serializing_if = "Option::is_none")]
    payout: Option<Transaction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payout_height: Option<u32>,
}

/// One transaction or output, if any, in consensus hex.
mod consensus {
    use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
    use bitcoin::consensus::{Decodable, Encodable};
    use serde::{de::Error, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Encodable, S: Serializer>(v: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
        v.as_ref().map(|t| serialize_hex(t)).serialize(s)
    }

    pub fn deserialize<'de, T: Decodable, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|h| deserialize_hex(&h).map_err(D::Error::custom))
            .transpose()
    }
}

/// Transactions or outputs in consensus hex.
mod consensus_all {
    use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
    use bitcoin::consensus::{Decodable, Encodable};
    use serde::{de::Error, Deserialize, Deserializer, Serialize, Serializer};

    #[allow(clippy::ptr_arg)]
    pub fn serialize<T: Encodable, S: Serializer>(v: &Vec<T>, s: S) -> Result<S::Ok, S::Error> {
        v.iter()
            .map(|t| serialize_hex(t))
            .collect::<Vec<_>>()
            .serialize(s)
    }

    pub fn deserialize<'de, T: Decodable, D: Deserializer<'de>>(d: D) -> Result<Vec<T>, D::Error> {
        Vec::<String>::deserialize(d)?
            .iter()
            .map(|h| deserialize_hex(h).map_err(D::Error::custom))
            .collect()
    }
}

impl State {
    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a recovery's state")
    }

    fn decode(row: &RecoveryRow) -> Result<Self> {
        ensure!(
            row.format == FORMAT,
            "a recovery stored in format {}, unknown",
            row.format
        );
        serde_json::from_slice(&row.state).context("a recovery's state")
    }
}

/// Force contract `id` of those in `dir` closed, her hub silent: her
/// notes at it claimed, the claim requested on Bitcoin at once, its fees
/// at `rate`; carried on as far as the chain lets it. Refused for a
/// contract not live: F not buried, or its protection over. One she
/// forced closed already is carried on.
pub async fn force_close<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &std::path::Path,
    id: &[u8; 32],
    rate: FeeRate,
) -> Result<Recovery> {
    let store = Store::existing(dir)?.context("no contracts")?;
    let contract = store.list()?.into_iter().find(|c| &c.id == id);
    let contract = contract.with_context(|| format!("no contract {}", crate::hex_id(id)))?;
    let giving_up = Stage::GivingUp.as_str();
    if store.recovery(id)?.is_some_and(|r| r.stage == giving_up) {
        finish_giving_up(wallet, &store, id)?;
    }
    if store.recovery(id)?.is_none() {
        let tip = chain.height().await.map_err(|e| anyhow!(e))?;
        ensure!(
            contract.fate == Some(Fate::Live),
            "its F is not buried in the chain yet"
        );
        ensure!(
            contract.live(tip),
            "its protection ended at block {}",
            contract.assert_height
        );
        let state = State::default().encode();
        store.start_recovery(id, FORMAT, Stage::Claiming.as_str(), &state)?;
        tracing::info!(id = %crate::hex_id(id), "forcing a contract closed");
    }
    Run::load(wallet, chain, &store, dir, id, rate)
        .await?
        .steps()
        .await?;
    view(&store.recovery(id)?.context("given up")?)
}

/// Her proof of contract `id`'s claim, among those in `dir`, brought from
/// wherever it was made: `export`, as `licash-r0 native-wrap` writes it
/// (`image_id`, `journal` and `proof`, each in hex), checked against the
/// program her contract names, the journal it fixed and the statement her
/// instances are garbled under; kept, then her `assert` of it broadcast at
/// once, its fees at `rate`. Only once the hub answered. Her one-time key
/// signs one proof only: once one is kept, the same again is no error,
/// another refused.
pub async fn import_proof<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &std::path::Path,
    id: &[u8; 32],
    export: &[u8],
    rate: FeeRate,
) -> Result<Recovery> {
    let store = Store::existing(dir)?.context("no contracts")?;
    let run = Run::load(wallet, chain, &store, dir, id, rate).await?;
    let proof = hex::encode(run.verified(export)?);
    let row = store.recovery(id)?.context("no recovery of it")?;
    let stage = Stage::parse(&row.stage)?;
    let mut state = State::decode(&row)?;
    match stage {
        Stage::Proving => {
            state.proof = Some(proof);
            let to = Stage::Asserting.as_str();
            let saved =
                store.save_recovery(id, (stage.as_str(), &row.state), to, true, &state.encode())?;
            ensure!(
                saved,
                "the recovery moved on meanwhile: bring the proof again"
            );
            tracing::info!(id = %crate::hex_id(id), stage = to, "recovering a contract");
        }
        Stage::Answered => bail!("her statement is not built yet: carry the recovery on first"),
        _ if state.proof.is_some() => ensure!(
            state.proof == Some(proof),
            "another proof is kept already, and her key signs one only"
        ),
        _ => bail!("no proof is needed at stage {}", stage.as_str()),
    }
    run.steps().await?;
    view(&store.recovery(id)?.context("given up")?)
}

/// Carry every recovery of those in `dir` on as far as the chain lets
/// it, its fees at `rate`: each as it then stands. One that cannot go on
/// holds up none of the others.
pub async fn advance<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &std::path::Path,
    rate: FeeRate,
) -> Result<Vec<Recovery>> {
    let Some(store) = Store::existing(dir)? else {
        return Ok(Vec::new());
    };
    for row in store.recoveries()? {
        let stage = Stage::parse(&row.stage);
        if stage.as_ref().is_ok_and(|s| s.over()) {
            continue;
        }
        if matches!(stage, Ok(Stage::GivingUp)) {
            if let Err(e) = finish_giving_up(wallet, &store, &row.id) {
                tracing::warn!(id = %crate::hex_id(&row.id), "giving a recovery up: {e:#}");
            }
            continue;
        }
        let run = match Run::load(wallet, chain, &store, dir, &row.id, rate).await {
            Ok(run) => run.steps().await,
            Err(e) => Err(e),
        };
        if let Err(e) = run {
            tracing::warn!(id = %crate::hex_id(&row.id), "recovering a contract: {e:#}");
            store.recovery_error(&row.id, Some(&format!("{e:#}")))?;
        }
    }
    list(&store)
}

/// The recoveries of the contracts in `dir`, the oldest first.
pub fn recoveries(dir: &std::path::Path) -> Result<Vec<Recovery>> {
    match Store::existing(dir)? {
        Some(store) => list(&store),
        None => Ok(Vec::new()),
    }
}

/// Her statement's input of contract `id` among those in `dir`, as kept
/// once the hub answered her request: what her proof is of, to prove
/// where she chooses (`licash-r0 native-prove-succinct`, then
/// `native-wrap`) and bring back by [`import_proof`]. Private: it shows
/// her notes and links them to her. Refused before it is built.
pub fn statement(dir: &std::path::Path, id: &[u8; 32]) -> Result<Vec<u8>> {
    let store = Store::existing(dir)?.context("no contracts")?;
    let row = store.recovery(id)?.context("no recovery of it")?;
    let stage = Stage::parse(&row.stage)?;
    ensure!(
        stage.disputed() && stage != Stage::Answered,
        "no statement to prove at stage {}",
        stage.as_str()
    );
    let path = store.statement_path(id);
    std::fs::read(&path).with_context(|| format!("{}", path.display()))
}

/// Give up recovering contract `id`, her notes given back to her: only
/// before her request is signed.
pub fn give_up<T: Transport>(
    wallet: &Wallet<T>,
    dir: &std::path::Path,
    id: &[u8; 32],
) -> Result<()> {
    let store = Store::existing(dir)?.context("no contracts")?;
    ensure!(store.recovery(id)?.is_some(), "no recovery of it");
    let giving_up = Stage::GivingUp.as_str();
    ensure!(
        store.revoke_recovery(id, giving_up, Some("given up"))?,
        "past giving up: her request is signed"
    );
    finish_giving_up(wallet, &store, id)
}

/// A recovery given up, finished: her notes back, then it forgotten. Its
/// stage says so first, so that one stopped between is finished later.
fn finish_giving_up<T: Transport>(wallet: &Wallet<T>, store: &Store, id: &[u8; 32]) -> Result<()> {
    wallet.unclaim_notes(id)?;
    store.drop_recovery(id)?;
    Ok(())
}

/// Those this build reads: one of a later build's is left out.
fn list(store: &Store) -> Result<Vec<Recovery>> {
    let rows = store.recoveries()?;
    let read = rows.iter().filter_map(|row| match view(row) {
        Ok(recovery) => Some(recovery),
        Err(e) => {
            tracing::warn!(id = %crate::hex_id(&row.id), "a recovery not read: {e:#}");
            None
        }
    });
    Ok(read.collect())
}

fn view(row: &RecoveryRow) -> Result<Recovery> {
    let state = State::decode(row)?;
    let txid = |tx: &Option<Transaction>| tx.as_ref().map(Transaction::compute_txid);
    let stage = Stage::parse(&row.stage)?;
    let answered = stage.disputed() || state.payout.is_some() || state.proof.is_some();
    let timeout = match stage {
        Stage::Lost => None,
        _ if answered => None,
        _ => txid(&state.spend),
    };
    let response = match answered {
        true => txid(&state.spend),
        false => None,
    };
    Ok(Recovery {
        id: row.id,
        stage,
        irrevocable: row.irrevocable,
        claimed: u64::from(state.amount),
        notes: state.notes.len(),
        request: txid(&state.r),
        publication: txid(&state.p),
        requested_at: state.r_height,
        timeout,
        timed_out_at: timeout.and(state.spent_height),
        sweep: txid(&state.sweep),
        swept_at: state.swept_height,
        next_at: state.next_at,
        error: row.error.clone(),
        response,
        deadline: state.deadline,
        proved: state.proof.is_some(),
        assertion: txid(&state.assertion),
        asserted_at: state.assertion_height,
        challenge: txid(&state.challenge),
        payout: txid(&state.payout),
        paid_at: state.payout_height,
    })
}

/// What a step did.
enum Next {
    /// Nothing: it waits for the chain.
    Wait,
    /// Its state changed, at the same stage: saved, then it waits.
    Save,
    /// On to a stage: saved, then its step.
    To(Stage),
}

/// One recovery carried on.
struct Run<'a, T: Transport, C: Chain> {
    wallet: &'a Wallet<T>,
    chain: &'a C,
    store: &'a Store,
    dir: &'a std::path::Path,
    id: [u8; 32],
    rate: FeeRate,
    o: Opening,
    keys: HolderKeys,
    trust: Trust,
    /// The chain's height as she opened it: nothing spends its output
    /// before.
    since: u32,
    /// What stopped its steps short: the network refusing a broadcast,
    /// say.
    refused: std::sync::Mutex<Vec<String>>,
    /// Whether this run claimed her notes, and built P again.
    took: AtomicBool,
    rebuilt: AtomicBool,
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
        let trust = crate::trust(wallet.network())?;
        let secrets = wallet.contract_secrets();
        let holder = Holder::load(&state, secrets.seed, secrets.spend, &Context { tip, trust })?;
        let o = holder.opening()?;
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
            trust,
            since,
            refused: Default::default(),
            took: AtomicBool::new(false),
            rebuilt: AtomicBool::new(false),
        })
    }

    /// Every step the chain lets it take now, each stored before the next.
    async fn steps(&self) -> Result<()> {
        loop {
            let row = self.store.recovery(&self.id)?.context("given up")?;
            let stage = Stage::parse(&row.stage)?;
            ensure!(stage != Stage::GivingUp, "given up");
            let mut state = State::decode(&row)?;
            let tip = self.chain.height().await.map_err(|e| anyhow!(e))?;
            let next = match stage {
                Stage::Claiming => self.claim(&mut state)?,
                Stage::Claimed => self.commit(&mut state, tip).await?,
                Stage::Irrevocable => self.request(&mut state)?,
                Stage::Signing => self.sign(&mut state)?,
                Stage::Published => self.publish(&mut state, tip).await?,
                Stage::Requested | Stage::TimingOut => self.wait(stage, &mut state, tip).await?,
                Stage::TimedOut | Stage::Paid => self.sweep(&mut state, tip)?,
                Stage::Sweeping => self.swept(&mut state, tip).await?,
                Stage::Answered => self.answered(&mut state, tip).await?,
                Stage::Proving => self.proving(&mut state, tip).await?,
                Stage::Asserting => self.asserting(&mut state, tip).await?,
                Stage::Asserted | Stage::Succeeding => {
                    self.asserted(stage, &mut state, tip).await?
                }
                Stage::Answering => self.answering(&mut state, tip).await?,
                Stage::Done | Stage::Lost | Stage::GivingUp => Next::Wait,
            };
            let to = match next {
                Next::Wait => return self.report(),
                Next::Save => stage,
                Next::To(to) => to,
            };
            let irrevocable = !matches!(to, Stage::Claiming | Stage::Claimed);
            let saved = self.store.save_recovery(
                &self.id,
                (stage.as_str(), &row.state),
                to.as_str(),
                irrevocable,
                &state.encode(),
            )?;
            if !saved {
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

    /// Its save refused: moved on by another step, or given up. Notes this
    /// run claimed, maybe after the give-up gave back hers, given back.
    fn outrun(&self) -> Result<()> {
        if !self.took.load(Ordering::SeqCst) {
            return Ok(());
        }
        let giving_up = Stage::GivingUp.as_str();
        let now = self.store.recovery(&self.id)?;
        if now.is_none_or(|r| r.stage == giving_up) {
            self.wallet.unclaim_notes(&self.id)?;
        }
        Ok(())
    }

    /// `why` it stopped short, stored by [`Run::report`].
    fn trouble(&self, why: String) {
        tracing::warn!(id = %crate::hex_id(&self.id), "recovering a contract: {why}");
        self.refused.lock().expect("refusals").push(why);
    }

    /// What stopped it short, stored.
    fn report(&self) -> Result<()> {
        let refused = self.refused.lock().expect("refusals").join("; ");
        if !refused.is_empty() {
            self.store.recovery_error(&self.id, Some(&refused))?;
        }
        Ok(())
    }

    /// Hand `tx` to the network: what it refused is kept for [`Run::report`].
    async fn broadcast(&self, tx: &Transaction) {
        if let Err(e) = self.chain.broadcast(tx).await {
            self.trouble(format!("{}: {e}", tx.compute_txid()));
        }
    }

    /// Give it up, `why`, her notes back: while revocable only.
    fn give_back(&self, why: &str) -> Result<()> {
        let giving_up = Stage::GivingUp.as_str();
        if self.store.revoke_recovery(&self.id, giving_up, Some(why))? {
            finish_giving_up(self.wallet, self.store, &self.id)?;
        }
        Ok(())
    }

    /// Her notes of the hub's keyset taken, the largest first within the
    /// cap, and claimed: the claim signed by each note's hold key.
    fn claim(&self, state: &mut State) -> Result<Next> {
        let parts = self.o.base.parts();
        let keyset = *parts.keyset.id();
        let mut notes = self.wallet.claimed_notes(&self.id, &keyset)?;
        if notes.is_empty() {
            let max = MAX_NOTES.min(parts.limits.max_authorizations as usize);
            notes = self
                .wallet
                .claim_notes(&self.id, &keyset, u64::from(parts.cap), max)?;
            self.took.store(!notes.is_empty(), Ordering::SeqCst);
        }
        if notes.is_empty() {
            let why = "no notes at its hub to claim";
            self.give_back(why)?;
            bail!(why);
        }
        let amount: u64 = notes.iter().map(|n| n.note.message().denomination()).sum();
        state.amount = u32::try_from(amount).context("a claim above any cap")?;
        let publication = match self.publication(&notes, state.amount) {
            Ok(publication) => publication,
            Err(e) => {
                let e = e.context("claiming her notes");
                self.give_back(&format!("{e:#}"))?;
                return Err(e);
            }
        };
        state.notes = notes.iter().map(|n| hex::encode(n.to_bytes())).collect();
        state.publication = Some(hex::encode(publication.to_bytes()));
        Ok(Next::To(Stage::Claimed))
    }

    fn publication(&self, notes: &[ClaimNote], amount: u32) -> Result<Publication> {
        let parts = self.o.base.parts();
        let limit = parts.limits.max_authorizations as usize;
        let messages: Vec<_> = notes.iter().map(|n| n.note.message().clone()).collect();
        let mut authorizations = messages.clone();
        authorizations.sort_by_key(|m| m.identifier().ok());
        let claim = UnsignedClaim::new(
            claim::Context {
                final_digest: *self.o.d.as_bytes(),
                deposit: parts.deposit,
                cap: parts.cap,
            },
            &parts.keyset,
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
        let bytes = publication.script()?.len();
        ensure!(
            bytes <= parts.limits.max_publication_bytes as usize,
            "a claim of {bytes} bytes, more than the contract publishes"
        );
        Ok(publication)
    }

    fn publication_of(&self, state: &State) -> Result<Publication> {
        let bytes = hex::decode(state.publication.as_deref().context("no claim")?)?;
        let parts = self.o.base.parts();
        let limit = parts.limits.max_authorizations as usize;
        Ok(Publication::from_bytes(&parts.keyset, limit, &bytes)?)
    }

    fn requested(&self, state: &State) -> Result<Request> {
        Ok(Request {
            amount: state.amount,
            claim_digest: *self.publication_of(state)?.claim().digest(),
        })
    }

    /// Past giving it up once R may be in the next block and her coins pay
    /// its fees, unless another copy of her wallet requested already (R,
    /// if of the same claim, taken as it is) or the contract's output is
    /// spent otherwise. Until then, revocable: another copy of hers sees
    /// only R in a block.
    async fn commit(&self, state: &mut State, tip: u32) -> Result<Next> {
        let (coin, out, wait) = close::request_input(&self.o)?;
        let script = &out.script_pubkey;
        let f_height = match state.f_height {
            Some(height) => height,
            None => {
                let found = confirmed_at(self.chain, coin.txid, script, self.since..=tip);
                found.await?.context("F in no block")?
            }
        };
        state.f_height = Some(f_height);
        state.next_at = Some(f_height + wait);
        let from = state.from.unwrap_or(f_height);
        if let Some((height, tx)) = first_spend(self.chain, coin, script, from..=tip).await? {
            let txid = tx.compute_txid();
            if txid != close::request_txid(&self.o)? {
                let why = format!("its collateral is spent already, by {txid}: closed otherwise");
                self.give_back(&why)?;
                bail!(why);
            }
            if close::read_request(&self.o, &tx)?.0 != self.requested(state)? {
                // That claim may be of her notes: they stay claimed.
                self.trouble("another copy of this wallet requested it, of other notes".into());
                return Ok(Next::To(Stage::Lost));
            }
            state.r = Some(tx);
            state.r_height = Some(height);
            return Ok(Next::To(Stage::Irrevocable));
        }
        state.from = Some(tip + 1);
        if tip + 1 < f_height + wait {
            return Ok(Next::Save);
        }
        self.fees_covered(state)
            .context("her coins on Bitcoin pay no request")?;
        Ok(Next::To(Stage::Irrevocable))
    }

    /// Whether her coins pay her request's fees: P built for R signed by
    /// keys of no one's, which spends no one-time key of hers.
    fn fees_covered(&self, state: &State) -> Result<()> {
        let anyone = Signer::derive(&[0; 32], &self.o.base.digest());
        let r = close::request(&self.o, &anyone.sign(self.requested(state)?))?;
        self.build_p(&r, &self.publication_of(state)?)?;
        Ok(())
    }
}

/// Her request and P, signed and on Bitcoin.
impl<T: Transport, C: Chain> Run<'_, T, C> {
    /// R signed, unless taken as another copy of hers posted it; P built
    /// to publish the claim, of R's anchor and coins of her wallet's.
    fn request(&self, state: &mut State) -> Result<Next> {
        let request = self.requested(state)?;
        let r = match state.r.clone() {
            Some(r) => r,
            None => {
                let secrets = self.wallet.contract_secrets();
                let signer = Signer::derive(&secrets.seed, &self.o.base.digest());
                ensure!(
                    signer.public() == *self.o.prefix.leaf().keys(),
                    "her request keys are not those her contract froze"
                );
                close::request(&self.o, &signer.sign(request))?
            }
        };
        ensure!(
            close::read_request(&self.o, &r)?.0 == request,
            "her request reads another"
        );
        let (p, prevouts) = self.build_p(&r, &self.publication_of(state)?)?;
        state.r = Some(r);
        state.p = Some(p);
        state.prevouts = prevouts;
        Ok(Next::To(Stage::Signing))
    }

    /// P: R's anchor and as many of her coins as pay its fees, R's as its
    /// child; the claim, the timeout's fee coin, her change.
    fn build_p(
        &self,
        r: &Transaction,
        publication: &Publication,
    ) -> Result<(Transaction, Vec<TxOut>)> {
        let anchor = close::anchor(r, &self.keys)?;
        let published = TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(publication.script()?),
        };
        let fee_coin = TxOut {
            value: self.timeout_fee()?,
            script_pubkey: close::fee_script(&self.keys)?,
        };
        let change = self.wallet.fallback_address()?.script_pubkey();
        let (_, contract, _) = close::request_input(&self.o)?;
        let r_out: Amount = r.output.iter().map(|o| o.value).sum();
        let r_fee = contract.value.checked_sub(r_out).context("R's fee")?;
        let r_vsize = r.vsize() as u64;
        let mut inputs = vec![anchor];
        for coin in super::coins(self.wallet, self.dir)? {
            let out = self.wallet.silent_coin_output(&coin.outpoint)?;
            inputs.push((coin.outpoint, out.context("a coin of hers unknown")?));
            let total: Amount = inputs.iter().map(|(_, o)| o.value).sum();
            let mut tx = Transaction {
                version: bitcoin::transaction::Version::TWO,
                lock_time: bitcoin::absolute::LockTime::ZERO,
                input: inputs.iter().map(|(c, _)| close::fee_input(*c)).collect(),
                output: vec![
                    published.clone(),
                    fee_coin.clone(),
                    TxOut {
                        value: Amount::ZERO,
                        script_pubkey: change.clone(),
                    },
                ],
            };
            let prevouts: Vec<TxOut> = inputs.iter().map(|(_, o)| o.clone()).collect();
            let vsize = self.vsize(&tx, &prevouts)?;
            let own = self.rate.fee_vb(vsize).context("P's fee")?;
            let package = self.rate.fee_vb(vsize + r_vsize).context("P's fee")?;
            let fee = own.max(package.checked_sub(r_fee).unwrap_or(Amount::ZERO));
            let Some(left) = total.checked_sub(fee_coin.value + fee) else {
                continue;
            };
            if left >= DUST {
                tx.output[2].value = left;
            } else {
                tx.output.pop();
            }
            return Ok((tx, prevouts));
        }
        bail!("too little of hers on Bitcoin to pay her request's fees")
    }

    /// `tx`'s size once signed: R's anchor at input 0, her coins after.
    fn vsize(&self, tx: &Transaction, prevouts: &[TxOut]) -> Result<u64> {
        let mut signed = tx.clone();
        close::sign_fee_input(&self.keys, &mut signed, 0, prevouts)?;
        for input in signed.input.iter_mut().skip(1) {
            input.witness = Witness::from_slice(&[[0u8; 64]]);
        }
        Ok(signed.vsize() as u64)
    }

    /// What the timeout's fee coin holds: its fee at her rate, with a
    /// margin.
    fn timeout_fee(&self) -> Result<Amount> {
        let coin = TxOut {
            value: DUST,
            script_pubkey: close::fee_script(&self.keys)?,
        };
        let any = OutPoint {
            txid: Txid::from_raw_hash(bitcoin::hashes::Hash::all_zeros()),
            vout: 1,
        };
        let vsize = close::timeout(&self.o, &self.keys, (any, coin))?.vsize() as u64;
        let fee = self.rate.fee_vb(vsize * TIMEOUT_FEE_MARGIN);
        Ok(fee.context("the timeout's fee")?.max(DUST))
    }

    /// P signed: her coins by her wallet, reserved for P from then on.
    /// Coins of hers spent meanwhile: P built again.
    fn sign(&self, state: &mut State) -> Result<Next> {
        let mut p = state.p.clone().context("no P")?;
        let prevouts = &state.prevouts;
        close::sign_fee_input(&self.keys, &mut p, 0, prevouts)?;
        for i in 1..p.input.len() {
            match self.wallet.sign_input(&p, prevouts, i) {
                Ok(signed) => p.input[i].witness = signed.input[i].witness.clone(),
                Err(e @ (licash_wallet::Error::Invalid(_) | licash_wallet::Error::NoteTaken)) => {
                    let again = self.rebuilt.swap(true, Ordering::SeqCst);
                    ensure!(!again, "a coin of P's again: {e}");
                    tracing::warn!(id = %crate::hex_id(&self.id), "P built again: {e}");
                    // What it kept for P freed: none if its first coin failed.
                    let _ = self.wallet.abandon_silent(&p.compute_txid());
                    state.p = None;
                    state.prevouts.clear();
                    return Ok(Next::To(Stage::Irrevocable));
                }
                Err(e) => return Err(e.into()),
            }
        }
        state.p = Some(p);
        Ok(Next::To(Stage::Published))
    }
}

/// What the chain shows of it, and her payout.
impl<T: Transport, C: Chain> Run<'_, T, C> {
    /// R and P broadcast until both are in a block: then the hub's window
    /// runs.
    async fn publish(&self, state: &mut State, tip: u32) -> Result<Next> {
        let r = state.r.clone().context("no R")?;
        let p = state.p.clone().context("no P")?;
        let mut next = Next::Wait;
        if state.r_height.is_none() {
            let (coin, out, _) = close::request_input(&self.o)?;
            let from = state.from.or(state.f_height).unwrap_or(self.since);
            match first_spend(self.chain, coin, &out.script_pubkey, from..=tip).await? {
                Some((height, tx)) if tx.compute_txid() == r.compute_txid() => {
                    if close::read_request(&self.o, &tx)?.0 != self.requested(state)? {
                        let _ = self.wallet.abandon_silent(&p.compute_txid());
                        self.trouble(
                            "another copy of this wallet requested it, of other notes".into(),
                        );
                        return Ok(Next::To(Stage::Lost));
                    }
                    // Its witness as in the block: hers to sweep with.
                    state.r = Some(tx);
                    state.r_height = Some(height);
                }
                Some((_, tx)) => {
                    let _ = self.wallet.abandon_silent(&p.compute_txid());
                    self.trouble(format!(
                        "its collateral went to {} instead",
                        tx.compute_txid()
                    ));
                    return Ok(Next::To(Stage::Lost));
                }
                None => {
                    state.from = Some(tip + 1);
                    next = Next::Save;
                }
            }
        }
        let Some(r_height) = state.r_height else {
            // Not before R's lock after F lets it in the next block.
            if state.next_at.is_none_or(|at| tip + 1 >= at) {
                self.keep_p(&p, &state.prevouts);
                self.broadcast(&r).await;
                self.broadcast(&p).await;
            }
            return Ok(next);
        };
        let (anchor, out) = close::anchor(&r, &self.keys)?;
        match first_spend(self.chain, anchor, &out.script_pubkey, r_height..=tip).await? {
            Some((height, tx)) if tx.compute_txid() == p.compute_txid() => {
                state.p_height = Some(height);
                state.from = Some(r_height);
                state.next_at = Some(r_height + close::timeout_delay(&self.o)?);
                Ok(Next::To(Stage::Requested))
            }
            Some((_, tx)) => bail!("R's anchor spent by {}, not P", tx.compute_txid()),
            None => {
                self.keep_p(&p, &state.prevouts);
                self.broadcast(&p).await;
                Ok(Next::Save)
            }
        }
    }

    /// P's coins kept for it again: a wallet rewinding past them forgets
    /// what reserved them. Refused once a block spends them, no matter.
    fn keep_p(&self, p: &Transaction, prevouts: &[TxOut]) {
        for i in 1..p.input.len() {
            if let Err(e) = self.wallet.sign_input(p, prevouts, i) {
                tracing::debug!(id = %crate::hex_id(&self.id), "a coin of P's not kept again: {e}");
            }
        }
    }

    /// The spend of R's output awaited: the hub's answer, or once its
    /// window passed, `request_timeout`, paid of P's fee coin.
    async fn wait(&self, stage: Stage, state: &mut State, tip: u32) -> Result<Next> {
        let r = state.r.clone().context("no R")?;
        let r_height = state.r_height.context("R in no block")?;
        let output = OutPoint {
            txid: r.compute_txid(),
            vout: 0,
        };
        let script = &r.output[0].script_pubkey;
        let from = state.from.unwrap_or(r_height);
        if let Some((height, tx)) = first_spend(self.chain, output, script, from..=tip).await? {
            let timeout = close::is_timeout(&self.o, &tx)?;
            state.spent_height = Some(height);
            state.spend = Some(tx);
            if !timeout {
                state.next_at = None;
                return Ok(Next::To(Stage::Answered));
            }
            state.from = Some(height);
            state.next_at = Some(height + close::sweep_delay(&self.o));
            return Ok(Next::To(Stage::TimedOut));
        }
        state.from = Some(tip + 1);
        if stage == Stage::TimingOut {
            self.broadcast(state.spend.as_ref().context("no timeout")?)
                .await;
            return Ok(Next::Save);
        }
        let at = r_height + close::timeout_delay(&self.o)?;
        if tip + 1 < at {
            return Ok(Next::Save);
        }
        let p = state.p.as_ref().context("no P")?;
        let fee = OutPoint {
            txid: p.compute_txid(),
            vout: 1,
        };
        let timeout = close::timeout(&self.o, &self.keys, (fee, p.output[1].clone()))?;
        self.broadcast(&timeout).await;
        state.spend = Some(timeout);
        Ok(Next::To(Stage::TimingOut))
    }

    /// Her bits and reserve swept to her wallet once their delay passed.
    fn sweep(&self, state: &mut State, tip: u32) -> Result<Next> {
        let (payout, height) = payout(state)?;
        let height = height.context("no payout in a block")?;
        if tip + 1 < height + close::sweep_delay(&self.o) {
            return Ok(Next::Wait);
        }
        let r = state.r.as_ref().context("no R")?;
        let (_, b) = close::read_request(&self.o, r)?;
        let to = self.wallet.fallback_address()?.script_pubkey();
        let sweep = close::sweep(&self.o, &self.keys, payout, state.amount, &b, to, self.rate)?;
        state.sweep = Some(sweep);
        state.next_at = None;
        Ok(Next::To(Stage::Sweeping))
    }

    /// Her sweep broadcast until in a block.
    async fn swept(&self, state: &mut State, tip: u32) -> Result<Next> {
        let sweep = state.sweep.clone().context("no sweep")?;
        let (payout, _) = payout(state)?;
        let reserve = sweep
            .input
            .last()
            .context("a sweep of nothing")?
            .previous_output;
        let script = &payout.output[reserve.vout as usize].script_pubkey;
        let from = state.from.unwrap_or(self.since);
        match first_spend(self.chain, reserve, script, from..=tip).await? {
            Some((height, tx)) if tx.compute_txid() == sweep.compute_txid() => {
                state.swept_height = Some(height);
                Ok(Next::To(Stage::Done))
            }
            Some((_, tx)) => {
                self.trouble(format!("her reserve went to {} instead", tx.compute_txid()));
                Ok(Next::To(Stage::Lost))
            }
            None => {
                state.from = Some(tip + 1);
                self.broadcast(&sweep).await;
                Ok(Next::Save)
            }
        }
    }
}

/// The hub's answer, her proof of her claim, and what it pays her.
impl<T: Transport, C: Chain> Run<'_, T, C> {
    /// The hub's answer read: a plain `respond` lets her proof decide.
    /// Her statement's input, from her notes claimed and the chain's
    /// evidence of R and P once both are deep enough, run as her proof
    /// will be and kept beside her instances: then her proof is needed. A
    /// counter, or an answer not read, stays here, with why.
    async fn answered(&self, state: &mut State, tip: u32) -> Result<Next> {
        // Its output taken meanwhile (her assert by another copy of her
        // wallet, or the hub's timeout): nothing to build.
        state.from = state.from.max(state.spent_height);
        if let Some(next) = self.answer_spent(state, tip).await? {
            return Ok(next);
        }
        let spend = state.spend.clone().context("no answer")?;
        let response = match dispute::response(&self.o, &spend)? {
            Response::Plain(response) => response,
            Response::Counter => {
                bail!("the hub countered her request, which this build does not answer yet")
            }
        };
        let r_height = state.r_height.context("R in no block")?;
        let p_height = state.p_height.context("P in no block")?;
        let terms = &self.o.base.parts().chain;
        // Her statement reads R and P at their depths: the headers to there.
        let deep = (r_height + terms.request_depth).max(p_height + terms.publication_depth) - 1;
        if tip < deep {
            state.next_at = Some(deep);
            return Ok(Next::Save);
        }
        let input = self.statement(state, response, deep).await?;
        let (journal, paid) = input
            .run(&mut |_, _| false)
            .map_err(|e| anyhow!("her statement: {e}"))?;
        ensure!(
            journal.to_bytes() == dispute::expected(&self.o).to_bytes(),
            "her statement commits another journal than her contract fixed"
        );
        ensure!(
            paid == state.amount,
            "her statement pays {paid}, not her claim"
        );
        let bytes = input
            .to_bytes()
            .map_err(|e| anyhow!("her statement: {e}"))?;
        self.store.keep_statement(&self.id, &bytes)?;
        state.deadline = Some(dispute::deadline(&self.o));
        state.next_at = None;
        Ok(Next::To(Stage::Proving))
    }

    /// Her statement's input proving her whole claim against `response`:
    /// the headers from her contract's checkpoint to block `to`, R and P
    /// in theirs, and her notes with their ownership.
    async fn statement(
        &self,
        state: &State,
        response: HubResponse,
        to: u32,
    ) -> Result<StatementInput> {
        let parts = self.o.base.parts();
        let mut headers = Vec::new();
        for height in parts.chain.checkpoint.height() + 1..=to {
            let header = self.chain.header(height).await.map_err(|e| anyhow!(e))?;
            let bytes = bitcoin::consensus::serialize(&header);
            headers.push(bytes.try_into().expect("a header's 80 bytes"));
        }
        let r = state.r.as_ref().context("no R")?;
        let p = state.p.as_ref().context("no P")?;
        let request_height = state.r_height.context("R in no block")?;
        let publication_height = state.p_height.context("P in no block")?;
        let (block, i) = self.block_of(request_height, r).await?;
        let request = WitnessInclusion::capture(&block, i, MAX_BLOCK)?;
        let (block, i) = self.block_of(publication_height, p).await?;
        let publication = Inclusion::capture(&block, i, MAX_BLOCK)?;
        let keyset = &parts.keyset;
        let notes = state
            .notes
            .iter()
            .map(|n| Ok(ClaimNote::from_bytes(keyset, &hex::decode(n)?)?))
            .collect::<Result<Vec<_>>>()?;
        let ownerships = self.wallet.ownerships(&notes)?;
        let items = notes
            .into_iter()
            .zip(ownerships)
            .map(|(n, ownership)| ItemWitness::Settled {
                note: Box::new(n.note),
                ownership,
            })
            .collect();
        let holder = HolderWitness {
            items,
            sources: vec![],
            chain: None,
            activations: vec![],
        };
        let observed = Observed {
            headers,
            request_height,
            request,
            publication_height,
            publication,
        };
        Ok(dispute::statement(&self.o, observed, response, holder))
    }

    /// Block `height` in consensus bytes, checked against its hash, and
    /// where `tx` is in it.
    async fn block_of(&self, height: u32, tx: &Transaction) -> Result<(Vec<u8>, usize)> {
        let hash = self.chain.hash(height).await.map_err(|e| anyhow!(e))?;
        let block = self.chain.block(&hash).await.map_err(|e| anyhow!(e))?;
        ensure!(
            block.block_hash() == hash && block.check_merkle_root(),
            "block {hash} does not match its header"
        );
        let txid = tx.compute_txid();
        let i = block.txdata.iter().position(|t| t.compute_txid() == txid);
        let i = i.with_context(|| format!("{txid} not in block {height}"))?;
        Ok((bitcoin::consensus::serialize(&block), i))
    }

    /// Her proof awaited, made or brought at her word ([`import_proof`]);
    /// under the mock authority, for regtest, made at once. Meanwhile the
    /// hub's answer's output watched ([`Run::answer_spent`]).
    async fn proving(&self, state: &mut State, tip: u32) -> Result<Next> {
        if let Some(next) = self.answer_spent(state, tip).await? {
            return Ok(next);
        }
        if !self.trust.mock {
            return Ok(Next::Save);
        }
        let proof = dispute::mock_proof(&self.o, &self.trust)?;
        state.proof = Some(hex::encode(dispute::proof_bytes(&proof)));
        Ok(Next::To(Stage::Asserting))
    }

    /// Her `assert` of her proof kept, broadcast until in a block, past
    /// her deadline too, until the hub's timeout is.
    async fn asserting(&self, state: &mut State, tip: u32) -> Result<Next> {
        if let Some(next) = self.answer_spent(state, tip).await? {
            return Ok(next);
        }
        let assertion = match state.assertion.clone() {
            Some(tx) => tx,
            None => dispute::assert(&self.o, &self.keys, &self.proof(state)?)?,
        };
        self.broadcast(&assertion).await;
        state.assertion = Some(assertion);
        Ok(Next::Save)
    }

    /// What spent the hub's answer's output, if anything did: her
    /// `assert`, as hers or another copy's of her wallet, its proof taken
    /// as hers; anything else, the hub's timeout once her deadline passed,
    /// loses it.
    async fn answer_spent(&self, state: &mut State, tip: u32) -> Result<Option<Next>> {
        let answer = state.spend.as_ref().context("no answer")?;
        let output = OutPoint {
            txid: answer.compute_txid(),
            vout: 0,
        };
        let script = answer.output[0].script_pubkey.clone();
        let from = state.from.or(state.spent_height).unwrap_or(self.since);
        match first_spend(self.chain, output, &script, from..=tip).await? {
            Some((height, tx)) if dispute::is_assert(&self.o, &tx) => {
                let proof = hex::encode(dispute::proof_bytes(&dispute::asserted(&self.o, &tx)?));
                if state.proof.as_ref().is_some_and(|p| *p != proof) {
                    self.trouble("another copy of this wallet asserted another proof".into());
                }
                state.proof = Some(proof);
                state.assertion = Some(tx);
                state.assertion_height = Some(height);
                state.from = Some(height);
                state.next_at = Some(height + dispute::success_delay(&self.o));
                Ok(Some(Next::To(Stage::Asserted)))
            }
            Some((_, tx)) => {
                let txid = tx.compute_txid();
                self.trouble(format!(
                    "its collateral went to {txid}, no assert of hers in time"
                ));
                Ok(Some(Next::To(Stage::Lost)))
            }
            None => {
                state.from = Some(tip + 1);
                Ok(None)
            }
        }
    }

    /// Her `assert` in a block: the hub's challenge awaited until its
    /// chance passed, then `success`, of P's fee coin, broadcast until in
    /// a block. A challenge, even once `success` is out, is answered.
    async fn asserted(&self, stage: Stage, state: &mut State, tip: u32) -> Result<Next> {
        let assertion = state.assertion.clone().context("no assert")?;
        let height = state.assertion_height.context("her assert in no block")?;
        let output = OutPoint {
            txid: assertion.compute_txid(),
            vout: 0,
        };
        let script = &assertion.output[0].script_pubkey;
        let from = state.from.unwrap_or(height);
        match first_spend(self.chain, output, script, from..=tip).await? {
            Some((at, tx)) if dispute::is_challenge(&self.o, &tx) => {
                state.challenge = Some(tx);
                state.challenge_height = Some(at);
                // A `success` she built never will be.
                state.payout = None;
                state.from = Some(at);
                state.next_at = None;
                return Ok(Next::To(Stage::Answering));
            }
            Some((at, tx)) if dispute::is_success(&self.o, &tx)? => {
                return Ok(self.paid(state, tx, at));
            }
            Some((_, tx)) => {
                let txid = tx.compute_txid();
                self.trouble(format!("her assert's output went to {txid}"));
                return Ok(Next::To(Stage::Lost));
            }
            None => state.from = Some(tip + 1),
        }
        if stage == Stage::Succeeding {
            self.broadcast(state.payout.as_ref().context("no success")?)
                .await;
            return Ok(Next::Save);
        }
        let at = height + dispute::success_delay(&self.o);
        state.next_at = Some(at);
        if tip + 1 < at {
            return Ok(Next::Save);
        }
        let success = dispute::success(&self.o, &self.keys, fee_coin(state)?)?;
        self.broadcast(&success).await;
        state.payout = Some(success);
        state.next_at = None;
        Ok(Next::To(Stage::Succeeding))
    }

    /// The hub's challenge answered from a kept instance her proof opens,
    /// of P's fee coin, broadcast until in a block: the hub's `failure`
    /// pays it once her chance passed.
    async fn answering(&self, state: &mut State, tip: u32) -> Result<Next> {
        let challenge = state.challenge.clone().context("no challenge")?;
        let height = state.challenge_height.context("a challenge in no block")?;
        let output = OutPoint {
            txid: challenge.compute_txid(),
            vout: 0,
        };
        let script = &challenge.output[0].script_pubkey;
        let from = state.from.unwrap_or(height);
        match first_spend(self.chain, output, script, from..=tip).await? {
            Some((at, tx)) if dispute::is_answer(&self.o, &tx) => {
                return Ok(self.paid(state, tx, at));
            }
            Some((_, tx)) => {
                let txid = tx.compute_txid();
                self.trouble(format!(
                    "its collateral went to {txid}, no answer of hers in time"
                ));
                return Ok(Next::To(Stage::Lost));
            }
            None => state.from = Some(tip + 1),
        }
        if state.payout.is_none() {
            let setups = self
                .o
                .setups(|s, i, _| self.store.kept_instance(&self.id, s, i))?;
            let [full, ..] = &setups;
            let proof = self.proof(state)?;
            let answer = dispute::answer(
                &self.o,
                &self.keys,
                full,
                &challenge,
                &proof,
                fee_coin(state)?,
            )?;
            let answer = answer.context("no instance she kept opens to her proof")?;
            state.payout = Some(answer);
        }
        self.broadcast(state.payout.as_ref().expect("her answer"))
            .await;
        Ok(Next::Save)
    }

    /// `tx`, her payout after the hub's answer, in block `at`.
    fn paid(&self, state: &mut State, tx: Transaction, at: u32) -> Next {
        state.payout = Some(tx);
        state.payout_height = Some(at);
        state.from = Some(at);
        state.next_at = Some(at + close::sweep_delay(&self.o));
        Next::To(Stage::Paid)
    }

    /// Her proof as kept.
    fn proof(&self, state: &State) -> Result<dispute::ClaimProof> {
        dispute::proof_of_bytes(&hex::decode(state.proof.as_deref().context("no proof")?)?)
    }

    /// `export`, a proof as `licash-r0 native-wrap` writes it, checked:
    /// of the program her contract names, of the journal it fixed, and
    /// proving her claim under the statement her instances are garbled
    /// under. Its bytes as kept.
    fn verified(&self, export: &[u8]) -> Result<Vec<u8>> {
        let export: serde_json::Value =
            serde_json::from_slice(export).context("not a proof's export")?;
        let field = |name: &str| -> Result<Vec<u8>> {
            let text = export[name]
                .as_str()
                .with_context(|| format!("no {name}"))?;
            hex::decode(text).with_context(|| format!("{name} not in hex"))
        };
        ensure!(
            field("image_id")? == dispute::holder_image(&self.o),
            "a proof of another program than her contract names"
        );
        ensure!(
            field("journal")? == dispute::expected(&self.o).to_bytes(),
            "a proof of another journal than her contract fixed"
        );
        let proof = dispute::proof_from_points(&field("proof")?)?;
        ensure!(
            dispute::proves(&self.o, &self.trust, &proof),
            "not a proof of her claim"
        );
        Ok(dispute::proof_bytes(&proof))
    }
}

/// P's fee coin, which the hub's answer left hers: what pays the fee of
/// her payout after it.
fn fee_coin(state: &State) -> Result<(OutPoint, TxOut)> {
    let p = state.p.as_ref().context("no P")?;
    let coin = OutPoint {
        txid: p.compute_txid(),
        vout: 1,
    };
    Ok((coin, p.output.get(1).context("P has no fee coin")?.clone()))
}

/// Her payout and the block it is in, once it is: `request_timeout`, her
/// hub silent; else `success` or her answer to its challenge.
fn payout(state: &State) -> Result<(&Transaction, Option<u32>)> {
    match &state.payout {
        Some(tx) => Ok((tx, state.payout_height)),
        None => Ok((
            state.spend.as_ref().context("no timeout")?,
            state.spent_height,
        )),
    }
}

/// The block of `heights` transaction `txid`, paying `script`, is in, if
/// any: blocks whose filter matches `script` are fetched, and checked
/// against their hashes.
async fn confirmed_at(
    chain: &impl Chain,
    txid: Txid,
    script: &bitcoin::Script,
    heights: std::ops::RangeInclusive<u32>,
) -> Result<Option<u32>> {
    for height in heights {
        let hash = chain.hash(height).await.map_err(|e| anyhow!(e))?;
        if let Some(filter) = chain.filter(&hash).await.map_err(|e| anyhow!(e))? {
            let filter = bitcoin::bip158::BlockFilter::new(&filter);
            if !filter.match_any(&hash, std::iter::once(script.as_bytes()))? {
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
mod tests {
    use super::*;

    fn tx() -> Transaction {
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        }
    }

    /// A state of a hub silent writes as the build before the hub's
    /// answer was followed wrote it, which that build reads: no field of
    /// the answer's in it.
    #[test]
    fn a_silent_recovery_s_state_is_written_as_before() {
        let state = State {
            notes: vec!["ab".into()],
            amount: 3_000,
            publication: Some("cd".into()),
            r: Some(tx()),
            prevouts: vec![tx().output[0].clone()],
            f_height: Some(101),
            r_height: Some(250),
            spent_height: Some(262),
            spend: Some(tx()),
            from: Some(262),
            next_at: Some(406),
            ..State::default()
        };
        let out = bitcoin::consensus::encode::serialize_hex(&tx().output[0]);
        let tx = bitcoin::consensus::encode::serialize_hex(&tx());
        let before = format!(
            r#"{{"notes":["ab"],"amount":3000,"publication":"cd","r":"{tx}","p":null,"prevouts":["{out}"],"f_height":101,"r_height":250,"p_height":null,"spent_height":262,"swept_height":null,"spend":"{tx}","sweep":null,"from":262,"next_at":406}}"#
        );
        assert_eq!(String::from_utf8(state.encode()).unwrap(), before);
        let row = RecoveryRow {
            id: [1; 32],
            format: FORMAT,
            stage: Stage::Requested.as_str().into(),
            irrevocable: true,
            error: None,
            state: before.into_bytes(),
            created_at: 0,
        };
        assert_eq!(State::decode(&row).unwrap().encode(), row.state);
    }

    /// The answer's fields kept, read back; a field of no build's refused.
    #[test]
    fn a_disputed_recovery_s_state_reads_back() {
        let state = State {
            spend: Some(tx()),
            deadline: Some(500),
            proof: Some("ef".into()),
            assertion: Some(tx()),
            assertion_height: Some(300),
            payout: Some(tx()),
            ..State::default()
        };
        let mut row = RecoveryRow {
            id: [1; 32],
            format: FORMAT,
            stage: Stage::Succeeding.as_str().into(),
            irrevocable: true,
            error: None,
            state: state.encode(),
            created_at: 0,
        };
        let read = State::decode(&row).unwrap();
        assert_eq!(read.deadline, Some(500));
        assert_eq!(read.proof.as_deref(), Some("ef"));
        assert_eq!(read.assertion, Some(tx()));
        assert_eq!(read.encode(), row.state);
        let shown = view(&row).unwrap();
        assert_eq!(shown.timeout, None);
        assert_eq!(shown.response, Some(tx().compute_txid()));
        assert!(shown.proved && shown.payout.is_some() && shown.paid_at.is_none());
        row.state = br#"{"later":1}"#.to_vec();
        assert!(State::decode(&row).is_err());
    }

    #[test]
    fn every_stage_reads_back_and_only_done_or_lost_is_over() {
        for stage in Stage::ALL {
            assert_eq!(Stage::parse(stage.as_str()).unwrap(), stage);
            assert_eq!(stage.over(), matches!(stage, Stage::Done | Stage::Lost));
        }
        assert!(Stage::parse("countered").is_err());
    }
}
