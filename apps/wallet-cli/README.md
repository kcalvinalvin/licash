# Wallet CLI

`licash-wallet` keeps a wallet (`crates/licash-wallet`) in one file and
pays through the hubs it trusts. `licash-wallet --help` lists the commands.

```sh
licash-wallet init
licash-wallet add-hub https://hub.example
licash-wallet receive payment.json        # or from stdin
licash-wallet send <address> 1000 > payment.json
licash-wallet resume                      # after a payment ended pending
```

A wallet trusts hubs of one network, `bitcoin` unless `init --network`
names `regtest`, `signet` or `mutinynet`.

`address`, `code`, `invoice` and `deposit` take `--qr` to also draw what
they print as a QR code, black on white, for a payer's phone to scan off
the screen. It is drawn only when stdout is a terminal and without `--json`:
a script reading stdout gets the same text as without `--qr`. `code --qr`
prints the code in upper case too, which the QR code holds in fewer modules.

A payment is JSON that only its payee can open: hand it over by any
channel. `send` and `resume` exit with 75 when a payment is recorded but its
hub has not answered; `resume` finishes it. Back up the wallet file: its
seed alone does not restore its notes. After restoring an old copy, `check`
drops the notes spent since.

Notes are unlinked from the payment that made them as they are received,
and a payment's change as it is sent, so that their hub cannot tell where
one came from when it is spent; `unlink` asks again for those left linked.
A hub that restarted no longer unlinks the notes it issued before: the
wallet renews them, paying them to itself for notes it unlinks, once, if
the hub's fee on them is at most `--renew-ppm` parts per million of their
value, or its spending schedule is at most 1 sat base, that relative rate,
and 0.1 sat per output (`LICASH_WALLET_RENEW_PPM`, 1000 by default, 0 for
free renewals only). Live contract ceilings take precedence. `history`
lists a renewal as a payment to the wallet's own address.

Ordinary payments opportunistically consolidate already unlinked notes and
shape their change toward 18 to 24 notes, using the smallest feasible
largest denomination. This leaves receiving room below the 32-note claim
limit without tying a distribution to a contract's cap. Linked notes are
not added merely to consolidate. Any increase in the payment's fee must
fit its live contract ceilings or the automatic fee allowance above.
The resulting denominations can change whether a later Lightning lock
needs a paid self-payment to make exact notes. Consolidation also consumes
old notes: keep wallet backups current, since a stale copy does not know
the replacement notes and cannot recover them from the seed alone.

A wallet that mostly receives gathers many small notes, so `unlink`,
`receive`, `claim` and `resume` also maintain them: at most one remint per hub
of unlinked notes to itself, toward the same distribution. Notes rest 1 to 6
hours after unlinking (a few minutes when nearing the 32-note limit), as
does the hub after each remint, so that a remint follows no payment closely.
Its fee must fit the live contract ceilings or the automatic allowance
above, and at most 1% of the notes held at that hub (at least 1 sat) is
spent on it in any 24 hours. A failed remint never fails the command, and
one refused for its fee is not retried at a higher one. `optimize <hub>`
previews a remint with its fee; `optimize <hub> --max-fee SAT` makes it
now, within that fee and the contract ceilings, bypassing the rests and the
daily budget. `history` lists a remint as `optimized notes`, with its fee.
A wallet file is upgraded to version 8 when first opened by this version,
which older versions refuse: notes unlinked before begin their rest then,
and nothing is reminted or asked of a hub by the upgrade itself.

`code` prints a payment code (a BIP321 URI, `--qr` for a QR code) naming the
wallet, the hubs it is paid at and its Nostr mailbox, if they name relays.
`pay <code> [amount]` pays it at a hub it names that the wallet trusts and
holds enough at, and is done once the hub releases the payment: the hub
keeps it for the payee, who collects it there and tells the payer so
through the hub, signed with a secret only the two share. A payment not
collected within an hour is sent to the payee's mailbox on Nostr by
`deliver`, by `watch`, or after `mailbox` and `resume`; one the hub
does not keep is sent there at once. A code naming no mailbox has its
payment printed to hand over as well, as has one no relay took: a payment
of many notes can be larger than the 64 KB relays commonly take, which
the warning then says. `mailbox` (or `collect`) receives
what the hubs keep for the wallet and what was sent to its mailbox; `watch`
keeps receiving and delivering, every `--every` seconds (60), until
stopped. Keep `watch` running, or schedule `deliver`, to send fallback mail
while the wallet is otherwise idle. `pay` does not mail older receipts:
a slow relay must not delay a new payment kept at its hub. Its change is
still unlinked before exit. Payments kept at the hub appear on Nostr only
if they missed the hour. Where the wallet holds
too little, it swaps what it lacks there first through a provider
announced on its hubs' relays, only if given `--max-fee` (all fees,
swap included): without it, it says what the payment would cost. A code
naming no hub the wallet trusts is paid so at a hub it says where to reach,
visited only if it proves it is the hub the code names: what the swap brings
past the payment stays there, apart from the hubs added, until `add-hub`:
`balance` shows it after the total, outside it and its `--json` rows.

Normal output stays short: `balance` shows the available amount and any
pending or uncounted funds; `balance --details` adds the full per-hub
breakdown. `pay` reports where the payment went (kept at the hub, or sent
to the mailbox) and the fee bound (sat and a percentage of the payment),
not whether the recipient has collected it yet; `deliver` says how many
payments are still on their way.
`pay --details` adds the payment ID and hub URL. Payments that must be
handed over still go unchanged to stdout or `--out`, with instructions on
stderr. Progress appears on an interactive terminal only. Diagnostic logs
are off unless requested with `--log text`, `--log json`, or `RUST_LOG`;
delivery failures and actionable warnings remain visible without logging.

A lock pays its payee on a secret's preimage until it expires, then returns
to the payer: the payee runs `secret` and hands over the hash, the payer
runs `lock <address> <amount> <hash>`, and the payee runs `claim <id>
<preimage>`. `resume` follows the payer's locks to their end.

Through a hub with a Lightning gateway, `pay-invoice <bolt11>` pays an
invoice with a lock refunded if the payment fails, and `invoice <amount>`
prints an invoice paying the wallet; `resume` takes the payment once made.
`pay-invoice <bolt11> --via <provider>` has a swap provider that pays
Lightning invoices pay it from its own node instead, for a lock of notes
at `--hub` to the provider, its fee on top (at most `--max-fee`, 1% and 100
sat by default). The lock expires soon after the provider's routes end: a
payment that fails or the provider declines leaves it to be refunded then.
The wallet records the provider with the lock, and `resume` asks the
provider again until the payment settles or fails, so a payment interrupted
before the provider heard of it is still made. Running it again for the
same invoice follows the same lock. The invoice is not paid through a hub's
gateway while a provider's lock pays it, nor the other way around. An invoice another wallet asked
its hub for pays that wallet notes there: notes at one hub pay notes at
another, with no notes of the provider at the other.

Through a hub taking deposits, `deposit <amount>` prints a BIP21 URI to pay
on Bitcoin, the hub's fee on top; `resume` takes the notes once the payment
confirmed. A deposit never received is taken back after its timeout with
`refund <hash> --to <address> --chain <url>`.

With `--sp`, the code also names the wallet's silent payment address and a
fallback address any wallet pays. `onchain --chain <url> scan` finds what
they pay with the tweaks of a hub serving them (fetching only the blocks
whose filters match from a node with `-blockfilterindex`). In blocks no
hub trusted serves tweaks of, it finds what the fallback addresses are
paid only, looking in them again for silent payments once a hub serves
their tweaks; with no such hub, the first scan needs `--from <height>`.
`onchain coins`
lists the coins, `onchain send <to> <amount>` pays an address, a silent
payment address or a code on Bitcoin, and `onchain move` deposits a coin
for notes at a hub taking deposits. Coins are never joined: a send spends
one coin alone (`--coin TXID:VOUT`, by default the largest, `--all` all of
it), and a move deposits one coin whole, leaving no change (`--coin`, by
default the largest), refused if the hub takes less than all of it.

A broadcast error can follow successful relay. The signed transaction and
its inputs stay reserved, and `onchain scan` rebroadcasts that same spend.
Do not make a replacement payment merely because the response was lost.
Existing pending spends need no conversion; transactions abandoned by an
older version cannot be reconstructed automatically.

`swap <amount> --to <hub> --provider <url>` swaps notes at another hub for
`amount` at `<hub>` through a swap provider, with a lock at each hub:
nothing is revealed until the provider's lock pays the wallet, and a swap
that fails leaves the wallet's lock to return once expired. `resume` takes
a pending swap on. Without `--provider`, the wallet takes one announced on
Nostr (on `--relay`, else the relays its hubs name) that swaps the amount
within `--max-fee` and `--max-margin` (200 blocks by default: what a
provider other than the hub swapped to needs at a hub of the usual
144-block window): one it swapped through before, else the cheapest. A
swap is refused before anything is locked unless the source hub admits a
lock that outlasts the provider's by that margin.

`payout <amount> --to <address> --chain <url>` pays
`amount` to an address on Bitcoin from notes at a hub, through a provider
paying out on chain: the wallet locks the amount and the fees to the
provider, which locks a coin under the lock's hash, and the wallet claims
that coin, paying the address exactly `amount`, once it finds it confirmed
on its own chain source. Nothing is revealed before; a payout that fails
leaves the lock to return once expired. `--max-fee` bounds everything on
top of the amount (the claim's fee, the provider's and the hub's). Without
`--provider`, the wallet takes one announced on Nostr (on `--relay`, else
the relays its hubs name) that pays the amount out within `--max-fee`: one
it paid out through before, else the cheapest.
`payouts --chain <url>` takes the payouts under way on and broadcasts their
claims: run it until the payout is done.

`open-contract <hub> --chain <url>` opens a recovery contract at a hub the
wallet trusts that opens them: its collateral protects the wallet's notes
there, up to its cap, until the contract's deadline. The wallet pays the
hub's price from its smallest coin on Bitcoin that covers it, its change
back to the wallet, and downloads about 100 MB of instances, kept in
`contracts/` next to the wallet file with `contracts.sqlite`: back them
up with it. `--blocks` sets the blocks to its close (by default the most
the hub allows), `--max-price` the most to pay (by default the price the
hub's info says). At a hub that lets wallets choose (its info names a
range), `--cap` with `--blocks` sets the most the contract protects, at the
price the range gives; `--coin TXID:VOUT` names the coin to pay from. Several contracts, at one hub or several, each take their
own coin. No contract opens at a hub charging more than is sane (over 10 sat
or 1% on a payment, lock or refund, over 1 sat for each note made, or over
20% of a chosen cap a year) unless `--any-fees` asks all the same.

The fees a hub charges as a contract opens there are the most the wallet
agreed to pay it, kept with the contract in `contracts.sqlite`. A hub may
raise its fees whenever it likes: while it charges more than agreed (at
several live contracts, the least of each fee), payments and locks there
are refused, its notes are not renewed, and `hubs` says so. `approve-fees
<hub>` asks the hub again and shows what it charges beside what was agreed;
`approve-fees <hub> --yes` agrees to what was last shown, each fee raised to
it and none lowered. Contracts opened before fees were kept with them hold
their hub to nothing, as does a hub with no live contract. `contracts` lists them; with `--chain`, it carries on those under
way, sees where each one's funding stands, and says what each live one
would claim of the wallet's notes now (`claims`, also in `--json`): the
notes at a hub are shared out among the contracts there so that together
they recover the most, the one ending first served first. On regtest, contracts are
under a mock proof authority: such a contract protects nothing. On signet
they are under the genuine programs, a trial: `force-close` below recovers
from them, not yet tried on signet itself (the hub's README says what else
it lacks). No other network for now.

`close <id> --chain <url>` closes a live contract early with its hub,
cooperatively: the hub and the wallet sign its fold together (MuSig2), which
pays the wallet's reserve back to it on Bitcoin at once and the rest to the
hub, less the fee, which the hub pays from its share. The contract protects
nothing from then on (`contracts` shows it `closed early`).

`force-close <id> --chain <url>` forces a live contract closed when its hub
stopped answering (`<id>` is the contract's id, or its start): its share
of the wallet's notes at that hub (as `contracts --chain` shows it) leaves
its balance and is claimed on Bitcoin for their value, the contract's cap
at most. A note worth more than the cap left is claimed for the cap when
nothing else recovers as much: its excess is lost. Until its request may be in the next block (a day
after the contract's funding on signet), `give-up-recovery <id>` gives
them back; from then on it is irrevocable. The wallet pays its fees from
its coins on Bitcoin at `--fee-rate` sat/vB (by default the chain's
estimate). `recoveries --chain <url>` carries every forced close on as
far as the chain lets it and shows the block its next step waits for: run
it until it is `done`, when the claimed notes' value and both reserves
are back in the wallet on Bitcoin. A hub that answers the request instead
(`answered`) says what the claim is owed, and must prove it on Bitcoin
before the block `recoveries` shows; otherwise the wallet's timeout pays
the whole claim. The wallet proves nothing: it checks the hub's proof once
asserted (`asserted`), challenges one that does not verify (`challenging`,
`challenged`), and carries the payout on to `done` (`succeeding`, `paid`,
`sweeping`), collecting what the outcome pays it and its reserve.

`discover --relay wss://relay.example` lists the hubs announced on Nostr,
at the URLs that answer as the hub that announced them; with `--swaps`, the
swap providers.

With `--tor`, the wallet reaches hubs, swap providers and Nostr relays,
onion services included, through a Tor client of its own (Arti), which
keeps its state in `tor/` next to the wallet file. `--proxy
socks5h://127.0.0.1:9050` goes through a Tor daemon instead.
