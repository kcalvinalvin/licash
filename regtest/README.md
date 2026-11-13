# Bitcoin integration

The contract on Bitcoin: Lichen's BABE transaction graph (`src/graph.rs`,
played by `src/contract.rs`), opened from licash's Base (`src/native.rs`),
with end-to-end tests that fund real contracts on a fresh Bitcoin Core 30
regtest node under the default relay policy. The garbled circuits are real;
proofs are mocked (a test key) except in `tests/native_real_proof.rs`.

## Prerequisites

Rust, Docker, and these checkouts beside this repository:

```sh
git clone https://github.com/AdamISZ/lichen ../lichen
git -C ../lichen checkout f025f81c646fe62d138f4476c52bdaf3f03faec2
git clone https://github.com/AdamISZ/ln-gap ../ln-gap
git -C ../ln-gap checkout 1f0db8ca9f78c36c75c1a69b42946ae1639bb4b8
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
regtest/lightning.sh                                               # Lightning, two pinned LND nodes
```

`regtest/lightning.sh` creates a private Docker network and removes its
containers on exit. With `LICASH_NATIVE_INPUTS=<dir>` the tests also write
the guest inputs they use, for `r0/` to execute or prove (see
[../r0/README.md](../r0/README.md), which also runs the real-proof test).
