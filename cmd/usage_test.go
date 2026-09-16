package cmd

import (
	"strings"
	"testing"
)

func TestUsageNotSignedIn(t *testing.T) {
	newTestStore(t)
	err := newUsageCmd().Execute()
	if err == nil || !strings.Contains(err.Error(), "wally login") {
		t.Errorf("err = %v, want a login hint", err)
	}
}

func TestDollars(t *testing.T) {
	if got := dollars(2_500_000); got != 2.5 {
		t.Errorf("dollars(2_500_000) = %v, want 2.5", got)
	}
}
