//! Tray icon pixmaps drawn with plain pixel math: a battery ring, or the level
//! as digits from a built-in 5x7 bitmap font. No font or image crates, so the
//! tray does not depend on a font file or a text shaping stack.

use std::f32::consts::{PI, TAU};

/// Width and height of every icon, in pixels.
pub const ICON_SIZE: u32 = 64;

const RING_INNER_RADIUS: f32 = 22.0;
const RING_OUTER_RADIUS: f32 = 28.0;

const GREY: Rgba = [128, 128, 128, 255];
const GREEN: Rgba = [0, 255, 0, 255];
const WHITE: Rgba = [255, 255, 255, 255];

/// Glyph cell of the bitmap font, in font pixels.
const GLYPH_WIDTH: u32 = 5;
const GLYPH_HEIGHT: u32 = 7;
/// Room the digits may take inside the icon, in icon pixels.
const TEXT_MAX_WIDTH: u32 = 60;
const TEXT_MAX_HEIGHT: u32 = 48;

/// Lightning bolt drawn over the ring while a bud charges, as a polygon in
/// icon pixel coordinates.
const BOLT: [(f32, f32); 7] = [
    (38.0, 6.0),
    (18.0, 36.0),
    (30.0, 36.0),
    (24.0, 58.0),
    (46.0, 26.0),
    (34.0, 26.0),
    (40.0, 6.0),
];

type Rgba = [u8; 4];

/// A square icon as ARGB32 in network byte order (A, R, G, B per pixel, rows
/// top to bottom), the layout StatusNotifierItem pixmaps use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixmap {
    pub width: u32,
    pub height: u32,
    pub argb: Vec<u8>,
}

/// Draw the tray icon. `level` None draws "-" in text mode and a ring with no
/// fill otherwise, so an unknown battery never looks like an empty one.
pub fn render(level: Option<u8>, text_mode: bool, charging: bool) -> Pixmap {
    let mut canvas = Canvas::new(ICON_SIZE);
    if text_mode {
        let text = level.map_or_else(|| "-".to_string(), |l| l.min(100).to_string());
        let color = if charging { GREEN } else { WHITE };
        canvas.draw_text(&text, color);
    } else {
        canvas.draw_ring(level);
        if charging {
            canvas.fill_polygon(&BOLT, GREEN);
        }
    }
    canvas.into_pixmap()
}

struct Canvas {
    size: u32,
    /// RGBA per pixel, row major.
    pixels: Vec<Rgba>,
}

impl Canvas {
    fn new(size: u32) -> Self {
        Self {
            size,
            pixels: vec![[0; 4]; (size * size) as usize],
        }
    }

    fn put(&mut self, x: u32, y: u32, color: Rgba) {
        if x < self.size && y < self.size {
            self.pixels[(y * self.size + x) as usize] = color;
        }
    }

    /// Grey ring with a green arc for `level`, clockwise from the top.
    fn draw_ring(&mut self, level: Option<u8>) {
        let center = self.size as f32 / 2.0;
        let filled = level.map_or(0.0, |l| f32::from(l.min(100)) / 100.0 * TAU);
        for y in 0..self.size {
            for x in 0..self.size {
                let dx = x as f32 - center;
                let dy = y as f32 - center;
                let dist = dx.hypot(dy);
                if dist <= RING_INNER_RADIUS || dist > RING_OUTER_RADIUS {
                    continue;
                }
                // atan2 grows clockwise on screen, where y points down; shift it
                // so the top of the ring is angle 0.
                let from_top = (dy.atan2(dx) + PI / 2.0).rem_euclid(TAU);
                let color = if from_top < filled { GREEN } else { GREY };
                self.put(x, y, color);
            }
        }
    }

    /// Fill a polygon, testing each pixel center with the even-odd rule.
    fn fill_polygon(&mut self, points: &[(f32, f32)], color: Rgba) {
        for y in 0..self.size {
            for x in 0..self.size {
                if contains(points, x as f32 + 0.5, y as f32 + 0.5) {
                    self.put(x, y, color);
                }
            }
        }
    }

    /// Centered text in the bitmap font, scaled up as far as the text box
    /// allows. Glyphs are stretched up to twice as tall as wide so short
    /// numbers stay readable at tray size.
    fn draw_text(&mut self, text: &str, color: Rgba) {
        let glyphs: Vec<&[u8; GLYPH_HEIGHT as usize]> = text.chars().filter_map(glyph).collect();
        let Some(count) = u32::try_from(glyphs.len()).ok().filter(|&c| c > 0) else {
            return;
        };
        // One font pixel of spacing between glyphs.
        let width_units = count * GLYPH_WIDTH + (count - 1);
        let fit_y = TEXT_MAX_HEIGHT / GLYPH_HEIGHT;
        let scale_x = (TEXT_MAX_WIDTH / width_units).clamp(1, fit_y);
        let scale_y = (scale_x * 2).min(fit_y);
        let left = (self.size - width_units * scale_x) / 2;
        let top = (self.size - GLYPH_HEIGHT * scale_y) / 2;
        for (i, rows) in (0u32..).zip(glyphs) {
            let glyph_left = left + i * (GLYPH_WIDTH + 1) * scale_x;
            for (row, bits) in (0u32..).zip(rows.iter()) {
                for col in 0..GLYPH_WIDTH {
                    if bits & (1 << (GLYPH_WIDTH - 1 - col)) == 0 {
                        continue;
                    }
                    for py in 0..scale_y {
                        for px in 0..scale_x {
                            self.put(
                                glyph_left + col * scale_x + px,
                                top + row * scale_y + py,
                                color,
                            );
                        }
                    }
                }
            }
        }
    }

    fn into_pixmap(self) -> Pixmap {
        let argb = self
            .pixels
            .iter()
            .flat_map(|&[r, g, b, a]| [a, r, g, b])
            .collect();
        Pixmap {
            width: self.size,
            height: self.size,
            argb,
        }
    }
}

/// Even-odd point in polygon test.
fn contains(points: &[(f32, f32)], x: f32, y: f32) -> bool {
    let mut inside = false;
    let mut prev = points[points.len() - 1];
    for &point in points {
        let ((x1, y1), (x2, y2)) = (prev, point);
        if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
            inside = !inside;
        }
        prev = point;
    }
    inside
}

/// Rows of a glyph, top to bottom; bit 4 is the leftmost pixel.
fn glyph(c: char) -> Option<&'static [u8; GLYPH_HEIGHT as usize]> {
    const DIGITS: [[u8; GLYPH_HEIGHT as usize]; 10] = [
        [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
    ];
    const DASH: [u8; GLYPH_HEIGHT as usize] = [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00];
    match c {
        '-' => Some(&DASH),
        _ => c.to_digit(10).map(|d| &DIGITS[d as usize]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * pixmap.width + x) * 4) as usize;
        pixmap.argb[i..i + 4].try_into().unwrap()
    }

    fn count(pixmap: &Pixmap, argb: [u8; 4]) -> usize {
        pixmap
            .argb
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| **p == argb)
            .count()
    }

    const ARGB_GREEN: [u8; 4] = [255, 0, 255, 0];
    const ARGB_GREY: [u8; 4] = [255, 128, 128, 128];
    const ARGB_WHITE: [u8; 4] = [255, 255, 255, 255];
    const CLEAR: [u8; 4] = [0, 0, 0, 0];

    #[test]
    fn every_icon_is_64_square_argb() {
        for text_mode in [false, true] {
            for level in [None, Some(0), Some(7), Some(55), Some(100), Some(255)] {
                let icon = render(level, text_mode, true);
                assert_eq!((icon.width, icon.height), (ICON_SIZE, ICON_SIZE));
                assert_eq!(icon.argb.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            }
        }
    }

    #[test]
    fn ring_fills_clockwise_from_the_top() {
        let icon = render(Some(25), false, false);
        // Top of the ring, just right of center: filled.
        assert_eq!(pixel(&icon, 33, 7), ARGB_GREEN);
        // Right side, at the quarter mark and a little past it.
        assert_eq!(pixel(&icon, 57, 31), ARGB_GREEN);
        assert_eq!(pixel(&icon, 57, 36), ARGB_GREY);
        // Left side and bottom: unfilled.
        assert_eq!(pixel(&icon, 6, 32), ARGB_GREY);
        assert_eq!(pixel(&icon, 32, 58), ARGB_GREY);
        // Center and corner stay transparent.
        assert_eq!(pixel(&icon, 32, 32), CLEAR);
        assert_eq!(pixel(&icon, 0, 0), CLEAR);
    }

    #[test]
    fn full_ring_has_no_grey() {
        let icon = render(Some(100), false, false);
        assert_eq!(count(&icon, ARGB_GREY), 0);
        assert!(count(&icon, ARGB_GREEN) > 0);
    }

    #[test]
    fn unknown_level_renders_no_fill() {
        let icon = render(None, false, false);
        assert_eq!(count(&icon, ARGB_GREEN), 0);
        assert!(count(&icon, ARGB_GREY) > 0);

        let empty = render(Some(0), false, false);
        assert_eq!(count(&empty, ARGB_GREEN), 0);
    }

    #[test]
    fn charging_draws_the_bolt_in_the_middle() {
        let icon = render(None, false, true);
        assert_eq!(pixel(&icon, 32, 31), ARGB_GREEN);
        assert_eq!(pixel(&render(None, false, false), 32, 31), CLEAR);
    }

    #[test]
    fn text_mode_draws_digits_centered() {
        let icon = render(Some(100), true, false);
        assert_eq!(count(&icon, ARGB_GREEN), 0);
        let white: Vec<(u32, u32)> = (0..ICON_SIZE)
            .flat_map(|y| (0..ICON_SIZE).map(move |x| (x, y)))
            .filter(|&(x, y)| pixel(&icon, x, y) == ARGB_WHITE)
            .collect();
        let min_x = white.iter().map(|p| p.0).min().unwrap();
        let max_x = white.iter().map(|p| p.0).max().unwrap();
        let min_y = white.iter().map(|p| p.1).min().unwrap();
        let max_y = white.iter().map(|p| p.1).max().unwrap();
        // "100" at 3x6 icon pixels per font pixel: a 51x42 box from (6, 11).
        // The "1" has no pixel in its first column, so ink starts one unit in.
        assert_eq!((min_x, max_x), (6 + 3, 6 + 51 - 1));
        assert_eq!((min_y, max_y), (11, 11 + 42 - 1));
    }

    #[test]
    fn text_mode_charging_is_green_and_unknown_is_a_dash() {
        let charging = render(Some(42), true, true);
        assert_eq!(count(&charging, ARGB_WHITE), 0);
        assert!(count(&charging, ARGB_GREEN) > 0);

        let unknown = render(None, true, false);
        // The dash is one font row: 5 units of 6x6 icon pixels.
        assert_eq!(count(&unknown, ARGB_WHITE), 5 * 6 * 6);
        assert_eq!(pixel(&unknown, 32, 32), ARGB_WHITE);
    }

    #[test]
    fn bitmap_font_covers_digits_and_dash() {
        for c in "0123456789-".chars() {
            let rows = glyph(c).unwrap();
            assert!(rows.iter().all(|r| *r < 1 << GLYPH_WIDTH));
            assert!(rows.iter().any(|r| *r != 0));
        }
        assert!(glyph('%').is_none());
    }
}
