//! The screen side of the boot log.
//!
//! `console.rs` decides where characters go; this module owns the one console instance and
//! paints it, so the boot sequence and the panic handler share a log without passing
//! references between them. `x86_64`-only, because the surface it paints comes from
//! firmware and the pointer it keeps is only sound on the machine that booted.

use alloc::boxed::Box;
use core::fmt::{self, Write as _};
use core::panic::PanicInfo;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::canvas::{Color, Rect};
use crate::console::Console;
use crate::framebuffer;
use crate::text;

/// Panel pixels per font pixel.
///
/// 2 turns the 8x8 glyphs into 16x16 cells: 80x50 characters on the 1280x800 surface OVMF
/// picks, which the boot log fits inside with room to spare. The number is a compromise -
/// 1 is too small to read from a normal viewing distance, 3 wraps the longer memory-map
/// lines.
pub const SCALE: usize = 2;

/// Pixels of panel left clear around the grid, so the log does not touch the edge.
pub const MARGIN: usize = 16;

/// Colour of the log text: a near-white with a hint of blue, readable on the teal
/// background and on the panic red.
pub const FOREGROUND: Color = Color::new(0xe8, 0xf0, 0xf4);

/// The console, once [`init`] has made one.
static CONSOLE: AtomicPtr<Console> = AtomicPtr::new(ptr::null_mut());

/// Allocates the console for the installed framebuffer and paints its first blank screen.
///
/// Requires `framebuffer::install` and a working heap, in that order. Returns `false` when
/// there is no framebuffer or the panel has no room for even one character cell; the boot
/// sequence reports both and fails, because a machine that boots into a blank panel with
/// no explanation is the failure mode this path exists to prevent.
pub fn init() -> bool {
    let mut grid = (0, 0);
    let has_screen = framebuffer::with_screen(|screen| {
        let info = screen.info();
        grid = text::grid_size(
            info.width.saturating_sub(2 * MARGIN),
            info.height.saturating_sub(2 * MARGIN),
            SCALE,
        );
    });
    if !has_screen {
        return false;
    }
    let Some(console) = Console::new(grid.0, grid.1) else {
        return false;
    };
    // Leaked on purpose: the console lives as long as the kernel and needs an owner that
    // can hand out a `&'static mut`, which the boot stack frame cannot.
    install(Box::leak(Box::new(console)));
    paint(false, 0);
    true
}

/// The console's size in characters, or `None` before [`init`].
pub fn grid() -> Option<(usize, usize)> {
    with_console(|console| (console.columns(), console.rows()))
}

/// Hands the console to this module for the rest of the run.
///
/// Same reasoning as `framebuffer::install`: storing the pointer consumes the only `&mut`,
/// which is what lets [`with_console`] hand out references without aliasing one.
pub fn install(console: &'static mut Console) {
    CONSOLE.store(console, Ordering::Release);
}

/// Runs `draw` on the console, or returns `None` if there is not one yet.
fn with_console<R>(draw: impl FnOnce(&mut Console) -> R) -> Option<R> {
    let pointer = CONSOLE.load(Ordering::Acquire);
    if pointer.is_null() {
        return None;
    }
    // SAFETY: `install` takes a `&'static mut` and stores its address, so a non-null
    // `CONSOLE` points at a console that outlives the process. Storing it consumed that
    // borrow, so no other reference exists; the kernel is single-threaded, `draw` runs
    // straight through and nothing it calls comes back here. The panic path uses this too,
    // which is why nothing here locks or allocates.
    Some(draw(unsafe { &mut *pointer }))
}

/// Prints one line to the console and repaints the rows it touched.
///
/// A no-op before [`init`]: the boot sequence logs its first line before the heap the
/// console sits on exists, and that one is serial-only by nature.
pub fn print_args(args: fmt::Arguments<'_>) {
    let scrolled = with_console(|console| {
        // The newline is added here so callers keep writing lines rather than fragments.
        let _ = console.write_fmt(args);
        console.write("\n");
        console.take_scrolled_rows()
    });
    if let Some(rows) = scrolled {
        paint(true, rows);
    }
}

/// Paints the panic screen: the log so far and the report, on the panic colour.
///
/// Returns whether there was a framebuffer to paint. Writing the report into the console
/// keeps the last lines of the log visible above it, which on a machine with no serial
/// cable is the only diagnosis available.
pub fn paint_panic(info: &PanicInfo<'_>) -> bool {
    if !framebuffer::with_screen(|screen| framebuffer::fill(screen, framebuffer::PANIC)) {
        return false;
    }
    with_console(|console| {
        console.write("\n*** PANIC ***\n");
        let _ = console.write_fmt(format_args!("{info}"));
    });
    // The background colour changed, so every row is stale even though no text moved.
    paint(false, 0);
    true
}

/// Paints the console over the boot background.
///
/// `only_dirty` limits the work to the rows that changed; `scrolled_rows` is how far the
/// grid moved since the last paint, which is cheaper to copy than to redraw.
///
/// Returns whether a console and a drawable surface were both there.
fn paint(only_dirty: bool, scrolled_rows: usize) -> bool {
    let mut painted = false;
    framebuffer::with_screen(|screen| {
        let Some(mut canvas) = framebuffer::canvas(screen) else {
            return;
        };
        let origin = (MARGIN as i32, MARGIN as i32);
        let background = framebuffer::BACKGROUND;
        let rendered = with_console(|console| {
            if !only_dirty {
                console.render(&mut canvas, origin, FOREGROUND, background, SCALE);
                return;
            }
            if scrolled_rows > 0 {
                let (cell_width, cell_height) = text::cell_size(SCALE);
                let area = Rect::new(
                    origin.0,
                    origin.1,
                    (console.columns() as u64 * u64::from(cell_width)).min(u64::from(u32::MAX))
                        as u32,
                    (console.rows() as u64 * u64::from(cell_height)).min(u64::from(u32::MAX))
                        as u32,
                );
                canvas.scroll_up(
                    area,
                    (scrolled_rows as u32).saturating_mul(cell_height),
                    background,
                );
            }
            console.present(&mut canvas, origin, FOREGROUND, background, SCALE);
        });
        painted = rendered.is_some();
    });
    painted
}
