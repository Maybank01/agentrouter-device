//! Pixels for the screen tools: scale a captured BGRA frame down, black out private areas, draw the
//! coordinate grid, and encode JPEG (one screenshot) or GIF (a burst). Pure Rust, no platform calls.

/// A captured frame: BGRA, top-down rows, no padding.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub bgra: Vec<u8>,
}

/// An RGB image (what the encoders take).
pub struct Rgb {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// The size a frame is sent at: the long edge at most `max_edge`, never upscaled.
pub fn fit(width: usize, height: usize, max_edge: usize) -> (usize, usize) {
    let long = width.max(height).max(1);
    if long <= max_edge {
        return (width.max(1), height.max(1));
    }
    let scale = max_edge as f64 / long as f64;
    (
        ((width as f64 * scale).round() as usize).max(1),
        ((height as f64 * scale).round() as usize).max(1),
    )
}

impl Frame {
    /// Paint a rectangle (frame pixels) black: private windows never leave the device.
    pub fn black_out(&mut self, x: i64, y: i64, w: i64, h: i64) {
        let x0 = x.clamp(0, self.width as i64) as usize;
        let y0 = y.clamp(0, self.height as i64) as usize;
        let x1 = (x + w).clamp(0, self.width as i64) as usize;
        let y1 = (y + h).clamp(0, self.height as i64) as usize;
        for row in y0..y1 {
            let start = (row * self.width + x0) * 4;
            let end = (row * self.width + x1) * 4;
            for px in self.bgra[start..end].as_chunks_mut::<4>().0 {
                px[0] = 32;
                px[1] = 32;
                px[2] = 32;
            }
        }
    }

    /// Area-average downscale to `width` × `height`, as RGB.
    pub fn scaled(&self, width: usize, height: usize) -> Rgb {
        let (sw, sh) = (self.width.max(1), self.height.max(1));
        let mut data = vec![0u8; width * height * 3];
        let xs: Vec<(usize, usize)> = (0..width)
            .map(|dx| {
                let a = dx * sw / width;
                let b = ((dx + 1) * sw / width).max(a + 1).min(sw);
                (a, b)
            })
            .collect();
        for dy in 0..height {
            let y0 = dy * sh / height;
            let y1 = ((dy + 1) * sh / height).max(y0 + 1).min(sh);
            for (dx, &(x0, x1)) in xs.iter().enumerate() {
                let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
                for sy in y0..y1 {
                    let row = sy * sw;
                    for sx in x0..x1 {
                        let i = (row + sx) * 4;
                        b += self.bgra[i] as u32;
                        g += self.bgra[i + 1] as u32;
                        r += self.bgra[i + 2] as u32;
                        n += 1;
                    }
                }
                let o = (dy * width + dx) * 3;
                let n = n.max(1);
                data[o] = (r / n) as u8;
                data[o + 1] = (g / n) as u8;
                data[o + 2] = (b / n) as u8;
            }
        }
        Rgb {
            width,
            height,
            data,
        }
    }
}

/// 3×5 digits for the grid labels.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b010, 0b010, 0b010],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

impl Rgb {
    fn blend(&mut self, x: usize, y: usize, color: [u8; 3], alpha: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let o = (y * self.width + x) * 3;
        for (c, &value) in color.iter().enumerate() {
            let old = self.data[o + c] as u32;
            self.data[o + c] = ((old * (255 - alpha) + value as u32 * alpha) / 255) as u8;
        }
    }

    fn label(&mut self, x: usize, y: usize, value: usize) {
        let text = value.to_string();
        let (scale, w) = (2, text.len() * 8);
        for dy in 0..12 {
            for dx in 0..w + 2 {
                self.blend(x + dx, y + dy, [255, 255, 255], 200);
            }
        }
        for (i, ch) in text.bytes().enumerate() {
            let glyph = DIGITS[(ch - b'0') as usize];
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..3 {
                    if bits & (0b100 >> col) != 0 {
                        for sy in 0..scale {
                            for sx in 0..scale {
                                self.blend(
                                    x + 1 + i * 8 + col * scale + sx,
                                    y + 1 + row * scale + sy,
                                    [200, 0, 0],
                                    255,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// A thin line every `step` pixels with its coordinate, to help a model aim (LINKED-DEVICES.md §20.1).
    pub fn grid(&mut self, step: usize) {
        let step = step.max(20);
        let color = [255, 0, 64];
        for x in (step..self.width).step_by(step) {
            for y in 0..self.height {
                self.blend(x, y, color, 110);
            }
        }
        for y in (step..self.height).step_by(step) {
            for x in 0..self.width {
                self.blend(x, y, color, 110);
            }
        }
        for x in (step..self.width).step_by(step) {
            self.label(x + 2, 2, x);
        }
        for y in (step..self.height).step_by(step) {
            self.label(2, y + 2, y);
        }
    }

    pub fn jpeg(&self, quality: u8) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let encoder = jpeg_encoder::Encoder::new(&mut out, quality.clamp(30, 95));
        encoder
            .encode(
                &self.data,
                self.width as u16,
                self.height as u16,
                jpeg_encoder::ColorType::Rgb,
            )
            .map_err(|e| format!("jpeg: {e}"))?;
        Ok(out)
    }
}

/// An animated GIF of same-sized frames, `delay_ms` apart, looping.
pub fn gif(frames: &[Rgb], delay_ms: u32) -> Result<Vec<u8>, String> {
    let Some(first) = frames.first() else {
        return Err("no frames".into());
    };
    let (w, h) = (first.width as u16, first.height as u16);
    let mut out = Vec::new();
    {
        let mut encoder =
            gif::Encoder::new(&mut out, w, h, &[]).map_err(|e| format!("gif: {e}"))?;
        encoder
            .set_repeat(gif::Repeat::Infinite)
            .map_err(|e| format!("gif: {e}"))?;
        for f in frames {
            if f.width != first.width || f.height != first.height {
                return Err("frames differ in size".into());
            }
            let mut frame = gif::Frame::from_rgb_speed(w, h, &f.data, 20);
            frame.delay = (delay_ms / 10).clamp(2, 65_535) as u16;
            encoder
                .write_frame(&frame)
                .map_err(|e| format!("gif: {e}"))?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, bgr: [u8; 3]) -> Frame {
        let mut bgra = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            bgra.extend_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
        }
        Frame {
            width: w,
            height: h,
            bgra,
        }
    }

    #[test]
    fn fit_keeps_aspect_and_never_upscales() {
        assert_eq!(fit(2560, 1440, 1280), (1280, 720));
        assert_eq!(fit(800, 600, 1280), (800, 600));
        assert_eq!(fit(1000, 3000, 1500), (500, 1500));
    }

    #[test]
    fn scale_averages_and_swaps_channels() {
        let f = solid(40, 20, [10, 20, 30]);
        let rgb = f.scaled(4, 2);
        assert_eq!(&rgb.data[..3], &[30, 20, 10]);
        assert_eq!(rgb.data.len(), 4 * 2 * 3);
    }

    #[test]
    fn black_out_clips_to_the_frame() {
        let mut f = solid(10, 10, [200, 200, 200]);
        f.black_out(-5, 5, 100, 100);
        assert_eq!(f.bgra[0], 200);
        assert_eq!(f.bgra[(5 * 10) * 4], 32);
    }

    #[test]
    fn encoders_produce_their_formats() {
        let mut rgb = solid(320, 200, [0, 128, 255]).scaled(320, 200);
        rgb.grid(100);
        let jpeg = rgb.jpeg(80).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        let gif = gif(&[rgb], 200).unwrap();
        assert_eq!(&gif[..6], b"GIF89a");
    }
}
