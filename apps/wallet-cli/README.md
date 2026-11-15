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
whose filters match from a node with `-blockfilterindex`); `onchain coins`
lists the coins, `onchain send <to> <amount>` pays an address, a silent
payment address or a code on Bitcoin, and `onchain move` deposits the coins
for notes at a hub taking deposits.

`swap <amount> --to <hub> --provider <url>` swaps notes at another hub for
`amount` at `<hub>` through a swap provider, with a lock at each hub:
nothing is revealed until the provider's lock pays the wallet, and a swap
that fails leaves the wallet's lock to return once expired. `resume` takes
a pending swap on. Without `--provider`, the wallet takes one announced on
Nostr (on `--relay`, else the relays its hubs name) that swaps the amount
within `--max-fee` and `--max-margin`: one it swapped through before, else
the cheapest.

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

`discover --relay wss://relay.example` lists the hubs announced on Nostr,
at the URLs that answer as the hub that announced them; with `--swaps`, the
swap providers.

With `--tor`, the wallet reaches hubs, swap providers and Nostr relays,
onion services included, through a Tor client of its own (Arti), which
keeps its state in `tor/` next to the wallet file. `--proxy
socks5h://127.0.0.1:9050` goes through a Tor daemon instead.
