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
announced on Nostr to pick from, or takes one by its address. While a new
wallet first catches up with Bitcoin, Home lists each step and its
progress; later, its "Catching up" line opens them.

Tap a fee in a payment review or its history for the amount, percentage,
and supplied recipient split. Current direct-payment estimates show base
plus percentage times amount, rounded up like the hub. Payouts and moves
split their fee by who gets it, as quoted or recorded: the hub, the payout
or swap provider, and the Bitcoin network for a payout's claim. Historical
rates or splits that were not saved are identified as unavailable, never
reconstructed from today's terms.

Settings is one page: the hubs (each tapped for its fees, more added by
address or found on Nostr), Backup, Bitcoin (each step of catching up:
Tor, the nodes, the block headers and filters against the best peer's
height, and the wallet's payments from its birthday, with their progress),
Tor (on or off, and a page with its bootstrap progress, what it is doing,
why it seems stuck, and a confirmed reset forgetting only its state and
directory cache), About (the
version, the storage used and the logs to export) and advanced mode, whose
own page holds the notes, the log, the limit for renewing notes, the light
client's settings, resetting its chain state and deleting the wallet. How
the app looks and speaks, and the shared parts to build a screen of, are in
[../../DESIGN.md](../../DESIGN.md).

## Backup and restore

- **Recovery words**: 24 words, shown after a warning, kept out of
  screenshots. They encode the wallet's existing 256-bit seed as BIP39
  entropy (not a BIP39 PBKDF seed, so other wallets do not read them). They
  bring back the keys and coins on Bitcoin only: notes held at hubs and
  contracts need the backup file. They are shown with the wallet's birthday,
  the first block its payments are looked for from (learned on its first
  scan if not known when it was made); restoring with it scans from there,
  without it from block 1.
- **Backup file**: everything the wallet holds as of when it is saved (the
  wallet and contract databases, the birthday, the instances a contract's recovery needs,
  contacts), in a checksummed, unencrypted tar the user saves through the
  system file picker. Tor state, proofs made on the phone and the light
  client's blocks are left out. Payments after it are not in it. Restore a
  backup on one phone only: two live copies can reuse one-time recovery
  keys and lose money.
- **Restore**, from the first screen: from a backup file (its network is
  the wallet's) or from the recovery words (of the network chosen). It never
  lands over existing wallet state; a backup this build cannot carry on
  ("needs a newer version of the app") or one missing a file a recovery
  needs is refused before anything is installed.
- **Reset chain state**, in advanced mode, after a confirmation: the wallet
  looks for payments again from its first block and the light client's
  blocks are fetched afresh. Notes, contracts, recoveries and payments under
  way are untouched. Nothing runs on its own: existing wallets see no change
  until a backup, restore or reset is asked for.

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
beyond the fee authority. Apart from it, a job every 4 hours or so while
the phone rests (idle, with a network) syncs a round with Bitcoin, so coins
paid to the wallet are found without opening the app. History shows each
remint as "Optimized notes",
its fee alone. In advanced mode, a hub's sheet in Settings offers "Optimize
notes": a preview, then a remint within the fee shown, bypassing the rests
and the daily budget but never the contract ceilings.

The app needs a hub taking deposits on the same network: `licash-hub run
--network signet --chain URL --deposit-sweep-address ADDRESS`, with
`--nostr-relay` (or `--mailbox-relay`) for payments between wallets through
their mailboxes, `--tweaks` for silent payments: the app's code holds a
silent payment address only if a hub it uses serves tweaks, and `--lnd URL
--lnd-macaroon FILE` for Lightning: the app pays invoices through the hub,
and Receive, given an amount, shows an invoice the hub makes, whose
notes the app takes once paid. Without an amount Receive shows the
wallet's code (its hubs, and its silent payment address); an address on
Bitcoin any wallet pays, the code alone and the silent payment address
alone are under Other ways, the address handed out only once asked for.
Once a swap provider announced on the hubs' relays takes donations
(`licash-swaps run` with `--lnd` and `--lnurl-url URL`) at a hub of the code, Other ways also
offers a donation address: a static LNURL any Lightning wallet pays, again
and again, while the phone is offline, the provider paying each donation in
notes at the code's hub, less its fee. Once shown, its provider and code
are kept and shown again, even unreachable, as an address handed out stays:
the code while all its hubs are trusted, hubs added since or not; another
takes its place only once it takes no more donations, or has failed to for a
week while another can. Tapping a hub in Settings lists its fees. Silent payments
before the first block the hub indexed are not found; payments to the
fallback address are, from the day before the wallet was made.

Send pays Bitcoin addresses and silent payment addresses from notes too (a
`bitcoin:` code at its `sp=` address first), through a swap provider paying
out on Bitcoin (`licash-swaps run --chain URL`) announced on a relay a hub of
the app names (`--nostr-relay URL --announce-url URL`); a hub's page, behind
the balance, cashes out notes there to an address that way, all of them or
an amount chosen. A `licash:` code is paid in notes only, through a swap to
a hub it names if the app trusts none.

Lightning invoices are paid the cheapest way found: through a trusted hub's
gateway, or through a swap provider announced the same way that pays
invoices from its own node (`licash-swaps run --lnd URL --lnd-macaroon
FILE`) for a lock of notes at a hub holding enough. Gateways come first
among equal fees. The confirm screen names the provider and says how long
the sats wait should it decline or fail the payment: they return only once
the lock expires, about a day or more, where a hub's gateway refunds at once.
The app records the provider with the lock and asks it again on each round
until the payment settles, so one interrupted before the provider heard of
it is still made. In advanced mode, "All providers" lists every way to pay
it, to choose another.

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
now of the hub's notes once unfolded: what its recovery would claim of them
now, the notes at a hub shared out among its contracts so that together
they recover the most, those ending first served first. Notes still
arriving count as not protected until they settle. The hub's page says how
much of what it holds they protect; what they cover counts as protected,
on Home and behind the balance;
what a contract is worth for now (nothing, under the mock authority) is said
wherever one shows. The fees a hub charges as a contract opens there are
the most the app pays it from then on: a hub that raised them since is not
paid, and its notes not renewed, until the owner approves the new fees,
asked when a payment there is refused for it (old beside new, then paid)
and on the hub's page in Settings. A contract may be closed early with its
hub, never without what it covers: Close says what the hub pays, the notes
held there up to its cap and the reserve, in one transaction on Bitcoin,
and how much beyond the cap stays at the hub unprotected, which cashing out
first takes too. The hub is asked whether anything spent those notes, then
retires them as it signs the fold; they leave the balance as the close is
signed, and the row says it is closing until the fold is in a block. Should
the hub not fold, the app files the same claim on Bitcoin by itself a little
before its request's last block: a forced close on the notes already
claimed. A contract that locks owed to the app depend on, or one too near
its end, is forced closed instead; so is one whose hub does not take a claim
with its fold yet. In advanced mode, a contract whose hub stopped
answering can be forced closed: the notes it covers are claimed on Bitcoin
(a note worth more than its cap for the cap, its excess lost) and paid back
there once the hub's window to answer passed. A hub that answers instead
says what the claim is owed, and must prove it on Bitcoin by a deadline the
row shows: the app checks the hub's proof, challenges one that does not
verify, and is paid what the outcome says. Its publication's
fees come from the contract's reserve alone (paying more past it is allowed
on the command line only). A coin for a challenge's fee, when the wallet
holds none to set aside, is cashed out of notes at the richest hub to the
wallet's own silent payment address through a swap provider. The phone proves nothing, and no
root key of the wallet is revealed: the claim's notes are tied to no key
that owns the contract. The contracts are kept beside the wallet's file, made
only once one is asked for.

Coverage uses the actual whole-note recovery selection, not nominal caps:
only settled, unreserved notes of the opening's keyset, at most 32 or its
lower signed note limit. Notes are allocated once, earliest deadline first.
Genuine current-image contracts count; unreadable or unsupported openings count as zero. Balance details show the
total number of held notes.

Unfold a signed contract for its locked value, payout-pool and reserve
diagram, and a link to the contract address in the configured explorer.
The claim cap is a limit inside the payout pool, not extra collateral.
Technical details reveal the signed note limit, denominations, keyset,
graph version and program identities. Earlier opening stages show progress
rather than guessed signed details.
Answered recoveries show what the hub says is owed separately from the
original claim. Once done, the returned amount is the actual sweep output, including
the effect of reserves and fees, not the original claimed amount.

Before its request is signed, Cancel closing asks for confirmation and
releases the notes reserved by that attempt. It does not close the contract
or recover Bitcoin; another attempt is possible while the contract permits
it. Once signed, even an unconfirmed request cannot be canceled.

Advanced settings has a holder-owned recovery planning margin (14 estimated
days by default). An unfolded contract reads the signed terms to show when
its request should confirm. The margin alone authorizes nothing.
A late hub answer remains possible until the holder's timeout wins on
chain, so the margin requires online monitoring and prompt confirmation.

Below it, Exits asks for the holder's consent, given once a contract
protects her notes, with a Bitcoin address. With it, as a contract nears
its end (three days before its request should confirm) and no other
contract at its hub protects her notes there, the app pays those notes out
to that address through the cheapest provider within 1% and 1,000 sat,
then closes the contract early. Not paid out a day before, the contract is
forced closed. Without consent, including on upgraded installs and after
a restore (consent is not carried in a backup bundle), nothing is done:
a high-priority notification asks her to act. Revoking stops new steps;
those under way are finished. The consent keeps the exit margin given with
it; saving a new margin gives it again. While any contract protects notes
the periodic job looks every 15 minutes or so, and a disputed forced close
notifies her to keep the app online until its challenge window passes.
A lock of her notes at a hub where a contract protects them expires early
enough for that forced close to claim its refund should the hub never
settle it; a payment needing a later expiry (near the contract's end) is
refused there until a contract lasting longer protects her notes.
A lock paying her whose hub never issues it once told its preimage is
activated on Bitcoin with a coin of her wallet as its expiry nears, if a
contract of hers there can still claim it; that contract is then not
closed early but forced closed, its claim taking the lock's pay branch.

A counterpart on the command line:

```sh
export LICASH_WALLET_NETWORK=signet LICASH_WALLET_CHAIN=URL
licash-wallet init && licash-wallet add-hub HUB_URL
licash-wallet code                # for the app to scan and pay in notes
licash-wallet code --onchain      # for the app to pay on Bitcoin
licash-wallet mailbox             # the payments it sent
licash-wallet pay 'CODE' 1000     # pay the code the app shows
licash-wallet onchain scan        # coins paid on Bitcoin
```

## Logs and tests

Payout deadlines are checked for every pending payout, independently of
the ordinary progress display limit. A payment to a payment code is done
once its hub releases it, kept there for the payee, who collects it at the
hub and tells the payer so through it. One not collected within an hour,
or that its hub does not keep, is sent to the payee's mailbox by the app
in the background: the periodic job runs while any is on its way. The
wallet file records where each stands; a `deliveries.json` left by an
earlier version is no longer read.

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
