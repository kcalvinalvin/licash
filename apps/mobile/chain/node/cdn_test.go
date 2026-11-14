package node

import (
	"bytes"
	"testing"
)

func TestCompactSize(t *testing.T) {
	for _, c := range []struct {
		in    []byte
		value uint64
		width int
	}{
		{nil, 0, 0},
		{[]byte{0}, 0, 1},
		{[]byte{0xfc}, 0xfc, 1},
		{[]byte{0xfd, 0x34, 0x12}, 0x1234, 3},
		{[]byte{0xfd, 0x34}, 0, 0},
		{[]byte{0xfe, 4, 3, 2, 1}, 0x01020304, 5},
		{[]byte{0xff, 8, 7, 6, 5, 4, 3, 2, 1}, 0x0102030405060708, 9},
		{[]byte{0xff, 1, 2, 3}, 0, 0},
	} {
		value, width := compactSize(c.in)
		if value != c.value || width != c.width {
			t.Errorf("%x: %d, %d; want %d, %d", c.in, value, width, c.value, c.width)
		}
	}
}

func TestSplitFilters(t *testing.T) {
	long := bytes.Repeat([]byte{7}, 300)
	file := append([]byte{2, 0xaa, 0xbb, 0}, 0xfd, 0x2c, 0x01)
	file = append(file, long...)
	filters, err := splitFilters(file, 3)
	if err != nil {
		t.Fatal(err)
	}
	if len(filters) != 3 || !bytes.Equal(filters[0], []byte{0xaa, 0xbb}) ||
		len(filters[1]) != 0 || !bytes.Equal(filters[2], long) {
		t.Fatalf("%x", filters)
	}
	for name, c := range map[string]struct {
		file  []byte
		count uint32
	}{
		"fewer":     {file, 4},
		"more":      {file, 2},
		"cut short": {file[:len(file)-1], 3},
		"no length": {append(file[:4:4], 0xfd), 2},
	} {
		if _, err := splitFilters(c.file, c.count); err == nil {
			t.Errorf("%s: read", name)
		}
	}
	if filters, err := splitFilters(nil, 0); err != nil || len(filters) != 0 {
		t.Errorf("empty: %v %v", filters, err)
	}
}
