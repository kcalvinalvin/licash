package node

// A release pins a block of each network's chain it ships for, deep when
// pinned: its height and hash, its filter header, and the SHA-256 of the
// two import files of block and filter headers ending there, both
// generated from Bitcoin Core by cmd/pin. A store holding nothing yet is
// filled from those files, made of block-dn's files of headers, every
// byte checked against the pin first, rather than header by header from
// peers: the server is only a transport then. Anything else syncs from
// peers, as without a pin:
//
//	no pin, or no CDN           never asked for
//	store holding headers       never asked for: nothing touched
//	store empty, marked tried   not again: peers
//	store empty, 3 idle starts  marked tried: peers
//	store empty                 downloaded, checked, imported
//
// Several of the server's files download at once, each on circuits of
// its own through a proxy, each kept once whole: a start killed or given
// up on (StartContext) resumes where it was. A start downloading none of
// them in importIdle, or failing importFailures times in a row, leaves
// peers to sync, those downloaded kept should the store still be empty on
// the next start; importStarts starts in a row downloading none mark the
// pin tried. Files other than the pinned, or none served, mark it tried
// at once.
//
// The import files' headers are the node's own, as cmd/pin makes them:
// Mutinynet's server runs as signet (block-dn#33), naming signet's magic
// in its own.

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
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
	// block and of filter headers from genesis to the block.
	Headers       [32]byte
	FilterHeaders [32]byte
}

const (
	// importMeta is the size of an import file's header: the network's
	// magic, a version, the type of headers and the first height.
	importMeta = 10
	// importStall is how long a file may go without a byte before it is
	// asked for again, importIdle how long a start may go without a file
	// downloaded before peers sync.
	importStall = 30 * time.Second
	importIdle  = 10 * time.Minute
	// importLanes is how many files download at once: once none is left
	// to ask for, the one furthest from done is asked for on another lane
	// too, the first whole kept.
	importLanes = 4
	// importStarts is how many starts in a row may download no file, and
	// importFailures how many downloads in a row may fail in a start, with
	// no byte for importQuiet: a network changing, or Tor starting, is
	// waited out.
	importStarts   = 3
	importFailures = 8
)

// importEntries is how many headers each of block-dn's files holds, of
// block or of filter headers, and importQuiet as above: variables for
// tests.
var (
	importEntries uint32 = 100_000
	importQuiet          = 2 * time.Minute
)

// Size is the bytes of the import file of entries of size bytes from
// genesis to the pinned block.
func (p *Pin) size(entry int64) int64 { return importMeta + entry*(int64(p.Height)+1) }

// Bytes is all the pinned import downloads.
func (p *Pin) Bytes() int64 { return p.size(80) + p.size(32) }

var (
	errDigest    = errors.New("not the pinned file")
	errNotServed = errors.New("not served")
	errIdle      = fmt.Errorf("no file downloaded in %v", importIdle)
	errFailing   = fmt.Errorf("%d downloads failed in a row, no byte for a while", importFailures)
)

// pinnedImport is what the node of a network of magic in dir imports from
// server before starting, downloaded and checked against pin now: nothing
// if the table above says peers serve. progress, if set, is told the
// bytes downloaded of all.
func pinnedImport(ctx context.Context, dir, server string, magic wire.BitcoinNet, pin *Pin,
	proxy *Proxy, progress func(done, total int64), log *slog.Logger) *neutrino.HeadersImportConfig {

	if pin == nil || server == "" {
		return nil
	}
	files := filepath.Join(dir, "import")
	if !emptyStore(dir) {
		_ = os.RemoveAll(files)
		return nil
	}
	tried := triedMark(dir, pin)
	if _, err := os.Stat(tried); !errors.Is(err, os.ErrNotExist) {
		_ = os.RemoveAll(files)
		log.Warn("pinned headers tried before, syncing them from peers", "height", pin.Height)
		return nil
	}
	state, ok := readState(files, pin)
	if !ok {
		state = stateOf(pin)
		if err := errors.Join(os.RemoveAll(files), os.MkdirAll(files, 0o700)); err != nil {
			log.Warn("pinned headers unusable, syncing them from peers", "err", err)
			return nil
		}
	}
	if state.Idle >= importStarts {
		_ = os.WriteFile(tried, nil, 0o600)
		_ = os.RemoveAll(files)
		log.Warn("pinned headers not downloaded, syncing them from peers", "starts", state.Idle)
		return nil
	}
	// Counted before downloading: a start killed mid-way counts too.
	state.Idle++
	if err := writeState(files, state); err != nil {
		log.Warn("pinned headers unusable, syncing them from peers", "err", err)
		return nil
	}
	progressed := false
	kept := func() {
		progressed = true
		if state.Idle != 0 {
			state.Idle = 0
			_ = writeState(files, state)
		}
	}
	began := time.Now()
	imp, err := downloadPinned(ctx, files, strings.TrimRight(server, "/"), magic, pin, proxy, progress, kept)
	switch {
	case err == nil:
		log.Info("phase: pinned headers downloaded and checked", "height", pin.Height,
			"took", time.Since(began).Round(time.Millisecond))
		return imp
	case errors.Is(err, errDigest) || errors.Is(err, errNotServed):
		_ = os.WriteFile(tried, nil, 0o600)
		_ = os.RemoveAll(files)
		log.Error("pinned headers refused: the server's files are not the release's, syncing from peers",
			"server", server, "err", err)
	case ctx.Err() != nil:
		// Given up on, not idle: resumed on the next start.
		if !progressed {
			state.Idle--
			_ = writeState(files, state)
		}
	default:
		// Peers sync from here on: the store is no longer empty next time.
		if entries, _ := os.ReadDir(files); entries != nil {
			for _, entry := range entries {
				if entry.Name() != "state" {
					_ = os.Remove(filepath.Join(files, entry.Name()))
				}
			}
		}
		log.Warn("pinned headers not downloaded, syncing from peers", "err", err)
	}
	return nil
}

// ImportDue is whether Start would download cfg.Pin's files before
// starting: a pin and a CDN given, and a store holding nothing, never
// tried, nor idle as many starts as allowed. A store refused for legacy
// files is not due.
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
	if _, err := os.Stat(triedMark(dir, pin)); !errors.Is(err, os.ErrNotExist) {
		return false
	}
	state, ok := readState(filepath.Join(dir, "import"), pin)
	return !ok || state.Idle < importStarts
}

// triedMark is the file marking pin's import tried in the store in dir:
// done with, peers syncing. An older release wrote it before downloading.
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

// importState is what the files in a store's import directory are of,
// written there before any is downloaded: anything there without it, or
// of another pin, is deleted.
type importState struct {
	Height        uint32 `json:"height"`
	Entries       uint32 `json:"entries"`
	Headers       string `json:"headers"`
	FilterHeaders string `json:"filter_headers"`
	// Idle is how many starts in a row downloaded no file.
	Idle int `json:"idle"`
}

func stateOf(pin *Pin) importState {
	return importState{Height: pin.Height, Entries: importEntries,
		Headers: hex.EncodeToString(pin.Headers[:]), FilterHeaders: hex.EncodeToString(pin.FilterHeaders[:])}
}

// readState is the state of the files in files, if of pin.
func readState(files string, pin *Pin) (importState, bool) {
	var state importState
	data, err := os.ReadFile(filepath.Join(files, "state"))
	if err != nil || json.Unmarshal(data, &state) != nil {
		return importState{}, false
	}
	idle := state.Idle
	state.Idle = 0
	if state != stateOf(pin) || idle < 0 {
		return importState{}, false
	}
	state.Idle = idle
	return state, true
}

func writeState(files string, state importState) error {
	data, err := json.Marshal(state)
	if err != nil {
		return err
	}
	path := filepath.Join(files, "state")
	out, err := os.OpenFile(path+".part", os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	_, err = out.Write(data)
	if err = errors.Join(err, out.Sync(), out.Close()); err != nil {
		return err
	}
	return os.Rename(path+".part", path)
}

// downloadPinned fetches both import files of pin, of a network of magic,
// from server's files into files, each checked whole before it is named
// as complete. kept is called as each of the server's files is kept.
func downloadPinned(ctx context.Context, files, server string, magic wire.BitcoinNet, pin *Pin,
	proxy *Proxy, progress func(done, total int64), kept func()) (*neutrino.HeadersImportConfig, error) {

	imp := &neutrino.HeadersImportConfig{
		BlockHeadersSource:  filepath.Join(files, "block_headers.bin"),
		FilterHeadersSource: filepath.Join(files, "filter_headers.bin"),
	}
	kinds := []struct {
		final, path, name string
		entry             int64
		digest            [32]byte
		last              func([]byte) bool
	}{
		{imp.BlockHeadersSource, "headers", "headers", 80, pin.Headers,
			func(entry []byte) bool {
				var header wire.BlockHeader
				return header.Deserialize(bytes.NewReader(entry)) == nil && header.BlockHash() == pin.Block
			}},
		{imp.FilterHeadersSource, "filter-headers", "filters", 32, pin.FilterHeaders,
			func(entry []byte) bool { return bytes.Equal(entry, pin.FilterHeader[:]) }},
	}
	// Parts of a start killed mid-way.
	if parts, _ := filepath.Glob(filepath.Join(files, "*.part")); parts != nil {
		for _, part := range parts {
			_ = os.Remove(part)
		}
	}
	d := &download{server: server, total: pin.Bytes(), progress: progress, kept: kept,
		wake: make(chan struct{})}
	units := make([][]*unit, len(kinds))
	for i, kind := range kinds {
		if whole(kind.final, pin.size(kind.entry)) {
			d.done += pin.size(kind.entry)
			continue
		}
		for start := uint32(0); start <= pin.Height; start += importEntries {
			n := min(importEntries, pin.Height+1-start)
			u := &unit{
				path: fmt.Sprintf("/%s/%d", kind.path, start),
				file: filepath.Join(files, fmt.Sprintf("%s-%d", kind.name, start)),
				size: kind.entry * int64(n),
				full: n == importEntries,
			}
			if u.done = whole(u.file, u.size); u.done {
				d.done, u.got = d.done+u.size, u.size
			} else {
				_ = os.Remove(u.file)
				d.queue = append(d.queue, u)
			}
			units[i] = append(units[i], u)
			d.units = append(d.units, u)
		}
	}
	d.report(nil, 0)
	if err := d.run(ctx, proxy); err != nil {
		return nil, err
	}
	for i, kind := range kinds {
		if units[i] == nil {
			continue
		}
		meta := importMetaOf(magic, byte(i))
		if err := assemble(kind.final, meta, units[i], kind.entry, kind.digest, kind.last); err != nil {
			return nil, fmt.Errorf("/%s: %w", kind.path, err)
		}
		d.report(nil, importMeta)
	}
	return imp, nil
}

// whole is whether the file at path holds size bytes, as one kept does.
func whole(path string, size int64) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular() && info.Size() == size
}

// unit is one of the server's files: asked for at path, up to size bytes,
// all it holds if full, kept at file once whole.
type unit struct {
	path, file string
	size       int64
	full       bool

	// done once kept, got the most bytes any lane has downloaded, running
	// the lanes downloading it, stops theirs.
	done    bool
	got     int64
	running int
	stops   []context.CancelFunc
}

// download is the server's files a start downloads, on several lanes.
type download struct {
	server   string
	total    int64
	progress func(done, total int64)
	kept     func()

	mu       sync.Mutex
	units    []*unit
	queue    []*unit // to ask for, first first
	done     int64
	failures int // in a row
	lastByte time.Time
	wake     chan struct{} // closed as anything changes
	idle     *time.Timer
}

// run downloads every unit not done: nil once all are, else why not.
func (d *download) run(ctx context.Context, proxy *Proxy) error {
	if len(d.queue) == 0 {
		return nil
	}
	ctx, fail := context.WithCancelCause(ctx)
	defer fail(nil)
	d.mu.Lock()
	d.idle = time.AfterFunc(importIdle, func() { fail(errIdle) })
	d.lastByte = time.Now()
	d.mu.Unlock()
	defer d.idle.Stop()
	var lanes sync.WaitGroup
	for lane := range importLanes {
		lanes.Go(func() { d.work(ctx, fail, proxy, lane) })
	}
	lanes.Wait()
	d.mu.Lock()
	defer d.mu.Unlock()
	for _, u := range d.units {
		if !u.done {
			return context.Cause(ctx)
		}
	}
	return nil
}

// work downloads units on one lane until none is left or ctx is done.
// A lane failing goes on circuits of another, the proxy's lanes being
// numbered up to 255.
func (d *download) work(ctx context.Context, fail context.CancelCauseFunc, proxy *Proxy, lane int) {
	failed, renewed := 0, 0
	client := httpClient(proxy.lane(lane), 0)
	defer func() { client.CloseIdleConnections() }()
	for {
		u, attempt, stop := d.next(ctx)
		if u == nil {
			return
		}
		part := fmt.Sprintf("%s.%d.part", u.file, lane)
		err := d.fetch(attempt, client, u, part)
		stop()
		if err = d.ended(u, part, err); err == nil {
			failed = 0
			continue
		}
		if errors.Is(err, errDigest) || errors.Is(err, errNotServed) {
			fail(fmt.Errorf("%s: %w", u.path, err))
			return
		}
		if ctx.Err() != nil {
			return
		}
		if d.failing() {
			fail(fmt.Errorf("%w, the last %s: %w", errFailing, u.path, err))
			return
		}
		failed++
		renewed++
		client.CloseIdleConnections()
		client = httpClient(proxy.lane((lane+importLanes*renewed)%256), 0)
		select {
		case <-ctx.Done():
			return
		case <-time.After(min(250*time.Millisecond<<failed, 15*time.Second)):
		}
	}
}

// next is the unit to download, within what context, and what stops its
// download, once there is one: nil once none is left or ctx is done.
func (d *download) next(ctx context.Context) (*unit, context.Context, context.CancelFunc) {
	d.mu.Lock()
	defer d.mu.Unlock()
	for {
		left := false
		for _, u := range d.units {
			left = left || !u.done
		}
		if !left || ctx.Err() != nil {
			return nil, nil, nil
		}
		if u := d.pick(); u != nil {
			attempt, stop := context.WithCancel(ctx)
			u.running++
			u.stops = append(u.stops, stop)
			return u, attempt, stop
		}
		wake := d.wake
		d.mu.Unlock()
		select {
		case <-wake:
		case <-ctx.Done():
		}
		d.mu.Lock()
	}
}

// pick is the unit to download next: the first asked for, else the one
// furthest from done that a single lane downloads, else none.
func (d *download) pick() *unit {
	for len(d.queue) > 0 {
		u := d.queue[0]
		d.queue = d.queue[1:]
		if !u.done {
			return u
		}
	}
	var furthest *unit
	for _, u := range d.units {
		if !u.done && u.running == 1 && (furthest == nil || u.size-u.got > furthest.size-furthest.got) {
			furthest = u
		}
	}
	return furthest
}

// changed wakes the lanes waiting for a unit: d.mu held.
func (d *download) changed() {
	close(d.wake)
	d.wake = make(chan struct{})
}

// ended is the end of u's download by one lane into part, failed with
// err if set: u kept if whole first, else asked for again unless another
// lane downloads it. nil if another lane kept it.
func (d *download) ended(u *unit, part string, err error) error {
	d.mu.Lock()
	defer d.mu.Unlock()
	defer d.changed()
	u.running--
	if u.done {
		_ = os.Remove(part)
		return nil
	}
	if err == nil {
		err = os.Rename(part, u.file)
	}
	if err != nil {
		_ = os.Remove(part)
		if u.running == 0 {
			d.queue = append([]*unit{u}, d.queue...)
		}
		d.failures++
		return err
	}
	u.done = true
	for _, stop := range u.stops {
		stop()
	}
	u.stops = nil
	d.failures = 0
	d.idle.Reset(importIdle)
	d.done += u.size - u.got
	u.got = u.size
	d.tell()
	d.kept()
	return nil
}

// failing is whether downloads failed too many times in a row.
func (d *download) failing() bool {
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.failures >= importFailures && time.Since(d.lastByte) >= importQuiet
}

// report is that a lane downloaded got bytes of u, or of none.
func (d *download) report(u *unit, got int64) {
	d.mu.Lock()
	defer d.mu.Unlock()
	d.lastByte = time.Now()
	if u == nil {
		d.done += got
	} else if got > u.got && !u.done {
		d.done += got - u.got
		u.got = got
	} else {
		return
	}
	d.tell()
}

// tell tells the progress: d.mu held.
func (d *download) tell() {
	if d.progress != nil {
		d.progress(d.done, d.total)
	}
}

// fetch writes u to part, synced, as the server serves it.
func (d *download) fetch(ctx context.Context, client *http.Client, u *unit, part string) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, d.server+u.path, nil)
	if err != nil {
		return err
	}
	req.Header.Set("Range", fmt.Sprintf("bytes=0-%d", u.size-1))
	// Cancelled if no answer comes, or then no byte for importStall.
	stall := time.AfterFunc(2*proxyTimeout, cancel)
	defer stall.Stop()
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if err := u.served(resp); err != nil {
		return err
	}
	out, err := os.OpenFile(part, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	defer out.Close()
	stall.Reset(importStall)
	body := io.LimitReader(resp.Body, u.size)
	buf := make([]byte, 64<<10)
	var read int64
	for {
		n, err := body.Read(buf)
		if n > 0 {
			stall.Reset(importStall)
			if _, err := out.Write(buf[:n]); err != nil {
				return err
			}
			read += int64(n)
			d.report(u, read)
		}
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return fmt.Errorf("after %d bytes: %w", read, err)
		}
	}
	if read != u.size {
		return fmt.Errorf("cut off after %d bytes of %d", read, u.size)
	}
	return errors.Join(out.Sync(), out.Close())
}

// served is why resp is not u as asked for, if it is not: refused for
// good if the server serves none or another file.
func (u *unit) served(resp *http.Response) error {
	total := resp.ContentLength
	switch code := resp.StatusCode; {
	case code == http.StatusOK:
	case code == http.StatusPartialContent:
		var first, last int64
		served := resp.Header.Get("Content-Range")
		if _, err := fmt.Sscanf(served, "bytes %d-%d/%d", &first, &last, &total); err != nil {
			total = -1
			if _, err := fmt.Sscanf(served, "bytes %d-%d/*", &first, &last); err != nil {
				return fmt.Errorf("%w: served as %q", errDigest, served)
			}
		}
		if first != 0 {
			return fmt.Errorf("%w: served as %q", errDigest, served)
		}
	case code == http.StatusBadRequest || code == http.StatusNotFound || code == http.StatusGone:
		// What block-dn answers for a file it does not have.
		return fmt.Errorf("%w: HTTP %d", errNotServed, code)
	default:
		// Anything else, a CDN's 403 to a Tor exit say, may pass.
		return fmt.Errorf("HTTP %d", code)
	}
	if total >= 0 && (total < u.size || u.full && total != u.size) {
		return fmt.Errorf("%w: %d bytes, not %d", errDigest, total, u.size)
	}
	return nil
}

// assemble names final the import file of units, of entries of entry
// bytes under meta, once of digest and ending in an entry passing last:
// the units deleted then.
func assemble(final string, meta []byte, units []*unit, entry int64, digest [32]byte,
	last func([]byte) bool) error {

	part := final + ".part"
	out, err := os.OpenFile(part, os.O_CREATE|os.O_TRUNC|os.O_RDWR, 0o600)
	if err != nil {
		return err
	}
	defer func() {
		_ = out.Close()
		_ = os.Remove(part)
	}()
	hash := sha256.New()
	to := io.MultiWriter(out, hash)
	if _, err := to.Write(meta); err != nil {
		return err
	}
	size := int64(len(meta))
	for _, u := range units {
		in, err := os.Open(u.file)
		if err != nil {
			return err
		}
		n, err := io.Copy(to, in)
		_ = in.Close()
		if err != nil {
			return err
		}
		if n != u.size {
			return fmt.Errorf("%s: %d bytes kept, not %d", u.file, n, u.size)
		}
		size += n
	}
	if !bytes.Equal(hash.Sum(nil), digest[:]) {
		return fmt.Errorf("%w: its SHA-256 differs", errDigest)
	}
	tail := make([]byte, entry)
	if _, err := out.ReadAt(tail, size-entry); size < int64(len(meta))+entry || err != nil || !last(tail) {
		return fmt.Errorf("%w: it does not end at the pinned block", errDigest)
	}
	if err := errors.Join(out.Sync(), out.Close()); err != nil {
		return err
	}
	if err := os.Rename(part, final); err != nil {
		return err
	}
	for _, u := range units {
		_ = os.Remove(u.file)
	}
	return nil
}

// importMetaOf is the header of an import file of headerType (0 block
// headers, 1 filter headers) of a network of magic, from genesis.
func importMetaOf(magic wire.BitcoinNet, headerType byte) []byte {
	meta := binary.LittleEndian.AppendUint32(nil, uint32(magic))
	meta = append(meta, 0, headerType)
	return binary.LittleEndian.AppendUint32(meta, 0)
}
