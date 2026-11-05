// Package instance keeps one `agentdevice run` per client folder and stops the running client for the installer:
//   - Lock: the run lock in the client's folder (Windows: a file opened without sharing; Unix: flock). A second start
//     (the Start menu shortcut while the logon task already runs it) finds it held and ends at once.
//   - StopOthers: ends every other process of this same program file (the running client, its Agents' commands
//     under it on Windows), found by the program's full path, never by name (workspace rule R39). The installer
//     runs it before it replaces or removes the program (`agentdevice stop`, `agentdevice uninstall`).
package instance

import (
	"errors"
	"path/filepath"
	"strings"
)

// ErrRunning is a held run lock: the client already runs for this folder.
var ErrRunning = errors.New("instance: the client already runs for this folder")

// Lock takes the run lock (file "run.lock" in dir) and returns its release.
func Lock(dir string) (func(), error) { return lock(filepath.Join(dir, "run.lock")) }

// StopOthers ends the other processes of the program at exe and returns how many it ended.
func StopOthers(exe string) (int, error) { return stopOthers(exe) }

// samePath compares two program paths as the system does (Windows: case-insensitive).
func samePath(a, b string, fold bool) bool {
	a, b = filepath.Clean(a), filepath.Clean(b)
	if fold {
		return strings.EqualFold(a, b)
	}
	return a == b
}
