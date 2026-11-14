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

A payment is JSON that only its payee can open: hand it over by any
channel. `send` and `resume` exit with 75 when a payment is recorded but its
hub has not answered; `resume` finishes it. Back up the wallet file: its
seed alone does not restore its notes. After restoring an old copy, `check`
drops the notes spent since.

A lock pays its payee on a secret's preimage until it expires, then returns
to the payer: the payee runs `secret` and hands over the hash, the payer
runs `lock <address> <amount> <hash>`, and the payee runs `claim <id>
<preimage>`. `resume` follows the payer's locks to their end.

Through a hub with a Lightning gateway, `pay-invoice <bolt11>` pays an
invoice with a lock refunded if the payment fails, and `invoice <amount>`
prints an invoice paying the wallet; `resume` takes the payment once made.

Onion hubs need a SOCKS proxy for now: `--proxy socks5h://127.0.0.1:9050`.
