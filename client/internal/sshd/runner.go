package sshd

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/agentrouter-top/AgentDevice/client/internal/facts"
)

// How a command's text reaches the shell: never as an argument (R21, "No data through command lines"): an argument
// is visible to every process, cut at 128 KiB on Linux and 32 767 characters on Windows, and an Agent's heredoc that
// writes a file is one command. The text goes into a file only this account can open in <temp>/agentdevice-run/;
// the shell runs a fixed runner that reads it, deletes it and runs it. stdin stays the session's. The runners are
// the ones the former Rust hostd proved (tag hostd-final, command_text.rs): plain text, no encoded command, no
// execution-policy change, no script run with -File (docs/development/AV-HYGIENE.md).

// unixFileRunner runs the text in "$1" in the account's (POSIX) shell: read, delete, eval.
const unixFileRunner = `c=$(cat -- "$1") && rm -f -- "$1" && eval "$c"`

// psRun is the PowerShell runner's second half, after $__ar_t holds the text: parse it alone first (a syntax error
// exits 1, as -Command does), run it dot-sourced keeping its last statement's status, exit 0/1 by that status
// (`exit N` inside exits N).
const psRun = "$null = [scriptblock]::Create($__ar_t); if (-not $?) { exit 1 }; " +
	"$__ar_sb = [scriptblock]::Create($__ar_t + \"`n`n`$__ar_ok = `$?\"); if (-not $?) { exit 1 }; " +
	"$__ar_ok = $true; . $__ar_sb; exit [int](-not $__ar_ok)"

// psUTF8 makes PowerShell read and write UTF-8 whatever the console code page (a zh-CN system defaults to 936), and
// keeps progress bars out of the shared console window (nobody reads them there; the output is in the pipes).
const psUTF8 = "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding; " +
	"$ProgressPreference = 'SilentlyContinue'; "

// psFileRunner is the PowerShell runner for the text in file (UTF-8): read, delete, run.
func psFileRunner(file string) string {
	quoted := strings.ReplaceAll(file, "'", "''")
	return psUTF8 + "$__ar_f = '" + quoted + "'; $__ar_t = [IO.File]::ReadAllText($__ar_f, [Text.UTF8Encoding]::new($false)); " +
		"Remove-Item -LiteralPath $__ar_f -Force; " + psRun
}

// runDir is where command files go: <temp>/agentdevice-run/ (the account's own temp folder).
func runDir() string { return filepath.Join(os.TempDir(), "agentdevice-run") }

// textFile writes text to a new owner-only file in dir and returns its path.
func textFile(dir, text string) (string, error) {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", fmt.Errorf("create %s: %w", dir, err)
	}
	b := make([]byte, 12)
	_, _ = rand.Read(b)
	path := filepath.Join(dir, "cmd-"+hex.EncodeToString(b)+".txt")
	f, err := createOwnerOnly(path)
	if err != nil {
		return "", fmt.Errorf("write command file %s: %w", path, err)
	}
	if _, err := f.WriteString(text); err != nil {
		f.Close()
		os.Remove(path)
		return "", fmt.Errorf("write command file %s: %w", path, err)
	}
	if err := f.Close(); err != nil {
		os.Remove(path)
		return "", fmt.Errorf("write command file %s: %w", path, err)
	}
	return path, nil
}

// argv returns the program and arguments for a session: an interactive shell when file is "", otherwise the
// runner for the command text in file.
func argv(sh facts.UserShellInfo, isWindows, pty bool, file string) []string {
	if isWindows {
		if file == "" {
			return []string{sh.Path, "-NoLogo"}
		}
		args := []string{sh.Path, "-NoLogo", "-NoProfile"}
		if !pty {
			args = append(args, "-NonInteractive")
		}
		return append(args, "-Command", psFileRunner(file))
	}
	if file == "" {
		return []string{sh.Path, "-l"}
	}
	return []string{sh.Path, "-c", unixFileRunner, "sh", file}
}
