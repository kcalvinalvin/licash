// Licash's chain access: a BIP157/158 light client on the official neutrino
// release, pinned and patched by upstream/materialize.sh.
module github.com/licash-org/licash/apps/mobile/chain

go 1.26.0

require (
	github.com/btcsuite/btcd/btcutil/v2 v2.0.0
	github.com/btcsuite/btcd/chaincfg/v2 v2.0.0
	github.com/btcsuite/btcd/chainhash/v2 v2.0.0
	github.com/btcsuite/btcd/wire/v2 v2.0.0
	github.com/btcsuite/btclog v1.0.0
	github.com/btcsuite/btcwallet/walletdb v1.6.0
	github.com/btcsuite/go-socks v0.0.0-20170105172521-4720035b7bfd
	github.com/lightninglabs/neutrino v0.18.0
)

require (
	github.com/aead/siphash v1.0.1 // indirect
	github.com/btcsuite/btcd v0.26.0 // indirect
	github.com/btcsuite/btcd/address/v2 v2.0.0 // indirect
	github.com/btcsuite/btcd/btcec/v2 v2.5.0 // indirect
	github.com/btcsuite/btcd/txscript/v2 v2.0.0 // indirect
	github.com/btcsuite/btcd/v2transport v1.0.1 // indirect
	github.com/btcsuite/btcwallet v0.16.18 // indirect
	github.com/btcsuite/btcwallet/wtxmgr v1.6.0 // indirect
	github.com/btcsuite/websocket v0.0.0-20150119174127-31079b680792 // indirect
	github.com/davecgh/go-spew v1.1.1 // indirect
	github.com/decred/dcrd/crypto/blake256 v1.1.0 // indirect
	github.com/decred/dcrd/dcrec/secp256k1/v4 v4.4.0 // indirect
	github.com/decred/dcrd/lru v1.1.3 // indirect
	github.com/kcalvinalvin/anet v0.0.0-20251112173137-d8ddc1f6dbee // indirect
	github.com/kkdai/bstream v1.0.0 // indirect
	github.com/lightninglabs/neutrino/cache v1.1.4 // indirect
	github.com/lightningnetwork/lnd/clock v1.0.1 // indirect
	github.com/lightningnetwork/lnd/fn/v2 v2.0.8 // indirect
	github.com/lightningnetwork/lnd/queue v1.0.1 // indirect
	github.com/lightningnetwork/lnd/ticker v1.1.1 // indirect
	github.com/pmezard/go-difflib v1.0.0 // indirect
	github.com/stretchr/objx v0.5.2 // indirect
	github.com/stretchr/testify v1.10.0 // indirect
	go.etcd.io/bbolt v1.3.11 // indirect
	golang.org/x/crypto v0.41.0 // indirect
	golang.org/x/exp v0.0.0-20250811191247-51f88131bc50 // indirect
	golang.org/x/mobile v0.0.0-20261011034528-197770444867 // indirect
	golang.org/x/mod v0.42.0 // indirect
	golang.org/x/sync v0.24.0 // indirect
	golang.org/x/sys v0.49.0 // indirect
	golang.org/x/tools v0.52.0 // indirect
	gopkg.in/yaml.v3 v3.0.1 // indirect
)

// The official v0.18.0 with the patches in upstream/patches, and nothing
// else. Run upstream/materialize.sh before building.
replace github.com/lightninglabs/neutrino => ./upstream/neutrino

tool golang.org/x/mobile/cmd/gobind
