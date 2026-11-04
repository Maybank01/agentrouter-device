package facts

import (
	"bufio"
	"context"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"golang.org/x/sys/unix"
)

func osInfo(context.Context) OS {
	o := OS{Family: "linux"}
	if raw, err := os.ReadFile("/etc/os-release"); err == nil {
		kv := map[string]string{}
		for _, line := range strings.Split(string(raw), "\n") {
			if k, v, ok := strings.Cut(line, "="); ok {
				kv[k] = strings.Trim(v, `"'`)
			}
		}
		o.Name = kv["PRETTY_NAME"]
		if o.Name == "" {
			o.Name = kv["NAME"]
		}
		o.Version = kv["VERSION_ID"]
	}
	var u unix.Utsname
	if unix.Uname(&u) == nil {
		o.Kernel = unix.ByteSliceToString(u.Release[:])
		if o.Version == "" {
			o.Version = o.Kernel
		}
	}
	return o
}

func cpuModel(context.Context) string {
	f, err := os.Open("/proc/cpuinfo")
	if err != nil {
		return ""
	}
	defer f.Close()
	s := bufio.NewScanner(f)
	for s.Scan() {
		k, v, ok := strings.Cut(s.Text(), ":")
		if ok && (strings.TrimSpace(k) == "model name" || strings.TrimSpace(k) == "Model") {
			return strings.TrimSpace(v)
		}
	}
	return ""
}

// gpus: NVIDIA cards through nvidia-smi; others by their PCI display class through lspci; both only when installed.
func gpus(ctx context.Context) []GPU {
	var out []GPU
	if text := output(ctx, 5*time.Second, "nvidia-smi", "--query-gpu=name,memory.total", "--format=csv,noheader,nounits"); text != "" {
		for _, line := range strings.Split(strings.TrimSpace(text), "\n") {
			name, memMiB, _ := strings.Cut(line, ",")
			g := GPU{Name: clip(name, 128), Vendor: "NVIDIA"}
			if n, err := strconv.ParseUint(strings.TrimSpace(memMiB), 10, 64); err == nil {
				g.MemoryBytes = n << 20
			}
			if g.Name != "" {
				out = append(out, g)
			}
		}
		return out
	}
	if text := output(ctx, 5*time.Second, "lspci", "-mm"); text != "" {
		for _, line := range strings.Split(text, "\n") {
			f := quoted(line)
			if len(f) >= 4 && (strings.Contains(f[1], "VGA") || strings.Contains(f[1], "3D") || strings.Contains(f[1], "Display")) {
				out = append(out, GPU{Name: clip(f[3], 128), Vendor: clip(f[2], 64)})
			}
		}
		return out
	}
	// Without either tool: the DRM cards, named by their PCI vendor.
	cards, _ := filepath.Glob("/sys/class/drm/card[0-9]/device/vendor")
	for _, c := range cards {
		raw, _ := os.ReadFile(c)
		if v, ok := pciVendors[strings.TrimSpace(string(raw))]; ok {
			out = append(out, GPU{Name: v + " GPU", Vendor: v})
		}
	}
	return out
}

var pciVendors = map[string]string{"0x10de": "NVIDIA", "0x1002": "AMD", "0x8086": "Intel"}

// quoted splits an lspci -mm line into its fields (quoted strings and bare words).
func quoted(line string) []string {
	var out []string
	for line = strings.TrimSpace(line); line != ""; line = strings.TrimSpace(line) {
		if line[0] == '"' {
			end := strings.IndexByte(line[1:], '"')
			if end < 0 {
				return append(out, line[1:])
			}
			out = append(out, line[1:end+1])
			line = line[end+2:]
			continue
		}
		word, rest, _ := strings.Cut(line, " ")
		out = append(out, word)
		line = rest
	}
	return out
}
