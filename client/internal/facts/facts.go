// Package facts says what this computer is, for compute/v1 (computer.schema.json $defs/facts): os, arch, shell,
// cpu, memory, gpu, software — as the system says them, nothing smoothed over. Every probe is bounded in time and
// none needs administrator rights or WMI (on a standard Windows account WMI's system queries are denied slowly, the
// cause of NetBird's 20 s startup delay; see internal/network). A probe that fails leaves its part empty.
package facts

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/agentrouter-top/AgentDevice/client/internal/proc"
	"github.com/shirou/gopsutil/v4/mem"
)

// Facts mirrors compute/v1 computer.schema.json $defs/facts.
type Facts struct {
	OS         OS         `json:"os"`
	Arch       string     `json:"arch"`
	Shell      Shell      `json:"shell"`
	CPU        CPU        `json:"cpu"`
	Memory     Memory     `json:"memory"`
	GPU        []GPU      `json:"gpu"`
	Software   []Software `json:"software"`
	ObservedAt time.Time  `json:"observedAt"`
}

type OS struct {
	Family  string `json:"family"`
	Name    string `json:"name"`
	Version string `json:"version"`
	Kernel  string `json:"kernel,omitempty"`
}

type Shell struct {
	Name    string `json:"name"`
	Path    string `json:"path"`
	Version string `json:"version,omitempty"`
}

type CPU struct {
	Model string  `json:"model,omitempty"`
	Cores float64 `json:"cores"`
}

type Memory struct {
	TotalBytes uint64 `json:"totalBytes"`
}

type GPU struct {
	Name        string `json:"name"`
	Vendor      string `json:"vendor,omitempty"`
	MemoryBytes uint64 `json:"memoryBytes,omitempty"`
}

type Software struct {
	Name    string `json:"name"`
	Version string `json:"version,omitempty"`
	Path    string `json:"path,omitempty"`
}

// Collect gathers the facts; it takes at most about ten seconds and never fails.
func Collect(ctx context.Context) Facts {
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	f := Facts{Arch: runtime.GOARCH, CPU: CPU{Cores: float64(runtime.NumCPU())}, GPU: []GPU{}, Software: []Software{}}
	var wg sync.WaitGroup
	wg.Go(func() { f.OS = osInfo(ctx) })
	wg.Go(func() {
		sh := UserShell()
		f.Shell = Shell{Name: sh.Name, Path: sh.Path, Version: shellVersion(ctx, sh)}
	})
	wg.Go(func() { f.CPU.Model = clip(cpuModel(ctx), 128) })
	wg.Go(func() {
		if vm, err := mem.VirtualMemoryWithContext(ctx); err == nil {
			f.Memory.TotalBytes = vm.Total
		}
	})
	wg.Go(func() {
		if g := gpus(ctx); g != nil {
			f.GPU = g
		}
	})
	wg.Go(func() { f.Software = software(ctx) })
	wg.Wait()
	f.OS.Name, f.OS.Version, f.OS.Kernel = clip(f.OS.Name, 128), clip(f.OS.Version, 64), clip(f.OS.Kernel, 128)
	if f.OS.Name == "" {
		f.OS.Name = runtime.GOOS
	}
	if f.OS.Version == "" {
		f.OS.Version = "unknown"
	}
	f.ObservedAt = time.Now().UTC().Truncate(time.Second)
	return f
}

// UserShellInfo is the shell an SSH session of this user runs commands in.
type UserShellInfo struct {
	Name string // bash, zsh, sh, powershell, …
	Path string
	// POSIX is true when the shell takes sh syntax (the exec runner is sh syntax).
	POSIX bool
}

var posixShells = map[string]bool{"sh": true, "bash": true, "zsh": true, "dash": true, "ksh": true, "ash": true, "mksh": true, "yash": true}

// UserShell: Windows PowerShell on Windows; elsewhere the account's login shell when it takes sh syntax, else
// /bin/sh (a fish or nu account runs commands in /bin/sh and says so in the facts).
func UserShell() UserShellInfo {
	if runtime.GOOS == "windows" {
		root := os.Getenv("SystemRoot")
		if root == "" {
			root = `C:\Windows`
		}
		return UserShellInfo{Name: "powershell", Path: filepath.Join(root, "System32", "WindowsPowerShell", "v1.0", "powershell.exe")}
	}
	for _, candidate := range []string{os.Getenv("SHELL"), loginShell()} {
		if candidate == "" {
			continue
		}
		name := filepath.Base(candidate)
		if posixShells[name] {
			if _, err := os.Stat(candidate); err == nil {
				return UserShellInfo{Name: name, Path: candidate, POSIX: true}
			}
		}
	}
	return UserShellInfo{Name: "sh", Path: "/bin/sh", POSIX: true}
}

// loginShell reads the account's shell from /etc/passwd (Linux; macOS keeps it in Directory Services and sets SHELL
// for launchd agents, so SHELL is enough there).
func loginShell() string {
	raw, err := os.ReadFile("/etc/passwd")
	if err != nil {
		return ""
	}
	uid := os.Getuid()
	for _, line := range strings.Split(string(raw), "\n") {
		p := strings.Split(line, ":")
		if len(p) >= 7 && p[2] == strconv.Itoa(uid) {
			return strings.TrimSpace(p[6])
		}
	}
	return ""
}

func shellVersion(ctx context.Context, sh UserShellInfo) string {
	switch sh.Name {
	case "powershell":
		return clip(firstLine(output(ctx, 5*time.Second, sh.Path, "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "$PSVersionTable.PSVersion.ToString()")), 64)
	case "bash", "zsh", "ksh", "mksh", "yash":
		return versionIn(output(ctx, 3*time.Second, sh.Path, "--version"))
	}
	return ""
}

// output runs a fixed probe (small fixed flags only) and returns its stdout, "" on any failure.
func output(ctx context.Context, timeout time.Duration, name string, args ...string) string {
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, name, args...)
	proc.Prepare(cmd)
	cmd.Stdin = nil
	out, err := cmd.Output()
	if err != nil && len(out) == 0 {
		return ""
	}
	return string(out)
}

var versionPattern = regexp.MustCompile(`\d+(?:\.\d+)+(?:[-+.][0-9A-Za-z]+)*`)

func versionIn(text string) string { return clip(versionPattern.FindString(text), 64) }

func firstLine(s string) string {
	s = strings.TrimSpace(s)
	if i := strings.IndexAny(s, "\r\n"); i >= 0 {
		s = s[:i]
	}
	return strings.TrimSpace(s)
}

func clip(s string, n int) string {
	s = strings.TrimSpace(s)
	if len([]rune(s)) > n {
		return string([]rune(s)[:n])
	}
	return s
}

// probes are the programs looked for on the PATH: toolchains, package managers, editors, runtimes. The version is
// what `<program> <flag>` prints, first match of a dotted number.
var probes = []struct{ name, flag string }{
	{"git", "--version"}, {"gh", "--version"}, {"node", "--version"}, {"npm", "--version"}, {"pnpm", "--version"},
	{"yarn", "--version"}, {"bun", "--version"}, {"deno", "--version"}, {"python3", "--version"},
	{"python", "--version"}, {"pip3", "--version"}, {"pip", "--version"}, {"uv", "--version"},
	{"go", "version"}, {"rustc", "--version"}, {"cargo", "--version"}, {"java", "-version"},
	{"dotnet", "--version"}, {"ruby", "--version"}, {"php", "--version"}, {"perl", "--version"},
	{"gcc", "--version"}, {"clang", "--version"}, {"make", "--version"}, {"cmake", "--version"},
	{"docker", "--version"}, {"podman", "--version"}, {"kubectl", "version"}, {"mise", "--version"},
	{"brew", "--version"}, {"winget", "--version"}, {"choco", "--version"}, {"scoop", "--version"},
	{"apt", "--version"}, {"dnf", "--version"}, {"pacman", "--version"}, {"ffmpeg", "-version"},
	{"code", "--version"}, {"cursor", "--version"}, {"pwsh", "--version"}, {"curl", "--version"},
	{"rg", "--version"}, {"jq", "--version"}, {"sqlite3", "--version"}, {"psql", "--version"},
	{"ollama", "--version"}, {"claude", "--version"}, {"codex", "--version"},
}

func software(ctx context.Context) []Software {
	var mu sync.Mutex
	var out []Software
	var wg sync.WaitGroup
	sem := make(chan struct{}, 8)
	for _, p := range probes {
		path, err := exec.LookPath(p.name)
		if err != nil {
			continue
		}
		wg.Go(func() {
			sem <- struct{}{}
			defer func() { <-sem }()
			// Windows' python stub in WindowsApps prints an install hint, not a version: then the version stays empty.
			text := output(ctx, 3*time.Second, path, p.flag)
			if text == "" && p.flag == "-version" {
				text = stderrOutput(ctx, 3*time.Second, path, p.flag)
			}
			mu.Lock()
			out = append(out, Software{Name: p.name, Version: versionIn(text), Path: clip(path, 1024)})
			mu.Unlock()
		})
	}
	wg.Wait()
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	if out == nil {
		out = []Software{}
	}
	return out
}

// stderrOutput is output for programs that print their version on stderr (java -version, ffmpeg).
func stderrOutput(ctx context.Context, timeout time.Duration, name string, args ...string) string {
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, name, args...)
	proc.Prepare(cmd)
	out, _ := cmd.CombinedOutput()
	return string(out)
}
