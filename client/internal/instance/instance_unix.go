//go:build !windows

package instance

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"syscall"
	"time"
)

func lock(path string) (func(), error) {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, fmt.Errorf("instance: open %s: %w", path, err)
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		_ = f.Close()
		if errors.Is(err, syscall.EWOULDBLOCK) {
			return nil, ErrRunning
		}
		return nil, fmt.Errorf("instance: lock %s: %w", path, err)
	}
	return func() { _ = f.Close() }, nil
}

// stopOthers sends SIGTERM (the client disconnects cleanly), then SIGKILL after 10 s. Linux reads /proc; elsewhere
// it is not supported (macOS is not a target, owner).
func stopOthers(exe string) (int, error) {
	if runtime.GOOS != "linux" {
		return 0, fmt.Errorf("instance: stopping the running client is not supported on %s: stop it yourself", runtime.GOOS)
	}
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return 0, fmt.Errorf("instance: process list: %w", err)
	}
	self, uid := os.Getpid(), os.Getuid()
	var found []*os.Process
	for _, e := range entries {
		pid, err := strconv.Atoi(e.Name())
		if err != nil || pid == self {
			continue
		}
		if fi, err := os.Stat(filepath.Join("/proc", e.Name())); err != nil || int(fi.Sys().(*syscall.Stat_t).Uid) != uid {
			continue
		}
		if target, err := os.Readlink(filepath.Join("/proc", e.Name(), "exe")); err != nil || !samePath(target, exe, false) {
			continue
		}
		if p, err := os.FindProcess(pid); err == nil && p.Signal(syscall.SIGTERM) == nil {
			found = append(found, p)
		}
	}
	deadline := time.Now().Add(10 * time.Second)
	for _, p := range found {
		for p.Signal(syscall.Signal(0)) == nil && time.Now().Before(deadline) {
			time.Sleep(200 * time.Millisecond)
		}
		_ = p.Signal(syscall.SIGKILL)
	}
	return len(found), nil
}
