package node

import (
	"bytes"
	"sync"
	"time"

	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/lightninglabs/neutrino/query"

	"github.com/licash-org/licash/apps/mobile/chain/mempool"
)

// relay connects peers to the mempool tracker: it feeds the tracker what
// peers announce and deliver and sends the requests the tracker returns.
// It decides nothing itself.
type relay struct {
	tracker *mempool.Tracker
	// updates holds at most one signal: something new is held.
	updates chan struct{}
	quit    chan struct{}
	wg      sync.WaitGroup

	mu    sync.Mutex
	peers map[string]query.Peer
}

func newRelay(tracker *mempool.Tracker) *relay {
	return &relay{
		tracker: tracker,
		updates: make(chan struct{}, 1),
		quit:    make(chan struct{}),
		peers:   make(map[string]query.Peer),
	}
}

// run serves every peer arriving on connected, and the tracker's clock,
// until stop.
func (r *relay) run(connected <-chan query.Peer, tick <-chan time.Time) {
	r.wg.Add(1)
	go func() {
		defer r.wg.Done()
		for {
			select {
			case p := <-connected:
				r.wg.Add(1)
				go func() {
					defer r.wg.Done()
					r.serve(p)
				}()
			case now := <-tick:
				r.send(r.tracker.Tick(now))
			case <-r.quit:
				return
			}
		}
	}()
}

func (r *relay) stop() {
	close(r.quit)
	r.wg.Wait()
}

// serve handles one peer until it disconnects.
func (r *relay) serve(p query.Peer) {
	addr := p.Addr()
	msgs, cancel := p.SubscribeRecvMsg()
	defer cancel()

	r.mu.Lock()
	r.peers[addr] = p
	r.mu.Unlock()
	defer func() {
		// The tracker first: once it has moved on from this peer no
		// request can name it.
		r.send(r.tracker.Disconnect(addr, time.Now()))
		r.mu.Lock()
		if r.peers[addr] == p {
			delete(r.peers, addr)
		}
		r.mu.Unlock()
	}()

	for {
		select {
		case msg, ok := <-msgs:
			if !ok {
				return
			}
			r.receive(addr, msg)
		case <-p.OnDisconnect():
			return
		case <-r.quit:
			return
		}
	}
}

func (r *relay) receive(addr string, msg wire.Message) {
	now := time.Now()
	switch m := msg.(type) {
	case *wire.MsgInv:
		r.send(r.tracker.Announce(addr, txids(m.InvList), now))
	case *wire.MsgTx:
		if _, held := r.tracker.Deliver(addr, m, now); held {
			select {
			case r.updates <- struct{}{}:
			default:
			}
		}
	case nil:
		// A message that could not be read comes as nil.
	default:
		if msg.Command() == wire.CmdNotFound {
			r.send(r.tracker.NotFound(addr, txids(inventory(msg)), now))
		}
	}
}

// inventory is the list of a notfound message, read as the inv message it
// is encoded like.
func inventory(msg wire.Message) []*wire.InvVect {
	var buf bytes.Buffer
	var inv wire.MsgInv
	if msg.BtcEncode(&buf, wire.ProtocolVersion, wire.BaseEncoding) != nil ||
		inv.BtcDecode(&buf, wire.ProtocolVersion, wire.BaseEncoding) != nil {
		return nil
	}
	return inv.InvList
}

// reject lets go of a held transaction the consumer found wrong, asks its
// other announcers, and returns the peer that delivered it.
func (r *relay) reject(txid chainhash.Hash) string {
	from, again := r.tracker.Reject(txid, time.Now())
	r.send(again)
	return from
}

// send requests transactions with their witnesses. A request to a peer
// that has just gone is dropped: the tracker moves on when it is told.
func (r *relay) send(requests []mempool.Request) {
	for _, req := range requests {
		r.mu.Lock()
		p := r.peers[req.Peer]
		r.mu.Unlock()
		if p == nil {
			continue
		}
		for ids := req.Txids; len(ids) > 0; {
			n := min(len(ids), wire.MaxInvPerMsg)
			getData := wire.NewMsgGetDataSizeHint(uint(n))
			for i := range ids[:n] {
				// Cannot fail: n is within the message's limit.
				_ = getData.AddInvVect(wire.NewInvVect(
					wire.InvTypeWitnessTx, &ids[i],
				))
			}
			p.QueueMessageWithEncoding(getData, nil, wire.WitnessEncoding)
			ids = ids[n:]
		}
	}
}

// txids are the transactions an inventory names. Peers announce by txid:
// neutrino's peers do not negotiate wtxid relay.
func txids(inv []*wire.InvVect) []chainhash.Hash {
	var out []chainhash.Hash
	for _, iv := range inv {
		if iv.Type == wire.InvTypeTx {
			out = append(out, iv.Hash)
		}
	}
	return out
}
