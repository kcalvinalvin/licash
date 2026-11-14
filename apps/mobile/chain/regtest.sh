#!/bin/sh
# Run the chain module's regtest tests against a throwaway Bitcoin Core.
#
#   regtest.sh [go test arguments]
#
# Core serves compact filters and listens on a loopback port Docker picks,
# so runs do not collide with each other or with anything else on the host.
set -eu

# Bitcoin Core 30.0, by digest.
IMAGE=${LICASH_BITCOIN_IMAGE:-bitcoin/bitcoin@sha256:68b927b6a2d3b019ce7655f3fd0eb9a4c7011310886ebc7890e5246c16df5ec6}

here=$(cd "$(dirname "$0")" && pwd)
name="licash-chain-$$"

"$here/upstream/materialize.sh" --check >/dev/null 2>&1 || "$here/upstream/materialize.sh"

docker run -d --rm --name "$name" -p 127.0.0.1::18444 "$IMAGE" \
	-regtest -listen=1 -bind=0.0.0.0:18444 \
	-blockfilterindex=1 -peerblockfilters=1 -fallbackfee=0.0002 >/dev/null
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT INT TERM

# As the user Core runs as: its RPC cookie is in that home.
cli() { docker exec -u bitcoin "$name" bitcoin-cli -regtest "$@"; }
tries=0
until cli getblockchaininfo >/dev/null 2>&1; do
	tries=$((tries + 1))
	[ "$tries" -lt 120 ] || {
		echo "regtest: Bitcoin Core did not start" >&2
		docker logs "$name" 2>&1 | tail -20 >&2
		exit 1
	}
	sleep 0.5
done
cli createwallet chain >/dev/null
cli -rpcwallet=chain generatetoaddress 101 "$(cli -rpcwallet=chain getnewaddress)" >/dev/null

p2p=$(docker port "$name" 18444/tcp | head -1)
cd "$here"
LICASH_BITCOIND_CONTAINER="$name" LICASH_BITCOIND_P2P="$p2p" \
	go test -tags regtest -count=1 -p 1 -run '^TestRegtest' ./node/ ./mobile/ "$@"
