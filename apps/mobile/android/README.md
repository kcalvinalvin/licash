# Android app

The Licash wallet for Android: the wallet core (`apps/mobile/core`, Rust
over UniFFI) and the light client (`apps/mobile/chain`, Go over gomobile)
under a Jetpack Compose UI.

## What it shows

Home is the balance, how much of it is protected, one line while something
is going on, Receive and Send. The rest waits behind a tap: where the money
is behind the balance (a page for each hub, to cash out and protect, and
one for what is on Bitcoin), the history behind Activity (or a swipe up),
what a payment cost and how it was made behind its line, what a word means
behind its (i). Times are said as times, never as blocks; ids, notes and
providers show in advanced mode only. With no hub yet, Home lists the hubs
announced on Nostr to pick from, or takes one by its address.

Settings is one page: the hubs (each tapped for its fees, more added by
address or found on Nostr), Bitcoin's state, Tor, About (the version, the
storage used and the logs to export) and advanced mode, whose own page
holds the notes, the log, the limit for renewing notes, the light client's
settings and deleting the wallet. How the app looks and speaks, and the
shared parts to build a screen of, are in [../../DESIGN.md](../../DESIGN.md).

## Build

Needs a JDK 17 or later, the Android SDK (platform 37) with the NDK at
`ANDROID_NDK_HOME`, Rust with the `aarch64-linux-android` and
`x86_64-linux-android` targets, Go and gomobile.

```sh
./native.sh                 # the core, its Kotlin bindings and the chain AAR
./prover.sh                 # optional: the phone's prover and the programs it proves
./gradlew assembleDebug     # app/build/outputs/apk/debug/app-debug.apk
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

`./prover.sh` builds `r0/host`'s `licash-r0` for each ABI (about 60 MB
each) and copies `r0/programs`; with `SAMPLE=<statement input>`, it adds a
sample statement that advanced mode proves on the phone, to see how long a
proof takes there.

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
(1000 parts per million of them unless set, or a spending schedule of at
most 1 sat base, that rate and 0.1 sat per output; live contract ceilings
take precedence, under Privacy), the notes of
each denomination, and how each payment was made: its transactions, block
heights and providers.

Like the CLI, each sync reshapes the notes of a wallet that mostly
receives, within the same fee authority and the rolling 1% daily budget
(see [the wallet's README](../../wallet-cli/README.md)). While notes wait
out their privacy rest, a periodic job (about every 15 minutes, as Android
allows) keeps looking after the app is closed, without the foreground
service; it stops once nothing is left to do, or when what is left is
beyond the fee authority. History shows each remint as "Optimized notes",
its fee alone. In advanced mode, a hub's sheet in Settings offers "Optimize
notes": a preview, then a remint within the fee shown, bypassing the rests
and the daily budget but never the contract ceilings.

The app needs a hub taking deposits on the same network: `licash-hub run
--network signet --chain URL --deposit-sweep-address ADDRESS`, with
`--nostr-relay` (or `--mailbox-relay`) for payments between wallets through
their mailboxes, `--tweaks` for silent payments: the app's code holds a
silent payment address only if a hub it uses serves tweaks, and `--lnd URL
--lnd-macaroon FILE` for Lightning: the app pays invoices through the hub,
and Receive, switched to Lightning, shows an invoice the hub makes, whose
notes the app takes once paid. Tapping a hub in Settings lists its fees. Silent payments
before the first block the hub indexed are not found; payments to the
fallback address are, from the day before the wallet was made.

Send pays Bitcoin addresses from notes too, and codes naming no hub the app
trusts at their address, through a swap provider paying out on Bitcoin
(`licash-swaps run --chain URL`) announced on a relay a hub of the app names
(`--nostr-relay URL --announce-url URL`); a hub's page, behind the balance,
cashes out all the notes there to an address that way.

A code naming no hub the app trusts, but saying where to reach the payee's,
is paid there in notes once the owner agrees: the hub proves it is the one the
code names, a swap brings notes there from a hub of the app's, and the payment
shows its fees for review first. What the swap brought past the payment stays
at that hub, shown apart behind the balance, as not in it, where adding the
hub counts it.

A hub's page sets up protection, a recovery contract, at the hubs that open
them (`licash-hub run --contracts`: under the mock authority on regtest,
where such a contract protects nothing, and of genuine programs on signet).
A contract locks the hub's collateral on Bitcoin behind the notes there, up
to its cap, until its last block to recover in, then closes by itself, its
collateral back to the hub. Its price is paid from a coin of the wallet's on
Bitcoin, its change kept there; with none, the quote offers to cash notes
out to the wallet first. At a hub that lets the wallet choose
(`--contracts-max-cap`), sliders choose its cap (at first half again the
largest coin, at most the hub's `--contracts-per-coin` times the coin
paying) and how long it lasts (at first half a year), priced live as the hub
prices them, and the coin paying, if there are several; its change then
moves into the balance at that hub once its funding confirms, unless
switched off. Opening downloads about 100 MB and takes minutes to an hour,
in the background, one at a time, the step it is at shown; several
contracts at a hub, and at several hubs, add up. Each says what it covers
now of the hub's notes once unfolded, those ending first covering first,
and the hub's page how much of what it holds they protect; the notes they
cover count as protected rather than not, on Home and behind the balance;
what a contract is worth for now (nothing, under the mock authority) is said
wherever one shows. The fees a hub charges as a contract opens there are
the most the app pays it from then on: a hub that raised them since is not
paid, and its notes not renewed, until the owner approves the new fees,
asked when a payment there is refused for it (old beside new, then paid)
and on the hub's page in Settings. A contract ends no sooner: End says to cash out the
hub's notes instead. In advanced mode, a contract whose hub stopped
answering can be forced closed: the notes it covers are claimed on Bitcoin
and paid back there once the hub's window to answer passed. A hub that
answers instead leaves it to a proof of the claim, by a deadline the row
shows: it offers to save the statement to prove on a computer (`licash-r0`,
as [the wallet's README](../../wallet-cli/README.md) says) and to bring the
proof back, and, in an app built with `./prover.sh`, to prove it on the
phone itself, for hours, carried on after any stop; that proof's last step
(its Groth16 wrap) still needs an x86 computer with Docker, its receipt
saved from the row. The contracts are kept beside the wallet's file, made
only once one is asked for.

Advanced settings has a holder-owned recovery planning margin (14 estimated
days by default). An unfolded contract reads the signed terms to show when
its request should confirm. This does not authorize automatic closing.
A late hub answer remains possible until the holder's timeout wins on
chain, so the margin requires online monitoring and prompt confirmation.

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

Payout deadlines are checked for every pending payout, independently of
the ordinary progress display limit. An unreadable `deliveries.json`
stops mailbox-delivery updates without replacing the file; restore it from
a backup or inspect it before retrying. A missing file still means no
pending deliveries, and valid existing files need no conversion.

Settings, About, Export logs shares the app's log and the light client's as
one file; Storage shows what the app keeps on the phone: the wallet, the Bitcoin
light client (each network's in advanced mode), Tor's state, logs, caches and
the app itself. The end-to-end test drives the app on the emulator or phone adb
picks, among the regtest ecosystem of `apps/e2e`:

```sh
LICASH_E2E_APP=$PWD/app/build/outputs/apk/debug/app-debug.apk ../../e2e/run.sh the_app_is
```

It types as on a hardware keyboard: give the emulator one (`hw.keyboard =
yes` in its AVD's `config.ini`), as CI does, so that no on-screen keyboard
covers the buttons it taps.
