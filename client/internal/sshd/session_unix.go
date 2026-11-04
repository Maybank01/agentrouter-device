//go:build !windows

package sshd

import (
	"os/exec"
	"syscall"

	"charm.land/ssh"

	"github.com/agentrouter-top/AgentDevice/client/internal/proc"
)

func (s *Server) run(sess ssh.Session, args []string, env []string, isPty bool, p ssh.Pty) int {
	if !isPty {
		return s.runPipes(sess, args, env)
	}
	cmd := exec.Command(args[0], args[1:]...)
	cmd.Env, cmd.Dir = env, s.cfg.Home
	// The shell leads its own session with the terminal as its controlling terminal (job control, ^C, ^Z).
	if err := p.Start(cmd, ssh.WithJobControl()); err != nil {
		fail(sess, "AgentDevice ssh: start %s on a terminal: %v (the command did not run)", args[0], err)
		return 255
	}
	tree, _ := proc.Track(cmd)
	done := make(chan struct{})
	forwardSignals(sess, tree.Signal, done)
	waitOrKill(sess.Context(), tree.Kill, done)
	err := cmd.Wait()
	close(done)
	return exitCode(cmd, err)
}

// signalExit is 128 + the signal number, as a shell reports a command a signal ended.
func signalExit(cmd *exec.Cmd) int {
	if ws, ok := cmd.ProcessState.Sys().(syscall.WaitStatus); ok && ws.Signaled() {
		return 128 + int(ws.Signal())
	}
	return 255
}
