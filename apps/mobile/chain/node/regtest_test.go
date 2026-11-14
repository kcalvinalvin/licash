//go:build regtest

package node

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

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
)

// These tests need the Bitcoin Core that regtest.sh starts.

func env(t *testing.T, name string) string {
	t.Helper()
	v := os.Getenv(name)
	if v == "" {
		t.Fatalf("%s is not set: run regtest.sh", name)
	}
	return v
}

// cli runs bitcoin-cli in the node's container.
func cli(t *testing.T, args ...string) string {
	t.Helper()
	full := append([]string{
		"exec", "-u", "bitcoin", env(t, "LICASH_BITCOIND_CONTAINER"),
		"bitcoin-cli", "-regtest", "-rpcwallet=chain",
	}, args...)
	out, err := exec.Command("docker", full...).CombinedOutput()
	if err != nil {
		t.Fatalf("bitcoin-cli %s: %v\n%s", strings.Join(args, " "), err, out)
	}
	return strings.TrimSpace(string(out))
}

func eventually(t *testing.T, what string, done func() bool) {
	t.Helper()
	for deadline := time.Now().Add(90 * time.Second); time.Now().Before(deadline); {
		if done() {
			return
		}
		time.Sleep(200 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for %s", what)
}

func decode(t *testing.T, rawHex string) *wire.MsgTx {
	t.Helper()
	raw, err := hex.DecodeString(rawHex)
	if err != nil {
		t.Fatal(err)
	}
	tx := wire.NewMsgTx(2)
	if err := tx.Deserialize(bytes.NewReader(raw)); err != nil {
		t.Fatal(err)
	}
	return tx
}

// spendOf signs, without sending, a transaction spending op to a new
// address of the Core wallet.
func spendOf(t *testing.T, op wire.OutPoint, amount string) *wire.MsgTx {
	t.Helper()
	raw := cli(t, "createrawtransaction",
		fmt.Sprintf(`[{"txid":"%s","vout":%d}]`, op.Hash, op.Index),
		fmt.Sprintf(`[{"%s":%s}]`, cli(t, "getnewaddress"), amount))
	var signed struct {
		Hex      string `json:"hex"`
		Complete bool   `json:"complete"`
	}
	if err := json.Unmarshal([]byte(cli(t, "signrawtransactionwithwallet", raw)), &signed); err != nil {
		t.Fatal(err)
	}
	if !signed.Complete {
		t.Fatal("the wallet could not sign the spend")
	}
	return decode(t, signed.Hex)
}

func TestRegtestMempoolAndBroadcastThroughBitcoinCore(t *testing.T) {
	n, err := Start(Config{
		DataDir:      t.TempDir(),
		Params:       chaincfg.RegressionNetParams,
		ConnectPeers: []string{env(t, "LICASH_BITCOIND_P2P")},
	})
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := n.Stop(); err != nil {
			t.Errorf("stop: %v", err)
		}
	}()

	height := func() int32 {
		var info struct {
			Blocks int32 `json:"blocks"`
		}
		if err := json.Unmarshal([]byte(cli(t, "getblockchaininfo")), &info); err != nil {
			t.Fatal(err)
		}
		return info.Blocks
	}
	synced := func(want int32) func() bool {
		return func() bool {
			got, _, err := n.Tip()
			return err == nil && got == want && n.Synced()
		}
	}
	start := height()
	eventually(t, "the header sync", synced(start))

	// An unconfirmed payment to a watched script is reported, with the
	// witness its id does not commit to.
	addr := cli(t, "getnewaddress")
	var info struct {
		Script string `json:"scriptPubKey"`
	}
	if err := json.Unmarshal([]byte(cli(t, "getaddressinfo", addr)), &info); err != nil {
		t.Fatal(err)
	}
	script, _ := hex.DecodeString(info.Script)
	n.Mempool().WatchScript(script)

	paid, err := chainhash.NewHashFromStr(cli(t, "sendtoaddress", addr, "1"))
	if err != nil {
		t.Fatal(err)
	}
	eventually(t, "the payment to the watched script", func() bool {
		return len(n.Mempool().Pays(script)) == 1
	})
	select {
	case <-n.Updates():
	default:
		t.Error("no update was signaled for the payment")
	}
	payment := n.Mempool().Pays(script)[0]
	if payment.Txid != *paid {
		t.Fatalf("reported %s, Core sent %s", payment.Txid, paid)
	}
	if !payment.Tx.HasWitness() {
		t.Error("the payment arrived without its witness")
	}
	funding := wire.OutPoint{Hash: *paid, Index: payment.Pays[0]}

	// So is an unconfirmed spend of a watched outpoint, sent by Core.
	n.Mempool().WatchOutpoint(funding)
	first := spendOf(t, funding, "0.999")
	var buf bytes.Buffer
	if err := first.Serialize(&buf); err != nil {
		t.Fatal(err)
	}
	cli(t, "sendrawtransaction", hex.EncodeToString(buf.Bytes()))
	eventually(t, "the spend of the watched outpoint", func() bool {
		return len(n.Mempool().Spends(funding)) == 1
	})
	spent := n.Mempool().Spends(funding)[0]
	if spent.Txid != first.TxHash() || len(spent.Tx.TxIn[0].Witness) == 0 {
		t.Fatalf("reported spend %s with witness %v", spent.Txid, spent.Tx.TxIn[0].Witness)
	}

	// A transaction broadcast through the node reaches Core's mempool.
	second := spendOf(t, wire.OutPoint{Hash: first.TxHash(), Index: 0}, "0.998")
	if err := n.Broadcast(second); err != nil {
		t.Fatalf("broadcast: %v", err)
	}
	eventually(t, "the broadcast to reach Core", func() bool {
		return strings.Contains(cli(t, "getrawmempool"), second.TxHash().String())
	})

	// Announcing transactions did not cost Core its connection, and the
	// node follows the chain on.
	if n.Peers() != 1 {
		t.Fatalf("connected to %d peers, want Core alone", n.Peers())
	}
	cli(t, "generatetoaddress", "1", cli(t, "getnewaddress"))
	eventually(t, "the new block", synced(start+1))
	n.Mempool().Forget(payment.Txid)
	n.Mempool().Forget(spent.Txid)
	if s := n.Mempool().Stats(); s.Held != 0 {
		t.Fatalf("held after confirmation was forgotten: %+v", s)
	}
}
