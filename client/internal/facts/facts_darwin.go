package facts

import (
	"context"
	"encoding/json"
	"strconv"
	"strings"
	"time"

	"golang.org/x/sys/unix"
)

func osInfo(ctx context.Context) OS {
	o := OS{Family: "macos"}
	o.Version = strings.TrimSpace(output(ctx, 5*time.Second, "/usr/bin/sw_vers", "-productVersion"))
	if o.Version != "" {
		o.Name = "macOS " + o.Version
	}
	if build := strings.TrimSpace(output(ctx, 5*time.Second, "/usr/bin/sw_vers", "-buildVersion")); build != "" && o.Version != "" {
		o.Name += " (" + build + ")"
	}
	var u unix.Utsname
	if unix.Uname(&u) == nil {
		o.Kernel = unix.ByteSliceToString(u.Sysname[:]) + " " + unix.ByteSliceToString(u.Release[:])
	}
	return o
}

func cpuModel(context.Context) string {
	s, _ := unix.Sysctl("machdep.cpu.brand_string")
	return s
}

func gpus(ctx context.Context) []GPU {
	text := output(ctx, 10*time.Second, "/usr/sbin/system_profiler", "SPDisplaysDataType", "-json")
	var doc struct {
		Displays []map[string]any `json:"SPDisplaysDataType"`
	}
	if json.Unmarshal([]byte(text), &doc) != nil {
		return nil
	}
	var out []GPU
	for _, d := range doc.Displays {
		g := GPU{Name: str(d["sppci_model"]), Vendor: strings.TrimPrefix(str(d["spdisplays_vendor"]), "sppci_vendor_")}
		for _, k := range []string{"spdisplays_vram", "spdisplays_vram_shared"} {
			if n := bytesOf(str(d[k])); n > 0 {
				g.MemoryBytes = n
				break
			}
		}
		if g.Name == "" {
			g.Name = str(d["_name"])
		}
		if g.Name != "" {
			g.Name, g.Vendor = clip(g.Name, 128), clip(g.Vendor, 64)
			out = append(out, g)
		}
	}
	return out
}

func str(v any) string {
	s, _ := v.(string)
	return s
}

// bytesOf reads "8 GB" / "1536 MB".
func bytesOf(s string) uint64 {
	num, unit, ok := strings.Cut(strings.TrimSpace(s), " ")
	if !ok {
		return 0
	}
	n, err := strconv.ParseUint(num, 10, 64)
	if err != nil {
		return 0
	}
	switch strings.ToUpper(unit) {
	case "GB":
		return n << 30
	case "MB":
		return n << 20
	}
	return 0
}
