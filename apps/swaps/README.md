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
licash-swaps --data ./swaps dashboard
```

`dashboard` draws the provider's dashboard in the terminal: swaps by route,
failures by reason and those under way, fees less what lost swaps cost, the
notes held at each hub with the days they last at the rate they fell, what
is free to lock and reserved, the coins on Bitcoin, and health checks,
among them whether the provider at `--url` answers `/health` and whether
its hubs answer it. It reads `<data>` only, beside the running provider or
without it, and never writes or migrates its files. History comes from the
records themselves, over a range of 24 hours to all of it (30 days at
first, or `--range`) against the range before. Arrows or digits turn the
pages, `[` and `]` the range; `--json` prints the range's view once
instead.

`init --network` names the network of its hubs, `bitcoin` by default.
The provider swaps between the hubs its wallet added, up to what it holds
at the destination hub; an alert says when that falls below `--min`, once
a start until it recovers. Back up `<data>/wallet.sqlite` and
`<data>/swaps.sqlite` together. A stopped provider takes its swaps on when
started again, but a swap whose client lock expires before it claims it is
lost (`status` counts it, and a high-priority notification says so):
`--margin` sets the blocks it keeps for that. Its wallet unlinks the notes
it is paid each round, and renews those a hub no longer unlinks since it
restarted, once, if the hub's fee on them is at most `--renew-ppm` parts
per million of their value (100 by default, 0 for free renewals only).

With `--chain URL` (Bitcoin Core's RPC, `http://user:password@host:8332`, or
Esplora, `esplora+https://esplora.example/api`), the provider also pays notes
out on Bitcoin: a client locks the coin's value and the fees to it at a hub,
the provider pays a coin of its own to an HTLC under the lock's hash, timed
out `--payout-margin` blocks before the lock expires, and claims the lock
with the preimage the client's claim of the coin reveals on chain. A coin
never claimed comes back to the provider once timed out. Fund it on chain at
the address `licash-swaps --data ./swaps address --chain URL` prints: its
wallet watches the chain from its tip when first started with `--chain`, or
from `--chain-from`. `--payout-fee-base`, `--payout-fee-ppm`,
`--payout-miner` (the coin's miners, on top), `--payout-rate` (sat/vB, else
the chain's estimate at start), `--payout-min` and `--payout-max` set the
terms; `status` shows the coins. A provider with payouts under way refuses
to start without `--chain`.

The client reveals its preimage to the destination hub, which issues the
provider's lock there and could keep the preimage from it. The provider
would then learn it only from a refund request of its lock on Bitcoin,
which the hub voids only by publishing the preimage within its response
window. So its lock expires earlier than the client's by that window,
`--confirm` (the blocks to get the request confirmed, 36 by default) and
`--margin`, and `/v1/swaps/info` gives that margin for each hub. A swap is
declined unless the source hub admits a lock that late, which takes a
source hub with a longer window than the destination's: between hubs of
equal windows, the provider swaps nothing. It makes no refund request
itself: a high-priority alert names the provider's lock left neither paid
nor refunded after it expired, and the height the request must confirm by.
A hub can provide the swaps into it instead, issuing the provider's lock
itself, with no such risk: see `licash-hub run --swaps`.

With `--nostr-relay URL --announce-url URL`, the provider announces itself on
Nostr relays for wallets to discover, under the key `/v1/swaps/info` names.
With `--tor`, it reaches hubs and relays, onion services included, through a
Tor client of its own, which keeps its state in `<data>/tor`; `--proxy
socks5h://...` goes through a Tor daemon instead. With `--onion`, it also
serves its API as an onion service, announced like `--announce-url`'s; back
up `<data>/tor/state` to keep the address.
Notification targets are Apprise URLs, as for the hub; `test-notify` checks
them. Each notification lists the event's figures, ids cut to 12
characters; `--notify-kinds` picks the kinds of event. A refunded swap is
one whose client never claimed the provider's lock; a lost one, at high
priority, one the client was paid for but whose lock the provider did not
claim in return. Swaps and payouts under way resume when a stopped
provider starts again.

```sh
docker build -f apps/swaps/Dockerfile -t licash-swaps .
docker run --rm -v licash-swaps:/data licash-swaps init
docker run --rm -v licash-swaps:/data --entrypoint licash-wallet licash-swaps add-hub https://hub-a.example
docker run -d -p 8340:8340 -v licash-swaps:/data licash-swaps
```
