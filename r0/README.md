# RISC Zero programs

- `native-guest/`: the holder's statement (`licash::statement::StatementInput`).
  It commits the 68-byte journal `c ‖ D ‖ H(pk_H ‖ O_D)` and verifies the
  Hub's counter proof against the counter program that Base names.
- `native-counter-guest/`: the Hub's counter (`licash::guest::CounterInput`).
  It commits the 104-byte journal `D ‖ E ‖ mask ‖ owed ‖ H(disclosure slots)`.
- `host/`: packs, executes and proves both. Every command also runs the
  library natively and fails when the guest disagrees with it.

Each is a workspace of its own.

## Prerequisites

[rzup](https://dev.risczero.com) with the `risc0` Rust toolchain (Rust
1.97), `cargo-risczero` and `r0vm` 3.1.x, and its RISC-V C toolchain (the
guests link libsecp256k1). `RISC0_TOOLCHAIN` and `RISC0_CC` override the default
paths, `~/.rustup/toolchains/risc0` and
`~/.risc0/cpp/bin/riscv32-unknown-elf-gcc`.

`r0/build-guest.sh` builds with the toolchains the kept programs were built
with alone, rzup's `rust` 1.97.0 (rustc `e638c6cfe`) and `cpp` 2024.1.5
(gcc 13.2.0), and the lockfiles as committed:

```sh
rzup install rust 1.97.0
rzup install cpp 2024.1.5
```

`LICASH_GUEST_ANY_TOOLCHAIN=1` builds with others, for a guest not to keep.

## Build

```sh
r0/build-guest.sh native-counter   # r0/guests/native-counter.{bin,id}
r0/build-guest.sh native-claim     # r0/guests/native-claim.{bin,id}
cargo build --release --manifest-path r0/host/Cargo.toml
```

Builds are local and untracked in `r0/guests/`; image ids change with the
source. A guest is built from a copy of its sources at `/tmp/licash-r0`,
whichever the checkout's directory (cargo hashes a path dependency outside
the guest's workspace by where it is), and cargo's directory is left out
of the paths it holds, so the same commit builds the same image id on any
machine. One build at a time uses that directory.

## Execute and prove

Export guest inputs from the regtest suite (see
[../regtest/README.md](../regtest/README.md)):

```sh
LICASH_NATIVE_INPUTS=$PWD/tmp/inputs regtest/run.sh --test native_lifecycle -- --test-threads=1
```

Then execute (journal and cycle count) or prove (succinct receipt, Groth16
wrap, verified and exported as JSON):

```sh
licash=r0/host/target/release/licash-r0
$licash native-exec tmp/inputs/native-honest-holder-c0.bin
$licash native-counter-exec tmp/inputs/native-stale-counter.bin
$licash native-prove tmp/inputs/native-honest-holder-c0.bin tmp/honest.json
$licash native-counter-prove tmp/inputs/native-stale-counter.bin tmp/counter.json
```

`LICASH_SEGMENT_PO2` (13 to 20) sets the zkVM segment size. Inputs exported
by the mock-proof suite carry mock counter seals, so the holder's program
refuses those that depend on a counter proof, as the library does.

The end-to-end test with real proofs plays both receipts on Bitcoin. It takes
hours and about 10 GB of memory, so it is run explicitly:

```sh
LICASH_NATIVE_GUESTS=$PWD/r0/guests LICASH_R0=$PWD/r0/host/target/release/licash-r0 \
  regtest/run.sh --test native_real_proof -- --ignored --nocapture
```
