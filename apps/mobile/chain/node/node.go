// Package node runs a BIP157/158 light client on the pinned official
// neutrino and watches the mempool through its peers.
package node

import (
	"context"
	"fmt"
	"log/slog"
	"net"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/btcsuite/btcwallet/walletdb"
	_ "github.com/btcsuite/btcwallet/walletdb/bdb" // the "bdb" driver
	"github.com/lightninglabs/neutrino"
	"github.com/lightninglabs/neutrino/banman"

	"github.com/licash-org/licash/apps/mobile/chain/mempool"
)

// Config is what a Node needs to start.
type Config struct {
	// DataDir holds headers, filters and known peers. It is created if
	// missing. Everything in it can be rebuilt from the network.
	DataDir string
	Params  chaincfg.Params
	// ConnectPeers, if any, are the only peers used.
	ConnectPeers []string
	// AddPeers are used along with the ones discovered.
	AddPeers []string
	// Mempool bounds what peers can make the node hold.
	Mempool mempool.Limits
	// CDN, if set, is the base URL of a block-dn server following the same
	// chain: headers are imported from it while the node is far behind, and
	// filters fetched from it by the file, each checked against its header.
	// The peers serve whatever it cannot.
	CDN string
	// Proxy, if set, is the SOCKS5 proxy everything is reached through:
	// the peers, the names resolved and the CDN.
	Proxy *Proxy
	// Log receives the node's own events; nothing if nil.
	Log *slog.Logger
}

// Mempool is a consumer's view of unconfirmed transactions: what to watch
// and what peers have reported for it. Reports are hints, see the mempool
// package. A report that fails the consumer's checks goes to Node.Reject.
type Mempool interface {
	WatchScript(pkScript []byte)
	UnwatchScript(pkScript []byte)
	WatchOutpoint(op wire.OutPoint)
	UnwatchOutpoint(op wire.OutPoint)
	Spends(op wire.OutPoint) []mempool.Seen
	Pays(pkScript []byte) []mempool.Seen
	Forget(txid chainhash.Hash)
	Stats() mempool.Stats
}

// Node is a running light client.
type Node struct {
	cs    *neutrino.ChainService
	db    walletdb.DB
	relay *relay
	cdn   *cdn

	stopOnce sync.Once
	stopErr  error
}

// Start opens the node's data and begins syncing. Peers are asked to relay
// transactions from the first connection on, whether or not anything is
// watched, so the handshake does not show when a wallet starts watching.
func Start(cfg Config) (*Node, error) {
	log := cfg.Log
	if log == nil {
		log = slog.New(slog.DiscardHandler)
	}
	if err := os.MkdirAll(cfg.DataDir, 0o700); err != nil {
		return nil, fmt.Errorf("creating the data directory: %w", err)
	}
	var server *cdn
	var imp *neutrino.HeadersImportConfig
	if cfg.CDN != "" {
		var err error
		if server, err = newCDN(cfg.CDN, &cfg.Params, cfg.Proxy, log); err != nil {
			log.Warn("block-dn unusable, syncing from peers", "err", err)
		} else {
			var remove func()
			imp, remove, err = server.importFiles(cfg.DataDir, heldHeaders(cfg.DataDir))
			if err != nil {
				log.Warn("downloading headers failed, syncing from peers", "err", err)
			}
			defer remove()
		}
	}
	n, err := start(cfg, imp)
	if err != nil && imp != nil {
		log.Warn("importing headers failed, syncing from peers", "err", err)
		n, err = start(cfg, nil)
	}
	if err != nil {
		return nil, err
	}
	n.cdn = server
	log.Info("light client started", "network", cfg.Params.Name,
		"connect", cfg.ConnectPeers, "add", cfg.AddPeers, "cdn", server != nil)
	return n, nil
}

// start runs the node, first importing the headers of imp if set.
func start(cfg Config, imp *neutrino.HeadersImportConfig) (*Node, error) {
	db, err := walletdb.Create(
		"bdb", filepath.Join(cfg.DataDir, "neutrino.db"), true,
		time.Minute, false,
	)
	if err != nil {
		return nil, fmt.Errorf("opening the chain database: %w", err)
	}
	cs, err := neutrino.NewChainService(neutrino.Config{
		DataDir:        cfg.DataDir,
		Database:       db,
		ChainParams:    cfg.Params,
		ConnectPeers:   cfg.ConnectPeers,
		AddPeers:       cfg.AddPeers,
		PersistToDisk:  true,
		MempoolEnabled: true,
		HeadersImport:  imp,
		Dialer:         dialer(cfg.Proxy),
		NameResolver:   resolver(cfg.Proxy),
	})
	if err != nil {
		_ = db.Close()
		return nil, fmt.Errorf("creating the chain service: %w", err)
	}
	if err := cs.Start(context.Background()); err != nil {
		// Not stopped: Stop waits on parts a failed Start never started.
		_ = db.Close()
		return nil, fmt.Errorf("starting the chain service: %w", err)
	}
	connected, unsubscribe, err := cs.ConnectedPeers()
	if err != nil {
		_ = cs.Stop()
		_ = db.Close()
		return nil, fmt.Errorf("subscribing to peers: %w", err)
	}

	n := &Node{cs: cs, db: db, relay: newRelay(mempool.New(cfg.Mempool))}
	ticker := time.NewTicker(time.Second)
	n.relay.run(connected, ticker.C)
	go func() {
		<-n.relay.quit
		ticker.Stop()
		unsubscribe()
	}()
	return n, nil
}

// Stop shuts the node down and closes its data. It is safe to call twice.
func (n *Node) Stop() error {
	n.stopOnce.Do(func() {
		n.relay.stop()
		n.stopErr = n.cs.Stop()
		if err := n.db.Close(); n.stopErr == nil {
			n.stopErr = err
		}
	})
	return n.stopErr
}

// Tip is the best block header known.
func (n *Node) Tip() (height int32, hash chainhash.Hash, err error) {
	best, err := n.cs.BestBlock()
	if err != nil {
		return 0, chainhash.Hash{}, err
	}
	return best.Height, best.Hash, nil
}

// Synced reports whether the node believes it has caught up with its peers.
func (n *Node) Synced() bool {
	return n.cs.IsCurrent()
}

// Peers is the number of connected peers.
func (n *Node) Peers() int {
	return int(n.cs.ConnectedCount())
}

// Broadcast announces tx to peers and hands it to those that ask.
func (n *Node) Broadcast(tx *wire.MsgTx) error {
	return n.cs.SendTransaction(tx)
}

// Mempool is the node's view of unconfirmed transactions.
func (n *Node) Mempool() Mempool {
	return n.relay.tracker
}

// Updates is signaled when something new is held for a watched script or
// outpoint. Signals coalesce: read Spends or Pays to learn what.
func (n *Node) Updates() <-chan struct{} {
	return n.relay.updates
}

// Reject lets go of a held transaction that failed the consumer's checks,
// asks the other peers that announced it, and bans the peer that sent it.
// No honest peer relays a transaction that cannot be valid.
func (n *Node) Reject(txid chainhash.Hash) {
	if from := n.relay.reject(txid); from != "" {
		// A ban that cannot be recorded still disconnects the peer.
		_ = n.cs.BanPeer(from, banman.ExceededBanThreshold)
	}
}

// BlockHash is the hash of the best chain's block at height.
func (n *Node) BlockHash(height uint32) (chainhash.Hash, error) {
	hash, err := n.cs.GetBlockHash(int64(height))
	if err != nil {
		return chainhash.Hash{}, err
	}
	return *hash, nil
}

// Header is the header of the best chain's block at height, from the
// headers the node keeps: no block fetched.
func (n *Node) Header(height uint32) (*wire.BlockHeader, error) {
	return n.cs.BlockHeaders.FetchHeaderByHeight(height)
}

// BlockTime is the time the best chain's block at height claims.
func (n *Node) BlockTime(height uint32) (time.Time, error) {
	header, err := n.cs.BlockHeaders.FetchHeaderByHeight(height)
	if err != nil {
		return time.Time{}, err
	}
	return header.Timestamp, nil
}

// filterBatch is how many filters from the one asked for peers are asked
// for at once: a wallet reads them in order.
const filterBatch = 200

// Filter is the basic compact filter (BIP158) of the block of hash, as
// its N and data, checked against the block's filter header. Once the node
// is current, it comes from the CDN with the others of its file if it can,
// else from peers with the ones after it.
func (n *Node) Filter(hash chainhash.Hash) ([]byte, error) {
	if n.cdn != nil && n.cs.IsCurrent() {
		if height, err := n.cs.GetBlockHeight(&hash); err == nil {
			n.cdn.prefetch(n.cs, uint32(height))
		}
	}
	filter, err := n.cs.GetCFilter(hash, wire.GCSFilterRegular,
		neutrino.OptimisticBatch(), neutrino.MaxBatchSize(filterBatch))
	if err != nil {
		return nil, err
	}
	return filter.NBytes()
}

// Block is the block of hash, from a peer, checked against its header.
func (n *Node) Block(hash chainhash.Hash) (*wire.MsgBlock, error) {
	block, err := n.cs.GetBlock(hash)
	if err != nil {
		return nil, err
	}
	return block.MsgBlock(), nil
}

// dialer is how neutrino reaches its peers: through proxy if set, else
// its own way.
func dialer(proxy *Proxy) func(net.Addr) (net.Conn, error) {
	if proxy == nil {
		return nil
	}
	return func(addr net.Addr) (net.Conn, error) { return proxy.Dial(addr.String()) }
}

// resolver is how neutrino resolves names, the DNS seeds' and the peers':
// through proxy if set, else its own way.
func resolver(proxy *Proxy) func(string) ([]net.IP, error) {
	if proxy == nil {
		return nil
	}
	return proxy.Lookup
}
