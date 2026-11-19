//! The Hub's statement guest (`licash_hp::check`): "the claim the
//! holder's publication carries is owed at most the response's `b′`". Its
//! input, read raw: `HubInput::to_bytes`. Its journal
//! (`licash_statement::Contract::journal`), fixed at the contract's
//! opening: `c ‖ H("licash/hp/word" ‖ Base digest) ‖ H(O_D ‖ checkpoint ‖ W)`.

use licash_hp::{check_with, HubInput, Program};
use risc0_zkvm::guest::env;
use risc0_zkvm::sha::Digest;

/// The image id of the chain program whose receipts the statement of a
/// chained contract (graph id 11 on) verifies: compiled in, and kept in this
/// directory so that a build from the same commit has the same one
/// (`r0/build-guest.sh chain` fails if the chain it builds is another).
const CHAIN_ID: &str = include_str!("../chain.id");

fn chain_id() -> [u8; 32] {
    let hex = CHAIN_ID.trim();
    let mut id = [0u8; 32];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("the chain program's id");
    }
    id
}

fn main() {
    let n: u32 = env::read();
    let mut words = vec![0u32; (n as usize).div_ceil(4)];
    env::read_slice(&mut words);
    let bytes: Vec<u8> = words
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .take(n as usize)
        .collect();
    // The `profile` build (never a kept program) prints the cycles each
    // step took, which the host shows: `docs/r0.md`.
    #[cfg(feature = "profile")]
    let mut last = env::cycle_count();
    #[cfg(feature = "profile")]
    let mut mark = |what: &'static str| {
        let now = env::cycle_count();
        println!("cycles {:>12}  {what}", now - last);
        last = now;
    };
    #[cfg(not(feature = "profile"))]
    let mut mark = |_: &'static str| {};
    let input = HubInput::from_bytes(&bytes).expect("the statement's input");
    mark("the input decoded: Base, notes and spends of the evidence verified");
    // A chained contract's statement verifies a receipt of the chain program
    // (the zkVM resolves it when this is proven); no other contract has one.
    let verify = |id: &[u8; 32], journal: &[u8]| {
        env::verify(Digest::from_bytes(*id), journal).expect("the chain's receipt")
    };
    let program = input.chain.as_ref().map(|_| Program {
        id: chain_id(),
        verify: &verify,
    });
    let journal = check_with(&input, program.as_ref(), &mut mark).expect("the Hub's statement");
    env::commit_slice(&journal);
}
