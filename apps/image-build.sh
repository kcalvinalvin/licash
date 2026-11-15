#!/bin/sh
# An image's binaries, for its build stage: PACKAGE:BINARY pairs of the
# applications' workspace, built from the repository root into
# /usr/local/bin.
#
# The build stage runs on the builder's own platform (FROM
# --platform=$BUILDPLATFORM) and cross-compiles for the image's,
# TARGETPLATFORM, when it is another: under QEMU, 32-bit ARM takes hours.
set -eu
packages=
binaries=
for pair; do
	packages="$packages -p ${pair%%:*}"
	binaries="$binaries ${pair#*:}"
done

host=$(rustc -vV | sed -n 's/^host: //p')
case ${TARGETPLATFORM:-} in
"") target=$host ;;
linux/amd64) target=x86_64-unknown-linux-gnu gnu=x86_64-linux-gnu gcc=x86-64-linux-gnu libc=amd64 ;;
linux/arm64 | linux/arm64/v8) target=aarch64-unknown-linux-gnu gnu=aarch64-linux-gnu gcc=aarch64-linux-gnu libc=arm64 ;;
linux/arm/v7) target=armv7-unknown-linux-gnueabihf gnu=arm-linux-gnueabihf gcc=arm-linux-gnueabihf libc=armhf ;;
*)
	echo "image-build.sh: no build for $TARGETPLATFORM" >&2
	exit 1
	;;
esac

out=apps/target/release
if [ "$target" != "$host" ]; then
	apt-get update
	apt-get install -y --no-install-recommends "gcc-$gcc" "libc6-dev-$libc-cross"
	rustup target add "$target"
	vars=$(echo "$target" | tr - _)
	export CARGO_BUILD_TARGET="$target" \
		"CARGO_TARGET_$(echo "$vars" | tr '[:lower:]' '[:upper:]')_LINKER=$gnu-gcc" \
		"CC_$vars=$gnu-gcc" "AR_$vars=$gnu-ar"
	out=apps/target/$target/release
fi

# shellcheck disable=SC2086 # a word per flag
cargo build --locked --release --manifest-path apps/Cargo.toml $packages
for binary in $binaries; do
	cp "$out/$binary" /usr/local/bin/
done
