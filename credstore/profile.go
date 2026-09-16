package credstore

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
)

const envProfileDirOverride = "WALLY_PROFILE_DIR"

// ProfileDir is where the credential store lives: WALLY_PROFILE_DIR when set,
// otherwise an OS-appropriate config directory under the user's home. This is
// also what lets several accounts share one machine, by pointing each at a
// different directory.
func ProfileDir() (string, error) {
	if v := strings.TrimSpace(os.Getenv(envProfileDirOverride)); v != "" {
		return v, nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("credstore: no user profile directory is available: %w", err)
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

// ensureSecureDir creates dir if needed and rejects it if it is a symlink,
// the same check the legacy store makes: following a symlink here would let
// something else redirect where the session gets written.
func ensureSecureDir(dir string) error {
	info, err := os.Lstat(dir)
	switch {
	case err == nil:
		if info.Mode()&os.ModeSymlink != 0 {
			return errors.New("credstore: credential directory may not be a symbolic link")
		}
	case !os.IsNotExist(err):
		return fmt.Errorf("credstore: could not inspect the credential directory: %w", err)
	}
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return fmt.Errorf("credstore: could not create the credential directory: %w", err)
	}
	if runtime.GOOS != "windows" {
		if err := os.Chmod(dir, 0o700); err != nil {
			return fmt.Errorf("credstore: could not restrict the credential directory: %w", err)
		}
	}
	return nil
}
