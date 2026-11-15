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
licash-hub --data ./hub dashboard
```

`dashboard` draws the hub's dashboard in the terminal: payments by kind
(transfers, locks, Lightning out and in, deposits), their failures by
reason and those under way, the fees earned, the notes outstanding and the
collateral of the contracts that protect them, the notes' anonymity sets,
health checks, among them whether the hub at `--url` answers `/health`,
and below the overview its recent events, newest first: each transfer,
lock, payment, invoice, deposit and contract as it moved, and the hub's
alerts. It reads the files in `<data>` only, beside the running hub or
without it, and never writes or migrates them. History comes from the
records themselves, over a range of 24 hours to all of it (30 days at
first, or `--range`) against the range before; what no record dates, such
as the collateral, is as it stands now. Arrows or digits turn the pages,
`[` and `]` the range; `--json` prints the range's view once instead.

`--network` (`bitcoin` by default, or `regtest`, `signet`, `mutinynet`) is
the chain `--chain` must follow: the hub scans no other. The signets are for
testing only: header checks cannot tell their blocks from forged ones.

`<data>/seed` is the hub's only secret: keep a copy offline. Never replace
`<data>/hub.sqlite` while the hub runs: the hub halts, and `/health` answers
503, until it is restarted.

Notes the hub locks for a deposit, a Lightning receipt or a swap into it
are backed by that payment, recorded in `deposits.sqlite`,
`lightning.sqlite` or `swaps/swaps.sqlite`. The hub refuses to start while
it may yet pay such a lock that none of these files records, as when one
was restored older than `hub.sqlite`, and names the locks: restore the
files as recent as `hub.sqlite`. `--allow-unbacked-locks` runs it anyway,
alerting the operator: it would pay those notes unbacked.

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
back it up with `hub.sqlite`. Alerts say when a claim is late or lost. With
both gateways, a deposit and a Lightning receipt never share a hash: one
payment backs each lock of the hub's notes, and a payment made under a hash
the other already locked for returns to its payer (an alert says so).

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

With `--swaps` (and `--chain`), the hub provides the swaps into it from the
hubs of `--swaps-from URL=CAP`: a wallet locks its notes at such a hub to
the hub's provider, and the hub locks notes it issues to the wallet under
the same hash. The wallet reveals the preimage to the hub itself, so the
provider always learns it in time to claim the wallet's lock. The hub's
lock expires `--swaps-confirm` (36 by default), `--swaps-margin` and two
blocks before the wallet's. `--swaps-fee-base`, `--swaps-fee-ppm`,
`--swaps-min` and `--swaps-max` set the terms. CAP bounds the notes the
provider holds at that hub, or is owed by the swaps under way: past it,
swaps from that hub are declined. A cap holds for every URL the provider's
wallet knows the hub by, such as the URL given and the `https` one it
moves to, and counts the swaps recorded at each of them. The
hub locks no hash its Lightning receipts or deposits hold. The provider's
wallet and swaps are kept in `<data>/swaps` (`wallet.sqlite` and
`swaps.sqlite`, made on the first start): back them up with `hub.sqlite`,
since an older `swaps.sqlite` loses the claims backing notes the hub
locked. The hub refuses to start without `--swaps` while swaps are under
way. To move the provider's notes out, stop the hub and run `licash-wallet
--wallet <data>/swaps/wallet.sqlite`.

With `--onion`, the hub also serves its API as an onion service of a Tor
client of its own (Arti), and announces its URL, reaching the relays
through Tor too. The service's key is in `<data>/tor/state`: back it up to
keep the address.

Notification targets are Apprise URLs (`gotify[s]://`, `tgram://`,
`ntfy[s]://`, `json[s]://`, `apprise[s]://`); `test-notify` checks them.
Each notification lists the event's figures, ids cut to 12 characters as
on the dashboard: a spend released or a lock paid or refunded shows the
hub's income, its fee, and its rate in ppm of the amount. Costs, such as
Lightning routing fees, go unrecorded, so no profit is shown.
`--notify-kinds` picks the kinds of event: by default all but locks
admitted (`locked`), still under way, whose fee is the hub's only if they
pay. The high-priority ones need
a look: a double spend refused; a lock activated on Bitcoin (its preimage
published in time, so it pays), or one the hub had refunded already, after
a reorganization (it owes the payee too: check the reserves); a
reorganization at or below a checkpoint (the hub follows Bitcoin no more,
taking and refunding no locks, until you act); a halt; and alerts.

```sh
docker build -f apps/hub/Dockerfile -t licash-hub .
docker run --rm -v licash-hub:/data licash-hub init
docker run -d -p 8338:8338 -v licash-hub:/data licash-hub
```
