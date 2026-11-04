package winconsole

import (
	"fmt"
	"io"
	"os"
	"unsafe"

	"golang.org/x/sys/windows"
)

const attachParentProcess = ^uint32(0) // ATTACH_PARENT_PROCESS

var (
	kernel32            = windows.NewLazySystemDLL("kernel32.dll")
	procAttachConsole   = kernel32.NewProc("AttachConsole")
	procAllocConsole    = kernel32.NewProc("AllocConsole")
	procSetConsoleTitle = kernel32.NewProc("SetConsoleTitleW")
	attached            bool
)

func hasStdout() bool {
	h, err := windows.GetStdHandle(windows.STD_OUTPUT_HANDLE)
	return err == nil && h != 0 && h != windows.InvalidHandle
}

func attach() bool {
	// A console program (go run, a console build) already has its streams.
	if hasStdout() {
		attached = true
		return true
	}
	if r, _, _ := procAttachConsole.Call(uintptr(attachParentProcess)); r == 0 {
		return false
	}
	reopen()
	attached = true
	return true
}

// reopen points stdin, stdout and stderr at the process's console.
func reopen() {
	if f, err := os.OpenFile("CONOUT$", os.O_WRONLY, 0); err == nil {
		os.Stdout, os.Stderr = f, f
		_ = windows.SetStdHandle(windows.STD_OUTPUT_HANDLE, windows.Handle(f.Fd()))
		_ = windows.SetStdHandle(windows.STD_ERROR_HANDLE, windows.Handle(f.Fd()))
	}
	if f, err := os.OpenFile("CONIN$", os.O_RDWR, 0); err == nil {
		os.Stdin = f
		_ = windows.SetStdHandle(windows.STD_INPUT_HANDLE, windows.Handle(f.Fd()))
		// No QuickEdit: a mouse selection would pause every write to the window until it is cleared.
		var mode uint32
		if windows.GetConsoleMode(windows.Handle(f.Fd()), &mode) == nil {
			mode = (mode &^ windows.ENABLE_QUICK_EDIT_MODE) | windows.ENABLE_EXTENDED_FLAGS
			_ = windows.SetConsoleMode(windows.Handle(f.Fd()), mode)
		}
	}
}

func resident(title string) (io.Writer, error) {
	if !attached {
		if r, _, err := procAllocConsole.Call(); r == 0 {
			return nil, fmt.Errorf("winconsole: AllocConsole: %w", err)
		}
		reopen()
		attached = true
	}
	if t, err := windows.UTF16PtrFromString(title); err == nil {
		_, _, _ = procSetConsoleTitle.Call(uintptr(unsafe.Pointer(t)))
	}
	return newLineWriter(os.Stdout), nil
}
