//go:build !linux && !darwin && !windows

package facts

import (
	"context"
	"runtime"
)

func osInfo(context.Context) OS {
	family := "other"
	if runtime.GOOS == "freebsd" {
		family = "freebsd"
	}
	return OS{Family: family, Name: runtime.GOOS}
}

func cpuModel(context.Context) string { return "" }
func gpus(context.Context) []GPU      { return nil }
