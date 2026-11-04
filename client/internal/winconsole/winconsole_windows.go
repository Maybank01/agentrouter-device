package winconsole

import (
	"os"

	"golang.org/x/sys/windows"
)

const attachParentProcess = ^uint32(0) // ATTACH_PARENT_PROCESS

var procAttachConsole = windows.NewLazySystemDLL("kernel32.dll").NewProc("AttachConsole")

func attach() bool {
	// A console program (go run, a console build) already has its streams.
	if h, err := windows.GetStdHandle(windows.STD_OUTPUT_HANDLE); err == nil && h != 0 && h != windows.InvalidHandle {
		return true
	}
	if r, _, _ := procAttachConsole.Call(uintptr(attachParentProcess)); r == 0 {
		return false
	}
	if f, err := os.OpenFile("CONOUT$", os.O_WRONLY, 0); err == nil {
		os.Stdout, os.Stderr = f, f
	}
	if f, err := os.OpenFile("CONIN$", os.O_RDONLY, 0); err == nil {
		os.Stdin = f
	}
	return true
}
