# RISC Zero programs

- `hp-guest/`: the Hub's statement (`licash_hp::check`): the claim the
  holder's publication carries is owed at most the Hub's response `b′`. It
  commits Lichen's 68-byte journal `c ‖ H("licash/hp/word" ‖ Base digest)
  ‖ H(O_D ‖ checkpoint ‖ W)`, fixed when the contract opens, so the holder
  garbles for it before anyone proves.
- `host/`: packs, executes and proves it. Every command also runs
  `licash_hp::check` natively and fails when the guest disagrees with it.

Each is a workspace of its own. The guest needs Lichen's statement crate in
`vendor/` (`vendor/fetch.sh`).

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
r0/build-guest.sh hp   # r0/guests/hp.{bin,id}
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

Export statement inputs (`licash_hp::HubInput::to_bytes`) from the
statement's tests: a 32-item claim the Hub answers with a 16-note spend,
and the same claim with a response it can't prove:

```sh
LICASH_HP_INPUTS=$PWD/tmp/hp-inputs cargo test --release \
  --manifest-path regtest/Cargo.toml -p licash-hp export_inputs
```

Then execute (journal and cycle count) or prove (succinct receipt, Groth16
wrap, verified and exported as JSON with the 256-byte seal that
`lichen_babe::risc0::proof_from_seal` reads):

```sh
licash=r0/host/target/release/licash-r0
$licash hp-exec tmp/hp-inputs/answered.bin
$licash hp-prove tmp/hp-inputs/answered.bin tmp/answered.json
```

`LICASH_SEGMENT_PO2` (13 to 20) sets the zkVM segment size.

The answered input (a 20 kB filing, 32 settled notes, a 16-note spend as
evidence) executes in about 68 M cycles (71 segments of 2^20). Cycles grow
with the signatures and DLEQ proofs a filing carries, so a filing at the
64 kB limit is roughly three times that. The Hub proves only when the
holder challenges its assertion, within the graph's assert window.

The end-to-end test with a real proof plays the receipt on Bitcoin. It
takes hours and about 10 GB of memory, so it is run explicitly:

```sh
LICASH_NATIVE_GUESTS=$PWD/r0/guests LICASH_R0=$PWD/r0/host/target/release/licash-r0 \
  regtest/run.sh --test native_real_proof -- --ignored --nocapture
```
