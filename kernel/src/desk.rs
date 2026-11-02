//! The desktop itself: its furniture, where that furniture is, and what a press on it means.
//!
//! # Three bands and one rule
//!
//! The screen is a top panel, a work area and a dock. The rule is that a window never covers
//! the first or the last: the panel says what the machine is doing and the dock says what is
//! running, and both have to stay legible with a window maximised. That rule is expressed
//! once, as [`Theme::work_area`](crate::theme::Theme::work_area), and the window list clamps
//! to it - this module only draws in the space it is left.
//!
//! # Furniture is drawn from the theme, so it is also hit from the theme
//!
//! Every rect here comes from a theme function that the compositor and the pointer both call:
//! a dock tile is where `dock_tile_rect` says it is, drawn there and hit there. There is no
//! second layout to keep in step, which is what stops a dock tile from drifting away from the
//! thing that opens it.
//!
//! # The wallpaper is cheap on purpose
//!
//! It is a flat colour, a horizon line and a grid of one-pixel rules - about twenty thousand
//! pixels of work on a 1280x800 panel, against three hundred thousand for a gradient. A
//! desktop that spends a third of its frame on the picture behind the windows has its
//! priorities backwards.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::canvas::{blend_clipped, Canvas, Color, Corners, Rect};
use crate::text;
use crate::theme::Theme;
use crate::window::{Window, Windows};

/// The applications the launcher offers, top left to bottom right.
///
/// A fixed list in a fixed order: a machine that boots in a second does not need a launcher
/// that can be sorted, and a fixed list is one the tests can name.
pub const LAUNCHER: [&str; 4] = ["Log", "System", "About", "Shell"];

/// What a press on the furniture means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    /// Nothing: the wallpaper.
    Wallpaper,
    /// The top panel, which does nothing yet but must not pass the press on.
    Panel,
    /// A launcher tile, by index into [`LAUNCHER`].
    Launch(usize),
    /// A dock tile, belonging to that window.
    Dock(u32),
}

/// The desktop's furniture, in one place.
#[derive(Clone, Debug)]
pub struct Desk {
    theme: Theme,
    width: u32,
    height: u32,
}

impl Desk {
    /// Furniture for a `width` by `height` panel.
    pub fn new(theme: Theme, width: u32, height: u32) -> Self {
        Self {
            theme,
            width,
            height,
        }
    }

    /// The theme the furniture is drawn with.
    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Notes a new panel size, which moves the dock and the work area.
    pub fn resize(&mut self, width: u32, height: u32) -> bool {
        if (width, height) == (self.width, self.height) {
            return false;
        }
        self.width = width;
        self.height = height;
        true
    }

    /// The work area: the space windows may use.
    pub fn work_area(&self) -> Rect {
        self.theme.work_area(self.width, self.height)
    }

    /// The top panel's rect.
    pub fn panel_rect(&self) -> Rect {
        Rect::new(0, 0, self.width, self.theme.panel_height)
    }

    /// The dock's rect, sized for `count` windows.
    pub fn dock_rect(&self, count: usize) -> Rect {
        self.theme.dock_rect(self.width, self.height, count as u32)
    }

    /// Where the launcher tiles start, and how many fit in a row.
    fn launcher_origin(&self) -> (Rect, u32) {
        let work = self.work_area();
        let margin = (self.theme.tile_size / 3).max(16);
        let columns = (work.width / (self.theme.tile_size + self.theme.tile_gap))
            .max(1)
            .min(LAUNCHER.len() as u32);
        (
            Rect::new(
                work.x + margin as i32,
                work.y + margin as i32,
                work.width.saturating_sub(margin * 2),
                work.height.saturating_sub(margin * 2),
            ),
            columns,
        )
    }
    /// What a press at `(x, y)` landed on.
    ///
    /// The panel is asked first, then the dock, then the launcher: all three are drawn over
    /// the wallpaper, and a press that lands on a tile must open an application rather than
    /// fall through to the desktop behind it.
    pub fn hit(&self, windows: &Windows, x: i32, y: i32) -> Hit {
        if self.panel_rect().contains(x, y) {
            return Hit::Panel;
        }
        let visible: Vec<&Window> = windows.iter().filter(|w| !w.minimised).collect();
        if !visible.is_empty() {
            let dock = self.dock_rect(visible.len());
            if dock.contains(x, y) {
                for (index, window) in visible.iter().enumerate() {
                    if self.theme.dock_tile_rect(dock, index as u32).contains(x, y) {
                        return Hit::Dock(window.id);
                    }
                }
                // The dock's own padding: a press there is a press on the dock, not on the
                // wallpaper, so it must not fall through and clear the selection.
                return Hit::Panel;
            }
        }
        let (origin, columns) = self.launcher_origin();
        for index in 0..LAUNCHER.len() {
            if self
                .theme
                .tile_rect(origin, columns, index as u32)
                .contains(x, y)
            {
                return Hit::Launch(index);
            }
        }
        Hit::Wallpaper
    }

    /// Paints the wallpaper, the launcher tiles, the top panel and the dock, back to front.
    pub fn paint(&self, canvas: &mut Canvas<'_>, windows: &Windows, clip: Option<Rect>) {
        self.paint_wallpaper(canvas, clip);
        self.paint_launcher(canvas, clip);
        self.paint_panel(canvas, windows, clip);
        self.paint_dock(canvas, windows, clip);
    }

    /// The wallpaper: a flat ground, a grid of rules, and a horizon.
    ///
    /// The grid is what makes the screen feel like a surface rather than a void, and it costs
    /// one `fill_rect` per line: about twenty lines at these dimensions.
    pub fn paint_wallpaper(&self, canvas: &mut Canvas<'_>, clip: Option<Rect>) {
        let theme = self.theme;
        fill(
            canvas,
            Rect::new(0, 0, self.width, self.height),
            theme.desktop,
            clip,
        );
        let step = 64;
        let mut x = 0;
        while x < self.width as i32 {
            fill(
                canvas,
                Rect::new(x, 0, 1, self.height),
                theme.wallpaper_line,
                clip,
            );
            x += step;
        }
        let mut y = 0;
        while y < self.height as i32 {
            fill(
                canvas,
                Rect::new(0, y, self.width, 1),
                theme.wallpaper_line,
                clip,
            );
            y += step;
        }
        // The horizon: one brighter rule at the work area's first line, so the wallpaper has a
        // top edge instead of looking as though it continues behind the panel.
        let work = self.work_area();
        fill(
            canvas,
            Rect::new(0, work.y, self.width, 1),
            theme.edge,
            clip,
        );
    }

    /// The launcher: one tile per entry, with its initial as its mark and its name below.
    pub fn paint_launcher(&self, canvas: &mut Canvas<'_>, clip: Option<Rect>) {
        let theme = self.theme;
        let (origin, columns) = self.launcher_origin();
        let (cell_width, cell_height) = text::cell_size(theme.display_scale);
        let (_, label_height) = text::cell_size(theme.text_scale);
        for (index, label) in LAUNCHER.iter().enumerate() {
            let rect = theme.tile_rect(origin, columns, index as u32);
            if !visible_at(rect, clip) {
                continue;
            }
            fill(canvas, rect, theme.tile, clip);
            let mark: String = label.chars().take(1).collect();
            let mark_width = text::measure(&mark, theme.display_scale).0 as i32;
            text::draw_str_clipped(
                canvas,
                rect.x + (rect.width as i32 - mark_width) / 2,
                rect.y + (rect.height as i32 / 2) - cell_height as i32,
                &mark,
                theme.tile_text,
                theme.display_scale,
                clip,
            );
            // The name, along the bottom, cut to what the tile can hold.
            let fit = (rect.width as usize / (cell_width as usize).max(1)).saturating_sub(1);
            let name: String = label.chars().take(fit).collect();
            let name_width = text::measure(&name, theme.text_scale).0 as i32;
            text::draw_str_clipped(
                canvas,
                rect.x + (rect.width as i32 - name_width) / 2,
                rect.y + rect.height as i32 - label_height as i32 - theme.padding as i32,
                &name,
                theme.tile_text,
                theme.text_scale,
                clip,
            );
        }
    }
    /// The top panel: the system's name on the left, what is running on the right.
    pub fn paint_panel(&self, canvas: &mut Canvas<'_>, windows: &Windows, clip: Option<Rect>) {
        let theme = self.theme;
        let panel = self.panel_rect();
        fill(canvas, panel, theme.panel, clip);
        let (_, cell_height) = text::cell_size(theme.label_scale);
        let y = panel.y + (panel.height.saturating_sub(cell_height)) as i32 / 2;
        text::draw_str_clipped(
            canvas,
            panel.x + theme.padding as i32,
            y,
            "grey",
            theme.panel_text,
            theme.label_scale,
            clip,
        );
        let count = window_count(windows.iter().filter(|window| !window.minimised).count());
        let width = text::measure(&count, theme.label_scale).0 as i32;
        text::draw_str_clipped(
            canvas,
            panel.x + panel.width as i32 - theme.padding as i32 - width,
            y,
            &count,
            theme.panel_text,
            theme.label_scale,
            clip,
        );
        // A rule under the panel, so it reads as a band rather than as a gap in the wallpaper.
        fill(
            canvas,
            Rect::new(0, panel.height as i32 - 1, self.width, 1),
            theme.edge,
            clip,
        );
    }

    /// The dock: one tile per open window, the focused one lit.
    pub fn paint_dock(&self, canvas: &mut Canvas<'_>, windows: &Windows, clip: Option<Rect>) {
        let theme = self.theme;
        let visible: Vec<&Window> = windows.iter().filter(|w| !w.minimised).collect();
        if visible.is_empty() {
            return;
        }
        let dock = self.dock_rect(visible.len());
        if !visible_at(dock, clip) {
            return;
        }
        // The dock floats over the wallpaper rather than butting against the screen's edge, so
        // it carries the same rounded corners a window does.
        let radius = theme.corner_radius.min(dock.height / 2);
        blend_clipped(canvas, dock, Corners::uniform(radius), theme.dock, clip);
        let (_, cell_height) = text::cell_size(theme.label_scale);
        for (index, window) in visible.iter().enumerate() {
            let tile = theme.dock_tile_rect(dock, index as u32);
            if !visible_at(tile, clip) {
                continue;
            }
            let color = if windows.focused() == Some(window.id) {
                theme.dock_tile_active
            } else {
                theme.dock_tile
            };
            blend_clipped(
                canvas,
                tile,
                Corners::uniform(theme.corner_radius / 2),
                color,
                clip,
            );
            // The window's initial: the dock is a list of what is running, not of what it says.
            let mark: String = window.title.chars().take(1).collect();
            let width = text::measure(&mark, theme.label_scale).0 as i32;
            text::draw_str_clipped(
                canvas,
                tile.x + (tile.width as i32 - width) / 2,
                tile.y + (tile.height as i32 - cell_height as i32) / 2,
                &mark,
                theme.button_text,
                theme.label_scale,
                clip,
            );
        }
    }
}

/// `1 window`, `3 windows`, `no windows`: the zero and the singular are the two cases a naive
/// count gets wrong on a screen somebody is looking at.
fn window_count(count: usize) -> String {
    match count {
        0 => String::from("no windows"),
        1 => String::from("1 window"),
        count => format!("{count} windows"),
    }
}

/// Fills `rect`, clipped to `clip` when one is given.
fn fill(canvas: &mut Canvas<'_>, rect: Rect, color: Color, clip: Option<Rect>) {
    match clip {
        Some(clip) => {
            if let Some(intersection) = rect.intersection(clip) {
                canvas.fill_rect(intersection, color);
            }
        }
        None => canvas.fill_rect(rect, color),
    }
}

/// Whether `rect` is worth drawing at all given `clip`.
fn visible_at(rect: Rect, clip: Option<Rect>) -> bool {
    clip.is_none_or(|clip| rect.intersection(clip).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{ChannelOrder, Geometry};
    use crate::theme;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A framebuffer big enough to paint a desktop into, and cheap to read back.
    struct Screen {
        pixels: Vec<u8>,
        width: usize,
        height: usize,
    }

    impl Screen {
        fn new(width: usize, height: usize) -> Self {
            Self {
                pixels: vec![0; width * height * 4],
                width,
                height,
            }
        }

        fn canvas(&mut self) -> Canvas<'_> {
            // The stride is in pixels, not bytes: firmware may pad a row, but never to a width
            // this test has to invent.
            let geometry = Geometry::new(self.width, self.height, self.width, 4);
            Canvas::new(&mut self.pixels, geometry, ChannelOrder::Rgb).expect("a valid layout")
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

    fn desk(windows: &mut Windows) -> Desk {
        let theme = theme::GREY;
        windows.set_work_area(theme.work_area(640, 400));
        Desk::new(theme, 640, 400)
    }

    #[test]
    fn the_panel_is_not_part_of_the_work_area_and_neither_is_the_dock() {
        let theme = theme::GREY;
        let mut windows = Windows::new(640, 400, theme.cascade);
        let desk = desk(&mut windows);
        let work = desk.work_area();
        assert!(
            work.y >= theme.panel_height as i32,
            "the panel overlapped the work area"
        );
        assert!(
            work.y + work.height as i32 <= 400 - theme.dock_height as i32,
            "the work area ran under the dock"
        );
        // A press on the panel belongs to the panel, whatever is behind it.
        assert_eq!(desk.hit(&windows, 5, 5), Hit::Panel);
    }

    #[test]
    fn a_launcher_tile_opens_the_entry_it_is_drawn_for() {
        let mut windows = Windows::new(640, 400, theme::GREY.cascade);
        let desk = desk(&mut windows);
        let (origin, columns) = desk.launcher_origin();
        for (index, _) in LAUNCHER.iter().enumerate() {
            let rect = theme::GREY.tile_rect(origin, columns, index as u32);
            let middle = (
                rect.x + rect.width as i32 / 2,
                rect.y + rect.height as i32 / 2,
            );
            assert_eq!(
                desk.hit(&windows, middle.0, middle.1),
                Hit::Launch(index),
                "the tile for {} is not openable where it is drawn",
                LAUNCHER[index]
            );
        }
        // The wallpaper is not a tile: the gap between two tiles hits nothing.
        let first = theme::GREY.tile_rect(origin, columns, 0);
        let gap_x = first.x + first.width as i32 + 2;
        assert_eq!(desk.hit(&windows, gap_x, first.y + 10), Hit::Wallpaper);
    }

    #[test]
    fn a_dock_tile_is_identifiable_before_and_after_a_window_appears() {
        let mut windows = Windows::new(640, 400, theme::GREY.cascade);
        let desk = desk(&mut windows);
        let first = windows.create("log", Vec::new(), 200, 140);
        let second = windows.create("count", Vec::new(), 200, 140);
        let dock = desk.dock_rect(2);
        for (index, id) in [first, second].into_iter().enumerate() {
            let tile = theme::GREY.dock_tile_rect(dock, index as u32);
            let (x, y) = (tile.x + 4, tile.y + 4);
            assert_eq!(desk.hit(&windows, x, y), Hit::Dock(id), "dock tile {index}");
        }
        // The dock moves when the number of tiles changes, so the same point stops being the
        // first tile and becomes the dock itself - never the wallpaper behind it.
        let wider = desk.dock_rect(3);
        assert!(wider.x < dock.x, "the dock did not grow to the left");
        assert_ne!(desk.hit(&windows, dock.x + 1, dock.y + 4), Hit::Wallpaper);
    }

    #[test]
    fn a_minimised_window_leaves_the_dock_and_the_count() {
        let mut windows = Windows::new(640, 400, theme::GREY.cascade);
        let desk = desk(&mut windows);
        let id = windows.create("log", Vec::new(), 200, 140);
        let dock = desk.dock_rect(1);
        let tile = theme::GREY.dock_tile_rect(dock, 0);
        let (x, y) = (tile.x + 4, tile.y + 4);
        assert_eq!(desk.hit(&windows, x, y), Hit::Dock(id));
        assert!(windows.minimise(id));
        // No windows: no dock at all, and the place it used to be is the wallpaper.
        assert_eq!(desk.hit(&windows, x, y), Hit::Wallpaper);
    }

    #[test]
    fn the_wallpaper_covers_the_screen_and_the_furniture_covers_the_wallpaper() {
        let mut windows = Windows::new(640, 400, theme::GREY.cascade);
        let desk = desk(&mut windows);
        let _ = windows.create("log", Vec::new(), 200, 140);
        let theme = desk.theme();
        let mut screen = Screen::new(640, 400);
        desk.paint(&mut screen.canvas(), &windows, None);
        // The corners of the screen are wallpaper, and the grid rules are drawn over it.
        assert_eq!(screen.pixel(3, 400 - 3), theme.desktop);
        assert_eq!(
            screen.pixel(0, 3),
            theme.panel,
            "the panel did not reach the edge"
        );
        // The panel's band is its own colour across the whole width.
        for x in [1, 320, 638] {
            assert_eq!(
                screen.pixel(x, 1),
                theme.panel,
                "the panel has a hole at {x}"
            );
        }
    }

    #[test]
    fn a_partially_damaged_screen_paints_only_inside_its_clip() {
        let mut windows = Windows::new(640, 400, theme::GREY.cascade);
        let desk = desk(&mut windows);
        let theme = desk.theme();
        let mut screen = Screen::new(640, 400);
        // A damage rect below the launcher and above the dock, straddling the wallpaper's
        // vertical rule at x=192 and its horizontal rule at y=256, so the rules are the pixels
        // worth asking about: painted where the clip allows and untouched one pixel outside it.
        let clip = Rect::new(180, 250, 24, 24);
        desk.paint(&mut screen.canvas(), &windows, Some(clip));
        assert_eq!(screen.pixel(192, 260), theme.wallpaper_line);
        assert_eq!(screen.pixel(200, 256), theme.wallpaper_line);
        assert_eq!(screen.pixel(200, 260), theme.desktop);
        // The clip's edges are hard, on all four sides.
        assert_eq!(screen.pixel(179, 260), Color::new(0, 0, 0));
        assert_eq!(screen.pixel(204, 260), Color::new(0, 0, 0));
        assert_eq!(screen.pixel(200, 249), Color::new(0, 0, 0));
        assert_eq!(screen.pixel(200, 274), Color::new(0, 0, 0));
        // And the last line inside it is painted, so the rect is not one pixel short.
        assert_eq!(screen.pixel(200, 273), theme.desktop);
    }
}
