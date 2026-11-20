package node

import (
	"errors"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"slices"
	"testing"
	"time"
)

var anyAddr = func(string) bool { return true }

func TestAMissingBookIsEmpty(t *testing.T) {
	b, err := openPeerBook(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	if got := b.latest(4, time.Now(), anyAddr); len(got) != 0 {
		t.Fatalf("latest of no book: %v", got)
	}
}

func TestAnUnreadableBookIsEmptyAndLeftForTheNextSave(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, peersFile)
	if err := os.WriteFile(path, []byte("{not json"), 0o600); err != nil {
		t.Fatal(err)
	}
	b, err := openPeerBook(dir)
	if err == nil {
		t.Fatal("an unreadable book read as fine")
	}
	if got := b.latest(4, time.Now(), anyAddr); len(got) != 0 {
		t.Fatalf("latest of an unreadable book: %v", got)
	}
	// Nothing written until a peer is seen.
	raw, _ := os.ReadFile(path)
	if string(raw) != "{not json" {
		t.Fatalf("the book was rewritten unasked: %q", raw)
	}
}

func TestPeersSeenAreRememberedLatestFirstAcrossStarts(t *testing.T) {
	dir := t.TempDir()
	now := time.Unix(1_800_000_000, 0)
	b, _ := openPeerBook(dir)
	if err := b.saw([]string{"1.1.1.1:8333", "2.2.2.2:8333"}, now); err != nil {
		t.Fatal(err)
	}
	if err := b.saw([]string{"3.3.3.3:8333"}, now.Add(time.Minute)); err != nil {
		t.Fatal(err)
	}
	again, err := openPeerBook(dir)
	if err != nil {
		t.Fatal(err)
	}
	got := again.latest(4, now.Add(time.Hour), anyAddr)
	want := []string{"3.3.3.3:8333", "1.1.1.1:8333", "2.2.2.2:8333"}
	if !slices.Equal(got, want) {
		t.Fatalf("latest: %v, want %v", got, want)
	}
	if got := again.latest(1, now.Add(time.Hour), anyAddr); !slices.Equal(got, want[:1]) {
		t.Fatalf("latest one: %v", got)
	}
	notThree := func(a string) bool { return a != "3.3.3.3:8333" }
	if got := again.latest(1, now.Add(time.Hour), notThree); !slices.Equal(got, want[1:2]) {
		t.Fatalf("latest usable: %v", got)
	}
}

func TestKnownPeersAreSavedOnlyNowAndThenButAlwaysOnFlush(t *testing.T) {
	dir := t.TempDir()
	now := time.Unix(1_800_000_000, 0)
	b, _ := openPeerBook(dir)
	_ = b.saw([]string{"1.1.1.1:8333"}, now)
	_ = b.saw([]string{"2.2.2.2:8333"}, now)
	// Seen again soon after: not written yet.
	later := now.Add(time.Minute)
	_ = b.saw([]string{"1.1.1.1:8333"}, later)
	if got := mustOpen(t, dir).peers["1.1.1.1:8333"]; got != now.Unix() {
		t.Fatalf("a known peer's time was written at once: %d", got)
	}
	if err := b.flush(later); err != nil {
		t.Fatal(err)
	}
	if got := mustOpen(t, dir).peers["1.1.1.1:8333"]; got != later.Unix() {
		t.Fatalf("flush did not write the time: %d", got)
	}
	// Long enough after: written.
	much := now.Add(peersSaveEvery + time.Minute)
	_ = b.saw([]string{"2.2.2.2:8333"}, much)
	if got := mustOpen(t, dir).peers["2.2.2.2:8333"]; got != much.Unix() {
		t.Fatalf("not written after peersSaveEvery: %d", got)
	}
}

func TestOldPeersAreForgottenAndTheBookIsBounded(t *testing.T) {
	dir := t.TempDir()
	now := time.Unix(1_800_000_000, 0)
	b, _ := openPeerBook(dir)
	_ = b.saw([]string{"9.9.9.9:8333"}, now.Add(-peersForgotten-time.Hour))
	var many []string
	for i := range peersKept + 10 {
		many = append(many, "10.0."+itoa(i/256)+"."+itoa(i%256)+":8333")
	}
	_ = b.saw(many, now)
	_ = b.flush(now)
	again := mustOpen(t, dir)
	if len(again.peers) != peersKept {
		t.Fatalf("kept %d peers, want %d", len(again.peers), peersKept)
	}
	if _, kept := again.peers["9.9.9.9:8333"]; kept {
		t.Fatal("a peer not seen for longer than peersForgotten was kept")
	}
}

func TestOnionAddresses(t *testing.T) {
	for addr, want := range map[string]bool{
		"abc.onion:8333":  true,
		"ABC.ONION:8333":  true,
		"abc.onion":       true,
		"1.2.3.4:8333":    false,
		"[::1]:8333":      false,
		"onion.org:8333":  false,
		"seed.onion.com":  false,
		"example.com:833": false,
	} {
		if got := isOnion(addr); got != want {
			t.Errorf("isOnion(%q) = %v", addr, got)
		}
	}
}

// fakeNet is a network for upkeep: what it is told to connect to, and
// the peers and heights it answers with.
type fakeNet struct {
	peers      []string
	connected  []string
	hdrs, best int32
	isCurrent  bool
}

func (f *fakeNet) handshaken() []string { return f.peers }
func (f *fakeNet) connect(addr string) error {
	f.connected = append(f.connected, addr)
	if addr == "fail:1" {
		return errors.New("refused")
	}
	return nil
}
func (f *fakeNet) headers() int32    { return f.hdrs }
func (f *fakeNet) peerHeight() int32 { return f.best }
func (f *fakeNet) current() bool     { return f.isCurrent }

func quiet() *slog.Logger { return slog.New(slog.NewTextHandler(io.Discard, nil)) }

func TestAStartReachesTheLatestRememberedPeersItCanReach(t *testing.T) {
	dir := t.TempDir()
	now := time.Unix(1_800_000_000, 0)
	b, _ := openPeerBook(dir)
	_ = b.saw([]string{"a.onion:8333"}, now.Add(-time.Minute))
	_ = b.saw([]string{"1.1.1.1:8333", "2.2.2.2:8333", "3.3.3.3:8333", "4.4.4.4:8333", "5.5.5.5:8333"}, now.Add(-2*time.Minute))
	net := &fakeNet{}
	noOnion := func(a string) bool { return !isOnion(a) }
	newUpkeep(net, b, nil, noOnion, quiet(), now).reconnect(now)
	want := []string{"1.1.1.1:8333", "2.2.2.2:8333", "3.3.3.3:8333", "4.4.4.4:8333"}
	if !slices.Equal(net.connected, want) {
		t.Fatalf("reached %v, want %v", net.connected, want)
	}
	net = &fakeNet{}
	newUpkeep(net, b, nil, anyAddr, quiet(), now).reconnect(now)
	if net.connected[0] != "a.onion:8333" || len(net.connected) != peersFirst {
		t.Fatalf("through a proxy, reached %v", net.connected)
	}
}

func TestPeersHandshakenAreRemembered(t *testing.T) {
	dir := t.TempDir()
	now := time.Unix(1_800_000_000, 0)
	b, _ := openPeerBook(dir)
	net := &fakeNet{peers: []string{"1.1.1.1:8333"}}
	u := newUpkeep(net, b, nil, anyAddr, quiet(), now)
	u.step(now.Add(upkeepEvery))
	if got := mustOpen(t, dir).latest(4, now, anyAddr); !slices.Equal(got, []string{"1.1.1.1:8333"}) {
		t.Fatalf("remembered %v", got)
	}
}

func TestFixedSeedsAreReachedOnlyWhileNoPeerAnswers(t *testing.T) {
	now := time.Unix(1_800_000_000, 0)
	seeds := []string{"s1:1", "s2:1", "s3:1", "s4:1", "s5:1", "s6:1", "x.onion:1"}
	noOnion := func(a string) bool { return !isOnion(a) }
	net := &fakeNet{}
	u := newUpkeep(net, nil, seeds, noOnion, quiet(), now)

	u.step(now.Add(seedsAfter - time.Second))
	if len(net.connected) != 0 {
		t.Fatalf("seeds reached before discovery had its chance: %v", net.connected)
	}
	u.step(now.Add(seedsAfter))
	if len(net.connected) != seedsAtOnce {
		t.Fatalf("first round reached %v", net.connected)
	}
	u.step(now.Add(seedsAfter + time.Second))
	if len(net.connected) != seedsAtOnce {
		t.Fatal("a round reached seeds again before seedsAfter")
	}
	u.step(now.Add(2 * seedsAfter))
	if len(net.connected) != 6 {
		t.Fatalf("the second round should reach the two left: %v", net.connected)
	}
	first := slices.Clone(net.connected)
	slices.Sort(first)
	if !slices.Equal(first, seeds[:6]) {
		t.Fatalf("two rounds reached %v, want each reachable seed once", first)
	}
	// With a peer, no more seeds.
	net.peers = []string{"p:1"}
	u.step(now.Add(10 * seedsAfter))
	if len(net.connected) != 6 {
		t.Fatalf("seeds reached with a peer: %v", net.connected)
	}
	// No peer again: a round starts over.
	net.peers = nil
	u.step(now.Add(11 * seedsAfter))
	if len(net.connected) != 6+seedsAtOnce {
		t.Fatalf("seeds not reached again once all were tried: %v", net.connected)
	}
}

func mustOpen(t *testing.T, dir string) *peerBook {
	t.Helper()
	b, err := openPeerBook(dir)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

func itoa(i int) string {
	return string([]byte{byte('0' + i/100), byte('0' + i/10%10), byte('0' + i%10)})
}
