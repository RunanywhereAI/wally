package cmd

import (
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/zalando/go-keyring"

	"github.com/RunanywhereAI/wally/console"
	"github.com/RunanywhereAI/wally/credstore"
	"github.com/RunanywhereAI/wally/runanywhere"
)

func newTestStore(t *testing.T) *credstore.Store {
	t.Helper()
	keyring.MockInit()
	t.Setenv("WALLY_PROFILE_DIR", t.TempDir())
	s, err := credstore.New()
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func TestSessionTokenNotSignedIn(t *testing.T) {
	s := &session{store: newTestStore(t), client: console.New()}
	if _, err := s.Token(); !errors.Is(err, runanywhere.ErrNotSignedIn) {
		t.Errorf("err = %v, want ErrNotSignedIn", err)
	}
}

func TestSessionTokenValid(t *testing.T) {
	store := newTestStore(t)
	if err := store.Save(credstore.Credentials{AccessToken: "toklive123", ExpiresAt: time.Now().Add(time.Hour).Unix()}); err != nil {
		t.Fatal(err)
	}
	s := &session{store: store, client: console.New()}
	got, err := s.Token()
	if err != nil {
		t.Fatal(err)
	}
	if got != "toklive123" {
		t.Errorf("token = %q", got)
	}
}

func TestSessionTokenRefreshesWhenExpired(t *testing.T) {
	var refreshHit bool
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/auth/cli/refresh" {
			refreshHit = true
			w.Header().Set("Content-Type", "application/json")
			io.WriteString(w, `{"access_token":"toknew456","refresh_token":"refnew456","email":"a@b.co","expires_in":3600}`)
			return
		}
		w.WriteHeader(http.StatusNotFound)
	}))
	defer srv.Close()

	store := newTestStore(t)
	if err := store.Save(credstore.Credentials{AccessToken: "tokold000", RefreshToken: "refold000", ExpiresAt: time.Now().Add(-time.Hour).Unix()}); err != nil {
		t.Fatal(err)
	}
	s := &session{store: store, client: console.New(console.WithBaseURL(srv.URL))}

	got, err := s.Token()
	if err != nil {
		t.Fatal(err)
	}
	if !refreshHit {
		t.Error("expected the session to call refresh")
	}
	if got != "toknew456" {
		t.Errorf("token = %q, want the refreshed token", got)
	}

	reloaded, err := store.Load()
	if err != nil {
		t.Fatal(err)
	}
	if reloaded.AccessToken != "toknew456" || reloaded.RefreshToken != "refnew456" {
		t.Errorf("store not updated after refresh: %+v", reloaded)
	}
}
