package instance

import (
	"errors"
	"fmt"
	"os"
	"unsafe"

	"golang.org/x/sys/windows"
)

func lock(path string) (func(), error) {
	p, err := windows.UTF16PtrFromString(path)
	if err != nil {
		return nil, err
	}
	// No sharing: while this handle is open, every other open of the file fails with a sharing violation.
	h, err := windows.CreateFile(p, windows.GENERIC_READ|windows.GENERIC_WRITE, 0, nil, windows.OPEN_ALWAYS, windows.FILE_ATTRIBUTE_NORMAL, 0)
	if errors.Is(err, windows.ERROR_SHARING_VIOLATION) {
		return nil, ErrRunning
	}
	if err != nil {
		return nil, fmt.Errorf("instance: open %s: %w", path, err)
	}
	return func() { _ = windows.CloseHandle(h) }, nil
}

func imagePath(h windows.Handle) string {
	buf := make([]uint16, windows.MAX_LONG_PATH)
	n := uint32(len(buf))
	if err := windows.QueryFullProcessImageName(h, 0, &buf[0], &n); err != nil {
		return ""
	}
	return windows.UTF16ToString(buf[:n])
}

func stopOthers(exe string) (int, error) {
	snap, err := windows.CreateToolhelp32Snapshot(windows.TH32CS_SNAPPROCESS, 0)
	if err != nil {
		return 0, fmt.Errorf("instance: process list: %w", err)
	}
	defer windows.CloseHandle(snap)
	parent := map[uint32]uint32{}
	var e windows.ProcessEntry32
	e.Size = uint32(unsafe.Sizeof(e))
	for err = windows.Process32First(snap, &e); err == nil; err = windows.Process32Next(snap, &e) {
		parent[e.ProcessID] = e.ParentProcessID
	}
	self := uint32(os.Getpid())
	// The program's processes this account may open (another account's are refused, so never touched).
	var clients []uint32
	for pid := range parent {
		if pid == self || pid == 0 {
			continue
		}
		h, err := windows.OpenProcess(windows.PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
		if err != nil {
			continue
		}
		same := samePath(imagePath(h), exe, true)
		_ = windows.CloseHandle(h)
		if same {
			clients = append(clients, pid)
		}
	}
	// With what they started (the Agents' shells): a command left running would keep the client's window open.
	victims := map[uint32]bool{}
	var add func(uint32)
	add = func(pid uint32) {
		if victims[pid] || pid == self {
			return
		}
		victims[pid] = true
		for child, p := range parent {
			if p == pid && child != pid {
				add(child)
			}
		}
	}
	for _, pid := range clients {
		add(pid)
	}
	var errs []error
	for pid := range victims {
		h, err := windows.OpenProcess(windows.PROCESS_TERMINATE|windows.SYNCHRONIZE, false, pid)
		if err != nil {
			continue // ended meanwhile, or not this account's
		}
		if err := windows.TerminateProcess(h, 1); err != nil {
			errs = append(errs, fmt.Errorf("instance: end process %d: %w", pid, err))
		} else {
			_, _ = windows.WaitForSingleObject(h, 10_000)
		}
		_ = windows.CloseHandle(h)
	}
	return len(clients), errors.Join(errs...)
}
