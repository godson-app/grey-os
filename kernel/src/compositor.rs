//! The compositor: the desktop, its windows and the pointer, onto one canvas.
//!
//! # What it does, in order
//!
//! Fill the desktop, then each window back to front, then the pointer. That order is the
//! whole z-ordering: a window painted later covers an earlier one, so the list's order is
//! the stack and there is nothing else to get wrong.
//!
//! # Damage
//!
//! [`Compositor::draw_all`] is for the first frame. Every frame after it is
//! [`Compositor::draw_damage`], which asks the window list what changed, merges the answer,
//! and repaints *only* those rectangles - background, then whichever windows reach into them,
//! then the pointer. The clipping is not an optimisation detail: a composite that repaints
//! outside the damage is a composite that has to repaint everything to stay correct.
//!
//! # Why it is testable
//!
//! Everything here is a function of a `Canvas`, which is a slice of bytes with a geometry. So
//! the host tests build a small surface, draw a desktop onto it, and read pixels back: a
//! title bar is the title bar's colour, the overlap of two windows belongs to the one in
//! front, and a damage rect that covers one window leaves the other's pixels alone.

use alloc::string::String;
use alloc::vec::Vec;

use crate::canvas::{Canvas, Color, Rect};
use crate::text;
use crate::theme::Theme;
use crate::window::{Window, Windows};

/// A window's text is truncated to this many characters, so a very long line cannot be asked
/// to draw ten thousand glyphs that are all clipped away.
const MAX_LINE_CHARS: usize = 200;

/// The desktop: the window list, the theme, and the count of frames drawn.
pub struct Compositor {
    /// The colours and measurements everything is drawn with.
    pub theme: Theme,
    /// The windows, back to front.
    pub windows: Windows,
    /// How many frames have been painted, for the report.
    pub frames: u64,
    /// Where [`Self::draw_pointer`] last put the pointer, so a frame that moves it can have
    /// the desktop repaint the triangle it left behind. The pointer is painted straight onto
    /// the panel rather than composited from a damage rect, so nothing else covers it, and a
    /// pointer that is never erased is a smear of triangles across every window it crosses.
    pointer: Option<(i32, i32)>,
}

impl Compositor {
    /// An empty desktop on a `width` by `height` panel.
    pub fn new(width: u32, height: u32, theme: Theme) -> Self {
        Self {
            theme,
            windows: Windows::new(width, height, theme.cascade),
            frames: 0,
            pointer: None,
        }
    }

    /// Creates a window at the next cascade position. See [`Windows::create`].
    pub fn create(&mut self, title: &str, lines: Vec<String>, width: u32, height: u32) -> u32 {
        self.windows.create(title, lines, width, height)
    }

    /// Paints the whole panel and returns how many windows it drew.
    ///
    /// It also clears the damage list: after a full frame nothing is pending any more, and
    /// leaving it there would make the next [`Self::draw_damage`] repaint what was already
    /// correct.
    pub fn draw_all(&mut self, canvas: &mut Canvas<'_>) -> usize {
        self.frames += 1;
        self.paint(canvas, None);
        self.windows.take_damage();
        self.windows.count()
    }

    /// Paints the pending damage and returns the rects it repainted, merged.
    ///
    /// An empty answer means nothing changed, and the canvas was not touched at all.
    pub fn draw_damage(&mut self, canvas: &mut Canvas<'_>) -> Vec<Rect> {
        let damage = self.windows.take_damage();
        if damage.is_empty() {
            return damage;
        }
        self.frames += 1;
        for rect in &damage {
            self.paint(canvas, Some(*rect));
        }
        damage
    }

    /// Draws the pointer at `(x, y)` and returns the area it covers, so a caller can damage
    /// it again when it moves.
    ///
    /// The shape is a filled triangle with a one-pixel dark outline, drawn row by row: at this
    /// size the outline is what keeps a white pointer visible over a white window.
    ///
    /// A move damages the rect the last call covered, because the triangle is painted onto the
    /// panel rather than composited: the desktop is what has to erase the old one, and it does
    /// that on the next frame - the caller's [`Self::draw_damage`] or [`Self::draw_all`].
    pub fn draw_pointer(&mut self, canvas: &mut Canvas<'_>, x: i32, y: i32) -> Rect {
        if let Some((previous_x, previous_y)) = self.pointer {
            if (previous_x, previous_y) != (x, y) {
                self.windows.damage(pointer_rect(previous_x, previous_y));
            }
        }
        self.pointer = Some((x, y));
        const HEIGHT: i32 = 16;
        for row in 0..HEIGHT {
            let width = (row * 2 / 3).max(1);
            // The outline is one pixel wider than the fill, so the fill never touches the
            // desktop directly.
            let outline = Rect::new(x, y + row, width as u32 + 2, 1);
            canvas.fill_rect(outline, self.theme.pointer_edge);
            let fill = Rect::new(x + 1, y + row, width as u32, 1);
            canvas.fill_rect(fill, self.theme.pointer);
        }
        pointer_rect(x, y)
    }
}

/// The rect a pointer at `(x, y)` covers: sixteen rows tall, and as wide as the widest of
/// them plus the outline.
fn pointer_rect(x: i32, y: i32) -> Rect {
    const HEIGHT: i32 = 16;
    Rect::new(x, y, (HEIGHT * 2 / 3) as u32 + 2, HEIGHT as u32)
}

impl Compositor {
    /// Fills the desktop and every window that reaches into `clip`.
    fn paint(&mut self, canvas: &mut Canvas<'_>, clip: Option<Rect>) {
        let theme = self.theme;
        fill(canvas, canvas_rect(canvas), theme.desktop, clip);
        for window in self.windows.iter() {
            if let Some(clip) = clip {
                if intersection(window.rect, clip).is_none() {
                    continue;
                }
            }
            let focused = self.windows.focused() == Some(window.id);
            self.paint_window(canvas, window, focused, clip);
        }
    }

    /// Paints one window: border, title bar, title, body, body text.
    fn paint_window(
        &self,
        canvas: &mut Canvas<'_>,
        window: &Window,
        focused: bool,
        clip: Option<Rect>,
    ) {
        let theme = self.theme;
        let frame = window.rect;
        let border = theme.border_width;
        let edge = if focused {
            theme.edge_focused
        } else {
            theme.edge
        };
        fill(canvas, frame, edge, clip);

        // The title bar sits inside the border.
        let title_bar = Rect::new(
            frame.x + border as i32,
            frame.y + border as i32,
            frame.width.saturating_sub(border * 2).max(1),
            theme.title_height,
        );
        fill(
            canvas,
            title_bar,
            if focused {
                theme.title_focused
            } else {
                theme.title
            },
            clip,
        );
        let (cell_width, cell_height) = text::cell_size(theme.text_scale);
        let title_x = title_bar.x + theme.padding as i32;
        let title_y = title_bar.y + (title_bar.height.saturating_sub(cell_height)) as i32 / 2;
        let title_columns =
            title_bar.width.saturating_sub(theme.padding * 2) as i32 / cell_width.max(1) as i32;
        let mut title = String::from(window.title.as_str());
        truncate(&mut title, title_columns.max(0) as usize);
        text_clipped(
            canvas,
            title_x,
            title_y,
            &title,
            theme.title_text,
            theme.text_scale,
            clip,
        );

        // The body fills what is left inside the border, below the title bar.
        let body = Rect::new(
            frame.x + border as i32,
            title_bar.y + title_bar.height as i32,
            frame.width.saturating_sub(border * 2).max(1),
            frame
                .height
                .saturating_sub(theme.title_height + border * 2)
                .max(1),
        );
        fill(canvas, body, theme.window, clip);

        let (columns, rows) = theme.body_cells(frame.width, frame.height);
        let text_x = body.x + theme.padding as i32;
        let mut text_y = body.y + theme.padding as i32;
        for line in window.visible_lines(rows) {
            let mut line = String::from(line.as_str());
            truncate(&mut line, columns.min(MAX_LINE_CHARS));
            text_clipped(
                canvas,
                text_x,
                text_y,
                &line,
                theme.text,
                theme.text_scale,
                clip,
            );
            text_y += cell_height as i32;
        }
    }
}

/// The whole canvas as a rect, for the desktop fill.
fn canvas_rect(canvas: &Canvas<'_>) -> Rect {
    let geometry = canvas.geometry();
    Rect::new(0, 0, geometry.width as u32, geometry.height as u32)
}

/// Fills `rect` with `color`, clipped to `clip` when there is one.
fn fill(canvas: &mut Canvas<'_>, rect: Rect, color: Color, clip: Option<Rect>) {
    let rect = match clip {
        Some(clip) => match intersection(rect, clip) {
            Some(inside) => inside,
            None => return,
        },
        None => rect,
    };
    canvas.fill_rect(rect, color);
}

/// Draws `text` at `(x, y)`, stopping at the clip's right edge when there is one.
///
/// Truncation rather than a per-pixel mask: the text module knows nothing about damage, and a
/// character that starts outside the damage would smear the repaint across pixels that are
/// already correct.
fn text_clipped(
    canvas: &mut Canvas<'_>,
    x: i32,
    y: i32,
    text: &str,
    color: Color,
    scale: usize,
    clip: Option<Rect>,
) {
    let limit = match clip {
        Some(clip) => clip.x + clip.width as i32,
        None => i32::MAX,
    };
    let advance = text::cell_size(scale).0 as i32;
    let mut pen = x;
    for character in text.chars() {
        if pen + advance > limit {
            break;
        }
        text::draw_char(canvas, pen, y, character, color, scale);
        pen += advance;
    }
}

/// The intersection of two rects, or `None` if they do not overlap.
fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = (a.x + a.width as i32).min(b.x + b.width as i32);
    let bottom = (a.y + a.height as i32).min(b.y + b.height as i32);
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect::new(
        left,
        top,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
}

/// Keeps at most `columns` characters of `text`, in place.
fn truncate(text: &mut String, columns: usize) {
    if text.chars().count() <= columns {
        return;
    }
    let end = text
        .char_indices()
        .nth(columns)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{ChannelOrder, Geometry};
    use crate::theme::GREY;

    /// A panel in memory: four bytes per pixel, red first, which is what the canvas wants.
    struct Surface {
        pixels: Vec<u8>,
        width: usize,
        height: usize,
    }

    impl Surface {
        fn new(width: usize, height: usize) -> Self {
            Self {
                pixels: vec![0u8; width * height * 4],
                width,
                height,
            }
        }

        fn canvas(&mut self) -> Canvas<'_> {
            let geometry = Geometry::new(self.width, self.height, self.width, 4);
            Canvas::new(&mut self.pixels, geometry, ChannelOrder::Rgb).expect("the layout is valid")
        }

        fn pixel(&self, x: usize, y: usize) -> Color {
            let index = (y * self.width + x) * 4;
            Color::new(
                self.pixels[index],
                self.pixels[index + 1],
                self.pixels[index + 2],
            )
        }
    }

    fn compositor() -> (Compositor, Surface) {
        (Compositor::new(400, 300, GREY), Surface::new(400, 300))
    }

    fn lines(text: &str) -> Vec<String> {
        vec![String::from(text)]
    }

    #[test]
    fn the_desktop_is_the_theme_colour_where_nothing_is_drawn() {
        let (mut desktop, mut surface) = compositor();
        assert_eq!(desktop.draw_all(&mut surface.canvas()), 0);
        assert_eq!(surface.pixel(399, 299), GREY.desktop);
        assert_eq!(surface.pixel(0, 0), GREY.desktop);
    }

    #[test]
    fn a_window_paints_its_edge_its_title_bar_and_its_body() {
        let (mut desktop, mut surface) = compositor();
        let id = desktop.create("log", lines("hello"), 200, 150);
        let frame = desktop.windows.window(id).expect("window").rect;
        desktop.draw_all(&mut surface.canvas());

        // The outermost pixel of the frame is the edge, and it is the focused colour because
        // a new window has focus.
        assert_eq!(
            surface.pixel(frame.x as usize, frame.y as usize),
            GREY.edge_focused
        );
        // Inside the edge and inside the title bar.
        let title_x = frame.x as usize + GREY.border_width as usize + 4;
        let title_y = frame.y as usize + GREY.border_width as usize + 4;
        assert_eq!(surface.pixel(title_x, title_y), GREY.title_focused);
        // The bottom of the body, below the last line of text, is the window's own colour.
        let body_x = frame.x as usize + frame.width as usize - 4;
        let body_y = frame.y as usize + frame.height as usize - 4;
        assert_eq!(surface.pixel(body_x, body_y), GREY.window);
        // The pixel just outside the frame is still the desktop.
        assert_eq!(
            surface.pixel(frame.x as usize + frame.width as usize, frame.y as usize),
            GREY.desktop
        );
    }

    #[test]
    fn the_window_in_front_owns_the_overlap() {
        let (mut desktop, mut surface) = compositor();
        let back = desktop.create("A", lines("b"), 200, 150);
        let front = desktop.create("B", lines("f"), 200, 150);
        desktop.windows.move_to(back, 20, 20);
        desktop.windows.move_to(front, 100, 20);
        desktop.windows.focus(front);
        desktop.draw_all(&mut surface.canvas());

        // Both title bars run across y = 26..56; x = 200 is inside both and past both titles.
        assert_eq!(surface.pixel(200, 40), GREY.title_focused);
        // x = 60 is inside the back window's title bar only, which is unfocused.
        assert_eq!(surface.pixel(60, 40), GREY.title);
        // Raising the back window swaps which one owns the overlap.
        assert!(desktop.windows.raise(back));
        let damage = desktop.draw_damage(&mut surface.canvas());
        assert!(!damage.is_empty());
        assert_eq!(surface.pixel(200, 40), GREY.title_focused);
        // The front window is now unfocused, which its own title bar shows at x = 250, where
        // the raised window no longer reaches.
        assert_eq!(surface.pixel(250, 40), GREY.title);
    }

    #[test]
    fn a_damage_rect_repaints_only_what_it_covers() {
        let (mut desktop, mut surface) = compositor();
        let left = desktop.create("left", lines("l"), 120, 120);
        let right = desktop.create("right", lines("r"), 120, 120);
        desktop.windows.move_to(left, 10, 10);
        desktop.windows.move_to(right, 200, 150);
        desktop.windows.focus(right);
        desktop.draw_all(&mut surface.canvas());
        let before = surface.pixel(210, 160);
        assert_eq!(before, GREY.title_focused, "the focused window's title bar");

        // Closing the left window damages only its frame.
        assert!(desktop.windows.close(left));
        let damage = desktop.draw_damage(&mut surface.canvas());
        assert_eq!(damage.len(), 1, "{damage:?}");
        // Where it was is the desktop again...
        assert_eq!(surface.pixel(15, 15), GREY.desktop);
        // ...and the other window was not touched at all.
        assert_eq!(surface.pixel(210, 160), before);
    }

    #[test]
    fn moving_a_window_repairs_the_strip_between_where_it_was_and_is() {
        let (mut desktop, mut surface) = compositor();
        let id = desktop.create("log", lines("l"), 120, 120);
        desktop.windows.move_to(id, 10, 10);
        desktop.draw_all(&mut surface.canvas());
        assert!(desktop.windows.move_to(id, 200, 150));
        let damage = desktop.draw_damage(&mut surface.canvas());
        // Two rects that do not overlap.
        assert_eq!(damage.len(), 2, "{damage:?}");
        assert_eq!(surface.pixel(15, 15), GREY.desktop);
        assert_eq!(surface.pixel(200, 150), GREY.edge_focused);
    }

    #[test]
    fn nothing_changed_paints_nothing() {
        let (mut desktop, mut surface) = compositor();
        desktop.create("log", lines("l"), 120, 120);
        desktop.draw_all(&mut surface.canvas());
        let snapshot = surface.pixels.clone();
        let damage = desktop.draw_damage(&mut surface.canvas());
        assert!(damage.is_empty());
        assert_eq!(surface.pixels, snapshot, "an empty frame painted pixels");
    }

    #[test]
    fn the_pointer_is_a_triangle_with_a_dark_outline() {
        let (mut desktop, mut surface) = compositor();
        desktop.draw_all(&mut surface.canvas());
        let covered = desktop.draw_pointer(&mut surface.canvas(), 50, 40);
        // The first pixel is the outline; the second row's inside is the fill.
        assert_eq!(surface.pixel(50, 40), GREY.pointer_edge);
        assert_eq!(surface.pixel(51, 41), GREY.pointer);
        // The returned rect is where it drew and wide enough for the widest row.
        assert_eq!(covered.x, 50);
        assert_eq!(covered.y, 40);
        assert!(covered.width >= 3);
        assert_eq!(covered.height, 16);
    }

    #[test]
    fn a_pointer_that_moves_is_erased_where_it_was() {
        let (mut desktop, mut surface) = compositor();
        desktop.draw_all(&mut surface.canvas());
        desktop.draw_pointer(&mut surface.canvas(), 50, 40);
        // Drawn again where it is: nothing has to be repainted.
        desktop.draw_pointer(&mut surface.canvas(), 50, 40);
        assert_eq!(
            desktop.windows.pending(),
            0,
            "an idle pointer damaged the panel"
        );
        desktop.draw_pointer(&mut surface.canvas(), 90, 40);
        assert!(
            desktop.windows.pending() > 0,
            "the pointer moved and nothing asked for the one it left behind to be repainted"
        );
        let damage = desktop.draw_damage(&mut surface.canvas());
        assert!(!damage.is_empty());
        // The triangle is gone from where it was and still there where it went: a pointer that
        // is drawn but never erased is a smear of triangles, which is what a preview showed.
        assert_eq!(surface.pixel(50, 40), GREY.desktop);
        assert_eq!(surface.pixel(90, 40), GREY.pointer_edge);
    }

    #[test]
    fn a_title_longer_than_its_bar_is_truncated_rather_than_drawn_over_the_edge() {
        let mut text = String::from("abcdef");
        truncate(&mut text, 3);
        assert_eq!(text, "abc");
        // Nothing is cut when it already fits.
        let mut text = String::from("abc");
        truncate(&mut text, 3);
        assert_eq!(text, "abc");
        // A multi-byte character is cut on a character boundary, not inside one.
        let mut text = String::from("αβγ");
        truncate(&mut text, 2);
        assert_eq!(text, "αβ");
        // Zero columns is a legitimate request and produces nothing.
        let mut text = String::from("abc");
        truncate(&mut text, 0);
        assert!(text.is_empty());
    }

    #[test]
    fn a_body_line_that_does_not_fit_is_clipped_at_the_frame_edge() {
        let (mut desktop, mut surface) = compositor();
        let id = desktop.create("log", lines(&"x".repeat(120)), 120, 120);
        let frame = desktop.windows.window(id).expect("window").rect;
        desktop.draw_all(&mut surface.canvas());
        // The last pixel of the frame is still the frame's colour, not a glyph from the line
        // that ran past it.
        let x = frame.x as usize + frame.width as usize - 1;
        let y = frame.y as usize + frame.height as usize - 1;
        let pixel = surface.pixel(x, y);
        assert!(
            pixel == GREY.edge_focused || pixel == GREY.window,
            "a clipped line painted {pixel:?} over the frame"
        );
    }
}
