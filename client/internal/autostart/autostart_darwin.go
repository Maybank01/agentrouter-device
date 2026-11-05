package autostart

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

func plistPath() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, "Library", "LaunchAgents", agentName+".plist"), nil
}

func domain() string { return "gui/" + strconv.Itoa(os.Getuid()) }

// On writes and loads the LaunchAgent; log is where its output goes.
func On(exe, log string, _ bool) (string, error) {
	path, err := plistPath()
	if err != nil {
		return "", fmt.Errorf("autostart: no home folder: %w", err)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return "", fmt.Errorf("autostart: create %s: %w", filepath.Dir(path), err)
	}
	if err := os.WriteFile(path, []byte(LaunchAgent(agentName, exe, log)), 0o644); err != nil {
		return "", fmt.Errorf("autostart: write %s: %w", path, err)
	}
	_, _ = tool("launchctl", "bootout", domain()+"/"+agentName)
	if _, err := tool("launchctl", "bootstrap", domain(), path); err != nil {
		return "", fmt.Errorf("autostart: %w", err)
	}
	return "LaunchAgent " + agentName, nil
}

// Off unloads and removes the LaunchAgent; false when there was none.
func Off() (bool, error) {
	path, err := plistPath()
	if err != nil {
		return false, err
	}
	if _, err := os.Stat(path); errors.Is(err, os.ErrNotExist) {
		return false, nil
	}
	_, _ = tool("launchctl", "bootout", domain()+"/"+agentName)
	if err := os.Remove(path); err != nil {
		return false, fmt.Errorf("autostart: remove %s: %w", path, err)
	}
	return true, nil
}

// Get reports the LaunchAgent.
func Get() Status {
	path, _ := plistPath()
	if _, err := os.Stat(path); err != nil {
		return Status{Where: path}
	}
	out, err := tool("launchctl", "print", domain()+"/"+agentName)
	running := err == nil && strings.Contains(out, "state = running")
	return Status{On: true, Running: running, Where: "LaunchAgent " + agentName}
}
