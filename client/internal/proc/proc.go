// Package proc starts the device's child processes (SSH sessions' shells, fact probes) the same way on every OS, and
// ends the whole process tree when the client disconnects while its command runs.
//
// Windows: no hidden and no extra windows (docs/development/AV-HYGIENE.md). `agentdevice run` keeps one visible
// console window (internal/winconsole); children start with no console flag at all, so a console program attaches to
// that window while its streams are our pipes. Terminals run in a pseudo console. Each session's shell goes into a
// Job Object, so the whole tree ends when the client disconnects while the command still runs.
// Unix: each session's shell leads its own process group (or session, with a terminal), signalled and killed as one.
package proc

import "os/exec"

// Tree is the process tree of one started command.
type Tree interface {
	// Signal forwards an SSH signal name ("INT", "TERM", "KILL", "HUP", …) to the tree; unknown names are ignored.
	Signal(name string) error
	// Kill ends the whole tree.
	Kill() error
	// Close releases the tree's handles; what is still running keeps running.
	Close() error
}

// Prepare sets the attributes every child gets (Unix: its own process group). Call before Start.
func Prepare(cmd *exec.Cmd) { prepare(cmd) }

// Track returns the tree of a started command (Windows: puts it into a new Job Object).
func Track(cmd *exec.Cmd) (Tree, error) { return track(cmd) }
