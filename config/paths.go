package config

import (
	"os"
	"path/filepath"
	"runtime"
	"strings"
)

// Dir is wally's config root. It mirrors credstore.ProfileDir so preferences sit
// beside the credential store: WALLY_PROFILE_DIR when set, else XDG_CONFIG_HOME
// or ~/.config on unix, and RunAnywhere/Wally under LOCALAPPDATA on Windows.
func Dir() (string, error) {
	if v := strings.TrimSpace(os.Getenv("WALLY_PROFILE_DIR")); v != "" {
		return v, nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	if runtime.GOOS == "windows" {
		if local := os.Getenv("LOCALAPPDATA"); local != "" {
			return filepath.Join(local, "RunAnywhere", "Wally"), nil
		}
		return filepath.Join(home, "RunAnywhere", "Wally"), nil
	}
	if xdg := os.Getenv("XDG_CONFIG_HOME"); xdg != "" {
		return filepath.Join(xdg, "wally"), nil
	}
	return filepath.Join(home, ".config", "wally"), nil
}
