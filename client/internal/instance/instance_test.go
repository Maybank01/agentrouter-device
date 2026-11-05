package instance

import (
	"errors"
	"testing"
)

func TestLockIsHeldOnce(t *testing.T) {
	dir := t.TempDir()
	release, err := Lock(dir)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := Lock(dir); !errors.Is(err, ErrRunning) {
		t.Fatalf("second lock: %v, want ErrRunning", err)
	}
	release()
	again, err := Lock(dir)
	if err != nil {
		t.Fatalf("after release: %v", err)
	}
	again()
}

func TestStopOthersLeavesOtherProgramsAlone(t *testing.T) {
	// A path no process runs from: nothing is found, nothing is ended.
	n, err := StopOthers(t.TempDir() + "/agentdevice-not-running")
	if n != 0 {
		t.Fatalf("ended %d processes of a program that does not run", n)
	}
	if err != nil && !isUnsupported(err) {
		t.Fatal(err)
	}
}
