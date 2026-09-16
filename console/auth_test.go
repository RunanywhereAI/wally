package console

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

func newTestClient(t *testing.T, handler http.HandlerFunc) *Client {
	t.Helper()
	server := httptest.NewServer(handler)
	t.Cleanup(server.Close)
	c := New(WithBaseURL(server.URL))
	c.sleep = func(time.Duration) {} // hermetic: never really wait in a test
	return c
}

func TestStartAuthorization_Success(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/auth/cli/start" {
			t.Fatalf("unexpected request: %s %s", r.Method, r.URL.Path)
		}
		var body cliStartRequest
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Fatalf("decoding request body: %v", err)
		}
		if body.Client != "rcli" || body.Hostname != "test-host" {
			t.Fatalf("unexpected request body: %+v", body)
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(cliStartResponse{
			RequestCode:     "req-code-1234",
			PollSecret:      "poll-secret-1234",
			VerificationURL: "https://console.runanywhere.ai/cloud/cli?code=req-code-1234",
			ExpiresIn:       99999, // out of range on purpose, to prove clamping
			Interval:        0,     // out of range on purpose, to prove clamping
		})
	})

	auth, err := client.StartAuthorization(context.Background(), "test-host", nil)
	if err != nil {
		t.Fatalf("StartAuthorization() error = %v", err)
	}
	if auth.RequestCode != "req-code-1234" || auth.PollSecret != "poll-secret-1234" {
		t.Fatalf("unexpected authorization: %+v", auth)
	}
	if auth.ExpiresIn != 1800*time.Second {
		t.Fatalf("ExpiresIn = %v, want clamped to 1800s", auth.ExpiresIn)
	}
	if auth.Interval != 1*time.Second {
		t.Fatalf("Interval = %v, want clamped to 1s", auth.Interval)
	}
}

func TestStartAuthorization_RetriesOnRateLimit(t *testing.T) {
	attempts := 0
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		attempts++
		if attempts == 1 {
			w.Header().Set("Retry-After", "1")
			w.WriteHeader(http.StatusTooManyRequests)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(cliStartResponse{
			RequestCode: "req-code-1234", PollSecret: "poll-secret-1234",
			VerificationURL: "https://console.runanywhere.ai/cloud/cli", ExpiresIn: 300, Interval: 2,
		})
	})

	var waited time.Duration
	client.sleep = func(d time.Duration) { waited = d }

	auth, err := client.StartAuthorization(context.Background(), "test-host", nil)
	if err != nil {
		t.Fatalf("StartAuthorization() error = %v", err)
	}
	if attempts != 2 {
		t.Fatalf("attempts = %d, want 2", attempts)
	}
	if waited != time.Second {
		t.Fatalf("slept %v, want 1s (the server's Retry-After)", waited)
	}
	if auth.RequestCode != "req-code-1234" {
		t.Fatalf("unexpected authorization: %+v", auth)
	}
}

func TestStartAuthorization_LongRateLimitFailsInsteadOfHanging(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "3600")
		w.WriteHeader(http.StatusTooManyRequests)
	})

	if _, err := client.StartAuthorization(context.Background(), "test-host", nil); err == nil {
		t.Fatal("expected an error when the console asks for a wait past the local ceiling")
	}
}

func TestPoll_Approved(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"status":"approved","access_token":"at-123","refresh_token":"rt-456","email":"person@example.com","expires_in":3600}`))
	})

	outcome, err := client.Poll(context.Background(), &Authorization{RequestCode: "req", PollSecret: "secret"})
	if err != nil {
		t.Fatalf("Poll() error = %v", err)
	}
	if outcome.Status != PollApproved {
		t.Fatalf("Status = %v, want Approved", outcome.Status)
	}
	if outcome.Grant == nil || outcome.Grant.AccessToken != "at-123" || outcome.Grant.Email != "person@example.com" {
		t.Fatalf("unexpected grant: %+v", outcome.Grant)
	}
}

func TestPoll_MissingFieldsDefault(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"status":"pending"}`))
	})

	outcome, err := client.Poll(context.Background(), &Authorization{RequestCode: "req", PollSecret: "secret"})
	if err != nil {
		t.Fatalf("Poll() error = %v", err)
	}
	if outcome.Status != PollPending || outcome.Grant != nil {
		t.Fatalf("unexpected outcome for a minimal pending response: %+v", outcome)
	}
}

func TestPoll_UnknownStatusFails(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"status":"bogus"}`))
	})

	if _, err := client.Poll(context.Background(), &Authorization{RequestCode: "req", PollSecret: "secret"}); err == nil {
		t.Fatal("expected an unrecognized poll status to fail rather than be treated as pending")
	}
}

func TestPoll_WrongTypeFails(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"status":123}`))
	})

	if _, err := client.Poll(context.Background(), &Authorization{RequestCode: "req", PollSecret: "secret"}); err == nil {
		t.Fatal("expected a status of the wrong JSON type to fail")
	}
}

func TestPoll_RateLimitIsPendingWithRetryAfter(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "5")
		w.WriteHeader(http.StatusTooManyRequests)
	})

	outcome, err := client.Poll(context.Background(), &Authorization{RequestCode: "req", PollSecret: "secret"})
	if err != nil {
		t.Fatalf("Poll() error = %v", err)
	}
	if outcome.Status != PollPending || outcome.RetryAfter != 5*time.Second {
		t.Fatalf("unexpected outcome for a 429: %+v", outcome)
	}
}

func TestNextPollDelay(t *testing.T) {
	cases := []struct {
		name       string
		interval   time.Duration
		retryAfter time.Duration
		want       time.Duration
	}{
		{"interval is the floor", 2 * time.Second, 0, 2 * time.Second},
		{"console's ask raises it", 2 * time.Second, 10 * time.Second, 10 * time.Second},
		{"a shorter ask never lowers it", 5 * time.Second, 1 * time.Second, 5 * time.Second},
		{"a negative ask is treated as none", 3 * time.Second, -1 * time.Second, 3 * time.Second},
		{"interval below a second floors at a second", 0, 0, time.Second},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := NextPollDelay(tc.interval, tc.retryAfter); got != tc.want {
				t.Fatalf("NextPollDelay(%v, %v) = %v, want %v", tc.interval, tc.retryAfter, got, tc.want)
			}
		})
	}
}

func TestPollUntilGranted_Loop(t *testing.T) {
	calls := 0
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		calls++
		w.Header().Set("Content-Type", "application/json")
		if calls < 3 {
			w.Write([]byte(`{"status":"pending"}`))
			return
		}
		w.Write([]byte(`{"status":"approved","access_token":"at-123","refresh_token":"rt-456","email":"person@example.com","expires_in":3600}`))
	})

	var waits int
	auth := &Authorization{RequestCode: "req", PollSecret: "secret", Interval: time.Second, ExpiresIn: time.Minute}
	grant, err := client.PollUntilGranted(context.Background(), auth, func(time.Duration) { waits++ })
	if err != nil {
		t.Fatalf("PollUntilGranted() error = %v", err)
	}
	if calls != 3 {
		t.Fatalf("calls = %d, want 3", calls)
	}
	if waits != 2 {
		t.Fatalf("waits = %d, want 2", waits)
	}
	if grant.AccessToken != "at-123" {
		t.Fatalf("unexpected grant: %+v", grant)
	}
}

func TestPollUntilGranted_Denied(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"status":"denied"}`))
	})

	auth := &Authorization{RequestCode: "req", PollSecret: "secret", Interval: time.Second, ExpiresIn: time.Minute}
	if _, err := client.PollUntilGranted(context.Background(), auth, nil); err == nil {
		t.Fatal("expected a denied login to return an error")
	}
}

func TestRefresh_Success(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/auth/cli/refresh" {
			t.Fatalf("unexpected path: %s", r.URL.Path)
		}
		var body cliRefreshRequest
		json.NewDecoder(r.Body).Decode(&body)
		if body.RefreshToken != "old-refresh-token" {
			t.Fatalf("unexpected refresh token sent: %q", body.RefreshToken)
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(grantResponse{
			AccessToken: "new-access", RefreshToken: "new-refresh", Email: "person@example.com", ExpiresIn: 3600,
		})
	})

	grant, err := client.Refresh(context.Background(), "old-refresh-token")
	if err != nil {
		t.Fatalf("Refresh() error = %v", err)
	}
	if grant.AccessToken != "new-access" {
		t.Fatalf("unexpected grant: %+v", grant)
	}
}

func TestRefresh_UnauthorizedWraps(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
	})

	_, err := client.Refresh(context.Background(), "expired-refresh-token")
	if !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("Refresh() error = %v, want it to wrap ErrUnauthorized", err)
	}
}

func TestRevoke_NoTokensIsNoop(t *testing.T) {
	called := false
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		called = true
	})
	if err := client.Revoke(context.Background(), "", ""); err != nil {
		t.Fatalf("Revoke() error = %v", err)
	}
	if called {
		t.Fatal("expected Revoke with no tokens to skip the network call")
	}
}

// TestRevoke_ServerErrorIsIndependentOfLocalClear proves the half of the
// logout contract that belongs to this package: a revoke that fails on the
// console is reported as ErrUnavailable, not swallowed and not fatal to the
// caller. credstore's TestStore_Clear proves the other half: Clear() removes
// the local credential regardless of what Revoke returned. A caller composes
// the two by calling both and ignoring Revoke's error before clearing.
func TestRevoke_ServerErrorIsIndependentOfLocalClear(t *testing.T) {
	client := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
	})

	err := client.Revoke(context.Background(), "at-123", "rt-456")
	if err == nil {
		t.Fatal("expected Revoke against a failing console to return an error")
	}
	if !errors.Is(err, ErrUnavailable) {
		t.Fatalf("Revoke() error = %v, want it to wrap ErrUnavailable", err)
	}
}
