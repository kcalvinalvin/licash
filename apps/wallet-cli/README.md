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
value (`LICASH_WALLET_RENEW_PPM`, 100 by default, 0 for free renewals
only). `history` lists a renewal as a payment to the wallet's own address.

`code` prints a payment code (a BIP321 URI, `--qr` for QR codes) naming the
wallet, the hubs it is paid at and its Nostr mailbox, if they name relays.
`pay <code> [amount]` pays it at a hub it names that the wallet trusts and
holds enough at, and sends the payment to the mailbox, else prints it to
hand over; `mailbox` receives what was sent there. Where the wallet holds
too little, it swaps what it lacks there first through a provider
announced on its hubs' relays, only if given `--max-fee` (all fees,
swap included): without it, it says what the payment would cost. A code
naming no hub the wallet trusts is paid so at a hub it says where to reach,
visited only if it proves it is the hub the code names: what the swap brings
past the payment stays there, apart from the hubs added, until `add-hub`:
`balance` shows it after the total, outside it and its `--json` rows.

Normal output stays short: `balance` shows the available amount and any
pending or uncounted funds; `balance --details` adds the full per-hub
breakdown. `pay` reports mailbox delivery and the fee bound (sat and a
percentage of the payment), not whether the recipient has collected it.
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
payment address or a code on Bitcoin, and `onchain move` deposits the coins
for notes at a hub taking deposits.

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
own coin. `contracts` lists them; with `--chain`, it carries on those under
way and sees where each one's funding stands. On regtest, contracts are
under a mock proof authority: such a contract protects nothing. On signet
they are under the genuine programs, a trial: `force-close` below recovers
from them, not yet tried on signet itself (the hub's README says what else
it lacks). No other network for now.

`force-close <id> --chain <url>` forces a live contract closed when its hub
stopped answering (`<id>` is the contract's id, or its start): the wallet's
notes at that hub, up to the contract's cap, leave its balance and are
claimed on Bitcoin. Until its request may be in the next block (a day
after the contract's funding on signet), `give-up-recovery <id>` gives
them back; from then on it is irrevocable. The wallet pays its fees from
its coins on Bitcoin at `--fee-rate` sat/vB (by default the chain's
estimate). `recoveries --chain <url>` carries every forced close on as
far as the chain lets it and shows the block its next step waits for: run
it until it is `done`, when the claimed notes' value and both reserves
are back in the wallet on Bitcoin. A hub that answers the request instead
(`answered`) leaves it to the wallet's proof of its claim: once the request
and its publication are deep enough, the forced close is `proving`, and
`recovery-statement <id>` writes the statement to prove (it shows the
wallet's notes: keep it private) with the commands that prove it with
`licash-r0` on a machine with 16 GB of memory, the Groth16 step on x86
with Docker. `import-proof <id> proof.json --chain <url>` brings the proof
back before the block `recoveries` shows: checked, kept, and asserted on
Bitcoin at once. `recoveries --chain <url>` then carries it on to `done`
(`asserted`, `succeeding`, `paid`, `sweeping`), answering the hub if it
challenges the proof. A hub's counter is not answered yet.

`discover --relay wss://relay.example` lists the hubs announced on Nostr,
at the URLs that answer as the hub that announced them; with `--swaps`, the
swap providers.

With `--tor`, the wallet reaches hubs, swap providers and Nostr relays,
onion services included, through a Tor client of its own (Arti), which
keeps its state in `tor/` next to the wallet file. `--proxy
socks5h://127.0.0.1:9050` goes through a Tor daemon instead.
