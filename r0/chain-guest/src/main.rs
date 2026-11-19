//! The chain program: a batch of the hub's header chain. Its input, read
//! raw: `licash_chain::Batch::to_bytes`. Its journal: `licash_chain::Journal`,
//! which names this program's own image id (the batch's, given in its input,
//! which the statements that verify its receipts check is theirs).
//!
//! A batch that resumes a chain verifies the receipt of the journal it
//! extends, of this same program (`env::verify`): the zkVM resolves it when
//! the batch is proven, so no chain is taken on trust.

use licash_chain::{extend, Batch};
use risc0_zkvm::guest::env;
use risc0_zkvm::sha::Digest;

fn main() {
    let n: u32 = env::read();
    let mut words = vec![0u32; (n as usize).div_ceil(4)];
    env::read_slice(&mut words);
    let bytes: Vec<u8> = words
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .take(n as usize)
        .collect();
    let batch = Batch::from_bytes(&bytes).expect("the batch's input");
    let verify = |id: &[u8; 32], journal: &[u8]| {
        env::verify(Digest::from_bytes(*id), journal).expect("the previous batch's receipt")
    };
    let journal = extend(&batch, &verify).expect("the chain's batch");
    env::commit_slice(&journal.to_bytes());
}
