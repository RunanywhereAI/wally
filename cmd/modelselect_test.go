package cmd

import (
	"errors"
	"io"
	"os"
	"path/filepath"
	"testing"

	"github.com/RunanywhereAI/wally/config"
	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/runanywhere"
)

type noopEngine struct{}

func (noopEngine) Start(runanywhere.InstalledModel) (runanywhere.Endpoint, error) {
	return runanywhere.Endpoint{}, nil
}
func (noopEngine) Stop() error { return nil }

func enableOnDevice(t *testing.T) {
	t.Helper()
	runanywhere.SetEngine(noopEngine{})
	t.Cleanup(func() { runanywhere.SetEngine(nil) })
}

type scriptedPrompter struct {
	answers []bool
	i       int
	asks    []string
	j       int
}

func (p *scriptedPrompter) Confirm(string) (bool, error) {
	a := p.answers[p.i]
	p.i++
	return a, nil
}

func (p *scriptedPrompter) Ask(string) (string, error) {
	if p.j < len(p.asks) {
		s := p.asks[p.j]
		p.j++
		return s, nil
	}
	return "", nil
}

func isolatePrefs(t *testing.T) {
	t.Helper()
	t.Setenv("WALLY_PROFILE_DIR", t.TempDir())
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
}

func installLocalModel(t *testing.T, framework, id string) {
	t.Helper()
	home := os.Getenv("RUNANYWHERE_HOME")
	dir := filepath.Join(home, "RunAnywhere", "Models", framework, id)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "model.gguf"), []byte("w"), 0o644); err != nil {
		t.Fatal(err)
	}
}

func wantKind(t *testing.T, err error, kind errmap.Kind) {
	t.Helper()
	var e *errmap.Error
	if !errors.As(err, &e) || e.Kind() != kind {
		t.Fatalf("err = %v, want kind %d", err, kind)
	}
}

func TestSelectModelExplicit(t *testing.T) {
	isolatePrefs(t)
	got, err := selectModel("gpt-x", true, &scriptedPrompter{}, io.Discard, false)
	if err != nil || got != "gpt-x" {
		t.Fatalf("got %q err %v", got, err)
	}
}

func TestSelectModelDefault(t *testing.T) {
	isolatePrefs(t)
	if err := config.SavePrefs(config.Prefs{DefaultModel: "team-default"}); err != nil {
		t.Fatal(err)
	}
	got, err := selectModel("", false, &scriptedPrompter{}, io.Discard, false)
	if err != nil || got != "team-default" {
		t.Fatalf("got %q err %v", got, err)
	}
}

func TestSelectModelSignedInNoModelPromptsBlank(t *testing.T) {
	isolatePrefs(t)
	_, err := selectModel("", true, &scriptedPrompter{}, io.Discard, false)
	wantKind(t, err, errmap.KindNoModelSpecified)
}

func TestSelectModelSignedInPromptsForModel(t *testing.T) {
	isolatePrefs(t)
	got, err := selectModel("", true, &scriptedPrompter{asks: []string{"picked-model"}}, io.Discard, false)
	if err != nil || got != "picked-model" {
		t.Fatalf("got %q err %v", got, err)
	}
}

func TestSelectModelNotSignedInNoLocal(t *testing.T) {
	isolatePrefs(t)
	_, err := selectModel("", false, &scriptedPrompter{}, io.Discard, false)
	wantKind(t, err, errmap.KindNotSignedIn)
}

func TestSelectModelNotSignedInAcceptsLocalAndRemembers(t *testing.T) {
	isolatePrefs(t)
	enableOnDevice(t)
	installLocalModel(t, "LlamaCpp", "qwen2.5-3b")
	// yes to on-device, no to "show again"
	got, err := selectModel("", false, &scriptedPrompter{answers: []bool{true, false}}, io.Discard, false)
	if err != nil || got != "qwen2.5-3b" {
		t.Fatalf("got %q err %v", got, err)
	}
	prefs, _ := config.LoadPrefs()
	if !prefs.AllowOnDeviceNoLogin || !prefs.SkipOnDeviceNoLoginAsk {
		t.Errorf("prefs not persisted: %+v", prefs)
	}
}

func TestSelectModelNotSignedInDeclinesLocal(t *testing.T) {
	isolatePrefs(t)
	enableOnDevice(t)
	installLocalModel(t, "LlamaCpp", "qwen2.5-3b")
	_, err := selectModel("", false, &scriptedPrompter{answers: []bool{false}}, io.Discard, false)
	wantKind(t, err, errmap.KindNotSignedIn)
}

func TestSelectModelLocalModelButOnDeviceDisabled(t *testing.T) {
	isolatePrefs(t)
	installLocalModel(t, "LlamaCpp", "qwen2.5-3b")
	// engine not enabled: an on-device model on disk must not be offered
	_, err := selectModel("", false, &scriptedPrompter{answers: []bool{true}}, io.Discard, false)
	wantKind(t, err, errmap.KindNotSignedIn)
}

func TestSelectModelRemembersAllow(t *testing.T) {
	isolatePrefs(t)
	enableOnDevice(t)
	installLocalModel(t, "LlamaCpp", "qwen2.5-3b")
	if err := config.SavePrefs(config.Prefs{AllowOnDeviceNoLogin: true, SkipOnDeviceNoLoginAsk: true}); err != nil {
		t.Fatal(err)
	}
	got, err := selectModel("", false, &scriptedPrompter{}, io.Discard, false)
	if err != nil || got != "qwen2.5-3b" {
		t.Fatalf("got %q err %v", got, err)
	}
}
