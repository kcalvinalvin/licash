package main

import (
	"context"
	"crypto/sha256"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/chainhash/v2"

	"github.com/licash-org/licash/apps/mobile/chain/node"
)

// poll is how often a sync from peers is looked at.
var poll = 30 * time.Second

// fromPeers makes the import files to height, or the deepest multiple of
// deep at least deep under the tip an Esplora server tells, without Core:
// a light client syncs block and filter headers from the network's peers
// (filter headers checked against several peers' checkpoints) into a
// store of its own, whose flat files hold the very entries of the import
// files; the pinned block's hash must be the one the Esplora server tells
// at that height. connect, if any, are the only peers.
func fromPeers(params *chaincfg.Params, esplora string, height int64, connect []string) (*importFiles, error) {
	esplora = strings.TrimRight(esplora, "/")
	tip, err := esploraGet(esplora + "/blocks/tip/height")
	if err != nil {
		return nil, err
	}
	best, err := strconv.ParseInt(tip, 10, 64)
	if err != nil {
		return nil, fmt.Errorf("the tip %q: %w", tip, err)
	}
	if height < 0 {
		height = (best - deep) / deep * deep
	}
	if height < 0 || height > best-deep {
		return nil, fmt.Errorf("block %d is not %d deep under the tip %d", height, deep, best)
	}

	dir, err := os.MkdirTemp("", "pin-peers-")
	if err != nil {
		return nil, err
	}
	defer os.RemoveAll(dir)
	n, err := node.Start(node.Config{
		DataDir:      dir,
		Params:       *params,
		ConnectPeers: connect,
		Log:          slog.New(slog.NewTextHandler(io.Discard, nil)),
	})
	if err != nil {
		return nil, err
	}
	began := time.Now()
	for {
		at, _, err := n.Tip()
		if err == nil && int64(at) >= height {
			break
		}
		if time.Since(began) > 3*time.Hour {
			_ = n.Stop()
			return nil, fmt.Errorf("peers served filter headers to %d of %d in 3 hours", at, height)
		}
		headers, _ := n.Headers()
		fmt.Fprintf(os.Stderr, "pin: %d peers, block headers %d, filter headers %d of %d\n",
			n.Peers(), headers, at, height)
		time.Sleep(poll)
	}
	if err := n.Stop(); err != nil {
		return nil, err
	}

	f := &importFiles{
		blocks:  importMeta(params, 0),
		filters: importMeta(params, 1),
	}
	store := filepath.Join(dir, "peer-headers-v1")
	for _, part := range []struct {
		name string
		size int64
		into *[]byte
	}{
		{"block_headers.bin", 80, &f.blocks},
		{"reg_filter_headers.bin", 32, &f.filters},
	} {
		held, err := os.ReadFile(filepath.Join(store, part.name))
		if err != nil {
			return nil, err
		}
		want := part.size * (height + 1)
		if int64(len(held)) < want {
			return nil, fmt.Errorf("%s holds %d bytes, not %d", part.name, len(held), want)
		}
		*part.into = append(*part.into, held[:want]...)
	}
	last := f.blocks[len(f.blocks)-80:]
	f.pin.Block = chainhash.DoubleHashH(last)
	copy(f.pin.FilterHeader[:], f.filters[len(f.filters)-32:])
	told, err := esploraGet(fmt.Sprintf("%s/block-height/%d", esplora, height))
	if err != nil {
		return nil, err
	}
	if told != f.pin.Block.String() {
		return nil, fmt.Errorf("peers' block %d is %s, the Esplora server's %s", height, f.pin.Block, told)
	}
	f.pin.Height = uint32(height)
	f.pin.Headers = sha256.Sum256(f.blocks)
	f.pin.FilterHeaders = sha256.Sum256(f.filters)
	return f, nil
}

func esploraGet(url string) (string, error) {
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return "", err
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, 4096))
	if err != nil {
		return "", err
	}
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("%s: HTTP %d", url, resp.StatusCode)
	}
	return strings.TrimSpace(string(body)), nil
}
