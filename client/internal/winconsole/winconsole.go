// Package winconsole lets the Windows build, a GUI program (no console window at login, the tray instead), print to
// the terminal it was started from: it attaches to the parent's console when there is one.
package winconsole

// Attach connects stdout and stderr to the parent's console; false when there is none (started from Explorer or at
// logon), and output then goes only to the log file.
func Attach() bool { return attach() }
