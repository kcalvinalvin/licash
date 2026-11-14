//go:build regtest

package mobile

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/lightninglabs/neutrino"
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

func TestRegtestUnconfirmedPaymentsToAWatchedScript(t *testing.T) {
	c, err := Start("regtest", t.TempDir(), os.Getenv("LICASH_BITCOIND_P2P"), "")
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = c.Stop() }()
	for deadline := time.Now().Add(90 * time.Second); c.Peers() < 1; time.Sleep(200 * time.Millisecond) {
		if time.Now().After(deadline) {
			t.Fatal("timed out connecting")
		}
	}
	address := core(t, "-rpcwallet=chain", "getnewaddress", "", "bech32m")
	var info struct{ ScriptPubKey string }
	_ = json.Unmarshal([]byte(core(t, "-rpcwallet=chain", "getaddressinfo", address)), &info)
	script, _ := hex.DecodeString(info.ScriptPubKey)
	if txs, err := c.Unconfirmed(script); err != nil || len(txs) != 0 {
		t.Fatalf("before any payment: %x %v", txs, err)
	}
	if _, err := c.Unconfirmed(nil); err == nil {
		t.Error("no script watched")
	}
	txid := core(t, "-rpcwallet=chain", "sendtoaddress", address, "0.0001")
	for deadline := time.Now().Add(60 * time.Second); ; time.Sleep(200 * time.Millisecond) {
		raw, err := c.Unconfirmed(script)
		if err != nil {
			t.Fatal(err)
		}
		if len(raw) > 0 {
			tx := wire.NewMsgTx(wire.TxVersion)
			if err := tx.Deserialize(bytes.NewReader(raw)); err != nil {
				t.Fatal(err)
			}
			if tx.TxHash().String() != txid {
				t.Fatalf("%s, sent %s", tx.TxHash(), txid)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the payment was never relayed")
		}
	}
}

func TestRegtestBlockTimesAreTheHeaders(t *testing.T) {
	c, err := Start("regtest", t.TempDir(), os.Getenv("LICASH_BITCOIND_P2P"), "")
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = c.Stop() }()
	for deadline := time.Now().Add(90 * time.Second); ; time.Sleep(200 * time.Millisecond) {
		height, err := c.Height()
		if err == nil && c.Synced() && fmt.Sprint(height) == core(t, "getblockcount") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("timed out syncing")
		}
	}
	var header struct{ Time int64 }
	hash := core(t, "getblockhash", "100")
	_ = json.Unmarshal([]byte(core(t, "getblockheader", hash)), &header)
	at, err := c.Time(100)
	if err != nil || at != header.Time || at == 0 {
		t.Fatalf("block 100 at %d, Core's %d: %v", at, header.Time, err)
	}
	height, _ := c.Height()
	for _, bad := range []int32{-1, height + 1} {
		if _, err := c.Time(bad); err == nil {
			t.Errorf("time of block %d", bad)
		}
	}
}

func TestRegtestTheLogFileHoldsTheLightClients(t *testing.T) {
	path := filepath.Join(t.TempDir(), "chain.log")
	if err := SetLog(path); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		logMu.Lock()
		logOut = os.Stderr
		logMu.Unlock()
		neutrino.DisableLog()
	})
	c, err := Start("regtest", t.TempDir(), os.Getenv("LICASH_BITCOIND_P2P"), "")
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = c.Stop() }()
	for deadline := time.Now().Add(90 * time.Second); !c.Synced(); time.Sleep(200 * time.Millisecond) {
		if time.Now().After(deadline) {
			t.Fatal("timed out syncing")
		}
	}
	logged, _ := os.ReadFile(path)
	for _, want := range []string{"light client started", "[INF] LCHN"} {
		if !bytes.Contains(logged, []byte(want)) {
			t.Errorf("no %q in the log:\n%s", want, logged)
		}
	}
}
