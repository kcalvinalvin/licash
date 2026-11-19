//! The Hub's statement guest (`licash_hp::check`): "the claim the
//! holder's publication carries is owed at most the response's `b′`". Its
//! input, read raw: `HubInput::to_bytes`. Its journal
//! (`licash_statement::Contract::journal`), fixed at the contract's
//! opening: `c ‖ H("licash/hp/word" ‖ Base digest) ‖ H(O_D ‖ checkpoint ‖ W)`.

use licash_hp::{check, HubInput};
use risc0_zkvm::guest::env;

fn main() {
    let n: u32 = env::read();
    let mut words = vec![0u32; (n as usize).div_ceil(4)];
    env::read_slice(&mut words);
    let bytes: Vec<u8> = words
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .take(n as usize)
        .collect();
    let input = HubInput::from_bytes(&bytes).expect("the statement's input");
    let journal = check(&input).expect("the Hub's statement");
    env::commit_slice(&journal);
}
