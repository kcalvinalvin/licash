#!/bin/sh
# Build what the app links into app/build/native: the wallet core for each
# ABI with its Kotlin bindings, and the chain module's AAR.
#
# Needs Rust with the aarch64-linux-android and x86_64-linux-android
# targets, the NDK at ANDROID_NDK_HOME, Go and gomobile. PROFILE=debug
# builds the core unoptimized.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
apps=$here/../..
out=$here/app/build/native
profile=${PROFILE:-release}
: "${ANDROID_NDK_HOME:?the NDK}"
api=26
bin=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin
target_dir=${CARGO_TARGET_DIR:-$apps/target}
flag=--release
[ "$profile" = release ] || flag=

mkdir -p "$out"
for pair in arm64-v8a:aarch64 x86_64:x86_64; do
	abi=${pair%%:*}
	arch=${pair#*:}
	triple=$arch-linux-android
	upper=$(echo "$arch" | tr a-z A-Z)
	export "CARGO_TARGET_${upper}_LINUX_ANDROID_LINKER=$bin/$triple$api-clang"
	export "CC_${arch}_linux_android=$bin/$triple$api-clang"
	export "CXX_${arch}_linux_android=$bin/$triple$api-clang++"
	export "AR_${arch}_linux_android=$bin/llvm-ar"
	export "RANLIB_${arch}_linux_android=$bin/llvm-ranlib"
	(cd "$apps" && cargo build --locked $flag -p licash-mobile --lib --target "$triple")
	mkdir -p "$out/jniLibs/$abi"
	cp "$target_dir/$triple/$profile/liblicash_mobile.so" "$out/jniLibs/$abi/"
done

rm -rf "$out/kotlin"
(cd "$apps" && cargo run --locked -p licash-mobile --features bindgen --bin uniffi-bindgen -- \
	generate --library "$out/jniLibs/arm64-v8a/liblicash_mobile.so" \
	--language kotlin --no-format --out-dir "$out/kotlin")

# Bindings are made from the libraries' symbols: strip them only now.
if [ "$profile" = release ]; then
	"$bin/llvm-strip" --strip-unneeded "$out"/jniLibs/*/liblicash_mobile.so
fi

chain=$apps/mobile/chain
[ -d "$chain/upstream/neutrino" ] || "$chain/upstream/materialize.sh"
(cd "$chain" && gomobile bind -target android/arm64,android/amd64 -androidapi $api \
	-javapkg org.licash.chain -o "$out/chain.aar" ./mobile)
rm -f "$out/chain-sources.jar"
