# iOS app

The Licash wallet for iPhones: the same wallet core (`apps/mobile/core`,
Rust over UniFFI) and light client (`apps/mobile/chain`, Go over gomobile)
as the Android app, under a SwiftUI interface that shows and does what
Android's does. Its look and wording follow [apps/DESIGN.md](../../DESIGN.md),
its behavior [docs/android.md](../../../docs/android.md), with one
difference: out of sight, iOS runs the app only when it chooses (a
background task while a payment is under way, a refresh about every 15
minutes and a longer run every 4 hours at best), so a payment or a
protection with a deadline needs the app opened more often than on
Android.

## Try it

The `ios` workflow keeps `licash-unsigned.ipa`, the app for iPhones,
unsigned. Install it with a sideloading tool (AltStore, SideStore,
Sideloadly), which signs it with your own Apple ID.

## Build

You need a Mac with Xcode, XcodeGen, Rust with the `aarch64-apple-ios` and
`aarch64-apple-ios-sim` targets, Go and gomobile.

```sh
./native.sh            # the Rust core, its Swift bindings and the Go light client
xcodegen generate      # Licash.xcodeproj, from project.yml
open Licash.xcodeproj  # or: xcodebuild test -scheme Licash -destination 'platform=iOS Simulator,name=iPhone 16'
```
