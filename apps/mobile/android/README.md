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

## Releases

Once every CI job passes on main, CI publishes the app signed, as
`licash-main.apk` with its `SHA256SUMS`, on the `main-build` prerelease,
replaced on each push. The `android` job builds the release APK unsigned;
only `publish-app` signs it, with the key in two repository secrets: the
debug key the app has been signed with so far, from the machine that built
it, so that the published app installs as an update over those builds (its
certificate's SHA-256 is `0bfd0c83...d474c1e2`, as `apksigner verify
--print-certs` shows):

```sh
base64 -w0 ~/.android/debug.keystore | gh secret set ANDROID_KEYSTORE
gh secret set ANDROID_KEYSTORE_PASSWORD --body android
```

Android updates an app only with an APK signed by the same key: keep that
keystore backed up, since without it every user must uninstall (and restore
the wallet from its backup). Its password is the debug default, so whoever
has the file can sign updates.

## Test networks

The app is for Bitcoin. Tapping the "licash" wordmark on the first screen
7 times, or the Advanced mode switch in Settings at any time, turns on
advanced mode: signet, Mutinynet and regtest, the light client's peers and
its block-dn server, the block explorer transactions link to, the most the
app pays, on its own, to renew notes a hub that restarted no longer unlinks
(100 parts per million of them unless set, under Privacy), the notes of
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

Balance opens recovery contracts at the hubs that open them (`licash-hub run
--contracts`, on regtest only for now, under the mock authority: such a
contract protects nothing). A contract locks the hub's collateral on Bitcoin
behind the notes there, up to its cap, until its last block to recover in,
then closes by itself, its collateral back to the hub. Its price is paid from
a coin of the wallet's on Bitcoin, its change kept there; with none, the quote
offers to cash notes out to the wallet first. Opening downloads about 100 MB
and takes minutes, in the background, one at a time; several contracts at a
hub, and at several hubs, add up. The notes contracts cover count as
protected rather than on credit, in Balance and on Home. A contract closes no
sooner: Close says to cash out the hub's notes instead. Force closing,
recovering the notes on Bitcoin, is not available yet. The contracts are kept
beside the wallet's file, made only once one is asked for.

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

It types as on a hardware keyboard: give the emulator one (`hw.keyboard =
yes` in its AVD's `config.ini`), as CI does, so that no on-screen keyboard
covers the buttons it taps.
