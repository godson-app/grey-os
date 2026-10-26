//! Glyph drawing: turns characters into pixels on a [`Canvas`].
//!
//! This module knows about glyphs and nothing else. Wrapping, cursors and scrolling are
//! `console.rs`'s job, which keeps the arithmetic that decides where a pixel lands small
//! enough to test on the host against a plain byte slice - no firmware, no port I/O.
//!
//! The font is 8x8 (see `font.rs`), which is small on a modern panel, so drawing takes
//! an integer scale. At scale 2 a character is a 16x16 pixel cell, the size a boot log
//! is comfortable at; the scale stays a parameter because the same renderer has to draw
//! a panic message on whatever surface the firmware chose.

use crate::canvas::{Canvas, Color, Rect};
use crate::font;

/// Largest scale [`draw_char`] honours.
///
/// A scale is a small integer by nature; clamping keeps the coordinate arithmetic far
/// from `i32` overflow, and keeps a nonsense value like `usize::MAX` from turning one
/// glyph into a rectangle the size of the address space.
pub const MAX_SCALE: usize = 16;

/// `scale`, or 1 when it is 0, or [`MAX_SCALE`] when it is larger.
///
/// Scale 0 would draw zero-width rectangles and advance the pen by nothing, so a bug
/// that passed it would produce a blank screen with no indication why. One pixel is the
/// useful reading of "as small as possible".
pub fn clamp_scale(scale: usize) -> usize {
    scale.clamp(1, MAX_SCALE)
}

/// The size in pixels of the cell one character occupies at `scale`.
pub fn cell_size(scale: usize) -> (u32, u32) {
    let scale = clamp_scale(scale) as u32;
    (
        font::GLYPH_WIDTH as u32 * scale,
        font::GLYPH_HEIGHT as u32 * scale,
    )
}

/// How many whole character cells fit in `width` x `height` pixels at `scale`, as
/// `(columns, rows)`.
///
/// Rounded down: a partial cell at the right or bottom edge is unusable for text, and
/// claiming it would put the last column half off-screen.
pub fn grid_size(width: u32, height: u32, scale: usize) -> (usize, usize) {
    let (cell_width, cell_height) = cell_size(scale);
    (
        (width / cell_width) as usize,
        (height / cell_height) as usize,
    )
}

/// Draws one character with its top-left corner at `x`, `y`, in `color`.
///
/// Pixels outside the canvas are clipped by [`Canvas::fill_rect`], so a glyph may start
/// partly or wholly off-screen. A character the font does not cover is drawn as
/// [`font::glyph_or_fallback`]'s box, which is visible on purpose.
pub fn draw_char(canvas: &mut Canvas<'_>, x: i32, y: i32, ch: char, color: Color, scale: usize) {
    let scale = clamp_scale(scale);
    let rows = font::glyph_or_fallback(ch);
    let block = scale as u32;

    for (row, bits) in rows.iter().enumerate() {
        let top = y.saturating_add((row * scale) as i32);
        let mut column = 0;
        while column < font::GLYPH_WIDTH {
            if bits & (1 << column) == 0 {
                column += 1;
                continue;
            }
            // Coalesce a horizontal run of ink into one rectangle. A glyph row is
            // usually two or three runs, and this keeps a full screen of text to a few
            // thousand `fill_rect` calls instead of tens of thousands.
            let start = column;
            while column < font::GLYPH_WIDTH && bits & (1 << column) != 0 {
                column += 1;
            }
            canvas.fill_rect(
                Rect::new(
                    x.saturating_add((start * scale) as i32),
                    top,
                    (column - start) as u32 * block,
                    block,
                ),
                color,
            );
        }
    }
}

/// Draws `text` from `x`, `y` rightwards, returning the pen position after the last
/// character.
///
/// Control characters are not interpreted: a `\n` here draws the fallback box. Layout is
/// the console's business, and swallowing a newline silently would hide a caller that
/// expected it to mean something.
pub fn draw_str(
    canvas: &mut Canvas<'_>,
    x: i32,
    y: i32,
    text: &str,
    color: Color,
    scale: usize,
) -> i32 {
    let advance = cell_size(scale).0 as i32;
    let mut pen = x;
    for ch in text.chars() {
        draw_char(canvas, pen, y, ch, color, scale);
        pen = pen.saturating_add(advance);
    }
    pen
}

/// Width and height in pixels `text` occupies at `scale`, as `(width, height)`.
///
/// Nothing is clipped: this is what a caller needs to decide whether the text fits.
/// Width saturates rather than wrapping, because a string wide enough to overflow a
/// `u64` of pixels is a bug, not a layout to render.
pub fn measure(text: &str, scale: usize) -> (u64, u64) {
    let (cell_width, cell_height) = cell_size(scale);
    (
        text.chars().count() as u64 * u64::from(cell_width),
        u64::from(cell_height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{ChannelOrder, Geometry};

    const GEOMETRY: Geometry = Geometry::new(16, 16, 16, crate::canvas::BYTES_PER_PIXEL);
    const INK: Color = Color::new(0xff, 0xff, 0xff);

    fn blank() -> [u8; 16 * 16 * 4] {
        [0u8; 16 * 16 * 4]
    }

    fn canvas(pixels: &mut [u8]) -> Canvas<'_> {
        Canvas::new(pixels, GEOMETRY, ChannelOrder::Rgb).expect("geometry fits the buffer")
    }

    fn is_ink(pixels: &[u8], x: usize, y: usize) -> bool {
        let offset = (y * GEOMETRY.stride + x) * 4;
        pixels[offset] != 0
    }

    #[test]
    fn draws_the_bitmap_with_bit_0_on_the_left() {
        // 'F' starts with 0x7f: its top row is ink in columns 0-6 and blank in column 7.
        // Mirrored, the first pixel would be blank and the last lit, which is exactly the
        // bug this pins down.
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            draw_char(&mut surface, 0, 0, 'F', INK, 1);
        }
        assert!(is_ink(&pixels, 0, 0), "left edge of the top bar");
        assert!(
            !is_ink(&pixels, 7, 0),
            "the font leaves the last column free"
        );
    }

    #[test]
    fn draws_the_expected_pixels_for_a_known_glyph() {
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            draw_char(&mut surface, 0, 0, 'A', INK, 1);
        }
        for (row, bits) in font::glyph('A').expect("A is covered").iter().enumerate() {
            for column in 0..font::GLYPH_WIDTH {
                let expected = bits & (1 << column) != 0;
                assert_eq!(
                    is_ink(&pixels, column, row),
                    expected,
                    "'A' at column {column}, row {row}"
                );
            }
        }
    }

    #[test]
    fn scales_every_glyph_pixel_into_a_square() {
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            draw_char(&mut surface, 0, 0, 'F', INK, 2);
        }
        // The top-left pixel of 'F' becomes a 2x2 block, and the font's blank column 7
        // becomes a blank 2-pixel-wide strip at x = 14 and 15.
        for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            assert!(is_ink(&pixels, x, y), "({x}, {y}) should be ink");
        }
        assert!(!is_ink(&pixels, 14, 0));
        assert!(!is_ink(&pixels, 15, 0));
    }

    #[test]
    fn treats_scale_zero_as_one_and_clamps_the_large_end() {
        assert_eq!(clamp_scale(0), 1);
        assert_eq!(clamp_scale(1), 1);
        assert_eq!(clamp_scale(MAX_SCALE + 5), MAX_SCALE);
    }

    #[test]
    fn clips_a_glyph_that_starts_off_screen() {
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            // Row 1 of 'F' is 0x46: ink in columns 1, 2 and 6. Shifted left by one, the
            // surface's column 0 shows the glyph's column 1.
            draw_char(&mut surface, -1, 0, 'F', INK, 1);
        }
        assert!(is_ink(&pixels, 0, 1));
        // Row 7 of 'F' is blank, so nothing reaches the surface there.
        assert!(!is_ink(&pixels, 0, 7));
    }

    #[test]
    fn draws_nothing_when_the_whole_glyph_is_outside() {
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            draw_char(&mut surface, 16, 0, 'F', INK, 1);
            draw_char(&mut surface, 0, 16, 'F', INK, 1);
        }
        assert!(pixels.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn advances_one_cell_per_character() {
        let mut pixels = blank();
        let pen = {
            let mut surface = canvas(&mut pixels);
            draw_str(&mut surface, 0, 0, "AB", INK, 1)
        };
        assert_eq!(
            pen, 16,
            "two characters at scale 1 occupy two 8-pixel cells"
        );
        // 'B' is drawn in the second cell, not on top of 'A'.
        assert!(is_ink(&pixels, 8, 0), "the left edge of 'B'");
    }

    #[test]
    fn an_empty_string_leaves_the_pen_alone() {
        let mut pixels = blank();
        let pen = {
            let mut surface = canvas(&mut pixels);
            draw_str(&mut surface, 4, 2, "", INK, 1)
        };
        assert_eq!(pen, 4);
        assert!(pixels.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn draws_the_fallback_box_for_a_control_character() {
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            draw_str(&mut surface, 0, 0, "\n", INK, 1);
        }
        // The fallback is a hollow box: its top row has ink in columns 2-5 only.
        assert!(is_ink(&pixels, 2, 0));
        assert!(!is_ink(&pixels, 0, 0));
        assert!(!is_ink(&pixels, 7, 0));
    }

    #[test]
    fn measures_text_in_pixels() {
        assert_eq!(measure("", 1), (0, 8));
        assert_eq!(measure("AB", 1), (16, 8));
        assert_eq!(measure("AB", 3), (48, 24));
        // Multi-byte characters are one cell each: the count is characters, not bytes.
        assert_eq!(measure("é€", 1), (16, 8));
    }

    #[test]
    fn divides_the_surface_into_whole_cells() {
        assert_eq!(grid_size(80, 40, 1), (10, 5));
        assert_eq!(grid_size(80, 40, 2), (5, 2));
        // A surface smaller than one cell has no room for text at all.
        assert_eq!(grid_size(7, 7, 1), (0, 0));
    }
}
