//! A chained contract's statement reads Bitcoin through a receipt of the
//! chain program (`licash-chain`): the receipt's journal, openings of the
//! blocks the exit and the judgment of the claim read, and the few headers
//! after the chain's tip.
//!
//! The contract's checkpoint is opened and must commit the state its Base
//! binds, so a chain that began anywhere proves what a chain checked from
//! that checkpoint would, and the statement takes nothing of it on trust but
//! the receipt's journal, which the zkVM verifies.

use std::collections::BTreeMap;

use licash::chain::{sha256d, Chain, Checkpoint};
use licash::Error;
use licash_statement::codec::{R, W};
use licash_statement::{btc::work, Blocks, Contract};

pub use licash_chain::{
    extend, path, root_of, Batch, Builder, Hash, Journal, Leaf, Lineage, Opening, Origin, Tree,
    DEPTH, JOURNAL_LEN,
};

use crate::Refusal;

/// What a statement of a chained contract is given of the chain: the chain
/// program's journal (the statement verifies its receipt), the state of its
/// last block, and openings of the blocks the judgment reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainProof {
    pub journal: Vec<u8>,
    pub tip: Vec<u8>,
    pub openings: Vec<Opening>,
}

impl ChainProof {
    pub fn write(&self, w: &mut W) {
        w.bytes(&self.journal);
        w.bytes(&self.tip);
        w.put(&(self.openings.len() as u32).to_be_bytes());
        for opening in &self.openings {
            opening.write(w);
        }
    }

    pub fn read(r: &mut R) -> Result<Self, Refusal> {
        let journal = r.bytes().map_err(|_| Refusal::Input)?;
        let tip = r.bytes().map_err(|_| Refusal::Input)?;
        let n = u32::from_be_bytes(r.take().map_err(|_| Refusal::Input)?) as usize;
        // an opening is at least as long as its siblings
        if n > r.0.len() / (DEPTH * 32) {
            return Err(Refusal::Input);
        }
        let openings = (0..n)
            .map(|_| Opening::read(r).map_err(|_| Refusal::Input))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            journal,
            tip,
            openings,
        })
    }
}

/// A chain read through its receipt's journal: the blocks opened, each
/// proven to be the block of its height on it, and the work after its last
/// block, which the statement checks itself ([`licash::chain::Checkpoint::push`]
/// from the journal's tip).
pub struct View {
    journal: Journal,
    checkpoint: u32,
    shown: BTreeMap<u32, Opening>,
    tail_work: u128,
}

impl View {
    /// `proof` for the contract whose Base binds `checkpoint`, with `tail`
    /// the headers after the journal's last block. Refused unless the
    /// journal is of the chain program `chain`, every opening is a leaf of
    /// its tree, the leaf at the contract's checkpoint commits the state
    /// Base binds, and the tail is a chain on the journal's tip.
    pub fn new(
        chain: &Hash,
        proof: &ChainProof,
        checkpoint: &Checkpoint,
        tail: &[[u8; 80]],
    ) -> Result<Self, Refusal> {
        let refuse = |reason: &str| Refusal::Chain(reason.into());
        let journal = Journal::from_bytes(&proof.journal)
            .filter(|j| &j.chain == chain && j.count > 0)
            .ok_or_else(|| refuse("not the chain program's journal"))?;
        let mut shown = BTreeMap::new();
        for opening in &proof.openings {
            let index = opening
                .height
                .checked_sub(journal.start)
                .filter(|i| *i < journal.count)
                .ok_or_else(|| refuse("an opening outside the chain"))?;
            let leaf = opening.leaf().digest();
            if root_of(&leaf, index, &opening.path) != Some(journal.root) {
                return Err(refuse("an opening is not in the chain's tree"));
            }
            shown.insert(opening.height, opening.clone());
        }
        match shown.get(&checkpoint.height()) {
            None => return Err(Refusal::Openings(vec![checkpoint.height()])),
            Some(o) if o.state != checkpoint.commitment() => {
                return Err(refuse("the chain does not pass through the checkpoint"))
            }
            Some(_) => {}
        }
        let mut tip = Checkpoint::from_bytes(&proof.tip)
            .map_err(|e| refuse(&format!("the tip's state: {e}")))?;
        if tip.commitment() != journal.tip || tip.height() != journal.tip_height() {
            return Err(refuse("the tip is not the journal's"));
        }
        let mut tail_work = 0u128;
        for header in tail {
            tip.push(*header)
                .map_err(|e| refuse(&format!("the tail: {e}")))?;
            let bits = u32::from_le_bytes(header[72..76].try_into().expect("four bytes"));
            tail_work = tail_work
                .checked_add(work(bits))
                .ok_or_else(|| refuse("the tail's work overflows"))?;
        }
        Ok(Self {
            journal,
            checkpoint: checkpoint.height(),
            shown,
            tail_work,
        })
    }

    /// The chain Base's checkpoint starts, through `tip`, with the blocks
    /// opened between them: what the claim's judgment looks blocks up in.
    pub fn chain(&self, checkpoint: &Checkpoint, tip: u32) -> Result<Chain, Error> {
        let shown = self
            .shown
            .iter()
            .filter(|(height, _)| **height > self.checkpoint && **height <= tip)
            .map(|(height, o)| (*height, o.header))
            .collect();
        Chain::sparse(checkpoint.clone(), tip, shown)
    }
}

impl Blocks for View {
    fn linked(&self, _: &Contract) -> bool {
        true
    }

    fn height_of(&self, _: &Contract, header: &[u8; 80]) -> Option<u32> {
        let hash = sha256d(header);
        self.shown
            .values()
            .find(|o| o.height > self.checkpoint && sha256d(&o.header) == hash)
            .map(|o| o.height)
    }

    fn work_above(&self, _: &Contract, height: u32) -> Option<u128> {
        let opening = self.shown.get(&height)?;
        self.journal
            .work
            .checked_sub(opening.work)?
            .checked_add(self.tail_work)
    }
}

/// What a receipt of the chain program proves, with the chain it is of: the
/// chain a hub keeps, the journal the receipt has and the state of its last
/// block, from which a statement's openings are made.
#[derive(Clone, Copy)]
pub struct Proven<'a> {
    pub lineage: &'a Lineage,
    pub journal: &'a Journal,
    pub tip: &'a Checkpoint,
}
