package app

import (
	"strings"
	"testing"
	"time"

	"github.com/agentrouter-top/AgentDevice/client/internal/sshd"
)

func TestActivityLines(t *testing.T) {
	at := time.Date(2026, 11, 4, 15, 4, 5, 0, time.Local)
	cases := []struct {
		a    sshd.Activity
		want []string
	}{
		{sshd.Activity{Time: at, Kind: "exec", Sub: "svc_agenthost", Run: "cnv_x", Command: "git status", Exit: 0}, []string{"15:04:05", "对话 cnv_x", "运行「git status」", "退出码 0"}},
		{sshd.Activity{Time: at, Kind: "exec", Sub: "acc_7", Command: "make", Exit: 2, Seconds: 3.4}, []string{"账号 acc_7", "退出码 2", "用时 3s"}},
		{sshd.Activity{Time: at, Kind: "terminal", Exit: 0}, []string{"用了一次终端"}},
		{sshd.Activity{Time: at, Kind: "sftp"}, []string{"传了文件（SFTP）"}},
		{sshd.Activity{Time: at, Kind: "forward", Target: "127.0.0.1:3000", Allowed: true}, []string{"连接本机服务 127.0.0.1:3000"}},
		{sshd.Activity{Time: at, Kind: "forward", Target: "10.0.0.5:22"}, []string{"已拒绝"}},
	}
	for _, c := range cases {
		line := ActivityLine(c.a)
		for _, w := range c.want {
			if !strings.Contains(line, w) {
				t.Errorf("%q lacks %q", line, w)
			}
		}
	}
	if got := sshd.FirstLine("\n  cd x && make  \nsecond"); got != "cd x && make" {
		t.Errorf("FirstLine = %q", got)
	}
	if got := sshd.FirstLine(strings.Repeat("长", 100)); len([]rune(got)) != 81 {
		t.Errorf("FirstLine not cut to 80: %d runes", len([]rune(got)))
	}
}
