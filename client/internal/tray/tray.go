//go:build windows || linux || (darwin && cgo)

package tray

import (
	"image/color"
	"os"
	"runtime"

	"fyne.io/systray"
)

// Available says whether a tray can be shown here (Linux: a desktop session with D-Bus).
func Available() bool {
	if runtime.GOOS == "linux" {
		return os.Getenv("DBUS_SESSION_BUS_ADDRESS") != "" && (os.Getenv("DISPLAY") != "" || os.Getenv("WAYLAND_DISPLAY") != "")
	}
	return true
}

// State is the tray's three looks.
type State int

const (
	Connecting State = iota
	Online
	Offline
)

// Handle updates a running tray.
type Handle struct {
	name, status *systray.MenuItem
}

func icon(c color.NRGBA) []byte {
	p := iconPNG(c)
	if runtime.GOOS == "windows" {
		return icoOf(p)
	}
	return p
}

// Set shows the device name and its state in words.
func (h *Handle) Set(s State, name, text string) {
	switch s {
	case Online:
		systray.SetIcon(icon(colorOnline))
	case Connecting:
		systray.SetIcon(icon(colorConnecting))
	default:
		systray.SetIcon(icon(colorOffline))
	}
	h.name.SetTitle(name)
	h.status.SetTitle(text)
	systray.SetTooltip("AgentDevice · " + name + " · " + text)
}

// Run shows the tray and blocks until Quit; it must run on the main goroutine. ready gets the handle; quit is
// called when the person chooses 退出.
func Run(ready func(*Handle), quit func()) {
	systray.Run(func() {
		systray.SetIcon(icon(colorConnecting))
		systray.SetTooltip("AgentDevice")
		h := &Handle{name: systray.AddMenuItem("AgentDevice", ""), status: systray.AddMenuItem("正在启动", "")}
		h.name.Disable()
		h.status.Disable()
		systray.AddSeparator()
		exit := systray.AddMenuItem("退出", "关掉 AgentDevice，云端暂时连不上这台电脑")
		go func() {
			<-exit.ClickedCh
			quit()
		}()
		ready(h)
	}, func() {})
}

// Quit closes the tray (Run returns).
func Quit() { systray.Quit() }
