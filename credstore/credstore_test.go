package credstore

import (
	"bytes"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/zalando/go-keyring"
)

func TestCredentials_AccessTokenExpired(t *testing.T) {
	now := time.Unix(1_000_000, 0)

	cases := []struct {
		name string
		c    Credentials
		want bool
	}{
		{"no expiry set", Credentials{ExpiresAt: 0}, false},
		{"well in the future", Credentials{ExpiresAt: now.Add(time.Hour).Unix()}, false},
		{"inside the skew window", Credentials{ExpiresAt: now.Add(30 * time.Second).Unix()}, true},
		{"already past", Credentials{ExpiresAt: now.Add(-time.Minute).Unix()}, true},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := tc.c.AccessTokenExpired(now, DefaultExpirySkew); got != tc.want {
				t.Fatalf("AccessTokenExpired() = %v, want %v", got, tc.want)
			}
		})
	}
}

func TestProfileDir_EnvOverride(t *testing.T) {
	dir := t.TempDir()
	t.Setenv(envProfileDirOverride, dir)
	got, err := ProfileDir()
	if err != nil {
		t.Fatalf("ProfileDir() error = %v", err)
	}
	if got != dir {
		t.Fatalf("ProfileDir() = %q, want %q", got, dir)
	}
}

func testCredentials() Credentials {
	return Credentials{
		ConsoleURL:   "https://inference.runanywhere.ai",
		Email:        "person@example.com",
		AccessToken:  "at-1234567890abcdef",
		RefreshToken: "rt-1234567890abcdef",
		ExpiresAt:    time.Now().Add(time.Hour).Unix(),
	}
}

// TestStore_NativeRoundTrip exercises the OS-keystore path against
// go-keyring's in-memory mock, never the real machine keychain: MockInit
// swaps the package-level provider go-keyring's Get/Set/Delete call, so this
// is hermetic regardless of what keystore CI actually has.
func TestStore_NativeRoundTrip(t *testing.T) {
	keyring.MockInit()
	dir := t.TempDir()
	t.Setenv(envProfileDirOverride, dir)

	store, err := New()
	if err != nil {
		t.Fatalf("New() error = %v", err)
	}
	want := testCredentials()
	if err := store.Save(want); err != nil {
		t.Fatalf("Save() error = %v", err)
	}

	if _, err := os.Stat(filepath.Join(dir, credentialsFileName)); !os.IsNotExist(err) {
		t.Fatalf("expected no fallback file when the native store succeeded, stat err = %v", err)
	}

	got, err := store.Load()
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	if got != want {
		t.Fatalf("Load() = %+v, want %+v", got, want)
	}

	if err := store.Clear(); err != nil {
		t.Fatalf("Clear() error = %v", err)
	}
	after, err := store.Load()
	if err != nil {
		t.Fatalf("Load() after Clear() error = %v", err)
	}
	if after.SignedIn() {
		t.Fatalf("expected no session after Clear(), got %+v", after)
	}
}

// TestStore_FileFallbackRoundTrip forces the native store to fail, the way it
// does on a Linux CI runner with no Secret Service running, and proves the
// fallback file is genuinely encrypted rather than merely permission
// restricted.
func TestStore_FileFallbackRoundTrip(t *testing.T) {
	keyring.MockInitWithError(keyring.ErrUnsupportedPlatform)
	dir := t.TempDir()
	t.Setenv(envProfileDirOverride, dir)

	store, err := New()
	if err != nil {
		t.Fatalf("New() error = %v", err)
	}
	want := testCredentials()
	if err := store.Save(want); err != nil {
		t.Fatalf("Save() error = %v", err)
	}

	credPath := filepath.Join(dir, credentialsFileName)
	keyPath := filepath.Join(dir, keyFileName)
	for _, p := range []string{credPath, keyPath} {
		info, err := os.Stat(p)
		if err != nil {
			t.Fatalf("expected %s to exist: %v", p, err)
		}
		if perm := info.Mode().Perm(); perm != 0o600 {
			t.Fatalf("%s has permissions %v, want 0600", p, perm)
		}
	}

	raw, err := os.ReadFile(credPath)
	if err != nil {
		t.Fatalf("reading fallback file: %v", err)
	}
	if bytes.Contains(raw, []byte(want.AccessToken)) {
		t.Fatal("fallback file contains the access token in plaintext")
	}
	if bytes.Contains(raw, []byte(want.RefreshToken)) {
		t.Fatal("fallback file contains the refresh token in plaintext")
	}

	got, err := store.Load()
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	if got != want {
		t.Fatalf("Load() = %+v, want %+v", got, want)
	}

	if err := store.Clear(); err != nil {
		t.Fatalf("Clear() error = %v", err)
	}
	if _, err := os.Stat(credPath); !os.IsNotExist(err) {
		t.Fatalf("expected the fallback file to be removed, stat err = %v", err)
	}
	after, err := store.Load()
	if err != nil {
		t.Fatalf("Load() after Clear() error = %v", err)
	}
	if after.SignedIn() {
		t.Fatalf("expected no session after Clear(), got %+v", after)
	}
}

func TestStore_LoadWithNothingStoredIsNotAnError(t *testing.T) {
	keyring.MockInit()
	dir := t.TempDir()
	t.Setenv(envProfileDirOverride, dir)

	store, err := New()
	if err != nil {
		t.Fatalf("New() error = %v", err)
	}
	got, err := store.Load()
	if err != nil {
		t.Fatalf("Load() on an empty store returned an error: %v", err)
	}
	if got.SignedIn() {
		t.Fatalf("expected a zero-value Credentials, got %+v", got)
	}
}

func TestStore_CorruptedFallbackFileFails(t *testing.T) {
	keyring.MockInitWithError(keyring.ErrUnsupportedPlatform)
	dir := t.TempDir()
	t.Setenv(envProfileDirOverride, dir)

	store, err := New()
	if err != nil {
		t.Fatalf("New() error = %v", err)
	}
	if err := store.Save(testCredentials()); err != nil {
		t.Fatalf("Save() error = %v", err)
	}
	if err := os.WriteFile(filepath.Join(dir, credentialsFileName), []byte("not encrypted data at all"), 0o600); err != nil {
		t.Fatalf("corrupting the fallback file: %v", err)
	}

	if _, err := store.Load(); err == nil {
		t.Fatal("expected a corrupted fallback file to fail rather than decode into garbage credentials")
	}
}

// TestStore_Clear_WithNothingStoredSucceeds proves the "delete, then verify
// it is gone" contract holds even when there was nothing to delete: a caller
// composing this with console.Revoke (see console.TestRevoke_ServerError...)
// must be able to call Clear() unconditionally on logout, including when the
// revoke never got far enough to leave a local session behind.
func TestStore_Clear_WithNothingStoredSucceeds(t *testing.T) {
	keyring.MockInit()
	dir := t.TempDir()
	t.Setenv(envProfileDirOverride, dir)

	store, err := New()
	if err != nil {
		t.Fatalf("New() error = %v", err)
	}
	if err := store.Clear(); err != nil {
		t.Fatalf("Clear() on an empty store returned an error: %v", err)
	}
}
