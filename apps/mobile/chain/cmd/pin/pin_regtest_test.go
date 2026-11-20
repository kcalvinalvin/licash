//go:build regtest

package main

import (
	"bytes"
	"crypto/sha256"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"testing"
)

func core(t *testing.T) *rpc {
	t.Helper()
	url, container := os.Getenv("LICASH_BITCOIND_RPC"), os.Getenv("LICASH_BITCOIND_CONTAINER")
	if url == "" || container == "" {
		t.Skip("regtest.sh sets LICASH_BITCOIND_RPC and LICASH_BITCOIND_CONTAINER")
	}
	cookie, err := exec.Command("docker", "exec", "-u", "bitcoin", container,
		"sh", "-c", "cat ~/.bitcoin/regtest/.cookie").Output()
	if err != nil {
		t.Fatal(err)
	}
	c := &rpc{url: url, auth: strings.TrimSpace(string(cookie))}
	var count int64
	if err := c.call("getblockcount", nil, &count); err != nil {
		t.Fatal(err)
	}
	// Deep enough for a pin, over more than one batch.
	deep, batch = 20, 50
	if count < 230 {
		var address string
		if err := c.call("getnewaddress", nil, &address); err != nil {
			// No wallet loaded by default: the one regtest.sh made.
			t.Fatal(err)
		}
		var mined []string
		if err := c.call("generatetoaddress", []any{230 - count, address}, &mined); err != nil {
			t.Fatal(err)
		}
	}
	return c
}

// The pin of Core's chain is made, checked against a server serving the
// same files, and written as Go; a server serving other bytes gets no pin.
func TestRegtestAPinIsMadeFromCoreAndTheSameServer(t *testing.T) {
	c := core(t)
	params, _, _ := paramsOf("regtest")
	files, err := fromCore(c, params, -1)
	if err != nil {
		t.Fatal(err)
	}
	var count int64
	_ = c.call("getblockcount", nil, &count)
	if want := (count - deep) / deep * deep; int64(files.pin.Height) != want {
		t.Fatalf("pinned %d, not %d", files.pin.Height, want)
	}
	var hash string
	_ = c.call("getblockhash", []any{files.pin.Height}, &hash)
	if files.pin.Block.String() != hash {
		t.Fatalf("pinned %s, Core says %s", files.pin.Block, hash)
	}
	if len(files.blocks) != 10+80*int(files.pin.Height+1) || len(files.filters) != 10+32*int(files.pin.Height+1) ||
		files.pin.Headers != sha256.Sum256(files.blocks) {
		t.Fatal("the files")
	}

	broken := false
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		end := strconv.Itoa(int(files.pin.Height) + 1)
		switch r.URL.Path {
		case "/headers/import/" + end:
			_, _ = w.Write(files.blocks)
		case "/filter-headers/import/" + end:
			b := bytes.Clone(files.filters)
			if broken {
				b[len(b)/2] ^= 1
			}
			_, _ = w.Write(b)
		default:
			http.NotFound(w, r)
		}
	}))
	defer server.Close()
	if err := sameAsServer(server.URL, files); err != nil {
		t.Fatal(err)
	}
	if err := imports(params, files); err != nil {
		t.Fatal(err)
	}
	var out bytes.Buffer
	if err := render(&out, "regtest", files.pin); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out.String(), fmt.Sprintf("Height:        %d,", files.pin.Height)) {
		t.Fatalf("%s", out.String())
	}
	broken = true
	if err := sameAsServer(server.URL, files); err == nil {
		t.Fatal("a server of other bytes agreed with")
	}
}
