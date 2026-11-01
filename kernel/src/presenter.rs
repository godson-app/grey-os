//! The frame the panel sees: a back buffer, the damage that goes into it, and the pointer on
//! top.
//!
//! # Three layers, and which one owns what
//!
//! A frame is built in one order, and it is the only order that stays correct when a frame is
//! painted in pieces:
//!
//! 1. The desktop and its windows are painted into the **back buffer**, a full-panel
//!    [`Surface`] in ordinary memory. It is the buffer's pixels, not the panel's, that a
//!    damage repaint blends against, so what the last frame happened to leave in a corner
//!    cannot colour this one.
//! 2. The changed bands are **blitted** onto the panel, whole rows at a time.
//! 3. The **pointer** is drawn onto the panel, never into the buffer.
//!
//! Step 3 is not a detail. A pointer in the back buffer is a pointer that a damage repaint
//! cannot erase, because the repaint would restore the buffer's own pixels over it - the
//! triangle would smear across every window it crossed. Keeping it out makes the buffer a
//! picture of the desktop as it would be with no pointer at all, and the frame that moves the
//! pointer blits both the old and the new triangle's rects from that picture before drawing
//! the pointer again.
//!
//! # Why the buffer is worth its memory
//!
//! Blending reads the destination. On the panel that destination is firmware memory holding
//! the previous frame, so a translucent pixel - the edge of a rounded corner - would blend
//! against it and the result would depend on how the frame was split into damage rects.
//! Against the back buffer every frame starts from the same opaque desktop, which is what
//! makes an incremental frame identical to a whole one instead of merely similar.
//!
//! Everything here is a function of two canvases, so the host tests paint a panel in a `Vec`
//! and compare the incremental frame with a whole one. A difference between them is the bug
//! this module exists to make impossible.

use crate::canvas::{Canvas, ChannelOrder, Rect, BYTES_PER_PIXEL};
use crate::compositor::{pointer_rect, Compositor};
use crate::surface::Surface;

/// A panel's worth of pixels in memory, and where the pointer was on the panel last.
pub struct Presenter {
    /// The desktop as it would be with no pointer on it.
    back: Surface,
    /// Where the last frame left the pointer, so the frame that moves it can erase it. Kept
    /// here rather than in the compositor because erasing is a blit from this buffer, not a
    /// repaint from the window list.
    pointer: Option<(i32, i32)>,
}

impl Presenter {
    /// Allocates a back buffer for a `width` by `height` panel in `order`, or `None` when the
    /// pixels do not fit in memory.
    ///
    /// `order` must be the panel's: the copy onto the panel is by bytes, so a buffer in the
    /// other channel order would put red where blue belongs.
    pub fn new(width: u32, height: u32, order: ChannelOrder) -> Option<Self> {
        Some(Self {
            back: Surface::new(width as usize, height as usize, order)?,
            pointer: None,
        })
    }

    /// Bytes the back buffer occupies, for the boot log.
    pub fn back_buffer_bytes(&self) -> usize {
        self.back.width() * self.back.height() * BYTES_PER_PIXEL
    }

    /// Paints a whole frame: the desktop into the buffer, the buffer onto the panel, then the
    /// pointer on the panel. Returns how many windows were drawn.
    ///
    /// This is the first frame and the frame before a screenshot is taken, where being right
    /// matters more than being cheap.
    pub fn paint_all(
        &mut self,
        desktop: &mut Compositor,
        panel: &mut Canvas<'_>,
        pointer: (i32, i32),
    ) -> usize {
        let windows = {
            let mut back = self.back.canvas();
            desktop.draw_all(&mut back)
        };
        let area = Rect::new(0, 0, self.back.width() as u32, self.back.height() as u32);
        let source = self.back.canvas();
        panel.blit(&source, area);
        self.pointer = Some(pointer);
        present_pointer(desktop, panel, pointer);
        windows
    }

    /// Paints what changed since the last frame: the damage into the buffer, the damaged
    /// bands and the pointer's old and new rects onto the panel, and the pointer last.
    ///
    /// Returns how many damage rects were repainted. Zero means no *damage* was painted, which
    /// is the answer for a frame in which nothing moved - the pointer may still have been erased
    /// and redrawn, which the caller does not have to care about.
    pub fn paint_damage(
        &mut self,
        desktop: &mut Compositor,
        panel: &mut Canvas<'_>,
        pointer: (i32, i32),
    ) -> usize {
        let damage = {
            let mut back = self.back.canvas();
            desktop.draw_damage(&mut back)
        };
        let moved = self.pointer != Some(pointer);
        if damage.is_empty() && !moved {
            return 0;
        }
        {
            let source = self.back.canvas();
            for rect in &damage {
                panel.blit(&source, *rect);
            }
            // The old footprint and the new one, separately: a pointer that moved one pixel
            // still has an overlap, and one blit of each rect covers both without a union.
            if moved {
                if let Some((x, y)) = self.pointer {
                    panel.blit(&source, pointer_rect(x, y));
                }
                panel.blit(&source, pointer_rect(pointer.0, pointer.1));
            }
        }
        self.pointer = Some(pointer);
        present_pointer(desktop, panel, pointer);
        damage.len()
    }
}

/// Draws the pointer on the panel, then throws away the note the compositor made of where the
/// pointer used to be.
///
/// The compositor damages the rect a pointer has left so that a caller painting straight onto
/// the panel can repaint it. Here that erasing has already happened: by the time the pointer is
/// drawn, the panel around it holds the back buffer's pixels. Leaving the note in the list would
/// make the next frame repaint a band that is already correct, and report damage for a frame in
/// which nothing moved.
fn present_pointer(desktop: &mut Compositor, panel: &mut Canvas<'_>, pointer: (i32, i32)) {
    desktop.draw_pointer(panel, pointer.0, pointer.1);
    desktop.windows.take_damage();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{Color, Geometry};
    use crate::theme::GREY;

    /// A panel in memory: four bytes per pixel, red first, which is what the canvas wants.
    struct Panel {
        pixels: Vec<u8>,
        width: usize,
        height: usize,
    }

    impl Panel {
        fn new(width: usize, height: usize) -> Self {
            Self {
                pixels: vec![0; width * height * BYTES_PER_PIXEL],
                width,
                height,
            }
        }

        fn canvas(&mut self) -> Canvas<'_> {
            let geometry = Geometry::new(self.width, self.height, self.width, BYTES_PER_PIXEL);
            Canvas::new(&mut self.pixels, geometry, ChannelOrder::Rgb).expect("the layout is valid")
        }

        fn pixel(&self, x: usize, y: usize) -> Color {
            let index = (y * self.width + x) * BYTES_PER_PIXEL;
            Color::new(
                self.pixels[index],
                self.pixels[index + 1],
                self.pixels[index + 2],
            )
        }
    }

    /// A desktop with two overlapping windows on a 240x180 panel.
    fn desktop() -> Compositor {
        let mut desktop = Compositor::new(240, 180, GREY);
        desktop.create("log", vec![String::from("hello")], 140, 110);
        desktop.create("about", vec![String::from("two")], 120, 90);
        desktop
    }

    fn buffer() -> Presenter {
        Presenter::new(240, 180, ChannelOrder::Rgb).expect("the buffer fits")
    }

    #[test]
    fn a_frame_through_the_buffer_is_what_a_direct_frame_paints() {
        // The buffer earns its memory only if it is invisible: the same compositor, painted
        // straight onto a panel and painted through the buffer, has to agree byte for byte.
        let mut desktop = desktop();
        let mut direct = Panel::new(240, 180);
        let mut through = Panel::new(240, 180);
        let mut presenter = buffer();

        desktop.draw_all(&mut direct.canvas());
        desktop.draw_pointer(&mut direct.canvas(), 12, 9);
        presenter.paint_all(&mut desktop, &mut through.canvas(), (12, 9));

        assert_eq!(through.pixels, direct.pixels);
    }

    #[test]
    fn a_damage_frame_leaves_the_panel_as_a_whole_frame_would() {
        // The headline invariant: a frame painted as damage rects and a frame painted whole
        // are the same picture. Three kinds of change at once - a raise, which repaints two
        // title bars because focus moved; a move, which leaves the desktop behind the window;
        // and a pointer that goes somewhere else entirely.
        let mut desktop = desktop();
        let log = desktop.windows.iter().next().expect("a window").id;
        let about = desktop.windows.iter().last().expect("a window").id;

        let mut panel = Panel::new(240, 180);
        let mut presenter = buffer();
        presenter.paint_all(&mut desktop, &mut panel.canvas(), (16, 16));

        assert!(desktop.windows.raise(log), "the raise has to have happened");
        assert!(desktop.windows.move_to(about, 40, 30), "and the move");
        let painted = presenter.paint_damage(&mut desktop, &mut panel.canvas(), (180, 120));
        assert!(painted > 0, "three changes are not an empty frame");

        let mut whole = Panel::new(240, 180);
        let mut fresh = buffer();
        fresh.paint_all(&mut desktop, &mut whole.canvas(), (180, 120));

        assert_eq!(panel.pixels, whole.pixels);
    }

    #[test]
    fn a_damage_rect_over_the_pointer_does_not_erase_it() {
        // The pointer lives on the panel, not in the buffer. A repaint of the rect under it
        // blits the buffer's own pixels back over the triangle, so the triangle has to be
        // drawn after that blit - which is the order this module claims.
        let mut desktop = Compositor::new(240, 180, GREY);
        let mut panel = Panel::new(240, 180);
        let mut presenter = buffer();
        presenter.paint_all(&mut desktop, &mut panel.canvas(), (8, 8));

        desktop.windows.damage(pointer_rect(8, 8));
        assert!(presenter.paint_damage(&mut desktop, &mut panel.canvas(), (8, 8)) > 0);

        assert_eq!(panel.pixel(8, 8), GREY.pointer_edge);
        assert_eq!(panel.pixel(9, 8), GREY.pointer);
        // Inside the damaged rect, outside the triangle: the blit put the desktop back.
        assert_eq!(panel.pixel(11, 8), GREY.desktop);
    }

    #[test]
    fn a_pointer_that_moves_leaves_nothing_behind() {
        let mut desktop = Compositor::new(240, 180, GREY);
        let mut panel = Panel::new(240, 180);
        let mut presenter = buffer();
        presenter.paint_all(&mut desktop, &mut panel.canvas(), (10, 10));
        assert_eq!(panel.pixel(11, 10), GREY.pointer);

        // No window moved, no line was added: the move is the whole frame.
        let painted = presenter.paint_damage(&mut desktop, &mut panel.canvas(), (200, 150));
        assert_eq!(painted, 0, "a pointer move repaints no damage rects");
        assert_eq!(panel.pixel(11, 10), GREY.desktop, "the old one was erased");
        assert_eq!(panel.pixel(201, 150), GREY.pointer, "the new one was drawn");
    }

    #[test]
    fn a_frame_with_nothing_to_do_does_not_touch_the_panel() {
        let mut desktop = Compositor::new(240, 180, GREY);
        let mut panel = Panel::new(240, 180);
        let mut presenter = buffer();
        presenter.paint_all(&mut desktop, &mut panel.canvas(), (30, 30));

        // A pixel no blit would cover, so writing it is a witness: if the frame copied
        // anything it would have been overwritten.
        panel.pixels[0] = 0xaa;
        assert_eq!(
            presenter.paint_damage(&mut desktop, &mut panel.canvas(), (30, 30)),
            0
        );
        assert_eq!(panel.pixels[0], 0xaa);
    }
}
