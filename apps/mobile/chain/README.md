# Chain access

The apps' view of Bitcoin without a full node: a BIP157/158 light client on
the official [neutrino](https://github.com/lightninglabs/neutrino) release,
for `bitcoin`, `signet`, `mutinynet` and `regtest`.

- `upstream/`: `materialize.sh` fetches neutrino v0.18.0, checks its content
  hash against the pin, applies `patches/*.patch` and writes
  `upstream/neutrino/` (gitignored), which `go.mod` builds from. The one
  patch lets peers relay transactions (`Config.MempoolEnabled`).
- `node/`: the light client. Given a [block-dn](https://github.com/guggero/block-dn)
  server, filters come from it by the file, each checked against filter
  headers fetched independently from peers before any of the file is
  stored. Block and filter headers never come from the CDN; peers serve
  missing or rejected filters too.
- Peers: neutrino's own discovery (DNS seeds filtered by service, then
  `addr`/`addrv2` gossip, onion addresses included through a proxy).
  Besides, `node/` remembers the peers it completed a handshake with in
  `good-peers.json` and a start reaches the latest four first; a node with
  no peer after 30 seconds reaches the network's fixed seeds, peers that
  offered compact filters when `cmd/seeds` generated
  `mobile/seeds_<network>.go`. Peers given explicitly are the only ones
  used: no book, no seeds. Each step of catching up is logged with the
  time it took (`phase: ...`).
- `mempool/`: unconfirmed transactions touching watched scripts and
  outpoints, as peers report them. Reports are hints: check what can be
  checked, `Node.Reject` what fails, and trust only confirmations.
- `mobile/`: the API the apps bind with gomobile.

## Build and test

Chain storage now lives in `peer-headers-v1/`. Legacy top-level header or
database files are refused, including after a downgrade; cached filters
from an older version cannot establish completeness of a wallet scan.
This is a breaking change for test installations: explicitly reset both
the test wallet's scan state and its chain data, or use a fresh test wallet
and data directory. On Android, clear application storage only after
saving anything needed from that test wallet. No files are deleted and no
rescan or conversion is started automatically on upgrade.

Needs Go (see `go.mod`), and Docker for the regtest tests.

```sh
upstream/materialize.sh          # once, and after changing the pin or patches
go test -race ./...
./regtest.sh                     # against a throwaway Bitcoin Core
```

Before a release, refresh the fixed seeds (from the DNS seeds, and from a
node's `getnodeaddresses` for onion ones, probed through Tor):

```sh
go run ./cmd/seeds -network bitcoin > mobile/seeds_bitcoin.go
go run ./cmd/seeds -network signet > mobile/seeds_signet.go
bitcoin-cli getnodeaddresses 0 | go run ./cmd/seeds -network bitcoin \
    -stdin -proxy 127.0.0.1:9050 > mobile/seeds_bitcoin.go
```
