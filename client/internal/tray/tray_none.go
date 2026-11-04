//go:build !(windows || linux || (darwin && cgo))

package tray

// Available is false: this build has no tray.
func Available() bool { return false }

type State int

const (
	Connecting State = iota
	Online
	Offline
)

type Handle struct{}

func (h *Handle) Set(State, string, string) {}

func Run(func(*Handle), func()) {}

func Quit() {}
