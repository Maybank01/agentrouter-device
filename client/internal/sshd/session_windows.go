package sshd

import (
	"os/exec"
	"time"

	"charm.land/ssh"

	"github.com/agentrouter-top/AgentDevice/client/internal/proc"
)

func (s *Server) run(sess ssh.Session, args []string, env []string, isPty bool, p ssh.Pty) int {
	if !isPty {
		return s.runPipes(sess, args, env)
	}
	cmd := exec.Command(args[0], args[1:]...)
	cmd.Env, cmd.Dir = env, s.cfg.Home
	// ConPTY: the pseudo console has no window. charm.land/ssh resizes it on window-change and copies the streams.
	if err := p.Start(cmd); err != nil {
		fail(sess, "AgentDevice ssh: start %s in a pseudo console: %v (the command did not run)", args[0], err)
		return 255
	}
	pid := cmd.Process.Pid
	tree, err := proc.TrackPID(pid)
	if err != nil {
		s.cfg.Log.Warn("ssh: terminal process tree not tracked", "error", err.Error())
	}
	done := make(chan struct{})
	if tree != nil {
		forwardSignals(sess, tree.Signal, done)
		waitOrKill(sess.Context(), tree.Kill, done)
	}
	// os/exec cannot wait for a ConPTY spawn (charm.land/ssh's own waiter holds it): wait on our own handle.
	code, err := proc.WaitPID(pid)
	close(done)
	if tree != nil {
		_ = tree.Close()
	}
	if err != nil {
		s.cfg.Log.Warn("ssh: terminal exit code unknown", "error", err.Error())
		code = 255
	}
	// Let the last output leave the pseudo console before the channel closes.
	time.Sleep(150 * time.Millisecond)
	return code
}

func signalExit(*exec.Cmd) int { return 255 }
