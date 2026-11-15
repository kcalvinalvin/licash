//! Stepping an opening: one task at a time, each step stored before the
//! effects it asks for run.

use std::collections::VecDeque;
use std::sync::Arc;

use bitcoin::{OutPoint, TxOut};
use licash_contract::opening::{Context, Hub, Input, Out, Quote};
use licash_contract::wire::{self, Envelope, Kind, Writer};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use super::{Answer, Openings, Refused, MAX_UNDER_WAY, WAIT};
use crate::{Frame, Opening};

/// How deep the block a quote is checkpointed at may be.
const MAX_AGE: u32 = 6;

/// Clears an opening's busy mark however its task ends.
struct Busy(Arc<Openings>, [u8; 32]);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.busy().remove(&self.1);
    }
}

/// What a message steps: the stored opening, or a new one on `quote`.
enum Start {
    Stored,
    New {
        quote: Box<Quote>,
        deposit: OutPoint,
    },
}

fn not_found() -> Refused {
    Refused {
        status: axum::http::StatusCode::NOT_FOUND,
        reason: "no such opening".into(),
    }
}

impl Openings {
    /// A holder's [`Opening`]: refused unless on the hub's terms, at a
    /// recent block of its chain, of a deposit confirmed as she says.
    pub async fn open(self: &Arc<Self>, body: &[u8]) -> Result<Answer, Refused> {
        let opening = Opening::decode(body).map_err(Refused::bad)?;
        let envelope = Envelope::from_bytes(&opening.open).map_err(Refused::bad)?;
        if !matches!(envelope.kind, Kind::Open) {
            return Err(Refused::bad("an opening starts with an Open"));
        }
        let open = wire::Open::decode(&envelope.body).map_err(Refused::bad)?;
        self.terms_of(&opening.quote).await?;
        let confirmed = self.confirmed(open.deposit, &open.coin).await;
        if !confirmed.map_err(Refused::unavailable)? {
            return Err(Refused::bad(
                "her deposit is no confirmed coin of the chain as she says",
            ));
        }
        let start = Start::New {
            quote: Box::new(opening.quote),
            deposit: open.deposit,
        };
        self.take(envelope.id, opening.open, start).await
    }

    /// The holder's next message of opening `id`.
    pub async fn step(self: &Arc<Self>, id: [u8; 32], message: Vec<u8>) -> Result<Answer, Refused> {
        let envelope = Envelope::from_bytes(&message).map_err(Refused::bad)?;
        if envelope.id != id {
            return Err(Refused::bad("a message of another opening"));
        }
        self.take(id, message, Start::Stored).await
    }

    /// The instance `index` of assertion `stage` the hub serves opening
    /// `id`'s holder, built again.
    pub async fn instance(
        &self,
        id: [u8; 32],
        stage: usize,
        index: usize,
    ) -> Result<Vec<u8>, Refused> {
        let ctx = self.context().await?;
        let row = self.store().row(&id).map_err(Refused::unavailable)?;
        let row = row.ok_or_else(not_found)?;
        let built = heavy(&self.heavy, move || {
            Hub::load(&row.state, &ctx)?.instance(stage, index, &ctx)
        });
        built.await?.map_err(Refused::bad)
    }

    /// Refused unless `quote` is the hub's: its terms, checkpointed at a
    /// block of its best chain at most [`MAX_AGE`] deep.
    async fn terms_of(&self, quote: &Quote) -> Result<(), Refused> {
        let height = quote.chain.checkpoint.height();
        let tip = self
            .source
            .tip()
            .await
            .map_err(Refused::unavailable)?
            .height;
        if height > tip || tip - height > MAX_AGE {
            return Err(Refused::bad(format!(
                "a quote of block {height}, the tip at {tip}: ask again"
            )));
        }
        let checkpoint = self.source.checkpoint(height).await;
        let ours = self.timing.quote(
            self.keyset.clone(),
            checkpoint.map_err(Refused::unavailable)?,
        );
        let bytes = |q: &Quote| {
            let mut w = Writer(Vec::new());
            w.quote(q);
            w.0
        };
        if bytes(&ours) != bytes(quote) {
            return Err(Refused::bad("not the hub's quote: ask again"));
        }
        Ok(())
    }

    /// Step opening `id` with `message` in a task of its own, answering
    /// its frames, or 202 if it takes longer than [`WAIT`] or another task
    /// steps the opening.
    async fn take(
        self: &Arc<Self>,
        id: [u8; 32],
        message: Vec<u8>,
        start: Start,
    ) -> Result<Answer, Refused> {
        if !self.busy().insert(id) {
            return Ok(Answer::Working);
        }
        let busy = Busy(self.clone(), id);
        let this = self.clone();
        let task = tokio::spawn(async move {
            let _busy = busy;
            this.run(id, message, start).await
        });
        match tokio::time::timeout(WAIT, task).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(e)) => Err(Refused::unavailable(e)),
            Err(_) => Ok(Answer::Working),
        }
    }

    async fn run(&self, id: [u8; 32], message: Vec<u8>, start: Start) -> Result<Answer, Refused> {
        let ctx = self.context().await?;
        let last: [u8; 32] = Sha256::digest(&message).into();
        let row = self.store().row(&id).map_err(Refused::unavailable)?;
        let (hub, out) = match (row, start) {
            (Some(row), _) if row.abandoned => return Err(Refused::bad("an opening given up")),
            (Some(row), _) => {
                let hub = Hub::load(&row.state, &ctx).map_err(Refused::unavailable)?;
                if row.last == last {
                    // Asked again: answered as before, its effects run.
                    let out = hub.pending().map_err(Refused::unavailable)?;
                    (hub, out)
                } else {
                    let (hub, out) =
                        stepped(&self.heavy, hub, Input::Message(message), ctx).await?;
                    self.save(&id, &hub, &last)?;
                    (hub, out)
                }
            }
            (None, Start::Stored) => return Err(not_found()),
            (None, Start::New { quote, deposit }) => {
                if self.store().under_way().map_err(Refused::unavailable)? >= MAX_UNDER_WAY {
                    return Err(Refused::unavailable("too many openings under way: later"));
                }
                let hub = Hub::new(*quote, self.timing.bounds);
                let (hub, out) = stepped(&self.heavy, hub, Input::Message(message), ctx).await?;
                let state = hub.to_bytes().map_err(Refused::unavailable)?;
                let inserted = self
                    .store()
                    .insert(&id, phase(&hub), ctx.trust.mock, &deposit, &last, &state)
                    .map_err(Refused::unavailable)?;
                if !inserted {
                    return Err(Refused::bad("her deposit is another opening's"));
                }
                tracing::info!(id = %crate::hex_id(&id), mock = ctx.trust.mock, "an opening started");
                (hub, out)
            }
        };
        self.effects(id, hub, out, last, ctx).await
    }

    /// Carry out `out`, what `hub` asked: its messages and the instances
    /// it serves answered, F funded, signed and broadcast by the node.
    async fn effects(
        &self,
        id: [u8; 32],
        mut hub: Hub,
        out: Vec<Out>,
        last: [u8; 32],
        ctx: Context,
    ) -> Result<Answer, Refused> {
        let mut queue: VecDeque<Out> = out.into();
        let mut frames = Vec::new();
        while let Some(out) = queue.pop_front() {
            match out {
                Out::Send(bytes) => frames.push(Frame::Message(bytes)),
                Out::Serve { stage, index } => frames.push(Frame::Instance { stage, index }),
                Out::Fund {
                    deposit,
                    coin,
                    outputs,
                    publication,
                } => {
                    let _funding = self.funding.lock().await;
                    let funded = self.fund(deposit, &coin, outputs, &publication).await;
                    let (funding, prevouts) = funded.map_err(Refused::unavailable)?;
                    let coins: Vec<OutPoint> = funding
                        .input
                        .iter()
                        .map(|i| i.previous_output)
                        .filter(|o| *o != deposit)
                        .collect();
                    let (next, more) =
                        stepped(&self.heavy, hub, Input::Funded { funding, prevouts }, ctx).await?;
                    hub = next;
                    self.save(&id, &hub, &last)?;
                    self.store()
                        .set_coins(&id, &coins)
                        .map_err(Refused::unavailable)?;
                    self.lock_coins(&coins).await;
                    tracing::info!(id = %crate::hex_id(&id), coins = coins.len(), "F funded");
                    queue.extend(more);
                }
                Out::Sign {
                    funding,
                    prevouts,
                    inputs,
                } => {
                    let theirs: Vec<(OutPoint, TxOut)> = funding
                        .input
                        .iter()
                        .zip(&prevouts)
                        .enumerate()
                        .filter(|(i, _)| !inputs.contains(i))
                        .map(|(_, (input, coin))| (input.previous_output, coin.clone()))
                        .collect();
                    let signed = self.sign(&funding, &theirs).await;
                    let signed = signed.map_err(Refused::unavailable)?;
                    let (next, more) =
                        stepped(&self.heavy, hub, Input::Signed(signed), ctx).await?;
                    hub = next;
                    self.save(&id, &hub, &last)?;
                    tracing::info!(id = %crate::hex_id(&id), "F signed");
                    queue.extend(more);
                }
                Out::Broadcast(tx) => match self.source.broadcast(&tx).await {
                    Ok(txid) => tracing::info!(id = %crate::hex_id(&id), %txid, "F broadcast"),
                    Err(e) => tracing::warn!(id = %crate::hex_id(&id), "broadcasting F: {e:#}"),
                },
                Out::Keep { .. } => {
                    return Err(Refused::unavailable(
                        "the hub was asked to keep an instance",
                    ));
                }
            }
        }
        Ok(Answer::Frames(frames))
    }

    /// Store `hub` after a step that took the message of digest `last`.
    fn save(&self, id: &[u8; 32], hub: &Hub, last: &[u8; 32]) -> Result<(), Refused> {
        let state = hub.to_bytes().map_err(Refused::unavailable)?;
        // Past its signatures, it is the holder's to fund F.
        let irrevocable = !hub.abandonable();
        self.store()
            .save(id, phase(hub), irrevocable, last, &state)
            .map_err(Refused::unavailable)
    }
}

fn phase(hub: &Hub) -> u8 {
    hub.phase().map_or(0, |p| p as u8)
}

/// `hub` stepped with `input` off the async threads, in its turn
/// ([`heavy`]): refused as the machine refuses it, unchanged.
async fn stepped(
    limit: &Arc<Semaphore>,
    mut hub: Hub,
    input: Input,
    ctx: Context,
) -> Result<(Hub, Vec<Out>), Refused> {
    let stepped = heavy(limit, move || {
        let out = hub.step(input, &ctx);
        (hub, out)
    });
    let (hub, out) = stepped.await?;
    Ok((hub, out.map_err(Refused::bad)?))
}

/// `work` off the async threads once `limit` has a permit free, held until
/// the work is done, even if no one waits for it any more: the most at once
/// ([`super::HEAVY`]).
async fn heavy<T: Send + 'static>(
    limit: &Arc<Semaphore>,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Refused> {
    let permit = limit.clone().acquire_owned().await;
    let permit = permit.map_err(Refused::unavailable)?;
    let done = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    });
    done.await.map_err(Refused::unavailable)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn heavy_work_runs_one_at_a_time_even_once_no_one_waits_for_it() {
        let limit = Arc::new(Semaphore::new(super::super::HEAVY));
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let work = || {
            let (running, most) = (running.clone(), most.clone());
            move || {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(100));
                running.fetch_sub(1, Ordering::SeqCst);
            }
        };
        // One whose request was answered 202: no one waits for it, and it
        // keeps its turn.
        let unwaited = heavy(&limit, work());
        let waited = tokio::time::timeout(Duration::from_millis(20), unwaited).await;
        assert!(waited.is_err());
        let others: Vec<_> = (0..4)
            .map(|_| {
                let (limit, work) = (limit.clone(), work());
                tokio::spawn(async move { heavy(&limit, work).await })
            })
            .collect();
        for other in others {
            other.await.unwrap().unwrap();
        }
        assert_eq!(most.load(Ordering::SeqCst), 1);
        assert_eq!(running.load(Ordering::SeqCst), 0);
    }
}
