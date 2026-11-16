package node

import (
	"bytes"
	"encoding/json"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"github.com/btcsuite/btcd/chaincfg/v2"
)

func TestCompactSize(t *testing.T) {
	for _, c := range []struct {
		in    []byte
		value uint64
		width int
	}{
		{nil, 0, 0},
		{[]byte{0}, 0, 1},
		{[]byte{0xfc}, 0xfc, 1},
		{[]byte{0xfd, 0x34, 0x12}, 0x1234, 3},
		{[]byte{0xfd, 0x34}, 0, 0},
		{[]byte{0xfe, 4, 3, 2, 1}, 0x01020304, 5},
		{[]byte{0xff, 8, 7, 6, 5, 4, 3, 2, 1}, 0x0102030405060708, 9},
		{[]byte{0xff, 1, 2, 3}, 0, 0},
	} {
		value, width := compactSize(c.in)
		if value != c.value || width != c.width {
			t.Errorf("%x: %d, %d; want %d, %d", c.in, value, width, c.value, c.width)
		}
	}
}

func TestCDNRefreshRejectsMalformedStatusWithoutReplacingGoodState(t *testing.T) {
	good := cdnStatus{Genesis: chaincfg.RegressionNetParams.GenesisHash.String(), BestFilter: 100, PerFilterFile: 20}
	var mu sync.Mutex
	status := good
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		defer mu.Unlock()
		_ = json.NewEncoder(w).Encode(status)
	}))
	defer server.Close()
	c, err := newCDN(server.URL, &chaincfg.RegressionNetParams, nil, slog.New(slog.DiscardHandler))
	if err != nil {
		t.Fatal(err)
	}
	for _, bad := range []cdnStatus{
		{Genesis: good.Genesis, PerFilterFile: 0},
		{Genesis: good.Genesis, PerFilterFile: maxFiltersPerFile + 1},
		{Genesis: "another chain", PerFilterFile: 20},
		{Genesis: good.Genesis, PerFilterFile: 21},
	} {
		mu.Lock()
		status = bad
		mu.Unlock()
		read := c.read
		if err := c.refresh(); err == nil {
			t.Fatalf("accepted %+v", bad)
		}
		if c.status != good || c.read != read {
			t.Fatal("bad status replaced valid state")
		}
	}
}

func TestFilterCountsAreBoundedBeforeAllocation(t *testing.T) {
	for _, count := range []uint32{1, maxFiltersPerFile + 1, ^uint32(0)} {
		if _, err := splitFilters(nil, count); err == nil {
			t.Fatalf("accepted count %d", count)
		}
	}
}

func TestPeerOnlyLayoutRefusesLegacyCachesWithoutChangingThem(t *testing.T) {
	base := t.TempDir()
	dir, err := peerDirectory(base)
	if err != nil || dir != filepath.Join(base, "peer-headers-v1") {
		t.Fatalf("fresh: %s, %v", dir, err)
	}
	for _, name := range []string{"block_headers.bin", "reg_filter_headers.bin", "neutrino.db"} {
		path := filepath.Join(base, name)
		if err := os.WriteFile(path, []byte("legacy cache"), 0600); err != nil {
			t.Fatal(err)
		}
		if _, err := peerDirectory(base); err == nil {
			t.Fatalf("accepted %s", name)
		}
		if data, err := os.ReadFile(path); err != nil || string(data) != "legacy cache" {
			t.Fatal("modified legacy cache")
		}
		if err := os.Remove(path); err != nil {
			t.Fatal(err)
		}
	}
	if again, err := peerDirectory(base); err != nil || again != dir {
		t.Fatalf("restart: %s, %v", again, err)
	}
}

func TestLegacyCacheIsRefusedBeforeContactingTheCDN(t *testing.T) {
	base := t.TempDir()
	path := filepath.Join(base, "neutrino.db")
	if err := os.WriteFile(path, []byte("unverified cached filters"), 0600); err != nil {
		t.Fatal(err)
	}
	var mu sync.Mutex
	requests := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		requests++
		mu.Unlock()
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer server.Close()
	_, err := Start(Config{DataDir: base, Params: chaincfg.RegressionNetParams, CDN: server.URL})
	if err == nil || !strings.Contains(err.Error(), "legacy chain data") {
		t.Fatalf("legacy cache: %v", err)
	}
	mu.Lock()
	defer mu.Unlock()
	if requests != 0 {
		t.Fatal("contacted CDN for an unverified store")
	}
	if data, err := os.ReadFile(path); err != nil || string(data) != "unverified cached filters" {
		t.Fatal("changed legacy cache")
	}
}

func TestSplitFilters(t *testing.T) {
	long := bytes.Repeat([]byte{7}, 300)
	file := append([]byte{2, 0xaa, 0xbb, 0}, 0xfd, 0x2c, 0x01)
	file = append(file, long...)
	filters, err := splitFilters(file, 3)
	if err != nil {
		t.Fatal(err)
	}
	if len(filters) != 3 || !bytes.Equal(filters[0], []byte{0xaa, 0xbb}) ||
		len(filters[1]) != 0 || !bytes.Equal(filters[2], long) {
		t.Fatalf("%x", filters)
	}
	for name, c := range map[string]struct {
		file  []byte
		count uint32
	}{
		"fewer":     {file, 4},
		"more":      {file, 2},
		"cut short": {file[:len(file)-1], 3},
		"no length": {append(file[:4:4], 0xfd), 2},
	} {
		if _, err := splitFilters(c.file, c.count); err == nil {
			t.Errorf("%s: read", name)
		}
	}
	if filters, err := splitFilters(nil, 0); err != nil || len(filters) != 0 {
		t.Errorf("empty: %v %v", filters, err)
	}
}
