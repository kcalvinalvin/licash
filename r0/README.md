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

## Programs kept

Contracts open under the programs `apps/contracts` names (`SIGNET` in
`apps/contracts/src/lib.rs`), each kept in `r0/programs/` as
`<image id>.bin`: a contract opened under one is proven with it alone, so
none named there is ever removed. Each file's image id is its name:

```sh
for p in r0/programs/*.bin; do
  cargo run -q --locked --release --manifest-path r0/host/Cargo.toml -- id "$p"
done
```

| Network | Program | Image id | Built from |
|---|---|---|---|
| signet | `native-claim` (holder) | `8be30cb2a683e896d251c7c363dddf794dc8ddb9d502922199cfa4d2f73b1390` | c325330 |
| signet | `native-counter` (counter) | `25051741d3d6438bc7f78ebf074a4b93f30363729564345bb800efd43ce21129` | c325330 |

Signet's are a trial under RISC Zero's shared Groth16 setup, and the
programs check signet's headers by their work alone, not by the network's
signature of each block: headers that fool them cost little to make.

Rebuilt, a program's file may differ from the one kept in its symbols'
names, which its image id does not cover (they vary from build to build):
compare image ids, not files.

To name new ones, build them from a commit, rebuild them in another
checkout to see the same image ids, copy each `r0/guests/<name>.bin` to
`r0/programs/<image id>.bin`, and name them in `apps/contracts`, keeping
the programs named before for the contracts opened under them.
Archived binaries alone do not provide prior-holder-image verification in
the wallet: that support and a genuine prior-image proof regression must
land before changing the holder image or receipt-verifier parameters.

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
