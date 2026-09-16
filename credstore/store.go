package credstore

import (
	"encoding/json"
	"errors"
	"fmt"

	"github.com/zalando/go-keyring"
)

// keychainService names every wally item in the OS keystore. The account
// under it is the profile directory itself, so WALLY_PROFILE_DIR isolates a
// second account's session in the keystore too, not only on disk.
const keychainService = "RunAnywhere Wally"

// Store is the credential store for one profile. The zero value is not
// usable; build one with New.
type Store struct {
	profileDir string
}

// New builds a Store against ProfileDir(). It does not touch disk or the
// keystore until Load, Save, or Clear is called.
func New() (*Store, error) {
	dir, err := ProfileDir()
	if err != nil {
		return nil, err
	}
	return &Store{profileDir: dir}, nil
}

func (s *Store) account() string {
	return s.profileDir
}

// Load reads the stored session. A missing credential is not an error: the
// zero Credentials is returned, matching "not signed in".
func (s *Store) Load() (Credentials, error) {
	if err := ensureSecureDir(s.profileDir); err != nil {
		return Credentials{}, err
	}

	if raw, err := keyring.Get(keychainService, s.account()); err == nil {
		return decodeCredentials([]byte(raw))
	} else if !errors.Is(err, keyring.ErrNotFound) {
		// The native store errored for a reason other than "nothing stored
		// here" - no Secret Service running, an unsupported platform, a locked
		// keychain a headless session cannot unlock. That is "unavailable," not
		// "not signed in," so fall through to whatever the file fallback holds
		// rather than reporting the person signed out.
	}

	plaintext, found, err := decryptFromFile(s.profileDir)
	if err != nil {
		return Credentials{}, err
	}
	if !found {
		return Credentials{}, nil
	}
	return decodeCredentials(plaintext)
}

func decodeCredentials(raw []byte) (Credentials, error) {
	var c Credentials
	if err := json.Unmarshal(raw, &c); err != nil {
		return Credentials{}, fmt.Errorf("credstore: stored credentials are corrupted: %w", err)
	}
	return c, nil
}

// Save encrypts and stores c, preferring the OS keystore and falling back to
// a locally encrypted file only when the keystore call itself fails.
func (s *Store) Save(c Credentials) error {
	if err := ensureSecureDir(s.profileDir); err != nil {
		return err
	}
	if (c.AccessToken != "" && !tokenSafe(c.AccessToken)) || (c.RefreshToken != "" && !tokenSafe(c.RefreshToken)) {
		return errors.New("credstore: refusing to store an invalid cloud session token")
	}

	encoded, err := json.Marshal(c)
	if err != nil {
		return fmt.Errorf("credstore: could not encode credentials: %w", err)
	}

	if err := keyring.Set(keychainService, s.account(), string(encoded)); err == nil {
		// The keystore now holds the current value; a stale fallback file would
		// otherwise sit on disk holding an older or orphaned copy.
		_ = removeFallbackFiles(s.profileDir)
		return nil
	}

	return encryptToFile(s.profileDir, encoded)
}

// Clear removes the stored session from wherever it lives, then verifies
// nothing is left. It succeeds even when the native keystore delete itself
// errors, as long as the credential is actually gone afterward - the same
// principle a caller applies at the console: a failed remote revoke should
// still leave the local session cleared.
func (s *Store) Clear() error {
	if err := ensureSecureDir(s.profileDir); err != nil {
		return err
	}

	keychainErr := keyring.Delete(keychainService, s.account())
	if errors.Is(keychainErr, keyring.ErrNotFound) {
		keychainErr = nil
	}
	fileErr := removeFallbackFiles(s.profileDir)

	remaining, err := s.Load()
	if err != nil {
		return err
	}
	if remaining.SignedIn() {
		return fmt.Errorf("credstore: credential was not fully removed (keychain: %v, file: %v)", keychainErr, fileErr)
	}
	return nil
}

// tokenSafe constrains a token to RFC 6750 b64token characters before it is
// ever written to disk or the keystore.
func tokenSafe(token string) bool {
	if token == "" || len(token) > 8192 {
		return false
	}
	for _, r := range token {
		alnum := (r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9')
		if !alnum && r != '-' && r != '.' && r != '_' && r != '~' && r != '+' && r != '/' && r != '=' {
			return false
		}
	}
	return true
}
