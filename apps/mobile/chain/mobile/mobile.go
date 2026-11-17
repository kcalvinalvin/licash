// Package mobile is the chain access the apps bind with gomobile: a light
// client of a named network, its blocks' hashes, filters and blocks, and
// broadcasting. Hashes are 32 bytes in Bitcoin's internal order, blocks
// and transactions consensus serialized.
package mobile

import (
	"bytes"
	"encoding/hex"
	"errors"
	"fmt"
	"net/url"
	"sort"
	"strings"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"

	"github.com/licash-org/licash/apps/mobile/chain/node"
)

// The test networks besides Bitcoin's own, as Licash names them.
const (
	// mutinynetChallenge is Mutinynet's signet challenge, as published at
	// https://blog.mutinywallet.com/mutinynet/.
	mutinynetChallenge = "512102f7561d208dd9ae99bf497273e16f389bdbd6c4742ddb8e6b216e64fa2928ad8f51ae"
	// mutinynetPeer is the node Mutinynet publishes: it has no DNS seed.
	mutinynetPeer = "45.79.52.207:38333"
)

// network is what a light client of name follows.
type network struct {
	params chaincfg.Params
	// cdn is the block-dn server to use by default, if any.
	cdn string
	// peers are the only peers to use when none are given, for a network
	// without DNS seeds.
	peers []string
}

func networkOf(name string) (network, error) {
	switch name {
	case "bitcoin":
		return network{params: chaincfg.MainNetParams, cdn: "https://block-dn.org"}, nil
	case "signet":
		return network{params: chaincfg.SigNetParams, cdn: "https://signet.block-dn.org"}, nil
	case "mutinynet":
		challenge, err := hex.DecodeString(mutinynetChallenge)
		if err != nil {
			return network{}, err
		}
		params := chaincfg.CustomSignetParams(challenge, nil)
		params.Name = "mutinynet"
		params.TargetTimePerBlock = 30 * time.Second
		return network{params: params, peers: []string{mutinynetPeer}}, nil
	case "regtest":
		return network{params: chaincfg.RegressionNetParams}, nil
	}
	return network{}, fmt.Errorf("unknown network %q: bitcoin, signet, mutinynet or regtest", name)
}

// Chain is a running light client.
type Chain struct {
	node *node.Node
}

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
	n, err := node.Start(node.Config{
		DataDir:      dataDir,
		Params:       net.params,
		ConnectPeers: connect,
		CDN:          cdn,
		Proxy:        through,
		Log:          logger(),
	})
	if err != nil {
		return nil, err
	}
	return &Chain{node: n}, nil
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
func (c *Chain) Height() (int32, error) {
	height, _, err := c.node.Tip()
	return height, err
}

// Hash is the hash of the best chain's block at height.
func (c *Chain) Hash(height int32) ([]byte, error) {
	if height < 0 {
		return nil, fmt.Errorf("no block %d", height)
	}
	hash, err := c.node.BlockHash(uint32(height))
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
	t, err := c.node.BlockTime(uint32(height))
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
	header, err := c.node.Header(uint32(height))
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
	return c.node.Filter(*h)
}

// Block is the block of hash, checked against its header.
func (c *Chain) Block(hash []byte) ([]byte, error) {
	h, err := chainhash.NewHash(hash)
	if err != nil {
		return nil, err
	}
	block, err := c.node.Block(*h)
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
	return c.node.Broadcast(msg)
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
	pool := c.node.Mempool()
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
// while the filter headers, which Height waits for, catch up.
func (c *Chain) Headers() (int32, error) {
	return c.node.Headers()
}

// PeerHeight is the highest block a connected peer told of, the height the
// client catches up to: 0 with no peer.
func (c *Chain) PeerHeight() int32 {
	return c.node.PeerHeight()
}

// Synced is whether the client believes it caught up with its peers.
func (c *Chain) Synced() bool {
	return c.node.Synced()
}

// Peers is the number of peers connected.
func (c *Chain) Peers() int {
	return c.node.Peers()
}

// Stop shuts the client down. It is safe to call twice.
func (c *Chain) Stop() error {
	return c.node.Stop()
}
