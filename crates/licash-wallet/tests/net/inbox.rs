//! A hub's inbox in memory, kept as the hub service keeps one, or not.

use licash_api::{
    inbox::{
        self, Acknowledge, Acknowledged, Acks, AskAcks, Entry, Feed, Keep, Receipt, Take, Taken,
    },
    Hex, Problem,
};

/// How a hub keeps payments asked kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Conduct {
    /// As the hub service: hands each over to its payee, tells its ack.
    #[default]
    Keeps,
    /// Keeps none and serves no inbox, as an older hub.
    None,
    /// Answers that it keeps them, then never hands them over.
    Withholds,
    /// Tells an ack of its own of every payment, which it cannot know.
    Forges,
    /// Answers every entry it keeps whatever it is asked after, and that
    /// there are more: an inbox that never moves on.
    Repeats,
    /// Answers a full page of entries no one can take after any entry, and
    /// that there are more: an inbox without end.
    Endless,
}

struct Kept {
    seq: u64,
    proposal: [u8; 32],
    ephemeral: Vec<u8>,
    tag: Vec<u8>,
    key: [u8; 32],
    commitment: [u8; 32],
    ack: Option<[u8; 32]>,
    receipt: Receipt,
}

#[derive(Default)]
pub struct Inbox {
    pub conduct: Conduct,
    kept: Vec<Kept>,
}

impl Inbox {
    /// Keep `receipt` of proposal `proposal`, of ephemeral key `ephemeral`,
    /// as `keep` asks: whether it answers kept.
    pub fn keep(
        &mut self,
        proposal: [u8; 32],
        ephemeral: Vec<u8>,
        keep: &Keep,
        receipt: Receipt,
    ) -> Result<bool, Problem> {
        let (tag, key, commitment) = keep.decode()?;
        match self.conduct {
            Conduct::None => return Ok(false),
            Conduct::Withholds => return Ok(true),
            Conduct::Keeps | Conduct::Forges | Conduct::Repeats | Conduct::Endless => {}
        }
        if self.kept.iter().any(|k| k.proposal == proposal) {
            return Ok(true);
        }
        let seq = self.kept.last().map_or(1000, |k| k.seq + 1);
        self.kept.push(Kept {
            seq,
            proposal,
            ephemeral,
            tag: tag.to_vec(),
            key,
            commitment,
            ack: None,
            receipt,
        });
        Ok(true)
    }

    /// Its answer at `path`, with `body`: none if it serves no inbox.
    pub fn serve(&mut self, path: &str, body: Option<&[u8]>) -> Option<Result<Vec<u8>, Problem>> {
        if self.conduct == Conduct::None {
            return None;
        }
        let json = |e: serde_json::Error| Problem::invalid(e.to_string());
        let answer = match (path, body) {
            (path, None) => {
                let after = path.strip_prefix("/v1/inbox?after=")?;
                let after: u64 = after.parse().map_err(|_| Problem::invalid("after")).ok()?;
                if self.conduct == Conduct::Endless {
                    let entries = (1..=inbox::MAX_ENTRIES as u64)
                        .map(|n| Entry {
                            seq: after + n,
                            ephemeral: Hex(vec![2; 33]),
                            tag: Hex(vec![0; inbox::TAG]),
                        })
                        .collect();
                    let feed = Feed {
                        entries,
                        more: true,
                    };
                    return Some(Ok(serde_json::to_vec(&feed).unwrap()));
                }
                let repeats = self.conduct == Conduct::Repeats;
                let entries: Vec<Entry> = self
                    .kept
                    .iter()
                    .filter(|k| repeats || k.seq > after)
                    .map(|k| Entry {
                        seq: k.seq,
                        ephemeral: Hex(k.ephemeral.clone()),
                        tag: Hex(k.tag.clone()),
                    })
                    .collect();
                let more = repeats || entries.len() > inbox::MAX_ENTRIES;
                let entries = entries.into_iter().take(inbox::MAX_ENTRIES).collect();
                Ok(serde_json::to_vec(&Feed { entries, more }).unwrap())
            }
            (inbox::TAKE, Some(body)) => (|| {
                let take: Take = serde_json::from_slice(body).map_err(json)?;
                let keys = inbox::arrays::<32>(take.payments.iter().map(|p| &p.key), "key")?;
                let payments = take
                    .payments
                    .iter()
                    .zip(keys)
                    .map(|(asked, key)| {
                        let found = self
                            .kept
                            .iter()
                            .find(|k| k.seq == asked.seq && k.key == key);
                        found.map(|k| k.receipt.clone())
                    })
                    .collect();
                Ok(serde_json::to_vec(&Taken { payments }).unwrap())
            })(),
            (inbox::ACK, Some(body)) => (|| {
                let told: Acknowledge = serde_json::from_slice(body).map_err(json)?;
                let keys = inbox::arrays::<32>(told.acks.iter().map(|a| &a.key), "key")?;
                let acks = inbox::arrays::<32>(told.acks.iter().map(|a| &a.ack), "ack")?;
                let acked = keys
                    .into_iter()
                    .zip(acks)
                    .map(|(key, ack)| {
                        let commitment = inbox::commitment(&ack);
                        let found = self.kept.iter_mut().find(|k| k.key == key);
                        match found {
                            Some(k) if k.commitment == commitment => {
                                k.ack = Some(ack);
                                true
                            }
                            _ => false,
                        }
                    })
                    .collect();
                Ok(serde_json::to_vec(&Acknowledged { acked }).unwrap())
            })(),
            (inbox::ACKS, Some(body)) => (|| {
                let asked: AskAcks = serde_json::from_slice(body).map_err(json)?;
                let keys = inbox::arrays::<32>(asked.keys.iter(), "key")?;
                let forges = self.conduct == Conduct::Forges;
                let acks = keys
                    .into_iter()
                    .map(|key| match forges {
                        true => Some(Hex(vec![0xaa; 32])),
                        false => {
                            let found = self.kept.iter().find(|k| k.key == key);
                            found.and_then(|k| k.ack).map(|a| Hex(a.to_vec()))
                        }
                    })
                    .collect();
                Ok(serde_json::to_vec(&Acks { acks }).unwrap())
            })(),
            _ => return None,
        };
        Some(answer)
    }

    /// How many payments it keeps, and of those how many acked.
    #[allow(dead_code)]
    pub fn counts(&self) -> (usize, usize) {
        let acked = self.kept.iter().filter(|k| k.ack.is_some()).count();
        (self.kept.len(), acked)
    }
}
