//! The native holder guest. Input: `licash::statement::StatementInput`
//! in its guest codec, framed as a length and raw words. Journal:
//! `c ‖ D ‖ H(pk_H ‖ O_D)` (68 bytes), or no proof when the selected
//! assertion does not hold. A Corrected counter's seal is verified against
//! Base's counter program, which both parties checked at the funding gates.

use licash::statement::StatementInput;
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
    let input = StatementInput::from_bytes(&bytes).expect("the statement's input");
    let counter = input.base.parts().recipe.counter_program;
    let (journal, _) = input
        .run(&mut |seal, public| licash_groth16::verify(&counter, seal, public))
        .expect("the selected assertion holds");
    env::commit_slice(&journal.to_bytes());
}
