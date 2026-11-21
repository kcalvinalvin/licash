//go:build regtest

package node

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/lightninglabs/neutrino/filterdb"
)

// coreBlock is what Core holds of a block, as block-dn serves it.
type coreBlock struct {
	hash         chainhash.Hash
	header       []byte
	filter       []byte
	filterHeader chainhash.Hash
}

// coreChain reads Core's best chain, in one call into its container.
func coreChain(t *testing.T) []coreBlock {
	t.Helper()
	script := `c="bitcoin-cli -regtest"; for h in $(seq 0 $($c getblockcount)); do
		b=$($c getblockhash $h)
		echo "$b $($c getblockheader $b false) $($c getblockfilter $b | tr -d ' \n')"
	done`
	out, err := exec.Command("docker", "exec", "-u", "bitcoin",
		env(t, "LICASH_BITCOIND_CONTAINER"), "sh", "-c", script).Output()
	if err != nil {
		t.Fatalf("reading Core's chain: %v", err)
	}
	var blocks []coreBlock
	for _, line := range strings.Split(strings.TrimSpace(string(out)), "\n") {
		fields := strings.Fields(line)
		hash, err1 := chainhash.NewHashFromStr(fields[0])
		header, err2 := hex.DecodeString(fields[1])
		var f struct{ Filter, Header string }
		err3 := json.Unmarshal([]byte(fields[2]), &f)
		filter, err4 := hex.DecodeString(f.Filter)
		filterHeader, err5 := chainhash.NewHashFromStr(f.Header)
		for _, err := range []error{err1, err2, err3, err4, err5} {
			if err != nil {
				t.Fatalf("%q: %v", line, err)
			}
		}
		blocks = append(blocks, coreBlock{*hash, header, filter, *filterHeader})
	}
	return blocks
}

// fakeCDN serves a snapshot of Core's chain as block-dn does, and what was
// asked of it.
type fakeCDN struct {
	params     *chaincfg.Params
	blocks     []coreBlock
	perHeaders uint32
	perFilters uint32

	mu sync.Mutex
	// A header of the import file broken.
	badHeaders bool
	// The files of filters with a filter broken, by their first block.
	badFilters map[uint32]bool
	asked      []string
}

func (f *fakeCDN) taken() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	asked := f.asked
	f.asked = nil
	return asked
}

// statusOnly waits for the status the node reads in the background once
// started, then checks it was all it asked: never headers.
func (f *fakeCDN) statusOnly(t *testing.T) {
	t.Helper()
	var asked []string
	eventually(t, "the CDN's status read", func() bool {
		asked = append(asked, f.taken()...)
		return len(asked) > 0
	})
	if !slices.Equal(asked, []string{"/status"}) {
		t.Fatalf("asked CDN for headers: %v", asked)
	}
}

func (f *fakeCDN) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	f.asked = append(f.asked, r.URL.Path)
	badHeaders, badFilters := f.badHeaders, f.badFilters
	f.mu.Unlock()
	tip := uint32(len(f.blocks) - 1)
	number := func(prefix string) (uint32, bool) {
		n, err := strconv.ParseUint(strings.TrimPrefix(r.URL.Path, prefix), 10, 32)
		return uint32(n), err == nil && strings.HasPrefix(r.URL.Path, prefix)
	}
	var body bytes.Buffer
	if r.URL.Path == "/status" {
		_ = json.NewEncoder(&body).Encode(map[string]any{
			"chain_genesis_hash":      f.blocks[0].hash.String(),
			"best_block_height":       tip,
			"best_filter_height":      tip,
			"entries_per_header_file": f.perHeaders,
			"entries_per_filter_file": f.perFilters,
		})
	} else if end, ok := number("/headers/import/"); ok && end <= tip+1 {
		f.importFile(&body, 0, end, func(b coreBlock, h uint32) []byte {
			header := slices.Clone(b.header)
			if badHeaders && h == 10 {
				header[4] ^= 1
			}
			return header
		})
	} else if end, ok := number("/filter-headers/import/"); ok && end <= tip+1 {
		f.importFile(&body, 1, end, func(b coreBlock, _ uint32) []byte {
			return b.filterHeader[:]
		})
	} else if first, ok := number("/headers/"); ok && first%f.perHeaders == 0 && first <= tip {
		for h := first; h < min(first+f.perHeaders, tip+1); h++ {
			header := slices.Clone(f.blocks[h].header)
			if badHeaders && h == 10 {
				header[4] ^= 1
			}
			body.Write(header)
		}
		http.ServeContent(w, r, "", time.Time{}, bytes.NewReader(body.Bytes()))
		return
	} else if first, ok := number("/filter-headers/"); ok && first%f.perHeaders == 0 && first <= tip {
		for h := first; h < min(first+f.perHeaders, tip+1); h++ {
			body.Write(f.blocks[h].filterHeader[:])
		}
		http.ServeContent(w, r, "", time.Time{}, bytes.NewReader(body.Bytes()))
		return
	} else if first, ok := number("/filters/"); ok && first%f.perFilters == 0 &&
		first+f.perFilters <= tip+1 {
		for h := first; h < first+f.perFilters; h++ {
			filter := slices.Clone(f.blocks[h].filter)
			if badFilters[first] && h == first+f.perFilters-1 {
				filter[len(filter)-1] ^= 1
			}
			_ = wire.WriteVarBytes(&body, 0, filter)
		}
	} else {
		http.NotFound(w, r)
		return
	}
	_, _ = w.Write(body.Bytes())
}

// importFile writes a neutrino import file of entries of headerType, from
// genesis to end.
func (f *fakeCDN) importFile(w *bytes.Buffer, headerType byte, end uint32,
	entry func(coreBlock, uint32) []byte) {

	_ = binary.Write(w, binary.LittleEndian, uint32(f.params.Net))
	w.WriteByte(0)
	w.WriteByte(headerType)
	_ = binary.Write(w, binary.LittleEndian, uint32(0))
	for h := uint32(0); h < end; h++ {
		w.Write(entry(f.blocks[h], h))
	}
}

func TestRegtestPeerHeadersAndCDNFilters(t *testing.T) {
	// A chain of several files of either.
	if count, _ := strconv.Atoi(cli(t, "getblockcount")); count < 230 {
		cli(t, "generatetoaddress", strconv.Itoa(230-count), cli(t, "getnewaddress"))
	}
	blocks := coreChain(t)
	tip := uint32(len(blocks) - 1)
	fake := &fakeCDN{
		params:     &chaincfg.RegressionNetParams,
		blocks:     blocks,
		perHeaders: 50,
		perFilters: 20,
		badFilters: map[uint32]bool{40: true},
	}
	server := httptest.NewServer(fake)
	defer server.Close()
	log := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelDebug}))
	run := func(dir string, peers ...string) *Node {
		t.Helper()
		n, err := Start(Config{
			DataDir:      dir,
			Params:       chaincfg.RegressionNetParams,
			ConnectPeers: peers,
			CDN:          server.URL,
			Log:          log,
		})
		if err != nil {
			t.Fatal(err)
		}
		return n
	}
	stop := func(n *Node) {
		if err := n.Stop(); err != nil {
			t.Errorf("stop: %v", err)
		}
	}
	// CDN status cannot advance the chain: no peer means only genesis.
	dir := t.TempDir()
	n := run(dir, "127.0.0.1:1")
	if height, hash, err := n.Tip(); err != nil || height != 0 || hash != blocks[0].hash {
		t.Fatalf("CDN advanced tip without peers: %d %s %v", height, hash, err)
	}
	fake.statusOnly(t)
	stop(n)

	// Started again, it imports nothing more and follows Core on.
	n = run(dir, env(t, "LICASH_BITCOIND_P2P"))
	defer stop(n)
	fake.statusOnly(t)
	eventually(t, "the node to catch up with Core", func() bool {
		height, _, err := n.Tip()
		return err == nil && uint32(height) == tip && n.Synced()
	})

	// Filters come from the CDN by the file, each checked.
	filterOf := func(h uint32) []byte {
		t.Helper()
		filter, err := n.Filter(blocks[h].hash)
		if err != nil {
			t.Fatalf("filter %d: %v", h, err)
		}
		return filter
	}
	stored := func(h uint32) []byte {
		filter, err := n.cs.FilterDB.FetchFilter(&blocks[h].hash, filterdb.RegularFilter)
		if err != nil || filter == nil {
			return nil
		}
		raw, _ := filter.NBytes()
		return raw
	}
	if !bytes.Equal(filterOf(25), blocks[25].filter) {
		t.Fatal("filter 25 is not Core's")
	}
	_ = filterOf(26)
	if asked := fake.taken(); !slices.Equal(asked, []string{"/filters/20"}) {
		t.Fatalf("asked %v for filters 25 and 26", asked)
	}
	for h := uint32(20); h < 40; h++ {
		if !bytes.Equal(stored(h), blocks[h].filter) {
			t.Fatalf("filter %d not stored from the CDN", h)
		}
	}

	// A file with a filter unlike its header is refused whole: the peers
	// serve, and the broken filter is never stored.
	if !bytes.Equal(filterOf(45), blocks[45].filter) {
		t.Fatal("filter 45 is not Core's")
	}
	if asked := fake.taken(); !slices.Equal(asked, []string{"/filters/40"}) {
		t.Fatalf("asked %v for filter 45", asked)
	}
	if got := stored(59); got != nil && !bytes.Equal(got, blocks[59].filter) {
		t.Fatal("the broken filter was stored")
	}

	// The last file, not sealed yet, comes from the peers.
	last := tip / 20 * 20
	if last+19 <= tip {
		t.Fatalf("tip %d ends a file", tip)
	}
	if !bytes.Equal(filterOf(tip), blocks[tip].filter) {
		t.Fatal("the tip's filter is not Core's")
	}
	for _, asked := range fake.taken() {
		if asked == fmt.Sprintf("/filters/%d", last) {
			t.Fatal("asked for an unsealed file")
		}
	}

	// Hashes and blocks are Core's.
	hash, err := n.BlockHash(100)
	if err != nil || hash != blocks[100].hash {
		t.Fatalf("hash 100: %s %v", hash, err)
	}
	block, err := n.Block(hash)
	if err != nil {
		t.Fatal(err)
	}
	var raw bytes.Buffer
	if err := block.Serialize(&raw); err != nil {
		t.Fatal(err)
	}
	if hex.EncodeToString(raw.Bytes()) != cli(t, "getblock", hash.String(), "0") {
		t.Fatal("block 100 is not Core's")
	}
}

func TestRegtestCDNHeaderFilesAreNeverRequested(t *testing.T) {
	blocks := coreChain(t)
	tip := uint32(len(blocks) - 1)
	fake := &fakeCDN{
		params:     &chaincfg.RegressionNetParams,
		blocks:     blocks,
		perHeaders: 50,
		perFilters: 20,
		badHeaders: true,
	}
	server := httptest.NewServer(fake)
	defer server.Close()
	var logged bytes.Buffer
	n, err := Start(Config{
		DataDir:      t.TempDir(),
		Params:       chaincfg.RegressionNetParams,
		ConnectPeers: []string{env(t, "LICASH_BITCOIND_P2P")},
		CDN:          server.URL,
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
	fake.statusOnly(t)
	eventually(t, "the node to catch up with Core from its peer", func() bool {
		height, hash, err := n.Tip()
		return err == nil && uint32(height) == tip && hash == blocks[tip].hash && n.Synced()
	})
}
