#!/usr/bin/env bash
# Build a guest ELF with the local RISC Zero toolchain (rzup's `risc0`
# rustup toolchain, cargo-risczero 3.0.x), then pack it with the zkOS kernel
# into r0/guests/<name>.bin and write its image id to r0/guests/<name>.id.
#
#   r0/build-guest.sh native-counter|native-claim
#
# The flags are those of risczero/risc0-guest-builder (Lichen's
# r0/build-guest.sh); `getrandom_backend="custom"` makes rand_core's OsRng
# use the zkVM's randomness syscall.
set -euo pipefail
cd "$(dirname "$0")/.."
flags="$(printf -- '-Cpasses=lower-atomic\x1f-Clink-arg=-Ttext=0x00200800\x1f-Clink-arg=--fatal-warnings\x1f-Cpanic=abort\x1f--cfg\x1fgetrandom_backend="custom"')"
# Keep the checkout directory's former name out of committed guest paths.
flags="$(printf '%s\x1f--remap-path-prefix=%s=licash' "$flags" "$PWD")"
# rzup installs the toolchain under rustup's directory, with or without rustup itself.
tc="${RISC0_TOOLCHAIN:-$HOME/.rustup/toolchains/risc0}"
name="${1:-}"
case "$name" in
    native-claim) dir=native-guest ;;
    native-counter) dir=native-counter-guest ;;
    *) echo 'usage: r0/build-guest.sh native-counter|native-claim' >&2; exit 2 ;;
esac
# The guests link libsecp256k1 (taproot control blocks): compile its C with
# rzup's RISC-V gcc.
cc="${RISC0_CC:-$HOME/.risc0/cpp/bin/riscv32-unknown-elf-gcc}"
export CC_riscv32im_risc0_zkvm_elf="$cc" AR_riscv32im_risc0_zkvm_elf="${cc%gcc}ar"
export CFLAGS_riscv32im_risc0_zkvm_elf="-march=rv32im -nostdlib"
mkdir -p r0/guests
CARGO_ENCODED_RUSTFLAGS="$flags" RISC0_FEATURE_bigint2= RUSTC="$tc/bin/rustc" \
    "$tc/bin/cargo" build --release --target riscv32im-risc0-zkvm-elf --manifest-path "r0/$dir/Cargo.toml"
elf="r0/$dir/target/riscv32im-risc0-zkvm-elf/release/licash-$name"
sha256sum "$elf"
cargo run --release --manifest-path r0/host/Cargo.toml -- pack "$elf" "r0/guests/$name.bin" > "r0/guests/$name.id"
cat "r0/guests/$name.id"
