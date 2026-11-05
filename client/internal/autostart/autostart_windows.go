package autostart

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/sys/windows"
)

// userSID is the account this process runs as, by SID (a name from the environment is not the account's name
// everywhere: a workgroup PC's OpenSSH session has USERDOMAIN=WORKGROUP, Rust hostd #192).
func userSID() (string, error) {
	u, err := windows.GetCurrentProcessToken().GetTokenUser()
	if err != nil {
		return "", fmt.Errorf("autostart: token user: %w", err)
	}
	return u.User.Sid.String(), nil
}

// On registers the logon task and, with start, starts it now. dir is the client's folder; the definition goes
// through a file there, not the command line.
func On(exe, dir string, start bool) (string, error) {
	sid, err := userSID()
	if err != nil {
		return "", err
	}
	file := filepath.Join(dir, "agentdevice-task.xml")
	if err := os.WriteFile(file, UTF16LE(TaskXML(sid, exe)), 0o600); err != nil {
		return "", fmt.Errorf("autostart: write %s: %w", file, err)
	}
	_, err = tool("schtasks", "/Create", "/TN", taskName, "/XML", file, "/F")
	_ = os.Remove(file)
	if err != nil {
		return "", fmt.Errorf("autostart: %w", err)
	}
	if !start {
		return "Task Scheduler task " + taskName + " (at logon)", nil
	}
	_, _ = tool("schtasks", "/End", "/TN", taskName)
	if _, err := tool("schtasks", "/Run", "/TN", taskName); err != nil {
		return "", fmt.Errorf("autostart: %w", err)
	}
	return "Task Scheduler task " + taskName + " (at logon)", nil
}

// Off ends and deletes the task; false when there was none.
func Off() (bool, error) {
	if _, err := tool("schtasks", "/Query", "/TN", taskName); err != nil {
		return false, nil
	}
	_, _ = tool("schtasks", "/End", "/TN", taskName)
	if _, err := tool("schtasks", "/Delete", "/TN", taskName, "/F"); err != nil {
		return false, fmt.Errorf("autostart: %w", err)
	}
	return true, nil
}

// Get reports the task (the CSV's last field is its status, in the system's language).
func Get() Status {
	out, err := tool("schtasks", "/Query", "/TN", taskName, "/FO", "CSV", "/NH")
	if err != nil {
		return Status{Where: "Task Scheduler task " + taskName}
	}
	return Status{On: true, Running: strings.Contains(out, `"Running"`) || strings.Contains(out, `"正在运行"`), Where: "Task Scheduler task " + taskName}
}
