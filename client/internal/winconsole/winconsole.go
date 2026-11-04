// Package winconsole gives the Windows build its one console window. The build is a GUI program (the tray), so:
//   - Attach: a command started from a terminal prints to that terminal (its parent's console).
//   - Resident: `agentdevice run` keeps ONE visible console window, 「AgentRouter 设备」 (owner 2026-11-04): the Agent
//     commands it runs attach to that console (no console of their own, no hidden one), so the window is the visible
//     sign that this computer is connected and shows one line per command. Closing it disconnects cleanly (Windows
//     delivers CTRL_CLOSE_EVENT, which Go turns into SIGTERM). Elsewhere both are no-ops and output is stdout.
package winconsole

import "io"

// Attach connects stdout and stderr to the parent's console; false when there is none (started from Explorer or at
// logon), and output then goes only to the log file.
func Attach() bool { return attach() }

// Resident makes sure the process has a visible console window titled title (the parent's console when there is
// one, otherwise a new one) and returns a writer for its activity lines. Writes never block the caller: a full
// buffer drops the line (it is in the log file too).
func Resident(title string) (io.Writer, error) { return resident(title) }

// lineWriter queues lines for one goroutine that writes them, so a console paused by a selection never blocks an
// SSH session.
type lineWriter struct{ ch chan []byte }

func newLineWriter(w io.Writer) *lineWriter {
	lw := &lineWriter{ch: make(chan []byte, 256)}
	go func() {
		for b := range lw.ch {
			_, _ = w.Write(b)
		}
	}()
	return lw
}

func (lw *lineWriter) Write(p []byte) (int, error) {
	b := append([]byte(nil), p...)
	select {
	case lw.ch <- b:
	default:
	}
	return len(p), nil
}
