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
- `crates/licash-api`: the HTTP API bodies and error codes of hubs and swap
  providers.
- `crates/licash-wallet`: a wallet over its own SQLite file: hubs, notes,
  payments made and taken, locks, Lightning, swaps, unlinking, and resuming
  after interruptions.
- `crates/licash-swaps`: a swap provider over its own wallet: swaps of notes
  at one hub for notes at another, with a timed lock at each.
- `crates/licash-groth16`: RISC Zero Groth16 receipt verification, used
  inside the holder's program and on the host.
- `r0/`: the holder's and the hub's programs for the RISC Zero zkVM, and a
  host that executes and proves them. See [r0/README.md](r0/README.md).
- `regtest/`: the contract's BABE transaction graph and end-to-end tests on
  Bitcoin Core and LND. See [regtest/README.md](regtest/README.md).
- `apps/`: the applications, a workspace of their own: the hub service
  (`apps/hub`), the swap provider service (`apps/swaps`), the wallet CLI
  (`apps/wallet-cli`), their HTTP transport (`apps/transport`), Tor
  through Arti (`apps/tor`), chain sources (`apps/chain-client`), operator
  notifications (`apps/notify`), announcements on Nostr (`apps/nostr`), and
  end-to-end tests of hubs, their Lightning gateways, swap providers and
  wallets on a regtest ecosystem in Docker (`apps/e2e`).
- `paper/`: the paper's source; `paper/build.sh` rebuilds the PDF (Docker).

## Quick start

The library needs only Rust:

```sh
cargo test --locked
cargo test --locked --manifest-path apps/Cargo.toml   # the applications
apps/e2e/run.sh                                       # the ecosystem (Docker)
```

## License

MIT, see [LICENSE](LICENSE).
