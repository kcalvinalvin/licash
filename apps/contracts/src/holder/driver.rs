//! One opening driven by its holder: her machine stepped with the hub's
//! answers, each step stored (the instances it keeps first) before what
//! it asks is done.

use std::collections::VecDeque;
use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use bitcoin::{Script, Transaction};
use futures_util::future::Either;
use futures_util::{Stream, StreamExt};
use licash_contract::opening::{Context, Holder, Input, Out, Phase, Quote, Trust};
use licash_contract::wire::{Kind, STAGES};
use licash_contract::KEPT;
use licash_wallet::{DepositState, Transport, Wallet};

use super::chain::Chain;
use super::store::Store;
use super::DEPOSIT_MARGIN;
use crate::{Frame, Opening, INSTANCE, MAX_BODY, OPEN, STEP};

/// How long to wait before asking a working hub again.
const RETRY: Duration = Duration::from_secs(2);

/// How many times to ask a working hub again before trying later.
const PATIENCE: u32 = 900;

/// How many frames of a hub's answer are under way at once: all the
/// instances an opening keeps, downloaded together, and the message the
/// hub sends before them. Tens of MB each, over Tor each on circuits of
/// its own, as one carries only so much: on signet 25 MB took 4-6 min
/// over Tor, 10 s without.
const DOWNLOADING_AT_ONCE: usize = STAGES * KEPT + 1;

/// How many times an instance is asked for before the opening waits for
/// later: an instance asked again is answered the same.
const DOWNLOAD_TRIES: u32 = 3;

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
                    let (wallet, hub, id) = (self.wallet, self.hub.clone(), self.id);
                    let hub = hub.as_str();
                    let fetch = move |stage, index| fetch(wallet, hub, id, stage, index);
                    let mut each = each_frame(frames, fetch);
                    while let Some(bytes) = each.next().await {
                        queue.extend(self.step(Input::Message(bytes?)).await?);
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
                Out::Keep { .. } | Out::Serve { .. } | Out::Fund { .. } => {
                    bail!("the holder asked to do the hub's part")
                }
            }
        }
        Ok(())
    }

    /// Carry on from her stored state: what its step asked, and the
    /// instances still to keep.
    pub async fn resume(&mut self) -> Result<()> {
        let ctx = self.context().await?;
        let outs = self
            .holder()
            .pending(&ctx)
            .map_err(|e| Refusal(format!("{e:#}")))?;
        let mut outs: Vec<Out> = outs
            .into_iter()
            .filter(|o| !matches!(o, Out::Keep { .. }))
            .collect();
        let missing = self.holder().missing().into_iter();
        let frames = missing.map(|(stage, index)| Frame::Instance { stage, index });
        let (wallet, hub, id) = (self.wallet, self.hub.clone(), self.id);
        let hub = hub.as_str();
        let fetch = move |stage, index| fetch(wallet, hub, id, stage, index);
        let mut each = each_frame(frames.collect(), fetch);
        while let Some(bytes) = each.next().await {
            outs.extend(self.step(Input::Message(bytes?)).await?);
        }
        self.drive(outs).await
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

    /// Her machine stepped with `input`: the instances it keeps on disk,
    /// then her state stored, then the rest of what it asks returned.
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
        let mut rest = Vec::with_capacity(out.len());
        for o in out {
            match o {
                Out::Keep {
                    stage,
                    index,
                    bytes,
                    ..
                } => self.store.keep(&self.id, stage, index, &bytes)?,
                o => rest.push(o),
            }
        }
        let holder = self.holder();
        let funding = holder.funding().map(|f| f.tx.compute_txid());
        let state = holder.to_bytes();
        self.store.save(&self.id, holder.phase(), funding, &state)?;
        // Where an opening's time goes, step by step.
        let (id, phase) = (crate::hex_id(&self.id), holder.phase());
        tracing::info!(%id, ?phase, took = ?started.elapsed(), "an opening's step taken");
        Ok(rest)
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

    /// POST `message` to the hub: its answer, asked again while it works.
    async fn post(&mut self, message: Vec<u8>) -> Result<Vec<Frame>> {
        let (url, body) = match self.quote.take() {
            Some(quote) if matches!(Kind::peek(&message), Some(Kind::Open)) => {
                let opening = Opening {
                    quote,
                    open: message,
                };
                (format!("{}{OPEN}", self.hub), opening.encode())
            }
            _ => {
                let path = STEP.replace("{id}", &crate::hex_id(&self.id));
                (format!("{}{path}", self.hub), message)
            }
        };
        let started = Instant::now();
        for _ in 0..PATIENCE {
            let body = Some(body.clone());
            let response = self.wallet.transport().request(&url, body).await;
            let response = response.map_err(|e| anyhow!("{}: {e}", self.hub))?;
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

/// The bytes of each of `frames`, in order: the instances among them
/// downloaded with `fetch`, [`DOWNLOADING_AT_ONCE`] frames under way at
/// once (a message taking no time), each taken once those before it are.
fn each_frame<'f, F>(
    frames: Vec<Frame>,
    fetch: impl Fn(usize, usize) -> F + 'f,
) -> impl Stream<Item = Result<Vec<u8>>> + 'f
where
    F: Future<Output = Result<Vec<u8>>> + 'f,
{
    futures_util::stream::iter(frames)
        .map(move |frame| match frame {
            Frame::Message(bytes) => Either::Left(std::future::ready(Ok(bytes))),
            Frame::Instance { stage, index } => Either::Right(fetch(stage, index)),
        })
        .buffered(DOWNLOADING_AT_ONCE)
}

/// Instance `index` of assertion `stage` of the opening `id`, as `hub`
/// serves it: tens of MB each, as large as any message an opening has.
async fn fetch<T: Transport>(
    wallet: &Wallet<T>,
    hub: &str,
    id: [u8; 32],
    stage: usize,
    index: usize,
) -> Result<Vec<u8>> {
    let id = crate::hex_id(&id);
    let path = INSTANCE
        .replace("{id}", &id)
        .replace("{stage}", &stage.to_string())
        .replace("{index}", &index.to_string());
    let url = format!("{hub}{path}");
    let started = Instant::now();
    let mut tries = 1;
    let response = loop {
        // Each over Tor on new circuits: one may stall, or crawl.
        match wallet.transport().download(&url, MAX_BODY).await {
            Ok(response) => break response,
            Err(e) if tries < DOWNLOAD_TRIES => {
                tracing::info!(%id, stage, index, tries, "an instance's download failed, again: {e}");
                tries += 1;
            }
            Err(e) => bail!("{hub}: {e}"),
        }
    };
    match response.status {
        200 => {
            let (bytes, took) = (response.body.len(), started.elapsed());
            tracing::info!(%id, stage, index, bytes, ?took, "an instance kept downloaded");
            Ok(response.body)
        }
        status => Err(failed(hub, status, &response.body)),
    }
}

/// The hub's answer of `status`: refused if it will refuse it again.
fn failed(hub: &str, status: u16, body: &[u8]) -> anyhow::Error {
    let reason = String::from_utf8_lossy(&body[..body.len().min(500)]).into_owned();
    if (400..500).contains(&status) {
        Refusal(format!("{hub} refused it: {reason}")).into()
    } else {
        anyhow!("{hub}: status {status}: {reason}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use licash::spend::ChainProfile;
    use licash_wallet::Response;

    use super::*;

    /// A hub whose downloads fail so many times, then are served.
    struct Failing(AtomicUsize);

    impl Transport for Failing {
        async fn request(&self, _: &str, _: Option<Vec<u8>>) -> Result<Response, String> {
            Err("not asked".into())
        }

        async fn download(&self, url: &str, _: usize) -> Result<Response, String> {
            let left = self.0.load(Ordering::SeqCst);
            if left > 0 {
                self.0.store(left - 1, Ordering::SeqCst);
                return Err("the answer stalled for 60 s".into());
            }
            let body = url.as_bytes().to_vec();
            Ok(Response { status: 200, body })
        }
    }

    #[tokio::test]
    async fn an_instance_whose_download_failed_is_asked_for_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.sqlite");
        let regtest = ChainProfile::Core30DefaultRegtest;
        let failing = Failing(AtomicUsize::new(DOWNLOAD_TRIES as usize - 1));
        let wallet = Wallet::create(&path, &[7; 32], regtest, failing).unwrap();
        let (hub, id) = ("http://hub.example", [1; 32]);
        let got = fetch(&wallet, hub, id, 1, 3).await.unwrap();
        let path = INSTANCE
            .replace("{id}", &crate::hex_id(&id))
            .replace("{stage}", "1")
            .replace("{index}", "3");
        assert_eq!(got, format!("{hub}{path}").into_bytes());
        // Failing every time asked: the opening waits.
        wallet
            .transport()
            .0
            .store(DOWNLOAD_TRIES as usize, Ordering::SeqCst);
        let failed = fetch(&wallet, hub, id, 1, 3).await.unwrap_err();
        assert!(failed.to_string().contains("stalled"), "{failed:#}");
        assert!(failed.downcast_ref::<Refusal>().is_none());
    }

    #[tokio::test]
    async fn instances_download_together_each_frame_taken_in_order() {
        let (under_way, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let fetch = |stage: usize, index: usize| {
            let (under_way, most) = (&under_way, &most);
            async move {
                let now = under_way.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                // Those asked first the slowest to come.
                tokio::time::sleep(Duration::from_millis(40 - 10 * index as u64)).await;
                under_way.fetch_sub(1, Ordering::SeqCst);
                if stage == 9 {
                    bail!("not served");
                }
                Ok(vec![stage as u8, index as u8])
            }
        };
        let instance = |stage, index| Frame::Instance { stage, index };
        // As a hub answers: its message, then the instances kept.
        let frames = vec![
            Frame::Message(vec![7]),
            instance(0, 0),
            instance(0, 2),
            instance(1, 1),
            instance(1, 3),
            Frame::Message(vec![8]),
        ];
        let each: Vec<Vec<u8>> = each_frame(frames, fetch)
            .map(Result::unwrap)
            .collect()
            .await;
        let wanted = [[7].as_slice(), &[0, 0], &[0, 2], &[1, 1], &[1, 3], &[8]];
        assert_eq!(each, wanted);
        assert_eq!(most.load(Ordering::SeqCst), STAGES * KEPT);
        // Never more at once.
        most.store(0, Ordering::SeqCst);
        let frames: Vec<Frame> = (0..9).map(|i| instance(i % 2, i % 4)).collect();
        let each: Vec<Vec<u8>> = each_frame(frames, fetch)
            .map(Result::unwrap)
            .collect()
            .await;
        assert_eq!(each[8], [0, 0]);
        assert_eq!(most.load(Ordering::SeqCst), DOWNLOADING_AT_ONCE);

        // One not served: those before it taken, then its error.
        let frames = vec![instance(0, 0), instance(9, 1), instance(0, 2)];
        let mut each = each_frame(frames, fetch);
        assert_eq!(each.next().await.unwrap().unwrap(), [0, 0]);
        assert!(each.next().await.unwrap().is_err());
    }
}
