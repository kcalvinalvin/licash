//go:build regtest

package node

import (
	"bytes"
	"context"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/lightninglabs/neutrino/headerfs"
)

// pinOf is the pin of Core's chain at height, as cmd/pin makes it, from
// the files fake serves.
func pinOf(fake *fakeCDN, height uint32) Pin {
	var blocks, filters bytes.Buffer
	fake.importFile(&blocks, 0, height+1, func(b coreBlock, _ uint32) []byte { return b.header })
	fake.importFile(&filters, 1, height+1, func(b coreBlock, _ uint32) []byte { return b.filterHeader[:] })
	return Pin{
		Height:        height,
		Block:         fake.blocks[height].hash,
		FilterHeader:  fake.blocks[height].filterHeader,
		Headers:       sha256.Sum256(blocks.Bytes()),
		FilterHeaders: sha256.Sum256(filters.Bytes()),
	}
}

// An empty store holds the pinned headers once started, and its peers take
// it on from there to Core's tip.
func TestRegtestPinnedHeadersAreImportedThenPeersTakeOver(t *testing.T) {
	if count, _ := strconv.Atoi(cli(t, "getblockcount")); count < 230 {
		cli(t, "generatetoaddress", strconv.Itoa(230-count), cli(t, "getnewaddress"))
	}
	blocks := coreChain(t)
	tip := uint32(len(blocks) - 1)
	smallFiles(t, 50)
	fake := &fakeCDN{params: &chaincfg.RegressionNetParams, blocks: blocks, perHeaders: 50, perFilters: 20}
	server := httptest.NewServer(fake)
	defer server.Close()
	pin := pinOf(fake, tip-25)
	var logged bytes.Buffer
	var stored [2]int64
	n, err := Start(Config{
		DataDir:      t.TempDir(),
		Params:       chaincfg.RegressionNetParams,
		ConnectPeers: []string{env(t, "LICASH_BITCOIND_P2P")},
		CDN:          server.URL,
		Pin:          &pin,
		Log:          slog.New(slog.NewTextHandler(&logged, nil)),
		Stored:       func(done, total int64) { stored = [2]int64{done, total} },
	})
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := n.Stop(); err != nil {
			t.Errorf("stop: %v", err)
		}
	}()
	// Told as stored, all of them in the end: those above genesis.
	if all := int64(pin.Height); stored != [2]int64{all, all} {
		t.Fatalf("stored %v of %d", stored, all)
	}
	asked := fake.taken()
	if !slices.Contains(asked, "/headers/0") || !slices.Contains(asked, "/filter-headers/200") {
		t.Fatalf("asked %v", asked)
	}
	if !strings.Contains(logged.String(), "phase: pinned headers imported") {
		t.Fatalf("not imported: %s", logged.String())
	}
	header, err := n.cs.BlockHeaders.FetchHeaderByHeight(pin.Height)
	if err != nil || header.BlockHash() != pin.Block {
		t.Fatalf("the pinned block not held at once: %v", err)
	}
	filterHeader, err := n.cs.RegFilterHeaders.FetchHeaderByHeight(pin.Height)
	if err != nil || *filterHeader != pin.FilterHeader {
		t.Fatalf("the pinned filter header not held at once: %v", err)
	}
	eventually(t, "the node to catch up with Core past the pin", func() bool {
		height, hash, err := n.Tip()
		return err == nil && uint32(height) == tip && hash == blocks[tip].hash && n.Synced()
	})
	last, err := n.cs.RegFilterHeaders.FetchHeaderByHeight(tip)
	if err != nil || *last != blocks[tip].filterHeader {
		t.Fatalf("filter headers past the pin: %v", err)
	}
}

// A start killed while storing the pinned headers, its block headers a
// batch ahead of its filter headers and the last cut short, leaves the
// next to store the rest from the files kept, nothing downloaded again,
// and peers take it on from there.
func TestRegtestAnImportKilledWhileStoringResumes(t *testing.T) {
	if count, _ := strconv.Atoi(cli(t, "getblockcount")); count < 230 {
		cli(t, "generatetoaddress", strconv.Itoa(230-count), cli(t, "getnewaddress"))
	}
	blocks := coreChain(t)
	tip := uint32(len(blocks) - 1)
	smallFiles(t, 50)
	fake := &fakeCDN{params: &chaincfg.RegressionNetParams, blocks: blocks, perHeaders: 50, perFilters: 20}
	server := httptest.NewServer(fake)
	defer server.Close()
	pin := pinOf(fake, tip-25)
	base := t.TempDir()
	dir, err := peerDirectory(base)
	if err != nil {
		t.Fatal(err)
	}
	params := chaincfg.RegressionNetParams
	if pinnedImport(context.Background(), dir, server.URL, params.Net, &pin, nil, nil, quiet()) == nil {
		t.Fatal("nothing to import")
	}
	fake.taken()
	// What the killed start stored.
	db, err := openChainDB(filepath.Join(dir, "neutrino.db"))
	if err != nil {
		t.Fatal(err)
	}
	headers, err := headerfs.NewBlockHeaderStore(dir, db, &params)
	if err != nil {
		t.Fatal(err)
	}
	filters, err := headerfs.NewFilterHeaderStore(dir, db, headerfs.RegularFilter, &params, nil)
	if err != nil {
		t.Fatal(err)
	}
	const storedBlocks, storedFilters = 100, 60
	var bs []headerfs.BlockHeader
	for height := uint32(1); height <= storedBlocks; height++ {
		var header wire.BlockHeader
		if err := header.Deserialize(bytes.NewReader(blocks[height].header)); err != nil {
			t.Fatal(err)
		}
		bs = append(bs, headerfs.BlockHeader{BlockHeader: &header, Height: height})
	}
	var fs []headerfs.FilterHeader
	for height := uint32(1); height <= storedFilters; height++ {
		fs = append(fs, headerfs.FilterHeader{FilterHash: blocks[height].filterHeader, Height: height})
	}
	fs[len(fs)-1].HeaderHash = blocks[storedFilters].hash
	if err := errors.Join(headers.WriteHeaders(bs...), filters.WriteHeaders(fs...), db.Close()); err != nil {
		t.Fatal(err)
	}
	flat, err := os.OpenFile(filepath.Join(dir, "block_headers.bin"), os.O_WRONLY|os.O_APPEND, 0)
	if err != nil {
		t.Fatal(err)
	}
	_, _ = flat.Write(blocks[storedBlocks+1].header[:16])
	_ = flat.Close()

	cfg := Config{
		DataDir:      base,
		Params:       params,
		ConnectPeers: []string{env(t, "LICASH_BITCOIND_P2P")},
		CDN:          server.URL,
		Pin:          &pin,
	}
	if !ImportDue(cfg) {
		t.Fatal("not due")
	}
	var logged bytes.Buffer
	var told [][2]int64
	cfg.Log = slog.New(slog.NewTextHandler(&logged, nil))
	cfg.Stored = func(done, total int64) { told = append(told, [2]int64{done, total}) }
	n, err := Start(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := n.Stop(); err != nil {
			t.Errorf("stop: %v", err)
		}
	}()
	if asked := fake.taken(); len(asked) != 0 {
		t.Fatalf("downloaded again %v", asked)
	}
	if !strings.Contains(logged.String(), "phase: pinned headers imported") {
		t.Fatalf("not resumed: %s", logged.String())
	}
	// Told from the filter headers held on, rising, to all of them.
	all := int64(pin.Height)
	if len(told) == 0 || told[0] != [2]int64{storedFilters, all} || told[len(told)-1] != [2]int64{all, all} ||
		!slices.IsSortedFunc(told, func(a, b [2]int64) int { return int(a[0] - b[0]) }) {
		t.Fatalf("told %v of %d", told, all)
	}
	for height := uint32(1); height <= pin.Height; height++ {
		header, err := n.cs.BlockHeaders.FetchHeaderByHeight(height)
		if err != nil || header.BlockHash() != blocks[height].hash {
			t.Fatalf("block header %d: %v", height, err)
		}
		filterHeader, err := n.cs.RegFilterHeaders.FetchHeaderByHeight(height)
		if err != nil || *filterHeader != blocks[height].filterHeader {
			t.Fatalf("filter header %d: %v", height, err)
		}
	}
	if _, err := os.Stat(triedMark(dir, &pin)); !os.IsNotExist(err) {
		t.Fatalf("marked tried: %v", err)
	}
	eventually(t, "the node to catch up with Core past the pin", func() bool {
		height, hash, err := n.Tip()
		return err == nil && uint32(height) == tip && hash == blocks[tip].hash && n.Synced()
	})
}

// Files that are not the pinned ones leave the node syncing from peers
// alone, as without a pin.
func TestRegtestFilesOtherThanThePinnedLeavePeersToSync(t *testing.T) {
	blocks := coreChain(t)
	tip := uint32(len(blocks) - 1)
	smallFiles(t, 50)
	fake := &fakeCDN{params: &chaincfg.RegressionNetParams, blocks: blocks, perHeaders: 50,
		perFilters: 20, badHeaders: true}
	server := httptest.NewServer(fake)
	defer server.Close()
	// The pin of Core's own files: the server breaks a header of its.
	pin := pinOf(fake, tip-5)
	var logged bytes.Buffer
	n, err := Start(Config{
		DataDir:      t.TempDir(),
		Params:       chaincfg.RegressionNetParams,
		ConnectPeers: []string{env(t, "LICASH_BITCOIND_P2P")},
		CDN:          server.URL,
		Pin:          &pin,
		Log:          slog.New(slog.NewTextHandler(&logged, nil)),
	})
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := n.Stop(); err != nil {
			t.Errorf("stop: %v", err)
		}
	}()
	if !strings.Contains(logged.String(), "the server's files are not the release's") {
		t.Fatalf("not refused: %s", logged.String())
	}
	eventually(t, "the node to catch up with Core from its peer", func() bool {
		height, hash, err := n.Tip()
		return err == nil && uint32(height) == tip && hash == blocks[tip].hash && n.Synced()
	})
}

// Files matching the pin that neutrino still refuses, as a release pinning
// a broken chain would: the node starts again from peers alone, in the
// same process, and catches up.
func TestRegtestAnImportRefusedByNeutrinoLeavesPeersToSync(t *testing.T) {
	blocks := coreChain(t)
	tip := uint32(len(blocks) - 1)
	smallFiles(t, 50)
	fake := &fakeCDN{params: &chaincfg.RegressionNetParams, blocks: blocks, perHeaders: 50,
		perFilters: 20, badHeaders: true}
	server := httptest.NewServer(fake)
	defer server.Close()
	pin := pinOf(fake, tip-5)
	served, err := http.Get(fmt.Sprintf("%s/headers/import/%d", server.URL, pin.Height+1))
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(served.Body)
	served.Body.Close()
	pin.Headers = sha256.Sum256(body)
	var logged bytes.Buffer
	n, err := Start(Config{
		DataDir:      t.TempDir(),
		Params:       chaincfg.RegressionNetParams,
		ConnectPeers: []string{env(t, "LICASH_BITCOIND_P2P")},
		CDN:          server.URL,
		Pin:          &pin,
		Log:          slog.New(slog.NewTextHandler(&logged, nil)),
	})
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := n.Stop(); err != nil {
			t.Errorf("stop: %v", err)
		}
	}()
	if !strings.Contains(logged.String(), "pinned headers not imported, syncing from peers") {
		t.Fatalf("not refused by neutrino: %s", logged.String())
	}
	eventually(t, "the node to catch up with Core from its peer", func() bool {
		height, hash, err := n.Tip()
		return err == nil && uint32(height) == tip && hash == blocks[tip].hash && n.Synced()
	})
}
