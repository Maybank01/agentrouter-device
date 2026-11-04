//go:build !windows

package winconsole

import (
	"io"
	"os"
)

func attach() bool { return true }

func resident(string) (io.Writer, error) { return newLineWriter(os.Stdout), nil }
