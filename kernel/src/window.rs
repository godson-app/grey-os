//! Windows: what is on the screen, in what order, and what has to be redrawn.
//!
//! # Order is the list
//!
//! Back to front, and the last window is the focused one. Raising a window moves it to the
//! end, which makes every question - what is on top at this point, what was clicked - a walk
//! from the back of the list. There is no separate z number to keep in step with the order,
//! which is the bug this shape exists to make impossible.
//!
//! # Damage is a list of rects, not a flag
//!
//! Every change a window makes adds the rect it affected, and the compositor asks for the
//! list. Two overlapping rects become one and a rect inside another disappears, so a frame
//! that moved a window by ten pixels repaints the band that actually changed rather than the
//! whole screen or the whole window. The merging is a plain function over `Rect`s, which is
//! why it has tests of its own: everything else here is bookkeeping around it.
//!
//! # What a window is
//!
//! A title and lines of text. That is enough for the log, the system panel and the shell that
//! the next milestone puts in one, and it keeps the compositor's job to filling rectangles and
//! drawing strings. A window that wants to draw anything else is a new kind of content, not a
//! new window.

use alloc::string::String;
use alloc::vec::Vec;

use crate::canvas::Rect;
use crate::ui::Widget;

/// One window: where it is, what it says, and how far its text is scrolled.
#[derive(Clone, Debug)]
pub struct Window {
    /// Stable for the life of the window, which is what the shell and the pointer hold.
    pub id: u32,
    /// Drawn in the title bar, truncated by the compositor rather than here.
    pub title: String,
    /// The whole frame: border, title bar and body. The theme draws inside it.
    pub rect: Rect,
    /// The body's text, one line per entry.
    pub lines: Vec<String>,
    /// How many lines have been scrolled off the top.
    pub scroll: usize,
    /// The window's interactive content, when it has any. Empty means the window is a text
    /// window and its body is [`Self::lines`].
    pub widgets: Vec<Widget>,
    /// Whether the window is in the dock rather than on the screen. A minimised window keeps
    /// its identity, its content and its place in the list; it is simply not drawn, not hit,
    /// and not in line to be focused by a click.
    pub minimised: bool,
    /// Where the window was before it was maximised, so restoring puts it back rather than
    /// leaving it at the work area's edge. `None` means it is not maximised.
    pub restore: Option<Rect>,
}

impl Window {
    /// Whether `(x, y)` is anywhere in the frame, title bar included.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        contains(self.rect, x, y)
    }

    /// The visible lines, from `scroll` onward, at most `rows` of them.
    pub fn visible_lines(&self, rows: usize) -> impl Iterator<Item = &String> {
        self.lines.iter().skip(self.scroll).take(rows)
    }

    /// Adds a line and keeps the view at the bottom, which is what a log wants.
    pub fn push_line(&mut self, line: String) {
        self.lines.push(line);
        self.scroll = self.lines.len().saturating_sub(1);
    }

    /// Replaces the body's text, for a window that shows state rather than history.
    pub fn set_lines(&mut self, lines: Vec<String>) {
        self.lines = lines;
        self.scroll = 0;
    }
}

/// The windows, back to front.
pub struct Windows {
    windows: Vec<Window>,
    next_id: u32,
    focus: Option<u32>,
    damage: Vec<Rect>,
    /// Where the next window goes, in cascade steps.
    step: i32,
    /// How far one step moves a window.
    cascade: i32,
    /// The panel, for placement, clamping and clipping.
    width: u32,
    height: u32,
    /// The area windows may occupy: the panel minus the top panel and the dock. Placement,
    /// dragging and maximising all clamp to it, so no window can hide under either bar.
    work: Rect,
    /// The smallest a window may be resized to, so its own title bar still fits.
    min_width: u32,
    min_height: u32,
}

impl Windows {
    /// An empty desktop on a `width` by `height` panel, with a cascade step of `cascade`.
    pub fn new(width: u32, height: u32, cascade: i32) -> Self {
        Self {
            windows: Vec::new(),
            next_id: 1,
            focus: None,
            damage: Vec::new(),
            step: 0,
            cascade,
            width,
            height,
            work: Rect::new(0, 0, width, height),
            min_width: 200,
            min_height: 120,
        }
    }

    /// Sets the smallest a window may be resized to, and returns whether it changed.
    pub fn set_min_size(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 || (width, height) == (self.min_width, self.min_height) {
            return false;
        }
        self.min_width = width;
        self.min_height = height;
        true
    }

    /// Narrows the area windows may occupy - the panel minus its top bar and its dock - and
    /// returns whether it changed.
    ///
    /// Called once, by whoever builds the desktop, before any window exists. Everything that
    /// places or clamps a window reads it, which is what keeps a drag from sliding a title bar
    /// under the top panel where it can no longer be grabbed.
    pub fn set_work_area(&mut self, work: Rect) -> bool {
        if work.width == 0 || work.height == 0 || work == self.work {
            return false;
        }
        self.work = work;
        true
    }

    /// The area windows may occupy.
    pub fn work_area(&self) -> Rect {
        self.work
    }

    /// The panel this desktop was built for.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// How many windows there are.
    pub fn count(&self) -> usize {
        self.windows.len()
    }

    /// The focused window's id: the last one in the list.
    pub fn focused(&self) -> Option<u32> {
        self.focus
    }

    /// Every window, back to front.
    pub fn iter(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter()
    }

    /// Every window, front to back: the order a hit test wants.
    pub fn iter_front_to_back(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter().rev()
    }

    /// The window with this id.
    pub fn window(&self, id: u32) -> Option<&Window> {
        self.windows.iter().find(|window| window.id == id)
    }

    /// The window with this id, mutably, without damaging anything: a caller that changes what
    /// a window *says* repaints it itself, and the two events that have to damage - a move and a
    /// raise - do it here.
    pub fn window_mut(&mut self, id: u32) -> Option<&mut Window> {
        self.windows.iter_mut().find(|window| window.id == id)
    }

    /// Creates a window at the next cascade position, in front, and focuses it.
    ///
    /// The position wraps after five steps and is clamped so the frame stays inside the work
    /// area: a window placed off the right or bottom edge, or under the top panel where its
    /// title bar cannot be grabbed, is a window nobody can find.
    pub fn create(&mut self, title: &str, lines: Vec<String>, width: u32, height: u32) -> u32 {
        let step = self.step;
        self.step = (self.step + 1) % 5;
        let work = self.work;
        let width = width.min(self.width);
        let height = height.min(self.height);
        let offset = 24 + step * self.cascade;
        let x = (work.x + offset).min(work.x + work.width.saturating_sub(width) as i32);
        let y = (work.y + offset).min(work.y + work.height.saturating_sub(height) as i32);
        let id = self.next_id;
        self.next_id += 1;
        let window = Window {
            id,
            title: String::from(title),
            rect: Rect::new(x.max(0), y.max(0), width, height),
            lines,
            scroll: 0,
            widgets: Vec::new(),
            minimised: false,
            restore: None,
        };
        let frame = window.rect;
        self.windows.push(window);
        self.refocus(id);
        self.damage.push(frame);
        id
    }

    /// Replaces a window's content with a column of widgets, and damages the body it changed.
    ///
    /// The lines go with it: a window's content is text *or* widgets, and a window that kept
    /// both would have two things claiming the same rectangle - the compositor paints the lines
    /// and the column from the same corner. Returns whether the window existed.
    pub fn set_widgets(&mut self, id: u32, widgets: Vec<Widget>) -> bool {
        let Some(window) = self.window_mut(id) else {
            return false;
        };
        window.widgets = widgets;
        window.lines.clear();
        window.scroll = 0;
        let frame = window.rect;
        self.damage(frame);
        true
    }

    /// Moves focus to `id` and damages the window that had it.
    ///
    /// Damaging both is not optional: focus changes the title bar and the edge of two
    /// windows, and a compositor that repaints only the new one leaves the old one painted as
    /// if it were still in front. The test that caught this is the one that raises the back
    /// window and then looks at the front one's title bar.
    fn refocus(&mut self, id: u32) {
        let previous = self.focus;
        self.focus = Some(id);
        if let Some(previous) = previous {
            if previous != id {
                if let Some(window) = self.window(previous) {
                    self.damage.push(window.rect);
                }
            }
        }
    }

    /// Brings a window to the front and focuses it. Returns whether it existed.
    pub fn raise(&mut self, id: u32) -> bool {
        let Some(index) = self.windows.iter().position(|window| window.id == id) else {
            return false;
        };
        let window = self.windows.remove(index);
        let frame = window.rect;
        self.windows.push(window);
        self.refocus(id);
        self.damage.push(frame);
        true
    }

    /// The topmost window under `(x, y)`, without changing anything.
    ///
    /// A minimised window is not under anything: it is not drawn, so a click where it used to
    /// be belongs to whatever is drawn there now.
    pub fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.iter_front_to_back()
            .find(|window| !window.minimised && window.contains(x, y))
            .map(|window| window.id)
    }

    /// The topmost window under the point, raised and focused: what a click means.
    pub fn click(&mut self, x: i32, y: i32) -> Option<u32> {
        let id = self.hit(x, y)?;
        self.raise(id);
        Some(id)
    }

    /// Moves a window to `(x, y)` and damages both where it was and where it went.
    ///
    /// The frame is clamped so that at least the title bar stays inside the work area: a
    /// window dragged past an edge has to remain findable and draggable, and one whose title
    /// bar slid under the top panel cannot be picked up again.
    pub fn move_to(&mut self, id: u32, x: i32, y: i32) -> bool {
        let work = self.work;
        let Some(window) = self.window_mut(id) else {
            return false;
        };
        let old = window.rect;
        let max_x = (work.x + work.width as i32).saturating_sub(48);
        let max_y = (work.y + work.height as i32).saturating_sub(24);
        let min_x = (work.x - (window.rect.width as i32 - 48)).min(max_x);
        window.rect.x = x.clamp(min_x, max_x);
        window.rect.y = y.clamp(work.y, max_y);
        let new = window.rect;
        self.damage.push(old);
        self.damage.push(new);
        true
    }

    /// Resizes a window to `width` by `height` at `(x, y)`, clamped to the work area and to
    /// the smallest a window may be.
    ///
    /// One call for a whole edge or corner: a resize that moved x but not the width would be a
    /// window that slid instead of growing, so the two cannot be set separately.
    pub fn resize_to(&mut self, id: u32, x: i32, y: i32, width: u32, height: u32) -> bool {
        let (work, min_width, min_height) = (self.work, self.min_width, self.min_height);
        let Some(window) = self.window_mut(id) else {
            return false;
        };
        let old = window.rect;
        let width = width.clamp(min_width, work.width.max(min_width));
        let height = height.clamp(min_height, work.height.max(min_height));
        let x = x.clamp(work.x, work.x + (work.width as i32 - width as i32).max(0));
        let y = y.clamp(work.y, work.y + (work.height as i32 - height as i32).max(0));
        window.rect = Rect::new(x, y, width, height);
        let new = window.rect;
        if new == old {
            return true;
        }
        self.damage.push(old);
        self.damage.push(new);
        true
    }

    /// Fills the work area with a window, remembering where it was.
    ///
    /// Maximising an already maximised window does nothing: pressing the button again is a
    /// no-op rather than a window that shrinks to its own size.
    pub fn maximise(&mut self, id: u32) -> bool {
        let work = self.work;
        let Some(window) = self.window_mut(id) else {
            return false;
        };
        if window.restore.is_some() {
            return false;
        }
        let old = window.rect;
        window.restore = Some(old);
        window.rect = work;
        window.minimised = false;
        self.damage.push(old);
        self.damage.push(work);
        true
    }

    /// Puts a maximised window back where it was.
    pub fn restore_window(&mut self, id: u32) -> bool {
        let Some(window) = self.window_mut(id) else {
            return false;
        };
        let Some(restore) = window.restore.take() else {
            return false;
        };
        let old = window.rect;
        window.rect = restore;
        self.damage.push(old);
        self.damage.push(restore);
        true
    }

    /// Whether a window is filling the work area.
    pub fn is_maximised(&self, id: u32) -> bool {
        self.window(id)
            .is_some_and(|window| window.restore.is_some())
    }

    /// Hides a window in the dock. Focus falls to whatever is now in front.
    ///
    /// The window stays in the list rather than being removed: a minimised window is not
    /// closed, and the dock's tile and the shell's handle still point at it.
    pub fn minimise(&mut self, id: u32) -> bool {
        let Some(window) = self.window_mut(id) else {
            return false;
        };
        if window.minimised {
            return false;
        }
        window.minimised = true;
        let rect = window.rect;
        self.damage.push(rect);
        if self.focus == Some(id) {
            self.focus = self
                .windows
                .iter()
                .rev()
                .find(|w| !w.minimised)
                .map(|w| w.id);
        }
        true
    }

    /// Brings a minimised window back, in front, and focuses it.
    pub fn unminimise(&mut self, id: u32) -> bool {
        let Some(window) = self.window_mut(id) else {
            return false;
        };
        if !window.minimised {
            return false;
        }
        window.minimised = false;
        self.raise(id)
    }

    /// Whether a window is on the screen at all.
    pub fn visible(&self, id: u32) -> bool {
        self.window(id).is_some_and(|window| !window.minimised)
    }

    /// Closes a window, damaging where it was.
    pub fn close(&mut self, id: u32) -> bool {
        let Some(index) = self.windows.iter().position(|window| window.id == id) else {
            return false;
        };
        let window = self.windows.remove(index);
        self.damage.push(window.rect);
        // Focus falls to whatever is now in front, or to nothing.
        if self.focus == Some(id) {
            self.focus = self.windows.last().map(|window| window.id);
        }
        true
    }

    /// Makes a window the focused one if it exists, which is what a keyboard command aimed at a
    /// particular window uses.
    pub fn focus(&mut self, id: u32) -> bool {
        let Some(rect) = self.window(id).map(|window| window.rect) else {
            return false;
        };
        self.refocus(id);
        // Focus changes a window's title bar and border, so it repaints.
        self.damage.push(rect);
        true
    }

    /// Adds one rect to the damage list.
    pub fn damage(&mut self, rect: Rect) {
        self.damage.push(rect);
    }

    /// Takes the damage list, merged and clipped to the panel, and empties it.
    pub fn take_damage(&mut self) -> Vec<Rect> {
        let merged = merge(&self.damage, self.width, self.height);
        self.damage.clear();
        merged
    }

    /// How many rects are waiting, before merging. For the report.
    pub fn pending(&self) -> usize {
        self.damage.len()
    }
}

/// Whether two rects share any pixel. The same test the damage merge uses, exposed because
/// the boot log reports how many windows overlap.
pub fn overlaps(a: Rect, b: Rect) -> bool {
    intersects(a, b)
}

/// The union of two rects, which is what two overlapping damage areas become.
fn union(a: Rect, b: Rect) -> Rect {
    let left = a.x.min(b.x);
    let top = a.y.min(b.y);
    let right = (a.x + a.width as i32).max(b.x + b.width as i32);
    let bottom = (a.y + a.height as i32).max(b.y + b.height as i32);
    Rect::new(
        left,
        top,
        (right - left).max(0) as u32,
        (bottom - top).max(0) as u32,
    )
}

/// Whether two rects share any pixel.
fn intersects(a: Rect, b: Rect) -> bool {
    if a.width == 0 || a.height == 0 || b.width == 0 || b.height == 0 {
        return false;
    }
    a.x < b.x + b.width as i32
        && b.x < a.x + a.width as i32
        && a.y < b.y + b.height as i32
        && b.y < a.y + a.height as i32
}

/// Whether `(x, y)` is inside `rect`.
fn contains(rect: Rect, x: i32, y: i32) -> bool {
    x >= rect.x && x < rect.x + rect.width as i32 && y >= rect.y && y < rect.y + rect.height as i32
}

/// Whether `rect` covers `other` completely.
fn covers(rect: Rect, other: Rect) -> bool {
    other.x >= rect.x
        && other.y >= rect.y
        && other.x + other.width as i32 <= rect.x + rect.width as i32
        && other.y + other.height as i32 <= rect.y + rect.height as i32
}

/// Clips `rect` to the panel, or returns `None` if nothing of it is visible.
fn clip(rect: Rect, width: u32, height: u32) -> Option<Rect> {
    let (x, y, w, h) = rect.clip(width as usize, height as usize)?;
    Some(Rect::new(x as i32, y as i32, w as u32, h as u32))
}

/// Merges overlapping damage and drops anything the panel cannot show.
///
/// The loop terminates because every merge strictly reduces the number of rects and the pass
/// that changes nothing is the last one. A second pass drops a rect another covers, which a
/// merge cannot express: the big one already includes it.
pub fn merge(rects: &[Rect], width: u32, height: u32) -> Vec<Rect> {
    let mut merged: Vec<Rect> = Vec::new();
    for rect in rects {
        if let Some(visible) = clip(*rect, width, height) {
            merged.push(visible);
        }
    }
    loop {
        let mut changed = false;
        'outer: for first in 0..merged.len() {
            for second in (first + 1)..merged.len() {
                if intersects(merged[first], merged[second]) {
                    let joined = union(merged[first], merged[second]);
                    merged.remove(second);
                    merged[first] = joined;
                    changed = true;
                    break 'outer;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut result: Vec<Rect> = Vec::with_capacity(merged.len());
    for (index, rect) in merged.iter().enumerate() {
        let covered = merged
            .iter()
            .enumerate()
            .any(|(other, candidate)| other != index && covers(*candidate, *rect));
        if !covered {
            result.push(*rect);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn desktop() -> Windows {
        Windows::new(1280, 800, 36)
    }

    fn lines(text: &str) -> Vec<String> {
        vec![String::from(text)]
    }

    #[test]
    fn a_new_window_is_in_front_and_has_focus() {
        let mut windows = desktop();
        let first = windows.create("log", lines("one"), 400, 300);
        assert_eq!(windows.focused(), Some(first));
        let second = windows.create("about", lines("two"), 400, 300);
        assert_eq!(windows.focused(), Some(second));
        assert_eq!(windows.count(), 2);
        // Back to front: the first is behind the second.
        let order: Vec<u32> = windows.iter().map(|window| window.id).collect();
        assert_eq!(order, vec![first, second]);
    }

    #[test]
    fn a_window_given_widgets_keeps_only_the_widgets_and_damages_itself() {
        let mut windows = desktop();
        let id = windows.create("system", lines("one"), 400, 300);
        let _ = windows.take_damage();
        let widget = crate::ui::Widget {
            kind: crate::ui::Kind::Button,
            action: crate::ui::Action::Command(0),
            label: String::from("press"),
            rect: Rect::new(4, 4, 40, 20),
            state: crate::ui::State::Rest,
        };
        assert!(windows.set_widgets(id, vec![widget.clone()]));
        let (shown, lines_left, scroll, rect) = {
            let window = windows.window(id).expect("window");
            (
                window.widgets.clone(),
                window.lines.len(),
                window.scroll,
                window.rect,
            )
        };
        assert_eq!(shown, vec![widget]);
        // The text went with it: a window's content is one thing or the other, never both in
        // the same rectangle.
        assert_eq!(lines_left, 0);
        assert_eq!(scroll, 0);
        // The whole frame is damaged: the body changed and the title bar is repainted with it.
        assert_eq!(windows.take_damage(), vec![rect]);
        // A window that is not there answers rather than panicking.
        assert!(!windows.set_widgets(99, Vec::new()));
    }

    #[test]
    fn the_hit_test_finds_the_topmost_window_and_a_click_raises_it() {
        let mut windows = desktop();
        let back = windows.create("log", lines("one"), 400, 300);
        let front = windows.create("about", lines("two"), 400, 300);
        // The cascade puts the second window 36 pixels down and right of the first, so the
        // point below is inside both.
        let (x, y) = {
            let first = windows.window(back).expect("first").rect;
            (first.x + 200, first.y + 200)
        };
        assert_eq!(windows.hit(x, y), Some(front));
        assert_eq!(windows.click(x, y), Some(front));
        // Clicking the first window where the second does not cover it raises the first.
        let (corner_x, corner_y) = {
            let first = windows.window(back).expect("first").rect;
            (first.x + 5, first.y + 5)
        };
        assert_eq!(windows.hit(corner_x, corner_y), Some(back));
        assert_eq!(windows.click(corner_x, corner_y), Some(back));
        assert_eq!(windows.focused(), Some(back));
        assert_eq!(
            windows.iter().last().map(|window| window.id),
            Some(back),
            "the clicked window is not the front one"
        );
    }

    #[test]
    fn a_point_outside_every_window_hits_nothing() {
        let mut windows = desktop();
        let id = windows.create("log", lines("one"), 200, 100);
        assert_eq!(windows.hit(1279, 799), None);
        assert_eq!(windows.click(1279, 799), None);
        assert_eq!(windows.focused(), Some(id), "a miss changed the focus");
    }

    #[test]
    fn moving_a_window_damages_where_it_was_and_where_it_went() {
        let mut windows = desktop();
        let id = windows.create("log", lines("one"), 400, 300);
        let _ = windows.take_damage();
        let before = windows.window(id).expect("window").rect;
        assert!(windows.move_to(id, 600, 500));
        let after = windows.window(id).expect("window").rect;
        assert_ne!(before, after);
        // Two rects that do not overlap: the band between them has to be repaired as well as
        // the window itself, which is why both are damaged.
        let damage = windows.take_damage();
        assert_eq!(damage.len(), 2, "{damage:?}");
        assert!(damage.contains(&before) && damage.contains(&after));
        assert!(windows.take_damage().is_empty(), "damage survived a take");
    }

    #[test]
    fn a_window_dragged_past_the_edge_keeps_its_title_bar_on_the_panel() {
        let mut windows = desktop();
        let id = windows.create("log", lines("one"), 400, 300);
        assert!(windows.move_to(id, 5000, 5000));
        let rect = windows.window(id).expect("window").rect;
        assert!(rect.x + rect.width as i32 > 0);
        assert!(rect.y >= 0);
        assert!(windows.move_to(id, -5000, -5000));
        let rect = windows.window(id).expect("window").rect;
        assert!(
            rect.x < 0,
            "the window should be able to hang over the left edge"
        );
        assert!(rect.x + rect.width as i32 > 0, "but not past it");
    }

    #[test]
    fn closing_the_focused_window_falls_back_to_the_one_in_front() {
        let mut windows = desktop();
        let first = windows.create("log", lines("one"), 300, 200);
        let second = windows.create("about", lines("two"), 300, 200);
        assert!(windows.close(second));
        assert_eq!(windows.count(), 1);
        assert_eq!(windows.focused(), Some(first));
        assert!(windows.close(first));
        assert_eq!(windows.focused(), None);
        assert!(!windows.close(first));
    }

    #[test]
    fn a_cascaded_window_stays_on_a_small_panel() {
        let mut windows = Windows::new(320, 200, 36);
        for _ in 0..6 {
            windows.create("log", lines("one"), 400, 300);
        }
        for window in windows.iter() {
            assert_eq!(window.rect.width, 320);
            assert_eq!(window.rect.height, 200);
            assert_eq!(window.rect.x, 0);
            assert_eq!(window.rect.y, 0);
        }
    }

    #[test]
    fn a_line_pushed_into_a_window_moves_the_view_to_it() {
        let mut windows = desktop();
        let id = windows.create("log", vec![String::from("a"), String::from("b")], 300, 200);
        let window = windows.window_mut(id).expect("window");
        window.push_line(String::from("c"));
        assert_eq!(window.scroll, 2);
        let visible: Vec<&str> = window.visible_lines(10).map(String::as_str).collect();
        assert_eq!(visible, vec!["c"]);
        window.set_lines(vec![String::from("only")]);
        assert_eq!(window.scroll, 0);
    }

    #[test]
    fn overlapping_damage_merges_and_damage_off_the_panel_disappears() {
        // Two overlapping rects become their union.
        let merged = merge(
            &[Rect::new(10, 10, 50, 50), Rect::new(40, 40, 50, 50)],
            1280,
            800,
        );
        assert_eq!(merged, vec![Rect::new(10, 10, 80, 80)]);

        // Two that only touch at an edge stay separate: a shared boundary is not an overlap,
        // and merging them would repaint a band of the screen that did not change.
        let separate = merge(
            &[Rect::new(0, 0, 10, 10), Rect::new(10, 0, 10, 10)],
            1280,
            800,
        );
        assert_eq!(separate.len(), 2);

        // One inside another: the outer one is enough.
        let covered = merge(
            &[Rect::new(0, 0, 100, 100), Rect::new(10, 10, 20, 20)],
            1280,
            800,
        );
        assert_eq!(covered, vec![Rect::new(0, 0, 100, 100)]);

        // Nothing visible: nothing to repaint. Partly visible: the visible part only.
        assert!(merge(&[Rect::new(-500, -500, 100, 100)], 1280, 800).is_empty());
        assert!(merge(&[Rect::new(0, 0, 0, 0)], 1280, 800).is_empty());
        assert_eq!(
            merge(&[Rect::new(-50, -50, 100, 100)], 1280, 800),
            vec![Rect::new(0, 0, 50, 50)]
        );

        // A chain of three overlapping rects collapses to one.
        let chain = merge(
            &[
                Rect::new(0, 0, 20, 20),
                Rect::new(15, 0, 20, 20),
                Rect::new(30, 0, 20, 20),
            ],
            1280,
            800,
        );
        assert_eq!(chain, vec![Rect::new(0, 0, 50, 20)]);
    }
}
