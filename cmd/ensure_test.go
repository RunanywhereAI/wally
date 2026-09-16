package cmd

import (
	"testing"
	"time"

	"github.com/RunanywhereAI/wally/daemon"
)

func TestStaleByIdentity(t *testing.T) {
	start := time.Unix(1_000, 0)
	const self = "/home/x/.local/bin/wally"

	cases := []struct {
		name    string
		id      daemon.Identity
		self    string
		selfMod time.Time
		want    bool
	}{
		{
			name:    "same binary started after its mtime is current",
			id:      daemon.Identity{Exe: self, StartedAtUnix: start.Unix()},
			self:    self,
			selfMod: start.Add(-time.Minute),
			want:    false,
		},
		{
			name:    "binary rewritten after the daemon started is stale",
			id:      daemon.Identity{Exe: self, StartedAtUnix: start.Unix()},
			self:    self,
			selfMod: start.Add(time.Minute),
			want:    true,
		},
		{
			name:    "different executable path is stale",
			id:      daemon.Identity{Exe: "/usr/local/bin/wally", StartedAtUnix: start.Unix()},
			self:    self,
			selfMod: start.Add(-time.Minute),
			want:    true,
		},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := staleByIdentity(c.id, c.self, c.selfMod); got != c.want {
				t.Fatalf("staleByIdentity = %v, want %v", got, c.want)
			}
		})
	}
}
