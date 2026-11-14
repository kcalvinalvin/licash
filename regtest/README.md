# Bitcoin integration

The contract on Bitcoin: Lichen's BABE transaction graph
(`crates/licash-contract`, played by `src/contract.rs`), opened from
licash's Base by the holder's and the hub's state machines of
`licash_contract::opening` over a byte pipe (`src/native.rs`, which has
Bitcoin Core fund and sign F), with end-to-end tests that fund real
contracts on a fresh Bitcoin Core 30 regtest node under the default relay
policy. The garbled circuits are real; proofs are mocked (a test key)
except in `tests/native_real_proof.rs`.

## Prerequisites

Rust, Docker, and these checkouts beside this repository:

```sh
git clone https://github.com/AdamISZ/lichen ../lichen
git -C ../lichen fetch origin 82c69a055a8ac9f2010d1f91ff15eef7ac1ca8ab
git -C ../lichen checkout 82c69a055a8ac9f2010d1f91ff15eef7ac1ca8ab
git clone https://github.com/AdamISZ/ln-gap ../ln-gap
git -C ../ln-gap fetch origin d7972035c12247a07b1dcc1b32e1682cd790d276
git -C ../ln-gap checkout d7972035c12247a07b1dcc1b32e1682cd790d276
```

Each node runs in its own throwaway `bitcoin/bitcoin:30.0` container with a
fresh datadir under `tmp/`. `LNGAP_BITCOIND` selects another `bitcoind`
binary and `LICASH_BITCOIN_IMAGE` another image.

## Run

From the repository root:

```sh
regtest/run.sh -- --test-threads=1                                 # the whole suite
regtest/run.sh --test native_lifecycle -- --test-threads=1         # recovery outcomes
regtest/run.sh --test native_interop -- --test-threads=1           # swaps and routes
regtest/run.sh --test native_opening -- --test-threads=1           # forged opening messages
cargo test --release --locked --manifest-path regtest/Cargo.toml -p licash-contract  # message codecs
regtest/lightning.sh                                               # Lightning, two pinned LND nodes
```

`regtest/lightning.sh` creates a private Docker network and removes its
containers on exit. With `LICASH_NATIVE_INPUTS=<dir>` the tests also write
the guest inputs they use, for `r0/` to execute or prove (see
[../r0/README.md](../r0/README.md), which also runs the real-proof test).
