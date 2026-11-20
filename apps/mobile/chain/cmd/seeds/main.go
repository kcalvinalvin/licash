// Command seeds writes the fixed seeds of a network: peers that answered
// a handshake offering what the light client needs (blocks, witnesses and
// compact filters), reached when no other peer answers. Run it before a
// release:
//
//	go run ./cmd/seeds -network bitcoin > mobile/seeds_bitcoin.go
//	bitcoin-cli getnodeaddresses 0 | go run ./cmd/seeds -network bitcoin -stdin \
//	    -proxy 127.0.0.1:9050 > mobile/seeds_bitcoin.go
//
// Candidates come from the network's DNS seeds that filter by service,
// and from the standard input: Bitcoin Core's getnodeaddresses output, or
// one host:port a line. Onion candidates are probed only through -proxy.
package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"math/rand/v2"
	"net"
	"os"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/btcsuite/go-socks/socks"
	"github.com/lightninglabs/neutrino"
)

func main() {
	network := flag.String("network", "bitcoin", "bitcoin or signet")
	proxy := flag.String("proxy", "", "SOCKS5 proxy (host:port) to probe through, onion candidates included")
	rounds := flag.Int("rounds", 5, "times each DNS seed is asked: each answers a few of its peers")
	most := flag.Int("max", 48, "the most seeds kept, a random sample of those that answered")
	stdin := flag.Bool("stdin", false, "also read candidates from the standard input")
	flag.Parse()

	params, name, err := paramsOf(*network)
	if err != nil {
		log.Fatal(err)
	}
	port, _ := strconv.Atoi(params.DefaultPort)
	candidates := map[string]bool{}
	for _, seed := range params.DNSSeeds {
		if !seed.HasFiltering {
			continue
		}
		host := fmt.Sprintf("x%x.%s", uint64(neutrino.RequiredServices), seed.Host)
		for range *rounds {
			ips, err := net.LookupIP(host)
			if err != nil {
				log.Printf("%s: %v", host, err)
				break
			}
			for _, ip := range ips {
				candidates[net.JoinHostPort(ip.String(), strconv.Itoa(port))] = true
			}
		}
	}
	if *stdin {
		read, err := fromInput(os.Stdin, port)
		if err != nil {
			log.Fatal(err)
		}
		for _, c := range read {
			candidates[c] = true
		}
	}
	log.Printf("%d candidates", len(candidates))

	good := probeAll(candidates, params, *proxy)
	rand.Shuffle(len(good), func(i, j int) { good[i], good[j] = good[j], good[i] })
	if len(good) > *most {
		good = good[:*most]
	}
	slices.Sort(good)
	log.Printf("%d seeds", len(good))
	if len(good) == 0 {
		log.Fatal("no candidate answered: nothing written")
	}
	os.Stdout.Write(source(name, good, time.Now().UTC()))
}

func paramsOf(network string) (*chaincfg.Params, string, error) {
	switch network {
	case "bitcoin":
		return &chaincfg.MainNetParams, "bitcoin", nil
	case "signet":
		return &chaincfg.SigNetParams, "signet", nil
	}
	return nil, "", fmt.Errorf("no fixed seeds for %q: bitcoin or signet", network)
}

// fromInput is the candidates in r: getnodeaddresses' JSON, offering what
// the light client needs, or one host:port (or host) a line.
func fromInput(r io.Reader, port int) ([]string, error) {
	raw, err := io.ReadAll(r)
	if err != nil {
		return nil, err
	}
	if trimmed := bytes.TrimSpace(raw); len(trimmed) > 0 && trimmed[0] == '[' {
		var nodes []struct {
			Address  string `json:"address"`
			Port     int    `json:"port"`
			Services uint64 `json:"services"`
		}
		if err := json.Unmarshal(trimmed, &nodes); err != nil {
			return nil, err
		}
		var out []string
		for _, n := range nodes {
			if wire.ServiceFlag(n.Services)&neutrino.RequiredServices == neutrino.RequiredServices {
				out = append(out, net.JoinHostPort(n.Address, strconv.Itoa(n.Port)))
			}
		}
		return out, nil
	}
	var out []string
	lines := bufio.NewScanner(bytes.NewReader(raw))
	for lines.Scan() {
		line := strings.TrimSpace(lines.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		if _, _, err := net.SplitHostPort(line); err != nil {
			line = net.JoinHostPort(line, strconv.Itoa(port))
		}
		out = append(out, line)
	}
	return out, lines.Err()
}

// probeAll is the candidates that answer a handshake offering what the
// light client needs.
func probeAll(candidates map[string]bool, params *chaincfg.Params, proxy string) []string {
	var (
		mu   sync.Mutex
		good []string
		wg   sync.WaitGroup
		slot = make(chan struct{}, 32)
	)
	for addr := range candidates {
		onion := strings.HasSuffix(strings.Split(addr, ":")[0], ".onion")
		if onion && proxy == "" {
			continue
		}
		wg.Add(1)
		slot <- struct{}{}
		go func() {
			defer wg.Done()
			defer func() { <-slot }()
			if err := probe(addr, params, proxy); err != nil {
				log.Printf("%s: %v", addr, err)
				return
			}
			mu.Lock()
			good = append(good, addr)
			mu.Unlock()
		}()
	}
	wg.Wait()
	return good
}

// probe is whether addr's version message offers RequiredServices.
func probe(addr string, params *chaincfg.Params, proxy string) error {
	timeout := 15 * time.Second
	var conn net.Conn
	var err error
	if proxy != "" {
		timeout = time.Minute
		conn, err = (&socks.Proxy{Addr: proxy}).DialTimeout("tcp", addr, timeout)
	} else {
		conn, err = net.DialTimeout("tcp", addr, timeout)
	}
	if err != nil {
		return err
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(timeout))
	none := wire.NewNetAddressIPPort(net.IPv4zero, 0, 0)
	version := wire.NewMsgVersion(none, none, rand.Uint64(), 0)
	version.ProtocolVersion = int32(wire.ProtocolVersion)
	version.Services = 0
	if err := wire.WriteMessage(conn, version, wire.ProtocolVersion, params.Net); err != nil {
		return err
	}
	for range 4 {
		msg, _, err := wire.ReadMessage(conn, wire.ProtocolVersion, params.Net)
		if err != nil {
			return err
		}
		if v, ok := msg.(*wire.MsgVersion); ok {
			if v.Services&neutrino.RequiredServices != neutrino.RequiredServices {
				return fmt.Errorf("offers %v", v.Services)
			}
			return nil
		}
	}
	return fmt.Errorf("no version message")
}

// source is the Go file of the seeds of network.
func source(network string, seeds []string, at time.Time) []byte {
	var b bytes.Buffer
	fmt.Fprintf(&b, "// Code generated by cmd/seeds on %s; DO NOT EDIT.\n\n", at.Format("2006-01-02"))
	b.WriteString("package mobile\n\n")
	fmt.Fprintf(&b, "// %sSeeds are peers that offered compact filters when generated.\n", network)
	fmt.Fprintf(&b, "var %sSeeds = []string{\n", network)
	for _, s := range seeds {
		fmt.Fprintf(&b, "\t%q,\n", s)
	}
	b.WriteString("}\n")
	return b.Bytes()
}
