#!/bin/sh
# Build what the app links from build/native: the wallet core for the
# device and the simulator with its Swift bindings, and the chain module's
# XCFramework.
#
# Needs Xcode, Rust with the aarch64-apple-ios and aarch64-apple-ios-sim
# targets, Go and gomobile. PROFILE=debug builds the core unoptimized.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
apps=$here/../..
out=$here/build/native
profile=${PROFILE:-release}
target_dir=${CARGO_TARGET_DIR:-$apps/target}
flag=--release
[ "$profile" = release ] || flag=
# The deployment target of project.yml, for the C the core compiles too.
export IPHONEOS_DEPLOYMENT_TARGET=17.0

mkdir -p "$out"
for triple in aarch64-apple-ios aarch64-apple-ios-sim; do
	(cd "$apps" && cargo rustc --locked $flag -p licash-mobile --lib --crate-type staticlib --target "$triple")
done
device=$target_dir/aarch64-apple-ios/$profile/liblicash_mobile.a
simulator=$target_dir/aarch64-apple-ios-sim/$profile/liblicash_mobile.a

rm -rf "$out/swift" "$out/headers" "$out/LicashCoreFFI.xcframework"
(cd "$apps" && cargo run --locked -p licash-mobile --features bindgen --bin uniffi-bindgen -- \
	generate --library "$device" --language swift --no-format --out-dir "$out/swift")
# The C half of the bindings, as the module an XCFramework's headers hold.
mkdir "$out/headers"
cp "$out/swift/LicashCoreFFI.h" "$out/headers/"
cp "$out/swift/LicashCoreFFI.modulemap" "$out/headers/module.modulemap"
xcodebuild -create-xcframework \
	-library "$device" -headers "$out/headers" \
	-library "$simulator" -headers "$out/headers" \
	-output "$out/LicashCoreFFI.xcframework"

chain=$apps/mobile/chain
[ -d "$chain/upstream/neutrino" ] || "$chain/upstream/materialize.sh"
rm -rf "$out/Chain.xcframework"
(cd "$chain" && gomobile bind -target ios/arm64,iossimulator/arm64 -iosversion 17.0 \
	-o "$out/Chain.xcframework" ./mobile)
