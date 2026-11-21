package node

import (
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
)

// A start waits on no CDN: its status, read in the background, is tried
// again while it fails, and filters come from it once read.
func TestAStartWaitsOnNoCDN(t *testing.T) {
	saved := cdnTries
	cdnTries = []time.Duration{0, 10 * time.Millisecond, 10 * time.Millisecond, 10 * time.Millisecond}
	defer func() { cdnTries = saved }()

	release := make(chan struct{})
	var asked atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/status" {
			http.NotFound(w, r)
			return
		}
		// The first hangs until the node started, the next fails: as
		// through Tor not yet up.
		switch asked.Add(1) {
		case 1:
			select {
			case <-release:
			case <-r.Context().Done():
			}
			http.Error(w, "not yet", http.StatusServiceUnavailable)
		case 2:
			http.Error(w, "not yet", http.StatusServiceUnavailable)
		default:
			_, _ = fmt.Fprintf(w, `{"chain_genesis_hash":%q,"best_filter_height":0,"entries_per_filter_file":20}`,
				chaincfg.RegressionNetParams.GenesisHash)
		}
	}))
	defer server.Close()
	defer close(release)

	began := time.Now()
	n, err := Start(Config{
		DataDir:      t.TempDir(),
		Params:       chaincfg.RegressionNetParams,
		ConnectPeers: []string{"127.0.0.1:1"},
		CDN:          server.URL,
		Log:          quiet(),
	})
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = n.Stop() }()
	if took := time.Since(began); took > statusTimeout/2 {
		t.Fatalf("the start waited on the CDN: %v", took)
	}
	if n.cdn.Load() != nil {
		t.Fatal("the CDN used before its status was read")
	}
	release <- struct{}{}
	for deadline := time.Now().Add(5 * time.Second); n.cdn.Load() == nil; time.Sleep(10 * time.Millisecond) {
		if time.Now().After(deadline) {
			t.Fatalf("the CDN never used, its status asked %d times", asked.Load())
		}
	}
	if got := asked.Load(); got != 3 {
		t.Fatalf("its status asked %d times", got)
	}
}
