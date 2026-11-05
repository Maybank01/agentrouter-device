// Package autostart turns on, off and reports that the client starts when the person logs in, with the operating
// system's own per-user mechanism and no administrator rights: a systemd user unit (Linux), a LaunchAgent (macOS),
// a Task Scheduler logon task at the least privilege level with no stored password (Windows). No Run key, no
// startup folder (docs/development/AV-HYGIENE.md). Off by default; only `agentdevice autostart on` turns it on.
// The definitions are the former Rust hostd's (tag hostd-final, service.rs), proven on all three systems; the
// program they start is `<agentdevice> run`.
package autostart

import (
	"fmt"
	"os/exec"
	"strings"

	"github.com/agentrouter-top/AgentDevice/client/internal/proc"
)

const (
	unitName  = "agentdevice.service"
	agentName = "top.agentrouter.agentdevice"
	taskName  = "AgentDevice"
)

// Status is what `autostart status` says.
type Status struct {
	On      bool
	Running bool
	Where   string
	Detail  string
}

// tool runs one OS tool with fixed arguments (names and paths only) and returns its own words on failure.
func tool(program string, args ...string) (string, error) {
	cmd := exec.Command(program, args...)
	proc.Prepare(cmd)
	out, err := cmd.CombinedOutput()
	text := strings.TrimSpace(string(out))
	if err != nil {
		if len(text) > 300 {
			text = text[:300] + "…"
		}
		return text, fmt.Errorf("%s %s: %v: %s", program, strings.Join(args, " "), err, text)
	}
	return text, nil
}

// SystemdUnit is the systemd user unit: restarted only after a crash.
func SystemdUnit(exe string) string {
	return "[Unit]\nDescription=AgentDevice (agentdevice autostart on)\nAfter=network-online.target\n\n" +
		"[Service]\nExecStart=" + SystemdQuote(exe) + " run --no-tray\nRestart=on-abnormal\nRestartSec=10\n\n" +
		"[Install]\nWantedBy=default.target\n"
}

// SystemdQuote is systemd's command-line quoting: double quotes, \ and " escaped, % and $ doubled.
func SystemdQuote(arg string) string {
	var b strings.Builder
	b.WriteByte('"')
	for _, c := range arg {
		switch c {
		case '\\':
			b.WriteString(`\\`)
		case '"':
			b.WriteString(`\"`)
		case '%':
			b.WriteString("%%")
		case '$':
			b.WriteString("$$")
		default:
			b.WriteRune(c)
		}
	}
	b.WriteByte('"')
	return b.String()
}

func xmlEscape(s string) string {
	return strings.NewReplacer("&", "&amp;", "<", "&lt;", ">", "&gt;", `"`, "&quot;", "'", "&apos;").Replace(s)
}

// LaunchAgent is the macOS login item: at login, restarted only after a crash, its output in log.
func LaunchAgent(label, exe, log string) string {
	label, exe, log = xmlEscape(label), xmlEscape(exe), xmlEscape(log)
	return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>` + label + `</string>
  <key>ProgramArguments</key>
  <array><string>` + exe + `</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>Crashed</key><true/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>` + log + `</string>
  <key>StandardErrorPath</key><string>` + log + `</string>
</dict>
</plist>
`
}

// TaskXML is the Task Scheduler task: this user at their logon, their own rights (least privilege, interactive
// token, no stored password), no time limit, one instance.
func TaskXML(user, exe string) string {
	user, exe = xmlEscape(user), xmlEscape(exe)
	return `<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>AgentDevice (agentdevice autostart on)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled><UserId>` + user + `</UserId></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author"><UserId>` + user + `</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec><Command>` + exe + `</Command><Arguments>run</Arguments></Exec>
  </Actions>
</Task>
`
}

// UTF16LE is how Task Scheduler reads a task file (little endian with its byte-order mark).
func UTF16LE(text string) []byte {
	out := []byte{0xFF, 0xFE}
	for _, r := range text {
		if r >= 0x10000 {
			r -= 0x10000
			hi, lo := 0xD800+(r>>10), 0xDC00+(r&0x3FF)
			out = append(out, byte(hi), byte(hi>>8), byte(lo), byte(lo>>8))
			continue
		}
		out = append(out, byte(r), byte(r>>8))
	}
	return out
}
