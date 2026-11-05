package instance

import (
	"runtime"
	"strings"
)

func isUnsupported(err error) bool {
	return runtime.GOOS != "linux" && runtime.GOOS != "windows" && strings.Contains(err.Error(), "not supported")
}
