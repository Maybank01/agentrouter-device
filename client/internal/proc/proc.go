// Package proc starts the device's child processes (SSH sessions' shells, fact probes) the same way on every OS:
// no window of their own on the person's desktop, and the whole process tree ends when the client disconnects
// while its command runs.
//
// Windows: the client is a tray program without a console, so a console child would open a new console window
// for every command an Agent runs. Children start with CREATE_NO_WINDOW: they get a console that has no window, and
// what they start shares it (docs/adr/0002-own-ssh-server.md "No window per command"). Each session's shell goes
// into a Job Object, so the whole tree ends when the client disconnects while the command still runs.
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

// Prepare sets the attributes every child gets (no window; own process group). Call before Start.
func Prepare(cmd *exec.Cmd) { prepare(cmd) }

// Track returns the tree of a started command (Windows: puts it into a new Job Object).
func Track(cmd *exec.Cmd) (Tree, error) { return track(cmd) }
