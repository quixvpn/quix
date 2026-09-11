package identity

import (
	"fmt"
	"os"
	"path/filepath"
)

func KeyPath() (string, error) {
	// allow overriding the identity file, useful for running two peers locally
	if custom := os.Getenv("QUIX_KEY_PATH"); custom != "" {
		return custom, nil
	}

	configDir, err := os.UserConfigDir()
	if err != nil {
		return "", err
	}
	quixDir := filepath.Join(configDir, "quix", "key")
	return quixDir, nil
}

func LoadSecretKey(path string) ([]byte, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return data, nil
}

func SaveSecretKey(path string, key []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return fmt.Errorf("create key dir: %w", err)
	}
	if err := os.WriteFile(path, key, 0600); err != nil {
		return fmt.Errorf("write key: %w", err)
	}
	return nil
}
