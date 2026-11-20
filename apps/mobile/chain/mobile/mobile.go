// Package mobile is the chain access the apps bind with gomobile: a light
// client of a named network, its blocks' hashes, filters and blocks, and
// broadcasting. Hashes are 32 bytes in Bitcoin's internal order, blocks
// and transactions consensus serialized.
package mobile

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"net/url"
	"sort"
	"strings"
	"sync"
	"sync/atomic"

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"

	"github.com/licash-org/licash/apps/mobile/chain/node"
)

// network is what a light client of name follows.
type network struct {
	params chaincfg.Params
	// cdn is the block-dn server to use by default, if any.
	cdn string
	// peers are the only peers to use when none are given, for a network
	// without DNS seeds.
	peers []string
	// seeds are reached when no other peer answers (cmd/seeds).
	seeds []string
	// pin is the block this release vouches for, an empty store filled to
	// it from cdn (cmd/pin): none if nil.
	pin *node.Pin
}

func networkOf(name string) (network, error) {
	switch name {
	case "bitcoin":
		return network{params: chaincfg.MainNetParams, cdn: "https://block-dn.org", seeds: bitcoinSeeds,
			pin: bitcoinPin}, nil
	case "signet":
		return network{params: chaincfg.SigNetParams, cdn: "https://signet.block-dn.org", seeds: signetSeeds,
			pin: signetPin}, nil
	case "mutinynet":
		// Its CDN is the only one there is, self-hosted.
		return network{params: node.Mutinynet(), cdn: "https://mutinynet-blockdn.sgn.space",
			peers: []string{node.MutinynetPeer}, pin: mutinynetPin}, nil
	case "regtest":
		return network{params: chaincfg.RegressionNetParams}, nil
	}
	return network{}, fmt.Errorf("unknown network %q: bitcoin, signet, mutinynet or regtest", name)
}

// Chain is a running light client: started, or downloading the headers
// this release pins first (Importing), which takes a minute or more.
type Chain struct {
	mu   sync.Mutex
	node *node.Node
	// err is why a start in the background failed.
	err error
	// cancel gives up on a start in the background; done is closed once
	// it ended.
	cancel context.CancelFunc
	done   chan struct{}
	// The bytes of pinned headers downloaded, of all, while downloading.
	downloaded, total atomic.Int64
}

// errImporting is what is asked of a client still downloading headers.
var errImporting = errors.New("the light client is still downloading block headers")

// Start runs a light client of network ("bitcoin", "signet", "mutinynet"
// or "regtest") with its data in dataDir. Peers, comma separated, are then
// the only ones used; without them, the network's are found. Cdn is the
// base URL of a block-dn server to sync faster from, "default" for the
// network's, or empty for none.
func Start(network, dataDir, peers, cdn string) (*Chain, error) {
	return StartThrough(network, dataDir, peers, cdn, "")
}

// StartThrough is Start reaching everything through proxy, a SOCKS5 proxy
// such as Tor's as socks5://username:password@host:port: the peers, the
// names of DNS seeds and peers, and the block-dn server, nothing directly.
// None if empty.
func StartThrough(network, dataDir, peers, cdn, proxy string) (*Chain, error) {
	through, err := proxyOf(proxy)
	if err != nil {
		return nil, err
	}
	net, err := networkOf(network)
	if err != nil {
		return nil, err
	}
	if cdn == "default" {
		cdn = net.cdn
	}
	connect := net.peers
	if peers != "" {
		connect = strings.Split(peers, ",")
	}
	return start(node.Config{
		DataDir:      dataDir,
		Params:       net.params,
		ConnectPeers: connect,
		FixedSeeds:   net.seeds,
		CDN:          cdn,
		Pin:          net.pin,
		Proxy:        through,
		Log:          logger(),
	})
}

// start runs a light client of cfg: at once, or once the headers it
// pins are downloaded, in the background.
func start(cfg node.Config) (*Chain, error) {
	c := &Chain{done: make(chan struct{})}
	cfg.Progress = func(done, total int64) {
		c.total.Store(total)
		c.downloaded.Store(done)
	}
	if !node.ImportDue(cfg) {
		defer close(c.done)
		n, err := node.Start(cfg)
		if err != nil {
			return nil, err
		}
		c.node = n
		return c, nil
	}
	// Downloading takes long: the caller carries on meanwhile.
	ctx, cancel := context.WithCancel(context.Background())
	c.cancel = cancel
	c.total.Store(cfg.Pin.Bytes())
	go func() {
		defer close(c.done)
		n, err := node.StartContext(ctx, cfg)
		c.mu.Lock()
		c.node, c.err = n, err
		c.mu.Unlock()
		c.total.Store(0)
	}()
	return c, nil
}

// running is the started node, or why there is none.
func (c *Chain) running() (*node.Node, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.node != nil {
		return c.node, nil
	}
	if c.err != nil {
		return nil, c.err
	}
	return nil, errImporting
}

// Importing is how far the download of the headers this release pins is,
// in percent, while the client downloads them before starting: -1 when
// not downloading.
func (c *Chain) Importing() int32 {
	total := c.total.Load()
	if total <= 0 {
		return -1
	}
	return int32(c.downloaded.Load() * 100 / total)
}

// proxyOf is the SOCKS5 proxy of proxy, a URL with a username and
// password: none if empty. Its errors never quote it, as it holds a
// password.
func proxyOf(proxy string) (*node.Proxy, error) {
	if proxy == "" {
		return nil, nil
	}
	refused := errors.New("not a proxy: socks5://username:password@host:port")
	u, err := url.Parse(proxy)
	if err != nil || u.Scheme != "socks5" || u.User == nil || u.Port() == "" ||
		u.Hostname() == "" || (u.Path != "" && u.Path != "/") || u.RawQuery != "" || u.Fragment != "" {
		return nil, refused
	}
	password, set := u.User.Password()
	if !set || u.User.Username() == "" {
		return nil, refused
	}
	return &node.Proxy{Addr: u.Host, Username: u.User.Username(), Password: password}, nil
}

// Height is the best chain's height.
// Nothing while downloading headers.
func (c *Chain) Height() (int32, error) {
	n, err := c.running()
	if errors.Is(err, errImporting) {
		return 0, nil
	} else if err != nil {
		return 0, err
	}
	height, _, err := n.Tip()
	return height, err
}

// Hash is the hash of the best chain's block at height.
func (c *Chain) Hash(height int32) ([]byte, error) {
	if height < 0 {
		return nil, fmt.Errorf("no block %d", height)
	}
	n, err := c.running()
	if err != nil {
		return nil, err
	}
	hash, err := n.BlockHash(uint32(height))
	if err != nil {
		return nil, err
	}
	return hash[:], nil
}

// Time is the time the best chain's block at height claims, in seconds
// since the Unix epoch.
func (c *Chain) Time(height int32) (int64, error) {
	if height < 0 {
		return 0, fmt.Errorf("no block %d", height)
	}
	n, err := c.running()
	if err != nil {
		return 0, err
	}
	t, err := n.BlockTime(uint32(height))
	if err != nil {
		return 0, err
	}
	return t.Unix(), nil
}

// Header is the header of the best chain's block at height, its 80 bytes
// as serialized: from the headers the node keeps, no block fetched.
func (c *Chain) Header(height int32) ([]byte, error) {
	if height < 0 {
		return nil, fmt.Errorf("no block %d", height)
	}
	n, err := c.running()
	if err != nil {
		return nil, err
	}
	header, err := n.Header(uint32(height))
	if err != nil {
		return nil, err
	}
	var buf bytes.Buffer
	if err := header.Serialize(&buf); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

// Filter is the basic compact filter (BIP158) of the block of hash,
// checked against its filter header.
func (c *Chain) Filter(hash []byte) ([]byte, error) {
	h, err := chainhash.NewHash(hash)
	if err != nil {
		return nil, err
	}
	n, err := c.running()
	if err != nil {
		return nil, err
	}
	return n.Filter(*h)
}

// Block is the block of hash, checked against its header.
func (c *Chain) Block(hash []byte) ([]byte, error) {
	h, err := chainhash.NewHash(hash)
	if err != nil {
		return nil, err
	}
	n, err := c.running()
	if err != nil {
		return nil, err
	}
	block, err := n.Block(*h)
	if err != nil {
		return nil, err
	}
	var buf bytes.Buffer
	if err := block.Serialize(&buf); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

// Broadcast hands the transaction tx to peers.
func (c *Chain) Broadcast(tx []byte) error {
	msg := wire.NewMsgTx(wire.TxVersion)
	if err := msg.Deserialize(bytes.NewReader(tx)); err != nil {
		return fmt.Errorf("not a transaction: %w", err)
	}
	n, err := c.running()
	if err != nil {
		return err
	}
	return n.Broadcast(msg)
}

// Unconfirmed is the transactions peers relayed that pay script, oldest
// first, consensus serialized back to back; script is watched from the
// first call on, so what peers announced before is not held. They are
// hints: a peer can make one up, and one can be dropped or replaced. Only
// a block confirms a payment.
func (c *Chain) Unconfirmed(script []byte) ([]byte, error) {
	if len(script) == 0 {
		return nil, fmt.Errorf("no script")
	}
	n, err := c.running()
	if err != nil {
		return nil, err
	}
	pool := n.Mempool()
	pool.WatchScript(script)
	seen := pool.Pays(script)
	sort.Slice(seen, func(i, j int) bool { return seen[i].At.Before(seen[j].At) })
	var buf bytes.Buffer
	for _, s := range seen {
		if err := s.Tx.Serialize(&buf); err != nil {
			return nil, err
		}
	}
	return buf.Bytes(), nil
}

// Headers is the height of the newest block header kept: ahead of Height
// while the filter headers, which Height waits for, catch up. Nothing
// while downloading headers.
func (c *Chain) Headers() (int32, error) {
	n, err := c.running()
	if errors.Is(err, errImporting) {
		return 0, nil
	} else if err != nil {
		return 0, err
	}
	return n.Headers()
}

// PeerHeight is the highest block a connected peer told of, the height the
// client catches up to: 0 with no peer.
func (c *Chain) PeerHeight() int32 {
	if n, err := c.running(); err == nil {
		return n.PeerHeight()
	}
	return 0
}

// Synced is whether the client believes it caught up with its peers.
func (c *Chain) Synced() bool {
	n, err := c.running()
	return err == nil && n.Synced()
}

// Peers is the number of peers connected.
func (c *Chain) Peers() int {
	if n, err := c.running(); err == nil {
		return n.Peers()
	}
	return 0
}

// Stop shuts the client down, giving up on downloading headers. It is
// safe to call twice.
func (c *Chain) Stop() error {
	if c.cancel != nil {
		c.cancel()
	}
	<-c.done
	c.mu.Lock()
	n := c.node
	c.mu.Unlock()
	if n == nil {
		return nil
	}
	return n.Stop()
}
