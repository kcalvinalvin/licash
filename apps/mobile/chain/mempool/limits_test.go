package mempool

import (
	"testing"
	"time"

	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
)

func TestAPeersUndeliveredAnnouncementsAreCapped(t *testing.T) {
	tr := New(Limits{AnnouncedPerPeer: 3})
	tr.WatchOutpoint(funding)
	var flood []chainhash.Hash
	for i := byte(0); i < 10; i++ {
		flood = append(flood, chainhash.Hash{i + 1})
	}
	got := tr.Announce("hostile", flood, t0)
	if len(got) != 1 || len(got[0].Txids) != 3 {
		t.Fatalf("asked for %v, want the first 3", got)
	}
	if s := tr.Stats(); s.Pending != 3 {
		t.Fatalf("pending: %+v", s)
	}
	// Another peer is not affected, and the cap frees as fetches end.
	tx := spend(funding, 1, 0)
	want(t, tr.Announce("honest", ids(tx), t0), ask("honest", tx))
	tr.Tick(t0.Add(time.Minute))
	if got := tr.Announce("hostile", flood, t0.Add(time.Minute)); len(got[0].Txids) != 3 {
		t.Fatalf("after its fetches ended: %v", got)
	}
}

func TestAnnouncersPerTransactionAreCapped(t *testing.T) {
	tr := New(Limits{AnnouncersPerTx: 2, RequestTimeout: time.Second})
	tr.WatchOutpoint(funding)
	tx := spend(funding, 1, 0)
	for _, peer := range []string{"a", "b", "c", "a"} {
		tr.Announce(peer, ids(tx), t0)
	}
	want(t, tr.Tick(t0.Add(time.Second)), ask("b", tx))
	// c was not remembered, and a is not asked twice.
	want(t, tr.Tick(t0.Add(2*time.Second)), nil)
	if s := tr.Stats(); s.Pending != 0 {
		t.Fatalf("pending: %+v", s)
	}
}

func TestPaymentsToAWatchedScriptAreReported(t *testing.T) {
	tr := New(Limits{})
	tr.WatchScript(script)
	one := spend(wire.OutPoint{Index: 1}, 1, 0, script)
	two := spend(wire.OutPoint{Index: 2}, 2, 0, []byte{0x52}, script, script)
	for _, tx := range []*wire.MsgTx{one, two} {
		tr.Announce("a", ids(tx), t0)
		if _, ok := tr.Deliver("a", tx, t0); !ok {
			t.Fatal("a payment to the watched script was not held")
		}
	}
	got := tr.Pays(script)
	if len(got) != 2 || got[0].Tx != two || got[1].Tx != one {
		t.Fatalf("payments, newest first: %+v", got)
	}
	if p := got[0].Pays; len(p) != 2 || p[0] != 2 || p[1] != 3 {
		t.Fatalf("paying outputs: %v", p)
	}
	if len(tr.Pays([]byte{0x52})) != 0 || len(tr.Spends(funding)) != 0 {
		t.Fatal("reported for something not watched")
	}
}

func TestUnwatchingLetsGoOfWhatWasHeldOnlyForIt(t *testing.T) {
	tr := New(Limits{})
	tr.WatchOutpoint(funding)
	tr.WatchScript(script)
	both := spend(funding, 1, 0, script)
	only := spend(funding, 2, 0)
	for _, tx := range []*wire.MsgTx{both, only} {
		tr.Announce("a", ids(tx), t0)
		tr.Deliver("a", tx, t0)
	}
	waiting := spend(funding, 3, 0)
	tr.Announce("a", ids(waiting), t0)

	tr.UnwatchOutpoint(funding)
	if got := tr.Pays(script); len(got) != 1 || got[0].Tx != both || len(got[0].Spends) != 0 {
		t.Fatalf("after unwatching the outpoint: %+v", got)
	}
	if s := tr.Stats(); s.Held != 1 || s.HeldBytes != both.SerializeSize()+entryOverhead {
		t.Fatalf("space was not returned: %+v", s)
	}
	// With nothing watched, nothing is held, pending, or fetched.
	tr.UnwatchScript(script)
	if s := tr.Stats(); s != (Stats{}) {
		t.Fatalf("after the last unwatch: %+v", s)
	}
	if _, ok := tr.Deliver("a", waiting, t0); ok {
		t.Fatal("took a delivery with nothing watched")
	}
	// Watching again starts clean: the peer's counts were let go too.
	tr.WatchOutpoint(funding)
	want(t, tr.Announce("a", ids(waiting), t0), ask("a", waiting))
}

func TestAForgottenTransactionIsNotFetchedAgain(t *testing.T) {
	tr := watchingFunding()
	held, pending := spend(funding, 1, 0), spend(funding, 2, 0)
	tr.Announce("a", ids(held, pending), t0)
	tr.Deliver("a", held, t0)

	tr.Forget(held.TxHash())
	tr.Forget(pending.TxHash())
	if s := tr.Stats(); s.Held != 0 || s.Pending != 0 || s.HeldBytes != 0 {
		t.Fatalf("after forgetting: %+v", s)
	}
	want(t, tr.Announce("b", ids(held, pending), t0), nil)
	if _, ok := tr.Deliver("a", pending, t0); ok {
		t.Fatal("took a forgotten transaction")
	}
}

func TestTheMemoryOfUnmatchedTransactionsIsBounded(t *testing.T) {
	tr := New(Limits{Known: 2})
	tr.WatchOutpoint(funding)
	elsewhere := wire.OutPoint{Index: 7}
	a, b, c := spend(elsewhere, 1, 0), spend(elsewhere, 2, 0), spend(elsewhere, 3, 0)
	for _, tx := range []*wire.MsgTx{a, b, c} {
		tr.Announce("p", ids(tx), t0)
		tr.Deliver("p", tx, t0)
	}
	if len(tr.known) != 2 || len(tr.knownRing) != 2 {
		t.Fatalf("remembered %d", len(tr.known))
	}
	// The oldest was let go and would be fetched again; the others not.
	want(t, tr.Announce("p", ids(b, c), t0), nil)
	want(t, tr.Announce("p", ids(a), t0), ask("p", a))
}

func TestATransactionLargerThanAPeersSpaceIsRefused(t *testing.T) {
	small := spend(funding, 1, 0)
	tr := New(Limits{BytesPerPeer: small.SerializeSize() + entryOverhead})
	tr.WatchOutpoint(funding)
	large := spend(funding, 2, 0, make([]byte, 100))
	for _, tx := range []*wire.MsgTx{small, large} {
		tr.Announce("a", ids(tx), t0)
	}
	if _, ok := tr.Deliver("a", large, t0); ok {
		t.Fatal("held a transaction larger than the peer's space")
	}
	if _, ok := tr.Deliver("a", small, t0); !ok {
		t.Fatal("refused a transaction that fits exactly")
	}
	want(t, tr.Announce("b", ids(large), t0), nil)
}

func TestDepartedPeersShareOneSpace(t *testing.T) {
	size := spend(funding, 0, 0).SerializeSize() + entryOverhead
	tr := New(Limits{BytesPerPeer: 2 * size})
	tr.WatchOutpoint(funding)
	// Reconnecting under new names does not add up.
	for i, peer := range []string{"p1", "p2", "p3", "p4", "p5"} {
		tx := spend(funding, int64(i), 0)
		tr.Announce(peer, ids(tx), t0)
		tr.Deliver(peer, tx, t0)
		tr.Disconnect(peer, t0)
	}
	if s := tr.Stats(); s.Held != 2 || s.HeldBytes != 2*size {
		t.Fatalf("held for departed peers: %+v", s)
	}
	if got := tr.Spends(funding); got[0].Tx.TxOut[0].Value != 4 || got[1].Tx.TxOut[0].Value != 3 {
		t.Fatalf("kept the wrong ones: %+v", got)
	}
	want(t, tr.Announce("", ids(spend(funding, 9, 0)), t0), nil)
}
