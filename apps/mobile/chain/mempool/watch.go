package mempool

import (
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
)

// WatchScript starts reporting transactions with an output to pkScript.
// Watch before the payment is expected: one announced earlier is not seen.
func (t *Tracker) WatchScript(pkScript []byte) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.scripts[string(pkScript)] = struct{}{}
	t.rematch()
}

// UnwatchScript stops, and lets go of what was held only for pkScript.
func (t *Tracker) UnwatchScript(pkScript []byte) {
	t.mu.Lock()
	defer t.mu.Unlock()
	delete(t.scripts, string(pkScript))
	t.rematch()
}

// WatchOutpoint starts reporting transactions that spend op.
func (t *Tracker) WatchOutpoint(op wire.OutPoint) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.outpoints[op] = struct{}{}
	t.rematch()
}

// UnwatchOutpoint stops, and lets go of what was held only for op.
func (t *Tracker) UnwatchOutpoint(op wire.OutPoint) {
	t.mu.Lock()
	defer t.mu.Unlock()
	delete(t.outpoints, op)
	t.rematch()
}

// Spends returns the transactions held that spend op, newest first. More
// than one is normal: replacements, and whatever peers made up.
func (t *Tracker) Spends(op wire.OutPoint) []Seen {
	return t.find(func(s *Seen) bool {
		for _, spent := range s.Spends {
			if spent == op {
				return true
			}
		}
		return false
	})
}

// Pays returns the transactions held with an output to pkScript, newest
// first.
func (t *Tracker) Pays(pkScript []byte) []Seen {
	return t.find(func(s *Seen) bool {
		for _, i := range s.Pays {
			if string(s.Tx.TxOut[i].PkScript) == string(pkScript) {
				return true
			}
		}
		return false
	})
}

// Stats is what a Tracker holds, for monitoring.
type Stats struct {
	WatchedScripts   int
	WatchedOutpoints int
	// Pending are announced transactions not yet delivered.
	Pending int
	// Held are delivered transactions touching something watched.
	Held      int
	HeldBytes int
}

func (t *Tracker) Stats() Stats {
	t.mu.Lock()
	defer t.mu.Unlock()
	s := Stats{
		WatchedScripts:   len(t.scripts),
		WatchedOutpoints: len(t.outpoints),
		Pending:          len(t.pending),
		Held:             len(t.held),
	}
	for _, n := range t.bytes {
		s.HeldBytes += n
	}
	return s
}

func (t *Tracker) find(want func(*Seen) bool) []Seen {
	t.mu.Lock()
	defer t.mu.Unlock()
	var out []Seen
	for i := len(t.held) - 1; i >= 0; i-- {
		if want(&t.held[i].seen) {
			out = append(out, t.held[i].seen)
		}
	}
	return out
}

func (t *Tracker) watching() bool {
	return len(t.scripts)+len(t.outpoints) > 0
}

// match is what tx touches of the watched set.
func (t *Tracker) match(tx *wire.MsgTx) (spends []wire.OutPoint, pays []uint32) {
	for _, in := range tx.TxIn {
		if _, ok := t.outpoints[in.PreviousOutPoint]; ok {
			spends = append(spends, in.PreviousOutPoint)
		}
	}
	for i, out := range tx.TxOut {
		if _, ok := t.scripts[string(out.PkScript)]; ok {
			pays = append(pays, uint32(i))
		}
	}
	return spends, pays
}

// rematch brings what is held in line with a changed watched set. With
// nothing watched nothing is fetched, so what is pending is dropped too.
func (t *Tracker) rematch() {
	kept := t.held[:0]
	for _, e := range t.held {
		e.seen.Spends, e.seen.Pays = t.match(e.seen.Tx)
		if len(e.seen.Spends)+len(e.seen.Pays) == 0 {
			t.release(e)
			continue
		}
		kept = append(kept, e)
	}
	clear(t.held[len(kept):])
	t.held = kept
	if !t.watching() {
		clear(t.pending)
		clear(t.load)
	}
}

// release returns an entry's space to its peer and forgets its id. The
// caller removes it from held.
func (t *Tracker) release(e *entry) {
	delete(t.byTxid, e.seen.Txid)
	t.bytes[e.seen.From] -= e.size
	if t.bytes[e.seen.From] <= 0 {
		delete(t.bytes, e.seen.From)
	}
}

// take removes the held entry for txid, if any.
func (t *Tracker) take(txid chainhash.Hash) *entry {
	e := t.byTxid[txid]
	if e == nil {
		return nil
	}
	for i := range t.held {
		if t.held[i] == e {
			t.held = append(t.held[:i], t.held[i+1:]...)
			break
		}
	}
	t.release(e)
	return e
}

// trim lets go of peer's oldest entries until it is within its space.
func (t *Tracker) trim(peer string) {
	for i := 0; i < len(t.held) && t.bytes[peer] > t.lim.BytesPerPeer; {
		if e := t.held[i]; e.seen.From == peer {
			t.held = append(t.held[:i], t.held[i+1:]...)
			t.release(e)
			continue
		}
		i++
	}
}

func (t *Tracker) remember(txid chainhash.Hash) {
	if _, ok := t.known[txid]; ok {
		return
	}
	if len(t.knownRing) < t.lim.Known {
		t.knownRing = append(t.knownRing, txid)
	} else {
		delete(t.known, t.knownRing[t.knownNext])
		t.knownRing[t.knownNext] = txid
		t.knownNext = (t.knownNext + 1) % len(t.knownRing)
	}
	t.known[txid] = struct{}{}
}
