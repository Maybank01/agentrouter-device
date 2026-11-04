//go:build !linux && !darwin && !windows

package autostart

import "errors"

var errUnsupported = errors.New("autostart: not supported on this system; start `agentdevice run` from your own init system")

func On(string, string) (string, error) { return "", errUnsupported }
func Off() (bool, error)                { return false, errUnsupported }
func Get() Status                       { return Status{} }
