#!/usr/bin/env bash
# bitcoind-compatible launcher for lichen-btc's isolated Regtest helper.
# Every invocation owns one throwaway container, never an existing node.
set -euo pipefail

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
    # Binding loopback first prevents the wildcard bind needed by Docker's
    # published port. Replace the helper's host-only RPC settings below.
    case "$arg" in
        -rpcbind=*|-rpcallowip=*) ;;
        *) args+=("$arg") ;;
    esac
done
if [[ "$regtest" != true || -z "$datadir" || ! -d "$datadir" ]]; then
    echo 'This launcher requires regtest and an existing isolated datadir.' >&2
    exit 2
fi

# The same version is used in CI. Override only to test another version.
image="${LICASH_BITCOIN_IMAGE:-bitcoin/bitcoin:30.0}"
name="licash-regtest-$$"
user="$(id -u):$(id -g)"
# In rootless Docker the container's root is the invoking host user. A
# container UID of 1000 instead maps to a subordinate, non-writing UID.
if docker info --format '{{json .SecurityOptions}}' | grep -q 'name=rootless'; then
    user='0:0'
fi
network=()
if [[ -n "$rpcport" ]]; then
    network=(--publish "127.0.0.1:$rpcport:$rpcport")
fi
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

docker run --rm --init --name "$name" "${network[@]}" \
    --user "$user" \
    --volume "$datadir:$datadir:z" \
    --entrypoint bitcoind "$image" "${args[@]}" \
    -rpcbind=0.0.0.0 -rpcallowip=0.0.0.0/0
