//go:build !windows

package state

// ownerOnly: the file was created 0600.
func ownerOnly(string) error { return nil }
