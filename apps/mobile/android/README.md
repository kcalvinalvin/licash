# Android app

The Licash wallet for Android: the wallet core (`apps/mobile/core`, Rust
over UniFFI) and the light client (`apps/mobile/chain`, Go over gomobile)
under a Jetpack Compose UI.

## Build

Needs a JDK 17 or later, the Android SDK (platform 37) with the NDK at
`ANDROID_NDK_HOME`, Rust with the `aarch64-linux-android` and
`x86_64-linux-android` targets, Go and gomobile.

```sh
./native.sh                 # the core, its Kotlin bindings and the chain AAR
./gradlew assembleDebug     # app/build/outputs/apk/debug/app-debug.apk
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

## Test networks

The app is for Bitcoin. Tapping the "licash" wordmark on the first screen
7 times, or the Advanced mode switch in Settings at any time, turns on
advanced mode: signet, Mutinynet and regtest, the light client's peers and
its block-dn server, the block explorer transactions link to, the notes of
each denomination, and every detail of a payment.

The app needs a hub taking deposits on the same network: `licash-hub run
--network signet --chain URL --deposit-sweep-address ADDRESS`, with
`--nostr-relay` (or `--mailbox-relay`) for payments between wallets through
their mailboxes, `--tweaks` for silent payments: the app's code holds a
silent payment address only if a hub it uses serves tweaks, and `--lnd URL
--lnd-macaroon FILE` for Lightning: the app pays invoices through the hub,
and Receive over Lightning shows an invoice the hub makes, whose notes the
app takes once paid. Tapping a hub in Settings lists its fees. Silent payments
before the first block the hub indexed are not found; payments to the
fallback address are, from the day before the wallet was made.

Send pays Bitcoin addresses from notes too, and codes naming no hub the app
trusts at their address, through a swap provider paying out on Bitcoin
(`licash-swaps run --chain URL`) announced on a relay a hub of the app names
(`--nostr-relay URL --announce-url URL`); Balance cashes out all the notes at
a hub to an address that way.

A code naming no hub the app trusts, but saying where to reach the payee's,
is paid there in notes once the owner agrees: the hub proves it is the one the
code names, a swap brings notes there from a hub of the app's, and the payment
shows its fees for review first. What the swap brought past the payment stays
at that hub, shown apart in Balance, where adding the hub (its URL shown to
confirm) counts it.

A counterpart on the command line:

```sh
export LICASH_WALLET_NETWORK=signet LICASH_WALLET_CHAIN=URL
licash-wallet init && licash-wallet add-hub HUB_URL
licash-wallet code --sp           # for the app to scan and pay
licash-wallet mailbox             # the payments it sent
licash-wallet pay 'CODE' 1000     # pay the code the app shows
licash-wallet onchain scan        # coins paid on Bitcoin
```

## Logs and tests

Settings, Export logs shares the app's log and the light client's as one
file; Storage shows what the app keeps on the phone: the wallet, the Bitcoin
light client (each network's in advanced mode), Tor's state, logs, caches and
the app itself. The end-to-end test drives the app on the emulator or phone adb
picks, among the regtest ecosystem of `apps/e2e`:

```sh
LICASH_E2E_APP=$PWD/app/build/outputs/apk/debug/app-debug.apk ../../e2e/run.sh the_app_is
```
