package daemon

import (
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func TestRunningTrueOnReal200(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))
	defer srv.Close()
	t.Setenv("WALLY_HOST", strings.TrimPrefix(srv.URL, "http://"))

	if !Running() {
		t.Error("Running() = false, want true for a real 200")
	}
}

func TestRunningFalseOnConnectionRefused(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := ln.Addr().String()
	ln.Close() // nothing listening now: the port refuses

	t.Setenv("WALLY_HOST", addr)

	if Running() {
		t.Error("Running() = true, want false: nothing is listening")
	}
}

func TestRunningFalseOnPersistentTimeout(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(700 * time.Millisecond) // longer than healthz's 500ms client timeout
		w.WriteHeader(http.StatusOK)
	}))
	defer srv.Close()
	t.Setenv("WALLY_HOST", strings.TrimPrefix(srv.URL, "http://"))

	if Running() {
		t.Error("Running() = true, want false: both attempts timed out")
	}
}

func TestRunningRetriesOnceAfterATimeout(t *testing.T) {
	// A daemon that answered slowly once and then answered a 200 is the case
	// the retry exists for: ensureDaemon (cmd/ensure.go) would otherwise
	// read the first timeout as "not running" and spawn a redundant `wally
	// serve` beside an already-running one.
	var calls int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if atomic.AddInt32(&calls, 1) == 1 {
			time.Sleep(700 * time.Millisecond)
			return
		}
		w.WriteHeader(http.StatusOK)
	}))
	defer srv.Close()
	t.Setenv("WALLY_HOST", strings.TrimPrefix(srv.URL, "http://"))

	if !Running() {
		t.Error("Running() = false, want true: the retry should have caught the second, healthy attempt")
	}
}
