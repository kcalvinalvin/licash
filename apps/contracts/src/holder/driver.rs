//! One opening driven by its holder: her machine stepped with the hub's
//! answers, each step stored (the instances it keeps first) before what
//! it asks is done.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use licash_contract::opening::{Context, Holder, Input, Out, Phase, Quote, Trust};
use licash_contract::wire::Kind;
use licash_wallet::{Transport, Wallet};

use super::chain::Chain;
use super::store::Store;
use crate::{Frame, Opening, INSTANCE, MAX_BODY, OPEN, STEP};

/// How long to wait before asking a working hub again.
const RETRY: Duration = Duration::from_secs(2);

/// How many times to ask a working hub again before trying later.
const PATIENCE: u32 = 900;

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
                    for frame in self.post(message).await? {
                        let bytes = match frame {
                            Frame::Message(bytes) => bytes,
                            Frame::Instance { stage, index } => self.fetch(stage, index).await?,
                        };
                        queue.extend(self.step(Input::Message(bytes)).await?);
                    }
                }
                Out::Sign {
                    funding,
                    prevouts,
                    inputs,
                } => {
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
        for (stage, index) in self.holder().missing() {
            let bytes = self.fetch(stage, index).await?;
            outs.extend(self.step(Input::Message(bytes)).await?);
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

    /// Instance `index` of assertion `stage`, as the hub serves it: tens
    /// of MB each, as large as any message an opening has.
    async fn fetch(&self, stage: usize, index: usize) -> Result<Vec<u8>> {
        let path = INSTANCE
            .replace("{id}", &crate::hex_id(&self.id))
            .replace("{stage}", &stage.to_string())
            .replace("{index}", &index.to_string());
        let url = format!("{}{path}", self.hub);
        let started = Instant::now();
        let response = self.wallet.transport().download(&url, MAX_BODY).await;
        let response = response.map_err(|e| anyhow!("{}: {e}", self.hub))?;
        match response.status {
            200 => {
                let (id, bytes, took) = (
                    crate::hex_id(&self.id),
                    response.body.len(),
                    started.elapsed(),
                );
                tracing::info!(%id, stage, index, bytes, ?took, "an instance kept downloaded");
                Ok(response.body)
            }
            status => Err(failed(&self.hub, status, &response.body)),
        }
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
