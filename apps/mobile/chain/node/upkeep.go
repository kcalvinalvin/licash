package node

// The node's upkeep, apart from neutrino's own: remembering the peers that
// answered, reaching the network's fixed seeds when nothing else does, and
// logging how long each step of catching up took.

import (
	"log/slog"
	"math/rand/v2"
	"sync"
	"time"
)

const (
	// upkeepEvery is how often the node's peers and progress are looked at.
	upkeepEvery = 2 * time.Second
	// seedsAfter is how long a node with no peer waits before it reaches
	// fixed seeds, and between rounds of them: discovery gets its chance
	// first, as in Bitcoin Core.
	seedsAfter = 30 * time.Second
	// seedsAtOnce is how many fixed seeds a round reaches.
	seedsAtOnce = 4
)

// network is what upkeep needs of the chain service: a seam for tests.
type network interface {
	// handshaken is the outbound peers the node completed a handshake with.
	handshaken() []string
	connect(addr string) error
	headers() int32
	peerHeight() int32
	current() bool
}

// upkeep runs until stop.
type upkeep struct {
	net   network
	book  *peerBook
	seeds []string
	// usable is whether an address can be reached: an onion one only
	// through a proxy.
	usable func(string) bool
	log    *slog.Logger

	// The start, and when each step was first reached: zero until then.
	start      time.Time
	firstPeer  time.Time
	headersAt  time.Time
	currentAt  time.Time
	seedsDue   time.Time
	seedsTried map[string]bool

	quit chan struct{}
	wg   sync.WaitGroup
}

// newUpkeep looks after net from start, reaching only the addresses
// usable takes. A nil book remembers no peers, and no seeds are reached
// without seeds: for a node told its peers.
func newUpkeep(net network, book *peerBook, seeds []string, usable func(string) bool,
	log *slog.Logger, start time.Time) *upkeep {
	var reachable []string
	for _, s := range seeds {
		if usable(s) {
			reachable = append(reachable, s)
		}
	}
	return &upkeep{
		net: net, book: book, seeds: reachable, usable: usable, log: log,
		start:      start,
		seedsDue:   start.Add(seedsAfter),
		seedsTried: make(map[string]bool),
		quit:       make(chan struct{}),
	}
}

// reconnect reaches the latest peers remembered, alongside discovery.
func (u *upkeep) reconnect(now time.Time) {
	if u.book == nil {
		return
	}
	latest := u.book.latest(peersFirst, now, u.usable)
	for _, addr := range latest {
		if err := u.net.connect(addr); err != nil {
			u.log.Debug("reaching a remembered peer", "peer", addr, "err", err)
		}
	}
	u.log.Info("reaching remembered peers", "count", len(latest))
}

func (u *upkeep) run(tick <-chan time.Time) {
	u.wg.Add(1)
	go func() {
		defer u.wg.Done()
		for {
			select {
			case now := <-tick:
				u.step(now)
			case <-u.quit:
				return
			}
		}
	}()
}

// stop ends the upkeep and saves the peers remembered.
func (u *upkeep) stop() {
	close(u.quit)
	u.wg.Wait()
	if u.book != nil {
		if err := u.book.flush(time.Now()); err != nil {
			u.log.Warn("saving the peers remembered", "err", err)
		}
	}
}

// step is one look at the node at now.
func (u *upkeep) step(now time.Time) {
	peers := u.net.handshaken()
	if u.book != nil && len(peers) > 0 {
		if err := u.book.saw(peers, now); err != nil {
			u.log.Warn("remembering peers", "err", err)
		}
	}
	u.progress(now, len(peers))
	if len(peers) == 0 && len(u.seeds) > 0 && !now.Before(u.seedsDue) {
		u.seedsDue = now.Add(seedsAfter)
		u.reachSeeds()
	}
}

// progress logs each step of catching up the first time it is reached.
func (u *upkeep) progress(now time.Time, peers int) {
	since := func() string { return now.Sub(u.start).Round(time.Second).String() }
	if u.firstPeer.IsZero() && peers > 0 {
		u.firstPeer = now
		u.log.Info("phase: first peer", "after", since())
	}
	if u.headersAt.IsZero() {
		if best := u.net.peerHeight(); best > 0 && u.net.headers() >= best {
			u.headersAt = now
			u.log.Info("phase: block headers caught up", "after", since(), "height", best)
		}
	}
	if u.currentAt.IsZero() && u.net.current() {
		u.currentAt = now
		u.log.Info("phase: filter headers caught up", "after", since())
	}
}

// reachSeeds reaches fixed seeds not tried yet, in random order, all
// again once each was.
func (u *upkeep) reachSeeds() {
	left := make([]string, 0, len(u.seeds))
	for _, s := range u.seeds {
		if !u.seedsTried[s] {
			left = append(left, s)
		}
	}
	if len(left) == 0 {
		clear(u.seedsTried)
		left = append(left, u.seeds...)
	}
	rand.Shuffle(len(left), func(i, j int) { left[i], left[j] = left[j], left[i] })
	if len(left) > seedsAtOnce {
		left = left[:seedsAtOnce]
	}
	for _, s := range left {
		u.seedsTried[s] = true
		if err := u.net.connect(s); err != nil {
			u.log.Debug("reaching a fixed seed", "peer", s, "err", err)
		}
	}
	u.log.Info("no peers yet: reaching fixed seeds", "count", len(left))
}
