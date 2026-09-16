package cmd

import (
	"errors"
	"os"
	"path/filepath"
	"testing"

	"github.com/RunanywhereAI/wally/catalog"
	"github.com/RunanywhereAI/wally/errmap"
)

// withCatalog substitutes catalogLoad for the duration of the test.
func withCatalog(t *testing.T, models []catalog.Model, err error) {
	t.Helper()
	orig := catalogLoad
	catalogLoad = func() ([]catalog.Model, error) { return models, err }
	t.Cleanup(func() { catalogLoad = orig })
}

func installLocalModelFor(t *testing.T, id string) {
	t.Helper()
	home := t.TempDir()
	t.Setenv("RUNANYWHERE_HOME", home)
	dir := filepath.Join(home, "RunAnywhere", "Models", "llama-cpp", id)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "model.gguf"), []byte("w"), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestValidateModel_LocalModelSkipsFetchEntirely(t *testing.T) {
	enableOnDevice(t)
	installLocalModelFor(t, "qwen2.5-3b")
	fetched := false
	orig := catalogLoad
	catalogLoad = func() ([]catalog.Model, error) {
		fetched = true
		return nil, nil
	}
	t.Cleanup(func() { catalogLoad = orig })

	if err := validateModel("qwen2.5-3b"); err != nil {
		t.Fatalf("validateModel: %v", err)
	}
	if fetched {
		t.Error("a local model must not trigger a catalog load at all")
	}
}

func TestValidateModel_KnownCloudModelIsOK(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
	withCatalog(t, []catalog.Model{{ID: "gpt-oss-20b"}}, nil)
	if err := validateModel("gpt-oss-20b"); err != nil {
		t.Fatalf("validateModel: %v", err)
	}
}

func TestValidateModel_UnknownCloudModelIsRejected(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
	withCatalog(t, []catalog.Model{{ID: "gpt-oss-20b"}}, nil)
	err := validateModel("not-a-real-model")
	var mapped *errmap.Error
	if !errors.As(err, &mapped) || mapped.Kind() != errmap.KindModelNotAvailable {
		t.Fatalf("err = %v, want errmap.KindModelNotAvailable", err)
	}
}

func TestValidateModel_EmptyCacheProceeds(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
	withCatalog(t, nil, nil)
	if err := validateModel("anything-at-all"); err != nil {
		t.Fatalf("validateModel: %v, want an empty cache to skip validation rather than block", err)
	}
}

func TestValidateModel_FetchErrorProceeds(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
	withCatalog(t, nil, errors.New("cache file is corrupted"))
	if err := validateModel("anything-at-all"); err != nil {
		t.Fatalf("validateModel: %v, want a broken/unreadable cache to skip validation rather than block a launch", err)
	}
}
