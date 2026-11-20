// Command pin makes the block a release vouches for (node.Pin), as a Go
// file for package mobile, from a Bitcoin Core node of the network:
//
//	go run ./cmd/pin -network bitcoin -rpc http://127.0.0.1:8332 \
//	    -cookie ~/.bitcoin/.cookie > mobile/pin_bitcoin.go
//
// or, without Core, from the network's peers and an Esplora server:
//
//	go run ./cmd/pin -network bitcoin \
//	    -esplora https://esplora.example/api > mobile/pin_bitcoin.go
//
// Core must be out of its initial download with its basic block filter
// index synced (-blockfilterindex=1). The pinned block is at least 1000
// deep, at a multiple of 1000. Both import files, of block headers and of
// filter headers, are made from Core alone, every entry from genesis to
// the block, or from what peers serve a light client (fromPeers); then the
// CDN's are downloaded and must be the same bytes, and the files are
// imported into an empty store as the app would: no file is written
// unless all of it holds.
package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"text/template"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/chainhash/v2"

	"github.com/licash-org/licash/apps/mobile/chain/node"
)

// deep is how deep the pinned block is at least, and what it is a
// multiple of; batch how many blocks Core is asked of at once.
var deep, batch int64 = 1000, 1000

func main() {
	network := flag.String("network", "bitcoin", "bitcoin or signet")
	rpc := flag.String("rpc", "", "Core's RPC URL, such as http://127.0.0.1:8332")
	cookie := flag.String("cookie", "", "Core's .cookie file, or set -user")
	user := flag.String("user", "", "RPC user:password, if no cookie")
	cdn := flag.String("cdn", "", "block-dn server to compare with (the network's by default)")
	esplora := flag.String("esplora", "", "without Core: an Esplora API telling the chain, such as https://esplora.example/api")
	height := flag.Int64("height", -1, "the block to pin (by default the deepest multiple of 1000, at least 1000 deep)")
	flag.Parse()
	if err := run(*network, *rpc, *cookie, *user, *esplora, *cdn, *height, os.Stdout); err != nil {
		fmt.Fprintln(os.Stderr, "pin:", err)
		os.Exit(1)
	}
}

func run(network, rpcURL, cookie, user, esplora, server string, height int64, out io.Writer) error {
	params, defaultCDN, err := paramsOf(network)
	if err != nil {
		return err
	}
	if server == "" {
		server = defaultCDN
	}
	auth := user
	if cookie != "" {
		b, err := os.ReadFile(cookie)
		if err != nil {
			return err
		}
		auth = strings.TrimSpace(string(b))
	}
	var files *importFiles
	switch {
	case (rpcURL == "") == (esplora == ""):
		return errors.New("-rpc or -esplora, one of them")
	case esplora != "":
		var connect []string
		if network == "mutinynet" { // No DNS seed.
			connect = []string{node.MutinynetPeer}
		}
		files, err = fromPeers(params, esplora, height, connect)
	default:
		files, err = fromCore(&rpc{url: rpcURL, auth: auth}, params, height)
	}
	if err != nil {
		return err
	}
	fmt.Fprintf(os.Stderr, "pin: block %d %s\n", files.pin.Height, files.pin.Block)
	if err := sameAsServer(server, files); err != nil {
		return err
	}
	if err := imports(params, files); err != nil {
		return err
	}
	return render(out, network, files.pin)
}

func paramsOf(network string) (*chaincfg.Params, string, error) {
	switch network {
	case "bitcoin":
		return &chaincfg.MainNetParams, "https://block-dn.org", nil
	case "signet":
		return &chaincfg.SigNetParams, "https://signet.block-dn.org", nil
	case "mutinynet":
		params := node.Mutinynet()
		return &params, "https://mutinynet.block-dn.org", nil
	case "regtest": // For its tests: a CDN given.
		return &chaincfg.RegressionNetParams, "", nil
	}
	return nil, "", fmt.Errorf("unknown network %q: bitcoin, signet or mutinynet", network)
}

// importFiles are the two files of a pin and the pin.
type importFiles struct {
	blocks, filters []byte
	pin             node.Pin
}

// fromCore makes the import files to height, or the deepest multiple of
// deep at least deep under Core's tip if negative, from Core's best chain.
func fromCore(core *rpc, params *chaincfg.Params, height int64) (*importFiles, error) {
	var info struct {
		Chain  string `json:"chain"`
		Blocks int64  `json:"blocks"`
		IBD    bool   `json:"initialblockdownload"`
	}
	if err := core.call("getblockchaininfo", nil, &info); err != nil {
		return nil, err
	}
	if info.IBD {
		return nil, errors.New("Core is still in its initial block download")
	}
	var genesis string
	if err := core.call("getblockhash", []any{0}, &genesis); err != nil {
		return nil, err
	}
	if genesis != params.GenesisHash.String() {
		return nil, fmt.Errorf("Core follows %s, not %s", info.Chain, params.Name)
	}
	var index map[string]struct {
		Synced bool  `json:"synced"`
		Best   int64 `json:"best_block_height"`
	}
	if err := core.call("getindexinfo", []any{"basic block filter index"}, &index); err != nil {
		return nil, err
	}
	if !index["basic block filter index"].Synced {
		return nil, errors.New("Core's basic block filter index is not synced: -blockfilterindex=1")
	}
	if height < 0 {
		height = (info.Blocks - deep) / deep * deep
	}
	if height < 0 || height > info.Blocks-deep {
		return nil, fmt.Errorf("block %d is not %d deep under Core's %d", height, deep, info.Blocks)
	}
	f := &importFiles{
		blocks:  importMeta(params, 0),
		filters: importMeta(params, 1),
	}
	for first := int64(0); first <= height; first += batch {
		last := min(first+batch-1, height)
		var hashes []string
		if err := core.batch("getblockhash", first, last, func(h int64) []any { return []any{h} }, &hashes); err != nil {
			return nil, err
		}
		var headers, filters []json.RawMessage
		if err := core.batchOf("getblockheader", hashes, func(h string) []any { return []any{h, false} }, &headers); err != nil {
			return nil, err
		}
		if err := core.batchOf("getblockfilter", hashes, func(h string) []any { return []any{h, "basic"} }, &filters); err != nil {
			return nil, err
		}
		for i := range hashes {
			var headerHex string
			var filter struct{ Header string }
			if err := json.Unmarshal(headers[i], &headerHex); err != nil {
				return nil, err
			}
			if err := json.Unmarshal(filters[i], &filter); err != nil {
				return nil, err
			}
			header, err := hex.DecodeString(headerHex)
			if err != nil || len(header) != 80 {
				return nil, fmt.Errorf("block %d: not a header", first+int64(i))
			}
			filterHeader, err := chainhash.NewHashFromStr(filter.Header)
			if err != nil {
				return nil, err
			}
			f.blocks = append(f.blocks, header...)
			f.filters = append(f.filters, filterHeader[:]...)
			if first+int64(i) == height {
				hash, err := chainhash.NewHashFromStr(hashes[i])
				if err != nil {
					return nil, err
				}
				f.pin.Block, f.pin.FilterHeader = *hash, *filterHeader
			}
		}
		if last%100_000 == 99_999 || last == height {
			fmt.Fprintf(os.Stderr, "pin: read to block %d of %d\n", last, height)
		}
	}
	f.pin.Height = uint32(height)
	f.pin.Headers = sha256.Sum256(f.blocks)
	f.pin.FilterHeaders = sha256.Sum256(f.filters)
	return f, nil
}

// importMeta is the header of an import file of headerType (0 block
// headers, 1 filter headers) of params' network, from genesis.
func importMeta(params *chaincfg.Params, headerType byte) []byte {
	m := uint32(params.Net)
	return []byte{byte(m), byte(m >> 8), byte(m >> 16), byte(m >> 24), 0, headerType, 0, 0, 0, 0}
}

// sameAsServer is an error unless server serves both files byte for byte.
func sameAsServer(server string, f *importFiles) error {
	end := f.pin.Height + 1
	for path, want := range map[string][]byte{
		fmt.Sprintf("/headers/import/%d", end):        f.blocks,
		fmt.Sprintf("/filter-headers/import/%d", end): f.filters,
	} {
		resp, err := http.Get(strings.TrimRight(server, "/") + path)
		if err != nil {
			return err
		}
		got, err := io.ReadAll(io.LimitReader(resp.Body, int64(len(want))+1))
		resp.Body.Close()
		if err != nil {
			return err
		}
		if resp.StatusCode != http.StatusOK {
			return fmt.Errorf("%s%s: HTTP %d", server, path, resp.StatusCode)
		}
		if sha256.Sum256(got) != sha256.Sum256(want) {
			return fmt.Errorf("%s%s is not Core's chain: no pin", server, path)
		}
	}
	fmt.Fprintf(os.Stderr, "pin: %s serves the same files\n", server)
	return nil
}

// imports is an error unless an empty store holds the pinned block once
// started with the files, as the app's would.
func imports(params *chaincfg.Params, f *importFiles) error {
	end := fmt.Sprint(f.pin.Height + 1)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/headers/import/" + end:
			_, _ = w.Write(f.blocks)
		case "/filter-headers/import/" + end:
			_, _ = w.Write(f.filters)
		default:
			http.NotFound(w, r)
		}
	}))
	defer server.Close()
	dir, err := os.MkdirTemp("", "pin-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(dir)
	var logged bytes.Buffer
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Minute)
	defer cancel()
	n, err := node.StartContext(ctx, node.Config{
		DataDir: dir,
		Params:  *params,
		// No peer: the import alone.
		ConnectPeers: []string{"127.0.0.1:1"},
		CDN:          server.URL,
		Pin:          &f.pin,
		Log:          slog.New(slog.NewTextHandler(&logged, nil)),
	})
	if err != nil {
		return err
	}
	defer n.Stop()
	held, err := n.Headers()
	if err != nil {
		return err
	}
	hash, err := n.BlockHash(f.pin.Height)
	if err != nil || held != int32(f.pin.Height) || hash != f.pin.Block {
		return fmt.Errorf("the files do not import: %v\n%s", err, logged.String())
	}
	fmt.Fprintln(os.Stderr, "pin: the files import")
	return nil
}

var file = template.Must(template.New("").Parse(`// Code generated by cmd/pin; DO NOT EDIT.

package mobile

import (
	"github.com/btcsuite/btcd/chainhash/v2"

	"github.com/licash-org/licash/apps/mobile/chain/node"
)

func init() {
	// Block {{.Height}} of {{.Network}}, its import files made apart from
	// block-dn (from Bitcoin Core, or from peers and an Esplora server) and
	// the same as block-dn's.
	{{.Network}}Pin = &node.Pin{
		Height:        {{.Height}},
		Block:         {{.Block}},
		FilterHeader:  {{.FilterHeader}},
		Headers:       {{.Headers}},
		FilterHeaders: {{.FilterHeaders}},
	}
}
`))

func render(out io.Writer, network string, pin node.Pin) error {
	bytesOf := func(b []byte) string {
		parts := make([]string, len(b))
		for i, x := range b {
			parts[i] = fmt.Sprintf("0x%02x", x)
		}
		return "{" + strings.Join(parts, ", ") + "}"
	}
	return file.Execute(out, map[string]any{
		"Network":       network,
		"Height":        pin.Height,
		"Block":         "chainhash.Hash" + bytesOf(pin.Block[:]),
		"FilterHeader":  "chainhash.Hash" + bytesOf(pin.FilterHeader[:]),
		"Headers":       "[32]byte" + bytesOf(pin.Headers[:]),
		"FilterHeaders": "[32]byte" + bytesOf(pin.FilterHeaders[:]),
	})
}

// rpc is Core's JSON-RPC interface.
type rpc struct {
	url, auth string
}

type request struct {
	JSONRPC string `json:"jsonrpc"`
	ID      int    `json:"id"`
	Method  string `json:"method"`
	Params  []any  `json:"params"`
}

type response struct {
	ID     int             `json:"id"`
	Result json.RawMessage `json:"result"`
	Error  *struct {
		Message string `json:"message"`
	} `json:"error"`
}

func (c *rpc) post(body any) ([]byte, error) {
	b, err := json.Marshal(body)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequest(http.MethodPost, c.url, bytes.NewReader(b))
	if err != nil {
		return nil, err
	}
	if user, password, ok := strings.Cut(c.auth, ":"); ok {
		req.SetBasicAuth(user, password)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	out, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode == http.StatusUnauthorized {
		return nil, errors.New("Core refused the RPC credentials")
	}
	return out, nil
}

func (c *rpc) call(method string, params []any, result any) error {
	if params == nil {
		params = []any{}
	}
	b, err := c.post(request{"2.0", 0, method, params})
	if err != nil {
		return err
	}
	var r response
	if err := json.Unmarshal(b, &r); err != nil {
		return fmt.Errorf("%s: %w", method, err)
	}
	if r.Error != nil {
		return fmt.Errorf("%s: %s", method, r.Error.Message)
	}
	return json.Unmarshal(r.Result, result)
}

// calls makes one request of method for each of params, in order.
func (c *rpc) calls(method string, params [][]any) ([]json.RawMessage, error) {
	reqs := make([]request, len(params))
	for i, p := range params {
		reqs[i] = request{"2.0", i, method, p}
	}
	b, err := c.post(reqs)
	if err != nil {
		return nil, err
	}
	var resps []response
	if err := json.Unmarshal(b, &resps); err != nil {
		return nil, fmt.Errorf("%s: %w", method, err)
	}
	if len(resps) != len(params) {
		return nil, fmt.Errorf("%s: %d answers to %d", method, len(resps), len(params))
	}
	out := make([]json.RawMessage, len(params))
	for _, r := range resps {
		if r.Error != nil {
			return nil, fmt.Errorf("%s: %s", method, r.Error.Message)
		}
		if r.ID < 0 || r.ID >= len(out) {
			return nil, fmt.Errorf("%s: an answer to no request", method)
		}
		out[r.ID] = r.Result
	}
	return out, nil
}

func (c *rpc) batch(method string, first, last int64, param func(int64) []any, out *[]string) error {
	var params [][]any
	for h := first; h <= last; h++ {
		params = append(params, param(h))
	}
	results, err := c.calls(method, params)
	if err != nil {
		return err
	}
	*out = make([]string, len(results))
	for i, r := range results {
		if err := json.Unmarshal(r, &(*out)[i]); err != nil {
			return err
		}
	}
	return nil
}

func (c *rpc) batchOf(method string, of []string, param func(string) []any, out *[]json.RawMessage) error {
	params := make([][]any, len(of))
	for i, x := range of {
		params[i] = param(x)
	}
	results, err := c.calls(method, params)
	*out = results
	return err
}
