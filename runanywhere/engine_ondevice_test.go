//go:build ondevice

package runanywhere

import (
	"strings"
	"testing"
)

// Start's extension guard runs before any call into the kit, so this proves
// the rejection path without touching rac_server. The rest of the engine
// (rac_server_start/_stop) is only provable against a running kit; that
// proof is scripts/build-ondevice.sh plus a live daemon request, not a unit
// test.
func TestStart_RejectsNonGGUF(t *testing.T) {
	e := &racServerEngine{}
	_, err := e.Start(InstalledModel{
		ID:        "lfm2.5-1.2b-instruct-mlx-4bit",
		Framework: "MLX",
		Path:      "/models/MLX/lfm2.5-1.2b-instruct-mlx-4bit/model.safetensors",
	})
	if err == nil {
		t.Fatal("expected an error for a non-GGUF model, got nil")
	}
	if !strings.Contains(err.Error(), "only GGUF/llama.cpp") {
		t.Fatalf("error %q does not name the GGUF/llama.cpp restriction", err.Error())
	}
}
