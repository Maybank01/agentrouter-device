package proc

import (
	"os/exec"
	"runtime"
	"testing"
)

// On Windows no child gets a creation flag that hides, detaches or adds a console (AV-HYGIENE.md): console programs
// share the client's one visible window.
func TestPrepareAddsNoConsoleFlags(t *testing.T) {
	cmd := exec.Command("x")
	Prepare(cmd)
	if runtime.GOOS == "windows" && cmd.SysProcAttr != nil && creationFlags(cmd) != 0 {
		t.Fatalf("creation flags %#x", creationFlags(cmd))
	}
}
