//! The look of the desktop, as data.
//!
//! A theme is a set of colours and measurements, not a set of drawing calls: the compositor
//! asks the theme what a window's title bar is and how tall it is, and never asks whether it
//! is drawing the focused window - the theme has both answers. Keeping it that way is what
//! makes "the window is the wrong colour" a value to change rather than a function to edit,
//! and it is why this module is testable without a screen.
//!
//! The measurements are in pixels at the panel's own scale, and the text scale is part of
//! the theme: a 4K panel wants a larger font in the same window, and that is a layout
//! decision, not a compositor one.

use crate::canvas::Color;

/// Everything the compositor asks the theme for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Theme {
    /// The name the boot log reports.
    pub name: &'static str,
    /// Behind every window.
    pub desktop: Color,
    /// A window's body.
    pub window: Color,
    /// The title bar of a window that does not have focus.
    pub title: Color,
    /// The title bar of the focused window.
    pub title_focused: Color,
    /// Title text, on both of the above.
    pub title_text: Color,
    /// A window's edge.
    pub edge: Color,
    /// The edge of the focused window: a second cue beside the title bar, so focus survives
    /// a colour-blind viewer and a bad panel.
    pub edge_focused: Color,
    /// Body text.
    pub text: Color,
    /// Text that is there but not the point, such as a status line.
    pub dim: Color,
    /// The pointer.
    pub pointer: Color,
    /// Where the pointer's outline meets the desktop.
    pub pointer_edge: Color,
    /// The title bar's height in pixels.
    pub title_height: u32,
    /// The border's thickness in pixels.
    pub border_width: u32,
    /// Space between the border and the text.
    pub padding: u32,
    /// How far each new window is offset from the one before it.
    pub cascade: i32,
    /// The scale text is drawn at.
    pub text_scale: usize,
}

/// The only theme, for now. Its colours are a cool grey with a blue accent: legible on the
/// teal boot background, distinguishable in greyscale, and not so bright that a mostly
/// empty desktop glows.
pub const GREY: Theme = Theme {
    name: "grey",
    desktop: Color::new(0x1f, 0x6f, 0x8b),
    window: Color::new(0x1a, 0x20, 0x26),
    title: Color::new(0x2e, 0x38, 0x42),
    title_focused: Color::new(0x2f, 0x6f, 0xa8),
    title_text: Color::new(0xdf, 0xe7, 0xee),
    edge: Color::new(0x3b, 0x47, 0x53),
    edge_focused: Color::new(0x7f, 0xb8, 0xe8),
    text: Color::new(0xd8, 0xe2, 0xea),
    dim: Color::new(0x8d, 0x9a, 0xa6),
    pointer: Color::new(0xf5, 0xf8, 0xfa),
    pointer_edge: Color::new(0x10, 0x14, 0x18),
    title_height: 30,
    border_width: 2,
    padding: 8,
    cascade: 36,
    text_scale: 2,
};

impl Theme {
    /// The frame a window occupies: its rect plus the border, which the caller's rect does
    /// not include.
    ///
    /// Exists so the compositor and the hit test cannot disagree about whether the border
    /// is part of the window. It is.
    pub const fn frame_grows_by(&self) -> u32 {
        self.border_width
    }

    /// The height a window needs before it has any content: two borders and the title bar.
    pub const fn chrome_height(&self) -> u32 {
        self.border_width * 2 + self.title_height
    }

    /// The width a window needs before it has any content: two borders and two paddings.
    pub const fn chrome_width(&self) -> u32 {
        (self.border_width + self.padding) * 2
    }

    /// How much body text fits in a window of `width` by `height`, in cells.
    ///
    /// Character cells are square at the text scale (`text::cell_size`), which is the
    /// reason one number describes both dimensions.
    pub fn body_cells(&self, width: u32, height: u32) -> (usize, usize) {
        let (cell_width, cell_height) = crate::text::cell_size(self.text_scale);
        let usable_width = width.saturating_sub(self.chrome_width());
        let usable_height = height.saturating_sub(self.chrome_height() + self.padding);
        (
            (usable_width / cell_width.max(1)) as usize,
            (usable_height / cell_height.max(1)) as usize,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Perceived brightness, the weight the eye gives each channel. Good enough to say "this
    /// text is readable on that background", which is the only claim the tests make.
    fn brightness(color: Color) -> i32 {
        (i32::from(color.r) * 299 + i32::from(color.g) * 587 + i32::from(color.b) * 114) / 1000
    }

    #[test]
    fn text_is_readable_on_every_background_it_is_drawn_on() {
        // Body text on a window, title text on both title bars, and the dim text too. A
        // theme whose text is close to its background is a theme nobody can read, and the
        // difference is easy to lose in a palette edit.
        for (text, background, name) in [
            (GREY.text, GREY.window, "body"),
            (GREY.title_text, GREY.title, "title"),
            (GREY.title_text, GREY.title_focused, "focused title"),
            (GREY.dim, GREY.window, "dim"),
        ] {
            let difference = (brightness(text) - brightness(background)).abs();
            assert!(
                difference >= 100,
                "{name} text is only {difference} apart from its background"
            );
        }
    }

    #[test]
    fn the_structures_are_distinguishable_from_what_they_sit_on() {
        // A window has to be visible against the desktop, and its edge against itself.
        assert!((brightness(GREY.window) - brightness(GREY.desktop)).abs() >= 60);
        assert!((brightness(GREY.edge) - brightness(GREY.window)).abs() >= 20);
        // Focus is two cues, not one: a different title bar *and* a different edge.
        assert_ne!(GREY.title_focused, GREY.title);
        assert_ne!(GREY.edge_focused, GREY.edge);
        // The pointer is the brightest thing on the screen, with a dark outline so it stays
        // visible over both the desktop and a window.
        assert!(brightness(GREY.pointer) > brightness(GREY.text));
        assert!(brightness(GREY.pointer) - brightness(GREY.pointer_edge) > 150);
    }

    #[test]
    fn the_chrome_leaves_room_for_the_text_it_is_there_for() {
        let (cell_width, cell_height) = crate::text::cell_size(GREY.text_scale);
        // A title bar fits its own text with a little air.
        assert!(GREY.title_height >= cell_height);
        // One border on each side, and a padding on each side, before any text fits.
        assert_eq!(GREY.chrome_width(), (GREY.border_width + GREY.padding) * 2);
        assert_eq!(
            GREY.chrome_height(),
            GREY.border_width * 2 + GREY.title_height
        );
        // A 480x320 window is the smallest the boot creates; it has to hold a few lines.
        let (columns, rows) = GREY.body_cells(480, 320);
        assert!(columns >= 25, "{columns} columns");
        assert!(rows >= 8, "{rows} rows");
        // The body's cells are counted in whole characters, so a window narrower than one
        // character has none rather than a fraction of one.
        assert!(cell_width > 0 && cell_height > 0);
    }

    #[test]
    fn a_window_smaller_than_its_chrome_reports_no_cells_rather_than_underflowing() {
        assert_eq!(GREY.body_cells(1, 1), (0, 0));
        assert_eq!(GREY.body_cells(0, 0), (0, 0));
    }

    #[test]
    fn the_cascade_step_is_small_enough_to_keep_windows_on_screen() {
        // Three windows cascade from the origin; the offsets have to be a fraction of a
        // window's width, or the second one is off the panel.
        assert!(GREY.cascade > 0);
        assert!(GREY.cascade < 64);
    }
}
