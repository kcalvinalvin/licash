package node

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/binary"
	"fmt"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
)

// pinnedServer serves import files of a made-up chain of height blocks,
// as block-dn does, and what was asked of it.
type pinnedServer struct {
	blocks, filters []byte
	pin             Pin

	mu    sync.Mutex
	asked []string
	// Served instead of the files, if set.
	serve func(w http.ResponseWriter, r *http.Request) bool
}

func newPinnedServer(t *testing.T, height uint32) (*pinnedServer, *httptest.Server) {
	t.Helper()
	p := &pinnedServer{}
	meta := func(kind byte) []byte { return []byte{0xfa, 0xbf, 0xb5, 0xda, 0, kind, 0, 0, 0, 0} }
	p.blocks, p.filters = meta(0), meta(1)
	var last wire.BlockHeader
	for h := uint32(0); h <= height; h++ {
		last = wire.BlockHeader{Version: 1, Nonce: h, PrevBlock: last.BlockHash()}
		var b bytes.Buffer
		if err := last.Serialize(&b); err != nil {
			t.Fatal(err)
		}
		p.blocks = append(p.blocks, b.Bytes()...)
		filter := chainhash.HashH([]byte(fmt.Sprint("filter header ", h)))
		p.filters = append(p.filters, filter[:]...)
		p.pin.FilterHeader = filter
	}
	p.pin.Height = height
	p.pin.Block = last.BlockHash()
	p.pin.Headers = sha256.Sum256(p.blocks)
	p.pin.FilterHeaders = sha256.Sum256(p.filters)
	server := httptest.NewServer(p)
	t.Cleanup(server.Close)
	return p, server
}

func (p *pinnedServer) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	p.mu.Lock()
	p.asked = append(p.asked, r.URL.Path)
	serve := p.serve
	p.mu.Unlock()
	if serve != nil && serve(w, r) {
		return
	}
	end := fmt.Sprint(p.pin.Height + 1)
	switch r.URL.Path {
	case "/headers/import/" + end:
		_, _ = w.Write(p.blocks)
	case "/filter-headers/import/" + end:
		_, _ = w.Write(p.filters)
	default:
		http.NotFound(w, r)
	}
}

func (p *pinnedServer) taken() []string {
	p.mu.Lock()
	defer p.mu.Unlock()
	asked := p.asked
	p.asked = nil
	return asked
}

func TestAnEmptyStoreGetsThePinnedFilesChecked(t *testing.T) {
	p, server := newPinnedServer(t, 40)
	dir := t.TempDir()
	var told [][2]int64
	imp := pinnedImport(context.Background(), dir, server.URL+"/", wire.TestNet, &p.pin, nil,
		func(done, total int64) { told = append(told, [2]int64{done, total}) }, quiet())
	if imp == nil {
		t.Fatal("nothing to import")
	}
	for source, want := range map[string][]byte{imp.BlockHeadersSource: p.blocks, imp.FilterHeadersSource: p.filters} {
		got, err := os.ReadFile(source)
		if err != nil || !bytes.Equal(got, want) {
			t.Fatalf("%s: %v", source, err)
		}
	}
	all := int64(len(p.blocks) + len(p.filters))
	if p.pin.Bytes() != all || len(told) == 0 || told[len(told)-1] != [2]int64{all, all} {
		t.Fatalf("progress %v of %d", told, all)
	}
	if asked := p.taken(); !slices.Equal(asked, []string{"/headers/import/41", "/filter-headers/import/41"}) {
		t.Fatalf("asked %v", asked)
	}
	// Once tried, never again: the store fills from peers then.
	if pinnedImport(context.Background(), dir, server.URL, wire.TestNet, &p.pin, nil, nil, quiet()) != nil {
		t.Fatal("imported twice")
	}
	if asked := p.taken(); len(asked) != 0 {
		t.Fatalf("asked again %v", asked)
	}
	if _, err := os.Stat(filepath.Join(dir, "import")); !os.IsNotExist(err) {
		t.Fatalf("the files left behind: %v", err)
	}
}

// Genesis alone, as neutrino writes it on a first start, is empty too.
// A server naming another network's magic in its files' headers, as
// Mutinynet's names signet's, serves the node's files under its own.
func TestAServerNamingAnotherMagicServesTheNodesFiles(t *testing.T) {
	p, server := newPinnedServer(t, 40)
	blocks, filters := slices.Clone(p.blocks), slices.Clone(p.filters)
	signet := binary.LittleEndian.AppendUint32(nil, uint32(wire.SigNet))
	copy(p.blocks, signet)
	copy(p.filters, signet)
	imp := pinnedImport(context.Background(), t.TempDir(), server.URL, wire.TestNet, &p.pin, nil, nil, quiet())
	if imp == nil {
		t.Fatal("nothing to import")
	}
	for source, want := range map[string][]byte{imp.BlockHeadersSource: blocks, imp.FilterHeadersSource: filters} {
		got, err := os.ReadFile(source)
		if err != nil || !bytes.Equal(got, want) {
			t.Fatalf("%s: %v", source, err)
		}
	}
}

func TestAStoreOfGenesisAloneIsEmpty(t *testing.T) {
	dir := t.TempDir()
	if !emptyStore(dir) {
		t.Fatal("no files: empty")
	}
	_ = os.WriteFile(filepath.Join(dir, "block_headers.bin"), make([]byte, 80), 0o600)
	_ = os.WriteFile(filepath.Join(dir, "reg_filter_headers.bin"), make([]byte, 32), 0o600)
	if !emptyStore(dir) {
		t.Fatal("genesis alone: empty")
	}
	for name, size := range map[string]int{"block_headers.bin": 160, "reg_filter_headers.bin": 64} {
		held := t.TempDir()
		_ = os.WriteFile(filepath.Join(held, name), make([]byte, size), 0o600)
		if emptyStore(held) {
			t.Fatalf("%s of %d bytes: not empty", name, size)
		}
	}
	odd := t.TempDir()
	_ = os.WriteFile(filepath.Join(odd, "block_headers.bin"), make([]byte, 79), 0o600)
	if emptyStore(odd) {
		t.Fatal("an odd size: not empty")
	}
}

// A store holding headers, synced or half way, is never touched, whatever
// it holds: an upgrading installation keeps syncing from peers.
func TestAStoreHoldingHeadersNeverAsks(t *testing.T) {
	p, server := newPinnedServer(t, 40)
	dir := t.TempDir()
	_ = os.WriteFile(filepath.Join(dir, "block_headers.bin"), make([]byte, 80*7), 0o600)
	_ = os.WriteFile(filepath.Join(dir, "reg_filter_headers.bin"), make([]byte, 32), 0o600)
	if pinnedImport(context.Background(), dir, server.URL, wire.TestNet, &p.pin, nil, nil, quiet()) != nil {
		t.Fatal("imported over headers")
	}
	if asked := p.taken(); len(asked) != 0 {
		t.Fatalf("asked %v", asked)
	}
	if _, err := os.Stat(filepath.Join(dir, "import-40.tried")); !os.IsNotExist(err) {
		t.Fatalf("marked tried: %v", err)
	}
}

func TestNoPinOrNoServerNeverAsks(t *testing.T) {
	p, server := newPinnedServer(t, 40)
	if pinnedImport(context.Background(), t.TempDir(), server.URL, wire.TestNet, nil, nil, nil, quiet()) != nil ||
		pinnedImport(context.Background(), t.TempDir(), "", wire.TestNet, &p.pin, nil, nil, quiet()) != nil {
		t.Fatal("imported")
	}
	if asked := p.taken(); len(asked) != 0 {
		t.Fatalf("asked %v", asked)
	}
}

// Whatever differs from the pin is refused, nothing of it kept, and the
// next start does not try again.
func TestFilesOtherThanThePinnedAreRefused(t *testing.T) {
	cases := map[string]func(p *pinnedServer){
		"a byte changed": func(p *pinnedServer) { p.blocks[500] ^= 1 },
		"filter headers changed": func(p *pinnedServer) {
			p.filters[len(p.filters)-1] ^= 1
		},
		"shorter": func(p *pinnedServer) { p.blocks = p.blocks[:len(p.blocks)-80] },
		"longer": func(p *pinnedServer) {
			p.filters = append(p.filters, make([]byte, 32)...)
		},
		"another first height": func(p *pinnedServer) { p.blocks[6] ^= 1 },
		"pinned elsewhere":     func(p *pinnedServer) { p.pin.Block[0] ^= 1 },
		"another filter header pinned": func(p *pinnedServer) {
			p.pin.FilterHeader[0] ^= 1
		},
		"not served": func(p *pinnedServer) {
			p.serve = func(w http.ResponseWriter, r *http.Request) bool {
				if strings.HasPrefix(r.URL.Path, "/filter-headers/") {
					http.Error(w, "gone", http.StatusServiceUnavailable)
					return true
				}
				return false
			}
		},
	}
	for name, change := range cases {
		t.Run(name, func(t *testing.T) {
			p, server := newPinnedServer(t, 40)
			change(p)
			dir := t.TempDir()
			var logged bytes.Buffer
			log := slog.New(slog.NewTextHandler(&logged, nil))
			if pinnedImport(context.Background(), dir, server.URL, wire.TestNet, &p.pin, nil, nil, log) != nil {
				t.Fatal("imported")
			}
			if _, err := os.Stat(filepath.Join(dir, "import")); !os.IsNotExist(err) {
				t.Fatalf("the files left behind: %v", err)
			}
			if _, err := os.Stat(filepath.Join(dir, "import-40.tried")); err != nil {
				t.Fatalf("not marked tried: %v", err)
			}
			if !strings.Contains(logged.String(), "syncing") {
				t.Fatalf("not logged: %s", logged.String())
			}
		})
	}
}

// A start given up on stops downloading: peers serve.
func TestADownloadGivenUpOnStops(t *testing.T) {
	p, server := newPinnedServer(t, 40)
	ctx, cancel := context.WithCancel(context.Background())
	p.serve = func(w http.ResponseWriter, r *http.Request) bool {
		w.Header().Set("Content-Length", fmt.Sprint(len(p.blocks)))
		_, _ = w.Write(p.blocks[:100])
		w.(http.Flusher).Flush()
		cancel()
		<-r.Context().Done()
		return true
	}
	dir := t.TempDir()
	done := make(chan bool)
	go func() {
		done <- pinnedImport(ctx, dir, server.URL, wire.TestNet, &p.pin, nil, nil, quiet()) == nil
	}()
	select {
	case refused := <-done:
		if !refused {
			t.Fatal("imported")
		}
	case <-time.After(4 * time.Second):
		t.Fatal("still downloading")
	}
}

// Given up on by the caller, the download is tried again on the next start;
// cut off by the server, it is not.
func TestOnlyADownloadGivenUpOnIsTriedAgain(t *testing.T) {
	p, server := newPinnedServer(t, 40)
	dir := t.TempDir()
	ctx, cancel := context.WithCancel(context.Background())
	p.serve = func(w http.ResponseWriter, r *http.Request) bool {
		if r.URL.Path == "/status" {
			return false
		}
		cancel()
		<-r.Context().Done()
		return true
	}
	cfg := Config{DataDir: dir, Params: chaincfg.RegressionNetParams, CDN: server.URL, Pin: &p.pin, Log: quiet()}
	if !ImportDue(cfg) {
		t.Fatal("an empty store: due")
	}
	if _, err := StartContext(ctx, cfg); err == nil {
		t.Fatal("started though given up on")
	}
	if !ImportDue(cfg) {
		t.Fatal("not due again")
	}
	p.serve = func(w http.ResponseWriter, r *http.Request) bool {
		http.Error(w, "gone", http.StatusServiceUnavailable)
		return true
	}
	if pinnedImport(context.Background(), filepath.Join(dir, "peer-headers-v1"), server.URL, wire.TestNet, &p.pin, nil, nil, quiet()) != nil {
		t.Fatal("imported")
	}
	if ImportDue(cfg) {
		t.Fatal("due again after failing")
	}
}
