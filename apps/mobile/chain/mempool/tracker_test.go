package mempool

import (
	"reflect"
	"testing"
	"time"

	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
)

var (
	t0      = time.Unix(1_800_000_000, 0)
	funding = wire.OutPoint{Hash: chainhash.Hash{0xf0}, Index: 1}
	script  = []byte{0x51, 0x20, 0xaa}
)

// spend builds a transaction spending op, distinct per nonce, with witness
// bytes that do not enter its id.
func spend(op wire.OutPoint, nonce int64, witness byte, pay ...[]byte) *wire.MsgTx {
	tx := wire.NewMsgTx(2)
	tx.AddTxIn(wire.NewTxIn(&op, nil, wire.TxWitness{{witness}}))
	tx.AddTxOut(wire.NewTxOut(nonce, []byte{0x6a}))
	for _, pk := range pay {
		tx.AddTxOut(wire.NewTxOut(1000, pk))
	}
	return tx
}

func ids(txs ...*wire.MsgTx) []chainhash.Hash {
	out := make([]chainhash.Hash, len(txs))
	for i, tx := range txs {
		out[i] = tx.TxHash()
	}
	return out
}

func ask(peer string, txs ...*wire.MsgTx) []Request {
	return []Request{{Peer: peer, Txids: ids(txs...)}}
}

func want(t *testing.T, got, expected []Request) {
	t.Helper()
	if !reflect.DeepEqual(got, expected) {
		t.Fatalf("requests\n got  %v\n want %v", got, expected)
	}
}

func watchingFunding() *Tracker {
	tr := New(Limits{})
	tr.WatchOutpoint(funding)
	return tr
}

func TestNothingIsFetchedWhileNothingIsWatched(t *testing.T) {
	tr := New(Limits{})
	tx := spend(funding, 1, 0)
	want(t, tr.Announce("a", ids(tx), t0), nil)
	if _, ok := tr.Deliver("a", tx, t0); ok {
		t.Fatal("held a transaction nobody asked for")
	}
	if s := tr.Stats(); s.Pending != 0 || s.Held != 0 {
		t.Fatalf("state without a watch: %+v", s)
	}
}

func TestAWatchedSpendIsFetchedOnceAndHeld(t *testing.T) {
	tr := watchingFunding()
	tx, other := spend(funding, 1, 0), spend(wire.OutPoint{Index: 9}, 2, 0)

	// Everything announced is asked for, so requests do not show what is
	// watched; a second announcer is not asked while the first may deliver.
	want(t, tr.Announce("a", ids(tx, other), t0), ask("a", tx, other))
	want(t, tr.Announce("b", ids(tx, other), t0), nil)

	if _, ok := tr.Deliver("a", other, t0); ok {
		t.Fatal("held a transaction touching nothing watched")
	}
	seen, ok := tr.Deliver("a", tx, t0)
	if !ok || seen.From != "a" || seen.Txid != tx.TxHash() ||
		!reflect.DeepEqual(seen.Spends, []wire.OutPoint{funding}) {

		t.Fatalf("delivery: %+v %v", seen, ok)
	}
	if got := tr.Spends(funding); len(got) != 1 || got[0].Tx != tx {
		t.Fatalf("spends: %+v", got)
	}
	// Neither is fetched again, by anyone.
	want(t, tr.Announce("c", ids(tx, other), t0), nil)
	if s := tr.Stats(); s.Pending != 0 || s.Held != 1 {
		t.Fatalf("stats: %+v", s)
	}
}

func TestOnlyTheAskedPeerCanDeliver(t *testing.T) {
	tr := watchingFunding()
	tx := spend(funding, 1, 0)
	if _, ok := tr.Deliver("a", tx, t0); ok {
		t.Fatal("accepted an unannounced transaction")
	}
	tr.Announce("a", ids(tx), t0)
	tr.Announce("b", ids(tx), t0)
	if _, ok := tr.Deliver("b", tx, t0); ok {
		t.Fatal("accepted a delivery from a peer that was not asked")
	}
	if _, ok := tr.Deliver("a", tx, t0); !ok {
		t.Fatal("refused the asked peer's delivery")
	}
}

func TestAWithholdingPeerDelaysByOneTimeoutPerAnnouncer(t *testing.T) {
	tr := New(Limits{RequestTimeout: 5 * time.Second})
	tr.WatchOutpoint(funding)
	tx := spend(funding, 1, 0)
	tr.Announce("hostile", ids(tx), t0)
	tr.Announce("slow", ids(tx), t0)
	tr.Announce("honest", ids(tx), t0)

	want(t, tr.Tick(t0.Add(4*time.Second)), nil)
	want(t, tr.Tick(t0.Add(5*time.Second)), ask("slow", tx))
	// The clock for the next peer starts when it is asked.
	want(t, tr.Tick(t0.Add(9*time.Second)), nil)
	want(t, tr.Tick(t0.Add(10*time.Second)), ask("honest", tx))
	if _, ok := tr.Deliver("hostile", tx, t0.Add(11*time.Second)); ok {
		t.Fatal("accepted a delivery after moving on")
	}
	if _, ok := tr.Deliver("honest", tx, t0.Add(11*time.Second)); !ok {
		t.Fatal("the honest announcer's delivery was lost")
	}
}

func TestAFetchWithNoAnnouncerLeftEndsAndCanStartAgain(t *testing.T) {
	tr := watchingFunding()
	tx := spend(funding, 1, 0)
	tr.Announce("a", ids(tx), t0)
	want(t, tr.Tick(t0.Add(time.Minute)), nil)
	if s := tr.Stats(); s.Pending != 0 {
		t.Fatalf("pending after the only announcer timed out: %+v", s)
	}
	want(t, tr.Announce("b", ids(tx), t0.Add(time.Minute)), ask("b", tx))
}

func TestNotFoundAndDisconnectMoveOnAtOnce(t *testing.T) {
	tr := watchingFunding()
	x, y := spend(funding, 1, 0), spend(funding, 2, 0)
	tr.Announce("a", ids(x, y), t0)
	tr.Announce("b", ids(x, y), t0)
	tr.Announce("c", ids(x, y), t0)

	want(t, tr.NotFound("b", ids(x), t0), nil) // b was not the one asked
	want(t, tr.NotFound("a", ids(x), t0), ask("b", x))
	// a is gone: b is asked for y too, and stays asked for x.
	want(t, tr.Disconnect("a", t0), ask("b", y))
	want(t, tr.Disconnect("b", t0), []Request{{Peer: "c", Txids: sorted(ids(x, y))}})
	want(t, tr.Disconnect("c", t0), nil)
	if s := tr.Stats(); s.Pending != 0 {
		t.Fatalf("pending with every announcer gone: %+v", s)
	}
}

func TestADepartedPeersDeliveryIsKept(t *testing.T) {
	tr := watchingFunding()
	tx := spend(funding, 1, 0)
	tr.Announce("a", ids(tx), t0)
	tr.Deliver("a", tx, t0)
	tr.Disconnect("a", t0)
	got := tr.Spends(funding)
	if len(got) != 1 || got[0].From != "" {
		t.Fatalf("after the peer left: %+v", got)
	}
}

func TestARejectedCopyIsFetchedFromTheOtherAnnouncers(t *testing.T) {
	tr := watchingFunding()
	// Same id, different witness: the id does not commit to it.
	forged, real := spend(funding, 1, 0xbd), spend(funding, 1, 0x01)
	if forged.TxHash() != real.TxHash() {
		t.Fatal("test transactions must share an id")
	}
	tr.Announce("hostile", ids(real), t0)
	tr.Announce("honest", ids(real), t0)
	tr.Deliver("hostile", forged, t0)
	tr.Announce("late", ids(real), t0) // remembered though already held

	from, again := tr.Reject(real.TxHash(), t0)
	if from != "hostile" {
		t.Fatalf("rejected copy came from %q", from)
	}
	want(t, again, ask("honest", real))
	if len(tr.Spends(funding)) != 0 {
		t.Fatal("the rejected copy is still held")
	}
	seen, ok := tr.Deliver("honest", real, t0)
	if !ok || seen.Tx.TxIn[0].Witness[0][0] != 0x01 {
		t.Fatalf("the real copy was not taken: %+v %v", seen, ok)
	}
	// A peer that announced it after it was held is asked as well.
	from, again = tr.Reject(real.TxHash(), t0)
	if from != "honest" {
		t.Fatalf("second rejection came from %q", from)
	}
	want(t, again, ask("late", real))
	want(t, tr.Tick(t0.Add(time.Hour)), nil)
	if from, again := tr.Reject(real.TxHash(), t0); from != "" || again != nil {
		t.Fatalf("rejecting what is not held: %q %v", from, again)
	}
}

func TestOnePeerCannotCrowdOutAnothersReport(t *testing.T) {
	tr := New(Limits{BytesPerPeer: 4 * (spend(funding, 0, 0).SerializeSize() + entryOverhead)})
	tr.WatchOutpoint(funding)
	truth := spend(funding, 1_000_000, 0)
	tr.Announce("honest", ids(truth), t0)
	tr.Deliver("honest", truth, t0)

	for i := int64(0); i < 100; i++ {
		fake := spend(funding, i, 0)
		tr.Announce("hostile", ids(fake), t0)
		if _, ok := tr.Deliver("hostile", fake, t0); !ok {
			t.Fatalf("fake %d was not held", i)
		}
	}
	got := tr.Spends(funding)
	if len(got) != 5 {
		t.Fatalf("held %d candidates, want the honest one and 4 fakes", len(got))
	}
	// Newest first: the hostile peer kept its last four, the honest one's
	// report is still there.
	if got[0].Tx.TxOut[0].Value != 99 || got[3].Tx.TxOut[0].Value != 96 || got[4].Tx != truth {
		t.Fatalf("wrong survivors: %v", got)
	}
	if s := tr.Stats(); s.HeldBytes > 2*tr.lim.BytesPerPeer {
		t.Fatalf("held %d bytes for two peers of %d each", s.HeldBytes, tr.lim.BytesPerPeer)
	}
}

func sorted(txids []chainhash.Hash) []chainhash.Hash {
	return requests(map[string][]chainhash.Hash{"": txids})[0].Txids
}
