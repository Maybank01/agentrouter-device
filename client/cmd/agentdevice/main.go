// agentdevice is the device connector (AgentDevice, base layers v1): installed on a computer, it makes the computer
// an execution device the cloud reaches by SSH over the owner's private NetBird network. It only executes: no MCP, no
// tools of its own, no administrator rights, no inbound port on the computer's own network.
//
//	agentdevice [run] [--name 台式机] [--no-tray]   log in and enrol when needed, then stay online
//	agentdevice login [--name 台式机]               log in, enrol and join once (computers without a desktop)
//	agentdevice status                              enrolment, endpoints, autostart
//	agentdevice autostart on|off|status             start at login (per user, no admin)
//	agentdevice unbind                              remove this computer from the account
//	agentdevice facts                               print the facts compute/v1 gets
//	agentdevice version
//
// Every command takes --home <folder> (default: the per-user config folder). Flags carry only small fixed values and
// paths; tokens and keys live in files (R21).
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/signal"
	"runtime"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/skratchdot/open-golang/open"

	"github.com/agentrouter-top/AgentDevice/client/internal/app"
	"github.com/agentrouter-top/AgentDevice/client/internal/autostart"
	"github.com/agentrouter-top/AgentDevice/client/internal/compute"
	"github.com/agentrouter-top/AgentDevice/client/internal/facts"
	"github.com/agentrouter-top/AgentDevice/client/internal/identity"
	"github.com/agentrouter-top/AgentDevice/client/internal/sshd"
	"github.com/agentrouter-top/AgentDevice/client/internal/state"
	"github.com/agentrouter-top/AgentDevice/client/internal/tray"
	"github.com/agentrouter-top/AgentDevice/client/internal/winconsole"
)

// version is set at release (-ldflags "-X main.version=…").
var version = "dev"

// baseEnv is the environment before NetBird's embedded client sets its own variables (it calls os.Setenv).
var baseEnv = os.Environ()

func main() {
	if len(os.Args) > 1 && os.Args[1] == sshd.PrintEnvCommand {
		os.Exit(sshd.PrintEnv())
	}
	console := winconsole.Attach()
	cmd, args := "run", os.Args[1:]
	if len(args) > 0 && !strings.HasPrefix(args[0], "-") {
		cmd, args = args[0], args[1:]
	}
	var err error
	switch cmd {
	case "run":
		err = runCmd(args, console)
	case "login":
		err = loginCmd(args)
	case "status":
		err = statusCmd(args)
	case "autostart":
		err = autostartCmd(args)
	case "unbind":
		err = unbindCmd(args)
	case "facts":
		err = factsCmd(args)
	case "version", "--version":
		fmt.Println("agentdevice", version)
	case "help", "-h", "--help":
		fmt.Print(usage)
	default:
		err = fmt.Errorf("unknown command %q\n\n%s", cmd, usage)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "agentdevice:", err)
		os.Exit(1)
	}
}

const usage = `用法：
  agentdevice [run] [--name 名字] [--no-tray]   登录（第一次）并保持在线
  agentdevice login [--name 名字]               只登录并加入网络（没有桌面的电脑）
  agentdevice status                            看这台电脑的状态
  agentdevice autostart on|off|status           登录电脑后自动启动
  agentdevice unbind                            从账号里移除这台电脑
  agentdevice facts                             看上报的电脑信息
  agentdevice version
每个命令都可以加 --home <文件夹>。
`

// common parses --home and builds the app with its log.
func common(fs *flag.FlagSet, args []string) (*app.App, func(), error) {
	home := fs.String("home", "", "the client's folder (default: the per-user config folder)")
	if err := fs.Parse(args); err != nil {
		return nil, nil, err
	}
	dir := state.Dir(*home)
	if *home == "" {
		d, err := state.DefaultDir()
		if err != nil {
			return nil, nil, err
		}
		dir = d
	}
	if err := dir.Ensure(); err != nil {
		return nil, nil, err
	}
	cfg, err := dir.LoadConfig()
	if err != nil {
		return nil, nil, err
	}
	logFile, closeLog := openLog(dir.Path("agentdevice.log"))
	self, _ := os.Executable()
	a := &app.App{
		Dir: dir, Cfg: cfg, Version: version, Self: self, BaseEnv: baseEnv, LogFile: logFile,
		Log: slog.New(slog.NewTextHandler(logFile, &slog.HandlerOptions{Level: slog.LevelInfo})),
	}
	return a, closeLog, nil
}

// openLog appends to the log file, starting a new one (the old one kept as .1) past 10 MiB.
func openLog(path string) (io.Writer, func()) {
	if fi, err := os.Stat(path); err == nil && fi.Size() > 10<<20 {
		_ = os.Rename(path, path+".1")
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
	if err != nil {
		return os.Stderr, func() {}
	}
	return f, func() { _ = f.Close() }
}

func signalContext() (context.Context, context.CancelFunc) {
	return signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
}

func defaultName() string {
	if h, err := os.Hostname(); err == nil && h != "" {
		if len(h) > 64 {
			h = h[:64]
		}
		return h
	}
	return "我的电脑"
}

// removedText is what the person sees when the device's login was removed or revoked (owner wording).
const removedText = "此设备已被移除，请重新登录"

// terminalUI prints to the terminal or the console window.
type terminalUI struct{ w io.Writer }

func (u terminalUI) out() io.Writer {
	if u.w != nil {
		return u.w
	}
	return os.Stdout
}

func (u terminalUI) Prompt(p identity.Prompt) {
	addr := p.VerificationURIComplete
	if addr == "" {
		addr = p.VerificationURI
	}
	fmt.Fprintf(u.out(), "在浏览器里打开 %s ，确认代码 %s（%s 前有效）。\n", addr, p.UserCode, p.Expiry.Local().Format("15:04"))
	_ = open.Run(addr)
}

func (u terminalUI) Status(_ tray.State, text string) {
	fmt.Fprintln(u.out(), time.Now().Format("15:04:05")+"  "+text)
}

// bothUI shows everything in the tray and in the console window.
type bothUI []app.UI

func (b bothUI) Prompt(p identity.Prompt) {
	for i, u := range b {
		if i == 0 {
			u.Prompt(p)
			continue
		}
		if t, ok := u.(terminalUI); ok { // the browser is opened once
			addr := p.VerificationURIComplete
			if addr == "" {
				addr = p.VerificationURI
			}
			fmt.Fprintf(t.out(), "在浏览器里确认登录：%s ，代码 %s\n", addr, p.UserCode)
		}
	}
}

func (b bothUI) Status(s tray.State, text string) {
	for _, u := range b {
		u.Status(s, text)
	}
}

// trayUI shows everything in the tray.
type trayUI struct {
	h    *tray.Handle
	name string
}

func (u *trayUI) Prompt(p identity.Prompt) {
	addr := p.VerificationURIComplete
	if addr == "" {
		addr = p.VerificationURI
	}
	u.h.Set(tray.Connecting, u.name, "在浏览器里确认登录，代码 "+p.UserCode)
	_ = open.Run(addr)
}

func (u *trayUI) Status(s tray.State, text string) { u.h.Set(s, u.name, text) }

func runCmd(args []string, console bool) error {
	fs := flag.NewFlagSet("run", flag.ContinueOnError)
	name := fs.String("name", "", "the device name, at the first enrolment (default: the computer's name)")
	noTray := fs.Bool("no-tray", false, "run without the tray icon")
	// The one visible console window comes first, before any child process starts: on Windows every command the
	// Agents run attaches to it (no hidden and no extra windows, docs/development/AV-HYGIENE.md).
	consoleOut, consoleErr := winconsole.Resident("AgentRouter 设备")
	a, closeLog, err := common(fs, args)
	if err != nil {
		return err
	}
	defer closeLog()
	if consoleErr != nil {
		a.Log.Warn("no console window", "error", consoleErr.Error())
	}
	a.Console = consoleOut
	if err := a.Cfg.Validate(a.Dir.Path("config.json")); err != nil {
		return err
	}
	dev, err := a.Dir.LoadDevice()
	if err != nil {
		return err
	}
	shown := *name
	switch {
	case dev != nil:
		shown = dev.Name
	case shown == "":
		shown = defaultName()
	}
	if consoleOut != nil {
		fmt.Fprintln(consoleOut, "AgentRouter 设备 · "+shown)
		if runtime.GOOS == "windows" {
			fmt.Fprintln(consoleOut, "关闭此窗口即断开连接")
		} else {
			fmt.Fprintln(consoleOut, "按 Ctrl+C 断开连接")
		}
		fmt.Fprintln(consoleOut, "下面每一行是云端 Agent 在这台电脑上做的一件事。")
	}
	// Ctrl+C, and on Windows closing the window (CTRL_CLOSE_EVENT arrives as SIGTERM), disconnect cleanly.
	ctx, cancel := signalContext()
	defer cancel()
	work := func(ui app.UI) error {
		var join *compute.Join
		if dev == nil {
			j, err := a.Login(ctx, shown, ui)
			if err != nil {
				return err
			}
			join = j
		}
		return a.Run(ctx, join, ui)
	}
	if *noTray || !tray.Available() {
		err := work(terminalUI{w: consoleOut})
		if errors.Is(err, identity.ErrDeviceRemoved) {
			a.Log.Error("stopped", "error", err.Error())
			if consoleOut != nil {
				fmt.Fprintln(consoleOut, time.Now().Format("15:04:05")+"  "+removedText)
			}
			if runtime.GOOS == "windows" {
				<-ctx.Done() // the message stays in the window until the person closes it
				return nil
			}
			return errors.New(removedText)
		}
		if errors.Is(err, context.Canceled) {
			return nil
		}
		return err
	}
	var once sync.Once
	quit := func() { once.Do(func() { cancel(); tray.Quit() }) }
	errc := make(chan error, 1)
	tray.Run(func(h *tray.Handle) {
		ui := bothUI{&trayUI{h: h, name: shown}, terminalUI{w: consoleOut}}
		go func() {
			err := work(ui)
			if err != nil && ctx.Err() == nil {
				// Stay in the tray with the reason; the full error is in the log.
				a.Log.Error("stopped", "error", err.Error())
				if errors.Is(err, identity.ErrDeviceRemoved) {
					ui.Status(tray.Offline, removedText)
				} else {
					ui.Status(tray.Offline, "停了："+short(err.Error()))
				}
				if console {
					fmt.Fprintln(os.Stderr, "agentdevice:", err)
				}
				<-ctx.Done()
			}
			errc <- err
			quit()
		}()
	}, quit)
	cancel()
	err = <-errc
	if errors.Is(err, context.Canceled) {
		return nil
	}
	return err
}

func short(s string) string {
	if r := []rune(s); len(r) > 60 {
		return string(r[:60]) + "…"
	}
	return s
}

func loginCmd(args []string) error {
	fs := flag.NewFlagSet("login", flag.ContinueOnError)
	name := fs.String("name", "", "the device name (default: the computer's name)")
	a, closeLog, err := common(fs, args)
	if err != nil {
		return err
	}
	defer closeLog()
	if dev, _ := a.Dir.LoadDevice(); dev != nil {
		return fmt.Errorf("this computer is already enrolled as %s (%s); `agentdevice unbind` first to enrol it again", dev.Name, dev.ComputerID)
	}
	n := *name
	if n == "" {
		n = defaultName()
	}
	ctx, cancel := signalContext()
	defer cancel()
	join, err := a.Login(ctx, n, terminalUI{})
	if err != nil {
		return err
	}
	if err := a.JoinOnce(ctx, join); err != nil {
		return fmt.Errorf("enrolled, but the first join failed (run `agentdevice run` to retry with a new enrolment after `agentdevice unbind`): %w", err)
	}
	dev, _ := a.Dir.LoadDevice()
	fmt.Printf("这台电脑已加入：%s（%s）。保持在线：agentdevice run；登录后自动启动：agentdevice autostart on\n", dev.Name, dev.ComputerID)
	return nil
}

func statusCmd(args []string) error {
	fs := flag.NewFlagSet("status", flag.ContinueOnError)
	a, closeLog, err := common(fs, args)
	if err != nil {
		return err
	}
	defer closeLog()
	dev, err := a.Dir.LoadDevice()
	if err != nil {
		return err
	}
	fmt.Println("文件夹      ", a.Dir)
	fmt.Println("身份        ", orNone(a.Cfg.Issuer))
	fmt.Println("算力层      ", orNone(a.Cfg.ComputeURL))
	if dev == nil {
		fmt.Println("这台电脑    没有加入（agentdevice 或 agentdevice login）")
	} else {
		fmt.Printf("这台电脑    %s（%s，属于 %s，%s 加入）\n", dev.Name, dev.ComputerID, dev.Own, dev.EnrolledAt.Local().Format("2006-01-02 15:04"))
		fmt.Println("网络        ", dev.ManagementURL)
	}
	if signer, _, err := a.Dir.HostKey(); err == nil {
		fmt.Println("SSH 主机密钥", state.AuthorizedKey(signer))
	}
	st := autostart.Get()
	switch {
	case !st.On:
		fmt.Println("自动启动     没有打开（agentdevice autostart on）")
	case st.Running:
		fmt.Println("自动启动     已打开，正在运行（" + st.Where + "）")
	default:
		fmt.Println("自动启动     已打开，现在没有运行（" + st.Where + "）")
	}
	return nil
}

func orNone(s string) string {
	if s == "" {
		return "（没有配置）"
	}
	return s
}

func autostartCmd(args []string) error {
	sub := "status"
	if len(args) > 0 && !strings.HasPrefix(args[0], "-") {
		sub, args = args[0], args[1:]
	}
	fs := flag.NewFlagSet("autostart", flag.ContinueOnError)
	a, closeLog, err := common(fs, args)
	if err != nil {
		return err
	}
	defer closeLog()
	switch sub {
	case "on":
		if dev, _ := a.Dir.LoadDevice(); dev == nil {
			return errors.New("this computer is not enrolled yet: run `agentdevice login` first")
		}
		if fs.Lookup("home").Value.String() != "" {
			return errors.New("autostart uses the default folder only: run it without --home")
		}
		exe, err := os.Executable()
		if err != nil {
			return fmt.Errorf("autostart: where is this program? %w", err)
		}
		where, err := autostart.On(exe, string(a.Dir))
		if err != nil {
			return err
		}
		fmt.Println("已打开：登录这台电脑后自动启动（" + where + "），现在已经在运行。关掉：agentdevice autostart off")
	case "off":
		removed, err := autostart.Off()
		if err != nil {
			return err
		}
		if removed {
			fmt.Println("已关掉自动启动（这台电脑仍在账号里；agentdevice unbind 才会移除）。")
		} else {
			fmt.Println("本来就没有打开自动启动。")
		}
	case "status":
		st := autostart.Get()
		fmt.Printf("on=%v running=%v %s %s\n", st.On, st.Running, st.Where, st.Detail)
	default:
		return fmt.Errorf("autostart: on, off or status, not %q", sub)
	}
	return nil
}

func unbindCmd(args []string) error {
	fs := flag.NewFlagSet("unbind", flag.ContinueOnError)
	a, closeLog, err := common(fs, args)
	if err != nil {
		return err
	}
	defer closeLog()
	ctx, cancel := signalContext()
	defer cancel()
	if err := a.Unbind(ctx); err != nil {
		return err
	}
	_, _ = autostart.Off()
	fmt.Println("已从账号里移除这台电脑，自动启动也关掉了。")
	return nil
}

func factsCmd(args []string) error {
	fs := flag.NewFlagSet("facts", flag.ContinueOnError)
	if err := fs.Parse(args); err != nil {
		return err
	}
	ctx, cancel := signalContext()
	defer cancel()
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	enc.SetEscapeHTML(false)
	return enc.Encode(facts.Collect(ctx))
}
