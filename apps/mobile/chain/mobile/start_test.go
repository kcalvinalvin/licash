package mobile

import (
	"errors"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"

	"github.com/licash-org/licash/apps/mobile/chain/node"
)

// Downloading the pinned headers, the client is handed over at once:
// nothing held yet, everything else refused, and stopped without waiting.
func TestAClientDownloadingPinnedHeadersIsHandedOverAtOnce(t *testing.T) {
	asked := make(chan string, 8)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/status" {
			http.NotFound(w, r)
			return
		}
		asked <- r.URL.Path
		w.Header().Set("Content-Length", "80000") // 80 * 1000, as pinned
		_, _ = w.Write(make([]byte, 1000))
		w.(http.Flusher).Flush()
		<-r.Context().Done()
	}))
	defer server.Close()
	pin := &node.Pin{Height: 999}
	c, err := start(node.Config{
		DataDir:      t.TempDir(),
		Params:       chaincfg.RegressionNetParams,
		ConnectPeers: []string{"127.0.0.1:1"},
		CDN:          server.URL,
		Pin:          pin,
	})
	if err != nil {
		t.Fatal(err)
	}
	if got := <-asked; got != "/headers/0" && got != "/filter-headers/0" {
		t.Fatalf("asked %s", got)
	}
	if p := c.Importing(); p < 0 || p > 100 {
		t.Fatalf("importing %d", p)
	}
	if h, err := c.Height(); h != 0 || err != nil {
		t.Fatalf("height %d, %v", h, err)
	}
	if h, err := c.Headers(); h != 0 || err != nil {
		t.Fatalf("headers %d, %v", h, err)
	}
	if c.Synced() || c.Peers() != 0 || c.PeerHeight() != 0 {
		t.Fatal("started")
	}
	if _, err := c.Hash(0); !errors.Is(err, errImporting) {
		t.Fatalf("hash: %v", err)
	}
	stopped := make(chan error)
	go func() { stopped <- c.Stop() }()
	select {
	case err := <-stopped:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("stopping waited for the download")
	}
	if err := c.Stop(); err != nil {
		t.Fatal(err)
	}
}

// Nothing to download, the client is started before it is handed over.
func TestAClientWithNothingToDownloadIsStarted(t *testing.T) {
	c, err := start(node.Config{
		DataDir:      t.TempDir(),
		Params:       chaincfg.RegressionNetParams,
		ConnectPeers: []string{"127.0.0.1:1"},
	})
	if err != nil {
		t.Fatal(err)
	}
	defer c.Stop()
	if c.Importing() != -1 {
		t.Fatal("importing")
	}
	if _, err := c.Hash(0); err != nil {
		t.Fatalf("genesis: %v", err)
	}
}
