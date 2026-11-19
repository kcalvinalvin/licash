//! The hub's header chain, proven once: the chain program.
//!
//! A dispute that shows every header from its contract's checkpoint costs
//! cycles with the contract's age. Here a chain program proves the headers
//! beforehand, in batches ([`extend`]), each verifying the receipt of the one
//! before, and commits to them ([`Journal`]): a Merkle tree with one [`Leaf`] a
//! block, its height, its hash, the full state a header is checked against
//! ([`licash::chain::Checkpoint::commitment`]) and the work since the chain's
//! start. A statement that reads Bitcoin through that receipt (`licash-hp`)
//! opens ([`Opening`]) the few blocks it needs.
//!
//! This crate is the whole of the program and of what the hub does to keep and
//! open the chain ([`Lineage`]), and nothing else: the program's image id is
//! compiled into the statements that verify its receipts, so it changes only
//! when this changes, and a change here is a new chain.
//!
//! **Why the chain may start anywhere.** The first batch takes its start
//! block as given, and nothing authenticates it. None need: the statement
//! opens the leaf at its contract's checkpoint and requires the state it
//! commits to be the one its Base binds. Every later leaf is that state
//! after a header [`licash::chain::Checkpoint::push`] accepted, as a chain
//! checked from the contract's checkpoint would reach: so what is proven of
//! it is what [`licash::chain::Chain::verify`] proves from the checkpoint,
//! whichever the start. One chain then serves every contract the hub opens
//! on it.
//!
//! The tree is of fixed depth with leaves and nodes hashed apart, and its
//! frontier is committed in each journal, so that the next batch extends the
//! tree the last one proved and no other.

use licash::chain::{sha256d, Checkpoint};
use licash::crypto::tagged_hash;
use licash::Error;
use licash_statement::codec::{R, W};
use licash_statement::{btc::work, Reject};

pub type Hash = [u8; 32];

/// Leaves a tree holds: 2^32, more than blocks there will be.
pub const DEPTH: usize = 32;

/// The length of a chain program's journal.
pub const JOURNAL_LEN: usize = 32 + 4 + 4 + 32 + 32 + 32 + 16;

fn bad(reason: &str) -> Error {
    Error::Protocol(reason.into())
}

fn node(left: &Hash, right: &Hash) -> Hash {
    tagged_hash("licash/chain/node", &[left, right])
}

/// What an empty subtree of each height hashes to.
fn empties() -> [Hash; DEPTH + 1] {
    let mut out = [[0u8; 32]; DEPTH + 1];
    out[0] = tagged_hash("licash/chain/empty", &[]);
    for level in 0..DEPTH {
        out[level + 1] = node(&out[level], &out[level]);
    }
    out
}

/// What the tree commits to of a block of the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub height: u32,
    /// The block's hash, internal order.
    pub hash: Hash,
    /// [`Checkpoint::commitment`] of the state after the block.
    pub state: Hash,
    /// The work of the blocks after the chain's start, through this one.
    pub work: u128,
}

impl Leaf {
    pub fn digest(&self) -> Hash {
        tagged_hash(
            "licash/chain/leaf",
            &[
                &self.height.to_be_bytes(),
                &self.hash,
                &self.state,
                &self.work.to_be_bytes(),
            ],
        )
    }
}

/// An incremental Merkle tree: its leaves' count and the roots of the
/// complete subtrees on its left edge (its frontier).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    count: u32,
    filled: [Hash; DEPTH],
}

impl Default for Tree {
    fn default() -> Self {
        Self {
            count: 0,
            filled: [[0; 32]; DEPTH],
        }
    }
}

impl Tree {
    pub fn count(&self) -> u32 {
        self.count
    }

    pub fn push(&mut self, leaf: Hash) -> Result<(), Error> {
        let count = self.count.checked_add(1).ok_or_else(|| bad("full tree"))?;
        let (mut hash, mut size) = (leaf, count);
        for level in 0..DEPTH {
            if size & 1 == 1 {
                self.filled[level] = hash;
                self.count = count;
                return Ok(());
            }
            hash = node(&self.filled[level], &hash);
            size >>= 1;
        }
        Err(bad("full tree"))
    }

    pub fn root(&self) -> Hash {
        let empty = empties();
        let (mut hash, mut size) = (empty[0], self.count);
        for (filled, empty) in self.filled.iter().zip(&empty) {
            hash = if size & 1 == 1 {
                node(filled, &hash)
            } else {
                node(&hash, empty)
            };
            size >>= 1;
        }
        hash
    }

    /// The roots of the subtrees the next leaf joins: the frontier as a
    /// tree of `count` leaves has it.
    fn live(&self) -> impl Iterator<Item = &Hash> {
        let count = self.count;
        self.filled
            .iter()
            .enumerate()
            .filter(move |(level, _)| count >> level & 1 == 1)
            .map(|(_, hash)| hash)
    }

    /// What a journal commits to of the tree, beside its root: its count and
    /// frontier. A tree is rebuilt only from what hashes to it.
    pub fn digest(&self) -> Hash {
        let mut parts: Vec<&[u8]> = Vec::new();
        let count = self.count.to_be_bytes();
        parts.push(&count);
        parts.extend(self.live().map(|h| &h[..]));
        tagged_hash("licash/chain/frontier", &parts)
    }

    pub fn write(&self, w: &mut W) {
        w.put(&self.count.to_be_bytes());
        for hash in self.live() {
            w.put(hash);
        }
    }

    pub fn read(r: &mut R) -> Result<Self, Error> {
        let count = u32::from_be_bytes(r.take().map_err(|_| bad("input"))?);
        let mut tree = Tree {
            count,
            ..Tree::default()
        };
        for level in 0..DEPTH {
            if count >> level & 1 == 1 {
                tree.filled[level] = r.take().map_err(|_| bad("input"))?;
            }
        }
        Ok(tree)
    }
}

/// The siblings from leaf `index` up to the root of the tree of `leaves`
/// (their digests).
pub fn path(leaves: &[Hash], index: usize) -> Vec<Hash> {
    let empty = empties();
    let mut level = leaves.to_vec();
    let (mut at, mut siblings) = (index, Vec::with_capacity(DEPTH));
    for empty in &empty[..DEPTH] {
        siblings.push(*level.get(at ^ 1).unwrap_or(empty));
        level = level
            .chunks(2)
            .map(|pair| node(&pair[0], pair.get(1).unwrap_or(empty)))
            .collect();
        at >>= 1;
    }
    siblings
}

/// The root a leaf at `index` reaches through `siblings`.
pub fn root_of(leaf: &Hash, index: u32, siblings: &[Hash]) -> Option<Hash> {
    if siblings.len() != DEPTH {
        return None;
    }
    let (mut hash, mut at) = (*leaf, index);
    for sibling in siblings {
        hash = if at & 1 == 0 {
            node(&hash, sibling)
        } else {
            node(sibling, &hash)
        };
        at >>= 1;
    }
    Some(hash)
}

/// What a batch of the chain program commits: the chain so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Journal {
    /// The chain program's own image id: a batch verifies receipts of this
    /// program alone.
    pub chain: Hash,
    /// The height of the chain's first block (leaf 0).
    pub start: u32,
    /// Its blocks, the start's included.
    pub count: u32,
    pub root: Hash,
    /// [`Tree::digest`]: what the next batch takes its tree from.
    pub frontier: Hash,
    /// [`Checkpoint::commitment`] of the state after the last block.
    pub tip: Hash,
    /// The work of the blocks after the start.
    pub work: u128,
}

impl Journal {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = W::default();
        w.put(&self.chain);
        w.put(&self.start.to_be_bytes());
        w.put(&self.count.to_be_bytes());
        w.put(&self.root);
        w.put(&self.frontier);
        w.put(&self.tip);
        w.put(&self.work.to_be_bytes());
        w.0
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != JOURNAL_LEN {
            return None;
        }
        let mut r = R(bytes);
        let journal = Self {
            chain: r.take().ok()?,
            start: u32::from_be_bytes(r.take().ok()?),
            count: u32::from_be_bytes(r.take().ok()?),
            root: r.take().ok()?,
            frontier: r.take().ok()?,
            tip: r.take().ok()?,
            work: u128::from_be_bytes(r.take().ok()?),
        };
        // a chain has its start, and its last block's height is a height
        let last = journal.start.checked_add(journal.count.checked_sub(1)?);
        last.map(|_| journal)
    }

    /// The height of the last block.
    pub fn tip_height(&self) -> u32 {
        self.start + (self.count - 1)
    }
}

/// A chain being extended: the state after its last block, its tree and its
/// work. The chain program's batch and the hub's own copy of it are this.
#[derive(Clone)]
pub struct Builder {
    state: Checkpoint,
    tree: Tree,
    start: u32,
    work: u128,
}

impl Builder {
    /// A chain of one block, `start`, which nothing authenticates here.
    pub fn start(start: Checkpoint) -> Result<Self, Error> {
        let mut tree = Tree::default();
        let height = start.height();
        tree.push(
            Leaf {
                height,
                hash: sha256d(start.header()),
                state: start.commitment(),
                work: 0,
            }
            .digest(),
        )?;
        Ok(Self {
            state: start,
            tree,
            start: height,
            work: 0,
        })
    }

    /// The chain `journal` commits, `tip` its last block's state and `tree`
    /// its tree, each as that journal binds it.
    pub fn resume(journal: &Journal, tip: Checkpoint, tree: Tree) -> Result<Self, Error> {
        if tip.commitment() != journal.tip
            || tree.digest() != journal.frontier
            || tree.root() != journal.root
            || tree.count() != journal.count
            || journal.count == 0
            || u64::from(journal.start) + u64::from(journal.count - 1) != u64::from(tip.height())
        {
            return Err(bad("the chain's state is not the journal's"));
        }
        Ok(Self {
            state: tip,
            tree,
            start: journal.start,
            work: journal.work,
        })
    }

    /// The next block of the chain: checked as a header of a chain from a
    /// checkpoint is, and added. Its leaf. On error the chain is unchanged.
    pub fn push(&mut self, header: [u8; 80]) -> Result<Leaf, Error> {
        let mut state = self.state.clone();
        state.push(header)?;
        let work = self
            .work
            .checked_add(work(u32::from_le_bytes(
                header[72..76].try_into().expect("four bytes"),
            )))
            .ok_or_else(|| bad("work overflows"))?;
        let leaf = Leaf {
            height: state.height(),
            hash: sha256d(&header),
            state: state.commitment(),
            work,
        };
        let mut tree = self.tree.clone();
        tree.push(leaf.digest())?;
        (self.state, self.tree, self.work) = (state, tree, work);
        Ok(leaf)
    }

    pub fn journal(&self, chain: Hash) -> Journal {
        Journal {
            chain,
            start: self.start,
            count: self.tree.count(),
            root: self.tree.root(),
            frontier: self.tree.digest(),
            tip: self.state.commitment(),
            work: self.work,
        }
    }

    pub fn tip(&self) -> &Checkpoint {
        &self.state
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }
}

/// Where a batch of the chain program starts.
// One is made a batch and dropped: nothing holds many of them.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum Origin {
    /// A chain of one block, the start, which nothing authenticates.
    Start(Checkpoint),
    /// The chain a journal commits: the state of its last block and its tree,
    /// each as that journal binds it. The batch verifies that journal's receipt.
    Resume {
        journal: Journal,
        tip: Checkpoint,
        tree: Tree,
    },
}

/// The chain program's input: its own image id (which a program cannot hold
/// as a constant of itself), where the batch starts and the headers it adds.
#[derive(Clone)]
pub struct Batch {
    pub chain: Hash,
    pub from: Origin,
    pub headers: Vec<[u8; 80]>,
}

impl Batch {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = W::default();
        w.put(&self.chain);
        match &self.from {
            Origin::Start(start) => {
                w.put(&[1]);
                w.bytes(&start.to_bytes());
            }
            Origin::Resume { journal, tip, tree } => {
                w.put(&[0]);
                w.bytes(&journal.to_bytes());
                w.bytes(&tip.to_bytes());
                tree.write(&mut w);
            }
        }
        w.list(&self.headers);
        w.0
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let input = |_| bad("input");
        let mut r = R(bytes);
        let chain = r.take().map_err(input)?;
        let from = match r.take::<1>().map_err(input)? {
            [1] => Origin::Start(
                Checkpoint::from_bytes(&r.bytes().map_err(input)?).map_err(|_| bad("input"))?,
            ),
            [0] => Origin::Resume {
                journal: Journal::from_bytes(&r.bytes().map_err(input)?)
                    .ok_or_else(|| bad("input"))?,
                tip: Checkpoint::from_bytes(&r.bytes().map_err(input)?)
                    .map_err(|_| bad("input"))?,
                tree: Tree::read(&mut r)?,
            },
            _ => return Err(bad("input")),
        };
        let headers = r.list().map_err(input)?;
        r.end().map_err(input)?;
        Ok(Self {
            chain,
            from,
            headers,
        })
    }
}

/// The chain program: its batch's journal. A batch that resumes a chain
/// verifies, through `verify` (the zkVM's `env::verify`: the receipt of this
/// same program, by its image id `batch.chain`, of that journal), the
/// receipt that proves the chain so far; it takes no chain on trust.
pub fn extend(batch: &Batch, verify: &dyn Fn(&Hash, &[u8])) -> Result<Journal, Error> {
    let mut chain = match &batch.from {
        Origin::Start(start) => Builder::start(start.clone())?,
        Origin::Resume { journal, tip, tree } => {
            if journal.chain != batch.chain {
                return Err(bad("the receipt is of another program"));
            }
            verify(&batch.chain, &journal.to_bytes());
            Builder::resume(journal, tip.clone(), tree.clone())?
        }
    };
    for header in &batch.headers {
        chain.push(*header)?;
    }
    Ok(chain.journal(batch.chain))
}

/// A block of the chain, opened: what its leaf commits (but its hash, which
/// its header gives) and its siblings to the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opening {
    pub height: u32,
    pub state: Hash,
    pub work: u128,
    pub header: [u8; 80],
    pub path: Vec<Hash>,
}

impl Opening {
    pub fn leaf(&self) -> Leaf {
        Leaf {
            height: self.height,
            hash: sha256d(&self.header),
            state: self.state,
            work: self.work,
        }
    }

    pub fn write(&self, w: &mut W) {
        w.put(&self.height.to_be_bytes());
        w.put(&self.state);
        w.put(&self.work.to_be_bytes());
        w.put(&self.header);
        for sibling in &self.path {
            w.put(sibling);
        }
    }

    pub fn read(r: &mut R) -> Result<Self, Error> {
        let take = |_: Reject| bad("input");
        Ok(Self {
            height: u32::from_be_bytes(r.take().map_err(take)?),
            state: r.take().map_err(take)?,
            work: u128::from_be_bytes(r.take().map_err(take)?),
            header: r.take().map_err(take)?,
            path: (0..DEPTH)
                .map(|_| r.take().map_err(take))
                .collect::<Result<_, _>>()?,
        })
    }
}

/// A chain kept whole, from which the batches are proven and the openings
/// made: what the hub holds of it, and tests.
#[derive(Clone)]
pub struct Lineage {
    start: Checkpoint,
    headers: Vec<[u8; 80]>,
    leaves: Vec<Leaf>,
    builder: Builder,
}

impl Lineage {
    pub fn new(start: Checkpoint) -> Result<Self, Error> {
        let builder = Builder::start(start.clone())?;
        let leaves = vec![Leaf {
            height: start.height(),
            hash: sha256d(start.header()),
            state: start.commitment(),
            work: 0,
        }];
        Ok(Self {
            start,
            headers: Vec::new(),
            leaves,
            builder,
        })
    }

    pub fn push(&mut self, header: [u8; 80]) -> Result<(), Error> {
        let leaf = self.builder.push(header)?;
        self.headers.push(header);
        self.leaves.push(leaf);
        Ok(())
    }

    pub fn start(&self) -> &Checkpoint {
        &self.start
    }

    pub fn tip_height(&self) -> u32 {
        self.start.height() + self.headers.len() as u32
    }

    /// The header of `height`, if it is on the chain.
    pub fn header(&self, height: u32) -> Option<&[u8; 80]> {
        let at = height.checked_sub(self.start.height())? as usize;
        match at {
            0 => Some(self.start.header()),
            _ => self.headers.get(at - 1),
        }
    }

    /// The journal of the chain through `height`, and the state of its last
    /// block, by replaying it: what a batch ending there commits.
    pub fn through(&self, chain: Hash, height: u32) -> Result<(Journal, Checkpoint), Error> {
        let n = height
            .checked_sub(self.start.height())
            .filter(|n| *n as usize <= self.headers.len())
            .ok_or_else(|| bad("a height beyond the chain"))? as usize;
        let mut builder = Builder::start(self.start.clone())?;
        for header in &self.headers[..n] {
            builder.push(*header)?;
        }
        Ok((builder.journal(chain), builder.tip().clone()))
    }

    /// The leaf of the block at `height`, if it is on the chain.
    pub fn leaf(&self, height: u32) -> Option<&Leaf> {
        self.leaves
            .get(height.checked_sub(self.start.height())? as usize)
    }

    /// The headers after the chain's start from height `from` through `to`,
    /// as many as it has.
    pub fn headers(&self, from: u32, to: u32) -> &[[u8; 80]] {
        let first = from.max(self.start.height() + 1) - self.start.height() - 1;
        let end = to.saturating_sub(self.start.height());
        self.headers
            .get(first as usize..(end as usize).min(self.headers.len()))
            .unwrap_or_default()
    }

    /// The chain through `height` alone: what it was then, as a reorg back to
    /// that block leaves it. By replaying it.
    pub fn truncated(&self, height: u32) -> Result<Self, Error> {
        let n = height
            .checked_sub(self.start.height())
            .filter(|n| *n as usize <= self.headers.len())
            .ok_or_else(|| bad("a height beyond the chain"))?;
        let mut chain = Self::new(self.start.clone())?;
        for header in &self.headers[..n as usize] {
            chain.push(*header)?;
        }
        Ok(chain)
    }

    /// The height of the block of `header`, if it is on the chain.
    pub fn height_of(&self, header: &[u8; 80]) -> Option<u32> {
        let at = match self.start.header() == header {
            true => 0,
            false => self.headers.iter().position(|h| h == header)? + 1,
        };
        Some(self.start.height() + at as u32)
    }

    /// The state of the last block and the tree: what the next batch takes.
    pub fn builder(&self) -> &Builder {
        &self.builder
    }

    /// An opening of `height` in the tree of the first `count` blocks (of
    /// the journal a receipt has).
    pub fn open(&self, height: u32, count: u32) -> Option<Opening> {
        let at = height.checked_sub(self.start.height())? as usize;
        if at >= count as usize || count as usize > self.leaves.len() {
            return None;
        }
        let digests: Vec<_> = self.leaves[..count as usize]
            .iter()
            .map(Leaf::digest)
            .collect();
        let leaf = self.leaves[at];
        Some(Opening {
            height,
            state: leaf.state,
            work: leaf.work,
            header: *self.header(height)?,
            path: path(&digests, at),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use licash::spend::ChainProfile;
    use licash_statement::testing::headers_on;

    const CHAIN: Hash = [0xc1; 32];

    fn genesis() -> Checkpoint {
        Checkpoint::genesis(ChainProfile::Core30DefaultRegtest).unwrap()
    }

    fn lineage(n: usize) -> Lineage {
        let g = genesis();
        let mut lineage = Lineage::new(g.clone()).unwrap();
        for header in headers_on(&sha256d(g.header()), n) {
            lineage.push(header).unwrap();
        }
        lineage
    }

    #[test]
    fn the_incremental_tree_is_the_tree_of_its_leaves() {
        let mut tree = Tree::default();
        let mut leaves = Vec::new();
        assert_eq!(tree.root(), empties()[DEPTH]);
        for i in 0u32..70 {
            let leaf = tagged_hash("test", &[&i.to_be_bytes()]);
            tree.push(leaf).unwrap();
            leaves.push(leaf);
            for at in 0..leaves.len() {
                let siblings = path(&leaves, at);
                assert_eq!(siblings.len(), DEPTH);
                assert_eq!(
                    root_of(&leaves[at], at as u32, &siblings),
                    Some(tree.root()),
                    "{at} of {}",
                    leaves.len()
                );
            }
            // a leaf of another place, or another leaf, reaches another root
            let siblings = path(&leaves, 0);
            assert_ne!(
                root_of(&leaves[0], 1, &siblings),
                Some(tree.root()),
                "wrong index"
            );
        }
        assert!(root_of(&leaves[0], 0, &[]).is_none());
        // what a tree writes it reads back, and its digest is what it commits
        let mut w = W::default();
        tree.write(&mut w);
        let back = Tree::read(&mut R(&w.0)).unwrap();
        assert_eq!((back.root(), back.digest()), (tree.root(), tree.digest()));
        let mut more = back;
        more.push([1; 32]).unwrap();
        tree.push([1; 32]).unwrap();
        assert_eq!(more.root(), tree.root());
    }

    #[test]
    fn a_batch_extends_the_tree_its_journal_commits_and_no_other() {
        let all = lineage(12);
        let (journal, tip) = all.through(CHAIN, 5).unwrap();
        let mut early = Builder::start(genesis()).unwrap();
        for height in 1..=5 {
            early.push(*all.header(height).unwrap()).unwrap();
        }
        assert_eq!(early.journal(CHAIN), journal);
        let tree = early.tree().clone();
        // resumed, it is the chain extended at once
        let mut resumed = Builder::resume(&journal, tip.clone(), tree.clone()).unwrap();
        for height in 6..=12 {
            resumed.push(*all.header(height).unwrap()).unwrap();
        }
        assert_eq!(resumed.journal(CHAIN), all.builder().journal(CHAIN));
        // a frontier, a count, a tip or a tree that is not the journal's is refused
        let mut forged = tree.clone();
        forged.filled[0][0] ^= 1;
        forged.filled[2][0] ^= 1;
        assert!(Builder::resume(&journal, tip.clone(), forged).is_err());
        let mut other = tree.clone();
        other.push([9; 32]).unwrap();
        assert!(Builder::resume(&journal, tip.clone(), other).is_err());
        let (_, later) = all.through(CHAIN, 6).unwrap();
        assert!(Builder::resume(&journal, later, tree.clone()).is_err());
        let mut moved = journal.clone();
        moved.start += 1;
        assert!(Builder::resume(&moved, tip, tree).is_err());
    }

    #[test]
    fn a_batch_is_what_the_hub_keeps_of_the_chain_and_travels_as_bytes() {
        use std::cell::RefCell;
        let all = lineage(12);
        let bytes: Vec<_> = (1..=12).map(|h| *all.header(h).unwrap()).collect();
        let asked = RefCell::new(Vec::new());
        let verify = |id: &Hash, journal: &[u8]| asked.borrow_mut().push((*id, journal.to_vec()));
        // the first batch, then the next, each the chain through its headers
        let first = Batch {
            chain: CHAIN,
            from: Origin::Start(genesis()),
            headers: bytes[..5].to_vec(),
        };
        let back = Batch::from_bytes(&first.to_bytes()).unwrap();
        let journal = extend(&back, &verify).unwrap();
        assert_eq!(journal, all.through(CHAIN, 5).unwrap().0);
        assert!(
            asked.borrow().is_empty(),
            "the first batch verifies nothing"
        );
        let (_, tip) = all.through(CHAIN, 5).unwrap();
        let mut early = Builder::start(genesis()).unwrap();
        for header in &bytes[..5] {
            early.push(*header).unwrap();
        }
        let next = Batch {
            chain: CHAIN,
            from: Origin::Resume {
                journal: journal.clone(),
                tip,
                tree: early.tree().clone(),
            },
            headers: bytes[5..].to_vec(),
        };
        let back = Batch::from_bytes(&next.to_bytes()).unwrap();
        assert_eq!(
            extend(&back, &verify).unwrap(),
            all.builder().journal(CHAIN)
        );
        // it verified the receipt of the journal it extends, of its own program
        assert_eq!(*asked.borrow(), [(CHAIN, journal.to_bytes())]);
        // a receipt of another program's journal is no chain to extend
        let mut other = next.clone();
        other.chain = [0xc2; 32];
        assert!(extend(&other, &verify).is_err());
        // a header that does not follow is no batch
        let mut off = next.clone();
        off.headers[3][4] ^= 1;
        assert!(extend(&off, &verify).is_err());
        // the encoding is strict
        let encoded = next.to_bytes();
        assert!(Batch::from_bytes(&encoded[..encoded.len() - 1]).is_err());
        assert!(Batch::from_bytes(&[encoded.clone(), vec![0]].concat()).is_err());
    }

    #[test]
    fn a_journal_of_no_chain_is_none() {
        let mut bytes = lineage(3).builder().journal(CHAIN).to_bytes();
        assert!(Journal::from_bytes(&bytes).is_some());
        // no blocks, and a last block past the heights there are
        bytes[36..40].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(Journal::from_bytes(&bytes), None);
        bytes[32..36].copy_from_slice(&u32::MAX.to_be_bytes());
        bytes[36..40].copy_from_slice(&5u32.to_be_bytes());
        assert_eq!(Journal::from_bytes(&bytes), None);
    }

    #[test]
    fn a_header_that_does_not_follow_is_refused_and_leaves_the_chain_as_it_was() {
        let mut all = lineage(4);
        let before = all.builder().journal(CHAIN);
        let mut off = *all.header(2).unwrap();
        off[4] ^= 1;
        assert!(all.push(off).is_err());
        assert!(all.push(*all.header(1).unwrap()).is_err());
        assert_eq!(all.builder().journal(CHAIN), before);
        assert_eq!(all.tip_height(), 4);
        // the journal is what it was and travels as bytes
        let bytes = before.to_bytes();
        assert_eq!(bytes.len(), JOURNAL_LEN);
        assert_eq!(Journal::from_bytes(&bytes), Some(before));
        assert_eq!(Journal::from_bytes(&bytes[1..]), None);
    }

    #[test]
    fn a_range_of_headers_and_a_leaf_are_those_of_their_heights() {
        let all = lineage(8);
        assert_eq!(all.headers(3, 5).len(), 3);
        assert_eq!(all.headers(3, 5)[0], *all.header(3).unwrap());
        assert_eq!(all.headers(3, 5)[2], *all.header(5).unwrap());
        // before the first, past the last, and empty
        assert_eq!(all.headers(0, 2).len(), 2);
        assert_eq!(all.headers(7, 99).len(), 2);
        assert!(all.headers(9, 99).is_empty());
        assert!(all.headers(5, 4).is_empty());
        assert_eq!(all.leaf(6).unwrap().hash, sha256d(all.header(6).unwrap()));
        assert_eq!(all.leaf(6).unwrap().height, 6);
        assert!(all.leaf(9).is_none());
    }

    #[test]
    fn a_chain_cut_back_is_the_chain_as_it_was() {
        let all = lineage(9);
        let cut = all.truncated(4).unwrap();
        let (journal, tip) = all.through(CHAIN, 4).unwrap();
        assert_eq!(cut.builder().journal(CHAIN), journal);
        assert_eq!(cut.builder().tip().commitment(), tip.commitment());
        assert_eq!(cut.tip_height(), 4);
        assert_eq!(
            all.truncated(9).unwrap().builder().journal(CHAIN),
            all.builder().journal(CHAIN)
        );
        assert_eq!(all.truncated(0).unwrap().tip_height(), 0);
        assert!(all.truncated(10).is_err());
    }

    #[test]
    fn work_and_heights_are_counted_from_the_start() {
        let all = lineage(6);
        let journal = all.builder().journal(CHAIN);
        assert_eq!(
            (journal.start, journal.count, journal.tip_height()),
            (0, 7, 6)
        );
        let per_block = licash_statement::btc::work(0x207f_ffff);
        assert_eq!(journal.work, 6 * per_block);
        for height in 0..=6 {
            let o = all.open(height, journal.count).unwrap();
            assert_eq!((o.height, o.work), (height, u128::from(height) * per_block));
            assert_eq!(
                root_of(&o.leaf().digest(), height, &o.path),
                Some(journal.root)
            );
        }
        // no opening beyond the receipt's count, nor of a block it has not
        assert!(all.open(6, 6).is_none());
        assert!(all.open(7, 7).is_none());
        // the tree of an earlier receipt's count, the later one's chain
        let (earlier, _) = all.through(CHAIN, 3).unwrap();
        let o = all.open(2, earlier.count).unwrap();
        assert_eq!(root_of(&o.leaf().digest(), 2, &o.path), Some(earlier.root));
        assert_eq!(all.height_of(all.header(4).unwrap()), Some(4));
        assert_eq!(all.height_of(&[7; 80]), None);
    }
}
