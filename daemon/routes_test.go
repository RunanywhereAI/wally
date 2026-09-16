package daemon

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/catalog"
)

type stubSession struct {
	base  string
	token string
}

func (s stubSession) ConsoleBaseURL() string { return s.base }
func (s stubSession) Token() (string, error) { return s.token, nil }

func newTestDaemon(sess *Router) *httptest.Server {
	mux := http.NewServeMux()
	sess.register(mux)
	return httptest.NewServer(mux)
}

func TestChatCompletionsProxiesToResolvedEndpoint(t *testing.T) {
	var gotAuth, gotPath, gotBody string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotAuth = r.Header.Get("Authorization")
		gotPath = r.URL.Path
		b, _ := io.ReadAll(r.Body)
		gotBody = string(b)
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, "data: chunk-1\n\ndata: [DONE]\n\n")
	}))
	defer upstream.Close()

	d := newTestDaemon(NewRouter(stubSession{base: upstream.URL, token: "sk-test"}))
	defer d.Close()

	req := `{"model":"cloud-x","messages":[{"role":"user","content":"hi"}]}`
	resp, err := http.Post(d.URL+"/v1/chat/completions", "application/json", strings.NewReader(req))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)

	if gotAuth != "Bearer sk-test" {
		t.Errorf("upstream Authorization = %q", gotAuth)
	}
	if gotPath != "/v1/chat/completions" {
		t.Errorf("upstream path = %q", gotPath)
	}
	if gotBody != req {
		t.Errorf("upstream body = %q", gotBody)
	}
	if !strings.Contains(string(body), "chunk-1") {
		t.Errorf("streamed response = %q", body)
	}
}

func TestChatCompletionsNotSignedIn(t *testing.T) {
	d := newTestDaemon(NewRouter(nil))
	defer d.Close()

	resp, err := http.Post(d.URL+"/v1/chat/completions", "application/json", strings.NewReader(`{"model":"cloud-x"}`))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)

	if resp.StatusCode != http.StatusUnauthorized {
		t.Errorf("status = %d, want 401", resp.StatusCode)
	}
	if !strings.Contains(string(body), "wally login") {
		t.Errorf("body = %q, want a login hint", body)
	}
}

func TestChatCompletionsMissingModel(t *testing.T) {
	d := newTestDaemon(NewRouter(stubSession{base: "https://x", token: "y"}))
	defer d.Close()

	resp, err := http.Post(d.URL+"/v1/chat/completions", "application/json", strings.NewReader(`{}`))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusBadRequest {
		t.Errorf("status = %d, want 400", resp.StatusCode)
	}
}

func TestHealthz(t *testing.T) {
	d := newTestDaemon(NewRouter(nil))
	defer d.Close()

	resp, err := http.Get(d.URL + "/healthz")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Errorf("status = %d, want 200", resp.StatusCode)
	}
}

// seedCatalog points WALLY_PROFILE_DIR at a fresh directory and writes it a
// cached cloud catalog, mirroring what `wally models` leaves behind.
func seedCatalog(t *testing.T, models []catalog.Model) {
	t.Helper()
	t.Setenv("WALLY_PROFILE_DIR", t.TempDir())
	if err := catalog.Save(models); err != nil {
		t.Fatal(err)
	}
}

// seedOnDeviceModel points RUNANYWHERE_HOME at a fresh directory holding one
// discoverable on-device model, the same layout runanywhere.InstalledModels
// walks.
func seedOnDeviceModel(t *testing.T, framework, id string) {
	t.Helper()
	home := t.TempDir()
	t.Setenv("RUNANYWHERE_HOME", home)
	dir := filepath.Join(home, "RunAnywhere", "Models", framework, id)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "model.gguf"), []byte("weights"), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestModelsListsCatalogAndOnDevice(t *testing.T) {
	seedCatalog(t, []catalog.Model{{ID: "cloud-x"}, {ID: "cloud-y"}})
	seedOnDeviceModel(t, "LlamaCpp", "qwen2.5-3b")

	d := newTestDaemon(NewRouter(nil))
	defer d.Close()

	resp, err := http.Get(d.URL + "/v1/models")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); ct != "application/json" {
		t.Errorf("content-type = %q, want application/json", ct)
	}

	var got openAIModelList
	if err := json.NewDecoder(resp.Body).Decode(&got); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if got.Object != "list" {
		t.Errorf("object = %q, want %q", got.Object, "list")
	}

	ids := make(map[string]bool)
	for _, m := range got.Data {
		if m.Object != "model" || m.OwnedBy != "wally" {
			t.Errorf("entry = %+v, want object=model owned_by=wally", m)
		}
		ids[m.ID] = true
	}
	for _, want := range []string{"cloud-x", "cloud-y", "qwen2.5-3b"} {
		if !ids[want] {
			t.Errorf("data missing model %q; got %+v", want, got.Data)
		}
	}
}

func TestCompletionsProxiesToResolvedEndpoint(t *testing.T) {
	var gotPath, gotAuth string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath = r.URL.Path
		gotAuth = r.Header.Get("Authorization")
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"id":"cmpl-1","choices":[{"text":"hi"}]}`)
	}))
	defer upstream.Close()

	d := newTestDaemon(NewRouter(stubSession{base: upstream.URL, token: "sk-test"}))
	defer d.Close()

	resp, err := http.Post(d.URL+"/v1/completions", "application/json", strings.NewReader(`{"model":"cloud-x","prompt":"hi"}`))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)

	if gotPath != "/v1/completions" {
		t.Errorf("upstream path = %q", gotPath)
	}
	if gotAuth != "Bearer sk-test" {
		t.Errorf("upstream Authorization = %q", gotAuth)
	}
	if !strings.Contains(string(body), "cmpl-1") {
		t.Errorf("response = %q", body)
	}
}

func TestEmbeddingsProxiesToResolvedEndpoint(t *testing.T) {
	var gotPath, gotBody string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath = r.URL.Path
		b, _ := io.ReadAll(r.Body)
		gotBody = string(b)
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"object":"list","data":[{"embedding":[0.1,0.2]}]}`)
	}))
	defer upstream.Close()

	d := newTestDaemon(NewRouter(stubSession{base: upstream.URL, token: "sk-test"}))
	defer d.Close()

	req := `{"model":"cloud-x","input":"hi"}`
	resp, err := http.Post(d.URL+"/v1/embeddings", "application/json", strings.NewReader(req))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)

	if gotPath != "/v1/embeddings" {
		t.Errorf("upstream path = %q", gotPath)
	}
	if gotBody != req {
		t.Errorf("upstream body = %q", gotBody)
	}
	if !strings.Contains(string(body), "embedding") {
		t.Errorf("response = %q", body)
	}
}

func TestCompletionsMissingModel(t *testing.T) {
	d := newTestDaemon(NewRouter(stubSession{base: "https://x", token: "y"}))
	defer d.Close()

	resp, err := http.Post(d.URL+"/v1/completions", "application/json", strings.NewReader(`{}`))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusBadRequest {
		t.Errorf("status = %d, want 400", resp.StatusCode)
	}
}
