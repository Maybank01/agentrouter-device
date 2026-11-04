// Package tray is the client's minimal tray icon (fyne.io/systray, BSD-3): the device name, its state and 退出.
// Linux uses the StatusNotifierItem D-Bus protocol (no cgo); macOS needs cgo (Cocoa); without either the client
// runs without a tray.
package tray

import (
	"bytes"
	"encoding/binary"
	"image"
	"image/color"
	"image/png"
	"math"
)

// iconPNG draws the icon: a filled circle in the given colour on transparency.
func iconPNG(c color.NRGBA) []byte {
	const size = 32
	img := image.NewNRGBA(image.Rect(0, 0, size, size))
	for y := 0; y < size; y++ {
		for x := 0; x < size; x++ {
			d := math.Hypot(float64(x)-15.5, float64(y)-15.5)
			switch {
			case d <= 13:
				img.SetNRGBA(x, y, c)
			case d <= 14:
				cc := c
				cc.A = uint8(float64(c.A) * (14 - d))
				img.SetNRGBA(x, y, cc)
			}
		}
	}
	var b bytes.Buffer
	_ = png.Encode(&b, img)
	return b.Bytes()
}

// icoOf wraps a PNG in an ICO container (Windows Vista and later read PNG entries), which Windows' tray needs.
func icoOf(p []byte) []byte {
	var b bytes.Buffer
	_ = binary.Write(&b, binary.LittleEndian, [3]uint16{0, 1, 1})
	_ = binary.Write(&b, binary.LittleEndian, struct {
		W, H, Colors, Reserved uint8
		Planes, BPP            uint16
		Size, Offset           uint32
	}{32, 32, 0, 0, 1, 32, uint32(len(p)), 22})
	b.Write(p)
	return b.Bytes()
}

var (
	colorOnline     = color.NRGBA{R: 0x2e, G: 0xa0, B: 0x43, A: 0xff}
	colorConnecting = color.NRGBA{R: 0xd9, G: 0x9a, B: 0x1e, A: 0xff}
	colorOffline    = color.NRGBA{R: 0x8a, G: 0x8a, B: 0x8a, A: 0xff}
)
