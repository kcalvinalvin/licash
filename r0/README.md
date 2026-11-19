# RISC Zero programs

- `hp-guest/`: the Hub's statement (`licash_hp::check`): the claim the
  holder's publication carries is owed at most the Hub's response `b′`. It
  commits Lichen's 68-byte journal `c ‖ H("licash/hp/word" ‖ Base digest)
  ‖ H(O_D ‖ checkpoint ‖ W)`, fixed when the contract opens, so the holder
  garbles for it before anyone proves.
- `host/`: packs, executes and proves it. Every command also runs
  `licash_hp::check` natively and fails when the guest disagrees with it.

Each is a workspace of its own. The guest reads the exit with
`crates/licash-statement` (what Bitcoin shows of it on Lichen's hub-proves
graph), a path dependency as `crates/licash-hp` is.

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

Contracts open under the program `apps/contracts` names (`SIGNET` in
`apps/contracts/src/lib.rs`), and those opened under the ones before
(`SIGNET_BEFORE`) are played still. Each is kept in `r0/programs/` as
`<image id>.bin`: a contract opened under one is proven with it alone, so
none named there is ever removed. A hub's prover lacking one responds to
no contested claim of its contracts (and alerts), so install every
program before starting an upgraded hub. Each file's image id is its name:

```sh
for p in r0/programs/*.bin; do
  cargo run -q --locked --release --manifest-path r0/host/Cargo.toml -- id "$p"
done
```

| Network | Program | Image id | Built from |
|---|---|---|---|
| signet | `hp` (the Hub's statement), new contracts (graph id 9: the compact filing) | `a2458fdbd126c100b82bb50e8934fe4f5f4c2a5a80e981f4ca63c6f189dfab8d` | 16d35e8 |
| signet | `hp`, contracts opened before (graph id 9: the compact filing, Lichen's statement crate) | `531b465ddc59b1a1f2295c97b54d7e090e0ced23b443084ba9e51dad1524dc64` | b8d8292 |
| signet | `hp`, contracts opened before (graph id 8, legacy filing, her contest window) | `7214a5393e19298874c6a864efd9e0c9beb221e68440df52b5a949f4b0f87734` | 72748a0 |
| signet | `hp`, contracts opened before (graph id 8, legacy filing, v5 Bases) | `817b403459c7b3ed2ea6dd707f0a75b7cc8ac564a601c74da6de92d9c0b6bd52` | 31fb677 |
| signet | `hp`, contracts opened before (v3 and v4 Bases) | `aa7f2d19079b339b1fee1e7666f445232f012965dc7c0ab8c35fb560a4d68a73` | 9d0f04f |
| signet | `hp`, contracts opened before (v3 Bases) | `6bf78e6092f9c0959cee4908b449a032e575d527c5c889d8b9f3813b888f0d0c` | 0f86cc6 |

The newest program reads co-funded (v5) Bases as well, whose claims name
the notes she funded her contract with, and both filings, each by its
contract's graph id (`licash::recovery::Format`). It checks each header
once, on the zkVM's accelerator: 9.4 k cycles a header, of the 28 k of the
programs before. A hub quotes a graph of the compact filing only where the
program it trusts decides it (`SIGNET_COMPACT`), and holders refuse such a
quote under any other: under a program that decodes only the legacy
filing, her compact claim would decode to nothing and be owed nothing. The
oldest program checks no signature of her request (a v3 Base binds
no keys), reads a request's digits above 15 as Bitcoin doesn't, and owes
a lock's refund claimed before its expiry, which a payer may take and
then activate the lock as its payee: its contracts keep that exposure
to their end.

The holder's and counter programs of the design before hub-proves are
gone with it: no contract opened under them is played.

Signet's are a trial under RISC Zero's shared Groth16 setup, and the
programs check signet's headers by their work alone, not by the network's
signature of each block: headers that fool them cost little to make.

Rebuilt, a program's file may differ from the one kept in its symbols'
names, which its image id does not cover (they vary from build to build):
compare image ids, not files.

To name new ones, build them from a commit, rebuild them in another
checkout to see the same image ids, copy each `r0/guests/<name>.bin` to
`r0/programs/<image id>.bin`, and name them in `apps/contracts`, moving
the programs named before to `SIGNET_BEFORE` for the contracts opened
under them.

## Execute and prove

Export statement inputs (`licash_hp::HubInput::to_bytes`) from the
statement's tests: a 32-item claim the Hub answers with a 16-note spend,
and the same claim with a response it can't prove:

```sh
LICASH_HP_INPUTS=$PWD/tmp/hp-inputs cargo test --release \
  --manifest-path regtest/Cargo.toml -p licash-hp export_inputs
```

Then execute (journal and cycle count) or prove: a succinct receipt, its
Groth16 wrap, verified and exported to `<work-dir>/proof.json` as JSON with
the 256-byte seal that `lichen_babe::risc0::proof_from_seal` reads. The work
directory keeps the prover's progress (each segment proved), so a stopped
proof carries on where it was:

```sh
licash=r0/host/target/release/licash-r0
$licash hp-exec tmp/hp-inputs/answered.bin
$licash hp-prove tmp/hp-inputs/answered.bin tmp/answered
$licash probe
$licash bench tmp/bench.json
```

`probe` prints what the machine proves with (`cuda` or `cpu`, the programs
installed, its memory) and what it lacks; `bench` proves two segments of
`r0/host/bench.bin` (the answered input), then wraps them, and prints the
seconds a segment takes and those of the wrap, as the hub plans its proofs
on. A program is a path, or an image id installed in
`$LICASH_R0_SHARE/programs/<id>.bin` (by default `../share/licash-r0` beside
the binary); RISC Zero's Groth16 setup is in its `groth16/` directory
(`stark_verify`, `stark_verify.dat`, `prover`, `stark_verify.cs`,
`stark_verify_final.pk.dmp`, from `risczero/risc0-groth16-prover`), x86-64
only. The hub's image (apps/hub/Dockerfile) installs both.

Built with `--features cuda` and installed beside it as `licash-r0-cuda`,
the CUDA build proves on the GPU: `licash-r0` uses it when
`licash-r0-cuda probe-gpu` finds a usable GPU, in segments of 2^18, then
2^17 should the GPU run out of memory (each attempt in a process of its
own), and proves on the CPU in segments of 2^19 otherwise. The Groth16 wrap
runs on the CPU either way. On an RTX 2060 (6 GB) the answered input proves
in about 20 minutes; on 6 CPU cores, about 8.5 hours.

`LICASH_SEGMENT_PO2` (13 to 20) sets the zkVM segment size.

The answered input (a 20 kB filing, 32 settled notes, a 16-note spend as
evidence) executes in about 68 M cycles (71 segments of 2^20), 73 M in
the program before the newest and 70 M in it with the compact filing. Each
header of the exit adds about 9.4 k cycles in the newest program (3,000
more headers, 98 M cycles), 28 k in those before (157 M). Cycles grow
with the signatures and DLEQ proofs a filing carries, so a filing at the
64 kB limit is roughly three times that. The Hub proves only when a holder
contests its response: it responds with less than the claim, the holder
checks the evidence it responded on (`licash_hp::bears_out`, the same test
as the statement's) and leaves a response that bears out alone, which then
pays what it says with no proof, and the Hub asserts the proof within the
graph's assert window of her contest otherwise.

A hub proves on signet through `licash-r0` (`LICASH_HP_PROVER` names
another), one proof at a time (apps/hub/README.md); no test plays a real
proof on Bitcoin yet.

## Measured

`r0/build-guest.sh hp-profile` builds the statement with its steps'
cycles printed (never a program to keep); `hp-exec` with it shows them. On
the answered input above (32 settled notes, a 16-note spend as evidence,
a handful of headers):

| Step | Cycles |
|---|---|
| the input decoded: Base, and every note and signature of the evidence | 25.9 M |
| the exit read: headers' work and transactions | 0.2 M |
| her request's one-time signatures (v4 and later Bases) | 5.2 M |
| the Licash header chain through her publication | 0.02 M (4.4 k a header) |
| her filing decoded and judged: each settled note's issuance proof and hold signature | 41.3 M |
| total | 67.6 M |

So proving cost is the notes' signatures and DLEQs (about 1.4 M cycles a
note), then the headers, and almost none of it is Bitcoin's. A dispute's
transactions weigh (`a_dispute_is_made_of_transactions_of_known_size`):

| Transaction | vB |
|---|---|
| `request` | 2 154 |
| `respond` | 491 |
| `contest` | 206 |
| `assert` | 7 725 |
| `challenge_assert` | 20 717 |
| a terminal with its fee coin | 625 to 668 |

An uncontested response is a `request`, her publication, a `respond` and
its terminal; the proof and its 28 kvB are for a contest alone.

A header costs the statement about 7.2 k cycles (measured with 1 000 more:
2.8 k in the exit read, one hash each, and 4.4 k in the Licash chain's
checks), where the filing and evidence cost about 1.4 M cycles a note. A
contract of half a year (26 280 headers) adds about 190 M cycles to the
statement's 68 M: its age, not its claim, is then most of a proof, which a
recursive proof of the header chain would take out.

## The lean filing

A contract of graph id 10 on (`licash::recovery::Format::Lean`) files as the
compact one does and is judged item by item: the statement verifies the
issuance proof and hold signature of the items its input flags as invalid
(`HubInput::invalid`) and no others, and takes the rest as valid, which can
only raise what her claim is owed. The Hub finds the invalid items natively
(`licash_hp::flagged`) and a flag on a valid item is no proof; an invalid item
is worth nothing and the rest of the claim is owed, where under the compact
filing one voids it whole. On the answered input:

| | compact | lean |
|---|---|---|
| her filing decoded and judged | 37.3 M | 19.1 M |
| total | 63.5 M | 45.3 M |

What is left is the evidence's decoding (25.9 M: every note and signature of
a spend the Hub cites) and, for each item, its hash to the curve. A signet
program that decides it is built with `r0/build-guest.sh hp` and set with
`SIGNET_LEAN` in `apps/contracts`; until then signet quotes the compact
filing's graph.
