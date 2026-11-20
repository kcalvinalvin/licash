package node

import (
	"bytes"
	"context"
	"crypto/sha256"
	"fmt"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
)

// pinnedServer serves files of headers of a made-up chain of height
// blocks, as block-dn does, and what was asked of it: blocks and filters
// are the import files the pin vouches for, the files are of their
// entries, per of them each (importEntries if unset).
type pinnedServer struct {
	blocks, filters []byte
	pin             Pin
	per             int

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
	per := p.per
	if per == 0 {
		per = int(importEntries)
	}
	for prefix, kind := range map[string]struct {
		file  []byte
		entry int
	}{"/headers/": {p.blocks, 80}, "/filter-headers/": {p.filters, 32}} {
		start, err := strconv.Atoi(strings.TrimPrefix(r.URL.Path, prefix))
		if !strings.HasPrefix(r.URL.Path, prefix) || err != nil {
			continue
		}
		entries := kind.file[importMeta:]
		from := start * kind.entry
		if start%per != 0 || from >= len(entries) {
			http.NotFound(w, r)
			return
		}
		file := entries[from:min(from+per*kind.entry, len(entries))]
		http.ServeContent(w, r, "", time.Time{}, bytes.NewReader(file))
		return
	}
	http.NotFound(w, r)
}

// smallFiles has the client and the servers of the test at hand split
// headers into files of entries, 16 if not given: a chain of 40 into 3.
func smallFiles(t *testing.T, entries ...uint32) {
	before := importEntries
	importEntries = 16
	if len(entries) > 0 {
		importEntries = entries[0]
	}
	t.Cleanup(func() { importEntries = before })
}

// asked is what of p's files were asked for, each once.
func (p *pinnedServer) files() []string {
	asked := p.taken()
	slices.Sort(asked)
	return slices.Compact(asked)
}

func (p *pinnedServer) taken() []string {
	p.mu.Lock()
	defer p.mu.Unlock()
	asked := p.asked
	p.asked = nil
	return asked
}

func TestAnEmptyStoreGetsThePinnedFilesChecked(t *testing.T) {
	smallFiles(t)
	p, server := newPinnedServer(t, 40)
	dir := t.TempDir()
	var told [][2]int64
	imp := pinnedImport(context.Background(), dir, server.URL+"/", wire.TestNet, &p.pin, nil,
		func(done, total int64) {
			if len(told) > 0 && done < told[len(told)-1][0] {
				t.Errorf("progress back from %v to %d", told[len(told)-1], done)
			}
			told = append(told, [2]int64{done, total})
		}, quiet())
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
	want := []string{"/filter-headers/0", "/filter-headers/16", "/filter-headers/32",
		"/headers/0", "/headers/16", "/headers/32"}
	if asked := p.files(); !slices.Equal(asked, want) {
		t.Fatalf("asked %v", asked)
	}
	// Downloaded and checked, though not imported yet, as a start killed
	// before importing them leaves them: never asked for again.
	again := pinnedImport(context.Background(), dir, server.URL, wire.TestNet, &p.pin, nil, nil, quiet())
	if again == nil || *again != *imp {
		t.Fatalf("imported again %v", again)
	}
	if asked := p.taken(); len(asked) != 0 {
		t.Fatalf("asked again %v", asked)
	}
	entries, _ := os.ReadDir(filepath.Join(dir, "import"))
	for _, entry := range entries {
		if name := entry.Name(); name != "state" && name != "block_headers.bin" && name != "filter_headers.bin" {
			t.Errorf("left behind: %s", name)
		}
	}
}

// A start that downloaded some files before failing leaves them for the
// next, which asks for the rest only: not marked tried meanwhile.
func TestAStartResumesWhereTheLastLeftOff(t *testing.T) {
	smallFiles(t)
	p, server := newPinnedServer(t, 40)
	p.serve = func(w http.ResponseWriter, r *http.Request) bool {
		if strings.HasPrefix(r.URL.Path, "/filter-headers/") {
			http.Error(w, "busy", http.StatusServiceUnavailable)
			return true
		}
		return false
	}
	dir := t.TempDir()
	if pinnedImport(context.Background(), dir, server.URL, wire.TestNet, &p.pin, nil, nil, quiet()) != nil {
		t.Fatal("imported")
	}
	if _, err := os.Stat(triedMark(dir, &p.pin)); !os.IsNotExist(err) {
		t.Fatalf("marked tried: %v", err)
	}
	if !importDue(dir, server.URL, &p.pin) {
		t.Fatal("not due")
	}
	_ = p.taken()
	p.serve = nil
	var told []int64
	imp := pinnedImport(context.Background(), dir, server.URL, wire.TestNet, &p.pin, nil,
		func(done, _ int64) { told = append(told, done) }, quiet())
	if imp == nil {
		t.Fatal("nothing to import")
	}
	if asked := p.files(); !slices.Equal(asked, []string{"/filter-headers/0", "/filter-headers/16", "/filter-headers/32"}) {
		t.Fatalf("asked %v", asked)
	}
	// Progress from the files kept on.
	if len(told) == 0 || told[0] <= int64(len(p.blocks)-importMeta) {
		t.Fatalf("progress %v", told)
	}
	got, err := os.ReadFile(imp.BlockHeadersSource)
	if err != nil || !bytes.Equal(got, p.blocks) {
		t.Fatalf("block headers: %v", err)
	}
}

// A file a lane stalls on is asked for on another once none is left to
// ask for, the first whole kept: never waited on for importStall.
func TestAFileStalledOnIsAskedForOnAnotherLane(t *testing.T) {
	smallFiles(t)
	p, server := newPinnedServer(t, 40)
	var stalled sync.Once
	p.serve = func(w http.ResponseWriter, r *http.Request) bool {
		first := false
		if r.URL.Path == "/headers/16" {
			stalled.Do(func() { first = true })
		}
		if first {
			w.Header().Set("Content-Length", "1280")
			_, _ = w.Write(make([]byte, 100))
			w.(http.Flusher).Flush()
			<-r.Context().Done()
		}
		return first
	}
	began := time.Now()
	imp := pinnedImport(context.Background(), t.TempDir(), server.URL, wire.TestNet, &p.pin, nil, nil, quiet())
	if imp == nil {
		t.Fatal("nothing to import")
	}
	if took := time.Since(began); took > importStall/2 {
		t.Fatalf("took %v", took)
	}
	if asked := p.taken(); len(slices.DeleteFunc(asked, func(path string) bool { return path != "/headers/16" })) != 2 {
		t.Fatalf("asked for the stalled file %d times", len(asked))
	}
	got, err := os.ReadFile(imp.BlockHeadersSource)
	if err != nil || !bytes.Equal(got, p.blocks) {
		t.Fatalf("block headers: %v", err)
	}
}

// What an older release, or another pin, left in the import directory is
// deleted unasked, but for its tried mark: peers then.
func TestFilesLeftBehindAreNeverTrusted(t *testing.T) {
	smallFiles(t)
	p, server := newPinnedServer(t, 40)
	dir := t.TempDir()
	files := filepath.Join(dir, "import")
	_ = os.MkdirAll(files, 0o700)
	other := p.pin
	other.Headers[0] ^= 1
	_ = writeState(files, stateOf(&other))
	_ = os.WriteFile(filepath.Join(files, "headers-0"), make([]byte, 16*80), 0o600)
	_ = os.WriteFile(filepath.Join(files, "block_headers.bin.part"), make([]byte, 100), 0o600)
	imp := pinnedImport(context.Background(), dir, server.URL, wire.TestNet, &p.pin, nil, nil, quiet())
	if imp == nil {
		t.Fatal("nothing to import")
	}
	if asked := p.files(); len(asked) != 6 {
		t.Fatalf("asked %v", asked)
	}
	got, err := os.ReadFile(imp.BlockHeadersSource)
	if err != nil || !bytes.Equal(got, p.blocks) {
		t.Fatalf("block headers: %v", err)
	}
	// Marked tried by an older release, before downloading: peers.
	legacy := t.TempDir()
	_ = os.WriteFile(triedMark(legacy, &p.pin), nil, 0o600)
	_ = os.MkdirAll(filepath.Join(legacy, "import"), 0o700)
	if importDue(legacy, server.URL, &p.pin) ||
		pinnedImport(context.Background(), legacy, server.URL, wire.TestNet, &p.pin, nil, nil, quiet()) != nil {
		t.Fatal("imported though tried")
	}
	if asked := p.taken(); len(asked) != 0 {
		t.Fatalf("asked %v", asked)
	}
	if _, err := os.Stat(filepath.Join(legacy, "import")); !os.IsNotExist(err) {
		t.Fatalf("the files left behind: %v", err)
	}
}

// Genesis alone, as neutrino writes it on a first start, is empty too.
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
	smallFiles(t)
	cases := map[string]func(p *pinnedServer){
		"a byte changed": func(p *pinnedServer) { p.blocks[500] ^= 1 },
		"filter headers changed": func(p *pinnedServer) {
			p.filters[len(p.filters)-1] ^= 1
		},
		"shorter": func(p *pinnedServer) { p.blocks = p.blocks[:len(p.blocks)-80] },
		// Files of more entries than the client asks for each.
		"longer":           func(p *pinnedServer) { p.per = 17 },
		"pinned elsewhere": func(p *pinnedServer) { p.pin.Block[0] ^= 1 },
		"another filter header pinned": func(p *pinnedServer) {
			p.pin.FilterHeader[0] ^= 1
		},
		"not served": func(p *pinnedServer) {
			p.serve = func(w http.ResponseWriter, r *http.Request) bool {
				if strings.HasPrefix(r.URL.Path, "/filter-headers/") {
					http.NotFound(w, r)
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
// failing, on importStarts starts in all, each counted as it begins.
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
	store := filepath.Join(dir, "peer-headers-v1")
	starts := 1
	for ; ImportDue(cfg); starts++ {
		if pinnedImport(context.Background(), store, server.URL, wire.TestNet, &p.pin, nil, nil, quiet()) != nil {
			t.Fatal("imported")
		}
	}
	if starts != importStarts {
		t.Fatalf("tried on %d starts", starts)
	}
	_ = p.taken()
	if pinnedImport(context.Background(), store, server.URL, wire.TestNet, &p.pin, nil, nil, quiet()) != nil {
		t.Fatal("imported")
	}
	if asked := p.taken(); len(asked) != 0 {
		t.Fatalf("asked %v", asked)
	}
	if _, err := os.Stat(triedMark(store, &p.pin)); err != nil {
		t.Fatalf("not marked tried: %v", err)
	}
}
