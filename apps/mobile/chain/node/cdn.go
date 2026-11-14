package node

// A block-dn server (github.com/guggero/block-dn): headers to import into a
// new or far-behind node in whole files, and compact filters by the file,
// each checked against the filter header the node holds before it is kept.
// The node's peers still check the imported filter headers against their
// checkpoints, and anything the server lacks or gets wrong comes from them.

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/btcsuite/btcd/btcutil/v2/gcs"
	"github.com/btcsuite/btcd/btcutil/v2/gcs/builder"
	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/lightninglabs/neutrino"
	"github.com/lightninglabs/neutrino/filterdb"
)

const (
	statusTimeout = 10 * time.Second
	fileTimeout   = 5 * time.Minute
	// statusEvery is how often the server's status is read again, for the
	// files it sealed meanwhile.
	statusEvery = time.Minute
	// maxFilterFile bounds a file of filters held in memory.
	maxFilterFile = 128 << 20
)

// cdnStatus is what a block-dn server's /status says of itself.
type cdnStatus struct {
	Genesis       string `json:"chain_genesis_hash"`
	BestBlock     uint32 `json:"best_block_height"`
	BestFilter    uint32 `json:"best_filter_height"`
	PerHeaderFile uint32 `json:"entries_per_header_file"`
	PerFilterFile uint32 `json:"entries_per_filter_file"`
}

type cdn struct {
	base   string
	client *http.Client
	log    *slog.Logger

	mu     sync.Mutex
	status cdnStatus
	read   time.Time
	// The files of filters fetched, or that failed, by their first block.
	done map[uint32]bool
}

// newCDN is the server at base, if it follows the chain of params.
func newCDN(base string, params *chaincfg.Params, log *slog.Logger) (*cdn, error) {
	c := &cdn{
		base:   strings.TrimRight(base, "/"),
		client: &http.Client{Timeout: fileTimeout},
		log:    log,
		done:   make(map[uint32]bool),
	}
	if err := c.refresh(); err != nil {
		return nil, err
	}
	if c.status.Genesis != params.GenesisHash.String() {
		return nil, fmt.Errorf("%s follows the chain of genesis %s, not %s",
			c.base, c.status.Genesis, params.GenesisHash)
	}
	if c.status.PerHeaderFile == 0 || c.status.PerFilterFile == 0 {
		return nil, fmt.Errorf("%s names no file sizes", c.base)
	}
	return c, nil
}

// refresh reads the server's status again.
func (c *cdn) refresh() error {
	ctx, cancel := context.WithTimeout(context.Background(), statusTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.base+"/status", nil)
	if err != nil {
		return err
	}
	resp, err := c.client.Do(req)
	if err != nil {
		return fmt.Errorf("reading the status of %s: %w", c.base, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("the status of %s: HTTP %d", c.base, resp.StatusCode)
	}
	var status cdnStatus
	if err := json.NewDecoder(io.LimitReader(resp.Body, 1<<16)).Decode(&status); err != nil {
		return fmt.Errorf("the status of %s: %w", c.base, err)
	}
	c.status, c.read = status, time.Now()
	return nil
}

// headersImport is what to import into a node holding held block headers:
// the server's sealed files of headers, if they hold at least a file more.
func (c *cdn) headersImport(held uint32) *neutrino.HeadersImportConfig {
	per := c.status.PerHeaderFile
	end := (min(c.status.BestBlock, c.status.BestFilter) + 1) / per * per
	if end < held+per {
		return nil
	}
	c.log.Info("importing headers", "server", c.base, "held", held, "to", end)
	return &neutrino.HeadersImportConfig{
		BlockHeadersSource:  fmt.Sprintf("%s/headers/import/%d", c.base, end),
		FilterHeadersSource: fmt.Sprintf("%s/filter-headers/import/%d", c.base, end),
	}
}

// heldHeaders is how many block headers the node in dir holds, from the
// size of neutrino's flat file of them: none if it has none yet.
func heldHeaders(dir string) uint32 {
	info, err := os.Stat(filepath.Join(dir, "block_headers.bin"))
	if err != nil {
		return 0
	}
	return uint32(info.Size() / 80)
}

// prefetch stores in cs the filters of the file holding the block at
// height, checked, unless done already, sealed by the server yet, or past
// the filter headers cs holds. Failures are logged: the peers serve then.
func (c *cdn) prefetch(cs *neutrino.ChainService, height uint32) {
	c.mu.Lock()
	defer c.mu.Unlock()
	per := c.status.PerFilterFile
	first := height / per * per
	last := first + per - 1
	if c.done[first] {
		return
	}
	if last > c.status.BestFilter && time.Since(c.read) > statusEvery {
		if err := c.refresh(); err != nil {
			c.log.Warn("block-dn status", "err", err)
		}
	}
	if last > c.status.BestFilter {
		return
	}
	if _, tip, err := cs.RegFilterHeaders.ChainTip(); err != nil || tip < last {
		return
	}
	c.done[first] = true
	started := time.Now()
	if err := c.filters(cs, first, per); err != nil {
		c.log.Warn("block-dn filters, asking peers", "first", first, "err", err)
		return
	}
	c.log.Debug("block-dn filters stored", "first", first, "count", per,
		"took", time.Since(started))
}

// filters fetches the file of count filters from block first, checks each
// against its filter header and stores them all, or none.
func (c *cdn) filters(cs *neutrino.ChainService, first, count uint32) error {
	resp, err := c.client.Get(fmt.Sprintf("%s/filters/%d", c.base, first))
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("HTTP %d", resp.StatusCode)
	}
	body, err := io.ReadAll(io.LimitReader(resp.Body, maxFilterFile+1))
	if err != nil {
		return err
	}
	if len(body) > maxFilterFile {
		return errors.New("file too large")
	}
	raws, err := splitFilters(body, count)
	if err != nil {
		return err
	}
	var prev chainhash.Hash
	if first > 0 {
		header, err := cs.RegFilterHeaders.FetchHeaderByHeight(first - 1)
		if err != nil {
			return err
		}
		prev = *header
	}
	data := make([]*filterdb.FilterData, 0, count)
	for i, raw := range raws {
		height := first + uint32(i)
		filter, err := gcs.FromNBytes(builder.DefaultP, builder.DefaultM, raw)
		if err != nil {
			return fmt.Errorf("the filter of block %d: %w", height, err)
		}
		header, err := builder.MakeHeaderForFilter(filter, prev)
		if err != nil {
			return err
		}
		want, err := cs.RegFilterHeaders.FetchHeaderByHeight(height)
		if err != nil {
			return err
		}
		if header != *want {
			return fmt.Errorf("the filter of block %d does not match its header", height)
		}
		hash, err := cs.GetBlockHash(int64(height))
		if err != nil {
			return err
		}
		data = append(data, &filterdb.FilterData{
			Filter:    filter,
			BlockHash: hash,
			Type:      filterdb.RegularFilter,
		})
		prev = header
	}
	return cs.FilterDB.PutFilters(data...)
}

// splitFilters reads count filters, each after its length as a Bitcoin
// CompactSize, and nothing after them.
func splitFilters(data []byte, count uint32) ([][]byte, error) {
	filters := make([][]byte, 0, count)
	for len(data) > 0 {
		size, n := compactSize(data)
		if n == 0 || uint64(len(data)-n) < size {
			return nil, fmt.Errorf("filter %d is cut short", len(filters))
		}
		filters = append(filters, data[n:n+int(size)])
		data = data[n+int(size):]
	}
	if uint32(len(filters)) != count {
		return nil, fmt.Errorf("%d filters, not %d", len(filters), count)
	}
	return filters, nil
}

// compactSize is the CompactSize at the start of data and its length, or
// a length of 0 if data does not start with one.
func compactSize(data []byte) (uint64, int) {
	if len(data) == 0 {
		return 0, 0
	}
	width := map[byte]int{0xfd: 2, 0xfe: 4, 0xff: 8}[data[0]]
	if width == 0 {
		return uint64(data[0]), 1
	}
	if len(data) < 1+width {
		return 0, 0
	}
	var v uint64
	for i := width; i > 0; i-- {
		v = v<<8 | uint64(data[i])
	}
	return v, 1 + width
}
