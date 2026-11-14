package mobile

import (
	"bytes"
	"os"
	"path/filepath"
	"testing"
)

func TestTheLogIsMovedAsideOnceFull(t *testing.T) {
	path := filepath.Join(t.TempDir(), "chain.log")
	if err := os.WriteFile(path, []byte("before\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	r, err := openLog(path)
	if err != nil {
		t.Fatal(err)
	}
	line := bytes.Repeat([]byte("x"), 1023)
	line = append(line, '\n')
	for i := 0; i < (logMax-len("before\n"))/len(line); i++ {
		if _, err := r.Write(line); err != nil {
			t.Fatal(err)
		}
	}
	// Appended to what was there, leaving no room for one more line.
	if _, err := os.Stat(path + ".1"); !os.IsNotExist(err) {
		t.Fatalf("moved aside early: %v", err)
	}
	if _, err := r.Write(line); err != nil {
		t.Fatal(err)
	}
	old, _ := os.ReadFile(path + ".1")
	now, _ := os.ReadFile(path)
	if !bytes.HasPrefix(old, []byte("before\n")) || !bytes.Equal(now, line) {
		t.Fatalf("aside %d bytes, now %q", len(old), now)
	}
	if err := SetLog(filepath.Join(t.TempDir(), "none", "chain.log")); err == nil {
		t.Error("a log in a missing directory")
	}
}
