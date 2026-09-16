package catalog

import (
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"time"

	"github.com/RunanywhereAI/wally/config"
)

type Model struct {
	ID            string `json:"id"`
	InputPerMTok  int64  `json:"input_per_mtok"`
	OutputPerMTok int64  `json:"output_per_mtok"`
	// ContextWindow and MaxOutputTokens are the account's provisioned limits
	// for this model (console.ModelInfo), cached so a harness can read the
	// real context window with no live call. A cache file written before
	// these fields existed loads both as zero, which callers treat as
	// "unknown" rather than a fake limit.
	ContextWindow   int64 `json:"context_window"`
	MaxOutputTokens int64 `json:"max_output_tokens"`
}

type cacheFile struct {
	UpdatedAt time.Time `json:"updated_at"`
	Models    []Model   `json:"models"`
}

func cachePath() (string, error) {
	dir, err := config.Dir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, "catalog.json"), nil
}

// Load reads the cached cloud catalog. A missing or unreadable cache returns an
// empty list and no error, so a caller can treat "nothing cached" as "cannot
// verify" without special-casing.
func Load() ([]Model, error) {
	p, err := cachePath()
	if err != nil {
		return nil, err
	}
	data, err := os.ReadFile(p)
	if errors.Is(err, fs.ErrNotExist) {
		return []Model{}, nil
	}
	if err != nil {
		return nil, err
	}
	var cf cacheFile
	if err := json.Unmarshal(data, &cf); err != nil {
		return []Model{}, nil
	}
	return cf.Models, nil
}

func Save(models []Model) error {
	p, err := cachePath()
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(cacheFile{UpdatedAt: time.Now(), Models: models}, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(p, data, 0o600)
}

func Contains(models []Model, id string) bool {
	for _, m := range models {
		if m.ID == id {
			return true
		}
	}
	return false
}
