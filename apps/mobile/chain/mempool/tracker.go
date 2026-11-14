// Package mempool tracks unconfirmed transactions that touch watched scripts
// and outpoints, from what peers announce and deliver.
//
// It is a state machine with no network and no clock of its own: a driver
// feeds it peer events and sends the requests it returns.
//
// What it holds are hints from peers, not facts. Nothing here can check that
// a transaction is valid or in anyone's mempool, and a peer can deliver a
// transaction with the announced id and a witness of its own making. The
// consumer checks what it can (a spend of its own outpoint, against the
// script and amount it knows), calls Reject on what fails, and treats only
// confirmations as final.
//
// What peers can and cannot do:
//   - While anything is watched, every announced transaction is fetched, so
//     the requests show a peer that something is watched and not what.
//   - A peer that announces and withholds delays a transaction by one
//     RequestTimeout: every other announcer is remembered and asked in turn.
//   - A peer fills only its own space. One peer's deliveries never evict
//     another's, so one honest peer's report survives any number of others.
//   - Nothing is kept across a restart, and a transaction announced before
//     its script or outpoint was watched is not seen again until it confirms.
package mempool

import (
	"sync"
	"time"

	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/wire/v2"
)

// Limits bound what peers can make a Tracker hold. The zero value of a
// field means its default.
type Limits struct {
	// RequestTimeout is how long a peer has to deliver what it announced
	// before the next announcer is asked.
	RequestTimeout time.Duration
	// AnnouncedPerPeer is how many undelivered announcements one peer may
	// have outstanding. Further ones are ignored.
	AnnouncedPerPeer int
	// AnnouncersPerTx is how many peers are remembered per announced
	// transaction.
	AnnouncersPerTx int
	// BytesPerPeer is the space one peer's matching deliveries may take.
	// Past it, that peer's oldest go. A larger transaction is refused.
	BytesPerPeer int
	// Known is how many delivered transactions that matched nothing are
	// remembered, so they are not fetched again.
	Known int
}

func (l Limits) withDefaults() Limits {
	if l.RequestTimeout <= 0 {
		l.RequestTimeout = 5 * time.Second
	}
	if l.AnnouncedPerPeer <= 0 {
		l.AnnouncedPerPeer = 5000
	}
	if l.AnnouncersPerTx <= 0 {
		l.AnnouncersPerTx = 8
	}
	if l.BytesPerPeer <= 0 {
		l.BytesPerPeer = 4 << 20
	}
	if l.Known <= 0 {
		l.Known = 20000
	}
	return l
}

// entryOverhead is charged per held transaction on top of its size, so that
// many tiny ones cannot hold more memory than the budget says.
const entryOverhead = 256

// departed holds what disconnected peers delivered, as one peer's space.
const departed = ""

// Request asks the driver to request transactions from a peer.
type Request struct {
	Peer  string
	Txids []chainhash.Hash
}

// Seen is a delivered transaction that touches something watched.
type Seen struct {
	Tx   *wire.MsgTx
	Txid chainhash.Hash
	// From is the peer that delivered it, empty once it has disconnected.
	From string
	At   time.Time
	// Spends are the watched outpoints it spends, Pays the indexes of its
	// outputs to watched scripts, as of its delivery.
	Spends []wire.OutPoint
	Pays   []uint32
}

// fetch is an announced transaction not yet delivered. queue[0] was asked
// at askedAt; the others announced it too and have not been asked.
type fetch struct {
	queue   []string
	askedAt time.Time
}

type entry struct {
	seen Seen
	// others announced it and were never asked: where to look again if
	// this delivery is rejected.
	others []string
	size   int
}

// Tracker is safe for concurrent use.
type Tracker struct {
	mu  sync.Mutex
	lim Limits

	scripts   map[string]struct{}
	outpoints map[wire.OutPoint]struct{}

	pending map[chainhash.Hash]*fetch
	// load counts the pending transactions a peer is queued for.
	load map[string]int

	// held is in delivery order, oldest first; byTxid finds the same
	// entries, and bytes is the space each delivering peer has taken.
	held   []*entry
	byTxid map[chainhash.Hash]*entry
	bytes  map[string]int

	known     map[chainhash.Hash]struct{}
	knownRing []chainhash.Hash
	knownNext int
}

// New returns an empty Tracker watching nothing.
func New(limits Limits) *Tracker {
	return &Tracker{
		lim:       limits.withDefaults(),
		scripts:   make(map[string]struct{}),
		outpoints: make(map[wire.OutPoint]struct{}),
		pending:   make(map[chainhash.Hash]*fetch),
		load:      make(map[string]int),
		byTxid:    make(map[chainhash.Hash]*entry),
		bytes:     make(map[string]int),
		known:     make(map[chainhash.Hash]struct{}),
	}
}
