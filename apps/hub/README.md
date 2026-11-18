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

A hub charges for what it does unless told otherwise. A payment, and each
branch of a lock, pays `base + ppm of the notes spent + a fee for each note
made for others than the hub`: by default 1 sat, 100 ppm (rounded up) and
0.1 sat a note (their sum rounded to the nearest sat). `--spend-fee-base`
and `--spend-fee-ppm` set the first two on a payment, `--lock-fee-*` and
`--refund-fee-*` in a lock's pay and refund branches, `--output-fee-msat`
the fee on a note, in thousandths of a sat, for all three. A payment
received over Lightning costs `--ln-receive-fee-base` and
`--ln-receive-fee-ppm`, 1 sat and 100 ppm by default. A hub that ran
without these flags charged nothing: upgraded, it charges the defaults
(`run` logs what it charges as it starts), and 0 on each keeps it free.
Wallets that predate the fee on notes do not pay it, so a payment of theirs
that makes five notes or more is refused until they upgrade:
`--output-fee-msat 0` spares them.

Fees may change at any restart, but a wallet holding a recovery contract
agreed to those charged as it opened, and no more: it stops paying a hub
that charges more, in any fee, until its owner approves the new fees by
hand. With `--contracts`, the hub keeps what it charged with each opening
(in `<data>/openings.sqlite`) and `run` warns, as it starts, of how many
contracts it signed opened for less than it charges now. Nothing else is
done with it, and openings from before it was kept are not counted.

`status` shows income, released payments and open locks. `status --details`
adds the internal note and event counts; `status --json` keeps the full
machine-readable statistics. Normal text logs omit timestamps and Rust
targets. Configuration is diagnostic detail (`RUST_LOG=debug`); warnings
remain visible. `--log json` retains structured logging.

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
Transaction fees include their share of the processed input, while contract
prices are relative to their cap. A read failure keeps the last good data
and the help/quit keys; `?` shows the error detail.

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

It also refuses to run with `--lnd` or `--deposit-sweep-address` disabled
while that gateway still has obligations to collect, including deposits
settled but not swept and locks left between writes by an interruption.
The refusal names the file and count. Idle files are inspected read-only,
never created or upgraded; open unpaid requests only warn. Enable the
gateway to finish its work. This also applies when upgrading an existing
installation; no records are discarded or migrated.

With `--lnd URL --lnd-macaroon FILE` (and `--chain`), the hub also runs a
Lightning gateway over LND: it pays invoices for locks that pay the hub,
and makes hold invoices paid with locks of its notes. An invoice of its
own node, a receipt still open, is paid inside the hub for its amount
alone: the payer's lock backs the receipt instead of an HTLC, and its
invoice is canceled. `--lnd-cert` pins
LND's certificate. The gateway keeps its invoices in
`<data>/lightning.sqlite`: back it up with `hub.sqlite`. An alert says when
LND's channels can no longer pay or receive the most it takes.
`--ln-max-receipts` bounds open and locked receipts (1024 by default).
Lowering it blocks new receipts, not retries or following existing ones.

With `--tweaks` (and `--chain`), the hub indexes the silent payment
(BIP352) tweaks of the chain's blocks from `--tweaks-from` (by default, the
tip at the first start) in `<data>/tweaks.sqlite`, and serves them to light
wallets at `/v1/chain/tweaks`. Deleting the file only loses the index.

The hub keeps the payments it releases for their payees when their payers
ask, for 14 days, in `<data>/inbox.sqlite`: payees collect them at
`/v1/inbox` and tell their payers they have them, through the hub, without
Nostr. Anyone sees the inbox's feed, which tells how many payments were
kept, not whom they pay or how much. Deleting the file only loses the
payments kept: their payers send them to the payees' mailboxes on Nostr
once no acknowledgment comes.

With `--deposit-sweep-address ADDRESS` (and `--chain`), the hub takes
deposits on Bitcoin: a payment to a deposit's address, once confirmed,
locks the hub's own notes to its payer, and the preimage the payer claims
them with lets the hub claim the coins to the sweep address. Unclaimed
deposits return to their payer after a timeout. `--deposit-*` flags set
the fee, amounts and timing. Deposits are kept in `<data>/deposits.sqlite`:
back it up with `hub.sqlite`. A claim left unmined is made again at a
higher fee rate as the timeout nears (twice the estimate once fewer than 12 blocks
are left, doubling every 3 blocks, all of the coin but dust at most), and
whichever of them is mined is the hub's. Alerts say when a claim is late or lost. With
the gateways and swap issuer sharing one turn, one payment backs each
lock of the hub's notes. New gateway requests refuse hashes already
reserved by another service, and a payment made under a hash
the other already locked for returns to its payer (an alert says so).
Reservations remain visible while a service is disabled. A swap's hash
reservation is not by itself proof that it owns a gateway's existing lock:
the recorded lock id decides ownership during recovery.

Contract HTTP requests have separate admission budgets for new openings,
progress and instance downloads; excess requests receive 503 before body
buffering or node RPC. Four concurrent instance downloads remain supported.
Admission lasts through response delivery, with a five-minute deadline for
request handling. Onion services accept at most 128 streams and allow two minutes
to receive HTTP headers, without limiting a valid response's duration.

With `--nostr-relay URL --announce-url URL` (and `--chain`), the hub
announces itself on Nostr relays for wallets to discover, under a key
derived from its seed that `/v1/info` names. `/v1/info` also names the
relays of its wallets' mailboxes, where payers send them payments: those
of `--nostr-relay` unless `--mailbox-relay` names others.

With `--contracts` (and `--chain` naming a Bitcoin Core node's JSON-RPC,
with a wallet), the hub opens recovery contracts with its wallets: each
locks collateral of the node's wallet behind the wallet's notes at the hub,
up to a cap, until the contract's close. `--contracts-wallet URL` names
another node's wallet instead, by its JSON-RPC as `--chain` takes it
(`http://host:port/wallet/NAME?cookie=PATH`), on the same network: the hub
may then follow the chain through an Esplora API. A wallet of its own,
funded with what the hub may lock, bounds the collateral at stake.
`/v1/info` names its fixed terms: the cap (4,096 sat), the price a wallet
pays to open one and the blocks to its close (1,000); the openings take
`/v1/contracts`. With `--contracts-max-cap SAT`, wallets may also choose a
contract's cap up to that and how long it lasts up to
`--contracts-max-blocks` (`/v1/contracts/quote?cap=SAT&blocks=N`), its cap
at most `--contracts-per-coin` times (2 by default) the coin the wallet
deposits, at the fixed price plus `--contracts-rate` millionths of the cap
a year (10,000, 1% a year, by default); `/v1/info` then names that range,
its largest cap no more than the contracts' wallet funds (its confirmed
balance, asked each minute with `getbalances`, less the hub's 100,000 sat
reserve and 50,000 for fees): a larger cap is refused as unavailable. A
login limited by `-rpcwhitelist` needs `getbalances` among its calls, or
the hub advertises the range's largest cap whatever its wallet holds.
The collateral of a contract is the next power of two minus one sat at or
above its cap (2,097,151 sat at least, 2,147,483,647 at most). F, a contract's
funding, spends the wallet's coins, which stay locked while the opening
lives. Openings are kept in `<data>/openings.sqlite`, as sensitive as the
seed: back it up with `hub.sqlite`. Each opening receives the kept garbled instances from
the holder's wallet, kept in `<data>/opening-instances/`. The hub takes one opening's step, or
builds one instance, at a time, the others waiting their turn: each opening
takes about 30 CPU seconds over a few threads, and the hub holds about
700 MB once it has opened a few: allow it 2 GB of memory.

On regtest, contracts are under a mock proof authority: such a contract
protects nothing. On signet they are under the genuine programs kept in
`r0/programs/`, a trial: RISC Zero's shared Groth16 setup, signet's headers
checked by their work alone and not the network's signature of each block.
The hub's terms there are signet's least: a day
for the wallet to answer a challenge and for each party's outputs to wait,
3 blocks deep, 12 blocks at least to respond to a request and 48 for the
hub to prove and assert its response (a proof takes hours on a CPU). No other
network yet.

A hub with contracts plays its holders' exits itself, polling its
contracts' Core wallet at `--chain-poll`. When a wallet forces its contract
closed, its request R and its publication (the claim it files) come on
Bitcoin. The hub decides what the claim is owed against its ledger: the
spends it retired the claimed notes for (owner-signed, a granted one with
its unlock) and, for each timed lock the claim names, the branch it did not
settle it on (its preimage, or its expiry). Owed all it asks, the hub lets
the holder's timeout pay her. Owed less (a claim of notes that are not the
keyset's, or spent), it responds with what is owed, publishes in the
response's disclose slot what that evidence needs public (a granted spend's
unlock, a preimage), proves that statement and asserts the proof;
challenged, it answers with a kept instance. On regtest the proof is the
mock authority's; on signet the hub proves with `licash-r0` (r0/README.md,
`LICASH_HP_PROVER` names another), as described below. It sweeps its outputs of each payout to its
wallet; a payout's rest and the holder's reserve pay each party's wallet
directly. A holder may close her contract early: at
`/v1/contracts/openings/{id}/fold` and `.../fold/sign` the hub signs the
fold with her (MuSig2, its nonce kept in memory and used at most once),
paying her reserve back and the rest to the hub less the fee at its fee
rate, and broadcasts it; any key-path spend of a contract output is such a
fold, and ends the contract. A contract no one exited ends at its close: the hub posts its
default, which pays the holder her reserve and the hub its collateral back,
to its wallet. The fee coins its wallet funds for these transactions are
kept in `openings.sqlite` before use, and a contract ended 6 blocks deep is
not looked at again. Keep `<data>/responses.sqlite` with `openings.sqlite`:
it pins the evidence each response is decided on and each one-time
signature (the response, the assertion) before it is made.

A response commits the hub to a proof: its assertion must confirm within
the assert window (48 blocks on signet, 2 of them to confirm in), or the
holder's timeout pays her what she claims, as if it had not responded. So
the hub proves one statement at a time, earliest deadline first, and
responds to a claim only if its prover can still make that proof in time
without making one it committed to late; contested by more holders at once
than it can prove for, it proves for the largest stakes and lets the others'
timeouts pay them (a claim is decided on the poll after the one that first
saw it, all of them known). It plans with its prover's speed, measured at
startup (`licash-r0 probe`, then `licash-r0 bench` once per device, kept in
`<data>/hp-proofs/bench.json`: delete it to measure again, as after
changing the GPU). A prover not ready (none installed, no
Groth16 setup, under 8.5 GB of memory, a failed benchmark) responds to no
contested claim. The hub's notifications alert it, a prover that fails, a
proof the plan would make late, an assertion's deadline 6 blocks away
unproven, the claims a round gave no response to, and more contracts open
than proofs of the smallest one its prover makes in an assert window (the
exits it can contest at once). Each proof's work is
in `<data>/hp-proofs/<id>/`: its input, frozen once it first holds, the
prover's progress, carried on across restarts, its `prover.log` and
`proof.json`. The proof is pinned in `responses.sqlite` before the assertion
is signed.

The image of apps/hub/Dockerfile (x86-64) has the prover: its CPU build,
its CUDA build, which it proves on when the container has a GPU (`--gpus
all`, or a Compose `deploy.resources.reservations.devices` entry with the
NVIDIA runtime; drivers 550 or newer, Turing or newer, 6 GB of memory are
enough), RISC Zero's Groth16 setup and the programs contracts open under.
On an RTX 2060 a proof takes about 20 minutes (the Groth16 wrap about 3 of
them, on the CPU) plus about 7 minutes for each 1,000 headers since the
contract's checkpoint: one of signet's most, 26,280 headers (half a year),
about 3.3 hours, so a hub proves only two such in an assert window. On a CPU alone
a proof takes hours, too slow for signet's window but for the smallest
contracts. Give it 10 GB of memory or more. ARM images have no
prover: their hubs respond to no contested claim on signet.

The hub stops, with an error, when one of its loops stops: the chain's
watch, the tweaks' index, the gateways, the contracts' responder and
sweeper, the swaps. Run it under a supervisor that starts it again (as
Docker's `restart: unless-stopped`).

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
moves to, and counts the swaps recorded at each of them. By default it
also takes in only what recovery contracts at that hub protect, keeping
them as a standalone provider does (see `apps/swaps/README.md`): the first
at each hub opened by the operator with `licash-wallet --wallet
<data>/swaps/wallet.sqlite open-contract URL --chain URL`, then replaced
before they end and forced closed at their safe deadline if needed, the
provider's wallet looking for its coins on the hub's chain. The
`--swaps-contract-successor-days`, `--swaps-contract-exit-days`,
`--swaps-contract-max-price` and `--swaps-drain-via` flags match the
provider's. The hub's own notes, which it issues, need no contract. With
`--swaps-from`, a hub on a network where no contract opens (only regtest
and signet do for now) refuses to start, unless `--swaps-no-contracts`
runs it unprotected, which is reckless; a hub upgraded to this version
takes nothing more in from a hub until a contract there is live. Back up
the contracts (`<data>/swaps/contracts.sqlite` and `<data>/swaps/contracts`)
with the provider's wallet. The
hub locks no hash its Lightning receipts or deposits hold. The provider's
wallet and swaps are kept in `<data>/swaps` (`wallet.sqlite` and
`swaps.sqlite`, made on the first start): back them up with `hub.sqlite`,
since an older `swaps.sqlite` loses the claims backing notes the hub
locked. The hub refuses to start without `--swaps` while swaps are under
way. Each round, the provider's wallet also reshapes the notes it was paid,
as a standalone provider's does (see `apps/swaps/README.md`); its first
start under this version upgrades `wallet.sqlite` to version 8, which older
versions refuse. To move the provider's notes out, stop the hub and run `licash-wallet
--wallet <data>/swaps/wallet.sqlite`. The provider's wallet reaches hubs
directly, so it cannot swap from a hub at an onion service only, unless
with `--swaps-tor`: then through a Tor client of the hub's own, that of
`--onion` (its state in `<data>/tor`), to onion services and to the clear
web alike.

With `--onion`, the hub also serves its API as an onion service of a Tor
client of its own (Arti), and announces its URL, reaching the relays
through Tor too. The service's key is in `<data>/tor/state`: back it up to
keep the address.

Notification targets are Apprise URLs (`gotify[s]://`, `tgram://`,
`ntfy[s]://`, `json[s]://`, `apprise[s]://`); `test-notify` checks them.
Each notification lists the event's figures, ids cut to 12 characters as
on the dashboard: a spend released or a lock paid or refunded shows the
hub's income, its fee, and its rate as a percentage of the amount. Costs,
such as Lightning routing fees, go unrecorded, so no profit is shown.
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
