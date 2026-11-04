package network

import (
	"fmt"
	"strconv"

	"github.com/netbirdio/netbird/client/system"
	"golang.org/x/sys/windows/registry"
)

// cheapStaticInfo reads what NetBird would have read through WMI from the registry instead (readable by every
// account): OS name "Windows", version "10" or "11", build major.minor.build.UBR.
func cheapStaticInfo() system.StaticInfo {
	si := system.StaticInfo{OSName: "Windows", OSVersion: "10", BuildVersion: "0.0.0.0"}
	k, err := registry.OpenKey(registry.LOCAL_MACHINE, `SOFTWARE\Microsoft\Windows NT\CurrentVersion`, registry.QUERY_VALUE)
	if err != nil {
		return si
	}
	defer k.Close()
	major, _, _ := k.GetIntegerValue("CurrentMajorVersionNumber")
	minor, _, _ := k.GetIntegerValue("CurrentMinorVersionNumber")
	build, _, _ := k.GetStringValue("CurrentBuildNumber")
	ubr, _, _ := k.GetIntegerValue("UBR")
	if n, err := strconv.Atoi(build); err == nil && n >= 22000 {
		si.OSVersion = "11"
	}
	si.BuildVersion = fmt.Sprintf("%d.%d.%s.%d", major, minor, build, ubr)
	return si
}
