#!/usr/bin/env bash
# Build a guest ELF with the local RISC Zero toolchain (rzup's `risc0`
# rustup toolchain, cargo-risczero 3.1.x), then pack it with the zkOS kernel
# into r0/guests/<name>.bin and write its image id to r0/guests/<name>.id.
#
#   r0/build-guest.sh hp
#
# The flags are those of risczero/risc0-guest-builder (Lichen's
# r0/build-guest.sh); `getrandom_backend="custom"` makes rand_core's OsRng
# use the zkVM's randomness syscall.
#
# The programs contracts open under are kept by image id in r0/programs/
# (r0/README.md): rebuilt from the same commit with the pinned toolchains
# below, from any checkout, each comes out the same, image id and all.
set -euo pipefail
cd "$(dirname "$0")/.."
# The toolchains the programs kept were built with: another may build
# other bytes, and so another image id. LICASH_GUEST_ANY_TOOLCHAIN=1 builds
# with whichever is installed, for a guest not to be kept.
pinned_rustc=e638c6cfea1eff5fbbb24a27e60538e3760d21b8 # rzup rust 1.97.0
pinned_gcc="riscv32-unknown-elf-gcc (gc891d8dc23e) 13.2.0" # rzup cpp 2024.1.5
# Cargo hashes a path dependency outside a guest's workspace (crates/licash)
# by where it is, into the guest's symbols: so the guest is built from a
# copy of its sources at this path, the same on every machine, whichever
# the checkout's. One build at a time uses it.
build=/tmp/licash-r0
src="$build/licash"
flags="$(printf -- '-Cpasses=lower-atomic\x1f-Clink-arg=-Ttext=0x00200800\x1f-Clink-arg=--fatal-warnings\x1f-Cpanic=abort\x1f--cfg\x1fgetrandom_backend="custom"')"
# Keep the copy's directory, and cargo's where the dependencies are, out of
# the paths a guest holds (those its panics name).
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
flags="$(printf '%s\x1f--remap-path-prefix=%s=licash\x1f--remap-path-prefix=%s=cargo' "$flags" "$src" "$cargo_home")"
# rzup installs the toolchain under rustup's directory, with or without rustup itself.
tc="${RISC0_TOOLCHAIN:-$HOME/.rustup/toolchains/risc0}"
name="${1:-}"
case "$name" in
    hp) dir=hp-guest ;;
    *) echo 'usage: r0/build-guest.sh hp' >&2; exit 2 ;;
esac
# The guests link libsecp256k1 (taproot control blocks): compile its C with
# rzup's RISC-V gcc.
cc="${RISC0_CC:-$HOME/.risc0/cpp/bin/riscv32-unknown-elf-gcc}"
rustc_commit="$("$tc/bin/rustc" -vV | sed -n 's/^commit-hash: //p')"
gcc_version="$("$cc" --version | head -1)"
if [ "$rustc_commit" != "$pinned_rustc" ] || [ "$gcc_version" != "$pinned_gcc" ]; then
    echo "not the pinned toolchains: rustc $rustc_commit (pinned $pinned_rustc), $gcc_version (pinned $pinned_gcc)" >&2
    [ "${LICASH_GUEST_ANY_TOOLCHAIN:-}" = 1 ] || exit 1
    echo 'building anyway (LICASH_GUEST_ANY_TOOLCHAIN=1): not a guest to keep' >&2
fi
# The guest's sources as the checkout has them, uncommitted changes and
# all: the root manifest (crates/licash inherits from it), the crates the
# guest depends on, and the guest. Lichen's statement crate comes from git,
# at the revision the guest's Cargo.lock pins, under cargo's directory.
sources=(Cargo.toml crates/licash crates/licash-hp "r0/$dir")
if [ -n "$(git status --porcelain -- "${sources[@]}")" ]; then
    echo "uncommitted changes in the guest's sources: not a guest to keep" >&2
fi
mkdir -p "$build" r0/guests
exec 9>"$build/lock"
flock 9
rm -rf "$src"
mkdir "$src"
git ls-files -z --cached --others --exclude-standard -- "${sources[@]}" |
    tar --null -T - -cf - | tar -xf - -C "$src"
export CC_riscv32im_risc0_zkvm_elf="$cc" AR_riscv32im_risc0_zkvm_elf="${cc%gcc}ar"
export CFLAGS_riscv32im_risc0_zkvm_elf="-march=rv32im -nostdlib -ffile-prefix-map=$src=licash -ffile-prefix-map=$cargo_home=cargo"
CARGO_ENCODED_RUSTFLAGS="$flags" RISC0_FEATURE_bigint2= RUSTC="$tc/bin/rustc" CARGO_TARGET_DIR="$build/target" \
    "$tc/bin/cargo" build --locked --release --target riscv32im-risc0-zkvm-elf --manifest-path "$src/r0/$dir/Cargo.toml"
elf="$build/target/riscv32im-risc0-zkvm-elf/release/licash-$dir"
sha256sum "$elf"
cargo run --locked --release --manifest-path r0/host/Cargo.toml -- pack "$elf" "r0/guests/$name.bin" > "r0/guests/$name.id"
cat "r0/guests/$name.id"
