# RISC Zero programs

The hub's statement as a RISC Zero zkVM program, and the host that executes
and proves it. When a holder contests the hub's response to her claim, the
hub proves with this program that the claim is owed at most what it
responded. A Groth16 wrap lets the holder's contract check that proof on
Bitcoin through BABE.

- `hp-guest/`: the statement (`licash_hp::check`).
- `host/`: packs, executes and proves it. Every command also runs the
  statement natively and fails if the guest disagrees.

## Build

You need [rzup](https://dev.risczero.com) with the toolchains that the kept
programs were built with:

```sh
rzup install rust 1.97.0
rzup install cpp 2024.1.5
r0/build-guest.sh hp      # r0/guests/hp.{bin,id}
cargo build --release --manifest-path r0/host/Cargo.toml
```

Kept programs, reproducible builds, executing and proving (CPU or CUDA),
and measured costs are covered in [docs/r0.md](../docs/r0.md).
