# Swap provider service

`licash-swaps` runs a swap provider. It moves value between hubs, and
between hubs and Bitcoin, with a timed lock on each side under the client's
hash, so neither side can cheat. It lets users move notes from one hub to
another, buy notes with bitcoin and pay out onchain for an exact amount.
The provider earns a fee on each swap. A hub can also run it as part of
itself.

## Run it

Use the image `ghcr.io/lichenbtc-org/licash-swaps`, or build it with `cargo
build --release --manifest-path apps/Cargo.toml -p licash-swaps-service`.
The provider is a Licash wallet: fund it at each hub it serves and on
Bitcoin.

```sh
licash-swaps --data ./swaps init --network signet   # prints its address
export LICASH_WALLET=./swaps/wallet.sqlite
licash-wallet add-hub https://hub-a.example
licash-wallet add-hub https://hub-b.example
licash-wallet open-contract https://hub-a.example   # protect its notes there
licash-swaps --data ./swaps run --listen 0.0.0.0:8340 --fee-ppm 2000 --chain URL
```

`licash-swaps --help` lists every command, flag and environment variable.
Funding, payouts, announcements and operations are covered in
[docs/swaps.md](../../docs/swaps.md).
