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

use crate::canvas::{Color, Rect};

/// Everything the compositor asks the theme for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Theme {
    /// The name the boot log reports.
    pub name: &'static str,
    /// Behind every window.
    pub desktop: Color,
    /// The wallpaper's grid rules: a few steps lighter than the ground, so the desktop reads
    /// as a surface with a texture rather than as a flat void.
    pub wallpaper_line: Color,
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
    /// The radius of a window's outer corners, in pixels.
    ///
    /// The corners are cut from the *outside* of the frame, so the border keeps its thickness
    /// through the curve and the title bar's own corners are this minus
    /// [`border_width`](Self::border_width).
    pub corner_radius: u32,
    /// Space between the border and the text.
    pub padding: u32,
    /// How far each new window is offset from the one before it.
    pub cascade: i32,
    /// The scale text is drawn at.
    pub text_scale: usize,
    /// The scale a window's title is drawn at: one step larger than the body.
    pub display_scale: usize,
    /// The scale the smallest labels are drawn at.
    pub label_scale: usize,
    /// The top panel: the bar the clock and the window count sit in.
    pub panel: Color,
    /// Text on the panel.
    pub panel_text: Color,
    /// The floating dock at the bottom of the screen.
    pub dock: Color,
    /// A dock tile that is not being pointed at.
    pub dock_tile: Color,
    /// A dock tile under the pointer, or whose window is focused.
    pub dock_tile_active: Color,
    /// A desktop tile: one of the large launcher squares.
    pub tile: Color,
    /// Text on a desktop tile.
    pub tile_text: Color,
    /// The minimise and maximise affordances in a title bar.
    pub control: Color,
    /// The close affordance, which is the one that loses data.
    pub control_close: Color,
    /// What a shadow is painted with; its strength is [`Self::shadow_alpha`].
    pub shadow: Color,
    /// A button at rest in a window's own content.
    pub button: Color,
    /// A button under the pointer.
    pub button_hover: Color,
    /// A button that is being held down.
    pub button_active: Color,
    /// Text on a button.
    pub button_text: Color,
    /// A raised surface inside a window: a sidebar, a card, a list row.
    pub content_raised: Color,
    /// The height of the top panel, in pixels.
    pub panel_height: u32,
    /// The height of the dock, in pixels.
    pub dock_height: u32,
    /// The side of a dock tile, in pixels.
    pub dock_tile_size: u32,
    /// Space between dock tiles.
    pub dock_gap: u32,
    /// Space between the dock and the bottom of the panel.
    pub dock_margin: u32,
    /// The side of a desktop launcher tile, in pixels.
    pub tile_size: u32,
    /// Space between desktop tiles.
    pub tile_gap: u32,
    /// The diameter of a title bar affordance.
    pub affordance: u32,
    /// Space between two affordances.
    pub affordance_gap: u32,
    /// Space between the affordances and the title bar's right edge.
    pub affordance_margin: u32,
    /// How far a shadow is offset below the window that casts it.
    pub shadow_offset: i32,
    /// How strongly a shadow is blended: 0 would be no shadow, 255 a black rectangle.
    pub shadow_alpha: u8,
    /// How opaque a window's chrome is. Below 255 the desktop shows through it, which is
    /// the translucency the desktop is allowed to have: the chrome, never the text on it.
    pub chrome_alpha: u8,
    /// The height of a button in a window's content, in pixels.
    pub button_height: u32,
    /// Space between two rows of content.
    pub row_gap: u32,
    /// The width a window may not be dragged below: narrower and its title bar has no room
    /// for its own affordances.
    pub min_window_width: u32,
    /// The height a window may not be dragged below.
    pub min_window_height: u32,
}

/// The only theme, for now. Its colours are a cool grey with a blue accent: legible on the
/// teal boot background, distinguishable in greyscale, and not so bright that a mostly
/// empty desktop glows.
pub const GREY: Theme = Theme {
    name: "grey",
    desktop: Color::new(0x1f, 0x6f, 0x8b),
    wallpaper_line: Color::new(0x25, 0x79, 0x96),
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
    corner_radius: 10,
    padding: 8,
    cascade: 36,
    text_scale: 2,
    display_scale: 3,
    label_scale: 1,
    panel: Color::new(0x12, 0x18, 0x1d),
    panel_text: Color::new(0xcf, 0xdb, 0xe4),
    dock: Color::new(0x16, 0x1c, 0x22),
    dock_tile: Color::new(0x2b, 0x35, 0x3e),
    dock_tile_active: Color::new(0x2f, 0x6f, 0xa8),
    tile: Color::new(0x16, 0x54, 0x6b),
    tile_text: Color::new(0xe8, 0xf2, 0xf6),
    control: Color::new(0x9a, 0xa8, 0xb4),
    control_close: Color::new(0xe0, 0x6c, 0x6c),
    shadow: Color::new(0x00, 0x00, 0x00),
    button: Color::new(0x2e, 0x38, 0x42),
    button_hover: Color::new(0x3a, 0x47, 0x53),
    button_active: Color::new(0x2f, 0x6f, 0xa8),
    button_text: Color::new(0xe6, 0xee, 0xf4),
    content_raised: Color::new(0x22, 0x2a, 0x32),
    panel_height: 30,
    dock_height: 68,
    dock_tile_size: 44,
    dock_gap: 10,
    dock_margin: 12,
    tile_size: 132,
    tile_gap: 16,
    affordance: 12,
    affordance_gap: 10,
    affordance_margin: 10,
    shadow_offset: 4,
    shadow_alpha: 56,
    chrome_alpha: 242,
    button_height: 30,
    row_gap: 8,
    min_window_width: 220,
    min_window_height: 140,
};

impl Theme {
    /// The title bar: inside the border, the full inner width, [`Self::title_height`] tall.
    ///
    /// One function rather than the same arithmetic in three places: the compositor draws the
    /// bar from it, the hit test asks what was clicked from it, and a window dragged by its
    /// bar is dragged by a rect this produced. A title bar the drawing and the hit test
    /// disagree about is a window that cannot be grabbed exactly where it looks grabbable.
    pub fn title_rect(&self, frame: Rect) -> Rect {
        let border = self.border_width;
        Rect::new(
            frame.x + border as i32,
            frame.y + border as i32,
            frame.width.saturating_sub(border * 2),
            self.title_height,
        )
    }

    /// The client area: inside the border, below the title bar. What a window's content gets.
    pub fn body_rect(&self, frame: Rect) -> Rect {
        let border = self.border_width;
        let title = self.title_rect(frame);
        Rect::new(
            title.x,
            title.y + title.height as i32,
            title.width,
            frame
                .height
                .saturating_sub(border * 2 + self.title_height)
                .max(1),
        )
    }

    /// The three title bar affordances, right to left: close, minimise, maximise.
    ///
    /// Right to left because close is the one reached for most, and the right-hand corner is
    /// where every desktop puts it. One private function for all three so they cannot drift
    /// apart: a spacing change moves them together or not at all.
    fn affordances(&self, frame: Rect, index: u32) -> Rect {
        let title = self.title_rect(frame);
        let size = self.affordance;
        let step = size + self.affordance_gap;
        let right = title.x + title.width as i32 - self.affordance_margin as i32;
        let left = right - size as i32 - (index * step) as i32;
        Rect::new(
            left,
            title.y + (title.height.saturating_sub(size)) as i32 / 2,
            size,
            size,
        )
    }

    /// The close affordance: the rightmost of the three.
    pub fn close_rect(&self, frame: Rect) -> Rect {
        self.affordances(frame, 0)
    }

    /// The minimise affordance.
    pub fn minimise_rect(&self, frame: Rect) -> Rect {
        self.affordances(frame, 1)
    }

    /// The maximise affordance.
    pub fn maximise_rect(&self, frame: Rect) -> Rect {
        self.affordances(frame, 2)
    }

    /// The work area on a `width` by `height` panel: everything between the top panel and the
    /// dock. A maximised window fills exactly this, and nothing may be dragged into either bar.
    pub fn work_area(&self, width: u32, height: u32) -> Rect {
        let top = self.panel_height;
        let bottom = self.dock_height + self.dock_margin;
        Rect::new(
            0,
            top as i32,
            width,
            height.saturating_sub(top).saturating_sub(bottom).max(1),
        )
    }

    /// The dock: centred at the bottom of the panel, as wide as `tiles` of them need.
    pub fn dock_rect(&self, width: u32, height: u32, tiles: u32) -> Rect {
        let tile = self.dock_tile_size;
        let inner = tiles * tile + tiles.saturating_sub(1) * self.dock_gap;
        let surround = (self.dock_gap + self.padding) * 2;
        let dock_width = (inner + surround).min(width);
        Rect::new(
            width.saturating_sub(dock_width) as i32 / 2,
            height.saturating_sub(self.dock_height + self.dock_margin) as i32,
            dock_width,
            self.dock_height,
        )
    }

    /// The `index`th dock tile inside `dock`, centred vertically in it.
    pub fn dock_tile_rect(&self, dock: Rect, index: u32) -> Rect {
        let tile = self.dock_tile_size;
        let step = tile + self.dock_gap;
        Rect::new(
            dock.x + (self.dock_gap + self.padding) as i32 + (index * step) as i32,
            dock.y + dock.height.saturating_sub(tile) as i32 / 2,
            tile,
            tile,
        )
    }

    /// The `index`th desktop launcher tile, laid out in rows of `columns` from `origin`.
    pub fn tile_rect(&self, origin: Rect, columns: u32, index: u32) -> Rect {
        let step = self.tile_size + self.tile_gap;
        let column = index % columns.max(1);
        let row = index / columns.max(1);
        Rect::new(
            origin.x + (column * step) as i32,
            origin.y + (row * step) as i32,
            self.tile_size,
            self.tile_size,
        )
    }

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

    #[test]
    fn the_corner_radius_fits_inside_the_chrome_it_is_cut_from() {
        // At least the border, so the curve does not vanish under the edge colour, and no
        // more than half the title bar's inner height, so the two top corners cannot meet in
        // the middle and turn the title bar into a lens.
        assert!(GREY.corner_radius >= GREY.border_width);
        assert!(GREY.corner_radius <= GREY.title_height / 2 + GREY.border_width);
    }
}
