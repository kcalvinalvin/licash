//go:build regtest

package mobile

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"

	"github.com/btcsuite/btcd/chainhash/v2"
)

// core runs bitcoin-cli in the container regtest.sh starts.
func core(t *testing.T, args ...string) string {
	t.Helper()
	name := os.Getenv("LICASH_BITCOIND_CONTAINER")
	if name == "" {
		t.Fatal("LICASH_BITCOIND_CONTAINER is not set: run regtest.sh")
	}
	full := append([]string{"exec", "-u", "bitcoin", name, "bitcoin-cli", "-regtest"}, args...)
	out, err := exec.Command("docker", full...).CombinedOutput()
	if err != nil {
		t.Fatalf("bitcoin-cli %s: %v\n%s", strings.Join(args, " "), err, out)
	}
	return strings.TrimSpace(string(out))
}

func TestRegtestChainAsTheAppsSeeIt(t *testing.T) {
	c, err := Start("regtest", t.TempDir(), os.Getenv("LICASH_BITCOIND_P2P"), "default")
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := c.Stop(); err != nil {
			t.Errorf("stop: %v", err)
		}
	}()
	for deadline := time.Now().Add(90 * time.Second); ; time.Sleep(200 * time.Millisecond) {
		height, err := c.Height()
		if err == nil && c.Synced() && fmt.Sprint(height) == core(t, "getblockcount") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("timed out syncing")
		}
	}
	if c.Peers() != 1 {
		t.Fatalf("%d peers", c.Peers())
	}

	// Hashes in internal order: reversed from how Core shows them.
	hash, err := c.Hash(100)
	if err != nil {
		t.Fatal(err)
	}
	shown, _ := chainhash.NewHashFromStr(core(t, "getblockhash", "100"))
	if !bytes.Equal(hash, shown[:]) {
		t.Fatalf("hash %x, Core's %s", hash, shown)
	}
	block, err := c.Block(hash)
	if err != nil || hex.EncodeToString(block) != core(t, "getblock", shown.String(), "0") {
		t.Fatalf("block 100 is not Core's: %v", err)
	}
	filter, err := c.Filter(hash)
	var want struct{ Filter string }
	_ = json.Unmarshal([]byte(core(t, "getblockfilter", shown.String())), &want)
	if err != nil || hex.EncodeToString(filter) != want.Filter {
		t.Fatalf("filter 100 is not Core's: %v", err)
	}
	for _, bad := range [][]byte{nil, hash[:31]} {
		if _, err := c.Block(bad); err == nil {
			t.Errorf("block of %x", bad)
		}
		if _, err := c.Filter(bad); err == nil {
			t.Errorf("filter of %x", bad)
		}
	}
	if _, err := c.Hash(-1); err == nil {
		t.Error("hash of -1")
	}
	if err := c.Broadcast([]byte{1, 2}); err == nil {
		t.Error("broadcast garbage")
	}
}
