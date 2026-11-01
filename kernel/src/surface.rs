//! Offscreen surfaces: the pixels a window is drawn into before the compositor puts them on
//! the desktop.
//!
//! [`Canvas`] is a view - it borrows pixels it does not own, which is what lets the host tests
//! paint into a `Vec` and read it back. A [`Surface`] is the other half: it owns the pixels, so
//! a window can keep its own image between frames and the compositor can blend it onto the
//! panel with the window's alpha channel intact.
//!
//! The layout is deliberately the panel's: same four bytes per pixel and the same channel
//! order, because [`Canvas::blend_surface`] blends the two byte-for-byte and has no conversion
//! to do. A surface is empty (fully transparent) until something fills it; drawing a rounded
//! shape into it leaves the corners transparent, and compositing it later lets the desktop show
//! through them.

use alloc::vec::Vec;

use crate::canvas::{Canvas, ChannelOrder, Color, Geometry, Rect, BYTES_PER_PIXEL};

/// A set of pixels the kernel owns, with the canvas view over them.
pub struct Surface {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
    order: ChannelOrder,
}

impl Surface {
    /// Allocates a `width` by `height` surface, every pixel transparent, or `None` if the size
    /// is empty or does not fit in memory.
    ///
    /// Zeroed rather than left as the allocator found it: a transparent corner is the whole
    /// point of a rounded window, and garbage there would be painted as though it were content.
    pub fn new(width: usize, height: usize, order: ChannelOrder) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let len = width.checked_mul(height)?.checked_mul(BYTES_PER_PIXEL)?;
        Some(Self {
            pixels: alloc::vec![0; len],
            width,
            height,
            order,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn order(&self) -> ChannelOrder {
        self.order
    }

    /// The pixels as a canvas, for drawing into.
    ///
    /// The geometry is built from the size the surface was allocated with, so [`Canvas::new`]
    /// cannot refuse it: the buffer is exactly `width * height` pixels wide and tall.
    pub fn canvas(&mut self) -> Canvas<'_> {
        let geometry = Geometry::new(self.width, self.height, self.width, BYTES_PER_PIXEL);
        Canvas::new(&mut self.pixels, geometry, self.order)
            .expect("a surface's own geometry always fits its own buffer")
    }

    /// Makes every pixel transparent again, for a surface reused from the previous frame.
    pub fn clear(&mut self) {
        self.pixels.fill(0);
    }

    /// Fills every pixel with `color`, opaque.
    pub fn fill(&mut self, color: Color) {
        self.canvas().fill(color);
    }

    /// Blends this surface onto `destination` with its own alpha channel, at `(x, y)`.
    ///
    /// The destination is assumed opaque: the source's alpha is applied to the colour channels
    /// only, and an opaque source pixel replaces all four bytes of the destination's. A
    /// destination that was itself translucent would need its own alpha in the arithmetic, which
    /// is the formula a stack of translucent surfaces needs and a desktop does not (ADR-042).
    ///
    /// Takes `&mut self` even though it only reads: [`Canvas::new`] wants a mutable borrow of
    /// the pixels, and adding a second, read-only constructor solely so this method could take
    /// `&self` would give the module two ways to describe the same bytes.
    pub fn composite_onto(&mut self, destination: &mut Canvas<'_>, x: i32, y: i32) {
        let area = Rect::new(x, y, self.width as u32, self.height as u32);
        let source = self.canvas();
        destination.blend_surface(&source, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{rounded_rect_coverage, Corners};

    fn surface(width: usize, height: usize) -> Surface {
        Surface::new(width, height, ChannelOrder::Rgb).expect("the size is valid")
    }

    /// The raw bytes of one pixel, as `[r, g, b, a]`.
    ///
    /// Read straight out of the surface's own buffer: this is a child module, so the private
    /// pixels are visible here, and going through a canvas would only re-derive the offset.
    fn pixel(surface: &Surface, x: usize, y: usize) -> [u8; BYTES_PER_PIXEL] {
        let offset = (y * surface.width + x) * BYTES_PER_PIXEL;
        surface.pixels[offset..offset + BYTES_PER_PIXEL]
            .try_into()
            .expect("a pixel is four bytes")
    }

    #[test]
    fn a_surface_rejects_a_size_it_cannot_hold() {
        assert!(Surface::new(0, 10, ChannelOrder::Rgb).is_none());
        assert!(Surface::new(10, 0, ChannelOrder::Rgb).is_none());
        // A size whose pixel count overflows a `usize` is refused rather than allocated.
        assert!(Surface::new(usize::MAX, 2, ChannelOrder::Rgb).is_none());
    }

    #[test]
    fn a_new_surface_is_transparent_and_its_canvas_matches_its_size() {
        let mut surface = surface(5, 3);
        let geometry = surface.canvas().geometry();
        assert_eq!(geometry.width, 5);
        assert_eq!(geometry.height, 3);
        assert_eq!(geometry.stride, 5);
        assert_eq!(pixel(&surface, 0, 0), [0; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&surface, 4, 2), [0; BYTES_PER_PIXEL]);
    }

    #[test]
    fn filling_then_compositing_an_opaque_surface_copies_the_colour() {
        let mut source = surface(4, 3);
        source.fill(Color::WHITE);
        let mut destination = surface(4, 3);
        destination.fill(Color::BLACK);
        source.composite_onto(&mut destination.canvas(), 0, 0);
        assert_eq!(pixel(&destination, 1, 1), [u8::MAX; BYTES_PER_PIXEL]);
    }

    #[test]
    fn a_cleared_surface_composites_nothing() {
        let mut source = surface(4, 3);
        source.fill(Color::WHITE);
        source.clear();
        let mut destination = surface(4, 3);
        destination.fill(Color::BLACK);
        source.composite_onto(&mut destination.canvas(), 0, 0);
        assert_eq!(pixel(&destination, 1, 1), [0, 0, 0, u8::MAX]);
    }

    #[test]
    fn a_rounded_shape_in_a_surface_leaves_its_corner_to_the_destination() {
        // A white rounded square in a transparent surface, composited over black: the middle
        // is white and the corner is still black, which is the whole rounded-window effect.
        let mut source = surface(20, 20);
        let shape = Rect::new(0, 0, 20, 20);
        let corners = Corners::uniform(8);
        source.canvas().blend_where(shape, Color::WHITE, |x, y| {
            rounded_rect_coverage(shape, corners, x, y)
        });
        let mut destination = surface(20, 20);
        destination.fill(Color::BLACK);
        source.composite_onto(&mut destination.canvas(), 0, 0);
        assert_eq!(pixel(&destination, 10, 10), [u8::MAX; BYTES_PER_PIXEL]);
        assert_eq!(pixel(&destination, 0, 0), [0, 0, 0, u8::MAX]);
        // The arc leaves that pixel partly white rather than all or nothing.
        let edge = pixel(&destination, 2, 2);
        assert!(edge[0] > 0 && edge[0] < u8::MAX, "{edge:?}");
    }
}
