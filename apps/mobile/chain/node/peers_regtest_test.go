//go:build regtest

package node

import (
	"path/filepath"
	"testing"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
)

// A peer that answered is reached again by the next start, which is told
// of no peer at all: regtest has no DNS seeds, so only the book finds it.
func TestRegtestARestartReachesTheRememberedPeer(t *testing.T) {
	dir := t.TempDir()
	core := env(t, "LICASH_BITCOIND_P2P")

	first, err := Start(Config{DataDir: dir, Params: chaincfg.RegressionNetParams, AddPeers: []string{core}})
	if err != nil {
		t.Fatal(err)
	}
	eventually(t, "a peer", func() bool { return first.Peers() > 0 })
	eventually(t, "the peer remembered", func() bool {
		b, err := openPeerBook(filepath.Dir(first.upkeep.book.path))
		return err == nil && len(b.latest(peersFirst, time.Now(), anyAddr)) > 0
	})
	if err := first.Stop(); err != nil {
		t.Fatal(err)
	}

	again, err := Start(Config{DataDir: dir, Params: chaincfg.RegressionNetParams})
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := again.Stop(); err != nil {
			t.Errorf("stop: %v", err)
		}
	}()
	eventually(t, "the remembered peer", func() bool { return again.Peers() > 0 })
}

// Told its peers, a node remembers none.
func TestRegtestPeersGivenAreNotRemembered(t *testing.T) {
	n, err := Start(Config{
		DataDir:      t.TempDir(),
		Params:       chaincfg.RegressionNetParams,
		ConnectPeers: []string{env(t, "LICASH_BITCOIND_P2P")},
		FixedSeeds:   []string{"192.0.2.1:18444"},
	})
	if err != nil {
		t.Fatal(err)
	}
	defer n.Stop()
	if n.upkeep.book != nil || len(n.upkeep.seeds) != 0 {
		t.Fatal("a node told its peers keeps a book or reaches seeds")
	}
}
