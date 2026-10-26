//! Geometry-driven pixel writing: the part of rendering that can be tested
//! without a machine.
//!
//! A [`Canvas`] is a byte slice plus the numbers that describe where the pixels
//! sit inside it. No port I/O, no `bootloader_api`, no allocation and no unsafe
//! code: that is what lets `cargo test --package kernel --lib` execute the
//! clipping and blending arithmetic on the host instead of trusting it on the
//! one machine nobody wants to debug it on.
//!
//! `framebuffer.rs` keeps its own `Color` for one more step, until the adapter
//! that maps a firmware `FrameBuffer` onto a `Canvas` lands. That adapter is also
//! where [`ChannelOrder`] gets checked against what the firmware really wants.

/// Bytes in one pixel. The kernel only renders into 32-bit surfaces, and
/// [`Canvas::new`] refuses anything else rather than guessing at the layout.
pub const BYTES_PER_PIXEL: usize = 4;

/// One opaque pixel, in 8-bit channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const BLACK: Self = Self::new(0, 0, 0);
    pub const WHITE: Self = Self::new(255, 255, 255);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// The order the firmware expects the colour bytes of a pixel in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelOrder {
    /// Red, green, blue in ascending memory order.
    Rgb,
    /// Blue, green, red in ascending memory order: the common `Bgra` layout.
    Bgr,
}

/// The surface layout, taken from the bootloader rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: usize,
    pub height: usize,
    /// Pixels per row, which firmware may set larger than `width`.
    pub stride: usize,
    pub bytes_per_pixel: usize,
}

impl Geometry {
    pub const fn new(width: usize, height: usize, stride: usize, bytes_per_pixel: usize) -> Self {
        Self {
            width,
            height,
            stride,
            bytes_per_pixel,
        }
    }

    /// Bytes one row occupies, or `None` if that does not fit a `usize`.
    pub fn row_bytes(&self) -> Option<usize> {
        self.stride.checked_mul(self.bytes_per_pixel)
    }

    /// Bytes the whole surface occupies, or `None` if that does not fit a `usize`.
    pub fn byte_len(&self) -> Option<usize> {
        self.row_bytes()?.checked_mul(self.height)
    }
}

/// A rectangle in surface coordinates.
///
/// The origin is signed so a caller can ask for a rect that starts off-screen and
/// let the canvas clip it. Unsigned coordinates would turn `-1` into a rectangle
/// four billion pixels wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The visible part of this rect on a `width` x `height` surface, as
    /// `(x, y, width, height)` in pixels, or `None` if none of it is visible.
    ///
    /// The arithmetic runs in `i64` because both the origin and the extent can be
    /// extreme: `Rect::new(-1, -1, u32::MAX, u32::MAX)` has to intersect to one
    /// pixel, not wrap around the far side of the surface.
    pub fn clip(self, width: usize, height: usize) -> Option<(usize, usize, usize, usize)> {
        let surface_width = i64::try_from(width).unwrap_or(i64::MAX);
        let surface_height = i64::try_from(height).unwrap_or(i64::MAX);
        let left = i64::from(self.x).max(0);
        let top = i64::from(self.y).max(0);
        let right = i64::from(self.x)
            .saturating_add(i64::from(self.width))
            .min(surface_width);
        let bottom = i64::from(self.y)
            .saturating_add(i64::from(self.height))
            .min(surface_height);
        if right <= left || bottom <= top {
            return None;
        }
        Some((
            usize::try_from(left).ok()?,
            usize::try_from(top).ok()?,
            usize::try_from(right - left).ok()?,
            usize::try_from(bottom - top).ok()?,
        ))
    }
}

/// A mutable view of a firmware surface.
pub struct Canvas<'a> {
    pixels: &'a mut [u8],
    geometry: Geometry,
    order: ChannelOrder,
}

impl<'a> Canvas<'a> {
    /// Wraps `pixels` if they really can hold `geometry`.
    ///
    /// Returns `None` instead of panicking when they cannot. The layout comes from
    /// firmware, so a surface smaller than the geometry it advertises is a failure
    /// to refuse rather than abort on - and once this succeeds, every index the
    /// drawing methods compute is inside `pixels` by construction.
    pub fn new(pixels: &'a mut [u8], geometry: Geometry, order: ChannelOrder) -> Option<Self> {
        if geometry.bytes_per_pixel != BYTES_PER_PIXEL
            || geometry.stride == 0
            || geometry.width > geometry.stride
        {
            return None;
        }
        if geometry.byte_len()? > pixels.len() {
            return None;
        }
        Some(Self {
            pixels,
            geometry,
            order,
        })
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Fills the whole surface, stride padding included, so no stale pixel of an
    /// earlier frame can survive behind the visible area.
    pub fn fill(&mut self, color: Color) {
        let encoded = self.encode(color, u8::MAX);
        let Some(byte_len) = self.geometry.byte_len() else {
            return;
        };
        let byte_len = byte_len.min(self.pixels.len());
        for pixel in self.pixels[..byte_len].as_chunks_mut::<BYTES_PER_PIXEL>().0 {
            *pixel = encoded;
        }
    }

    /// Paints `rect`, clipped to the surface. Padding pixels are left alone: they
    /// are not part of the image.
    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        let encoded = self.encode(color, u8::MAX);
        self.paint(rect, |row, offset, count| {
            for pixel in row[offset..offset + count]
                .as_chunks_mut::<BYTES_PER_PIXEL>()
                .0
            {
                *pixel = encoded;
            }
        });
    }

    /// Blends `color` over `rect` with `alpha`, clipped to the surface.
    pub fn blend_rect(&mut self, rect: Rect, color: Color, alpha: u8) {
        match alpha {
            0 => return,
            u8::MAX => return self.fill_rect(rect, color),
            _ => {}
        }
        let source = self.encode(color, alpha);
        self.paint(rect, |row, offset, count| {
            for pixel in row[offset..offset + count]
                .as_chunks_mut::<BYTES_PER_PIXEL>()
                .0
            {
                // Only the three colour bytes are blended. The fourth is padding in
                // the firmware's format, and pushing it towards the blend would
                // claim an alpha channel this canvas does not have.
                for (dst, src) in pixel.iter_mut().zip(source.iter()).take(3) {
                    *dst = blend_channel(*dst, *src, alpha);
                }
            }
        });
    }

    /// Moves the pixels of `rect` up by `dy` pixels and fills the gap with `color`.
    ///
    /// This is a console scroll: `copy_within` moves whole rows, so the work is proportional
    /// to pixels rather than to glyphs. The alternative - clearing the band and redrawing
    /// the text that was already there - spends seconds on a boot log that outgrows the
    /// screen, because every line then repaints every row.
    pub fn scroll_up(&mut self, rect: Rect, dy: u32, color: Color) {
        let Some((x, y, width, height)) = rect.clip(self.geometry.width, self.geometry.height)
        else {
            return;
        };
        let dy = (usize::try_from(dy).unwrap_or(usize::MAX)).min(height);
        if dy == 0 {
            return;
        }
        if dy == height {
            return self.fill_rect(rect, color);
        }
        let stride_bytes = self.geometry.stride * BYTES_PER_PIXEL;
        let offset = x * BYTES_PER_PIXEL;
        let count = width * BYTES_PER_PIXEL;
        // `copy_within` handles the overlap, and `new` proved every index here is inside
        // the buffer, padding rows included.
        for row in y..y + height - dy {
            let source = (row + dy) * stride_bytes + offset;
            self.pixels
                .copy_within(source..source + count, row * stride_bytes + offset);
        }
        self.fill_rect(
            Rect::new(x as i32, (y + height - dy) as i32, width as u32, dy as u32),
            color,
        );
    }

    /// Paints `color` on the pixels of `rect` for which `keep` returns true, clipped to the
    /// surface.
    ///
    /// `keep` receives each pixel's coordinates relative to `rect`'s top-left corner, so a
    /// caller can hand over a bitmap without knowing where the rect was clipped. This is
    /// what makes a glyph one call: drawing it as a mask costs a fraction of the ~30
    /// `fill_rect` calls per ink run it used to take, which was measurable in the boot log.
    pub fn fill_where(&mut self, rect: Rect, color: Color, keep: impl Fn(u32, u32) -> bool) {
        let Some((x, y, width, height)) = rect.clip(self.geometry.width, self.geometry.height)
        else {
            return;
        };
        let encoded = self.encode(color, u8::MAX);
        let stride_bytes = self.geometry.stride * BYTES_PER_PIXEL;
        // Clipping only ever moves the origin right and down, so these are never negative.
        let origin_x = (x as i64 - i64::from(rect.x)) as u32;
        let origin_y = (y as i64 - i64::from(rect.y)) as u32;
        for row_index in 0..height {
            let start = (y + row_index) * stride_bytes;
            let row = &mut self.pixels[start..start + stride_bytes];
            for column in 0..width {
                if !keep(origin_x + column as u32, origin_y + row_index as u32) {
                    continue;
                }
                let offset = (x + column) * BYTES_PER_PIXEL;
                row[offset..offset + BYTES_PER_PIXEL].copy_from_slice(&encoded);
            }
        }
    }

    /// Runs `paint` once per visible row of `rect`, handing it the row, the byte
    /// offset of the rect inside that row, and the byte width to paint.
    fn paint(&mut self, rect: Rect, mut paint: impl FnMut(&mut [u8], usize, usize)) {
        let Some((x, y, width, height)) = rect.clip(self.geometry.width, self.geometry.height)
        else {
            return;
        };
        let offset = x * BYTES_PER_PIXEL;
        let count = width * BYTES_PER_PIXEL;
        // `new` proved the buffer holds `height` full rows, so every row here has
        // room for `offset + count`.
        //
        // The first row is indexed directly rather than reached with `rows().skip(y)`:
        // skipping walks every row above the rect, and text drawing makes thousands of
        // these calls per line, nearly all of them low on the screen.
        let stride_bytes = self.geometry.stride * BYTES_PER_PIXEL;
        let start = y * stride_bytes;
        let end = start + height * stride_bytes;
        for row in self.pixels[start..end].chunks_mut(stride_bytes) {
            paint(row, offset, count);
        }
    }

    /// The bytes one pixel occupies, in the firmware's order.
    ///
    /// The fourth byte is padding; `alpha` keeps a `Bgra` surface trivially opaque
    /// where the firmware cares at all.
    fn encode(&self, color: Color, alpha: u8) -> [u8; BYTES_PER_PIXEL] {
        match self.order {
            ChannelOrder::Rgb => [color.r, color.g, color.b, alpha],
            ChannelOrder::Bgr => [color.b, color.g, color.r, alpha],
        }
    }
}

/// Blends `src` over `dst` with `alpha`, rounding to nearest.
fn blend_channel(dst: u8, src: u8, alpha: u8) -> u8 {
    let max = u32::from(u8::MAX);
    let alpha = u32::from(alpha);
    // `+ max / 2` is half of 255: round to nearest instead of always truncating,
    // which would darken anything blended at half alpha by one step.
    let blended = (u32::from(src) * alpha + u32::from(dst) * (max - alpha) + max / 2) / max;
    u8::try_from(blended).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four visible pixels per row, stride six: two padding pixels at the end of
    /// every row, so a write that ignores the stride cannot pass unnoticed.
    const GEOMETRY: Geometry = Geometry::new(4, 3, 6, BYTES_PER_PIXEL);
    /// Three rows of six pixels.
    const BUFFER_LEN: usize = 3 * 6 * BYTES_PER_PIXEL;

    fn blank() -> Vec<u8> {
        vec![0; BUFFER_LEN]
    }

    fn pixel(pixels: &[u8], x: usize, y: usize) -> [u8; BYTES_PER_PIXEL] {
        let offset = (y * GEOMETRY.stride + x) * BYTES_PER_PIXEL;
        pixels[offset..offset + BYTES_PER_PIXEL]
            .try_into()
            .expect("a pixel is four bytes")
    }

    fn canvas(pixels: &mut [u8]) -> Canvas<'_> {
        Canvas::new(pixels, GEOMETRY, ChannelOrder::Bgr).expect("geometry fits its buffer")
    }

    #[test]
    fn rejects_a_buffer_too_small_for_the_geometry_it_claims() {
        let mut pixels = vec![0; BUFFER_LEN - 1];
        assert!(Canvas::new(&mut pixels, GEOMETRY, ChannelOrder::Bgr).is_none());
    }

    #[test]
    fn rejects_geometry_its_own_loops_cannot_trust() {
        let mut pixels = blank();
        let wrong_pixel_size = Geometry::new(4, 3, 6, 3);
        let zero_stride = Geometry::new(0, 3, 0, BYTES_PER_PIXEL);
        let stride_shorter_than_width = Geometry::new(6, 3, 4, BYTES_PER_PIXEL);
        for geometry in [wrong_pixel_size, zero_stride, stride_shorter_than_width] {
            assert!(
                Canvas::new(&mut pixels, geometry, ChannelOrder::Bgr).is_none(),
                "{geometry:?} should have been refused"
            );
        }
    }

    #[test]
    fn fill_covers_the_whole_surface_including_padding() {
        let mut pixels = blank();
        canvas(&mut pixels).fill(Color::WHITE);
        assert!(pixels.iter().all(|byte| *byte == u8::MAX));
    }

    #[test]
    fn fill_rect_leaves_padding_and_other_rows_alone() {
        let mut pixels = blank();
        canvas(&mut pixels).fill_rect(Rect::new(1, 1, 2, 1), Color::WHITE);

        assert_eq!(pixel(&pixels, 1, 1), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 2, 1), [u8::MAX; BYTES_PER_PIXEL]);
        // The pixel before the rect, the pixel after it, and both padding pixels.
        assert_eq!(pixel(&pixels, 0, 1), [0; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 3, 1), [0; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 4, 1), [0; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 5, 1), [0; BYTES_PER_PIXEL]);
        // A row above and a row below.
        assert_eq!(pixel(&pixels, 1, 0), [0; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 1, 2), [0; BYTES_PER_PIXEL]);
    }

    #[test]
    fn clips_a_rect_that_starts_off_screen() {
        let mut pixels = blank();
        canvas(&mut pixels).fill_rect(Rect::new(-2, -2, 3, 3), Color::WHITE);

        // Only the bottom-right pixel of the rect lands on the surface.
        assert_eq!(pixel(&pixels, 0, 0), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 1, 0), [0; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 0, 1), [0; BYTES_PER_PIXEL]);
    }

    #[test]
    fn clips_a_rect_that_runs_past_the_far_edges() {
        let mut pixels = blank();
        canvas(&mut pixels).fill_rect(Rect::new(2, 2, 10, 10), Color::WHITE);

        assert_eq!(pixel(&pixels, 2, 2), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 3, 2), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 0, 2), [0; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 2, 1), [0; BYTES_PER_PIXEL]);
        // Padding past the last visible pixel of the row stays untouched too.
        assert_eq!(pixel(&pixels, 4, 2), [0; BYTES_PER_PIXEL]);
    }

    #[test]
    fn ignores_a_rect_that_is_entirely_off_screen() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            canvas.fill_rect(Rect::new(10, 10, 4, 4), Color::WHITE);
            canvas.fill_rect(Rect::new(-10, -10, 4, 4), Color::WHITE);
            canvas.fill_rect(Rect::new(0, 5, 4, 4), Color::WHITE);
        }
        assert!(pixels.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn ignores_an_empty_rect() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            canvas.fill_rect(Rect::new(0, 0, 0, 3), Color::WHITE);
            canvas.fill_rect(Rect::new(0, 0, 3, 0), Color::WHITE);
        }
        assert!(pixels.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn blending_with_zero_alpha_changes_nothing() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            canvas.fill(Color::WHITE);
            canvas.blend_rect(Rect::new(0, 0, 4, 3), Color::BLACK, 0);
        }
        assert!(pixels.iter().all(|byte| *byte == u8::MAX));
    }

    #[test]
    fn blending_with_full_alpha_is_opaque() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            canvas.fill(Color::BLACK);
            canvas.blend_rect(Rect::new(0, 0, 4, 3), Color::WHITE, u8::MAX);
        }
        for y in 0..GEOMETRY.height {
            for x in 0..GEOMETRY.width {
                assert_eq!(pixel(&pixels, x, y), [u8::MAX; BYTES_PER_PIXEL]);
            }
            // Padding is outside the rect, so it still holds what `fill` wrote
            // there: black, with the opaque fourth byte `fill` puts in every pixel.
            assert_eq!(pixel(&pixels, 4, y), [0, 0, 0, u8::MAX]);
        }
    }

    #[test]
    fn blending_half_alpha_white_over_black_rounds_to_mid_grey() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            canvas.fill(Color::BLACK);
            canvas.blend_rect(Rect::new(0, 0, 1, 1), Color::WHITE, 128);
        }
        // 128/255 of the way from 0 to 255 is 128, not 127: the arithmetic rounds.
        assert_eq!(&pixel(&pixels, 0, 0)[..3], &[128, 128, 128]);
        // The padding byte is not blended: it still holds the opaque value `fill`
        // wrote, rather than drifting towards grey with the colour channels.
        assert_eq!(pixel(&pixels, 0, 0)[3], u8::MAX);
        // Everything outside the rect is still the black `fill` left behind.
        assert_eq!(pixel(&pixels, 1, 0), [0, 0, 0, u8::MAX]);
    }

    #[test]
    fn encodes_the_same_colour_in_either_channel_order() {
        let color = Color::new(1, 2, 3);

        let mut bgr_pixels = blank();
        Canvas::new(&mut bgr_pixels, GEOMETRY, ChannelOrder::Bgr)
            .expect("geometry fits")
            .fill_rect(Rect::new(0, 0, 1, 1), color);
        assert_eq!(pixel(&bgr_pixels, 0, 0), [3, 2, 1, u8::MAX]);

        let mut rgb_pixels = blank();
        Canvas::new(&mut rgb_pixels, GEOMETRY, ChannelOrder::Rgb)
            .expect("geometry fits")
            .fill_rect(Rect::new(0, 0, 1, 1), color);
        assert_eq!(pixel(&rgb_pixels, 0, 0), [1, 2, 3, u8::MAX]);
    }

    #[test]
    fn fills_only_the_pixels_the_mask_selects() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            // A 2x2 rect at the top left, keeping only the diagonal.
            canvas.fill_where(Rect::new(0, 0, 2, 2), Color::WHITE, |x, y| x == y);
        }
        assert_eq!(pixel(&pixels, 0, 0), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 1, 1), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 1, 0), [0; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 0, 1), [0; BYTES_PER_PIXEL]);
    }

    #[test]
    fn a_clipped_mask_is_still_in_step_with_the_rect() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            // Half of the rect is off the left edge, so the mask must be sampled from
            // column 2 at x = 0 - the rect's own coordinates, not the surface's.
            canvas.fill_where(Rect::new(-2, 0, 4, 1), Color::WHITE, |x, _| x >= 2);
        }
        assert_eq!(pixel(&pixels, 0, 0), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 1, 0), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 2, 0), [0; BYTES_PER_PIXEL]);
    }

    #[test]
    fn scrolling_moves_rows_up_and_fills_the_gap() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            canvas.fill(Color::WHITE);
            canvas.fill_rect(Rect::new(0, 0, 4, 1), Color::new(0, 0, 1));
            canvas.fill_rect(Rect::new(0, 1, 4, 1), Color::new(0, 0, 2));
            canvas.scroll_up(Rect::new(0, 0, 4, 3), 1, Color::BLACK);
        }
        // Row 1 moved up to row 0, row 2 stayed white, and the vacated row is black. The
        // test canvas is Bgr, so `Color::new(0, 0, n)` lands as [n, 0, 0].
        assert_eq!(pixel(&pixels, 0, 0), [2, 0, 0, u8::MAX]);
        assert_eq!(pixel(&pixels, 0, 1), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 0, 2), [0, 0, 0, u8::MAX]);
    }

    #[test]
    fn scrolling_by_the_whole_height_clears_the_rect() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            canvas.fill(Color::WHITE);
            canvas.scroll_up(Rect::new(0, 0, 4, 3), 3, Color::BLACK);
        }
        // The visible area is black; the padding columns are outside the rect and keep
        // what `fill` put there.
        for y in 0..3 {
            assert_eq!(pixel(&pixels, 0, y), [0, 0, 0, u8::MAX]);
            assert_eq!(pixel(&pixels, 4, y), [u8::MAX; BYTES_PER_PIXEL]);
        }
    }

    #[test]
    fn scrolling_by_nothing_changes_nothing() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            canvas.fill(Color::WHITE);
            canvas.scroll_up(Rect::new(0, 0, 4, 3), 0, Color::BLACK);
        }
        assert!(pixels.iter().all(|byte| *byte == u8::MAX));
    }
}
