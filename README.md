# Licash

Owner-bound ecash with local Bitcoin recovery. Payments are private, offchain
reissuances by a hub; every holder keeps an individual, collateralized right
to recover her own notes on Bitcoin, and the hub can reduce a claim only with
a spend the holder signed.

The [paper](paper/licash.pdf) specifies the protocol and is the reference
for its design, security, limitations and evaluation. This repository is its
research implementation; it has not had a security review.

## Layout

- `crates/licash`: the protocol library (notes, spends, grants, claims, both
  statements). The crate docs map modules to paper sections.
- `crates/licash-hub`: a hub's operations over its own SQLite ledger.
- `crates/licash-api`: the hub's HTTP API bodies and error codes.
- `crates/licash-wallet`: a wallet over its own SQLite file: hubs, notes,
  payments made and taken, unlinking, and resuming after interruptions.
- `crates/licash-groth16`: RISC Zero Groth16 receipt verification, used
  inside the holder's program and on the host.
- `r0/`: the holder's and the hub's programs for the RISC Zero zkVM, and a
  host that executes and proves them. See [r0/README.md](r0/README.md).
- `regtest/`: the contract's BABE transaction graph and end-to-end tests on
  Bitcoin Core and LND. See [regtest/README.md](regtest/README.md).
- `apps/`: the applications, a workspace of their own: the hub service
  (`apps/hub`), operator notifications (`apps/notify`) and the wallet CLI
  (`apps/wallet-cli`).
- `paper/`: the paper's source; `paper/build.sh` rebuilds the PDF (Docker).

## Quick start

The library needs only Rust:

```sh
cargo test --locked
cargo test --locked --manifest-path apps/Cargo.toml   # the applications
```

## License

MIT, see [LICENSE](LICENSE).
