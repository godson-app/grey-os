//! The screen side of the boot log.
//!
//! `console.rs` decides where characters go; this module owns the one console instance, the
//! back buffer it is drawn into, and the copy that puts both on the panel - so the boot
//! sequence and the panic handler share a log without passing references between them.
//! `x86_64`-only, because the surface it paints comes from firmware and the pointers it
//! keeps are only sound on the machine that booted.

use alloc::alloc::{alloc_zeroed, Layout};
use alloc::boxed::Box;
use core::fmt::{self, Write as _};
use core::panic::PanicInfo;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::canvas::{Canvas, ChannelOrder, Color, Geometry, Rect};
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

/// Top-left corner of the grid on the panel.
const ORIGIN: (i32, i32) = (MARGIN as i32, MARGIN as i32);

/// Colour of the log text: a near-white with a hint of blue, readable on the teal
/// background and on the panic red.
pub const FOREGROUND: Color = Color::new(0xe8, 0xf0, 0xf4);

/// The console, once [`init`] has made one.
static CONSOLE: AtomicPtr<Console> = AtomicPtr::new(ptr::null_mut());

/// The pixels the console is drawn into before they reach the panel.
///
/// One full-screen surface in ordinary memory. Drawing a line then touches a few kilobytes
/// of RAM instead of the same pixels one at a time in the panel's own memory, and the panel
/// is written in whole rows by [`Canvas::blit`] afterwards. The layout is the panel's,
/// because that copy is by bytes (ADR-026).
struct BackBuffer {
    base: *mut u8,
    len: usize,
    geometry: Geometry,
    order: ChannelOrder,
}

static BACK_BUFFER: AtomicPtr<BackBuffer> = AtomicPtr::new(ptr::null_mut());

/// Allocates the back buffer for a panel with `geometry` and `order`.
///
/// Returns whether there is now one. A failed allocation is not fatal: the console falls
/// back to drawing straight onto the panel, which is slower but correct, and the boot log
/// says which of the two happened.
fn install_back_buffer(geometry: Geometry, order: ChannelOrder) -> bool {
    let Some(len) = geometry.byte_len() else {
        return false;
    };
    let Ok(layout) = Layout::from_size_align(len, align_of::<usize>()) else {
        return false;
    };
    // Zeroed even though the console paints every row it owns before anything is copied
    // out: a surface that started as garbage would paint noise over the panel the moment
    // some future path blitted a row it had not drawn.
    //
    // SAFETY: `layout` has a non-zero size - it comes from the byte length of a panel that
    // has a width and a height - and the allocator is this crate's, which `kernel_main`
    // initialises before it calls `display::init`.
    let base = unsafe { alloc_zeroed(layout) };
    if base.is_null() {
        return false;
    }
    // Leaked like the console: the kernel owns it for the rest of the run, and the pointer
    // has to outlive the boot stack frame that allocated it.
    let buffer = Box::leak(Box::new(BackBuffer {
        base,
        len,
        geometry,
        order,
    }));
    BACK_BUFFER.store(buffer, Ordering::Release);
    true
}

/// Bytes the back buffer occupies, or `None` when there is not one.
pub fn back_buffer_bytes() -> Option<usize> {
    let pointer = BACK_BUFFER.load(Ordering::Acquire);
    if pointer.is_null() {
        return None;
    }
    // SAFETY: `install_back_buffer` stores a leaked buffer and never replaces it.
    Some(unsafe { (*pointer).len })
}

/// Runs `draw` on a canvas over the back buffer, or returns `None` if there is not one.
fn with_back_buffer<R>(draw: impl FnOnce(&mut Canvas<'_>) -> R) -> Option<R> {
    let pointer = BACK_BUFFER.load(Ordering::Acquire);
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the buffer is leaked, so a non-null pointer stays valid for the rest of the
    // run, and its layout was fixed when it was allocated.
    let buffer = unsafe { &*pointer };
    // SAFETY: `base` and `len` describe that allocation, and no other reference to those
    // pixels is live. The slice is rebuilt per call instead of being kept as a
    // `&'static mut`: one long-lived `&mut` handed out twice would be two live mutable
    // references, whereas here each call's borrow ends with the call. The kernel is
    // single-threaded and `draw` runs straight through, the same argument as for the
    // console and the framebuffer.
    let pixels = unsafe { core::slice::from_raw_parts_mut(buffer.base, buffer.len) };
    let mut canvas = Canvas::new(pixels, buffer.geometry, buffer.order)?;
    Some(draw(&mut canvas))
}

/// Allocates the console for the installed framebuffer and paints its first blank screen.
///
/// Requires `framebuffer::install` and a working heap, in that order. Returns `false` when
/// there is no framebuffer or the panel has no room for even one character cell; the boot
/// sequence reports both and fails, because a machine that boots into a blank panel with
/// no explanation is the failure mode this path exists to prevent.
pub fn init() -> bool {
    let mut grid = (0, 0);
    let mut panel = None;
    let has_screen = framebuffer::with_screen(|screen| {
        panel = framebuffer::layout(screen);
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
    // Before the first paint, which draws into it. A panel whose layout the renderer cannot
    // use has already failed by here, so the buffer is built with the panel's own geometry
    // and channel order and the copy onto it is a plain row copy.
    if let Some((geometry, order)) = panel {
        install_back_buffer(geometry, order);
    }
    // Leaked on purpose: the console lives as long as the kernel and needs an owner that
    // can hand out a `&'static mut`, which the boot stack frame cannot.
    install(Box::leak(Box::new(console)));
    paint(false, 0, framebuffer::BACKGROUND);
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
        paint(true, rows, framebuffer::BACKGROUND);
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
    // The background colour changed, so every row is stale even though no text moved. The
    // fill above covered the margins, and this repaint covers the grid - in the back buffer
    // first, like any other line, so the panel is written once. It has to be told the new
    // colour: without that the grid would be repainted on the boot background and the panic
    // screen would only be red around the text.
    paint(false, 0, framebuffer::PANIC);
    true
}

/// Paints the console over `background`.
///
/// `only_dirty` limits the work to the rows that changed; `scrolled_rows` is how far the
/// grid moved since the last paint, which is cheaper to copy than to redraw.
///
/// The drawing happens in the back buffer when there is one, and only the area that changed
/// is copied onto the panel: a line append moves one band of pixels, a scroll moves the
/// whole grid because every row of it moved. Without a back buffer the console draws
/// straight onto the panel, which is what the first boot did and what a failed allocation
/// still falls back to.
///
/// Returns whether a console and a drawable surface were both there.
fn paint(only_dirty: bool, scrolled_rows: usize, background: Color) -> bool {
    with_console(|console| {
        let mut painted = false;
        framebuffer::with_screen(|screen| {
            let Some(mut panel) = framebuffer::canvas(screen) else {
                return;
            };
            let copied = with_back_buffer(|back| {
                let area = draw(console, back, only_dirty, scrolled_rows, background);
                panel.blit(back, area);
            });
            if copied.is_none() {
                draw(console, &mut panel, only_dirty, scrolled_rows, background);
            }
            painted = true;
        });
        painted
    })
    .unwrap_or(false)
}

/// Draws the console onto `target` and returns the area of it whose pixels changed.
///
/// A scroll moves the pixels of the whole grid, so everything is reported as changed; a
/// line append reports the rows from the first stale one down, which may be none.
fn draw(
    console: &mut Console,
    target: &mut Canvas<'_>,
    only_dirty: bool,
    scrolled_rows: usize,
    background: Color,
) -> Rect {
    let area = grid_area(console);
    if !only_dirty {
        console.render(target, ORIGIN, FOREGROUND, background, SCALE);
        // Re-rendering the grid made it current again, so the next line has only its own
        // row to paint instead of the whole screen.
        console.mark_painted();
        return area;
    }
    if scrolled_rows > 0 {
        let (_, cell_height) = text::cell_size(SCALE);
        target.scroll_up(
            area,
            (scrolled_rows as u32).saturating_mul(cell_height),
            background,
        );
        console.present(target, ORIGIN, FOREGROUND, background, SCALE);
        return area;
    }
    let first = console.dirty_from();
    console.present(target, ORIGIN, FOREGROUND, background, SCALE);
    rows_area(console, first)
}

/// The area the whole grid occupies on the panel.
fn grid_area(console: &Console) -> Rect {
    let (cell_width, cell_height) = text::cell_size(SCALE);
    Rect::new(
        ORIGIN.0,
        ORIGIN.1,
        cells_to_pixels(console.columns(), cell_width),
        cells_to_pixels(console.rows(), cell_height),
    )
}

/// The area of the rows from `first` to the bottom of the grid, empty if `first` is the
/// first row below it.
fn rows_area(console: &Console, first: usize) -> Rect {
    let (cell_width, cell_height) = text::cell_size(SCALE);
    let top = ORIGIN
        .1
        .saturating_add(i32::try_from(cells_to_pixels(first, cell_height)).unwrap_or(i32::MAX));
    Rect::new(
        ORIGIN.0,
        top,
        cells_to_pixels(console.columns(), cell_width),
        cells_to_pixels(console.rows().saturating_sub(first), cell_height),
    )
}

/// `cells` cells of `size` pixels each, clamped to what a [`Rect`] can hold.
fn cells_to_pixels(cells: usize, size: u32) -> u32 {
    (cells as u64 * u64::from(size)).min(u64::from(u32::MAX)) as u32
}
