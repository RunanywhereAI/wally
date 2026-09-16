package daemon

import (
	"encoding/json"
	"errors"
	"net/http"
	"os"
	"syscall"
	"time"
)

// Identity is who a running daemon is: its build version, when the process
// started, and the binary it runs from. A client reads it to decide whether a
// daemon already listening is the current binary or a stale one from before a
// reinstall that must be replaced.
type Identity struct {
	Version       string `json:"version"`
	StartedAtUnix int64  `json:"started_at_unix"`
	Exe           string `json:"exe"`
}

// processStart is when this daemon process began. whoami reports it so a client
// can compare it against the on-disk binary's mtime: a reinstall rewrites the
// binary after the running daemon started, which is how "stale" is detected
// even when the version string has not changed between builds.
var processStart = time.Now()

func selfExe() string {
	p, _ := os.Executable()
	return p
}

// FetchIdentity asks the daemon at Addr() who it is. ok is false when nothing
// answers or the daemon predates /whoami, both of which a caller treats as a
// stale daemon to replace.
func FetchIdentity() (id Identity, ok bool) {
	client := http.Client{Timeout: 500 * time.Millisecond}
	resp, err := client.Get("http://" + Addr() + "/whoami")
	if err != nil {
		return Identity{}, false
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return Identity{}, false
	}
	if err := json.NewDecoder(resp.Body).Decode(&id); err != nil {
		return Identity{}, false
	}
	return id, true
}

// Running reports whether the daemon is listening and healthy. Only a real
// 200 from /healthz counts.
//
// A connection refused settles it immediately: nothing is bound to the port,
// the same line ollama/cmd/cmd.go's checkServerHeartbeat draws (it matches
// " refused" in the error text) before deciding to start a server. A
// timeout is a different claim — the health check was slow, not that
// nothing is listening — and the caller, ensureDaemon in cmd/ensure.go,
// reacts to "not running" by spawning a second `wally serve`. One retry
// keeps a single slow health check from doing that to a daemon that was
// already running.
func Running() bool {
	if ok, refused := healthz(); ok || refused {
		return ok
	}
	ok, _ := healthz()
	return ok
}

// healthz makes one /healthz request. refused is true only for a connection
// refused, the one outcome Running does not retry.
func healthz() (ok, refused bool) {
	client := http.Client{Timeout: 500 * time.Millisecond}
	resp, err := client.Get("http://" + Addr() + "/healthz")
	if err != nil {
		return false, errors.Is(err, syscall.ECONNREFUSED)
	}
	defer resp.Body.Close()
	return resp.StatusCode == http.StatusOK, false
}
