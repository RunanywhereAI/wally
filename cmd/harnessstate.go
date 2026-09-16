package cmd

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// harnessState is wally's own record for one harness, distinct from the
// harness's own config (which wally never touches outside a launch). Right
// now that's just the default model set through `wally harness`.
type harnessState struct {
	DefaultModel string `json:"default_model,omitempty"`
}

func harnessStateFile(name string) (string, error) {
	dir, err := harnessStateDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, name+".json"), nil
}

// loadDefaultModel returns the stored default model for a harness, or "" if
// nothing has been set.
func loadDefaultModel(name string) (string, error) {
	path, err := harnessStateFile(name)
	if err != nil {
		return "", err
	}
	data, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return "", nil
		}
		return "", err
	}
	var state harnessState
	if err := json.Unmarshal(data, &state); err != nil {
		return "", fmt.Errorf("harness state for %s is corrupted: %w", name, err)
	}
	return state.DefaultModel, nil
}

// saveDefaultModel stores model as name's default, overwriting any prior
// value.
func saveDefaultModel(name, model string) error {
	path, err := harnessStateFile(name)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(harnessState{DefaultModel: model}, "", "  ")
	if err != nil {
		return err
	}
	data = append(data, '\n')
	return os.WriteFile(path, data, 0o600)
}

// clearHarnessState removes name's stored state and reports whether
// anything existed to remove.
func clearHarnessState(name string) (existed bool, err error) {
	path, err := harnessStateFile(name)
	if err != nil {
		return false, err
	}
	if _, statErr := os.Stat(path); statErr != nil {
		if os.IsNotExist(statErr) {
			return false, nil
		}
		return false, statErr
	}
	return true, os.Remove(path)
}
