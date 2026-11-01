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

    /// Copies `rect` from `source` onto this surface, clipped to both.
    ///
    /// This is the present half of drawing into a back buffer: whole rows move at once, so
    /// the cost is proportional to the pixels copied rather than to the glyphs they came
    /// from, and the panel is written once per band instead of once per pixel.
    ///
    /// The two surfaces must share a layout - the same bytes per pixel and the same
    /// [`ChannelOrder`]. The copy is by bytes, so a surface in the other order would take
    /// red for blue; that case copies nothing rather than painting wrong colours.
    pub fn blit(&mut self, source: &Canvas<'_>, rect: Rect) {
        if source.geometry.bytes_per_pixel != self.geometry.bytes_per_pixel
            || source.order != self.order
        {
            return;
        }
        let Some((x, y, width, height)) = rect.clip(self.geometry.width, self.geometry.height)
        else {
            return;
        };
        // Clip again against the source. A back buffer always has the panel's geometry, but
        // reading past the last row of a smaller source would be out of bounds, and this is
        // the only place in the module where two buffers are indexed at once.
        let width = width.min(source.geometry.width.saturating_sub(x));
        let height = height.min(source.geometry.height.saturating_sub(y));
        if width == 0 || height == 0 {
            return;
        }
        let destination_stride = self.geometry.stride * BYTES_PER_PIXEL;
        let source_stride = source.geometry.stride * BYTES_PER_PIXEL;
        let offset = x * BYTES_PER_PIXEL;
        let count = width * BYTES_PER_PIXEL;
        for row in 0..height {
            let from = (y + row) * source_stride + offset;
            let to = (y + row) * destination_stride + offset;
            self.pixels[to..to + count].copy_from_slice(&source.pixels[from..from + count]);
        }
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

    /// Blends `color` into the pixels of `rect` for which `coverage` is non-zero, clipped
    /// to the surface.
    ///
    /// `coverage` is the "how much of this pixel is the shape" answer, in `0..=255`, and it
    /// receives the pixel's *absolute* coordinates. That is the point of this method: a
    /// caller may clip the *area* it paints to a damage rect without changing the *shape* it
    /// is painting, so repainting one corner of a rounded window lands the corner exactly
    /// where a full frame would.
    ///
    /// The fourth byte follows the coverage upwards and never downwards, so a surface built
    /// from several opaque pieces keeps full alpha in the middle while its anti-aliased
    /// outline stays translucent. On the firmware's own surface that byte is padding, and
    /// letting it track coverage is harmless.
    pub fn blend_where(&mut self, rect: Rect, color: Color, coverage: impl Fn(i32, i32) -> u8) {
        let Some((x, y, width, height)) = rect.clip(self.geometry.width, self.geometry.height)
        else {
            return;
        };
        let encoded = self.encode(color, u8::MAX);
        let stride_bytes = self.geometry.stride * BYTES_PER_PIXEL;
        for row in 0..height {
            let start = (y + row) * stride_bytes;
            let line = &mut self.pixels[start..start + stride_bytes];
            for column in 0..width {
                let alpha = coverage((x + column) as i32, (y + row) as i32);
                if alpha == 0 {
                    continue;
                }
                let offset = (x + column) * BYTES_PER_PIXEL;
                let pixel = &mut line[offset..offset + BYTES_PER_PIXEL];
                if alpha == u8::MAX {
                    pixel.copy_from_slice(&encoded);
                } else {
                    for (destination, source) in pixel.iter_mut().zip(encoded.iter()).take(3) {
                        *destination = blend_channel(*destination, *source, alpha);
                    }
                    pixel[3] = pixel[3].max(alpha);
                }
            }
        }
    }

    /// Blends `source` over this surface at the same coordinates, using the source's fourth
    /// byte as per-pixel alpha, clipped to both surfaces.
    ///
    /// The destination is treated as opaque: this is the step that puts a window's own
    /// pixels on the desktop, and a desktop that is not opaque is a different problem. Both
    /// surfaces must agree on channel order - a byte-wise blend of the wrong order is a
    /// red/blue swap, so it is refused rather than drawn wrong.
    ///
    /// Pixels the source does not cover (alpha zero) are left alone, which is what lets a
    /// rounded window keep the desktop's pixels at its corners without a separate mask.
    pub fn blend_surface(&mut self, source: &Canvas<'_>, rect: Rect) {
        if self.order != source.order {
            return;
        }
        let Some((x, y, width, height)) = rect.clip(self.geometry.width, self.geometry.height)
        else {
            return;
        };
        // Clip again against the source: reading past its last row would be out of bounds,
        // and this is the only place in the module where two buffers are indexed at once.
        let width = width.min(source.geometry.width.saturating_sub(x));
        let height = height.min(source.geometry.height.saturating_sub(y));
        if width == 0 || height == 0 {
            return;
        }
        let destination_stride = self.geometry.stride * BYTES_PER_PIXEL;
        let source_stride = source.geometry.stride * BYTES_PER_PIXEL;
        let offset = x * BYTES_PER_PIXEL;
        let count = width * BYTES_PER_PIXEL;
        for row in 0..height {
            let from = (y + row) * source_stride + offset;
            let to = (y + row) * destination_stride + offset;
            let source_row = &source.pixels[from..from + count];
            let destination_row = &mut self.pixels[to..to + count];
            for (src, dst) in source_row
                .as_chunks::<BYTES_PER_PIXEL>()
                .0
                .iter()
                .zip(destination_row.as_chunks_mut::<BYTES_PER_PIXEL>().0)
            {
                match src[3] {
                    0 => {}
                    u8::MAX => dst.copy_from_slice(src),
                    alpha => {
                        for (destination, source) in dst.iter_mut().zip(src.iter()).take(3) {
                            *destination = blend_channel(*destination, *source, alpha);
                        }
                    }
                }
            }
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

/// The four corner radii of a rounded rectangle, clockwise from the top left.
///
/// One radius per corner rather than one for the whole shape: a title bar is rounded where
/// it meets the window's outer edge and square where it meets the body below it, and that
/// distinction is what lets a rounded window be assembled from plain rectangles without a
/// notch appearing at the seam.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Corners {
    pub top_left: u32,
    pub top_right: u32,
    pub bottom_right: u32,
    pub bottom_left: u32,
}

impl Corners {
    /// All four corners square.
    pub const ZERO: Self = Self::new(0, 0, 0, 0);

    pub const fn new(top_left: u32, top_right: u32, bottom_right: u32, bottom_left: u32) -> Self {
        Self {
            top_left,
            top_right,
            bottom_right,
            bottom_left,
        }
    }

    /// The same radius on all four corners, which is what a plain rounded rectangle wants.
    pub const fn uniform(radius: u32) -> Self {
        Self::new(radius, radius, radius, radius)
    }

    /// The same corners reduced so that no two of them overlap on a `width` by `height` rect.
    ///
    /// Without this a radius larger than half the shorter side would turn the coverage test
    /// inside out and paint something that is not a rectangle at all. Reducing every corner by
    /// the same limit keeps the shape symmetric and cannot make a pair of radii exceed an
    /// edge: each is at most `min(width, height) / 2`, so a pair is at most `min(width,
    /// height)`.
    pub fn clamped(self, width: u32, height: u32) -> Self {
        let limit = width.min(height) / 2;
        Self {
            top_left: self.top_left.min(limit),
            top_right: self.top_right.min(limit),
            bottom_right: self.bottom_right.min(limit),
            bottom_left: self.bottom_left.min(limit),
        }
    }
}

/// How much of the pixel at `(x, y)` a rounded rectangle covers, in `0..=255`.
///
/// Sixteen samples per pixel, all in integer arithmetic, so a corner reaches the edge smoothly
/// instead of stepping and neither floating point nor `libm` is needed on a machine that has
/// neither. The radii are independent, so a shape can be round on its outer corners and square
/// on an inner edge.
///
/// The common case is skipped: a pixel that cannot touch a corner is either wholly inside the
/// straight edges (255) or straddles one of them, and only the pixels near a corner are
/// sampled. That is what keeps filling a mostly rectangular window close to the cost of
/// filling a rectangle.
pub fn rounded_rect_coverage(rect: Rect, corners: Corners, x: i32, y: i32) -> u8 {
    /// Samples across each pixel, in each direction.
    const SAMPLES: i64 = 4;
    /// Fixed-point units per pixel: two per sample, so a sample lands in the middle of one.
    const UNITS: i64 = SAMPLES * 2;

    let corners = corners.clamped(rect.width, rect.height);

    let left = i64::from(rect.x) * UNITS;
    let top = i64::from(rect.y) * UNITS;
    let right = (i64::from(rect.x) + i64::from(rect.width)) * UNITS;
    let bottom = (i64::from(rect.y) + i64::from(rect.height)) * UNITS;
    let pixel_left = i64::from(x) * UNITS;
    let pixel_top = i64::from(y) * UNITS;
    let pixel_right = pixel_left + UNITS;
    let pixel_bottom = pixel_top + UNITS;

    // Wholly outside the bounding rectangle: nothing to sample.
    if pixel_right <= left || pixel_left >= right || pixel_bottom <= top || pixel_top >= bottom {
        return 0;
    }

    let top_left = i64::from(corners.top_left) * UNITS;
    let top_right = i64::from(corners.top_right) * UNITS;
    let bottom_right = i64::from(corners.bottom_right) * UNITS;
    let bottom_left = i64::from(corners.bottom_left) * UNITS;

    // Near a corner the outline is curved, so the pixel has to be sampled. Everywhere else it
    // is bounded by straight edges, and a pixel wholly inside them is fully covered.
    let near_corner = (pixel_left < left + top_left && pixel_top < top + top_left)
        || (pixel_right > right - top_right && pixel_top < top + top_right)
        || (pixel_right > right - bottom_right && pixel_bottom > bottom - bottom_right)
        || (pixel_left < left + bottom_left && pixel_bottom > bottom - bottom_left);
    if !near_corner
        && pixel_left >= left
        && pixel_right <= right
        && pixel_top >= top
        && pixel_bottom <= bottom
    {
        return u8::MAX;
    }

    let mut inside = 0i64;
    for row in 0..SAMPLES {
        let sample_y = pixel_top + (2 * row + 1);
        for column in 0..SAMPLES {
            let sample_x = pixel_left + (2 * column + 1);
            if sample_x < left || sample_x >= right || sample_y < top || sample_y >= bottom {
                continue;
            }
            let from_left = sample_x - left;
            let from_top = sample_y - top;
            let from_right = right - sample_x;
            let from_bottom = bottom - sample_y;
            let covered = if from_left < top_left && from_top < top_left {
                within_circle(top_left, from_left, from_top)
            } else if from_right < top_right && from_top < top_right {
                within_circle(top_right, from_right, from_top)
            } else if from_right < bottom_right && from_bottom < bottom_right {
                within_circle(bottom_right, from_right, from_bottom)
            } else if from_left < bottom_left && from_bottom < bottom_left {
                within_circle(bottom_left, from_left, from_bottom)
            } else {
                true
            };
            if covered {
                inside += 1;
            }
        }
    }
    // `SAMPLES * SAMPLES` samples, and `+ half` rounds to nearest rather than always down.
    let samples = SAMPLES * SAMPLES;
    u8::try_from((inside * 255 + samples / 2) / samples).unwrap_or(u8::MAX)
}

/// Whether a sample `from_x` from one edge and `from_y` from the other is inside a corner's
/// quarter circle of `radius`.
///
/// The circle is centred `radius` in from both edges, so a sample near the true corner is
/// outside it: that is exactly the pixel a rounded corner is there to take away.
fn within_circle(radius: i64, from_x: i64, from_y: i64) -> bool {
    let dx = radius - from_x;
    let dy = radius - from_y;
    dx * dx + dy * dy <= radius * radius
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

    #[test]
    fn blitting_copies_the_rect_and_its_padding_stays_put() {
        let mut source_pixels = blank();
        {
            let mut source = canvas(&mut source_pixels);
            source.fill(Color::WHITE);
        }
        let mut pixels = blank();
        {
            let source = canvas(&mut source_pixels);
            let mut destination = canvas(&mut pixels);
            destination.blit(&source, Rect::new(1, 1, 2, 2));
        }
        for (x, y) in [(1, 1), (2, 1), (1, 2), (2, 2)] {
            assert_eq!(
                pixel(&pixels, x, y),
                [u8::MAX; BYTES_PER_PIXEL],
                "({x}, {y})"
            );
        }
        assert_eq!(
            pixel(&pixels, 0, 0),
            [0; BYTES_PER_PIXEL],
            "outside the rect"
        );
        assert_eq!(
            pixel(&pixels, 3, 1),
            [0; BYTES_PER_PIXEL],
            "outside the rect"
        );
        // The two padding columns are not part of the image and must not be copied into.
        for y in 0..3 {
            assert_eq!(
                pixel(&pixels, 4, y),
                [0; BYTES_PER_PIXEL],
                "padding, row {y}"
            );
            assert_eq!(
                pixel(&pixels, 5, y),
                [0; BYTES_PER_PIXEL],
                "padding, row {y}"
            );
        }
    }

    #[test]
    fn blitting_clips_a_rect_that_hangs_off_the_surface() {
        let mut source_pixels = blank();
        {
            let mut source = canvas(&mut source_pixels);
            source.fill(Color::WHITE);
        }
        let mut pixels = blank();
        {
            let source = canvas(&mut source_pixels);
            let mut destination = canvas(&mut pixels);
            // Two columns and two rows of this rect are off the surface, on each side.
            destination.blit(&source, Rect::new(-2, -2, 6, 5));
        }
        // Only the part that is on the surface arrives, at the same coordinates it had.
        for y in 0..3 {
            for x in 0..4 {
                assert_eq!(
                    pixel(&pixels, x, y),
                    [u8::MAX; BYTES_PER_PIXEL],
                    "({x}, {y})"
                );
            }
        }
        assert_eq!(pixel(&pixels, 4, 0), [0; BYTES_PER_PIXEL], "padding");

        let mut untouched = blank();
        {
            let source = canvas(&mut source_pixels);
            let mut destination = canvas(&mut untouched);
            destination.blit(&source, Rect::new(0, 0, 4, 0));
        }
        assert!(
            untouched.iter().all(|byte| *byte == 0),
            "a zero-height rect"
        );
    }

    #[test]
    fn blitting_refuses_a_surface_in_the_other_channel_order() {
        let mut source_pixels = blank();
        {
            let mut source = Canvas::new(&mut source_pixels, GEOMETRY, ChannelOrder::Rgb)
                .expect("geometry fits");
            source.fill(Color::WHITE);
        }
        let mut pixels = blank();
        {
            let source = Canvas::new(&mut source_pixels, GEOMETRY, ChannelOrder::Rgb)
                .expect("geometry fits");
            // This canvas is Bgr, so a byte copy would swap red and blue. Nothing is copied.
            let mut destination = canvas(&mut pixels);
            destination.blit(&source, Rect::new(0, 0, 4, 3));
        }
        assert!(pixels.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn a_rounded_rect_leaves_its_corners_empty_and_fills_the_middle() {
        let shape = Rect::new(0, 0, 20, 20);
        let corners = Corners::uniform(8);
        // The true corner is outside the quarter circle, so nothing lands there.
        assert_eq!(rounded_rect_coverage(shape, corners, 0, 0), 0);
        assert_eq!(rounded_rect_coverage(shape, corners, 19, 0), 0);
        // The straight edges and the middle are fully covered.
        assert_eq!(rounded_rect_coverage(shape, corners, 10, 10), u8::MAX);
        assert_eq!(rounded_rect_coverage(shape, corners, 0, 10), u8::MAX);
        assert_eq!(rounded_rect_coverage(shape, corners, 10, 0), u8::MAX);
        // A pixel the arc cuts through is partly covered, which is the anti-aliasing.
        let partial = rounded_rect_coverage(shape, corners, 2, 2);
        assert!(partial > 0 && partial < u8::MAX, "{partial}");
        // Outside the bounding rectangle there is no coverage at all.
        assert_eq!(rounded_rect_coverage(shape, corners, 20, 10), 0);
        assert_eq!(rounded_rect_coverage(shape, corners, 10, -1), 0);
    }

    #[test]
    fn a_corner_radius_of_zero_is_an_ordinary_rectangle() {
        let shape = Rect::new(-3, 5, 12, 9);
        for (x, y, expected) in [
            (-3, 5, u8::MAX),
            (8, 13, u8::MAX),
            (-4, 5, 0),
            (9, 5, 0),
            (-3, 14, 0),
        ] {
            assert_eq!(
                rounded_rect_coverage(shape, Corners::ZERO, x, y),
                expected,
                "({x}, {y})"
            );
        }
    }

    #[test]
    fn per_corner_radii_round_one_corner_and_square_another() {
        // The top corners are round, the bottom two are square: a title bar's shape.
        let shape = Rect::new(0, 0, 20, 20);
        let corners = Corners::new(8, 8, 0, 0);
        assert_eq!(rounded_rect_coverage(shape, corners, 0, 0), 0);
        assert_eq!(rounded_rect_coverage(shape, corners, 19, 0), 0);
        assert_eq!(rounded_rect_coverage(shape, corners, 0, 19), u8::MAX);
        assert_eq!(rounded_rect_coverage(shape, corners, 19, 19), u8::MAX);
    }

    #[test]
    fn coverage_of_a_uniform_shape_is_the_same_from_opposite_corners() {
        let shape = Rect::new(4, 6, 23, 17);
        let corners = Corners::uniform(7);
        for y in 0..17 {
            for x in 0..23 {
                assert_eq!(
                    rounded_rect_coverage(shape, corners, 4 + x, 6 + y),
                    rounded_rect_coverage(shape, corners, 4 + 22 - x, 6 + 16 - y),
                    "({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn a_radius_larger_than_the_shape_is_clamped_rather_than_wrapping() {
        // Half the shorter side is the largest a corner can be: beyond that the two arcs
        // would cross and the shape would stop being a rectangle.
        let corners = Corners::uniform(1000).clamped(10, 6);
        assert_eq!(corners, Corners::uniform(3));
        let shape = Rect::new(0, 0, 10, 6);
        assert_eq!(rounded_rect_coverage(shape, corners, 5, 3), u8::MAX);
        assert_eq!(rounded_rect_coverage(shape, corners, 0, 0), 0);
    }

    #[test]
    fn blend_where_mixes_the_colour_and_tracks_the_alpha_it_was_given() {
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            canvas.blend_where(Rect::new(1, 1, 1, 1), Color::WHITE, |_, _| 128);
        }
        // Half of white over black is 128 in every colour channel and in alpha, because
        // `blend_where` treats the fourth byte as the coverage it just painted.
        assert_eq!(pixel(&pixels, 1, 1), [128; BYTES_PER_PIXEL]);
        // The neighbours are untouched.
        assert_eq!(pixel(&pixels, 0, 1), [0; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 2, 1), [0; BYTES_PER_PIXEL]);
    }

    #[test]
    fn blend_where_clips_the_area_without_moving_the_shape() {
        // A coverage function that names the left half of the surface, drawn into a rect
        // that is already clipped to the right half: nothing is painted.
        let mut pixels = blank();
        {
            let mut canvas = canvas(&mut pixels);
            canvas.blend_where(Rect::new(2, 0, 2, 3), Color::WHITE, |x, _| {
                if x < 2 {
                    u8::MAX
                } else {
                    0
                }
            });
        }
        assert!(pixels.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn blend_surface_uses_the_source_alpha_and_leaves_a_transparent_pixel_alone() {
        // A source with one opaque pixel, one half-covered pixel and one untouched pixel.
        let mut source_pixels = blank();
        {
            let mut source = canvas(&mut source_pixels);
            source.fill_rect(Rect::new(0, 0, 1, 1), Color::WHITE);
            source.blend_where(Rect::new(1, 0, 1, 1), Color::WHITE, |_, _| 128);
        }
        let mut pixels = blank();
        {
            let source = canvas(&mut source_pixels);
            let mut destination = canvas(&mut pixels);
            destination.fill(Color::BLACK);
            destination.blend_surface(&source, Rect::new(0, 0, 4, 3));
        }
        // An opaque source pixel copies over; a transparent one leaves the black body alone.
        assert_eq!(pixel(&pixels, 0, 0), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&pixels, 2, 0), [0, 0, 0, u8::MAX]);
        // A half-covered source pixel is half of white over black.
        assert_eq!(
            pixel(&pixels, 1, 0),
            [64, 64, 64, u8::MAX],
            "the fourth byte belongs to the destination"
        );
    }

    #[test]
    fn blending_a_surface_in_the_other_channel_order_is_refused() {
        let mut source_pixels = blank();
        {
            let mut source =
                Canvas::new(&mut source_pixels, GEOMETRY, ChannelOrder::Rgb).expect("fits");
            source.fill_rect(Rect::new(0, 0, 4, 3), Color::WHITE);
        }
        let mut pixels = blank();
        {
            let source =
                Canvas::new(&mut source_pixels, GEOMETRY, ChannelOrder::Rgb).expect("fits");
            let mut destination = canvas(&mut pixels);
            destination.blend_surface(&source, Rect::new(0, 0, 4, 3));
        }
        assert!(pixels.iter().all(|byte| *byte == 0));
    }
}
