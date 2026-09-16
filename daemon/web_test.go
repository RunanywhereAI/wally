package daemon

import (
	"encoding/json"
	"io"
	"net/http"
	"strings"
	"testing"
)

func TestDashboardServesHTML(t *testing.T) {
	d := newTestDaemon(NewRouter(nil))
	defer d.Close()

	resp, err := http.Get(d.URL + "/")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		t.Errorf("status = %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); !strings.HasPrefix(ct, "text/html") {
		t.Errorf("content-type = %q, want text/html", ct)
	}
	body, _ := io.ReadAll(resp.Body)
	if !strings.Contains(string(body), "Wally dashboard") {
		t.Errorf("body does not look like the dashboard page: %q", truncate(string(body), 200))
	}
}

func TestDashboardMissingPathIsNotFound(t *testing.T) {
	d := newTestDaemon(NewRouter(nil))
	defer d.Close()

	resp, err := http.Get(d.URL + "/does-not-exist")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusNotFound {
		t.Errorf("status = %d, want 404", resp.StatusCode)
	}
}

func TestStatusShape(t *testing.T) {
	d := newTestDaemon(NewRouter(nil))
	defer d.Close()

	resp, err := http.Get(d.URL + "/status")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		t.Errorf("status = %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); ct != "application/json" {
		t.Errorf("content-type = %q, want application/json", ct)
	}

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal(err)
	}

	var got map[string]json.RawMessage
	if err := json.Unmarshal(body, &got); err != nil {
		t.Fatalf("body did not decode as JSON: %v\nbody: %s", err, body)
	}
	for _, key := range []string{"ok", "channel", "on_device_enabled", "cloud_models", "on_device_models", "harnesses"} {
		if _, ok := got[key]; !ok {
			t.Errorf("status response is missing key %q; body: %s", key, body)
		}
	}
}

// TestStatusNeverLeaksEndpointsOrSecrets guards the hard rule: /status must
// never carry the console API URL, the approval/web-origin URL, or a token.
// config.Channel() collapses both of those to the word "development" or
// "production" for exactly this reason.
func TestStatusNeverLeaksEndpointsOrSecrets(t *testing.T) {
	d := newTestDaemon(NewRouter(nil))
	defer d.Close()

	resp, err := http.Get(d.URL + "/status")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal(err)
	}
	low := strings.ToLower(string(body))

	// Harness install hints legitimately carry third-party install URLs
	// (claude.ai, hermes-agent.nousresearch.com), so this checks for our own
	// console and credentials, not for URLs in general.
	forbidden := []string{
		"inference.runanywhere", "console.runanywhere", "railway",
		"token", "authorization", "bearer", "apikey", "api_key",
	}
	for _, s := range forbidden {
		if strings.Contains(low, s) {
			t.Errorf("status body contains forbidden substring %q; body: %s", s, body)
		}
	}
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}
