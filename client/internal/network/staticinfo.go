//go:build (linux && !android) || windows || (darwin && !ios)

package network

import (
	"sync"
	_ "unsafe" // go:linkname

	"github.com/netbirdio/netbird/client/system"
)

// NetBird gathers "static info" once per process before its first login (client/system static_info.go:
// serial number, product name, manufacturer, cloud and platform detection; on Windows also OS name and version).
// On a standard Windows account the WMI queries for these are denied only after a long wait, which delayed every
// start by about 20 s (`updateStaticInfo took 20.09s`, AgentDevice#221 P3); elsewhere the cloud probes can take up
// to 5 s on a computer that is not a cloud VM. None of it is used by this client. skipSlowStaticInfo fills that
// cache once with the values that cost nothing (no WMI, no network), so NetBird never runs the queries.
//
// This reaches NetBird's package variables by linkname: a NetBird upgrade that renames them fails the build (the
// pin is reviewed then), never the program. Upstream proposal: an option to skip the static info.

//go:linkname nbStaticOnce github.com/netbirdio/netbird/client/system.once
var nbStaticOnce sync.Once

//go:linkname nbStaticInfo github.com/netbirdio/netbird/client/system.staticInfo
var nbStaticInfo system.StaticInfo

func skipSlowStaticInfo() {
	nbStaticOnce.Do(func() { nbStaticInfo = cheapStaticInfo() })
}
