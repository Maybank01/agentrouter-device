package app

import (
	"fmt"
	"io"
	"strings"
	"time"

	"github.com/agentrouter-top/AgentDevice/client/internal/sshd"
)

// ActivityLine is one line of the visible activity log, in plain Chinese: time, who (the conversation when the token
// names one), what, and how it ended.
func ActivityLine(a sshd.Activity) string {
	who := "云端"
	switch {
	case a.Run != "":
		who = "对话 " + a.Run
	case strings.HasPrefix(a.Sub, "acc_"):
		who = "账号 " + a.Sub
	case a.Sub != "":
		who = a.Sub
	}
	took := ""
	if a.Seconds >= 1 {
		took = fmt.Sprintf("，用时 %s", (time.Duration(a.Seconds * float64(time.Second))).Round(time.Second))
	}
	var what string
	switch a.Kind {
	case "exec":
		what = fmt.Sprintf("运行「%s」，退出码 %d%s", a.Command, a.Exit, took)
	case "terminal":
		if a.Command == "" {
			what = fmt.Sprintf("用了一次终端，退出码 %d%s", a.Exit, took)
		} else {
			what = fmt.Sprintf("在终端里运行「%s」，退出码 %d%s", a.Command, a.Exit, took)
		}
	case "sftp":
		what = "传了文件（SFTP）" + took
	case "forward":
		if a.Allowed {
			what = "连接本机服务 " + a.Target
		} else {
			what = "要连 " + a.Target + "，已拒绝（只允许本机服务）"
		}
	default:
		what = a.Kind
	}
	return a.Time.Local().Format("15:04:05") + "  " + who + "  " + what
}

// say writes one line to the visible console, if there is one.
func say(w io.Writer, line string) {
	if w != nil {
		fmt.Fprintln(w, line)
	}
}
