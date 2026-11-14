package mempool

import (
	"bytes"
	"sort"
	"time"

	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
)

// Announce records that peer announced txids and returns what to request
// from it now. A transaction already asked of another peer is not asked
// twice: peer waits its turn in case that one does not deliver.
func (t *Tracker) Announce(peer string, txids []chainhash.Hash, now time.Time) []Request {
	t.mu.Lock()
	defer t.mu.Unlock()
	if !t.watching() || peer == departed {
		return nil
	}
	var ask []chainhash.Hash
	for _, txid := range txids {
		if _, ok := t.known[txid]; ok {
			continue
		}
		if e := t.byTxid[txid]; e != nil {
			// Somewhere to look again should this delivery be rejected.
			if e.seen.From != peer && !contains(e.others, peer) &&
				len(e.others) < t.lim.AnnouncersPerTx {

				e.others = append(e.others, peer)
			}
			continue
		}
		if t.load[peer] >= t.lim.AnnouncedPerPeer {
			continue
		}
		f := t.pending[txid]
		switch {
		case f == nil:
			t.pending[txid] = &fetch{queue: []string{peer}, askedAt: now}
			ask = append(ask, txid)
		case contains(f.queue, peer) || len(f.queue) >= t.lim.AnnouncersPerTx:
			continue
		default:
			f.queue = append(f.queue, peer)
		}
		t.load[peer]++
	}
	if len(ask) == 0 {
		return nil
	}
	return []Request{{Peer: peer, Txids: ask}}
}

// Deliver takes a transaction peer sent. It is ignored unless peer was asked
// for it. If it touches something watched it is held and returned.
func (t *Tracker) Deliver(peer string, tx *wire.MsgTx, now time.Time) (Seen, bool) {
	txid := tx.TxHash()
	t.mu.Lock()
	defer t.mu.Unlock()
	f := t.pending[txid]
	if f == nil || f.queue[0] != peer {
		return Seen{}, false
	}
	t.settle(txid, f)

	spends, pays := t.match(tx)
	size := tx.SerializeSize() + entryOverhead
	if len(spends)+len(pays) == 0 || size > t.lim.BytesPerPeer {
		t.remember(txid)
		return Seen{}, false
	}
	e := &entry{
		seen: Seen{
			Tx: tx, Txid: txid, From: peer, At: now,
			Spends: spends, Pays: pays,
		},
		others: append([]string(nil), f.queue[1:]...),
		size:   size,
	}
	t.held = append(t.held, e)
	t.byTxid[txid] = e
	t.bytes[peer] += size
	t.trim(peer)
	return e.seen, true
}

// NotFound records that peer has none of txids after all. The next
// announcer of each is asked.
func (t *Tracker) NotFound(peer string, txids []chainhash.Hash, now time.Time) []Request {
	t.mu.Lock()
	defer t.mu.Unlock()
	next := make(map[string][]chainhash.Hash)
	for _, txid := range txids {
		if f := t.pending[txid]; f != nil && f.queue[0] == peer {
			t.advance(txid, f, now, next)
		}
	}
	return requests(next)
}

// Tick moves on from every peer that has not delivered in time. Call it
// about once a second.
func (t *Tracker) Tick(now time.Time) []Request {
	t.mu.Lock()
	defer t.mu.Unlock()
	next := make(map[string][]chainhash.Hash)
	for txid, f := range t.pending {
		if now.Sub(f.askedAt) >= t.lim.RequestTimeout {
			t.advance(txid, f, now, next)
		}
	}
	return requests(next)
}

// Disconnect records that peer is gone. What it was asked for is asked of
// the next announcer. What it delivered stays, in the space all departed
// peers share.
func (t *Tracker) Disconnect(peer string, now time.Time) []Request {
	t.mu.Lock()
	defer t.mu.Unlock()
	if peer == departed {
		return nil
	}
	next := make(map[string][]chainhash.Hash)
	for txid, f := range t.pending {
		switch i := index(f.queue, peer); {
		case i == 0:
			t.advance(txid, f, now, next)
		case i > 0:
			f.queue = append(f.queue[:i], f.queue[i+1:]...)
		}
	}
	delete(t.load, peer)

	for _, e := range t.held {
		if i := index(e.others, peer); i >= 0 {
			e.others = append(e.others[:i], e.others[i+1:]...)
		}
		if e.seen.From == peer {
			e.seen.From = departed
			t.bytes[departed] += e.size
		}
	}
	delete(t.bytes, peer)
	t.trim(departed)
	return requests(next)
}

// Reject lets go of a held transaction the consumer found wrong, typically
// a witness that does not satisfy the watched output. It returns the peer
// that delivered it (empty if unknown or departed) and asks the other
// announcers for the transaction: the id may be a real transaction's, with
// only this copy's witness made up.
func (t *Tracker) Reject(txid chainhash.Hash, now time.Time) (from string, ask []Request) {
	t.mu.Lock()
	defer t.mu.Unlock()
	e := t.take(txid)
	if e == nil {
		return "", nil
	}
	if len(e.others) == 0 || !t.watching() {
		return e.seen.From, nil
	}
	t.pending[txid] = &fetch{queue: e.others, askedAt: now}
	for _, peer := range e.others {
		t.load[peer]++
	}
	return e.seen.From, []Request{{Peer: e.others[0], Txids: []chainhash.Hash{txid}}}
}

// Forget lets go of a transaction for good: it confirmed, or can no longer
// confirm. It is not fetched again.
func (t *Tracker) Forget(txid chainhash.Hash) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.take(txid)
	if f := t.pending[txid]; f != nil {
		t.settle(txid, f)
	}
	t.remember(txid)
}

// settle ends a fetch and frees its announcers' counts.
func (t *Tracker) settle(txid chainhash.Hash, f *fetch) {
	delete(t.pending, txid)
	for _, peer := range f.queue {
		t.unload(peer)
	}
}

func (t *Tracker) unload(peer string) {
	if t.load[peer]--; t.load[peer] <= 0 {
		delete(t.load, peer)
	}
}

// advance gives up on the peer asked for txid and asks the next announcer,
// recording the request in next. With no announcer left the fetch ends: a
// new announcement starts it again.
func (t *Tracker) advance(txid chainhash.Hash, f *fetch, now time.Time,
	next map[string][]chainhash.Hash) {

	t.unload(f.queue[0])
	f.queue = f.queue[1:]
	if len(f.queue) == 0 {
		delete(t.pending, txid)
		return
	}
	f.askedAt = now
	next[f.queue[0]] = append(next[f.queue[0]], txid)
}

// requests orders what to ask by peer and transaction id, so a driver and
// its tests see the same thing every time.
func requests(next map[string][]chainhash.Hash) []Request {
	if len(next) == 0 {
		return nil
	}
	out := make([]Request, 0, len(next))
	for peer, txids := range next {
		sort.Slice(txids, func(i, j int) bool {
			return bytes.Compare(txids[i][:], txids[j][:]) < 0
		})
		out = append(out, Request{Peer: peer, Txids: txids})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Peer < out[j].Peer })
	return out
}

func index(peers []string, peer string) int {
	for i, p := range peers {
		if p == peer {
			return i
		}
	}
	return -1
}

func contains(peers []string, peer string) bool {
	return index(peers, peer) >= 0
}
