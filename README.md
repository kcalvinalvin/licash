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
- `crates/licash-groth16`: RISC Zero Groth16 receipt verification, used
  inside the holder's program and on the host.
- `r0/`: the holder's and the hub's programs for the RISC Zero zkVM, and a
  host that executes and proves them. See [r0/README.md](r0/README.md).
- `regtest/`: the contract's BABE transaction graph and end-to-end tests on
  Bitcoin Core and LND. See [regtest/README.md](regtest/README.md).
- `paper/`: the paper's source; `paper/build.sh` rebuilds the PDF (Docker).

## Quick start

The library needs only Rust:

```sh
cargo test --locked
```

## License

MIT, see [LICENSE](LICENSE).
