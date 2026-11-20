package mobile

import (
	"net"
	"strings"
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
		{"mutinynet", signet.String(), "https://mutinynet-blockdn.sgn.space", 1, 30 * time.Second},
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
	// Fixed seeds where there are DNS seeds, each a host and port.
	for name, want := range map[string]bool{"bitcoin": true, "signet": true, "mutinynet": false, "regtest": false} {
		nw, _ := networkOf(name)
		if (len(nw.seeds) > 0) != want {
			t.Errorf("%s: %d fixed seeds", name, len(nw.seeds))
		}
		for _, s := range nw.seeds {
			if _, port, err := net.SplitHostPort(s); err != nil || port != nw.params.DefaultPort {
				t.Errorf("%s: fixed seed %q", name, s)
			}
		}
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

func TestTheProxyIsAURLWithAUsernameAndPassword(t *testing.T) {
	if proxy, err := proxyOf(""); proxy != nil || err != nil {
		t.Fatalf("none: %v, %v", proxy, err)
	}
	proxy, err := proxyOf("socks5://user:s3cret@127.0.0.1:9150")
	if err != nil || proxy.Addr != "127.0.0.1:9150" || proxy.Username != "user" || proxy.Password != "s3cret" {
		t.Fatalf("%+v, %v", proxy, err)
	}
	for _, refused := range []string{
		"socks4://user:s3cret@127.0.0.1:9150",
		"http://user:s3cret@127.0.0.1:9150",
		"socks5://127.0.0.1:9150",
		"socks5://user@127.0.0.1:9150",
		"socks5://:s3cret@127.0.0.1:9150",
		"socks5://user:s3cret@127.0.0.1",
		"socks5://user:s3cret@:9150",
		"socks5://user:s3cret@127.0.0.1:9150/path",
		"socks5://user:s3cret@127.0.0.1:9150?x=1",
		"socks5://user:s3cret@[::1",
		"user:s3cret@127.0.0.1:9150",
	} {
		proxy, err := proxyOf(refused)
		if proxy != nil || err == nil {
			t.Errorf("%s: %+v", refused, proxy)
			continue
		}
		if strings.Contains(err.Error(), "s3cret") {
			t.Errorf("%s: the password in %q", refused, err)
		}
	}
	// Refused before anything starts.
	if _, err := StartThrough("regtest", t.TempDir(), "", "", "socks5://u:s3cret@127.0.0.1"); err == nil ||
		strings.Contains(err.Error(), "s3cret") {
		t.Errorf("started through a proxy without a port: %v", err)
	}
}
