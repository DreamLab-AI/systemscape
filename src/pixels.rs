//! Half-block pixel canvas: two square pixels per terminal cell, antialiased.
//!
//! The 3D scene is rasterised onto a supersampled surface and averaged down to
//! twice the terminal's row count. Each pair of pixel rows is then emitted as
//! one row of `▀` cells whose foreground is the top pixel and whose background
//! is the bottom pixel. Text labels sit on a separate cell layer so they stay
//! crisp over the shaded scene.
use gemini_engine::core::{Canvas, ColChar, Modifier, Vec2D};
use std::fmt::Write;

pub type Rgb = (u8, u8, u8);

/// Terminal columns a character occupies: 0 for combining marks and joiners,
/// 2 for East Asian wide characters and most emoji, 1 otherwise. An
/// approximation of `wcwidth` covering the ranges seen in agent transcripts.
pub fn char_width(c: char) -> usize {
    match u32::from(c) {
        0x0300..=0x036F
        | 0x200B..=0x200F
        | 0x20D0..=0x20FF
        | 0xFE00..=0xFE0F
        | 0xE0100..=0xE01EF => 0,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x1FA70..=0x1FAFF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// Marks the cell covered by the right half of a wide character.
const CONTINUATION: char = '\0';

/// Linear blend from `a` (t = 0) to `b` (t = 1).
pub fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

/// Samples per output pixel along each axis. The scene is rasterised at this
/// multiple and box-filtered down, so an edge pixel takes the blend of what it
/// partly covers instead of snapping from one colour to the next.
const SAMPLES: usize = 2;

pub struct PixelCanvas {
    pub width: usize,
    pub rows: usize,
    samples: Vec<Rgb>,
    text: Vec<Option<(char, Rgb)>>,
}

impl PixelCanvas {
    /// A canvas of `width` × `rows` cells filled with a vertical gradient.
    pub fn new(width: usize, rows: usize, top: Rgb, bottom: Rgb) -> Self {
        let (sample_width, sample_height) = (width * SAMPLES, rows * 2 * SAMPLES);
        let mut samples = Vec::with_capacity(sample_width * sample_height);
        for y in 0..sample_height {
            let shade = mix(top, bottom, y as f64 / sample_height.max(2) as f64);
            samples.extend(std::iter::repeat_n(shade, sample_width));
        }
        Self {
            width,
            rows,
            samples,
            text: vec![None; width * rows],
        }
    }

    /// Size of the surface the 3D scene is drawn to, in samples.
    pub fn sample_dims(&self) -> (usize, usize) {
        Self::samples_for(self.width, self.rows)
    }

    /// Sample-surface size of a canvas of `width` × `rows` cells.
    pub fn samples_for(width: usize, rows: usize) -> (usize, usize) {
        (width * SAMPLES, rows * 2 * SAMPLES)
    }

    /// The text cell (column, row) containing a sample position.
    pub fn cell_of(sample: Vec2D) -> (i64, i64) {
        (
            sample.x.div_euclid(SAMPLES as i64),
            sample.y.div_euclid(2 * SAMPLES as i64),
        )
    }

    /// Average the block of samples behind output pixel (`x`, `y`).
    fn pixel(&self, x: usize, y: usize) -> Rgb {
        let stride = self.width * SAMPLES;
        let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
        for sy in 0..SAMPLES {
            for sx in 0..SAMPLES {
                let s = self.samples[(y * SAMPLES + sy) * stride + x * SAMPLES + sx];
                r += u32::from(s.0);
                g += u32::from(s.1);
                b += u32::from(s.2);
            }
        }
        let n = (SAMPLES * SAMPLES) as u32;
        ((r / n) as u8, (g / n) as u8, (b / n) as u8)
    }

    /// Paint whole text rows `from..to` one colour, as a backdrop for text.
    pub fn fill_rows(&mut self, from: usize, to: usize, colour: Rgb) {
        let per_row = self.width * SAMPLES * 2 * SAMPLES;
        let end = (to.min(self.rows) * per_row).min(self.samples.len());
        let start = (from * per_row).min(end);
        self.samples[start..end].fill(colour);
    }

    /// Write text into the cell layer, clipped to the canvas. Wide characters
    /// take two cells and zero-width ones are dropped, so every row keeps
    /// exactly the canvas width on screen.
    pub fn label(&mut self, col: i64, row: i64, text: &str, colour: Rgb) {
        if row < 0 || row >= self.rows as i64 {
            return;
        }
        let base = row as usize * self.width;
        let width = self.width as i64;
        let mut x = col;
        for c in text.chars().filter(|c| !c.is_control()) {
            let cells = char_width(c) as i64;
            if cells == 0 {
                continue;
            }
            if x >= width {
                break;
            }
            if x >= 0 {
                let glyph = if cells == 2 && x + 1 >= width { ' ' } else { c };
                self.put(base, x as usize, glyph, colour);
                if glyph != ' ' && cells == 2 {
                    self.put(base, x as usize + 1, CONTINUATION, colour);
                }
            }
            x += if x + 1 >= width { 1 } else { cells };
        }
    }

    /// Set one cell, blanking the other half of any wide character it splits.
    fn put(&mut self, base: usize, x: usize, c: char, colour: Rgb) {
        let continuation = |cell: Option<(char, Rgb)>| matches!(cell, Some((CONTINUATION, _)));
        let old = self.text[base + x];
        if continuation(old) {
            // Over the right half of a wide character: blank its left half.
            if x > 0 {
                self.text[base + x - 1] = Some((' ', colour));
            }
        } else if x + 1 < self.width && continuation(self.text[base + x + 1]) {
            // Over the left half of a wide character: drop its right half.
            self.text[base + x + 1] = None;
        }
        self.text[base + x] = Some((c, colour));
    }

    /// One ANSI string per terminal row, each covering the full width.
    pub fn rows(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.rows);
        for row in 0..self.rows {
            let mut line = String::with_capacity(self.width * 12);
            let (mut fg, mut bg): (Option<Rgb>, Option<Rgb>) = (None, None);
            for x in 0..self.width {
                let top = self.pixel(x, row * 2);
                let bottom = self.pixel(x, row * 2 + 1);
                let (glyph, want_fg, want_bg) = match self.text[row * self.width + x] {
                    // The wide character to the left already covers this cell.
                    Some((CONTINUATION, _)) => continue,
                    // Darken the scene behind text so labels stay readable.
                    Some((c, colour)) => {
                        (c, Some(colour), mix(mix(top, bottom, 0.5), (0, 0, 0), 0.5))
                    }
                    None if top == bottom => (' ', None, top),
                    None => ('▀', Some(top), bottom),
                };
                if bg != Some(want_bg) {
                    let _ = write!(line, "\x1b[48;2;{};{};{}m", want_bg.0, want_bg.1, want_bg.2);
                    bg = Some(want_bg);
                }
                if let Some(c) = want_fg.filter(|c| fg != Some(*c)) {
                    let _ = write!(line, "\x1b[38;2;{};{};{}m", c.0, c.1, c.2);
                    fg = Some(c);
                }
                line.push(glyph);
            }
            line.push_str("\x1b[0m");
            out.push(line);
        }
        out
    }
}

impl Canvas for PixelCanvas {
    fn plot(&mut self, pos: Vec2D, c: ColChar) {
        let (width, height) = self.sample_dims();
        if pos.x < 0 || pos.y < 0 || pos.x >= width as i64 || pos.y >= height as i64 {
            return;
        }
        if let Modifier::Colour(colour) = c.modifier {
            self.samples[pos.y as usize * width + pos.x as usize] = (colour.r, colour.g, colour.b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_cover_the_canvas_and_pair_pixels_into_half_blocks() {
        let mut canvas = PixelCanvas::new(4, 2, (0, 0, 0), (0, 0, 0));
        let red = ColChar::SOLID.with_mod(Modifier::Colour(gemini_engine::core::Colour::rgb(
            255, 0, 0,
        )));
        // Cover output pixel (1, 0) fully and pixel (2, 0) by half.
        for (x, y) in [(2, 0), (3, 0), (2, 1), (3, 1), (4, 0), (4, 1)] {
            canvas.plot(Vec2D::new(x, y), red);
        }
        canvas.plot(Vec2D::new(99, 99), ColChar::SOLID); // out of bounds: ignored
        canvas.label(2, 1, "ok!", (1, 2, 3)); // clipped at the right edge
        let rows = canvas.rows();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("\x1b[38;2;255;0;0m▀"));
        assert!(
            rows[0].contains("\x1b[38;2;127;0;0m▀"),
            "half-covered pixel blends"
        );
        assert!(rows[1].contains("ok") && !rows[1].contains('!'));
        let visible = |s: &str| {
            let mut n = 0;
            let mut escape = false;
            for c in s.chars() {
                match (escape, c) {
                    (false, '\x1b') => escape = true,
                    (true, 'm') => escape = false,
                    (false, _) => n += 1,
                    _ => {}
                }
            }
            n
        };
        assert!(rows.iter().all(|r| visible(r) == 4));

        // Wide characters take two cells; a split pair is blanked, not left half.
        let mut canvas = PixelCanvas::new(6, 1, (0, 0, 0), (0, 0, 0));
        canvas.label(0, 0, "日本e\u{301}x", (9, 9, 9));
        let row = &canvas.rows()[0];
        assert!(row.contains("日本ex"), "{row:?}");
        assert_eq!(visible(row) + 2, 6, "two wide glyphs cover four cells");
        canvas.label(1, 0, "z", (9, 9, 9)); // lands on the right half of 日
        let row = &canvas.rows()[0];
        assert!(row.contains(" z本"), "{row:?}");
        canvas.label(5, 0, "語", (9, 9, 9)); // no room for both halves
        assert!(!canvas.rows()[0].contains('語'));
        // A wide glyph straddling two others blanks the half of each it splits.
        let mut canvas = PixelCanvas::new(6, 1, (0, 0, 0), (0, 0, 0));
        canvas.label(0, 0, "日本", (9, 9, 9));
        canvas.label(1, 0, "語", (9, 9, 9));
        let row = &canvas.rows()[0];
        assert!(row.contains(" 語 "), "{row:?}");
        assert_eq!(visible(row) + 1, 6, "one wide glyph and four narrow cells");
    }
}
