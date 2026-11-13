#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$root/tmp/regtest"
export TMPDIR="$root/tmp/regtest"
export LNGAP_BITCOIND="${LNGAP_BITCOIND:-$root/regtest/bitcoind-docker.sh}"

if [[ "$LNGAP_BITCOIND" == "$root/regtest/bitcoind-docker.sh" ]]; then
    image="${LICASH_BITCOIN_IMAGE:-bitcoin/bitcoin:30.0}"
    docker image inspect "$image" >/dev/null 2>&1 || docker pull "$image"
    docker image inspect "$image" --format 'Bitcoin image: {{.Id}}'
fi

cd "$root"
exec cargo test --release --locked --manifest-path regtest/Cargo.toml "$@"
