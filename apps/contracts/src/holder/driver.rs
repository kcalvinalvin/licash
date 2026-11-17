//! One opening driven by its holder: her machine stepped with the hub's
//! answers, each step stored before its disclosures, instance uploads,
//! signatures or other effects leave the wallet.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use bitcoin::{Script, Transaction};
use licash_contract::opening::{Context, Holder, Input, Out, Phase, Quote, Trust};
use licash_contract::wire::{self, Envelope, Kind};
use licash_wallet::{DepositState, Transport, Wallet};

use super::chain::Chain;
use super::store::Store;
use super::DEPOSIT_MARGIN;
use crate::{Frame, Opening, INSTANCE, OPEN, STEP};

/// How long to wait before asking a working hub again.
const RETRY: Duration = Duration::from_secs(2);

/// How many times to ask a working hub again before trying later.
const PATIENCE: u32 = 900;

/// How many instances are uploaded beside each other, each on a circuit
/// of its own over Tor: one carries only so much.
const UPLOADS: usize = 3;

/// What an opening sent to and got from its hub since this process
/// started driving it, in bytes, and since when: its speed. Kept in
/// memory only, lost with the process, which says nothing of it then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Traffic {
    pub bytes: u64,
    pub since: Instant,
    /// About how many bytes it sends and gets in all, once it builds the
    /// first of the instances it uploads, most of it.
    pub expected: Option<u64>,
}

static TRAFFIC: LazyLock<Mutex<HashMap<[u8; 32], Traffic>>> = LazyLock::new(Mutex::default);

/// The opening `id`'s [`Traffic`], if this process drove it.
pub fn traffic(id: &[u8; 32]) -> Option<Traffic> {
    let all = TRAFFIC.lock().unwrap_or_else(PoisonError::into_inner);
    all.get(id).copied()
}

fn count(id: &[u8; 32], bytes: usize) {
    let mut all = TRAFFIC.lock().unwrap_or_else(PoisonError::into_inner);
    let traffic = all.entry(*id).or_insert_with(|| Traffic {
        bytes: 0,
        since: Instant::now(),
        expected: None,
    });
    traffic.bytes = traffic.bytes.saturating_add(bytes as u64);
}

/// The opening `id` has `instances` of `bytes` each to upload still, from
/// now: what it sends in all, about, unless known already.
fn expect(id: &[u8; 32], instances: usize, bytes: usize) {
    let mut all = TRAFFIC.lock().unwrap_or_else(PoisonError::into_inner);
    let traffic = all.entry(*id).or_insert_with(|| Traffic {
        bytes: 0,
        since: Instant::now(),
        expected: None,
    });
    let rest = (instances as u64).saturating_mul(bytes as u64);
    traffic
        .expected
        .get_or_insert(traffic.bytes.saturating_add(rest));
}

/// The hub refused what she sent, or she what it sent: the opening goes
/// no further.
#[derive(Debug)]
pub(super) struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

pub(super) struct Driver<'a, T, C> {
    pub wallet: &'a Wallet<T>,
    pub chain: &'a C,
    pub store: &'a Store,
    pub trust: Trust,
    /// The hub's URL.
    pub hub: String,
    pub id: [u8; 32],
    /// Her machine, but while a step takes it.
    pub holder: Option<Holder>,
    /// The quote she opens on, sent with her Open the first time.
    pub quote: Option<Quote>,
}

impl<T: Transport, C: Chain> Driver<'_, T, C> {
    pub fn holder(&self) -> &Holder {
        self.holder.as_ref().expect("a holder between steps")
    }

    /// Carry out `outs`, and what the hub's answers to them ask, until
    /// nothing is left to do.
    pub async fn drive(&mut self, outs: Vec<Out>) -> Result<()> {
        let mut queue: VecDeque<Out> = outs.into();
        while let Some(out) = queue.pop_front() {
            match out {
                Out::Send(message) if matches!(Kind::peek(&message), Some(Kind::Funded)) => {
                    self.funded().await;
                    // The hub broadcasts F too, once told: it may not hear.
                    if let Err(e) = self.post(message).await {
                        tracing::warn!(id = %crate::hex_id(&self.id), "telling the hub F is signed: {e:#}");
                    }
                }
                Out::Send(message) => {
                    let frames = self.post(message).await?;
                    for frame in frames {
                        let Frame::Message(bytes) = frame;
                        queue.extend(self.step(Input::Message(bytes)).await?);
                    }
                }
                Out::Serve { i } => {
                    // With those after it, sent beside each other.
                    let mut batch = vec![i];
                    while let Some(&Out::Serve { i }) = queue.front() {
                        batch.push(i);
                        queue.pop_front();
                    }
                    for frame in self.serve(batch).await? {
                        let Frame::Message(bytes) = frame;
                        queue.extend(self.step(Input::Message(bytes)).await?);
                    }
                }
                Out::Sign {
                    funding,
                    prevouts,
                    inputs,
                } => {
                    self.paying_deposits(&funding).await?;
                    let mut signed = funding;
                    for index in inputs {
                        signed = self.wallet.sign_input(&signed, &prevouts, index)?;
                    }
                    queue.extend(self.step(Input::Signed(signed)).await?);
                }
                Out::Broadcast(tx) => {
                    self.chain.broadcast(&tx).await.map_err(|e| anyhow!(e))?;
                }
                Out::Keep { .. } | Out::Fund { .. } => {
                    bail!("the holder asked to do the hub's part")
                }
            }
        }
        Ok(())
    }

    /// Carry on from her stored state. The pinned choice is stored before
    /// either its opening shares or any rebuilt kept instance is uploaded:
    /// but those the hub answered as had, until it has them all and still
    /// waits, as if it lost one. Then all of them again, the next time.
    pub async fn resume(&mut self) -> Result<()> {
        let outs = self
            .holder()
            .pending()
            .map_err(|e| Refusal(format!("{e:#}")))?;
        let served = self.store.served_of(&self.id)?;
        let had = |o: &Out| matches!(o, Out::Serve { i } if served.contains(i));
        let serves = outs
            .iter()
            .filter(|o| matches!(o, Out::Serve { .. }))
            .count();
        let all_served = serves > 0 && outs.iter().filter(|o| had(o)).count() == serves;
        let outs: Vec<Out> = outs.into_iter().filter(|o| !had(o)).collect();
        self.drive(outs).await?;
        if all_served && self.holder().phase() == Phase::Chosen {
            tracing::warn!(id = %crate::hex_id(&self.id), "the hub has every instance, yet waits: all again");
            self.store.unserve(&self.id)?;
        }
        Ok(())
    }

    /// Give the opening up if `result` says the hub or she refused, short
    /// of her signing F: her coin freed if she signed it into F.
    pub fn settle(&self, result: Result<()>) -> Result<()> {
        let Err(e) = result else {
            return Ok(());
        };
        let holder = self.holder();
        if e.downcast_ref::<Refusal>().is_some() && holder.abandonable() {
            self.store.abandon(&self.id, &format!("{e:#}"))?;
            // A deposit her change was to pay, never to be paid now.
            for hash in self.deposits_paid_by(holder.change()) {
                if let Err(e) = self.wallet.give_up_deposit(&hash, "its opening given up") {
                    tracing::warn!(%hash, "giving the deposit of F's change up: {e}");
                }
            }
            if holder.phase() == Phase::Signing {
                if let Some(f) = holder.funding() {
                    // Not reserved unless she signed before stopping.
                    let _ = self.wallet.abandon_silent(&f.tx.compute_txid());
                }
            }
            tracing::warn!(id = %crate::hex_id(&self.id), "an opening given up: {e:#}");
        }
        Err(e)
    }

    /// The deposits of the wallet's `script` pays: their hashes.
    fn deposits_paid_by(&self, script: &Script) -> Vec<String> {
        let deposits = self.wallet.deposits().unwrap_or_default();
        let paid = deposits.into_iter().filter(|d| {
            let to = d.terms.as_ref().and_then(|t| super::script_of(&t.address));
            to.as_deref() == Some(script)
        });
        paid.map(|d| d.hash).collect()
    }

    /// Refused, short of her signing `funding`, if an output of it pays a
    /// deposit of the wallet's other than exactly, open and unpaid, with
    /// [`DEPOSIT_MARGIN`] blocks left to confirm in: an opening resumed
    /// late, say. Its notes would never come, its coins back only after
    /// the deposit's timeout.
    async fn paying_deposits(&self, funding: &Transaction) -> Result<()> {
        let tip = self.chain.height().await.map_err(|e| anyhow!(e))?;
        let deposits = self.wallet.deposits()?;
        for out in &funding.output {
            for d in &deposits {
                let Some(terms) = &d.terms else {
                    continue;
                };
                if super::script_of(&terms.address).as_ref() != Some(&out.script_pubkey) {
                    continue;
                }
                let open = d.state == DepositState::Open && d.funding.is_none();
                let exact = out.value.to_sat() == terms.pays;
                let time = terms.fund_by >= tip.saturating_add(DEPOSIT_MARGIN);
                if !(open && exact && time) {
                    let (hash, value) = (&d.hash, out.value.to_sat());
                    return Err(Refusal(format!(
                        "F pays deposit {hash} {value} sat, of {} by block {} at {tip}, {:?}: \
                         not signed",
                        terms.pays, terms.fund_by, d.state
                    ))
                    .into());
                }
            }
        }
        Ok(())
    }

    async fn context(&self) -> Result<Context> {
        let tip = self.chain.height().await.map_err(|e| anyhow!(e))?;
        Ok(Context {
            tip,
            trust: self.trust,
        })
    }

    /// Her machine stepped with `input`, with state durable before effects.
    async fn step(&mut self, input: Input) -> Result<Vec<Out>> {
        let ctx = self.context().await?;
        let started = Instant::now();
        let mut holder = self.holder.take().expect("a holder between steps");
        let stepped = tokio::task::spawn_blocking(move || {
            let out = holder.step(input, &ctx);
            (holder, out)
        });
        let (holder, out) = stepped.await?;
        self.holder = Some(holder);
        let out = out.map_err(|e| Refusal(format!("{e:#}")))?;
        let holder = self.holder();
        let funding = holder.funding().map(|f| f.tx.compute_txid());
        let state = holder.to_bytes();
        self.store.save(&self.id, holder.phase(), funding, &state)?;
        // Where an opening's time goes, step by step.
        let (id, phase) = (crate::hex_id(&self.id), holder.phase());
        tracing::info!(%id, ?phase, took = ?started.elapsed(), "an opening's step taken");
        Ok(out)
    }

    /// F signed whole: broadcast, and watched from now on.
    async fn funded(&self) {
        let id = crate::hex_id(&self.id);
        let Some(f) = self.holder().funding() else {
            return;
        };
        let txid = f.tx.compute_txid();
        match self.chain.broadcast(f.tx).await {
            Ok(()) => tracing::info!(%id, %txid, "F broadcast"),
            Err(e) => tracing::warn!(%id, %txid, "broadcasting F: {e}"),
        }
    }

    /// Her instances `batch` built and uploaded, [`UPLOADS`] at a time:
    /// the hub's answers to them, once all are, or the first failure. Each
    /// the hub answered is kept as served, whatever the others do.
    async fn serve(&mut self, batch: Vec<u32>) -> Result<Vec<Frame>> {
        use futures_util::StreamExt as _;
        let holder = std::sync::Arc::new(self.holder.take().expect("a holder between steps"));
        let (id, total) = (self.id, batch.len());
        let this = &*self;
        let uploads = futures_util::stream::iter(batch).map(|i| {
            let holder = holder.clone();
            async move {
                let built = tokio::task::spawn_blocking(move || holder.instance(i)).await;
                let bytes = built.map_err(|e| anyhow!(e))??;
                expect(&id, total, bytes.len());
                let message = Envelope {
                    id,
                    kind: Kind::Instance,
                    body: wire::Instance { i, bytes }.encode(),
                }
                .to_bytes();
                let frames = this.send(message).await?;
                // The hub has it: not sent again if the opening resumes.
                this.store.served(&id, i)?;
                Ok::<_, anyhow::Error>(frames)
            }
        });
        let answers: Vec<Result<Vec<Frame>>> = uploads.buffer_unordered(UPLOADS).collect().await;
        let holder = std::sync::Arc::try_unwrap(holder);
        self.holder = Some(holder.unwrap_or_else(|_| unreachable!("every upload done")));
        let mut frames = Vec::new();
        for answer in answers {
            frames.extend(answer?);
        }
        Ok(frames)
    }

    /// POST `message` to the hub: its answer, asked again while it works.
    async fn post(&mut self, message: Vec<u8>) -> Result<Vec<Frame>> {
        if let Some(quote) = self.quote.take() {
            if matches!(Kind::peek(&message), Some(Kind::Open)) {
                let opening = Opening {
                    quote,
                    open: message,
                };
                let url = format!("{}{OPEN}", self.hub);
                return self.ask(url, opening.encode()).await;
            }
        }
        self.send(message).await
    }

    /// POST `message`, of an opening under way, to the hub: its answer,
    /// asked again while it works.
    async fn send(&self, message: Vec<u8>) -> Result<Vec<Frame>> {
        let route = if Kind::peek(&message) == Some(Kind::Instance) {
            INSTANCE
        } else {
            STEP
        };
        let path = route.replace("{id}", &crate::hex_id(&self.id));
        self.ask(format!("{}{path}", self.hub), message).await
    }

    /// POST `body` to `url`: the hub's answer, asked again while it works.
    async fn ask(&self, url: String, body: Vec<u8>) -> Result<Vec<Frame>> {
        let started = Instant::now();
        for _ in 0..PATIENCE {
            let request = Some(body.clone());
            let response = self.wallet.transport().request(&url, request).await;
            let response = response.map_err(|e| anyhow!("{}: {e}", self.hub))?;
            count(&self.id, body.len() + response.body.len());
            match response.status {
                200 => {
                    let (id, took) = (crate::hex_id(&self.id), started.elapsed());
                    tracing::info!(%id, ?took, "the hub answered an opening's step");
                    return Frame::decode_all(&response.body).context("the hub's answer");
                }
                202 => tokio::time::sleep(RETRY).await,
                status => return Err(failed(&self.hub, status, &response.body)),
            }
        }
        bail!("{}: still working on the opening: later", self.hub)
    }
}

/// The hub's answer of `status`: refused if it will refuse it again.
pub(super) fn failed(hub: &str, status: u16, body: &[u8]) -> anyhow::Error {
    let reason = String::from_utf8_lossy(&body[..body.len().min(500)]).into_owned();
    if (400..500).contains(&status) {
        Refusal(format!("{hub} refused it: {reason}")).into()
    } else {
        anyhow!("{hub}: status {status}: {reason}")
    }
}
