//go:build (linux && !android) || (darwin && !ios)

package network

import "github.com/netbirdio/netbird/client/system"

// cheapStaticInfo: on Linux and macOS NetBird reads the OS facts elsewhere; the hardware serials and the cloud
// detection stay empty.
func cheapStaticInfo() system.StaticInfo { return system.StaticInfo{} }
