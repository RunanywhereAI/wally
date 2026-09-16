package harness

import (
	"os"
	"path/filepath"
)

// effectiveArgs returns args unchanged when the person passed any of their
// own; theirs wins whole rather than mixing with ours. Empty falls back to
// defaultArgs, the minimum invocation this harness needs to reach a usable
// surface without any other service running.
func effectiveArgs(defaultArgs, args []string) []string {
	if len(args) > 0 {
		return args
	}
	return defaultArgs
}

// writeTempFile creates a private (mode 0600, via os.CreateTemp) file holding
// contents and returns its path. pattern must contain exactly one "*"; every
// harness uses a distinct pattern prefix so its Restore only ever removes its
// own litter, never another harness's.
func writeTempFile(pattern, contents string) (string, error) {
	f, err := os.CreateTemp("", pattern)
	if err != nil {
		return "", err
	}
	defer f.Close()
	if _, err := f.WriteString(contents); err != nil {
		name := f.Name()
		os.Remove(name)
		return "", err
	}
	return f.Name(), nil
}

// removeGlob deletes every file matching pattern in the OS temp directory,
// tolerating ones already gone. Used by Restore to sweep up a temp config a
// crashed launch never got to clean up itself.
func removeGlob(pattern string) error {
	matches, err := filepath.Glob(filepath.Join(os.TempDir(), pattern))
	if err != nil {
		return err
	}
	for _, m := range matches {
		if err := os.Remove(m); err != nil && !os.IsNotExist(err) {
			return err
		}
	}
	return nil
}

func containsString(list []string, s string) bool {
	for _, v := range list {
		if v == s {
			return true
		}
	}
	return false
}
