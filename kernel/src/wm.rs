//! Interaction: what a press, a movement and a release mean, and which window they belong to.
//!
//! # The pointer is a state machine with one owner
//!
//! A press picks a target and keeps it until the button is released - a *grab*. Every movement
//! while the button is down goes to that target, even when the pointer has left the window's
//! rectangle, which is what makes dragging work at all: a title bar cannot follow a pointer
//! that is no longer over it if the window stops listening at its own edge. Nothing else can
//! take the grab, so a drag cannot be interrupted by a window appearing underneath.
//!
//! The states are idle, pressed and dragging, and they are derived from three values that
//! arrive from the device: the position, whether the button is down, and whether it *became*
//! down since the last packet. Edge detection belongs here rather than in the shell loop: a
//! click is the transition, not the level, and a shell that raised a window on every packet
//! would rearrange the desktop under somebody's hand.
//!
//! # Geometry comes from the theme
//!
//! Whether a point is a title bar, an affordance or a resize edge is answered by
//! [`Theme`]'s own rects - the same ones the compositor draws from. This module therefore has
//! no pixel constants of its own beyond the resize band, and a window whose title bar moves is
//! grabbable in its new place without anything being changed twice.
//!
//! # Why it is separate from the compositor
//!
//! Everything here is a function of a window list, a theme and a pointer - no canvas, no
//! pixels. So the tests below drive whole gestures: press a title bar, move by a hundred and
//! thirty-seven pixels, release, and check the window moved by exactly that and left nothing
//! behind.

use crate::canvas::Rect;
use crate::theme::Theme;
use crate::ui::Action;
use crate::window::Windows;

/// Which edges of a window a resize is holding. A corner holds two.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Edges {
    /// The left edge.
    pub left: bool,
    /// The right edge.
    pub right: bool,
    /// The top edge.
    pub top: bool,
    /// The bottom edge.
    pub bottom: bool,
}

impl Edges {
    /// No edge at all.
    pub const NONE: Self = Self {
        left: false,
        right: false,
        top: false,
        bottom: false,
    };

    /// Whether any edge is held.
    pub const fn any(self) -> bool {
        self.left || self.right || self.top || self.bottom
    }
}

/// How wide the band along a window's edge is, in pixels, in which a press starts a resize.
///
/// Wider than the border, because a two-pixel border is a target nobody can hit while moving,
/// and narrow enough that the body of a small window still has room in the middle.
pub const RESIZE_BAND: i32 = 6;

/// What a press landed on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// Nothing: the desktop, the panel or the dock, which are not windows.
    None,
    /// A window's title bar: a move.
    Title(u32),
    /// A window's edge or corner: a resize.
    Edge(u32),
    /// A window's body, but not on anything interactive inside it.
    Body(u32),
    /// A button in a window's content.
    Command {
        /// The window the button is in.
        window: u32,
        /// What the button does.
        action: Action,
    },
    /// A window's close affordance.
    Close(u32),
    /// A window's minimise affordance.
    Minimise(u32),
    /// A window's maximise affordance.
    Maximise(u32),
}

impl Target {
    /// The window the press landed on, if it landed on one.
    pub fn window(self) -> Option<u32> {
        match self {
            Target::None => None,
            Target::Title(id)
            | Target::Edge(id)
            | Target::Body(id)
            | Target::Close(id)
            | Target::Minimise(id)
            | Target::Maximise(id) => Some(id),
            Target::Command { window, .. } => Some(window),
        }
    }

    /// Whether the press was one that puts a window in front.
    ///
    /// Everything but a miss does: a press on a body, a title bar, an edge or one of the
    /// affordances is a press on that window, and the window it pressed has to be the one the
    /// user is looking at.
    pub fn raises(self) -> bool {
        self.window().is_some()
    }
}

/// What a drag is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Grab {
    /// Nothing is held.
    #[default]
    Nothing,
    /// A window is being moved. `dx` and `dy` are where in the frame the pointer grabbed it,
    /// so the frame stays under the same point of the pointer for the whole drag instead of
    /// jumping to it.
    Move {
        /// The window being moved.
        id: u32,
        /// Distance from the frame's left edge to the grab point.
        dx: i32,
        /// Distance from the frame's top edge to the grab point.
        dy: i32,
    },
    /// A window is being resized from `edges`, starting from `start`.
    Resize {
        /// The window being resized.
        id: u32,
        /// Which edges the pointer holds.
        edges: Edges,
        /// The pointer's horizontal position when the drag started.
        grab_x: i32,
        /// The pointer's vertical position when the drag started.
        grab_y: i32,
        /// The frame as it was when the drag started.
        start: Rect,
    },
}

impl Grab {
    /// The window a grab is holding, if it is holding one.
    ///
    /// What a caller needs to say "moving window #4" without matching the grab itself: the
    /// difference between a move and a resize is the window manager's, the difference between
    /// which window is what a shell's log line is for.
    pub const fn window(self) -> Option<u32> {
        match self {
            Grab::Nothing => None,
            Grab::Move { id, .. } | Grab::Resize { id, .. } => Some(id),
        }
    }
}

/// The pointer's state: what it is holding, and whether the button is down.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Session {
    grab: Grab,
    down: bool,
}

impl Session {
    /// A pointer that is up and holding nothing.
    pub const fn new() -> Self {
        Self {
            grab: Grab::Nothing,
            down: false,
        }
    }

    /// What is being held.
    pub const fn grab(&self) -> Grab {
        self.grab
    }

    /// Whether the button is down, as far as this session knows.
    pub const fn is_down(&self) -> bool {
        self.down
    }

    /// Whether a drag is in progress.
    pub const fn dragging(&self) -> bool {
        !matches!(self.grab, Grab::Nothing)
    }

    /// Classifies a point against the windows, using the theme's own geometry.
    ///
    /// The order is the order of the drawing: an affordance sits inside the title bar and the
    /// resize band sits inside the frame, so the more specific target has to be asked first.
    pub fn target(theme: &Theme, windows: &Windows, x: i32, y: i32) -> Target {
        let Some(id) = windows.hit(x, y) else {
            return Target::None;
        };
        let Some(window) = windows.window(id) else {
            return Target::None;
        };
        let frame = window.rect;
        for (rect, target) in [
            (theme.close_rect(frame), Target::Close(id)),
            (theme.minimise_rect(frame), Target::Minimise(id)),
            (theme.maximise_rect(frame), Target::Maximise(id)),
        ] {
            if rect.contains(x, y) {
                return target;
            }
        }
        if edge_at(frame, x, y).any() {
            return Target::Edge(id);
        }
        if theme.title_rect(frame).contains(x, y) {
            return Target::Title(id);
        }
        // Inside the body: a widget first, the window itself second.
        let body = theme.body_rect(frame);
        if let Some(index) = crate::ui::hit(&window.widgets, x - body.x, y - body.y) {
            let action = window.widgets[index].action;
            if action.is_interactive() {
                return Target::Command { window: id, action };
            }
        }
        Target::Body(id)
    }

    /// Starts a press at `(x, y)`: picks the target, raises what it landed on, and takes the
    /// grab if the target is a title bar or an edge.
    ///
    /// The raise is unconditional for every target that names a window, including the close
    /// button: a press that is about to close a window still has to put it in front first, so
    /// the damage a focus change causes happens in the same frame as the close.
    pub fn press(theme: &Theme, windows: &mut Windows, x: i32, y: i32) -> (Self, Target) {
        let target = Self::target(theme, windows, x, y);
        let mut session = Self {
            grab: Grab::Nothing,
            down: true,
        };
        let Some(id) = target.window() else {
            return (session, target);
        };
        let _ = windows.raise(id);
        match target {
            Target::Title(id) => {
                if let Some(window) = windows.window(id) {
                    session.grab = Grab::Move {
                        id,
                        dx: x - window.rect.x,
                        dy: y - window.rect.y,
                    };
                }
            }
            Target::Edge(id) => {
                if let Some(window) = windows.window(id) {
                    session.grab = Grab::Resize {
                        id,
                        edges: edge_at(window.rect, x, y),
                        grab_x: x,
                        grab_y: y,
                        start: window.rect,
                    };
                }
            }
            _ => {}
        }
        (session, target)
    }

    /// Applies a movement to whatever is held, and returns whether the screen changed.
    ///
    /// A movement with nothing held is not a drag - it is the pointer moving over the desktop -
    /// so it damages nothing here. The caller still repaints, because the pointer moved.
    pub fn moved(&mut self, windows: &mut Windows, x: i32, y: i32) -> bool {
        match self.grab {
            Grab::Nothing => false,
            Grab::Move { id, dx, dy } => windows.move_to(id, x - dx, y - dy),
            Grab::Resize {
                id,
                edges,
                grab_x,
                grab_y,
                start,
            } => {
                let (mut left, mut top) = (start.x, start.y);
                let (mut width, mut height) = (start.width as i32, start.height as i32);
                let (move_x, move_y) = (x - grab_x, y - grab_y);
                if edges.left {
                    left += move_x;
                    width -= move_x;
                }
                if edges.right {
                    width += move_x;
                }
                if edges.top {
                    top += move_y;
                    height -= move_y;
                }
                if edges.bottom {
                    height += move_y;
                }
                // A frame dragged inside out has a negative size. The window list clamps to
                // the minimum a window may be, and nothing negative reaches a `Rect`.
                windows.resize_to(id, left, top, width.max(1) as u32, height.max(1) as u32)
            }
        }
    }

    /// Releases the button and drops the grab.
    pub fn release(&mut self) {
        self.grab = Grab::Nothing;
        self.down = false;
    }
}

/// Which edges of `frame` a point is on, within [`RESIZE_BAND`] of them.
///
/// Outside the frame entirely is no edge: a press on the desktop beside a window is not a
/// resize, however close it is. The band is capped at a third of each dimension so a small
/// window still has a middle to drag by its title bar and a body to click.
pub fn edge_at(frame: Rect, x: i32, y: i32) -> Edges {
    let right = frame.x + frame.width as i32;
    let bottom = frame.y + frame.height as i32;
    if x < frame.x || x >= right || y < frame.y || y >= bottom {
        return Edges::NONE;
    }
    let horizontal = RESIZE_BAND.min(frame.width as i32 / 3);
    let vertical = RESIZE_BAND.min(frame.height as i32 / 3);
    Edges {
        left: x < frame.x + horizontal,
        right: x >= right - horizontal,
        top: y < frame.y + vertical,
        bottom: y >= bottom - vertical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use alloc::vec;

    fn desktop() -> Windows {
        let mut windows = Windows::new(1280, 800, 36);
        windows.set_work_area(theme::GREY.work_area(1280, 800));
        windows
    }

    fn window_at(windows: &mut Windows, x: i32, y: i32, width: u32, height: u32) -> u32 {
        let id = windows.create("log", vec![], width, height);
        windows.move_to(id, x, y);
        id
    }

    /// Whether the damage list repaints all of `rect`, whether it kept the two frames apart or
    /// merged them into one.
    fn repaints(damage: &[Rect], rect: Rect) -> bool {
        damage.iter().any(|d| {
            d.x <= rect.x
                && d.y <= rect.y
                && d.x + d.width as i32 >= rect.x + rect.width as i32
                && d.y + d.height as i32 >= rect.y + rect.height as i32
        })
    }

    #[test]
    fn a_press_on_the_title_bar_moves_the_window_by_exactly_the_pointer_delta() {
        let theme = theme::GREY;
        let mut windows = desktop();
        let id = window_at(&mut windows, 120, 90, 400, 300);
        let start = windows.window(id).expect("window").rect;
        let title = theme.title_rect(start);
        let (grab_x, grab_y) = (title.x + 40, title.y + 8);
        let (session, target) = Session::press(&theme, &mut windows, grab_x, grab_y);
        assert_eq!(target, Target::Title(id));
        assert!(session.dragging(), "a title bar press is a drag");
        // The grab names the window it holds, which is what a shell's log line needs to say
        // which window a drag moved without matching the grab itself.
        assert_eq!(session.grab().window(), Some(id));
        let _ = windows.take_damage();
        let mut session = session;
        assert!(session.moved(&mut windows, grab_x + 137, grab_y + 84));
        let now = windows.window(id).expect("window").rect;
        assert_eq!(now.x, start.x + 137);
        assert_eq!(now.y, start.y + 84);
        assert_eq!((now.width, now.height), (start.width, start.height));
        let damage = windows.take_damage();
        assert!(
            repaints(&damage, start) && repaints(&damage, now),
            "the frame was not repainted where it left and where it arrived"
        );
    }

    #[test]
    fn a_window_dragged_up_stops_at_the_panel_rather_than_vanishing_under_it() {
        let theme = theme::GREY;
        let mut windows = desktop();
        let id = window_at(&mut windows, 120, 300, 400, 200);
        let title = theme.title_rect(windows.window(id).expect("window").rect);
        let (mut session, _) = Session::press(&theme, &mut windows, title.x + 40, title.y + 8);
        // Dragged far above the screen: a window whose title bar went under the panel could not
        // be picked up again, so the clamp is at the panel, not at the screen's edge.
        assert!(session.moved(&mut windows, title.x + 40, -600));
        assert_eq!(
            windows.window(id).expect("window").rect.y,
            windows.work_area().y
        );
    }

    #[test]
    fn a_drag_keeps_following_the_pointer_outside_the_window() {
        let theme = theme::GREY;
        let mut windows = desktop();
        let id = window_at(&mut windows, 100, 100, 300, 200);
        let start = windows.window(id).expect("window").rect;
        let title = theme.title_rect(start);
        let (mut session, _) = Session::press(&theme, &mut windows, title.x + 10, title.y + 10);
        // Far outside the frame in both directions: a drag that stopped at the window's edge
        // would leave the window behind.
        assert!(session.moved(&mut windows, 900, 700));
        let now = windows.window(id).expect("window").rect;
        // The window moves by the pointer's delta, not to the pointer: the grab was ten pixels
        // into the frame, so the frame lands ten pixels behind the pointer.
        let (dx, dy) = (900 - (title.x + 10), 700 - (title.y + 10));
        assert_eq!(now.x, start.x + dx);
        assert_eq!(now.y, start.y + dy);
        session.release();
        assert_eq!(session.grab(), Grab::Nothing);
        assert!(!session.dragging());
        assert!(!session.is_down());
        assert!(
            !session.moved(&mut windows, 1000, 750),
            "a released drag moves nothing"
        );
    }

    #[test]
    fn a_press_on_the_body_raises_the_window_but_does_not_move_it() {
        let theme = theme::GREY;
        let mut windows = desktop();
        let back = window_at(&mut windows, 100, 100, 400, 300);
        let _front = window_at(&mut windows, 140, 140, 300, 200);
        let body = theme.body_rect(windows.window(back).expect("window").rect);
        // A point in the back window's body that is not in the front window, not within the
        // resize band and not in the title bar: the strip along the bottom of the frame.
        let (x, y) = (
            body.x + body.width as i32 / 2,
            body.y + body.height as i32 - 20,
        );
        assert!(!edge_at(body, x, y).any(), "the point is in a resize band");
        assert_eq!(Session::target(&theme, &windows, x, y), Target::Body(back));
        let (session, target) = Session::press(&theme, &mut windows, x, y);
        assert_eq!(target, Target::Body(back));
        assert_eq!(windows.focused(), Some(back), "the press did not raise it");
        assert!(!session.dragging());
        assert_eq!(windows.window(back).expect("window").rect.y, 100);
    }

    #[test]
    fn the_affordances_are_hit_exactly_where_they_are_drawn() {
        let theme = theme::GREY;
        let mut windows = desktop();
        let id = window_at(&mut windows, 200, 200, 420, 320);
        let frame = windows.window(id).expect("window").rect;
        for (rect, expected) in [
            (theme.close_rect(frame), Target::Close(id)),
            (theme.minimise_rect(frame), Target::Minimise(id)),
            (theme.maximise_rect(frame), Target::Maximise(id)),
        ] {
            let (x, y) = (rect.x + 1, rect.y + 1);
            assert_eq!(Session::target(&theme, &windows, x, y), expected);
        }
        // One pixel to the left of the close button is the title bar, not close: the buttons
        // must not be targets any larger than they look.
        let close = theme.close_rect(frame);
        assert_eq!(
            Session::target(&theme, &windows, close.x - 1, close.y + 1),
            Target::Title(id)
        );
    }

    #[test]
    fn a_press_on_an_edge_resizes_from_that_edge_only() {
        let theme = theme::GREY;
        let mut windows = desktop();
        let id = window_at(&mut windows, 100, 100, 400, 300);
        let start = windows.window(id).expect("window").rect;
        let (grab_x, grab_y) = (start.x + start.width as i32 - 2, start.y + 150);
        let (mut session, target) = Session::press(&theme, &mut windows, grab_x, grab_y);
        assert_eq!(target, Target::Edge(id));
        assert!(session.moved(&mut windows, grab_x + 60, grab_y + 40));
        let now = windows.window(id).expect("window").rect;
        assert_eq!(now.width, start.width + 60, "the right edge did not move");
        assert_eq!(
            now.height, start.height,
            "a horizontal drag changed the height"
        );
        assert_eq!(
            (now.x, now.y),
            (start.x, start.y),
            "the frame slid instead of growing"
        );
    }

    #[test]
    fn a_resize_cannot_shrink_a_window_past_its_minimum() {
        let theme = theme::GREY;
        let mut windows = desktop();
        let id = window_at(&mut windows, 100, 100, 400, 300);
        let start = windows.window(id).expect("window").rect;
        let (mut session, _) = Session::press(
            &theme,
            &mut windows,
            start.x + start.width as i32 - 2,
            start.y + 150,
        );
        // Dragging the right edge far to the left of the left edge: the window has to stop at
        // its minimum size rather than turn inside out.
        assert!(session.moved(&mut windows, start.x - 500, start.y + 150));
        let now = windows.window(id).expect("window").rect;
        assert_eq!(now.width, 200, "the minimum width was not applied");
        assert!(now.height >= 120);
        assert_eq!(
            now.x, start.x,
            "the left edge moved while the right one was held"
        );
    }

    #[test]
    fn a_minimised_window_is_not_hit_and_focus_falls_to_the_one_behind() {
        let theme = theme::GREY;
        let mut windows = desktop();
        let back = window_at(&mut windows, 100, 100, 400, 300);
        let front = window_at(&mut windows, 100, 100, 400, 300);
        assert_eq!(windows.focused(), Some(front));
        assert!(windows.minimise(front));
        assert_eq!(
            windows.focused(),
            Some(back),
            "focus stayed on a hidden window"
        );
        assert!(!windows.visible(front));
        let frame = windows.window(front).expect("window").rect;
        assert_eq!(windows.hit(frame.x + 10, frame.y + 10), Some(back));
        assert_eq!(
            Session::target(&theme, &windows, frame.x + 10, frame.y + 10),
            Target::Title(back),
            "a press landed on the hidden window"
        );
        assert!(windows.unminimise(front));
        assert!(windows.visible(front));
        assert_eq!(windows.focused(), Some(front), "restoring did not focus it");
    }

    #[test]
    fn a_press_on_an_open_screen_hits_nothing() {
        let theme = theme::GREY;
        let mut windows = desktop();
        let _ = window_at(&mut windows, 100, 100, 200, 150);
        assert_eq!(Session::target(&theme, &windows, 1279, 799), Target::None);
        let (session, target) = Session::press(&theme, &mut windows, 1279, 799);
        assert_eq!(target, Target::None);
        assert!(!session.dragging());
        // A press on nothing still counts as a press, or a click on the desktop would be
        // reported again on every packet the button is held.
        assert!(session.is_down());
    }
}
