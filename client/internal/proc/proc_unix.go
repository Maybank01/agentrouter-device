//go:build !windows

package proc

import (
	"os/exec"
	"syscall"
)

func prepare(cmd *exec.Cmd) {
	if cmd.SysProcAttr == nil {
		cmd.SysProcAttr = &syscall.SysProcAttr{}
	}
	if !cmd.SysProcAttr.Setsid {
		cmd.SysProcAttr.Setpgid = true
	}
}

type group struct{ pid int }

func track(cmd *exec.Cmd) (Tree, error) { return group{cmd.Process.Pid}, nil }

var signals = map[string]syscall.Signal{
	"ABRT": syscall.SIGABRT, "ALRM": syscall.SIGALRM, "FPE": syscall.SIGFPE, "HUP": syscall.SIGHUP,
	"ILL": syscall.SIGILL, "INT": syscall.SIGINT, "KILL": syscall.SIGKILL, "PIPE": syscall.SIGPIPE,
	"QUIT": syscall.SIGQUIT, "SEGV": syscall.SIGSEGV, "TERM": syscall.SIGTERM, "USR1": syscall.SIGUSR1,
	"USR2": syscall.SIGUSR2,
}

// The group id equals the leader's pid (Setpgid or Setsid).
func (g group) Signal(name string) error {
	sig, ok := signals[name]
	if !ok {
		return nil
	}
	return syscall.Kill(-g.pid, sig)
}

func (g group) Kill() error  { return syscall.Kill(-g.pid, syscall.SIGKILL) }
func (g group) Close() error { return nil }

// SignalNumber is the number of an SSH signal name (0 when unknown), for exit codes 128+n.
func SignalNumber(name string) int { return int(signals[name]) }

// WaitPID is only needed on Windows (the ConPTY spawn).
func WaitPID(int) (int, error) { return -1, nil }

func creationFlags(*exec.Cmd) uint32 { return 0 }
