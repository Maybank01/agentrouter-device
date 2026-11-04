package sshd

import (
	"fmt"
	"os"
	"unsafe"

	"golang.org/x/sys/windows"
)

// createOwnerOnly creates a new file whose DACL is protected and names only this process's user (full control): no
// inherited entries, no Administrators, no SYSTEM.
func createOwnerOnly(path string) (*os.File, error) {
	user, err := windows.GetCurrentProcessToken().GetTokenUser()
	if err != nil {
		return nil, fmt.Errorf("token user: %w", err)
	}
	sd, err := windows.SecurityDescriptorFromString("D:P(A;;FA;;;" + user.User.Sid.String() + ")")
	if err != nil {
		return nil, fmt.Errorf("security descriptor: %w", err)
	}
	sa := &windows.SecurityAttributes{SecurityDescriptor: sd}
	sa.Length = uint32(unsafe.Sizeof(*sa))
	name, err := windows.UTF16PtrFromString(path)
	if err != nil {
		return nil, err
	}
	h, err := windows.CreateFile(name, windows.GENERIC_WRITE, windows.FILE_SHARE_READ, sa, windows.CREATE_NEW, windows.FILE_ATTRIBUTE_NORMAL, 0)
	if err != nil {
		return nil, err
	}
	return os.NewFile(uintptr(h), path), nil
}
