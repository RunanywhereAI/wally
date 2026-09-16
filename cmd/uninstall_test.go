package cmd

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/RunanywhereAI/wally/credstore"
	"github.com/RunanywhereAI/wally/tui"
)

// seedUninstallEnv points every scope at temp directories: WALLY_PROFILE_DIR
// for credstore/chats/config/harness, RUNANYWHERE_HOME for the models store.
// Returns the credstore Store built against the same profile dir.
func seedUninstallEnv(t *testing.T) *credstore.Store {
	t.Helper()
	store := newTestStore(t)
	t.Setenv("RUNANYWHERE_HOME", t.TempDir())
	return store
}

func writeFile(t *testing.T, path string, size int) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, bytes.Repeat([]byte("x"), size), 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestBuildUninstallScopes_SixScopes(t *testing.T) {
	seedUninstallEnv(t)
	scopes, err := buildUninstallScopes()
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"Models", "Chats", "Configs", "Harness integration state", "All data", "Sign-in / credentials"}
	if len(scopes) != len(want) {
		t.Fatalf("got %d scopes, want %d", len(scopes), len(want))
	}
	for i, title := range want {
		if scopes[i].title != title {
			t.Errorf("scope %d = %q, want %q", i, scopes[i].title, title)
		}
	}
}

func TestUninstallScope_SizeReflectsSeededFiles(t *testing.T) {
	seedUninstallEnv(t)
	chats, err := chatsDir()
	if err != nil {
		t.Fatal(err)
	}
	writeFile(t, filepath.Join(chats, "one.json"), 128)

	scopes, err := buildUninstallScopes()
	if err != nil {
		t.Fatal(err)
	}
	size, err := scopes[1].size() // Chats
	if err != nil {
		t.Fatal(err)
	}
	if size != 128 {
		t.Errorf("chats size = %d, want 128", size)
	}
}

func TestUninstallScope_EmptyScopeIsZeroNotError(t *testing.T) {
	seedUninstallEnv(t)
	scopes, err := buildUninstallScopes()
	if err != nil {
		t.Fatal(err)
	}
	for _, s := range scopes {
		if s.kind == scopeCredentials {
			continue
		}
		size, err := s.size()
		if err != nil {
			t.Errorf("%s: size() error = %v, want nil for a directory nothing wrote to", s.title, err)
		}
		if size != 0 {
			t.Errorf("%s: size = %d, want 0 before anything is seeded", s.title, size)
		}
	}
}

func TestUninstallScope_ConfigsScopeIsPrefsFile(t *testing.T) {
	seedUninstallEnv(t)
	prefs, err := prefsFilePath()
	if err != nil {
		t.Fatal(err)
	}
	writeFile(t, prefs, 42)

	scopes, err := buildUninstallScopes()
	if err != nil {
		t.Fatal(err)
	}
	size, err := scopes[2].size() // Configs
	if err != nil {
		t.Fatal(err)
	}
	if size != 42 {
		t.Errorf("configs size = %d, want 42", size)
	}
}

func TestUninstallScope_CredentialsDetail(t *testing.T) {
	store := seedUninstallEnv(t)
	scopes, err := buildUninstallScopes()
	if err != nil {
		t.Fatal(err)
	}
	credScope := scopes[5]
	if got := credScope.detail(); got != "not signed in" {
		t.Errorf("detail() = %q, want %q", got, "not signed in")
	}

	if err := store.Save(credstore.Credentials{AccessToken: "toklive123", Email: "dev@example.com", ExpiresAt: time.Now().Add(time.Hour).Unix()}); err != nil {
		t.Fatal(err)
	}
	if got := credScope.detail(); got != "signed in as dev@example.com" {
		t.Errorf("detail() = %q, want signed-in identity", got)
	}
}

func TestUninstallScope_ConfigsSizeExcludesCredentialFiles(t *testing.T) {
	seedUninstallEnv(t)
	profile, err := credstore.ProfileDir()
	if err != nil {
		t.Fatal(err)
	}
	// Mimic credstore's own fallback file sitting beside prefs.json, without
	// importing its private name.
	writeFile(t, filepath.Join(profile, "credentials.enc"), 64)

	scopes, err := buildUninstallScopes()
	if err != nil {
		t.Fatal(err)
	}
	size, err := scopes[2].size() // Configs, must not count the credential file
	if err != nil {
		t.Fatal(err)
	}
	if size != 0 {
		t.Errorf("configs size = %d, want 0 (credential fallback file is not a config)", size)
	}

	credSize, err := scopes[5].size() // Sign-in, must count it
	if err != nil {
		t.Fatal(err)
	}
	if credSize != 64 {
		t.Errorf("sign-in size = %d, want 64", credSize)
	}
}

// stubSelect returns a scripted response once, then ErrCancelled forever, so
// a test loop terminates without a real terminal.
func stubSelect(indices []int) func(string, []tui.Item, string) ([]int, error) {
	used := false
	return func(string, []tui.Item, string) ([]int, error) {
		if used {
			return nil, tui.ErrCancelled
		}
		used = true
		return indices, nil
	}
}

func TestRunUninstall_NothingDeletedBeforeConfirm(t *testing.T) {
	seedUninstallEnv(t)
	chats, err := chatsDir()
	if err != nil {
		t.Fatal(err)
	}
	marker := filepath.Join(chats, "keep.json")
	writeFile(t, marker, 16)

	var out bytes.Buffer
	confirmCalls := 0
	declineThenAccept := func(string) (bool, error) {
		confirmCalls++
		return false, nil // decline first
	}

	if err := runUninstall(&out, stubSelect([]int{1}), declineThenAccept); err != nil {
		t.Fatal(err)
	}
	if confirmCalls != 1 {
		t.Fatalf("confirm called %d times, want 1", confirmCalls)
	}
	if _, err := os.Stat(marker); err != nil {
		t.Fatalf("file removed before/without an accepted confirm: %v", err)
	}

	out.Reset()
	accept := func(string) (bool, error) { return true, nil }
	if err := runUninstall(&out, stubSelect([]int{1}), accept); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(marker); !os.IsNotExist(err) {
		t.Fatalf("file still present after an accepted confirm: err = %v", err)
	}
}

func TestRunUninstall_CancelledSelectionNeverConfirms(t *testing.T) {
	seedUninstallEnv(t)
	chats, err := chatsDir()
	if err != nil {
		t.Fatal(err)
	}
	marker := filepath.Join(chats, "keep.json")
	writeFile(t, marker, 16)

	var out bytes.Buffer
	confirmCalled := false
	confirm := func(string) (bool, error) {
		confirmCalled = true
		return true, nil
	}
	cancelledSelect := func(string, []tui.Item, string) ([]int, error) {
		return nil, tui.ErrCancelled
	}

	if err := runUninstall(&out, cancelledSelect, confirm); err != nil {
		t.Fatal(err)
	}
	if confirmCalled {
		t.Fatal("confirm was called after the selection was cancelled")
	}
	if _, err := os.Stat(marker); err != nil {
		t.Fatalf("file removed despite a cancelled selection: %v", err)
	}
}

func TestRunUninstall_CancelledConfirmRemovesNothing(t *testing.T) {
	seedUninstallEnv(t)
	chats, err := chatsDir()
	if err != nil {
		t.Fatal(err)
	}
	marker := filepath.Join(chats, "keep.json")
	writeFile(t, marker, 16)

	var out bytes.Buffer
	cancelledConfirm := func(string) (bool, error) { return false, tui.ErrCancelled }
	if err := runUninstall(&out, stubSelect([]int{1}), cancelledConfirm); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(marker); err != nil {
		t.Fatalf("file removed despite a cancelled confirm: %v", err)
	}
}

func TestRunUninstall_SignInScopeSignsOut(t *testing.T) {
	store := seedUninstallEnv(t)
	if err := store.Save(credstore.Credentials{AccessToken: "toklive123", Email: "dev@example.com", ExpiresAt: time.Now().Add(time.Hour).Unix()}); err != nil {
		t.Fatal(err)
	}

	var out bytes.Buffer
	accept := func(string) (bool, error) { return true, nil }
	if err := runUninstall(&out, stubSelect([]int{5}), accept); err != nil { // Sign-in / credentials
		t.Fatal(err)
	}

	creds, err := store.Load()
	if err != nil {
		t.Fatal(err)
	}
	if creds.SignedIn() {
		t.Fatal("still signed in after confirming the sign-in scope")
	}
}

func TestRunUninstall_TargetedScopeNeverSignsOut(t *testing.T) {
	store := seedUninstallEnv(t)
	if err := store.Save(credstore.Credentials{AccessToken: "toklive123", Email: "dev@example.com", ExpiresAt: time.Now().Add(time.Hour).Unix()}); err != nil {
		t.Fatal(err)
	}

	var out bytes.Buffer
	accept := func(string) (bool, error) { return true, nil }
	// "All data" (index 4), not sign-in.
	if err := runUninstall(&out, stubSelect([]int{4}), accept); err != nil {
		t.Fatal(err)
	}

	creds, err := store.Load()
	if err != nil {
		t.Fatal(err)
	}
	if !creds.SignedIn() {
		t.Fatal("a targeted data wipe signed the person out; only the sign-in scope may do that")
	}
}

func TestFormatBytes(t *testing.T) {
	cases := map[int64]string{
		0:       "0 B",
		512:     "512 B",
		1024:    "1.0 KiB",
		1536:    "1.5 KiB",
		1 << 20: "1.0 MiB",
		1 << 30: "1.0 GiB",
	}
	for n, want := range cases {
		if got := formatBytes(n); got != want {
			t.Errorf("formatBytes(%d) = %q, want %q", n, got, want)
		}
	}
}

func TestRunUninstall_ErrorPropagatesFromSelect(t *testing.T) {
	seedUninstallEnv(t)
	boom := errors.New("boom")
	failingSelect := func(string, []tui.Item, string) ([]int, error) { return nil, boom }
	var out bytes.Buffer
	err := runUninstall(&out, failingSelect, func(string) (bool, error) { return true, nil })
	if !errors.Is(err, boom) {
		t.Errorf("err = %v, want boom", err)
	}
}
