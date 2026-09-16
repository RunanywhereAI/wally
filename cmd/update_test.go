package cmd

import "testing"

func TestIsNewer(t *testing.T) {
	cases := []struct {
		latest, current string
		want            bool
	}{
		{"0.5.7", "0.6.0-dev", false}, // dev build is ahead of the old release
		{"v0.7.0", "0.6.0-dev", true}, // a real newer release
		{"0.6.0", "0.6.0-dev", false}, // same base, prerelease ignored
		{"0.6.1", "0.6.0", true},      // patch bump
		{"1.0.0", "0.9.9", true},      // major bump
		{"0.6.0", "0.6.0", false},     // equal
	}
	for _, tc := range cases {
		if got := isNewer(tc.latest, tc.current); got != tc.want {
			t.Errorf("isNewer(%q, %q) = %v, want %v", tc.latest, tc.current, got, tc.want)
		}
	}
}
