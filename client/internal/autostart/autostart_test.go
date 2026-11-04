package autostart

import (
	"strings"
	"testing"
)

func TestSystemdUnitQuotesTheProgram(t *testing.T) {
	unit := SystemdUnit(`/home/a b/50%/$x/"q"/agentdevice`)
	if !strings.Contains(unit, `ExecStart="/home/a b/50%%/$$x/\"q\"/agentdevice" run --no-tray`) {
		t.Fatal(unit)
	}
	if !strings.Contains(unit, "Restart=on-abnormal") || !strings.Contains(unit, "WantedBy=default.target") {
		t.Fatal(unit)
	}
}

func TestPlistAndTaskEscapeAndRunOnlyRun(t *testing.T) {
	plist := LaunchAgent(agentName, "/Users/a&b/agentdevice", "/tmp/l<og")
	if !strings.Contains(plist, "<string>/Users/a&amp;b/agentdevice</string><string>run</string>") || !strings.Contains(plist, "/tmp/l&lt;og") {
		t.Fatal(plist)
	}
	task := TaskXML(`S-1-5-21-1-2-3-1001`, `C:\Users\Ann & Bo\AgentDevice\agentdevice.exe`)
	for _, want := range []string{
		`<Command>C:\Users\Ann &amp; Bo\AgentDevice\agentdevice.exe</Command><Arguments>run</Arguments>`,
		"<RunLevel>LeastPrivilege</RunLevel>", "<LogonType>InteractiveToken</LogonType>",
		"<UserId>S-1-5-21-1-2-3-1001</UserId>",
	} {
		if !strings.Contains(task, want) {
			t.Fatalf("missing %q in\n%s", want, task)
		}
	}
	b := UTF16LE("A中")
	if len(b) != 6 || b[0] != 0xFF || b[1] != 0xFE || b[2] != 'A' || b[4] != 0x2D || b[5] != 0x4E {
		t.Fatalf("UTF16LE = % x", b)
	}
}
