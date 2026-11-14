package mobile

import (
	"io"
	"log/slog"
	"os"
	"sync"

	"github.com/btcsuite/btclog"
	"github.com/lightninglabs/neutrino"
)

// logMax is the size a log file grows to before it is moved aside, to
// path.1, replacing the one there: what is kept is at most twice this.
const logMax = 1 << 20

var (
	logMu  sync.Mutex
	logOut io.Writer = os.Stderr
)

// SetLog has the light clients started from now on log to the file at
// path as well, besides standard error (on Android, logcat), neutrino's
// peers and sync among it.
func SetLog(path string) error {
	f, err := openLog(path)
	if err != nil {
		return err
	}
	logMu.Lock()
	defer logMu.Unlock()
	logOut = io.MultiWriter(os.Stderr, f)
	upstream := btclog.NewBackend(logOut).Logger("LCHN")
	upstream.SetLevel(btclog.LevelInfo)
	neutrino.UseLogger(upstream)
	return nil
}

func logger() *slog.Logger {
	logMu.Lock()
	defer logMu.Unlock()
	return slog.New(slog.NewTextHandler(logOut, nil))
}

// rotating is a log file moved aside once it reaches logMax.
type rotating struct {
	mu   sync.Mutex
	path string
	f    *os.File
	size int64
}

func openLog(path string) (*rotating, error) {
	r := &rotating{path: path}
	if err := r.open(); err != nil {
		return nil, err
	}
	return r, nil
}

func (r *rotating) open() error {
	f, err := os.OpenFile(r.path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return err
	}
	info, err := f.Stat()
	if err != nil {
		_ = f.Close()
		return err
	}
	r.f, r.size = f, info.Size()
	return nil
}

func (r *rotating) Write(p []byte) (int, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.size+int64(len(p)) > logMax {
		_ = r.f.Close()
		_ = os.Rename(r.path, r.path+".1")
		if err := r.open(); err != nil {
			return 0, err
		}
	}
	n, err := r.f.Write(p)
	r.size += int64(n)
	return n, err
}
