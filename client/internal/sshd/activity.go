package sshd

import (
	"strings"
	"time"

	"charm.land/ssh"

	"github.com/agentrouter-top/AgentDevice/client/internal/identity"
)

// Activity is one thing an Agent did on this computer, for the client's visible activity log: a command (exec), a
// terminal, an SFTP session or a forward. It carries no token and only the command's first line.
type Activity struct {
	Time    time.Time
	Kind    string // exec | terminal | sftp | forward
	Sub     string // who: the token's sub (svc_agenthost, acc_…)
	Run     string // which conversation, when the token names one
	Command string // first line of the command, at most 80 characters ("" for an interactive shell)
	Target  string // forward: host:port
	Allowed bool   // forward: allowed or refused
	Exit    int
	Seconds float64
}

func (s *Server) report(ctx ssh.Context, a Activity) {
	if s.cfg.Activity == nil {
		return
	}
	if c, ok := ctx.Value(claimsKey{}).(*identity.Claims); ok {
		a.Sub, a.Run = c.Sub, c.Run
	}
	if a.Time.IsZero() {
		a.Time = time.Now()
	}
	s.cfg.Activity(a)
}

// FirstLine is the command's first non-empty line, cut to 80 characters.
func FirstLine(cmd string) string {
	for _, line := range strings.Split(cmd, "\n") {
		line = strings.TrimSpace(strings.TrimSuffix(line, "\r"))
		if line == "" {
			continue
		}
		if r := []rune(line); len(r) > 80 {
			return string(r[:80]) + "…"
		}
		return line
	}
	return ""
}
