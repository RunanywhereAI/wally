package cmd

import (
	"os"
	"os/exec"
	"time"

	"github.com/RunanywhereAI/wally/daemon"
	"github.com/RunanywhereAI/wally/errmap"
)

func ensureDaemon() error {
	if daemon.Running() {
		if !daemonStale() {
			return nil
		}
		// A daemon from an older build is listening: a reinstall left the
		// previous `wally serve` running, so clients would talk to stale code.
		// Stop it and start one from this binary instead.
		_ = stopDaemon()
		waitDaemonStopped(5 * time.Second)
	}
	return spawnDaemon()
}

// daemonStale reports whether the daemon already listening is a different or
// older build than this binary. A daemon that does not answer /whoami predates
// the check and counts as stale.
func daemonStale() bool {
	id, ok := daemon.FetchIdentity()
	if !ok {
		return true
	}
	self, err := os.Executable()
	if err != nil {
		return false
	}
	fi, err := os.Stat(self)
	if err != nil {
		return false
	}
	return staleByIdentity(id, self, fi.ModTime())
}

// staleByIdentity is the pure decision daemonStale wraps: the running daemon is
// stale when it runs a different binary, or when this binary's file was written
// after that daemon started (a reinstall rewrites it in place, so the mtime
// moves past the old process's start time even when the version is unchanged).
func staleByIdentity(id daemon.Identity, self string, selfMod time.Time) bool {
	if id.Exe != "" && id.Exe != self {
		return true
	}
	return selfMod.Unix() > id.StartedAtUnix
}

func spawnDaemon() error {
	self, err := os.Executable()
	if err != nil {
		return errmap.NewDaemonNotRunning().WithDetail(err.Error())
	}
	c := exec.Command(self, "serve", "--foreground")
	detach(c)
	if err := c.Start(); err != nil {
		return errmap.NewDaemonNotRunning().WithDetail(err.Error())
	}
	_ = c.Process.Release()

	deadline := time.Now().Add(8 * time.Second)
	for time.Now().Before(deadline) {
		if daemon.Running() {
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return errmap.NewDaemonNotRunning()
}

func waitDaemonStopped(d time.Duration) {
	deadline := time.Now().Add(d)
	for time.Now().Before(deadline) {
		if !daemon.Running() {
			return
		}
		time.Sleep(100 * time.Millisecond)
	}
}
