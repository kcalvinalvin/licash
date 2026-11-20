package node

import (
	"encoding/json"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
)

// fakeSocks is a SOCKS5 proxy taking one username and password, CONNECT
// to its names and Tor's RESOLVE of them: what it was asked, in order.
type fakeSocks struct {
	listener net.Listener
	names    map[string]string // name -> address (host:port for CONNECT, IP for RESOLVE)

	mu    sync.Mutex
	asked []string
}

func newFakeSocks(t *testing.T, names map[string]string) *fakeSocks {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	f := &fakeSocks{listener: listener, names: names}
	t.Cleanup(func() { listener.Close() })
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go f.serve(conn)
		}
	}()
	return f
}

func (f *fakeSocks) proxy(password string) *Proxy {
	return &Proxy{Addr: f.listener.Addr().String(), Username: "user", Password: password}
}

func (f *fakeSocks) serve(conn net.Conn) {
	defer conn.Close()
	head := make([]byte, 2)
	if _, err := io.ReadFull(conn, head); err != nil {
		return
	}
	methods := make([]byte, head[1])
	io.ReadFull(conn, methods)
	conn.Write([]byte{5, 2})
	io.ReadFull(conn, head)
	user := make([]byte, head[1])
	io.ReadFull(conn, user)
	io.ReadFull(conn, head[:1])
	password := make([]byte, head[0])
	io.ReadFull(conn, password)
	if string(user) != "user" || string(password) != "secret" {
		conn.Write([]byte{1, 1})
		return
	}
	conn.Write([]byte{1, 0})
	request := make([]byte, 5)
	if _, err := io.ReadFull(conn, request); err != nil || request[3] != 3 {
		return
	}
	name := make([]byte, request[4]+2)
	io.ReadFull(conn, name)
	host := string(name[:len(name)-2])
	port := int(name[len(name)-2])<<8 | int(name[len(name)-1])
	f.mu.Lock()
	f.asked = append(f.asked, map[byte]string{1: "connect ", 0xf0: "resolve "}[request[1]]+host)
	f.mu.Unlock()
	to, known := f.names[host]
	switch {
	case !known:
		conn.Write([]byte{5, 4, 0, 1, 0, 0, 0, 0, 0, 0})
	case request[1] == 0xf0:
		reply := append([]byte{5, 0, 0, 1}, net.ParseIP(to).To4()...)
		conn.Write(append(reply, 0, 0))
	case request[1] == 1:
		_ = port
		out, err := net.Dial("tcp", to)
		if err != nil {
			conn.Write([]byte{5, 4, 0, 1, 0, 0, 0, 0, 0, 0})
			return
		}
		defer out.Close()
		conn.Write([]byte{5, 0, 0, 1, 0, 0, 0, 0, 0, 0})
		go io.Copy(out, conn)
		io.Copy(conn, out)
	}
}

func (f *fakeSocks) askedOf() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string(nil), f.asked...)
}

func TestNamesResolveAtTheProxysEnd(t *testing.T) {
	f := newFakeSocks(t, map[string]string{"seed.example": "192.0.2.7"})
	ips, err := f.proxy("secret").Lookup("seed.example")
	if err != nil || len(ips) != 1 || !ips[0].Equal(net.ParseIP("192.0.2.7")) {
		t.Fatalf("seed.example: %v, %v", ips, err)
	}
	if _, err := f.proxy("secret").Lookup("none.example"); err == nil {
		t.Fatal("none.example resolved")
	}
	if _, err := f.proxy("wrong").Lookup("seed.example"); err == nil {
		t.Fatal("resolved with a wrong password")
	}
	// An address is itself, the proxy not asked.
	ips, err = f.proxy("secret").Lookup("198.51.100.1")
	if err != nil || !ips[0].Equal(net.ParseIP("198.51.100.1")) {
		t.Fatalf("an address: %v, %v", ips, err)
	}
	want := []string{"resolve seed.example", "resolve none.example"}
	if got := f.askedOf(); strings.Join(got, ",") != strings.Join(want, ",") {
		t.Fatalf("asked %v, want %v", got, want)
	}
}

func TestTheCDNsFilesComeThroughTheProxy(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/status" {
			_ = json.NewEncoder(w).Encode(cdnStatus{Genesis: chaincfg.RegressionNetParams.GenesisHash.String(), PerFilterFile: 20})
			return
		}
		io.WriteString(w, "filters of "+r.URL.Path)
	}))
	defer server.Close()
	f := newFakeSocks(t, map[string]string{"cdn.example": server.Listener.Addr().String()})
	c, err := newCDN("http://cdn.example", &chaincfg.RegressionNetParams, f.proxy("secret"), slog.New(slog.DiscardHandler))
	if err != nil {
		t.Fatal(err)
	}
	response, err := c.client.Get(c.base + "/filters/0")
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil || string(body) != "filters of /filters/0" {
		t.Fatalf("filter file: %q, %v", body, err)
	}
	for _, asked := range f.askedOf() {
		if asked != "connect cdn.example" {
			t.Errorf("asked %s", asked)
		}
	}
	if len(f.askedOf()) == 0 {
		t.Error("nothing went through the proxy")
	}
}

func TestPeersAreReachedThroughTheProxy(t *testing.T) {
	peer, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer peer.Close()
	go func() {
		conn, err := peer.Accept()
		if err == nil {
			conn.Write([]byte("version"))
			conn.Close()
		}
	}()
	f := newFakeSocks(t, map[string]string{"203.0.113.9": peer.Addr().String()})
	addr := &net.TCPAddr{IP: net.ParseIP("203.0.113.9"), Port: 38333}
	conn, err := dialer(f.proxy("secret"))(addr)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	got, _ := io.ReadAll(conn)
	if string(got) != "version" {
		t.Fatalf("read %q", got)
	}
	if dialer(nil) != nil || resolver(nil) != nil {
		t.Fatal("no proxy, yet not neutrino's own ways")
	}
}

// An onion peer, as neutrino hands its dialer one learned by gossip or
// asked for, is reached by its name at the proxy's end.
func TestOnionPeersAreReachedByNameThroughTheProxy(t *testing.T) {
	onion := "pg6mmjiyjmcrsslvykfwnntlaru7p5svn6y2ymmju6nubxndf4pscryd.onion"
	f := newFakeSocks(t, map[string]string{})
	n, err := Start(Config{DataDir: t.TempDir(), Params: chaincfg.RegressionNetParams, Proxy: f.proxy("secret")})
	if err != nil {
		t.Fatal(err)
	}
	defer n.Stop()
	if err := n.cs.ConnectNode(onion+":18444", false); err != nil {
		t.Fatal(err)
	}
	for deadline := time.Now().Add(10 * time.Second); time.Now().Before(deadline); time.Sleep(50 * time.Millisecond) {
		if slices.Contains(f.askedOf(), "connect "+onion) {
			return
		}
	}
	t.Fatalf("the proxy was asked %v", f.askedOf())
}
