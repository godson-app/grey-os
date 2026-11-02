//! The compositor: the desktop, its windows and the pointer, onto one canvas.
//!
//! # What it does, in order
//!
//! The wallpaper, the launcher tiles, then each window back to front, then the top panel and
//! the dock, then the pointer. That order is the whole z-ordering: a window painted later
//! covers an earlier one, so the list's order is the stack, and the furniture is painted last
//! because the panel and the dock are the two bands a window may never cover.
//!
//! # Animation is a function of the clock
//!
//! The one thing here that moves is a title bar's colour as focus changes, and it is stored as
//! an [`Animation`] - a start tick and a duration - rather than as a step taken every frame. So
//! two frames drawn for the same tick draw the same picture, a dropped frame costs a frame
//! instead of desynchronising the fade, and the fade can be tested without a clock at all: a
//! caller that would rather see the end state says so with [`Compositor::settle`].
//!
//! # Damage
//!
//! [`Compositor::draw_all`] is for the first frame. Every frame after it is
//! [`Compositor::draw_damage`], which asks the window list what changed, adds whatever a
//! running transition is about to change, and repaints *only* those rectangles - furniture,
//! then whichever windows reach into them, then the furniture that sits above them. The
//! clipping is not an optimisation detail: a composite that repaints outside the damage is a
//! composite that has to repaint everything to stay correct.
//!
//! # Why it is testable
//!
//! Everything here is a function of a `Canvas`, which is a slice of bytes with a geometry. So
//! the host tests build a small surface, draw a desktop onto it, and read pixels back: a
//! title bar is the title bar's colour, the overlap of two windows belongs to the one in
//! front, and a damage rect that covers one window leaves the other's pixels alone.

use alloc::string::String;
use alloc::vec::Vec;

use crate::anim::{lerp, Animation, Curve, FULL};
use crate::canvas::{
    blend_clipped, blend_clipped_alpha, fill_clipped, Canvas, Color, Corners, Rect,
};
use crate::desk::Desk;
use crate::text;
use crate::theme::Theme;
use crate::ui;
use crate::window::{Window, Windows};

/// A window's text is truncated to this many characters, so a very long line cannot be asked
/// to draw ten thousand glyphs that are all clipped away.
const MAX_LINE_CHARS: usize = 200;

/// How long a title bar takes to change between the focused and unfocused colour, in ticks.
///
/// Eight ticks at the hundred hertz the clock counts at is eighty milliseconds: long enough to
/// read as something moving, short enough that a press does not feel as though it is waiting
/// for the screen.
pub const FOCUS_FADE: u64 = 8;

/// One window's title bar on its way between the unfocused and the focused colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fade {
    /// Which window it belongs to.
    id: u32,
    /// Which colour the bar is heading for: `true` focused, `false` unfocused.
    focused: bool,
    /// Where the bar was when the transition started, from zero to [`FULL`]. A bar that is
    /// told to go back the way it came carries on from where it is rather than jumping.
    from: u8,
    /// The transition, or `None` when the bar has arrived. A window that has just appeared is
    /// settled rather than fading in: it did not change focus, so there is nothing to watch.
    animation: Option<Animation>,
}

impl Fade {
    /// How far from unfocused to focused the bar is at `tick`, from zero to [`FULL`].
    fn progress(&self, tick: u64) -> u8 {
        let target = if self.focused { FULL } else { 0 };
        match self.animation {
            Some(animation) => lerp(self.from, target, animation.progress(tick)),
            None => target,
        }
    }

    /// Whether it is still moving at `tick`.
    fn moving(&self, tick: u64) -> bool {
        self.animation
            .is_some_and(|animation| !animation.done(tick))
    }
}

/// The glyph inside a title bar affordance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mark {
    /// A cross: close, the one that loses work.
    Close,
    /// A bar along the bottom: minimise.
    Minimise,
    /// A bracket: maximise, which fills the work area.
    Maximise,
}

/// The desktop: the window list, the furniture, the theme, and the count of frames drawn.
pub struct Compositor {
    /// The colours and measurements everything is drawn with.
    pub theme: Theme,
    /// The windows, back to front.
    pub windows: Windows,
    /// The wallpaper, the launcher, the panel and the dock, in one place that knows where each
    /// of them is: the pointer asks the same questions the drawing does.
    pub desk: Desk,
    /// How many frames have been painted, for the report.
    pub frames: u64,
    /// Where [`Self::draw_pointer`] last put the pointer, so a frame that moves it can have
    /// the desktop repaint the triangle it left behind. The pointer is painted straight onto
    /// the panel rather than composited from a damage rect, so nothing else covers it, and a
    /// pointer that is never erased is a smear of triangles across every window it crosses.
    pointer: Option<(i32, i32)>,
    /// The tick the frame being drawn belongs to. A frame is a function of this and of the
    /// window list, and of nothing else.
    tick: u64,
    /// One entry per window whose title bar the compositor has seen: what colour it is heading
    /// for, and whether it has arrived.
    fades: Vec<Fade>,
    /// Whether the next frame is drawn with every transition finished. See [`Self::settle`].
    settling: bool,
}

impl Compositor {
    /// An empty desktop on a `width` by `height` panel.
    pub fn new(width: u32, height: u32, theme: Theme) -> Self {
        let desk = Desk::new(theme, width, height);
        let mut windows = Windows::new(width, height, theme.cascade);
        windows.set_min_size(theme.min_window_width, theme.min_window_height);
        // The panel and the dock are not part of the work area, so no window can be dragged
        // over them and a maximised one fits between the two exactly.
        windows.set_work_area(desk.work_area());
        Self {
            theme,
            windows,
            desk,
            frames: 0,
            pointer: None,
            tick: 0,
            fades: Vec::new(),
            settling: false,
        }
    }

    /// Creates a window at the next cascade position. See [`Windows::create`].
    pub fn create(&mut self, title: &str, lines: Vec<String>, width: u32, height: u32) -> u32 {
        self.windows.create(title, lines, width, height)
    }

    /// Tells the compositor what the clock reads, which is what a transition is measured
    /// against. Called once per frame by whoever owns the clock.
    pub fn set_tick(&mut self, tick: u64) {
        self.tick = tick;
    }

    /// The tick the last frame was drawn for.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Whether anything is still moving, so a caller knows a frame is worth drawing even
    /// though no window changed.
    pub fn animating(&self) -> bool {
        self.fades.iter().any(|fade| fade.moving(self.tick))
    }

    /// Draws the next frame with every transition finished.
    ///
    /// A window raised while the clock stands still would otherwise be left halfway between
    /// two colours forever. A caller drawing a frame to be looked at rather than watched says
    /// so here; the tests use it to assert exact colours without simulating a clock.
    pub fn settle(&mut self) {
        self.settling = true;
    }

    /// Paints the whole panel and returns how many windows it drew.
    ///
    /// It also clears the damage list: after a full frame nothing is pending any more, and
    /// leaving it there would make the next [`Self::draw_damage`] repaint what was already
    /// correct.
    pub fn draw_all(&mut self, canvas: &mut Canvas<'_>) -> usize {
        self.frames += 1;
        self.begin_frame();
        self.paint(canvas, None);
        self.windows.take_damage();
        self.windows.count()
    }

    /// Paints the pending damage and returns the rects it repainted.
    ///
    /// An empty answer means nothing changed, and the canvas was not touched at all.
    pub fn draw_damage(&mut self, canvas: &mut Canvas<'_>) -> Vec<Rect> {
        self.begin_frame();
        let mut damage = self.windows.take_damage();
        damage.extend(self.animation_damage());
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

    /// Records what every window's title bar is doing this frame, and lets a settling frame
    /// finish the job before anything is drawn.
    fn begin_frame(&mut self) {
        self.sync_fades();
        if self.settling {
            self.settling = false;
            for fade in &mut self.fades {
                fade.animation = None;
            }
        }
    }

    /// Records which colour each window's title bar is heading for, starting a transition for
    /// any window whose state changed since the last frame.
    ///
    /// A window that has never been seen is recorded as settled: a window that appears is not
    /// a window that changed focus, so fading its bar in from grey would be an animation of
    /// nothing - on the frame a user is most likely to be looking at.
    fn sync_fades(&mut self) {
        let focus = self.windows.focused();
        let tick = self.tick;
        let fades = &mut self.fades;
        for window in self.windows.iter() {
            let focused = focus == Some(window.id);
            match fades.iter_mut().find(|fade| fade.id == window.id) {
                Some(fade) if fade.focused == focused => {}
                Some(fade) => {
                    // The curve a colour change uses: smooth at both ends, where a step
                    // would be the one thing a viewer noticed.
                    let from = fade.progress(tick);
                    *fade = Fade {
                        id: window.id,
                        focused,
                        from,
                        animation: Some(Animation::new(tick, FOCUS_FADE, Curve::EaseInOut)),
                    };
                }
                None => fades.push(Fade {
                    id: window.id,
                    focused,
                    from: if focused { FULL } else { 0 },
                    animation: None,
                }),
            }
        }
    }

    /// The rects a running transition is about to change, marking the ones that have arrived
    /// as settled so that the frame after this one is quiet.
    ///
    /// A fade needs this because it is the one thing on screen that changes without the window
    /// list being told: without it a title bar would be left halfway between two colours for
    /// as long as nobody touched the desktop.
    fn animation_damage(&mut self) -> Vec<Rect> {
        let tick = self.tick;
        let mut damage = Vec::new();
        let mut settled = Vec::with_capacity(self.fades.len());
        for fade in self.fades.drain(..) {
            // A window that closed mid-fade is gone, and the close damaged where it was:
            // there is nothing left for the compositor to repaint on its behalf.
            let Some(window) = self.windows.window(fade.id) else {
                continue;
            };
            match fade.animation {
                Some(_) => {
                    // Both the frames it is still moving on and the one it arrives on: the
                    // last of them is what paints the colour it was heading for.
                    damage.push(self.theme.title_rect(window.rect));
                    let animation = if fade.moving(tick) {
                        fade.animation
                    } else {
                        None
                    };
                    settled.push(Fade { animation, ..fade });
                }
                None => settled.push(fade),
            }
        }
        self.fades = settled;
        damage
    }

    /// Fills the desktop and everything that reaches into `clip`, in z-order: wallpaper,
    /// launcher, windows, panel, dock.
    fn paint(&mut self, canvas: &mut Canvas<'_>, clip: Option<Rect>) {
        self.desk.paint_wallpaper(canvas, clip);
        self.desk.paint_launcher(canvas, clip);
        for window in self.windows.iter() {
            if window.minimised {
                continue;
            }
            if let Some(clip) = clip {
                if window.rect.intersection(clip).is_none() {
                    continue;
                }
            }
            let focused = self.windows.focused() == Some(window.id);
            self.paint_window(canvas, window, focused, clip);
        }
        // The two bands a window may never cover, so they are painted after every window.
        self.desk.paint_panel(canvas, &self.windows, clip);
        self.desk.paint_dock(canvas, &self.windows, clip);
    }

    /// Paints one window: shadow, edge, title bar, title, affordances, body, body text.
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
        let radius = theme.corner_radius;
        let edge = if focused {
            theme.edge_focused
        } else {
            theme.edge
        };

        // The shadow, cast straight down: what lifts a window off the desktop instead of
        // leaving it looking like a hole cut in it. First, so every part of the window that
        // covers it wins, and skipped entirely when the theme asks for none.
        if theme.shadow_alpha > 0 {
            let shadow = Rect::new(
                frame.x,
                frame.y + theme.shadow_offset,
                frame.width,
                frame.height,
            );
            blend_clipped_alpha(
                canvas,
                shadow,
                Corners::uniform(radius),
                theme.shadow,
                theme.shadow_alpha,
                clip,
            );
        }

        // The corners come off the outside of the frame, so the border keeps its thickness all
        // the way round the curve.
        blend_clipped(canvas, frame, Corners::uniform(radius), edge, clip);

        // Everything inside the border, with its corners a border-width tighter than the
        // frame's: that is what nests the title bar and body inside the curve instead of
        // letting them cut across it.
        let inner = Rect::new(
            frame.x + border as i32,
            frame.y + border as i32,
            frame.width.saturating_sub(border * 2).max(1),
            frame.height.saturating_sub(border * 2).max(1),
        );
        let inner_radius = radius.saturating_sub(border);

        // The title bar is round at the top, where it meets the frame, and square at the
        // bottom, where it meets the body. Square-on-square is what keeps the seam between
        // them a straight line with no notch in the middle of the window.
        let title_bar = Rect::new(inner.x, inner.y, inner.width, theme.title_height);
        let title_color = mix(
            theme.title,
            theme.title_focused,
            self.fade_progress(window.id),
        );
        blend_clipped(
            canvas,
            title_bar,
            Corners::new(inner_radius, inner_radius, 0, 0),
            title_color,
            clip,
        );

        // The title stops at the affordances rather than running under them: a title cut by a
        // cross is a title nobody can read.
        let (cell_width, cell_height) = text::cell_size(theme.text_scale);
        let title_x = title_bar.x + theme.padding as i32;
        let title_y = title_bar.y + (title_bar.height.saturating_sub(cell_height)) as i32 / 2;
        let room = theme.maximise_rect(frame).x - theme.padding as i32 - title_x;
        let mut title = String::from(window.title.as_str());
        truncate(
            &mut title,
            (room.max(0) / cell_width.max(1) as i32) as usize,
        );
        text::draw_str_clipped(
            canvas,
            title_x,
            title_y,
            &title,
            theme.title_text,
            theme.text_scale,
            clip,
        );
        for (rect, mark, ink) in [
            (theme.close_rect(frame), Mark::Close, theme.control_close),
            (theme.minimise_rect(frame), Mark::Minimise, theme.control),
            (theme.maximise_rect(frame), Mark::Maximise, theme.control),
        ] {
            self.paint_mark(canvas, mark, rect, ink, clip);
        }

        // The body fills what is left inside the border, below the title bar: square at the
        // top where the title bar meets it, round at the bottom for the frame's curve.
        let body = Rect::new(
            inner.x,
            title_bar.y + title_bar.height as i32,
            inner.width,
            inner.height.saturating_sub(theme.title_height).max(1),
        );
        blend_clipped(
            canvas,
            body,
            Corners::new(0, 0, inner_radius, inner_radius),
            theme.window,
            clip,
        );

        let (columns, rows) = theme.body_cells(frame.width, frame.height);
        let text_x = body.x + theme.padding as i32;
        let mut text_y = body.y + theme.padding as i32;
        for line in window.visible_lines(rows) {
            let mut line = String::from(line.as_str());
            truncate(&mut line, columns.min(MAX_LINE_CHARS));
            text::draw_str_clipped(
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

        // A window's own controls, mapped from its content's corner onto the screen: the only
        // place that knows where content space starts. That corner is the *body's*, not the
        // padding inside it - `ui::column` insets its rows by the padding itself - and it is
        // the same origin the window list classifies a press from, so a button is drawn and hit
        // at one set of numbers.
        ui::paint_at(canvas, &theme, &window.widgets, body.x, body.y, clip);
    }

    /// How far a window's title bar has come from unfocused to focused, zero to [`FULL`].
    fn fade_progress(&self, id: u32) -> u8 {
        match self.fades.iter().find(|fade| fade.id == id) {
            Some(fade) => fade.progress(self.tick),
            None if self.windows.focused() == Some(id) => FULL,
            None => 0,
        }
    }

    /// Draws one title bar affordance's glyph inside `rect`.
    fn paint_mark(
        &self,
        canvas: &mut Canvas<'_>,
        mark: Mark,
        rect: Rect,
        ink: Color,
        clip: Option<Rect>,
    ) {
        // A quarter of the affordance as margin on each side, which at twelve pixels leaves a
        // six-pixel glyph: the smallest that still reads as a shape rather than a smudge.
        let inset = (rect.width / 4).max(1) as i32;
        let left = rect.x + inset;
        let top = rect.y + inset;
        let side = rect.width.saturating_sub(inset as u32 * 2).max(1) as i32;
        match mark {
            Mark::Close => {
                for step in 0..side {
                    fill_clipped(canvas, Rect::new(left + step, top + step, 1, 1), ink, clip);
                    fill_clipped(
                        canvas,
                        Rect::new(left + step, top + side - 1 - step, 1, 1),
                        ink,
                        clip,
                    );
                }
            }
            Mark::Minimise => {
                // A bar along the bottom, where the window goes.
                fill_clipped(
                    canvas,
                    Rect::new(left, top + side - (side / 3).max(1), side as u32, 1),
                    ink,
                    clip,
                );
            }
            Mark::Maximise => {
                // An outlined square: the shape a window takes when it fills the work area.
                fill_clipped(canvas, Rect::new(left, top, side as u32, 1), ink, clip);
                fill_clipped(
                    canvas,
                    Rect::new(left, top + side - 1, side as u32, 1),
                    ink,
                    clip,
                );
                fill_clipped(canvas, Rect::new(left, top, 1, side as u32), ink, clip);
                fill_clipped(
                    canvas,
                    Rect::new(left + side - 1, top, 1, side as u32),
                    ink,
                    clip,
                );
            }
        }
    }
}

/// The rect a pointer at `(x, y)` covers: sixteen rows tall, and as wide as the widest of
/// them plus the outline.
///
/// Public because a back buffer erases the pointer by blitting this rect from the desktop it
/// holds, and the presenter has to name the same footprint the drawing code uses - a second
/// copy of the arithmetic is a second thing to forget to change.
pub fn pointer_rect(x: i32, y: i32) -> Rect {
    const HEIGHT: i32 = 16;
    Rect::new(x, y, (HEIGHT * 2 / 3) as u32 + 2, HEIGHT as u32)
}

/// `from` to `to` at `progress`, a channel at a time.
fn mix(from: Color, to: Color, progress: u8) -> Color {
    Color::new(
        lerp(from.r, to.r, progress),
        lerp(from.g, to.g, progress),
        lerp(from.b, to.b, progress),
    )
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

    /// A desktop whose windows cast no shadow.
    ///
    /// The tests that read a single sharp edge use this: a shadow lands inside the frame's own
    /// corner band, where "this pixel is a blend of two colours" would then be true for the
    /// wrong reason and the claim about anti-aliasing would prove nothing.
    fn unshadowed() -> (Compositor, Surface) {
        let theme = Theme {
            shadow_alpha: 0,
            ..GREY
        };
        (Compositor::new(400, 300, theme), Surface::new(400, 300))
    }

    fn lines(text: &str) -> Vec<String> {
        vec![String::from(text)]
    }

    #[test]
    fn the_desktop_is_the_theme_colour_where_nothing_is_drawn() {
        let (mut desktop, mut surface) = compositor();
        assert_eq!(desktop.draw_all(&mut surface.canvas()), 0);
        // The bottom right is wallpaper: no launcher tile reaches it, there is no dock to draw
        // because there are no windows, and no grid rule crosses that pixel.
        assert_eq!(surface.pixel(399, 299), GREY.desktop);
        // The top left is the panel, which is drawn over the wallpaper across the full width.
        assert_eq!(surface.pixel(0, 0), GREY.panel);
    }

    #[test]
    fn a_window_paints_its_edge_its_title_bar_and_its_body() {
        let (mut desktop, mut surface) = compositor();
        let id = desktop.create("log", lines("hello"), 200, 150);
        let frame = desktop.windows.window(id).expect("window").rect;
        desktop.draw_all(&mut surface.canvas());

        // The corners are rounded: the true corner of the frame is desktop, because the quarter
        // circle cuts it away, which is the difference between this and a square frame.
        assert_eq!(
            surface.pixel(frame.x as usize, frame.y as usize),
            GREY.desktop
        );
        // The middle of the left edge is the edge colour, and it is the focused colour because
        // a new window has focus.
        let edge_y = frame.y as usize + frame.height as usize / 2;
        assert_eq!(surface.pixel(frame.x as usize, edge_y), GREY.edge_focused);
        // Inside the edge and inside the title bar.
        let title_x = frame.x as usize + GREY.border_width as usize + 4;
        let title_y = frame.y as usize + GREY.border_width as usize + 4;
        assert_eq!(surface.pixel(title_x, title_y), GREY.title_focused);
        // The bottom of the body, below the last line of text and clear of the rounded
        // corners, is the window's own colour.
        let body_x = frame.x as usize + frame.width as usize / 2;
        let body_y = frame.y as usize + frame.height as usize - 4;
        assert_eq!(surface.pixel(body_x, body_y), GREY.window);
        // Between the outer curve and the title bar's own curve the frame's edge shows through:
        // the border is the same thickness around the corner as it is along the sides.
        assert_eq!(
            surface.pixel(frame.x as usize + 3, frame.y as usize + 3),
            GREY.edge_focused
        );
        // The pixel just outside the frame is still the desktop.
        assert_eq!(
            surface.pixel(frame.x as usize + frame.width as usize, frame.y as usize),
            GREY.desktop
        );
    }

    #[test]
    fn a_widget_is_painted_where_a_press_would_find_it() {
        // One rect, two readers: the compositor maps a widget from content space onto the
        // screen, and the window list maps a press back into it. This is the test that pins the
        // origin they agree on - the body's own corner, since `ui::column` insets by the padding
        // itself - so a pixel of padding added on one side only fails here.
        let (mut desktop, mut surface) = compositor();
        let id = desktop.create("system", Vec::new(), 240, 200);
        let frame = desktop.windows.window(id).expect("window").rect;
        let body = GREY.body_rect(frame);
        let content = ui::column(
            &GREY,
            body.width,
            body.height,
            &[(ui::Kind::Button, ui::Action::Command(3), "press")],
        );
        assert!(desktop.windows.set_widgets(id, content));
        desktop.draw_all(&mut surface.canvas());

        let button = desktop.windows.window(id).expect("window").widgets[0].rect;
        let (x, y) = (body.x + button.x + 2, body.y + button.y + 2);
        assert_eq!(
            surface.pixel(x as usize, y as usize),
            GREY.button,
            "the button is not painted where its content rect says"
        );
        assert_eq!(
            crate::wm::Session::target(&GREY, &desktop.windows, x, y),
            crate::wm::Target::Command {
                window: id,
                action: ui::Action::Command(3),
            },
            "the press and the paint disagree about where the button is"
        );
    }

    #[test]
    fn the_rounded_corner_is_blended_rather_than_a_staircase() {
        // A corner rounded by skipping whole pixels would leave every pixel of it either the
        // desktop or the edge colour: the arc would be a staircase. Coverage is sixteen samples
        // a pixel, so the frame's own border band has to hold at least one pixel that is a blend
        // of the two. The band is what makes the claim unambiguous: the inner rectangle starts a
        // border further in, so nothing else paints there.
        let (mut desktop, mut surface) = unshadowed();
        let id = desktop.create("log", lines("hello"), 200, 150);
        let frame = desktop.windows.window(id).expect("window").rect;
        desktop.draw_all(&mut surface.canvas());

        let (radius, border) = (GREY.corner_radius, GREY.border_width);
        let mut blended = 0;
        for dy in 0..radius {
            for dx in 0..radius {
                if dx >= border && dy >= border {
                    continue;
                }
                let x = frame.x as usize + dx as usize;
                let y = frame.y as usize + dy as usize;
                let colour = surface.pixel(x, y);
                if colour != GREY.desktop && colour != GREY.edge_focused {
                    blended += 1;
                }
            }
        }
        assert!(
            blended > 0,
            "every pixel of the corner is the desktop or the edge, so the arc is a staircase"
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
        // Raising the back window swaps which one owns the overlap. Settled, so that the
        // assertion is about which window is in front rather than about how far through the
        // focus fade the frame happens to be.
        assert!(desktop.windows.raise(back));
        desktop.settle();
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
        // Where it was is the desktop again, clear of the wallpaper's grid rules...
        assert_eq!(surface.pixel(15, 35), GREY.desktop);
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
        assert_eq!(surface.pixel(15, 35), GREY.desktop);
        // The middle of the window's left edge, away from the rounded corners. The window is
        // 120 tall and was moved to y = 150, so its left edge runs to y = 210.
        assert_eq!(surface.pixel(200, 210), GREY.edge_focused);
    }

    #[test]
    fn a_damage_rect_over_a_corner_paints_what_a_whole_frame_would() {
        // The same rounded frame painted twice: once in one call, once in four clipped calls
        // that tile it. Because the coverage function is asked about absolute pixels rather
        // than positions inside the clip, the two surfaces must agree byte for byte, including
        // the anti-aliased arc. This is the invariant that lets damage rectangles exist.
        let shape = Rect::new(10, 10, 40, 40);
        let corners = Corners::uniform(10);
        let mut whole = Surface::new(60, 60);
        blend_clipped(&mut whole.canvas(), shape, corners, Color::WHITE, None);
        let mut tiled = Surface::new(60, 60);
        for clip in [
            Rect::new(10, 10, 20, 20),
            Rect::new(30, 10, 20, 20),
            Rect::new(10, 30, 20, 20),
            Rect::new(30, 30, 20, 20),
        ] {
            blend_clipped(
                &mut tiled.canvas(),
                shape,
                corners,
                Color::WHITE,
                Some(clip),
            );
        }
        assert_eq!(whole.pixels, tiled.pixels);
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
        // The right edge of the frame, halfway down and clear of the rounded corners, is still
        // the frame's colour, not a glyph from the line that ran past it.
        let x = frame.x as usize + frame.width as usize - 1;
        let y = frame.y as usize + frame.height as usize / 2;
        assert_eq!(
            surface.pixel(x, y),
            GREY.edge_focused,
            "a clipped line painted over the frame"
        );
    }
}
