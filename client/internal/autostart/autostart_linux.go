package autostart

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

func unitPath() (string, error) {
	base := os.Getenv("XDG_CONFIG_HOME")
	if base == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		base = filepath.Join(home, ".config")
	}
	return filepath.Join(base, "systemd", "user", unitName), nil
}

// On writes and starts the systemd user unit.
func On(exe, _ string) (string, error) {
	path, err := unitPath()
	if err != nil {
		return "", fmt.Errorf("autostart: no home folder: %w", err)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return "", fmt.Errorf("autostart: create %s: %w", filepath.Dir(path), err)
	}
	if err := os.WriteFile(path, []byte(SystemdUnit(exe)), 0o644); err != nil {
		return "", fmt.Errorf("autostart: write %s: %w", path, err)
	}
	for _, args := range [][]string{{"--user", "daemon-reload"}, {"--user", "enable", unitName}, {"--user", "restart", unitName}} {
		if _, err := tool("systemctl", args...); err != nil {
			return "", fmt.Errorf("autostart: %w (is a systemd user session running? `loginctl enable-linger` keeps it after logout)", err)
		}
	}
	return "systemd user unit " + unitName, nil
}

// Off stops and removes the unit; false when there was none.
func Off() (bool, error) {
	path, err := unitPath()
	if err != nil {
		return false, err
	}
	if _, err := os.Stat(path); errors.Is(err, os.ErrNotExist) {
		return false, nil
	}
	_, _ = tool("systemctl", "--user", "disable", "--now", unitName)
	if err := os.Remove(path); err != nil {
		return false, fmt.Errorf("autostart: remove %s: %w", path, err)
	}
	_, err = tool("systemctl", "--user", "daemon-reload")
	return true, err
}

// Get reports the unit.
func Get() Status {
	path, _ := unitPath()
	if _, err := os.Stat(path); err != nil {
		return Status{Where: path}
	}
	out, _ := tool("systemctl", "--user", "is-active", unitName)
	return Status{On: true, Running: out == "active", Where: "systemd user unit " + unitName, Detail: "journalctl --user -u " + unitName}
}
