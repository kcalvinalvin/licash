package node

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"time"

	"github.com/btcsuite/go-socks/socks"
)

// proxyTimeout bounds reaching a peer or a name through the proxy.
const proxyTimeout = time.Minute

// Proxy is a SOCKS5 proxy, such as Tor's, through which the node reaches
// everything while set: its peers, the names it resolves (with Tor's
// RESOLVE extension) and the block-dn server. Nothing is reached directly
// then.
type Proxy struct {
	// Addr is the proxy's host:port.
	Addr     string
	Username string
	Password string
}

// Dial is a stream to addr, a host:port, through the proxy: the host
// resolved at the proxy's end.
func (p *Proxy) Dial(addr string) (net.Conn, error) {
	proxy := &socks.Proxy{Addr: p.Addr, Username: p.Username, Password: p.Password}
	return proxy.DialTimeout("tcp", addr, proxyTimeout)
}

// Lookup is the address of host, resolved at the proxy's end: one, as
// Tor's RESOLVE answers. An address is itself.
func (p *Proxy) Lookup(host string) ([]net.IP, error) {
	if ip := net.ParseIP(host); ip != nil {
		return []net.IP{ip}, nil
	}
	if len(host) == 0 || len(host) > 255 {
		return nil, fmt.Errorf("not a name to resolve: %q", host)
	}
	conn, err := net.DialTimeout("tcp", p.Addr, proxyTimeout)
	if err != nil {
		return nil, fmt.Errorf("reaching the proxy: %w", err)
	}
	defer conn.Close()
	if err := conn.SetDeadline(time.Now().Add(proxyTimeout)); err != nil {
		return nil, err
	}
	ip, err := p.resolve(conn, host)
	if err != nil {
		return nil, fmt.Errorf("resolving %s through the proxy: %w", host, err)
	}
	return []net.IP{ip}, nil
}

// resolve asks the proxy at conn for the address of host.
func (p *Proxy) resolve(conn net.Conn, host string) (net.IP, error) {
	if len(p.Username) > 255 || len(p.Password) > 255 {
		return nil, errors.New("a username or password over 255 bytes")
	}
	// Username and password only (RFC 1929).
	if _, err := conn.Write([]byte{5, 1, 2}); err != nil {
		return nil, err
	}
	reply := make([]byte, 2)
	if _, err := io.ReadFull(conn, reply); err != nil {
		return nil, err
	}
	if reply[0] != 5 || reply[1] != 2 {
		return nil, errors.New("the proxy takes no username and password")
	}
	auth := []byte{1, byte(len(p.Username))}
	auth = append(auth, p.Username...)
	auth = append(auth, byte(len(p.Password)))
	auth = append(auth, p.Password...)
	if _, err := conn.Write(auth); err != nil {
		return nil, err
	}
	if _, err := io.ReadFull(conn, reply); err != nil {
		return nil, err
	}
	if reply[1] != 0 {
		return nil, errors.New("the proxy refused the username and password")
	}
	request := []byte{5, 0xf0, 0, 3, byte(len(host))}
	request = append(request, host...)
	request = append(request, 0, 0)
	if _, err := conn.Write(request); err != nil {
		return nil, err
	}
	head := make([]byte, 4)
	if _, err := io.ReadFull(conn, head); err != nil {
		return nil, err
	}
	if head[1] != 0 {
		return nil, fmt.Errorf("the proxy answered %d", head[1])
	}
	var ip net.IP
	switch head[3] {
	case 1:
		ip = make(net.IP, net.IPv4len)
	case 4:
		ip = make(net.IP, net.IPv6len)
	default:
		return nil, fmt.Errorf("an answer of address type %d", head[3])
	}
	if _, err := io.ReadFull(conn, ip); err != nil {
		return nil, err
	}
	// Its port, unused.
	if _, err := io.ReadFull(conn, reply); err != nil {
		return nil, err
	}
	return ip, nil
}

// httpClient is a client of timeout, through proxy if set: the
// environment's proxy settings are never read then.
func httpClient(proxy *Proxy, timeout time.Duration) *http.Client {
	if proxy == nil {
		return &http.Client{Timeout: timeout}
	}
	return &http.Client{
		Timeout: timeout,
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, addr string) (net.Conn, error) {
				return proxy.Dial(addr)
			},
			ForceAttemptHTTP2:   true,
			TLSHandshakeTimeout: proxyTimeout,
			MaxIdleConns:        4,
			IdleConnTimeout:     90 * time.Second,
		},
	}
}
