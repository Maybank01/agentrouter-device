//go:build windows

package proc

import (
	"fmt"
	"os/exec"
	"syscall"

	"golang.org/x/sys/windows"
)

func prepare(cmd *exec.Cmd) {
	if cmd.SysProcAttr == nil {
		cmd.SysProcAttr = &syscall.SysProcAttr{}
	}
	cmd.SysProcAttr.CreationFlags |= windows.CREATE_NO_WINDOW
}

type job struct {
	handle windows.Handle
	pid    int
}

// track puts the started process into a new Job Object. The shell is assigned right after it starts, before it has
// read its command, so what it starts is in the job too. Kill ends every member; closing the handle does not, so
// what a finished command left running on purpose keeps running (as with OpenSSH).
func track(cmd *exec.Cmd) (Tree, error) {
	return TrackPID(cmd.Process.Pid)
}

// TrackPID is Track for a process started elsewhere (the ConPTY spawn).
func TrackPID(pid int) (Tree, error) {
	h, err := windows.CreateJobObject(nil, nil)
	if err != nil {
		return nil, fmt.Errorf("proc: CreateJobObject: %w", err)
	}
	ph, err := windows.OpenProcess(windows.PROCESS_SET_QUOTA|windows.PROCESS_TERMINATE, false, uint32(pid))
	if err != nil {
		windows.CloseHandle(h)
		return nil, fmt.Errorf("proc: OpenProcess %d: %w", pid, err)
	}
	defer windows.CloseHandle(ph)
	if err := windows.AssignProcessToJobObject(h, ph); err != nil {
		windows.CloseHandle(h)
		return nil, fmt.Errorf("proc: AssignProcessToJobObject %d: %w", pid, err)
	}
	return &job{handle: h, pid: pid}, nil
}

// Signal: Ctrl+C / Ctrl+Break reach a console process group only through the console, which an SSH session does
// not share with the client; INT, TERM, KILL and HUP therefore end the tree, other signals are ignored.
func (j *job) Signal(name string) error {
	switch name {
	case "INT", "TERM", "KILL", "HUP", "QUIT":
		return j.Kill()
	}
	return nil
}

func (j *job) Kill() error {
	if j.handle == 0 {
		return nil
	}
	return windows.TerminateJobObject(j.handle, 1)
}

// Close releases the job handle; members keep running.
func (j *job) Close() error {
	if j.handle == 0 {
		return nil
	}
	err := windows.CloseHandle(j.handle)
	j.handle = 0
	return err
}

// SignalNumber has no meaning on Windows.
func SignalNumber(string) int { return 0 }

// WaitPID waits for a process started outside os/exec (the ConPTY spawn) and returns its exit code.
func WaitPID(pid int) (int, error) {
	h, err := windows.OpenProcess(windows.SYNCHRONIZE|windows.PROCESS_QUERY_LIMITED_INFORMATION, false, uint32(pid))
	if err != nil {
		return -1, fmt.Errorf("proc: OpenProcess %d: %w", pid, err)
	}
	defer windows.CloseHandle(h)
	if _, err := windows.WaitForSingleObject(h, windows.INFINITE); err != nil {
		return -1, fmt.Errorf("proc: wait for %d: %w", pid, err)
	}
	var code uint32
	if err := windows.GetExitCodeProcess(h, &code); err != nil {
		return -1, fmt.Errorf("proc: exit code of %d: %w", pid, err)
	}
	return int(code), nil
}
