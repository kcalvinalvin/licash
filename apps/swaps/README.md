# Swap provider service

`licash-swaps` serves a swap provider (`crates/licash-swaps`) over the HTTP
API of `licash_api::swaps`: clients swap notes at one hub for notes at
another, with a timed lock at each under the client's hash.
`licash-swaps --help` lists the commands, flags and environment variables.

```sh
licash-swaps --data ./swaps init                # prints the provider's address
export LICASH_WALLET=./swaps/wallet.sqlite
licash-wallet add-hub https://hub-a.example
licash-wallet add-hub https://hub-b.example
licash-wallet receive payment.json              # fund it at each hub
licash-swaps --data ./swaps run --listen 0.0.0.0:8340 --fee-ppm 2000 \
  --notify tgram://BOT_TOKEN/CHAT_ID
licash-swaps --data ./swaps status
licash-swaps --data ./swaps events --follow
```

The provider swaps between the hubs its wallet added, up to what it holds
at the destination hub. Back up `<data>/wallet.sqlite` and
`<data>/swaps.sqlite` together. A stopped provider takes its swaps on when
started again, but a swap whose client lock expires before it claims it is
lost (`status` counts it, and a high-priority notification says so):
`--margin` sets the blocks it keeps for that.

With `--nostr-relay URL --announce-url URL`, the provider announces itself on
Nostr relays for wallets to discover, under the key `/v1/swaps/info` names.
With `--tor`, it reaches hubs, onion services included, through a Tor client
of its own, which keeps its state in `<data>/tor`. With `--onion`, it also
serves its API as an onion service, announced like `--announce-url`'s; back
up `<data>/tor/state` to keep the address.
Notification targets are Apprise URLs, as for the hub; `test-notify` checks
them.

```sh
docker build -f apps/swaps/Dockerfile -t licash-swaps .
docker run --rm -v licash-swaps:/data licash-swaps init
docker run --rm -v licash-swaps:/data --entrypoint licash-wallet licash-swaps add-hub https://hub-a.example
docker run -d -p 8340:8340 -v licash-swaps:/data licash-swaps
```
