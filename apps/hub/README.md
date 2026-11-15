# Hub service

`licash-hub` serves a hub (`crates/licash-hub`) over the HTTP API of
`crates/licash-api`. `licash-hub --help` lists the commands, flags and
environment variables.

```sh
licash-hub --data ./hub init
licash-hub --data ./hub run --listen 0.0.0.0:8338 --chain esplora+https://esplora.example/api \
  --notify tgram://BOT_TOKEN/CHAT_ID
licash-hub --data ./hub status
licash-hub --data ./hub events --follow
```

`--network` (`bitcoin` by default, or `regtest`, `signet`, `mutinynet`) is
the chain `--chain` must follow: the hub scans no other. The signets are for
testing only: header checks cannot tell their blocks from forged ones.

`<data>/seed` is the hub's only secret: keep a copy offline. Never replace
`<data>/hub.sqlite` while the hub runs: the hub halts, and `/health` answers
503, until it is restarted.

With `--lnd URL --lnd-macaroon FILE` (and `--chain`), the hub also runs a
Lightning gateway over LND: it pays invoices for locks that pay the hub,
and makes hold invoices paid with locks of its notes. `--lnd-cert` pins
LND's certificate. The gateway keeps its invoices in
`<data>/lightning.sqlite`: back it up with `hub.sqlite`. An alert says when
LND's channels can no longer pay or receive the most it takes.

With `--tweaks` (and `--chain`), the hub indexes the silent payment
(BIP352) tweaks of the chain's blocks from `--tweaks-from` (by default, the
tip at the first start) in `<data>/tweaks.sqlite`, and serves them to light
wallets at `/v1/chain/tweaks`. Deleting the file only loses the index.

With `--deposit-sweep-address ADDRESS` (and `--chain`), the hub takes
deposits on Bitcoin: a payment to a deposit's address, once confirmed,
locks the hub's own notes to its payer, and the preimage the payer claims
them with lets the hub claim the coins to the sweep address. Unclaimed
deposits return to their payer after a timeout. `--deposit-*` flags set
the fee, amounts and timing. Deposits are kept in `<data>/deposits.sqlite`:
back it up with `hub.sqlite`. Alerts say when a claim is late or lost.

With `--nostr-relay URL --announce-url URL` (and `--chain`), the hub
announces itself on Nostr relays for wallets to discover, under a key
derived from its seed that `/v1/info` names. `/v1/info` also names the
relays of its wallets' mailboxes, where payers send them payments: those
of `--nostr-relay` unless `--mailbox-relay` names others.

With `--contracts` (and `--chain` naming a Bitcoin Core node's JSON-RPC,
with a wallet), the hub opens recovery contracts with its wallets: each
locks collateral of the node's wallet behind the wallet's notes at the hub,
up to a cap, until the contract's close. `/v1/info` names the cap, the
price a wallet pays to open one and the most blocks to its close; the
openings take `/v1/contracts`. F, a contract's funding, spends the node's
coins, which stay locked while the opening lives. Openings are kept in
`<data>/openings.sqlite`, as sensitive as the seed: back it up with
`hub.sqlite`. Regtest only for now, under a mock proof authority: such a
contract protects nothing. Each opening downloads about 100 MB of garbled
instances to the wallet.

With `--onion`, the hub also serves its API as an onion service of a Tor
client of its own (Arti), and announces its URL, reaching the relays
through Tor too. The service's key is in `<data>/tor/state`: back it up to
keep the address.

Notification targets are Apprise URLs (`gotify[s]://`, `tgram://`,
`ntfy[s]://`, `json[s]://`, `apprise[s]://`); `test-notify` checks them.

```sh
docker build -f apps/hub/Dockerfile -t licash-hub .
docker run --rm -v licash-hub:/data licash-hub init
docker run -d -p 8338:8338 -v licash-hub:/data licash-hub
```
