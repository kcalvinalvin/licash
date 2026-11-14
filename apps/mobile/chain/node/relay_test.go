package node

import (
	"testing"
	"time"

	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/lightninglabs/neutrino/query"

	"github.com/licash-org/licash/apps/mobile/chain/mempool"
)

// fakePeer is a peer the test speaks for.
type fakePeer struct {
	addr string
	in   chan wire.Message
	out  chan wire.Message
	gone chan struct{}
}

func newFakePeer(addr string) *fakePeer {
	return &fakePeer{
		addr: addr,
		in:   make(chan wire.Message),
		out:  make(chan wire.Message, 16),
		gone: make(chan struct{}),
	}
}

func (p *fakePeer) QueueMessageWithEncoding(msg wire.Message, _ chan<- struct{},
	enc wire.MessageEncoding) {

	if enc != wire.WitnessEncoding {
		panic("a request that would strip witnesses")
	}
	p.out <- msg
}
func (p *fakePeer) SubscribeRecvMsg() (<-chan wire.Message, func()) { return p.in, func() {} }
func (p *fakePeer) Addr() string                                    { return p.addr }
func (p *fakePeer) OnDisconnect() <-chan struct{}                   { return p.gone }

func (p *fakePeer) announces(txid chainhash.Hash) {
	inv := wire.NewMsgInv()
	_ = inv.AddInvVect(wire.NewInvVect(wire.InvTypeTx, &txid))
	// A block announcement in the same message is not the relay's.
	_ = inv.AddInvVect(wire.NewInvVect(wire.InvTypeBlock, &chainhash.Hash{9}))
	p.in <- inv
}

// asked returns the transactions the relay requested from p, or nil if it
// requested nothing for a while.
func (p *fakePeer) asked(t *testing.T) []chainhash.Hash {
	t.Helper()
	select {
	case msg := <-p.out:
		var out []chainhash.Hash
		for _, iv := range msg.(*wire.MsgGetData).InvList {
			if iv.Type != wire.InvTypeWitnessTx {
				t.Fatalf("asked for %v, not for a transaction with its witness", iv.Type)
			}
			out = append(out, iv.Hash)
		}
		return out
	case <-time.After(200 * time.Millisecond):
		return nil
	}
}

func signaled(r *relay) bool {
	select {
	case <-r.updates:
		return true
	case <-time.After(200 * time.Millisecond):
		return false
	}
}

func TestTheRelayFetchesWhatPeersAnnounceAndSignalsWhatIsHeld(t *testing.T) {
	funding := wire.OutPoint{Hash: chainhash.Hash{1}, Index: 0}
	tx := wire.NewMsgTx(2)
	tx.AddTxIn(wire.NewTxIn(&funding, nil, wire.TxWitness{{0x01}}))
	tx.AddTxOut(wire.NewTxOut(1, []byte{0x6a}))
	txid := tx.TxHash()

	r := newRelay(mempool.New(mempool.Limits{RequestTimeout: time.Second}))
	r.tracker.WatchOutpoint(funding)
	connected := make(chan query.Peer)
	tick := make(chan time.Time)
	r.run(connected, tick)
	defer r.stop()

	a, b, c := newFakePeer("a"), newFakePeer("b"), newFakePeer("c")
	for _, p := range []*fakePeer{a, b, c} {
		connected <- p
	}

	// Asked of the first announcer only, the others wait their turn.
	a.announces(txid)
	if got := a.asked(t); len(got) != 1 || got[0] != txid {
		t.Fatalf("a was asked for %v", got)
	}
	// One at a time: the order they are recorded in is the order they
	// are asked in.
	for _, p := range []*fakePeer{b, c} {
		p.announces(txid)
		if got := p.asked(t); got != nil {
			t.Fatalf("%s was asked for %v while a may still deliver", p.addr, got)
		}
	}

	// a withholds: the clock moves on to b. b says it has nothing: c.
	tick <- time.Now().Add(2 * time.Second)
	if got := b.asked(t); len(got) != 1 || got[0] != txid {
		t.Fatalf("after a's timeout b was asked for %v", got)
	}
	nothing := &notFound{}
	_ = nothing.AddInvVect(wire.NewInvVect(wire.InvTypeTx, &txid))
	b.in <- nothing
	if got := c.asked(t); len(got) != 1 || got[0] != txid {
		t.Fatalf("after b's notfound c was asked for %v", got)
	}

	// A delivery from a peer no longer asked is ignored; c's is held.
	a.in <- tx
	if signaled(r) {
		t.Fatal("took a delivery from a peer that was not asked")
	}
	c.in <- tx
	if !signaled(r) {
		t.Fatal("no signal for a held transaction")
	}
	if got := r.tracker.Spends(funding); len(got) != 1 || got[0].From != "c" {
		t.Fatalf("held: %+v", got)
	}
}

func TestTheRelayMovesOnWhenAPeerDisconnects(t *testing.T) {
	funding := wire.OutPoint{Hash: chainhash.Hash{2}, Index: 0}
	txid := chainhash.Hash{7}
	r := newRelay(mempool.New(mempool.Limits{}))
	r.tracker.WatchOutpoint(funding)
	connected := make(chan query.Peer)
	r.run(connected, nil)
	defer r.stop()

	a, b := newFakePeer("a"), newFakePeer("b")
	connected <- a
	connected <- b
	a.announces(txid)
	a.asked(t)
	b.announces(txid)
	b.asked(t)

	close(a.gone)
	if got := b.asked(t); len(got) != 1 || got[0] != txid {
		t.Fatalf("after a left b was asked for %v", got)
	}
	// Rejecting what is not held names nobody.
	if from := r.reject(txid); from != "" {
		t.Fatalf("rejected a transaction that was never held, from %q", from)
	}
}

// notFound is a notfound message: encoded as an inv message is.
type notFound struct{ wire.MsgInv }

func (*notFound) Command() string { return wire.CmdNotFound }
