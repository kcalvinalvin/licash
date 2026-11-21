package main

import (
	"bytes"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/licash-org/licash/apps/mobile/chain/node"
)

// The files are served as block-dn's: one per node.ImportEntries headers
// from genesis, the last shorter, each asked for whole as the app asks.
func TestTheFilesAreServedAsBlockDNs(t *testing.T) {
	const last = 50
	height := uint32(node.ImportEntries + last - 1)
	entry := func(i, size int) []byte {
		return bytes.Repeat([]byte{byte(i), byte(i >> 8), byte(i >> 16)}, size)[:size]
	}
	f := &importFiles{pin: node.Pin{Height: height},
		blocks: make([]byte, importMetaSize), filters: make([]byte, importMetaSize)}
	for i := 0; i <= int(height); i++ {
		f.blocks = append(f.blocks, entry(i, 80)...)
		f.filters = append(f.filters, entry(i, 32)...)
	}
	server := httptest.NewServer(serveFiles(f))
	defer server.Close()

	for _, tc := range []struct {
		path        string
		first, size int
	}{
		{"/headers/0", 0, 80},
		{fmt.Sprintf("/headers/%d", node.ImportEntries), node.ImportEntries, 80},
		{"/filter-headers/0", 0, 32},
		{fmt.Sprintf("/filter-headers/%d", node.ImportEntries), node.ImportEntries, 32},
	} {
		n := min(node.ImportEntries, int(height)+1-tc.first)
		req, _ := http.NewRequest(http.MethodGet, server.URL+tc.path, nil)
		req.Header.Set("Range", fmt.Sprintf("bytes=0-%d", n*tc.size-1))
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		got, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		if want := fmt.Sprintf("bytes 0-%d/%d", n*tc.size-1, n*tc.size); resp.StatusCode != http.StatusPartialContent ||
			resp.Header.Get("Content-Range") != want {
			t.Fatalf("%s: HTTP %d, %q, not %q", tc.path, resp.StatusCode, resp.Header.Get("Content-Range"), want)
		}
		if len(got) != n*tc.size || !bytes.Equal(got[:tc.size], entry(tc.first, tc.size)) ||
			!bytes.Equal(got[len(got)-tc.size:], entry(tc.first+n-1, tc.size)) {
			t.Fatalf("%s: not entries %d to %d", tc.path, tc.first, tc.first+n-1)
		}
	}
	for _, path := range []string{"/headers/1", fmt.Sprintf("/headers/%d", 2*node.ImportEntries),
		"/headers/-100000", "/headers/import/100050", "/blocks/0", "/status"} {
		resp, err := http.Get(server.URL + path)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		if resp.StatusCode != http.StatusNotFound {
			t.Fatalf("%s: HTTP %d", path, resp.StatusCode)
		}
	}
}
