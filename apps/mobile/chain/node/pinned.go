package node

// A release pins a block of each network's chain it ships for, deep when
// pinned: its height and hash, its filter header, and the SHA-256 of the
// two import files block-dn serves ending there, both generated from
// Bitcoin Core by cmd/pin and found identical to the server's. A store
// holding nothing yet is filled from those files, every byte checked
// against the pin first, rather than header by header from peers: the
// server is only a transport then. Anything else syncs from peers, as
// without a pin:
//
//	no pin, or no CDN           never asked for
//	store holding headers       never asked for: nothing touched
//	store empty, tried before   not again: peers (a start killed mid-way)
//	store empty                 marked tried, downloaded, checked, imported
//
// A download the caller gives up on (StartContext) is unmarked: the next
// start tries again.
//
// Any failure falls back to peers, logged. The files are deleted once
// imported or refused.

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/lightninglabs/neutrino"
)

// Pin is a block of a network's chain a release vouches for.
type Pin struct {
	Height uint32
	// Block is the block's hash, FilterHeader its basic filter header,
	// both in Bitcoin's internal order.
	Block        chainhash.Hash
	FilterHeader chainhash.Hash
	// Headers and FilterHeaders are the SHA-256 of the import files of
	// block and of filter headers from genesis to the block, as served.
	Headers       [32]byte
	FilterHeaders [32]byte
}

const (
	// importMeta is the size of an import file's header: the network's
	// magic, a version, the type of headers and the first height.
	importMeta = 10
	// importStall is how long a download may go without a byte, and
	// importDeadline how long it may take in all, before peers serve.
	importStall    = time.Minute
	importDeadline = 30 * time.Minute
)

// Size is the bytes of the import file of entries of size bytes from
// genesis to the pinned block.
func (p *Pin) size(entry int64) int64 { return importMeta + entry*(int64(p.Height)+1) }

// Bytes is all the pinned import downloads.
func (p *Pin) Bytes() int64 { return p.size(80) + p.size(32) }

// pinnedImport is what the node in dir imports from server before
// starting, downloaded and checked against pin now: nothing if the
// table above says peers serve. progress, if set, is told the bytes
// downloaded of all.
func pinnedImport(ctx context.Context, dir, server string, pin *Pin, proxy *Proxy,
	progress func(done, total int64), log *slog.Logger) *neutrino.HeadersImportConfig {

	if pin == nil || server == "" {
		return nil
	}
	files := filepath.Join(dir, "import")
	if !emptyStore(dir) {
		_ = os.RemoveAll(files)
		return nil
	}
	tried := triedMark(dir, pin)
	if !importDue(dir, server, pin) {
		_ = os.RemoveAll(files)
		log.Warn("pinned headers tried before, syncing them from peers", "height", pin.Height)
		return nil
	}
	// Marked before downloading: a start killed mid-way does not download
	// again on every start.
	if err := os.WriteFile(tried, nil, 0o600); err != nil {
		log.Warn("pinned headers unusable, syncing them from peers", "err", err)
		return nil
	}
	began := time.Now()
	imp, err := downloadPinned(ctx, files, strings.TrimRight(server, "/"), pin, proxy, progress)
	if err != nil {
		_ = os.RemoveAll(files)
		if errors.Is(err, errDigest) {
			log.Error("pinned headers refused: the server's files are not the release's, syncing from peers",
				"server", server, "err", err)
		} else {
			log.Warn("pinned headers not downloaded, syncing from peers", "err", err)
		}
		return nil
	}
	log.Info("phase: pinned headers downloaded and checked", "height", pin.Height,
		"took", time.Since(began).Round(time.Millisecond))
	return imp
}

// ImportDue is whether Start would download cfg.Pin's files before
// starting: a pin and a CDN given, and a store holding nothing, never
// tried. A store refused for legacy files is not due.
func ImportDue(cfg Config) bool {
	if _, err := os.Stat(filepath.Join(cfg.DataDir, "block_headers.bin")); err == nil {
		return false
	}
	return importDue(filepath.Join(cfg.DataDir, "peer-headers-v1"), cfg.CDN, cfg.Pin)
}

func importDue(dir, server string, pin *Pin) bool {
	if pin == nil || server == "" || !emptyStore(dir) {
		return false
	}
	_, err := os.Stat(triedMark(dir, pin))
	return errors.Is(err, os.ErrNotExist)
}

// triedMark is the file marking pin's import tried in the store in dir.
func triedMark(dir string, pin *Pin) string {
	return filepath.Join(dir, fmt.Sprintf("import-%d.tried", pin.Height))
}

// emptyStore is whether the store in dir holds no header past genesis,
// from the sizes of neutrino's flat files of them. Anything unexpected is
// not empty.
func emptyStore(dir string) bool {
	for name, genesis := range map[string]int64{"block_headers.bin": 80, "reg_filter_headers.bin": 32} {
		info, err := os.Stat(filepath.Join(dir, name))
		switch {
		case errors.Is(err, os.ErrNotExist):
		case err != nil || info.Size() != genesis:
			return false
		}
	}
	return true
}

var errDigest = errors.New("not the pinned file")

// downloadPinned fetches both files of pin from server into files, each
// checked whole before it is named as complete.
func downloadPinned(ctx context.Context, files, server string, pin *Pin, proxy *Proxy,
	progress func(done, total int64)) (*neutrino.HeadersImportConfig, error) {

	ctx, cancel := context.WithTimeout(ctx, importDeadline)
	defer cancel()
	if err := os.RemoveAll(files); err != nil {
		return nil, err
	}
	if err := os.MkdirAll(files, 0o700); err != nil {
		return nil, err
	}
	client := httpClient(proxy, 0)
	total, done := pin.Bytes(), int64(0)
	tell := func(n int64) {
		done += n
		if progress != nil {
			progress(done, total)
		}
	}
	end := pin.Height + 1
	parts := []struct {
		path, name string
		size       int64
		digest     [32]byte
		last       func([]byte) bool
	}{
		{fmt.Sprintf("/headers/import/%d", end), "block_headers", pin.size(80), pin.Headers,
			func(tail []byte) bool {
				var header wire.BlockHeader
				return header.Deserialize(bytes.NewReader(tail[len(tail)-80:])) == nil &&
					header.BlockHash() == pin.Block
			}},
		{fmt.Sprintf("/filter-headers/import/%d", end), "filter_headers", pin.size(32), pin.FilterHeaders,
			func(tail []byte) bool { return bytes.Equal(tail[len(tail)-32:], pin.FilterHeader[:]) }},
	}
	imp := &neutrino.HeadersImportConfig{}
	for _, p := range parts {
		final := filepath.Join(files, p.name+".bin")
		if err := fetchChecked(ctx, client, server+p.path, final, p.size, p.digest, p.last, tell); err != nil {
			return nil, fmt.Errorf("%s: %w", p.path, err)
		}
		if p.name == "block_headers" {
			imp.BlockHeadersSource = final
		} else {
			imp.FilterHeadersSource = final
		}
	}
	return imp, nil
}

// fetchChecked writes url's body to final only once it is size bytes of
// SHA-256 digest whose last entry passes last.
func fetchChecked(ctx context.Context, client *http.Client, url, final string, size int64,
	digest [32]byte, last func([]byte) bool, tell func(int64)) error {

	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return err
	}
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("HTTP %d", resp.StatusCode)
	}
	if resp.ContentLength >= 0 && resp.ContentLength != size {
		return fmt.Errorf("%w: %d bytes, not %d", errDigest, resp.ContentLength, size)
	}
	part := final + ".part"
	out, err := os.OpenFile(part, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	defer func() {
		_ = out.Close()
		_ = os.Remove(part)
	}()
	// A stalled download cancelled rather than waited on.
	stall := time.AfterFunc(importStall, cancel)
	defer stall.Stop()
	hash := sha256.New()
	tail := make([]byte, 0, 2*80)
	body := io.LimitReader(resp.Body, size+1)
	buf := make([]byte, 256<<10)
	var read int64
	for {
		n, err := body.Read(buf)
		if n > 0 {
			stall.Reset(importStall)
			read += int64(n)
			if read > size {
				return fmt.Errorf("%w: longer than %d bytes", errDigest, size)
			}
			hash.Write(buf[:n])
			if _, err := out.Write(buf[:n]); err != nil {
				return err
			}
			tail = append(tail, buf[:n]...)
			if len(tail) > 80 {
				tail = append(tail[:0], tail[len(tail)-80:]...)
			}
			tell(int64(n))
		}
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			if ctx.Err() != nil && read < size {
				return fmt.Errorf("stalled or too slow after %d bytes: %w", read, ctx.Err())
			}
			return err
		}
	}
	if read != size {
		return fmt.Errorf("%w: %d bytes, not %d", errDigest, read, size)
	}
	if !bytes.Equal(hash.Sum(nil), digest[:]) {
		return fmt.Errorf("%w: its SHA-256 differs", errDigest)
	}
	if size < importMeta+80 && len(tail) < 80 || !last(tail) {
		return fmt.Errorf("%w: it does not end at the pinned block", errDigest)
	}
	if err := out.Sync(); err != nil {
		return err
	}
	if err := out.Close(); err != nil {
		return err
	}
	return os.Rename(part, final)
}

// importMetaOf is the header of an import file of headerType (0 block
// headers, 1 filter headers) of a network of magic, from genesis.
func importMetaOf(magic wire.BitcoinNet, headerType byte) []byte {
	meta := binary.LittleEndian.AppendUint32(nil, uint32(magic))
	meta = append(meta, 0, headerType)
	return binary.LittleEndian.AppendUint32(meta, 0)
}
