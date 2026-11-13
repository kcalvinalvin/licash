//! The native counter guest: the Hub proves which claimed items its evidence
//! voids and the value still owed. Journal: `D ‖ E ‖ mask ‖ owed ‖ H(disclosure slots)` (104 bytes).

use licash::guest::CounterInput;
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
    let input = CounterInput::from_bytes(&bytes).expect("the counter input");
    let journal = input.run().expect("a valid counter");
    env::commit_slice(&journal.to_bytes());
}
