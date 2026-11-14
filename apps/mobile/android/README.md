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

The app is for Bitcoin. Tapping the "licash" wordmark on the first screen,
or the version in Settings, 7 times turns on developer mode: signet,
Mutinynet and regtest, the light client's peers and its block-dn server,
the block explorer transactions link to, and every detail of a payment.

The app needs a hub taking deposits on the same network: `licash-hub run
--network signet --chain URL --deposit-sweep-address ADDRESS`, with
`--nostr-relay` (or `--mailbox-relay`) for payments between wallets through
their mailboxes, and `--tweaks` for silent payments: the app's code holds a
silent payment address only if a hub it uses serves tweaks. Silent payments
before the first block the hub indexed are not found; payments to the
fallback address are, from the day before the wallet was made.

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
file. The end-to-end test drives the app on the emulator or phone adb
picks, among the regtest ecosystem of `apps/e2e`:

```sh
LICASH_E2E_APP=$PWD/app/build/outputs/apk/debug/app-debug.apk ../../e2e/run.sh the_app_is
```
