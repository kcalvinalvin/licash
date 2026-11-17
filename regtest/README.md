# The contract's crates, and the contracts on Bitcoin

`regtest/Cargo.toml` is the workspace of the recovery contract's crates,
`crates/licash-contract` (Licash's front on Lichen's BABE graph, its
opening and its dispute) and `crates/licash-hp` (the hub's statement),
kept apart from the library's: they pull Lichen by path from `vendor/`.

The contracts are played on Bitcoin by the hub service's tests
(`apps/hub/tests/contracts.rs`): a wallet opens contracts at the hub
service over HTTP on a fresh Bitcoin Core 30 regtest node, under the mock
authority (the garbled circuits are real; proofs are mocked), and forces
them closed.

## Prerequisites

Rust, Docker, and Lichen at the revision `vendor/fetch.sh` pins, which it
copies into `vendor/` from a checkout beside this repository:

```sh
git clone https://github.com/lichenbtc-org/lichen ../lichen
vendor/fetch.sh
```

Each node runs in its own throwaway `bitcoin/bitcoin:30.0` container with a
fresh datadir under `tmp/`. `LICHEN_BITCOIND` selects another `bitcoind`
binary and `LICASH_BITCOIN_IMAGE` another image.

## Run

From the repository root:

```sh
cargo test --release --locked --manifest-path regtest/Cargo.toml -p licash-contract -p licash-hp
regtest/run.sh                    # the contracts on Bitcoin Core
regtest/run.sh --nocapture        # with their output
```
