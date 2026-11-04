//go:build !((linux && !android) || windows || (darwin && !ios))

package network

func skipSlowStaticInfo() {}
