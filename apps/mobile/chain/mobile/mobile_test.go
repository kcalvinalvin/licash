package mobile

import (
	"testing"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
)

func TestNetworks(t *testing.T) {
	signet := chaincfg.SigNetParams.GenesisHash
	for _, c := range []struct {
		name, genesis, cdn string
		peers              int
		spacing            time.Duration
	}{
		{"bitcoin", chaincfg.MainNetParams.GenesisHash.String(), "https://block-dn.org", 0, 10 * time.Minute},
		{"signet", signet.String(), "https://signet.block-dn.org", 0, 10 * time.Minute},
		{"mutinynet", signet.String(), "", 1, 30 * time.Second},
		{"regtest", chaincfg.RegressionNetParams.GenesisHash.String(), "", 0, 10 * time.Minute},
	} {
		net, err := networkOf(c.name)
		if err != nil {
			t.Fatal(err)
		}
		p := net.params
		if p.GenesisHash.String() != c.genesis || net.cdn != c.cdn ||
			len(net.peers) != c.peers || p.TargetTimePerBlock != c.spacing {
			t.Errorf("%s: %s %q %v %v", c.name, p.GenesisHash, net.cdn, net.peers, p.TargetTimePerBlock)
		}
	}
	// Mutinynet's own magic, from its challenge, and the signet difficulty.
	mutinynet, _ := networkOf("mutinynet")
	if mutinynet.params.Net == chaincfg.SigNetParams.Net {
		t.Error("Mutinynet has signet's magic")
	}
	if mutinynet.params.PowLimitBits != chaincfg.SigNetParams.PowLimitBits ||
		mutinynet.params.TargetTimespan != chaincfg.SigNetParams.TargetTimespan {
		t.Error("Mutinynet's difficulty is not signet's")
	}
	for _, name := range []string{"", "testnet", "mainnet"} {
		if _, err := networkOf(name); err == nil {
			t.Errorf("%q: known", name)
		}
	}
	if _, err := Start("testnet", t.TempDir(), "", ""); err == nil {
		t.Error("started an unknown network")
	}
}
