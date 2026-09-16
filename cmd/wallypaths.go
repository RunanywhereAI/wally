package cmd

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"

	"github.com/RunanywhereAI/wally/credstore"
	"github.com/RunanywhereAI/wally/runanywhere"
)

// chatsDir, prefsFilePath, and harnessStateDir all live under credstore's own
// profile directory rather than a second wally root: that is the one
// directory WALLY_PROFILE_DIR already isolates per account, so a second
// account on the same machine gets its own chats and harness state along
// with its own credentials.
func chatsDir() (string, error) {
	root, err := credstore.ProfileDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(root, "chats"), nil
}

// prefsFilePath is config.Dir()/prefs.json (config/prefs.go), wally's own
// settings file, named here rather than imported: config.Dir() and
// credstore.ProfileDir() compute the same directory, and the coordinator's
// call was to standardize on the credstore export.
const prefsFileName = "prefs.json"

func prefsFilePath() (string, error) {
	root, err := credstore.ProfileDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(root, prefsFileName), nil
}

func harnessStateDir() (string, error) {
	root, err := credstore.ProfileDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(root, "harness"), nil
}

// modelsDir is a separate root from credstore's profile directory: on-device
// models live in the kit's own store (runanywhere.ModelsRoot), not under
// wally's config tree.
func modelsDir() string {
	return runanywhere.ModelsRoot()
}

// dirSize sums the size of every regular file under root. A root that does
// not exist yet is 0 bytes, not an error: nothing has written there.
func dirSize(root string) (int64, error) {
	if root == "" {
		return 0, nil
	}
	var total int64
	err := filepath.WalkDir(root, func(_ string, d fs.DirEntry, err error) error {
		if err != nil {
			if os.IsNotExist(err) {
				return nil
			}
			return err
		}
		if d.IsDir() {
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		total += info.Size()
		return nil
	})
	if err != nil && !os.IsNotExist(err) {
		return 0, err
	}
	return total, nil
}

// dirSizeExcluding sums root the same way as dirSize, but skips any
// immediate entry (file or directory) named in skip. It is how the sign-in
// scope reports a size without knowing credstore's private file names:
// whatever credstore leaves directly under root, once the entries wally's
// other scopes already own are excluded, lands in this total.
func dirSizeExcluding(root string, skip ...string) (int64, error) {
	if root == "" {
		return 0, nil
	}
	skipSet := make(map[string]bool, len(skip))
	for _, name := range skip {
		skipSet[name] = true
	}
	entries, err := os.ReadDir(root)
	if err != nil {
		if os.IsNotExist(err) {
			return 0, nil
		}
		return 0, err
	}
	var total int64
	for _, e := range entries {
		if skipSet[e.Name()] {
			continue
		}
		size, err := dirSize(filepath.Join(root, e.Name()))
		if err != nil {
			return 0, err
		}
		total += size
	}
	return total, nil
}

// removeDir deletes root recursively and reports whether it existed. A path
// that was never created is a no-op, not an error.
func removeDir(root string) (existed bool, err error) {
	if root == "" {
		return false, nil
	}
	if _, err := os.Stat(root); err != nil {
		if os.IsNotExist(err) {
			return false, nil
		}
		return false, err
	}
	return true, os.RemoveAll(root)
}

// formatBytes renders n the way a person reads a file size, not the way a
// log line does.
func formatBytes(n int64) string {
	const unit = 1024
	if n < unit {
		return fmt.Sprintf("%d B", n)
	}
	div, exp := int64(unit), 0
	for v := n / unit; v >= unit; v /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f %ciB", float64(n)/float64(div), "KMGTPE"[exp])
}
