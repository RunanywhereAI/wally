package cmd

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/catalog"
	"github.com/RunanywhereAI/wally/errmap"
)

// seedModel writes a fake installed model under home's on-device store, the
// same layout ModelsRoot resolves: {home}/RunAnywhere/Models/<framework>/<id>.
func seedModel(t *testing.T, home, framework, id string, size int) string {
	t.Helper()
	dir := filepath.Join(home, "RunAnywhere", "Models", framework, id)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "model.gguf"), make([]byte, size), 0o644); err != nil {
		t.Fatal(err)
	}
	return dir
}

func runModelsCmd(t *testing.T, stdin string, args ...string) (string, error) {
	t.Helper()
	// Isolate the catalog cache path (config.Dir() mirrors WALLY_PROFILE_DIR)
	// so a real cache file on the machine running the test is never read.
	t.Setenv("WALLY_PROFILE_DIR", t.TempDir())
	var out bytes.Buffer
	cmd := newModelsCmd()
	cmd.SetArgs(args)
	cmd.SetOut(&out)
	cmd.SetErr(&out)
	cmd.SetIn(strings.NewReader(stdin))
	err := cmd.Execute()
	return out.String(), err
}

func TestModelsListShowsInstalled(t *testing.T) {
	home := t.TempDir()
	t.Setenv("RUNANYWHERE_HOME", home)
	seedModel(t, home, "llama-cpp", "qwen2.5-7b-instruct", 2048)

	out, err := runModelsCmd(t, "", "list")
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"qwen2.5-7b-instruct", "llama-cpp", "2.0 KiB", "yes"} {
		if !strings.Contains(out, want) {
			t.Errorf("list output missing %q:\n%s", want, out)
		}
	}
}

func TestModelsListEmpty(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())

	out, err := runModelsCmd(t, "", "list")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, "models pull") {
		t.Errorf("empty list output should point at pull: %q", out)
	}
}

func TestModelsListEmptyCatalogPointsAtLogin(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())

	out, err := runModelsCmd(t, "", "list")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, "ONLINE (cloud)") || !strings.Contains(out, "wally login") {
		t.Errorf("list output should point an uncached catalog at wally login: %q", out)
	}
}

func TestModelsListShowsOnlineCatalog(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
	orig := catalogLoad
	catalogLoad = func() ([]catalog.Model, error) {
		return []catalog.Model{{ID: "gpt-oss-20b", InputPerMTok: 100000, OutputPerMTok: 400000}}, nil
	}
	t.Cleanup(func() { catalogLoad = orig })

	out, err := runModelsCmd(t, "", "list")
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"ONLINE (cloud)", "gpt-oss-20b", "0.10", "0.40"} {
		if !strings.Contains(out, want) {
			t.Errorf("list output missing %q:\n%s", want, out)
		}
	}
}

func TestModelsShowFound(t *testing.T) {
	home := t.TempDir()
	t.Setenv("RUNANYWHERE_HOME", home)
	dir := seedModel(t, home, "onnx", "phi-3-mini", 512)

	out, err := runModelsCmd(t, "", "show", "phi-3-mini")
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"phi-3-mini", "onnx", dir, "512 B", "no"} {
		if !strings.Contains(out, want) {
			t.Errorf("show output missing %q:\n%s", want, out)
		}
	}
}

func TestModelsShowNotFound(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())

	_, err := runModelsCmd(t, "", "show", "does-not-exist")
	var mapped *errmap.Error
	if !errors.As(err, &mapped) || mapped.Kind() != errmap.KindModelNotInstalled {
		t.Fatalf("err = %v, want errmap.KindModelNotInstalled", err)
	}
}

func TestModelsRmNotFound(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())

	_, err := runModelsCmd(t, "", "rm", "does-not-exist", "--yes")
	var mapped *errmap.Error
	if !errors.As(err, &mapped) || mapped.Kind() != errmap.KindModelNotInstalled {
		t.Fatalf("err = %v, want errmap.KindModelNotInstalled", err)
	}
}

func TestModelsRmWithYesDeletes(t *testing.T) {
	home := t.TempDir()
	t.Setenv("RUNANYWHERE_HOME", home)
	dir := seedModel(t, home, "llama-cpp", "gemma-3-4b", 4096)

	out, err := runModelsCmd(t, "", "rm", "gemma-3-4b", "--yes")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, "4.0 KiB") {
		t.Errorf("rm output should report freed space: %q", out)
	}
	if _, err := os.Stat(dir); !os.IsNotExist(err) {
		t.Errorf("model directory still exists after rm --yes: %v", err)
	}
	if _, ok := findInstalledModel("gemma-3-4b"); ok {
		t.Error("gemma-3-4b still reported as installed after rm")
	}
}

func TestModelsRmWithoutConfirmationKeepsModel(t *testing.T) {
	home := t.TempDir()
	t.Setenv("RUNANYWHERE_HOME", home)
	dir := seedModel(t, home, "llama-cpp", "gemma-3-4b", 4096)

	out, err := runModelsCmd(t, "n\n", "rm", "gemma-3-4b")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, "Nothing removed") {
		t.Errorf("declined rm should say nothing removed: %q", out)
	}
	if _, err := os.Stat(dir); err != nil {
		t.Errorf("model directory removed despite declined confirmation: %v", err)
	}
}

func TestModelsPullRunsConfiguredScript(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("the pull-model.sh seam is a unix shell script; isExecutableFile keys off the +x bit")
	}
	home := t.TempDir()
	t.Setenv("RUNANYWHERE_HOME", home)

	script := filepath.Join(t.TempDir(), "pull-model.sh")
	body := "#!/bin/sh\necho \"pulled $1 into $2\"\n"
	if err := os.WriteFile(script, []byte(body), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("WALLY_PULL_SCRIPT", script)

	out, err := runModelsCmd(t, "", "pull", "qwen2.5-7b-instruct")
	if err != nil {
		t.Fatalf("pull failed: %v\n%s", err, out)
	}
	if !strings.Contains(out, "pulled qwen2.5-7b-instruct into") {
		t.Errorf("pull did not run the configured script: %q", out)
	}
}

func TestModelsPullWithoutScriptIsHonest(t *testing.T) {
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())

	_, err := runModelsCmd(t, "", "pull", "qwen2.5-7b-instruct")
	if !errors.Is(err, ErrModelPullNotWired) {
		t.Fatalf("err = %v, want ErrModelPullNotWired", err)
	}
}
