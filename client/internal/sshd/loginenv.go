package sshd

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"runtime"
	"time"

	"github.com/agentrouter-top/AgentDevice/client/internal/facts"
	"github.com/agentrouter-top/AgentDevice/client/internal/proc"
)

// PrintEnvCommand is the hidden subcommand LoginEnv runs: it prints the process environment as a JSON array.
const PrintEnvCommand = "__print-env"

// PrintEnv implements PrintEnvCommand.
func PrintEnv() int {
	if err := json.NewEncoder(os.Stdout).Encode(os.Environ()); err != nil {
		return 1
	}
	return 0
}

// LoginEnv returns the account's login environment on Unix: what its login shell's start-up files set (PATH with
// ~/.local/bin, version managers, …), which a client started by systemd --user or launchd does not have. The login
// shell runs once at start, with only fixed arguments: it re-runs this program (self) with PrintEnvCommand. On
// Windows, or when that fails within ten seconds, the client's own environment.
func LoginEnv(ctx context.Context, sh facts.UserShellInfo, self string, own []string) []string {
	if runtime.GOOS == "windows" || self == "" {
		return own
	}
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, sh.Path, "-l", "-c", `exec "$0" `+PrintEnvCommand, self)
	cmd.Env = own
	proc.Prepare(cmd)
	out, err := cmd.Output()
	if err != nil {
		return own
	}
	var env []string
	if json.Unmarshal(out, &env) != nil || len(env) == 0 {
		return own
	}
	return env
}
