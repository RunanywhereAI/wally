package catalog

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestSaveLoadRoundTripsContextWindow(t *testing.T) {
	t.Setenv("WALLY_PROFILE_DIR", t.TempDir())

	want := []Model{
		{ID: "qwen3-coder", InputPerMTok: 300, OutputPerMTok: 1200, ContextWindow: 262_144, MaxOutputTokens: 32_768},
		{ID: "kimi-k2.6", InputPerMTok: 500, OutputPerMTok: 2000},
	}
	if err := Save(want); err != nil {
		t.Fatalf("Save: %v", err)
	}

	got, err := Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if len(got) != len(want) {
		t.Fatalf("Load returned %d models, want %d", len(got), len(want))
	}
	for i := range want {
		if got[i] != want[i] {
			t.Errorf("model %d = %+v, want %+v", i, got[i], want[i])
		}
	}
}

func TestLoadOldCacheWithoutContextWindowDefaultsToZero(t *testing.T) {
	t.Setenv("WALLY_PROFILE_DIR", t.TempDir())

	// Simulate a cache file written before ContextWindow/MaxOutputTokens
	// existed: only id and price fields present.
	type oldModel struct {
		ID            string `json:"id"`
		InputPerMTok  int64  `json:"input_per_mtok"`
		OutputPerMTok int64  `json:"output_per_mtok"`
	}
	type oldCacheFile struct {
		UpdatedAt time.Time  `json:"updated_at"`
		Models    []oldModel `json:"models"`
	}
	p, err := cachePath()
	if err != nil {
		t.Fatalf("cachePath: %v", err)
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o700); err != nil {
		t.Fatalf("mkdir cache dir: %v", err)
	}
	data, err := json.Marshal(oldCacheFile{UpdatedAt: time.Now(), Models: []oldModel{{ID: "qwen3-coder", InputPerMTok: 300, OutputPerMTok: 1200}}})
	if err != nil {
		t.Fatalf("marshal old cache: %v", err)
	}
	if err := os.WriteFile(p, data, 0o600); err != nil {
		t.Fatalf("write old cache: %v", err)
	}

	got, err := Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if len(got) != 1 {
		t.Fatalf("Load returned %d models, want 1", len(got))
	}
	if got[0].ContextWindow != 0 || got[0].MaxOutputTokens != 0 {
		t.Errorf("old cache entry = %+v, want ContextWindow and MaxOutputTokens both 0", got[0])
	}
	if got[0].ID != "qwen3-coder" || got[0].InputPerMTok != 300 {
		t.Errorf("old cache entry = %+v, want id/price preserved", got[0])
	}
}
