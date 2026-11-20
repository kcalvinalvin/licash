//go:build regtest

package node

import (
	"bytes"
	"crypto/sha256"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/btcsuite/btcd/chaincfg/v2"
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
	fake := &fakeCDN{params: &chaincfg.RegressionNetParams, blocks: blocks, perHeaders: 50, perFilters: 20}
	server := httptest.NewServer(fake)
	defer server.Close()
	pin := pinOf(fake, tip-25)
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
	end := fmt.Sprint(pin.Height + 1)
	asked := fake.taken()
	if !slices.Contains(asked, "/headers/import/"+end) || !slices.Contains(asked, "/filter-headers/import/"+end) {
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

// Files that are not the pinned ones leave the node syncing from peers
// alone, as without a pin.
func TestRegtestFilesOtherThanThePinnedLeavePeersToSync(t *testing.T) {
	blocks := coreChain(t)
	tip := uint32(len(blocks) - 1)
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
