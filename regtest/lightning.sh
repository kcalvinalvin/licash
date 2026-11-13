#!/usr/bin/env bash
# Explicit slow gate with a private Docker network and disposable LND wallets.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$root/tmp"
run="$(mktemp -d "$root/tmp/lightning.XXXXXXXX")"
export LICASH_LN_NETWORK="licash-$(basename "$run")"
export LICASH_LN_CORE="$LICASH_LN_NETWORK-core"
export LICASH_LN_DIR="$run"
export LNGAP_BITCOIND="$root/regtest/bitcoind-lightning-docker.sh"
export LICASH_LND_IMAGE='lightninglabs/lnd:v0.21.6-beta@sha256:54b3c87ad0bc3fea3fab8af23ace6421147ea97e6b56bf3911592d7853c185b8'
cleanup() {
    docker rm -f "$LICASH_LN_CORE-payer" "$LICASH_LN_CORE-receiver" "$LICASH_LN_CORE" >/dev/null 2>&1 || true
    docker network rm "$LICASH_LN_NETWORK" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
docker image inspect "$LICASH_LND_IMAGE" >/dev/null 2>&1 || docker pull "$LICASH_LND_IMAGE"
# A unique bridge keeps the nodes separate. Core publishes RPC to host
# loopback only; an internal-only bridge breaks that path on rootless Docker.
# Neither LND publishes a host port or bootstraps public peers.
docker network create "$LICASH_LN_NETWORK" >/dev/null
printf 'Isolated Lightning test artifacts: %s\n' "$run"
if [ "$#" -eq 0 ]; then set -- --test native_lightning; fi
"$root/regtest/run.sh" "$@" -- --ignored --test-threads=1 --nocapture
