//! The widget toolkit: what a window's content is made of.
//!
//! # Content is a list of widgets with rects, not a tree with a layout engine
//!
//! A window's content is a vertical list: a heading, some labels, a separator, a button. That
//! is every window this system has, and it is what a column layout produces in one pass -
//! [`column`] hands back widgets that already know where they are. There is no measure pass,
//! no flex, and no parent to ask: a widget's rect *is* its layout, which makes "what did I
//! click" a rectangle test and "where is it drawn" the same number.
//!
//! # Where the coordinates are
//!
//! A widget's rect is in its window's content space: `(0, 0)` is the top-left of the body,
//! inside the border and below the title bar. The compositor paints the content into a
//! surface of exactly that size and blits it, so nothing here has to know where on the screen
//! the window is - which is why the toolkit's tests need no screen at all.
//!
//! # State is a value the pointer sets, not something a widget decides
//!
//! [`State`] is set by the desktop's pointer handling before it paints: rest, hover, pressed.
//! A widget does not decide that it is hovered; the pointer does. That keeps this module pure
//! drawing and hit testing, and it is why hovering cannot change layout - only colour.

use alloc::string::String;
use alloc::vec::Vec;

use crate::canvas::{blend_clipped, fill_clipped, Canvas, Color, Corners, Rect};
use crate::text;
use crate::theme::Theme;

/// What a widget is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A line of body text.
    Label,
    /// A line of large text: the top of a panel's content.
    Heading,
    /// A button: a rounded rectangle with a label, which does something when pressed.
    Button,
    /// A one-pixel rule between two groups.
    Separator,
}

/// What a widget does when it is pressed.
///
/// An identifier rather than a closure: the kernel has no cheap way to own a captured
/// environment, and a number is enough for the window's owner to know what was pressed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Action {
    /// Nothing: a label, a heading, a separator.
    #[default]
    None,
    /// The window's own action, numbered by whoever built the content.
    Command(u8),
}

impl Action {
    /// Whether this is something a press can activate.
    pub fn is_interactive(&self) -> bool {
        matches!(self, Action::Command(_))
    }
}

/// How a widget is being pointed at.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum State {
    /// Not pointed at.
    #[default]
    Rest,
    /// The pointer is over it.
    Hover,
    /// The pointer is over it and the button is down.
    Pressed,
}

/// One widget: what it is, what it says, where it is, and how it looks right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Widget {
    /// What it draws.
    pub kind: Kind,
    /// What it does.
    pub action: Action,
    /// The text it shows.
    pub label: String,
    /// Where it is, in the window's content space.
    pub rect: Rect,
    /// How it is being pointed at.
    pub state: State,
}

impl Widget {
    /// Whether `(x, y)` in content space is over this widget.
    pub fn hit(&self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }
}

/// The height a row of `kind` occupies, without the gap after it.
fn row_height(theme: &Theme, kind: Kind) -> u32 {
    match kind {
        Kind::Label => text::cell_size(theme.text_scale).1,
        Kind::Heading => text::cell_size(theme.display_scale).1,
        Kind::Button => theme.button_height,
        // A rule is a pixel of ink inside a row the height of the gap around it, so it sits
        // between two groups rather than touching either.
        Kind::Separator => theme.row_gap * 2,
    }
}

/// Lays out a top-down column of widgets in `width` by `height` content space.
///
/// A row that would not fit is dropped rather than drawn past the bottom edge: a window
/// resized smaller than its content loses its last rows, which is what a clipped window looks
/// like anyway, and it keeps every returned rect inside the surface it will be painted on.
pub fn column(theme: &Theme, width: u32, height: u32, items: &[Item<'_>]) -> Vec<Widget> {
    let padding = theme.padding;
    let inner = width.saturating_sub(padding * 2);
    let bottom = height.saturating_sub(padding) as i32;
    let mut widgets = Vec::with_capacity(items.len());
    let mut y = padding as i32;
    for (kind, action, label) in items {
        let row = row_height(theme, *kind);
        if y + row as i32 > bottom {
            break;
        }
        widgets.push(Widget {
            kind: *kind,
            action: *action,
            label: String::from(*label),
            rect: Rect::new(padding as i32, y, inner, row),
            state: State::Rest,
        });
        y += (row + theme.row_gap) as i32;
    }
    widgets
}

/// The index of the topmost widget under `(x, y)` in content space, if it is interactive.
///
/// Only interactive widgets are returned: a label has no action, so a press on it is a press
/// on the window, not on the text.
pub fn hit(widgets: &[Widget], x: i32, y: i32) -> Option<usize> {
    widgets
        .iter()
        .enumerate()
        .rev()
        .find(|(_, widget)| widget.action.is_interactive() && widget.hit(x, y))
        .map(|(index, _)| index)
}

/// Sets every widget's state from the pointer, and returns whether anything changed.
///
/// One call for the whole content: a caller that set states widget by widget would have to
/// remember to clear the last one, and a hover that never clears is a button that stays lit.
pub fn point(widgets: &mut [Widget], pointer: Option<(i32, i32)>, down: bool) -> bool {
    let mut changed = false;
    for widget in widgets.iter_mut() {
        let state = match pointer {
            Some((x, y)) if widget.action.is_interactive() && widget.hit(x, y) => {
                if down {
                    State::Pressed
                } else {
                    State::Hover
                }
            }
            _ => State::Rest,
        };
        if state != widget.state {
            widget.state = state;
            changed = true;
        }
    }
    changed
}

/// One entry in a column, before it has a rect.
pub type Item<'a> = (Kind, Action, &'a str);

/// The colour a button is painted with, given its state.
fn surface_color(theme: &Theme, widget: &Widget) -> Color {
    match widget.state {
        State::Rest => theme.button,
        State::Hover => theme.button_hover,
        State::Pressed => theme.button_active,
    }
}

/// `text` cut down to the characters that fit in `width` pixels at `scale`.
///
/// Cut by character rather than by byte: a multi-byte character sliced in half is not a
/// string, and drawing one is a panic on a machine with no way to report it.
fn fitted(text: &str, width: u32, scale: usize) -> String {
    let cell = text::cell_size(scale).0.max(1);
    let columns = (width / cell) as usize;
    text.chars().take(columns).collect()
}

/// The part of `rect` that lies inside `clip`, or `None` if none of it does.
fn within(rect: Rect, clip: Option<Rect>) -> Option<Rect> {
    match clip {
        Some(clip) => rect.intersection(clip),
        None => (rect.width > 0 && rect.height > 0).then_some(rect),
    }
}

/// `rect` moved so that `(x, y)` is its top-left.
fn moved(rect: Rect, x: i32, y: i32) -> Rect {
    Rect::new(rect.x + x, rect.y + y, rect.width, rect.height)
}

/// Paints one widget at `rect`, keeping inside `clip`.
///
/// `rect` is where the widget goes rather than where its layout put it, which is what lets a
/// caller paint a content-space column anywhere on a canvas without a second copy of the
/// layout: the rect is moved, everything else - the label, the state, the shape - is the
/// widget's own.
fn paint_in(
    canvas: &mut Canvas<'_>,
    theme: &Theme,
    widget: &Widget,
    rect: Rect,
    clip: Option<Rect>,
) {
    if within(rect, clip).is_none() {
        return;
    }
    match widget.kind {
        Kind::Label => {
            text::draw_str_clipped(
                canvas,
                rect.x,
                rect.y,
                &widget.label,
                theme.text,
                theme.text_scale,
                clip,
            );
        }
        Kind::Heading => {
            text::draw_str_clipped(
                canvas,
                rect.x,
                rect.y,
                &widget.label,
                theme.title_text,
                theme.display_scale,
                clip,
            );
        }
        Kind::Separator => {
            let rule = Rect::new(rect.x, rect.y + (rect.height / 2) as i32, rect.width, 1);
            fill_clipped(canvas, rule, theme.edge, clip);
        }
        Kind::Button => {
            let radius = (theme.corner_radius / 2).min(rect.height / 2).max(1);
            blend_clipped(
                canvas,
                rect,
                Corners::uniform(radius),
                surface_color(theme, widget),
                clip,
            );
            // The label is centred in the button and cut to what fits: a button whose text
            // spills past its own edge is worse than a button with a shorter word on it.
            let label = fitted(&widget.label, rect.width, theme.text_scale);
            let (width, height) = text::measure(&label, theme.text_scale);
            let x = rect.x + (i64::from(rect.width) - width as i64).max(0) as i32 / 2;
            let y = rect.y + (i64::from(rect.height) - height as i64).max(0) as i32 / 2;
            text::draw_str_clipped(
                canvas,
                x,
                y,
                &label,
                theme.button_text,
                theme.text_scale,
                clip,
            );
        }
    }
}

/// Paints one widget where its own rect says it is.
pub fn paint_widget(canvas: &mut Canvas<'_>, theme: &Theme, widget: &Widget) {
    paint_in(canvas, theme, widget, widget.rect, None);
}

/// Paints a whole column of widgets where their rects say they are.
pub fn paint(canvas: &mut Canvas<'_>, theme: &Theme, widgets: &[Widget]) {
    for widget in widgets {
        paint_widget(canvas, theme, widget);
    }
}

/// Paints a column whose rects are content-space, into a body whose top-left is `(x, y)`.
///
/// This is the only place that knows where a window's content sits on the screen. The
/// compositor passes the body's corner and the rect it is repainting, and the clipping keeps a
/// damage frame from writing outside the part of the screen it owns.
pub fn paint_at(
    canvas: &mut Canvas<'_>,
    theme: &Theme,
    widgets: &[Widget],
    x: i32,
    y: i32,
    clip: Option<Rect>,
) {
    for widget in widgets {
        paint_in(canvas, theme, widget, moved(widget.rect, x, y), clip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{ChannelOrder, Geometry};
    use crate::theme::GREY;

    /// A surface in memory: four bytes per pixel, red first, which is what the canvas wants.
    struct Surface {
        pixels: Vec<u8>,
        width: usize,
        height: usize,
    }

    impl Surface {
        fn new(width: usize, height: usize) -> Self {
            Self {
                pixels: alloc::vec![0; width * height * 4],
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

    /// The content every test here lays out: a heading, a line and the one button.
    fn items() -> [(Kind, Action, &'static str); 3] {
        [
            (Kind::Heading, Action::None, "grey os"),
            (Kind::Label, Action::None, "a kernel"),
            (Kind::Button, Action::Command(0), "press"),
        ]
    }

    fn content(width: u32, height: u32) -> Vec<Widget> {
        column(&GREY, width, height, &items())
    }

    #[test]
    fn a_column_insets_its_rows_by_the_padding_and_drops_what_does_not_fit() {
        let widgets = content(200, 200);
        assert_eq!(widgets.len(), items().len(), "a row that fits was dropped");
        for widget in &widgets {
            assert_eq!(
                widget.rect.x, GREY.padding as i32,
                "a row did not start at the padding"
            );
            assert_eq!(
                widget.rect.width,
                200 - GREY.padding * 2,
                "a row did not end at the padding"
            );
            assert!(
                widget.rect.y >= GREY.padding as i32
                    && widget.rect.y + widget.rect.height as i32 <= 200 - GREY.padding as i32,
                "a row left the content it was laid out in"
            );
        }
        // Top down and never overlapping: a column is one pass, not a measure and a place.
        for pair in widgets.windows(2) {
            assert!(
                pair[0].rect.y + pair[0].rect.height as i32 <= pair[1].rect.y,
                "two rows share a pixel"
            );
        }
        // A window too short for its content loses its last rows instead of drawing past its
        // own edge - which is what a clipped window looks like anyway.
        let short = column(&GREY, 200, GREY.padding + 8, &items());
        assert!(
            short.len() < widgets.len(),
            "a short window kept rows that do not fit"
        );
    }

    #[test]
    fn only_a_button_is_a_target_and_only_inside_its_own_rect() {
        let widgets = content(200, 200);
        let (label, button) = (widgets[1].rect, widgets[2].rect);
        assert_eq!(
            hit(&widgets, label.x + 2, label.y + 2),
            None,
            "a label has no action, so a press on it is a press on the window"
        );
        assert_eq!(hit(&widgets, button.x + 2, button.y + 2), Some(2));
        let past = button.x + button.width as i32 + 1;
        assert_eq!(hit(&widgets, past, button.y + 2), None);
        assert_eq!(hit(&widgets, 199, 199), None, "the empty part of a column");
    }

    #[test]
    fn pointing_lights_one_widget_at_a_time_and_says_when_nothing_changed() {
        let mut widgets = content(200, 200);
        let button = widgets[2].rect;
        let (x, y) = (button.x + 2, button.y + 2);
        assert!(
            point(&mut widgets, Some((x, y)), false),
            "the first hover was not a change, so nothing would be repainted"
        );
        assert_eq!(widgets[2].state, State::Hover);
        assert!(
            !point(&mut widgets, Some((x, y)), false),
            "a hover that did not move asked for a repaint"
        );
        assert!(
            point(&mut widgets, Some((x, y)), true),
            "the press was not a change"
        );
        assert_eq!(widgets[2].state, State::Pressed);
        assert_eq!(widgets[1].state, State::Rest, "a label lit up");
        assert!(
            point(&mut widgets, None, false),
            "moving off the column left a button lit"
        );
        assert_eq!(widgets[2].state, State::Rest);
    }

    #[test]
    fn a_button_is_painted_where_a_press_would_find_it() {
        let widgets = content(200, 200);
        let mut surface = Surface::new(220, 220);
        paint_at(&mut surface.canvas(), &GREY, &widgets, 10, 20, None);
        let button = widgets[2].rect;
        let (x, y) = (10 + button.x + 2, 20 + button.y + 2);
        assert_eq!(
            surface.pixel(x as usize, y as usize),
            GREY.button,
            "the button is not painted where its own rect says it is"
        );
        // The same point in content space is what the window manager hit tests, which is the
        // whole point of one rect having two readers.
        assert_eq!(hit(&widgets, button.x + 2, button.y + 2), Some(2));
        // Above the button is a heading on the body, not another button.
        let heading = widgets[0].rect;
        assert_ne!(
            surface.pixel((10 + heading.x + 2) as usize, (20 + heading.y + 2) as usize),
            GREY.button
        );
    }

    #[test]
    fn a_clip_narrows_the_column_without_moving_it() {
        let widgets = content(200, 200);
        let button = widgets[2].rect;
        let (x, y) = ((10 + button.x + 2) as usize, (20 + button.y + 2) as usize);
        let mut surface = Surface::new(220, 220);
        // A band that holds the top of the column and stops above the button.
        let band = Rect::new(0, 0, 220, (20 + button.y) as u32 - 1);
        paint_at(&mut surface.canvas(), &GREY, &widgets, 10, 20, Some(band));
        assert_eq!(
            surface.pixel(x, y),
            Color::new(0, 0, 0),
            "a clipped widget painted outside its clip"
        );
        // The same column against a clip that holds it paints the button: the clip narrows
        // what is painted, it does not decide what exists.
        let whole = Rect::new(0, 0, 220, 219);
        paint_at(&mut surface.canvas(), &GREY, &widgets, 10, 20, Some(whole));
        assert_eq!(surface.pixel(x, y), GREY.button);
    }
}
