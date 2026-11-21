package node

import (
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/btcsuite/btcwallet/walletdb"
)

// noFreelist is the freelist page bbolt's meta names when it keeps none on
// disk, rebuilding it on every open by reading the whole file.
const noFreelist = ^uint64(0)

// freelistPage is the freelist page named by the newer of the two meta
// pages of the bbolt file at path. bbolt's file format: a page header of
// 16 bytes, then the meta's magic, version, page size and flags (4 bytes
// each), its root bucket (16), its freelist page (8), its page count (8)
// and its transaction id (8).
func freelistPage(t *testing.T, path string) uint64 {
	t.Helper()
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	size := int(binary.LittleEndian.Uint32(raw[24:]))
	var freelist, newest uint64
	for _, at := range []int{0, size} {
		meta := raw[at+16:]
		if binary.LittleEndian.Uint32(meta) != 0xED0CDAED {
			t.Fatalf("no bbolt meta at %d", at)
		}
		if txid := binary.LittleEndian.Uint64(meta[48:]); txid >= newest {
			newest, freelist = txid, binary.LittleEndian.Uint64(meta[32:])
		}
	}
	return freelist
}

// churn writes and deletes enough to leave free pages, in one commit each.
func churn(t *testing.T, db walletdb.DB) {
	t.Helper()
	err := walletdb.Update(db, func(tx walletdb.ReadWriteTx) error {
		b, err := tx.CreateTopLevelBucket([]byte("churn"))
		if err != nil {
			return err
		}
		for i := range 2000 {
			if err := b.Put(fmt.Appendf(nil, "key %d", i), make([]byte, 100)); err != nil {
				return err
			}
		}
		return nil
	})
	if err == nil {
		err = walletdb.Update(db, func(tx walletdb.ReadWriteTx) error {
			return tx.DeleteTopLevelBucket([]byte("churn"))
		})
	}
	if err != nil {
		t.Fatal(err)
	}
}

func TestTheChainDatabaseKeepsItsFreelist(t *testing.T) {
	path := filepath.Join(t.TempDir(), "neutrino.db")
	db, err := openChainDB(path)
	if err != nil {
		t.Fatal(err)
	}
	churn(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if freelistPage(t, path) == noFreelist {
		t.Fatal("no freelist kept: every open reads the whole file")
	}
}

// A file an older release wrote, with no freelist, gets one as it is
// opened, before any commit: a start killed then does not read it whole
// again. That release still opens the file after.
func TestAnOlderReleasesChainDatabaseGetsAFreelist(t *testing.T) {
	path := filepath.Join(t.TempDir(), "neutrino.db")
	older := func() walletdb.DB {
		t.Helper()
		db, err := walletdb.Create("bdb", path, true, time.Minute, false)
		if err != nil {
			t.Fatal(err)
		}
		return db
	}
	db := older()
	churn(t, db)
	_ = db.Close()
	if freelistPage(t, path) != noFreelist {
		t.Fatal("the older release's file has a freelist: this test proves nothing")
	}

	db, err := openChainDB(path)
	if err != nil {
		t.Fatal(err)
	}
	_ = db.Close()
	if freelistPage(t, path) == noFreelist {
		t.Fatal("no freelist once opened")
	}
	if db, err = openChainDB(path); err != nil {
		t.Fatal(err)
	}
	churn(t, db)
	_ = db.Close()
	if freelistPage(t, path) == noFreelist {
		t.Fatal("no freelist after a commit")
	}

	db = older()
	err = walletdb.View(db, func(tx walletdb.ReadTx) error {
		if tx.ReadBucket([]byte("churn")) != nil {
			return fmt.Errorf("a deleted bucket read back")
		}
		return nil
	})
	churn(t, db)
	_ = db.Close()
	if err != nil {
		t.Fatal(err)
	}
}
