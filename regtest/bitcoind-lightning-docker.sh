#!/usr/bin/env bash
# Extra ZMQ/network wiring for the explicit Lightning gate only. Never attach
# LND to a user's node or change the ordinary regtest launcher's behavior.
set -euo pipefail
: "${LICASH_LN_NETWORK:?run regtest/lightning.sh}"
: "${LICASH_LN_CORE:?run regtest/lightning.sh}"
: "${LICASH_LN_DIR:?run regtest/lightning.sh}"
exec >"$LICASH_LN_DIR/core.log" 2>&1
datadir=''
rpcport=''
regtest=false
args=()
for arg in "$@"; do
    case "$arg" in
        -datadir=*) datadir="${arg#-datadir=}" ;;
        -rpcport=*) rpcport="${arg#-rpcport=}" ;;
        -regtest|-regtest=1) regtest=true ;;
    esac
    case "$arg" in
        -rpcbind=*|-rpcallowip=*) ;;
        *) args+=("$arg") ;;
    esac
done
if [[ "$regtest" != true || -z "$datadir" || ! -d "$datadir" || -z "$rpcport" ]]; then
    echo 'Lightning launcher requires an isolated regtest datadir and RPC port.' >&2
    exit 2
fi
user="$(id -u):$(id -g)"
if docker info --format '{{json .SecurityOptions}}' | grep -q 'name=rootless'; then user='0:0'; fi
cleanup() { docker rm -f "$LICASH_LN_CORE" >/dev/null 2>&1 || true; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
docker run --rm --init --name "$LICASH_LN_CORE" --network "$LICASH_LN_NETWORK" --network-alias core \
    --publish "127.0.0.1:$rpcport:$rpcport" --user "$user" --volume "$datadir:$datadir:z" \
    --entrypoint bitcoind "${LICASH_BITCOIN_IMAGE:-bitcoin/bitcoin:30.0}" "${args[@]}" \
    -rpcbind=0.0.0.0 -rpcallowip=0.0.0.0/0 \
    -zmqpubrawblock=tcp://0.0.0.0:28332 -zmqpubrawtx=tcp://0.0.0.0:28333
