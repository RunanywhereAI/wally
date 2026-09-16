package runanywhere

import (
	"errors"
	"os"
	"path/filepath"
	"testing"
)

type stubSession struct {
	base  string
	token string
	err   error
}

func (s stubSession) ConsoleBaseURL() string { return s.base }
func (s stubSession) Token() (string, error) { return s.token, s.err }

type fakeEngine struct {
	started InstalledModel
	ep      Endpoint
}

func (f *fakeEngine) Start(m InstalledModel) (Endpoint, error) {
	f.started = m
	return f.ep, nil
}
func (f *fakeEngine) Stop() error { return nil }

// installModel seeds a discoverable on-device model and points RUNANYWHERE_HOME
// at it, so InstalledModels finds exactly this one.
func installModel(t *testing.T, framework, id string) {
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

func TestResolveCloud(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
	ep, err := Resolve("gpt-cloud", stubSession{base: "https://console.example/api", token: "sk-live"})
	if err != nil {
		t.Fatalf("Resolve: %v", err)
	}
	if ep.BaseURL != "https://console.example/api/v1" || ep.APIKey != "sk-live" || ep.Local {
		t.Errorf("endpoint = %+v", ep)
	}
}

func TestResolveNotSignedIn(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
	if _, err := Resolve("gpt-cloud", nil); !errors.Is(err, ErrNotSignedIn) {
		t.Errorf("err = %v, want ErrNotSignedIn", err)
	}
}

func TestResolveTokenError(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
	want := errors.New("keychain locked")
	if _, err := Resolve("gpt-cloud", stubSession{err: want}); !errors.Is(err, want) {
		t.Errorf("err = %v, want %v", err, want)
	}
}

func TestResolveLocalModelGatedWithoutEngine(t *testing.T) {
	installModel(t, "LlamaCpp", "qwen2.5-3b")
	engine = nil
	if _, err := Resolve("qwen2.5-3b", stubSession{base: "https://x", token: "y"}); !errors.Is(err, ErrOnDeviceNotEnabled) {
		t.Errorf("err = %v, want ErrOnDeviceNotEnabled", err)
	}
}

func TestResolveLocalModelUsesEngine(t *testing.T) {
	installModel(t, "LlamaCpp", "qwen2.5-3b")
	fe := &fakeEngine{ep: Endpoint{BaseURL: "http://127.0.0.1:8081/v1", Local: true}}
	SetEngine(fe)
	t.Cleanup(func() { engine = nil })

	ep, err := Resolve("qwen2.5-3b", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !ep.Local || ep.BaseURL != "http://127.0.0.1:8081/v1" {
		t.Errorf("endpoint = %+v", ep)
	}
	if fe.started.ID != "qwen2.5-3b" || fe.started.Path == "" {
		t.Errorf("engine started with %+v", fe.started)
	}
}

func TestInstalledModelsDiscovery(t *testing.T) {
	installModel(t, "LlamaCpp", "qwen2.5-3b")
	models := InstalledModels()
	if len(models) != 1 || models[0].ID != "qwen2.5-3b" || models[0].Framework != "LlamaCpp" {
		t.Fatalf("models = %+v", models)
	}
	if filepath.Base(models[0].Path) != "model.gguf" {
		t.Errorf("path = %q", models[0].Path)
	}
}

func TestFirstToolCallingLocalModel(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
	if _, ok := FirstToolCallingLocalModel(); ok {
		t.Error("expected none when nothing is installed")
	}
	installModel(t, "LlamaCpp", "qwen2.5-3b")
	m, ok := FirstToolCallingLocalModel()
	if !ok || m.ID != "qwen2.5-3b" {
		t.Errorf("got %+v ok=%v", m, ok)
	}
}
