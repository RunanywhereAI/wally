package cmd

import (
	"bytes"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/RunanywhereAI/wally/credstore"
)

func TestWhoamiSignedIn(t *testing.T) {
	store := newTestStore(t)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/v1/me" {
			io.WriteString(w, `{"email":"dev@example.com","plan":"pro","tokens_this_month":1200,"monthly_token_limit":50000}`)
			return
		}
		w.WriteHeader(http.StatusNotFound)
	}))
	defer srv.Close()
	t.Setenv("WALLY_CONSOLE_URL", srv.URL)

	if err := store.Save(credstore.Credentials{AccessToken: "toklive123", ExpiresAt: time.Now().Add(time.Hour).Unix()}); err != nil {
		t.Fatal(err)
	}

	var out bytes.Buffer
	cmd := newWhoamiCmd()
	cmd.SetOut(&out)
	if err := cmd.Execute(); err != nil {
		t.Fatal(err)
	}
	got := out.String()
	for _, want := range []string{"dev@example.com", "pro", "1200", "50000"} {
		if !strings.Contains(got, want) {
			t.Errorf("whoami output missing %q:\n%s", want, got)
		}
	}
}

func TestWhoamiNotSignedIn(t *testing.T) {
	newTestStore(t)
	cmd := newWhoamiCmd()
	err := cmd.Execute()
	if err == nil || !strings.Contains(err.Error(), "wally login") {
		t.Errorf("err = %v, want a login hint", err)
	}
}

func TestLogoutRevokesAndClears(t *testing.T) {
	store := newTestStore(t)
	var revokeHit bool
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/auth/cli/revoke" {
			revokeHit = true
			w.WriteHeader(http.StatusOK)
			return
		}
		w.WriteHeader(http.StatusNotFound)
	}))
	defer srv.Close()
	t.Setenv("WALLY_CONSOLE_URL", srv.URL)

	if err := store.Save(credstore.Credentials{AccessToken: "toklive123", RefreshToken: "reflive123", ExpiresAt: time.Now().Add(time.Hour).Unix()}); err != nil {
		t.Fatal(err)
	}

	var out bytes.Buffer
	cmd := newLogoutCmd()
	cmd.SetOut(&out)
	if err := cmd.Execute(); err != nil {
		t.Fatal(err)
	}
	if !revokeHit {
		t.Error("logout did not call revoke")
	}
	creds, _ := store.Load()
	if creds.SignedIn() {
		t.Error("logout did not clear the stored credential")
	}
	if !strings.Contains(out.String(), "Signed out") {
		t.Errorf("output = %q", out.String())
	}
}

func TestLogoutClearsEvenWhenRevokeFails(t *testing.T) {
	store := newTestStore(t)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer srv.Close()
	t.Setenv("WALLY_CONSOLE_URL", srv.URL)

	if err := store.Save(credstore.Credentials{AccessToken: "toklive123", RefreshToken: "reflive123", ExpiresAt: time.Now().Add(time.Hour).Unix()}); err != nil {
		t.Fatal(err)
	}

	var out bytes.Buffer
	cmd := newLogoutCmd()
	cmd.SetOut(&out)
	if err := cmd.Execute(); err != nil {
		t.Fatal(err)
	}
	creds, _ := store.Load()
	if creds.SignedIn() {
		t.Error("a failed revoke must still clear the local credential")
	}
	if !strings.Contains(out.String(), "could not be reached") {
		t.Errorf("output should explain the console was unreachable: %q", out.String())
	}
}
