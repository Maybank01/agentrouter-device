package facts

import (
	"context"
	"fmt"
	"strconv"
	"strings"

	"golang.org/x/sys/windows/registry"
)

// osInfo reads the registry (readable by every account; WMI is not used: see the package comment). Windows 11 still
// calls itself "Windows 10" in ProductName; build 22000 and later is Windows 11.
func osInfo(context.Context) OS {
	o := OS{Family: "windows", Name: "Windows"}
	k, err := registry.OpenKey(registry.LOCAL_MACHINE, `SOFTWARE\Microsoft\Windows NT\CurrentVersion`, registry.QUERY_VALUE)
	if err != nil {
		return o
	}
	defer k.Close()
	name, _, _ := k.GetStringValue("ProductName")
	build, _, _ := k.GetStringValue("CurrentBuild")
	ubr, _, _ := k.GetIntegerValue("UBR")
	display, _, _ := k.GetStringValue("DisplayVersion")
	major, _, errMajor := k.GetIntegerValue("CurrentMajorVersionNumber")
	minor, _, _ := k.GetIntegerValue("CurrentMinorVersionNumber")
	if n, err := strconv.Atoi(build); err == nil && n >= 22000 {
		name = strings.Replace(name, "Windows 10", "Windows 11", 1)
	}
	if name != "" {
		o.Name = name
	}
	if display != "" {
		o.Name += " " + display
	}
	if errMajor != nil {
		major, minor = 10, 0
	}
	if build != "" {
		o.Version = fmt.Sprintf("%d.%d.%s", major, minor, build)
		o.Kernel = fmt.Sprintf("%d.%d.%s.%d", major, minor, build, ubr)
	}
	return o
}

func cpuModel(context.Context) string {
	k, err := registry.OpenKey(registry.LOCAL_MACHINE, `HARDWARE\DESCRIPTION\System\CentralProcessor\0`, registry.QUERY_VALUE)
	if err != nil {
		return ""
	}
	defer k.Close()
	s, _, _ := k.GetStringValue("ProcessorNameString")
	return s
}

// gpus reads the display adapters' driver keys (class {4d36e968-…}), readable by every account.
func gpus(context.Context) []GPU {
	const class = `SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}`
	k, err := registry.OpenKey(registry.LOCAL_MACHINE, class, registry.ENUMERATE_SUB_KEYS)
	if err != nil {
		return nil
	}
	defer k.Close()
	subs, _ := k.ReadSubKeyNames(-1)
	var out []GPU
	for _, s := range subs {
		if len(s) != 4 {
			continue
		}
		sk, err := registry.OpenKey(registry.LOCAL_MACHINE, class+`\`+s, registry.QUERY_VALUE)
		if err != nil {
			continue
		}
		name, _, _ := sk.GetStringValue("DriverDesc")
		vendor, _, _ := sk.GetStringValue("ProviderName")
		var memBytes uint64
		if v, _, err := sk.GetIntegerValue("HardwareInformation.qwMemorySize"); err == nil {
			memBytes = v
		} else if b, _, err := sk.GetBinaryValue("HardwareInformation.MemorySize"); err == nil && len(b) >= 4 {
			memBytes = uint64(b[0]) | uint64(b[1])<<8 | uint64(b[2])<<16 | uint64(b[3])<<24
		} else if v, _, err := sk.GetIntegerValue("HardwareInformation.MemorySize"); err == nil {
			memBytes = v
		}
		sk.Close()
		if name == "" || strings.Contains(name, "Basic Display") || strings.Contains(name, "Remote Display") {
			continue
		}
		out = append(out, GPU{Name: clip(name, 128), Vendor: clip(vendor, 64), MemoryBytes: memBytes})
	}
	return out
}
