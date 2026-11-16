#!/bin/sh
# Build the phone's prover into app/build/prover, for an app that proves a
# forced close's claim on the phone itself. Optional: an app built without
# it proves on a computer only.
#
# - jniLibs/<abi>/liblicash_r0.so: r0/host's licash-r0 for each ABI, run
#   as a process of its own (named as a library for Android to install it
#   where it may run), with the C++ runtime it links.
# - assets/r0/programs/<image id>.bin: the programs contracts name, kept in
#   r0/programs.
# - SAMPLE=<statement input> adds it as assets/r0/sample/statement.input
#   with GUEST=<packed guest> [default: r0/guests/native-claim.bin], for a
#   developer to see how long a proof takes on the phone.
#
# Needs Rust with the aarch64-linux-android and x86_64-linux-android
# targets and the NDK at ANDROID_NDK_HOME. Run after ../native.sh, before
# gradle.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$here/../../..
out=$here/app/build/prover
: "${ANDROID_NDK_HOME:?the NDK}"
api=26
bin=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin
sysroot=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib
target_dir=${CARGO_TARGET_DIR:-$repo/r0/host/target}

rm -rf "$out"
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
	cargo build --locked --release --manifest-path "$repo/r0/host/Cargo.toml" \
		--target "$triple" --target-dir "$target_dir"
	mkdir -p "$out/jniLibs/$abi"
	"$bin/llvm-strip" --strip-all -o "$out/jniLibs/$abi/liblicash_r0.so" \
		"$target_dir/$triple/release/licash-r0"
	cp "$sysroot/$triple/libc++_shared.so" "$out/jniLibs/$abi/"
done

mkdir -p "$out/assets/r0/programs"
cp "$repo"/r0/programs/*.bin "$out/assets/r0/programs/"
if [ -n "${SAMPLE:-}" ]; then
	mkdir -p "$out/assets/r0/sample"
	cp "$SAMPLE" "$out/assets/r0/sample/statement.input"
	cp "${GUEST:-$repo/r0/guests/native-claim.bin}" "$out/assets/r0/sample/native-claim.bin"
fi
ls -l "$out"/jniLibs/*/
