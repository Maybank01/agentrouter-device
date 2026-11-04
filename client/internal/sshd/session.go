package sshd

import (
	"errors"
	"io"
	"os/exec"

	"charm.land/ssh"

	"github.com/agentrouter-top/AgentDevice/client/internal/proc"
)

// runPipes runs a command without a terminal: stdin, stdout and stderr are the session's own streams (binary-safe).
func (s *Server) runPipes(sess ssh.Session, args []string, env []string) int {
	cmd := exec.Command(args[0], args[1:]...)
	cmd.Env, cmd.Dir = env, s.cfg.Home
	cmd.Stdout, cmd.Stderr = sess, sess.Stderr()
	proc.Prepare(cmd)
	// stdin is copied by our own goroutine: os/exec would wait for it, and a client keeps stdin open after the
	// command has ended.
	stdin, err := cmd.StdinPipe()
	if err != nil {
		fail(sess, "AgentDevice ssh: stdin pipe for %s: %v", args[0], err)
		return 255
	}
	if err := cmd.Start(); err != nil {
		fail(sess, "AgentDevice ssh: start %s: %v (the command did not run)", args[0], err)
		return 255
	}
	tree, err := proc.Track(cmd)
	if err != nil {
		s.cfg.Log.Warn("ssh: process tree not tracked; a disconnect ends only the shell", "error", err.Error())
		tree = nil
	}
	go func() {
		_, _ = io.Copy(stdin, sess)
		_ = stdin.Close()
	}()
	done := make(chan struct{})
	kill := func() error {
		if tree != nil {
			return tree.Kill()
		}
		return cmd.Process.Kill()
	}
	signal := func(name string) error {
		if tree != nil {
			return tree.Signal(name)
		}
		return nil
	}
	forwardSignals(sess, signal, done)
	waitOrKill(sess.Context(), kill, done)
	err = cmd.Wait()
	close(done)
	if tree != nil {
		_ = tree.Close()
	}
	return exitCode(cmd, err)
}

func exitCode(cmd *exec.Cmd, err error) int {
	if err == nil {
		return 0
	}
	var ee *exec.ExitError
	if errors.As(err, &ee) {
		if code := ee.ExitCode(); code >= 0 {
			return code
		}
		return signalExit(cmd)
	}
	return 255
}
