package node

// The peers that served the node, remembered across starts in a file of
// our own: neutrino's address manager saves its peers only every ten
// minutes and on a clean stop, which a phone rarely gets, and does not
// tell the peers that answered from the ones merely heard of. A start
// connects the latest of them first, alongside its usual discovery.

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"time"
)

const (
	// peersFile is the book of peers in the node's data directory.
	peersFile = "good-peers.json"
	// peersKept bounds the book.
	peersKept = 64
	// peersForgotten is how long a peer not seen since stays in the book.
	peersForgotten = 30 * 24 * time.Hour
	// peersFirst is how many of the latest peers a start connects to.
	peersFirst = 4
	// peersSaveEvery bounds how often the times of known peers are saved;
	// a peer new to the book is saved at once.
	peersSaveEvery = 5 * time.Minute
)

// peerSeen is a peer the node completed a handshake with, and when it
// last did.
type peerSeen struct {
	Addr string `json:"addr"`
	Seen int64  `json:"seen"`
}

// peerBook is the remembered peers, latest first once read.
type peerBook struct {
	path string

	mu    sync.Mutex
	peers map[string]int64
	saved time.Time
	dirty bool
}

// openPeerBook reads the book in dir. A missing or unreadable book is an
// empty one: the node then finds its peers as it would without one.
func openPeerBook(dir string) (*peerBook, error) {
	b := &peerBook{path: filepath.Join(dir, peersFile), peers: make(map[string]int64)}
	raw, err := os.ReadFile(b.path)
	if errors.Is(err, os.ErrNotExist) {
		return b, nil
	}
	if err != nil {
		return b, fmt.Errorf("reading the peers remembered: %w", err)
	}
	var seen []peerSeen
	if err := json.Unmarshal(raw, &seen); err != nil {
		return b, fmt.Errorf("the peers remembered are unreadable, forgetting them: %w", err)
	}
	for _, p := range seen {
		if p.Addr != "" && p.Seen > b.peers[p.Addr] {
			b.peers[p.Addr] = p.Seen
		}
	}
	return b, nil
}

// latest is up to n of the peers seen within peersForgotten of now that
// usable takes, latest first.
func (b *peerBook) latest(n int, now time.Time, usable func(string) bool) []string {
	b.mu.Lock()
	defer b.mu.Unlock()
	var addrs []string
	for _, p := range b.sortedLocked(now) {
		if len(addrs) == n {
			break
		}
		if usable(p.Addr) {
			addrs = append(addrs, p.Addr)
		}
	}
	return addrs
}

// saw records that the node is connected to addrs at now, and saves the
// book if one of them is new to it or it was last saved long enough ago.
func (b *peerBook) saw(addrs []string, now time.Time) error {
	b.mu.Lock()
	defer b.mu.Unlock()
	added := false
	for _, a := range addrs {
		if _, known := b.peers[a]; !known {
			added = true
		}
		b.peers[a] = now.Unix()
		b.dirty = true
	}
	if !b.dirty || (!added && now.Sub(b.saved) < peersSaveEvery) {
		return nil
	}
	return b.saveLocked(now)
}

// flush saves what is not saved yet.
func (b *peerBook) flush(now time.Time) error {
	b.mu.Lock()
	defer b.mu.Unlock()
	if !b.dirty {
		return nil
	}
	return b.saveLocked(now)
}

// sortedLocked is the peers not forgotten by now, latest first, ties by
// address, at most peersKept.
func (b *peerBook) sortedLocked(now time.Time) []peerSeen {
	oldest := now.Add(-peersForgotten).Unix()
	all := make([]peerSeen, 0, len(b.peers))
	for a, s := range b.peers {
		if s >= oldest {
			all = append(all, peerSeen{Addr: a, Seen: s})
		}
	}
	sort.Slice(all, func(i, j int) bool {
		if all[i].Seen != all[j].Seen {
			return all[i].Seen > all[j].Seen
		}
		return all[i].Addr < all[j].Addr
	})
	if len(all) > peersKept {
		all = all[:peersKept]
	}
	return all
}

// saveLocked writes the book whole beside itself, then over itself: a
// crash leaves the old book or the new one.
func (b *peerBook) saveLocked(now time.Time) error {
	all := b.sortedLocked(now)
	b.peers = make(map[string]int64, len(all))
	for _, p := range all {
		b.peers[p.Addr] = p.Seen
	}
	raw, err := json.Marshal(all)
	if err != nil {
		return err
	}
	aside := b.path + ".new"
	if err := os.WriteFile(aside, raw, 0o600); err != nil {
		return fmt.Errorf("saving the peers remembered: %w", err)
	}
	if err := os.Rename(aside, b.path); err != nil {
		return fmt.Errorf("saving the peers remembered: %w", err)
	}
	b.saved, b.dirty = now, false
	return nil
}
