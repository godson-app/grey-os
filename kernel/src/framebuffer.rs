//! Bridges the firmware's GOP framebuffer to the renderer in [`crate::canvas`].
//!
//! The bootloader hands over a raw byte buffer plus a description of how it is laid
//! out, and that description is firmware data rather than something we chose: it is
//! read once, when the surface is wrapped in a [`Canvas`], and a layout this renderer
//! cannot draw is reported instead of guessed at. There is deliberately no per-pixel
//! arithmetic here - `Canvas::new` rejects any geometry whose indices could fall
//! outside the buffer, so every draw after it is in bounds by construction.

use bootloader_api::info::{FrameBuffer, PixelFormat};
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::canvas::{Canvas, ChannelOrder, Color, Geometry};

/// Painted behind the boot log.
pub const BACKGROUND: Color = Color::new(0x1f, 0x6f, 0x8b);

/// Painted over the screen when the kernel panics, so a failure is visible on a
/// machine with no serial cable attached.
pub const PANIC: Color = Color::new(0xb0, 0x1b, 0x2f);

/// Reads the firmware's layout, or `None` if this renderer cannot draw into it.
///
/// Public because a back buffer has to be built with exactly the panel's geometry and
/// channel order: it is copied onto the panel byte for byte (see [`Canvas::blit`]).
pub fn layout(framebuffer: &FrameBuffer) -> Option<(Geometry, ChannelOrder)> {
    let info = framebuffer.info();
    let order = match info.pixel_format {
        PixelFormat::Rgb => ChannelOrder::Rgb,
        PixelFormat::Bgr => ChannelOrder::Bgr,
        // `U8` is a palette surface: one byte per pixel, no channels to blend. Guessing
        // at a layout would paint noise over the whole screen, so refuse it - and
        // refuse anything a newer firmware might report.
        _ => return None,
    };
    let geometry = Geometry::new(info.width, info.height, info.stride, info.bytes_per_pixel);
    Some((geometry, order))
}

/// Wraps the live framebuffer in a [`Canvas`], or `None` if it cannot be drawn.
pub fn canvas(framebuffer: &mut FrameBuffer) -> Option<Canvas<'_>> {
    let (geometry, order) = layout(framebuffer)?;
    Canvas::new(framebuffer.buffer_mut(), geometry, order)
}

/// Fills the whole screen with `color`.
///
/// Returns `false` when the firmware layout is unsupported, so the caller can report
/// that rather than exit as though the screen had been painted.
pub fn fill(framebuffer: &mut FrameBuffer, color: Color) -> bool {
    match canvas(framebuffer) {
        Some(mut surface) => {
            surface.fill(color);
            true
        }
        None => false,
    }
}

/// The kernel is handed exactly one GOP framebuffer at boot and that memory stays
/// mapped for the whole run, so the address is stored rather than the borrow.
static SCREEN: AtomicPtr<FrameBuffer> = AtomicPtr::new(core::ptr::null_mut());

/// Publishes the boot framebuffer so the panic handler, which runs with no caller in
/// scope, can still paint the screen. The borrow is `'static` so the stored pointer
/// has no expiry, and storing it consumes the only `&mut` to that framebuffer, which
/// is what lets [`with_screen`] hand out a reference without aliasing one.
pub fn install(framebuffer: &'static mut FrameBuffer) {
    SCREEN.store(framebuffer, Ordering::Release);
}

/// Runs `draw` on the installed framebuffer, returning whether there was one.
pub fn with_screen<R>(draw: impl FnOnce(&mut FrameBuffer) -> R) -> bool {
    let pointer = SCREEN.load(Ordering::Acquire);
    if pointer.is_null() {
        return false;
    }
    // SAFETY: `install` takes a `&'static mut` and stores its address, so a non-null
    // `SCREEN` points at a framebuffer that outlives the process. Storing it consumed
    // that borrow, so no other reference to it exists. The kernel is single-threaded,
    // `draw` runs straight through, and nothing it calls comes back here, so the
    // reference is created once and never aliased. This path also runs on panic, which
    // is why it takes no lock and allocates nothing: a red screen is the last thing
    // the system does before halting.
    let screen = unsafe { &mut *pointer };
    draw(screen);
    true
}
