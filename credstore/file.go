package credstore

import (
	"crypto/aes"
	"crypto/cipher"
	"crypto/rand"
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

// The file fallback only runs when no OS keystore answered, so there is no
// keystore to hold the encryption key either. The key sits in its own 0600
// file beside the ciphertext: real AES-256-GCM encryption, but the key's own
// protection is still the filesystem permission it inherited from
// ensureSecureDir. That is weaker than an OS keystore's per-app binding, and
// it is the best a no-keystore host offers; it is not a reason to fall back
// to a bare 0600 plaintext file, which is what this replaces.
const (
	keyFileName         = "credstore.key"
	credentialsFileName = "credentials.enc"
	encryptionKeySize   = 32 // AES-256
)

func loadOrCreateKey(dir string) ([]byte, error) {
	path := filepath.Join(dir, keyFileName)
	data, err := os.ReadFile(path)
	if err == nil {
		if len(data) != encryptionKeySize {
			return nil, errors.New("credstore: stored encryption key has an unexpected size")
		}
		return data, nil
	}
	if !os.IsNotExist(err) {
		return nil, fmt.Errorf("credstore: could not read the encryption key: %w", err)
	}

	key := make([]byte, encryptionKeySize)
	if _, err := rand.Read(key); err != nil {
		return nil, fmt.Errorf("credstore: could not generate an encryption key: %w", err)
	}
	if err := writeFileAtomic(path, key, 0o600); err != nil {
		return nil, fmt.Errorf("credstore: could not store the encryption key: %w", err)
	}
	return key, nil
}

func openGCM(dir string) (cipher.AEAD, error) {
	key, err := loadOrCreateKey(dir)
	if err != nil {
		return nil, err
	}
	block, err := aes.NewCipher(key)
	if err != nil {
		return nil, fmt.Errorf("credstore: could not initialize encryption: %w", err)
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return nil, fmt.Errorf("credstore: could not initialize encryption: %w", err)
	}
	return gcm, nil
}

// encryptToFile seals plaintext with a locally generated key and writes
// nonce||ciphertext atomically to the fallback credentials file.
func encryptToFile(dir string, plaintext []byte) error {
	gcm, err := openGCM(dir)
	if err != nil {
		return err
	}
	nonce := make([]byte, gcm.NonceSize())
	if _, err := rand.Read(nonce); err != nil {
		return fmt.Errorf("credstore: could not generate a nonce: %w", err)
	}
	sealed := gcm.Seal(nonce, nonce, plaintext, nil)
	return writeFileAtomic(filepath.Join(dir, credentialsFileName), sealed, 0o600)
}

// decryptFromFile reads and opens the fallback credentials file. found is
// false, with a nil error, when no fallback file exists: that is the normal
// "nothing stored yet" case, not a failure.
func decryptFromFile(dir string) (plaintext []byte, found bool, err error) {
	path := filepath.Join(dir, credentialsFileName)
	sealed, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, false, nil
		}
		return nil, false, fmt.Errorf("credstore: could not read the credential file: %w", err)
	}
	gcm, err := openGCM(dir)
	if err != nil {
		return nil, false, err
	}
	if len(sealed) < gcm.NonceSize() {
		return nil, false, errors.New("credstore: credential file is corrupted")
	}
	nonce, ciphertext := sealed[:gcm.NonceSize()], sealed[gcm.NonceSize():]
	plaintext, err = gcm.Open(nil, nonce, ciphertext, nil)
	if err != nil {
		return nil, false, fmt.Errorf("credstore: credential file failed to decrypt: %w", err)
	}
	return plaintext, true, nil
}

// removeFallbackFiles deletes the fallback credentials file and its key.
// Removing the key too means a later login starts from fresh key material
// instead of reusing one a prior session already exposed to memory.
func removeFallbackFiles(dir string) error {
	for _, name := range []string{credentialsFileName, keyFileName} {
		if err := os.Remove(filepath.Join(dir, name)); err != nil && !os.IsNotExist(err) {
			return fmt.Errorf("credstore: could not remove %s: %w", name, err)
		}
	}
	return nil
}

func writeFileAtomic(path string, data []byte, perm os.FileMode) error {
	dir := filepath.Dir(path)
	tmp, err := os.CreateTemp(dir, filepath.Base(path)+".tmp-*")
	if err != nil {
		return err
	}
	tmpPath := tmp.Name()
	defer os.Remove(tmpPath)

	if _, err := tmp.Write(data); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Sync(); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	if err := os.Chmod(tmpPath, perm); err != nil {
		return err
	}
	return os.Rename(tmpPath, path)
}
