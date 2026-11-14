# Chain access

The apps' view of Bitcoin without a full node: a BIP157/158 light client on
the official [neutrino](https://github.com/lightninglabs/neutrino) release,
for `bitcoin`, `signet`, `mutinynet` and `regtest`.

- `upstream/`: `materialize.sh` fetches neutrino v0.18.0, checks its content
  hash against the pin, applies `patches/*.patch` and writes
  `upstream/neutrino/` (gitignored), which `go.mod` builds from. The one
  patch lets peers relay transactions (`Config.MempoolEnabled`).
- `node/`: the light client. Given a [block-dn](https://github.com/guggero/block-dn)
  server, a new or far-behind node imports its headers in whole files, and
  filters come from it by the file, each checked against the filter header
  before it is stored; peers serve whatever it cannot.
- `mempool/`: unconfirmed transactions touching watched scripts and
  outpoints, as peers report them. Reports are hints: check what can be
  checked, `Node.Reject` what fails, and trust only confirmations.
- `mobile/`: the API the apps bind with gomobile.

## Build and test

Needs Go (see `go.mod`), and Docker for the regtest tests.

```sh
upstream/materialize.sh          # once, and after changing the pin or patches
go test -race ./...
./regtest.sh                     # against a throwaway Bitcoin Core
```
