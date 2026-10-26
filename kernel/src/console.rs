//! The character grid behind the boot log: cursor, wrapping and scrolling.
//!
//! Pure layout, no screen and no port I/O: [`Console::render`] draws the grid onto any
//! [`Canvas`], and the `x86_64`-only `display.rs` owns the instance the kernel writes
//! through. That split is what lets `cargo test --package kernel --lib` check that a line
//! wraps at the right column and that a full screen scrolls correctly, rather than
//! discovering it from a photograph of a panel.
//!
//! The grid is a flat `Vec<u8>` of printable ASCII, allocated once at boot: writing a
//! character never allocates, which is what lets the panic handler keep using the console
//! after something has gone wrong.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

use crate::canvas::{Canvas, Color, Rect};
use crate::text;

/// Columns a tab advances to: the classic terminal stop.
pub const TAB_WIDTH: usize = 8;

/// A fixed grid of characters with a cursor, wrapping and scrolling.
pub struct Console {
    columns: usize,
    rows: usize,
    /// `columns * rows` cells, row-major, `b' '` where nothing has been written.
    cells: Vec<u8>,
    /// Where the next character goes. May equal `columns`, meaning "the line is full and
    /// the next character wraps first", which is how a character in the last column can be
    /// written without an extra blank line appearing immediately.
    column: usize,
    row: usize,
    /// First row whose pixels may be out of date, or `rows` when the panel is current.
    ///
    /// Without this the display would repaint every glyph on every line, which turns a
    /// forty-line boot log into forty full-screen repaints - measurable on a 1280x800
    /// surface under emulation, and pointless when only one row changed.
    dirty_from: usize,
    /// Rows the grid has scrolled since the display last copied the pixels. A scroll is
    /// cheaper to paint as a pixel copy than as a redraw, so the display needs to know.
    scrolled: usize,
}

impl Console {
    /// A blank grid of `columns` x `rows` cells, or `None` if either is zero.
    ///
    /// Zero is refused rather than accepted as an empty console: it only happens when the
    /// panel is smaller than one character cell, and a caller that ignored it would print
    /// into nothing and report success.
    pub fn new(columns: usize, rows: usize) -> Option<Self> {
        if columns == 0 || rows == 0 {
            return None;
        }
        Some(Self {
            columns,
            rows,
            cells: vec![b' '; columns * rows],
            column: 0,
            row: 0,
            dirty_from: 0,
            scrolled: 0,
        })
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Cursor position as `(column, row)`, zero-based. `column` may equal `columns`.
    pub fn cursor(&self) -> (usize, usize) {
        (self.column, self.row)
    }

    /// The character at `column`, `row`, or `None` if that is off the grid.
    pub fn cell(&self, column: usize, row: usize) -> Option<char> {
        if column >= self.columns || row >= self.rows {
            return None;
        }
        self.cells
            .get(row * self.columns + column)
            .copied()
            .map(char::from)
    }

    /// The text of one row, trailing blanks trimmed. `None` if `row` is off the grid.
    ///
    /// Tests and diagnostics only; the renderer works from the cells.
    pub fn row_text(&self, row: usize) -> Option<String> {
        if row >= self.rows {
            return None;
        }
        let start = row * self.columns;
        let cells = &self.cells[start..start + self.columns];
        let end = cells
            .iter()
            .rposition(|cell| *cell != b' ')
            .map_or(0, |last| last + 1);
        Some(cells[..end].iter().map(|cell| char::from(*cell)).collect())
    }

    /// Empties the grid and puts the cursor back at the top left.
    pub fn clear(&mut self) {
        self.cells.fill(b' ');
        self.column = 0;
        self.row = 0;
        self.dirty_from = 0;
    }

    /// Appends `text`, applying the layout rules:
    ///
    /// * `\n` moves to the start of the next line, scrolling when already at the bottom;
    /// * `\r` returns to column 0 without moving down;
    /// * `\t` writes spaces up to the next [`TAB_WIDTH`] stop, so a tab that runs off the
    ///   end of a line wraps like any other text;
    /// * any other control character is ignored, since there is nothing useful to draw;
    /// * the rest of Unicode becomes `?`, because the font is ASCII and a visible guess
    ///   beats a silent gap.
    ///
    /// A character written in the last column leaves the cursor past the end, so the wrap
    /// happens before the *next* character: that is what keeps a full line from adding a
    /// blank line straight after it.
    pub fn write(&mut self, text: &str) {
        for ch in text.chars() {
            match ch {
                '\n' => self.line_feed(),
                '\r' => self.column = 0,
                '\t' => {
                    let spaces = TAB_WIDTH - self.column % TAB_WIDTH;
                    for _ in 0..spaces {
                        self.put(b' ');
                    }
                }
                ' '..='~' => self.put(ch as u8),
                ch if ch.is_control() => {}
                _ => self.put(b'?'),
            }
        }
    }

    /// Writes one printable byte at the cursor and advances it.
    fn put(&mut self, byte: u8) {
        if self.column >= self.columns {
            self.line_feed();
        }
        let index = self.row * self.columns + self.column;
        // The grid never changes size and `put` runs the wrap first, so this is in range;
        // going through `get_mut` keeps a future bug from becoming an out-of-bounds write.
        if let Some(cell) = self.cells.get_mut(index) {
            *cell = byte;
        }
        self.dirty_from = self.dirty_from.min(self.row);
        self.column += 1;
    }

    /// Drops the top row, moves everything up, and blanks the bottom row.
    fn scroll(&mut self) {
        let width = self.columns;
        self.cells.copy_within(width..width * self.rows, 0);
        self.cells[width * (self.rows - 1)..].fill(b' ');
        // Every row moved up by one, and the display will copy the pixels rather than
        // redraw them. What is left to paint is the row that moved into place and the blank
        // one below it - `dirty_from` is where the row that moved used to start - plus
        // anything that was already stale above it.
        self.dirty_from = self.dirty_from.saturating_sub(1);
        self.scrolled = self.scrolled.saturating_add(1);
    }

    /// Moves to column 0 of the next row, scrolling when the cursor is on the last one.
    fn line_feed(&mut self) {
        self.column = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.scroll();
        }
    }

    /// Draws the whole grid onto `canvas` with its top-left corner at `origin`.
    ///
    /// The grid area is repainted with `background` first, so text written over longer
    /// text does not leave the tail of the old line behind; empty cells then cost nothing
    /// beyond that repaint. Use this when the background itself changes - the boot screen
    /// and the panic screen - and [`Console::present`] for ordinary logging.
    pub fn render(
        &self,
        canvas: &mut Canvas<'_>,
        origin: (i32, i32),
        foreground: Color,
        background: Color,
        scale: usize,
    ) {
        self.paint_rows(0..self.rows, canvas, origin, foreground, background, scale);
    }

    /// First row that needs repainting, or [`Console::rows`] when the panel is current.
    pub fn dirty_from(&self) -> usize {
        self.dirty_from
    }

    /// Rows scrolled since the last call, and resets the count.
    ///
    /// The display copies the pixels up by that many rows instead of repainting them: the
    /// grid has already moved, so only the rows still marked dirty need drawing.
    pub fn take_scrolled_rows(&mut self) -> usize {
        core::mem::take(&mut self.scrolled)
    }

    /// Draws the rows that changed since the last call and marks the panel current.
    ///
    /// This is what keeps a boot log linear: appending a line repaints one row, not the
    /// forty above it. Only [`Console::scroll`] and [`Console::clear`] invalidate the whole
    /// grid, and a scroll is exactly when every row really did move.
    pub fn present(
        &mut self,
        canvas: &mut Canvas<'_>,
        origin: (i32, i32),
        foreground: Color,
        background: Color,
        scale: usize,
    ) {
        let first = self.dirty_from;
        self.dirty_from = self.rows;
        self.paint_rows(
            first..self.rows,
            canvas,
            origin,
            foreground,
            background,
            scale,
        );
    }

    /// Paints one band of rows: the background for the band, then its glyphs.
    fn paint_rows(
        &self,
        rows: Range<usize>,
        canvas: &mut Canvas<'_>,
        origin: (i32, i32),
        foreground: Color,
        background: Color,
        scale: usize,
    ) {
        let (cell_width, cell_height) = text::cell_size(scale);
        let (cell_width, cell_height) = (cell_width as i32, cell_height as i32);
        let (x, y) = origin;
        if !rows.is_empty() {
            canvas.fill_rect(
                Rect::new(
                    x,
                    y.saturating_add(rows.start as i32 * cell_height),
                    (self.columns as u64 * cell_width as u64).min(u64::from(u32::MAX)) as u32,
                    (rows.len() as u64 * cell_height as u64).min(u64::from(u32::MAX)) as u32,
                ),
                background,
            );
        }

        for row in rows {
            for column in 0..self.columns {
                let Some(cell) = self.cells.get(row * self.columns + column) else {
                    continue;
                };
                if *cell == b' ' {
                    continue;
                }
                text::draw_char(
                    canvas,
                    x.saturating_add(column as i32 * cell_width),
                    y.saturating_add(row as i32 * cell_height),
                    char::from(*cell),
                    foreground,
                    scale,
                );
            }
        }
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.write(text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{ChannelOrder, Geometry};

    const SURFACE_WIDTH: usize = 64;
    const SURFACE_HEIGHT: usize = 24;
    const FG: Color = Color::new(0xff, 0xff, 0xff);
    const BG: Color = Color::new(0x00, 0x00, 0x11);
    const BG_CHANNELS: [u8; 3] = [0x00, 0x00, 0x11];

    fn console() -> Console {
        Console::new(8, 3).expect("a non-empty grid")
    }

    fn blank() -> [u8; SURFACE_WIDTH * SURFACE_HEIGHT * 4] {
        [0u8; SURFACE_WIDTH * SURFACE_HEIGHT * 4]
    }

    fn canvas(pixels: &mut [u8]) -> Canvas<'_> {
        let geometry = Geometry::new(SURFACE_WIDTH, SURFACE_HEIGHT, SURFACE_WIDTH, 4);
        Canvas::new(pixels, geometry, ChannelOrder::Rgb).expect("geometry fits the buffer")
    }

    fn pixel(pixels: &[u8], x: usize, y: usize) -> [u8; 4] {
        let offset = (y * SURFACE_WIDTH + x) * 4;
        [
            pixels[offset],
            pixels[offset + 1],
            pixels[offset + 2],
            pixels[offset + 3],
        ]
    }

    #[test]
    fn refuses_a_grid_with_no_room() {
        assert!(Console::new(0, 3).is_none());
        assert!(Console::new(3, 0).is_none());
    }

    #[test]
    fn writes_left_to_right_and_leaves_the_cursor_after_the_text() {
        let mut console = console();
        console.write("ab");
        assert_eq!(console.row_text(0).as_deref(), Some("ab"));
        assert_eq!(console.cursor(), (2, 0));
    }

    #[test]
    fn wraps_before_the_character_after_a_full_line() {
        let mut console = console();
        console.write("12345678");
        // Eight characters exactly fill the row; the cursor sits past the end rather than
        // pushing the grid down a line, which is what stops the log double-spacing.
        assert_eq!(console.cursor(), (8, 0));
        console.write("9");
        assert_eq!(console.row_text(0).as_deref(), Some("12345678"));
        assert_eq!(console.cell(0, 1), Some('9'));
    }

    #[test]
    fn scrolls_a_full_screen_up_by_one_line() {
        let mut console = Console::new(4, 2).expect("a non-empty grid");
        console.write("abcdefghij");
        // Three lines of text went through a two-row grid: the first is gone, the rest
        // moved up, and the cursor is on the bottom row after the last character.
        assert_eq!(console.row_text(0).as_deref(), Some("efgh"));
        assert_eq!(console.row_text(1).as_deref(), Some("ij"));
        assert_eq!(console.cursor(), (2, 1));
    }

    #[test]
    fn a_single_row_grid_keeps_only_the_last_line() {
        let mut console = Console::new(4, 1).expect("a non-empty grid");
        console.write("abcdef");
        assert_eq!(console.row_text(0).as_deref(), Some("ef"));
    }

    #[test]
    fn carriage_return_overwrites_from_column_zero() {
        let mut console = console();
        console.write("ab\rX");
        assert_eq!(console.row_text(0).as_deref(), Some("Xb"));
        assert_eq!(console.cursor(), (1, 0));
    }

    #[test]
    fn newline_starts_the_next_line() {
        let mut console = console();
        console.write("ab\nc");
        assert_eq!(console.row_text(0).as_deref(), Some("ab"));
        assert_eq!(console.row_text(1).as_deref(), Some("c"));
        assert_eq!(console.cursor(), (1, 1));
    }

    #[test]
    fn tab_advances_to_the_next_stop() {
        let mut console = Console::new(12, 2).expect("a non-empty grid");
        console.write("a\tb");
        assert_eq!(console.cell(0, 0), Some('a'));
        assert_eq!(console.cell(1, 0), Some(' '));
        assert_eq!(console.cell(8, 0), Some('b'));
    }

    #[test]
    fn ignores_control_characters_other_than_the_ones_it_handles() {
        let mut console = console();
        console.write("a\u{1}\u{7f}b");
        assert_eq!(console.row_text(0).as_deref(), Some("ab"));
        assert_eq!(console.cursor(), (2, 0));
    }

    #[test]
    fn shows_non_ascii_as_a_question_mark() {
        let mut console = console();
        console.write("é");
        assert_eq!(console.cell(0, 0), Some('?'));
    }

    #[test]
    fn clear_empties_the_grid_and_homes_the_cursor() {
        let mut console = console();
        console.write("abc\ndef");
        console.clear();
        assert_eq!(console.row_text(0).as_deref(), Some(""));
        assert_eq!(console.row_text(1).as_deref(), Some(""));
        assert_eq!(console.cursor(), (0, 0));
    }

    #[test]
    fn accepts_format_arguments_through_the_write_trait() {
        let mut console = console();
        core::fmt::write(&mut console, format_args!("{}-{}", 1, 2)).expect("writing to a grid");
        assert_eq!(console.row_text(0).as_deref(), Some("1-2"));
    }

    #[test]
    fn offers_no_cell_outside_the_grid() {
        let console = console();
        assert_eq!(console.cell(8, 0), None);
        assert_eq!(console.cell(0, 3), None);
        assert_eq!(console.row_text(3), None);
    }

    #[test]
    fn renders_ink_on_the_background_and_repaints_over_old_text() {
        let mut console = console();
        console.write("F");
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            console.render(&mut surface, (0, 0), FG, BG, 1);
        }
        // 'F' has ink in its top-left pixel; the far corner of the grid is background.
        assert_eq!(pixel(&pixels, 0, 0), [0xff, 0xff, 0xff, u8::MAX]);
        assert_eq!(&pixel(&pixels, 63, 23)[..3], &BG_CHANNELS);

        // Clearing has to erase the glyph, which only happens because the grid area is
        // repainted instead of drawn over.
        console.clear();
        {
            let mut surface = canvas(&mut pixels);
            console.render(&mut surface, (0, 0), FG, BG, 1);
        }
        assert_eq!(&pixel(&pixels, 0, 0)[..3], &BG_CHANNELS);
    }

    #[test]
    fn renders_at_the_origin_it_is_given() {
        let mut console = Console::new(2, 1).expect("a non-empty grid");
        console.write("F");
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            console.render(&mut surface, (32, 8), FG, BG, 1);
        }
        assert_eq!(pixel(&pixels, 0, 0), [0, 0, 0, 0], "outside the grid");
        assert_eq!(pixel(&pixels, 32, 8), [0xff, 0xff, 0xff, u8::MAX]);
    }

    #[test]
    fn a_new_console_needs_painting_from_the_top() {
        assert_eq!(console().dirty_from(), 0);
    }

    #[test]
    fn present_marks_the_panel_current_and_only_later_rows_dirty() {
        let mut console = console();
        console.write("first\n");
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            console.present(&mut surface, (0, 0), FG, BG, 1);
        }
        assert_eq!(
            console.dirty_from(),
            console.rows(),
            "a painted console has nothing left to repaint"
        );

        console.write("second");
        assert_eq!(console.dirty_from(), 1, "only row 1 changed");
    }

    #[test]
    fn a_scroll_leaves_only_the_rows_that_moved_to_paint() {
        let mut console = Console::new(4, 3).expect("a non-empty grid");
        console.write("a\nb\nc");
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            console.present(&mut surface, (0, 0), FG, BG, 1);
        }
        // Fill the bottom row, then let one more character wrap: the grid scrolls, the
        // display copies the pixels up, and only the row that moved into place (plus the
        // blank one below it) has to be redrawn.
        console.write("defgh");
        assert_eq!(console.take_scrolled_rows(), 1);
        assert_eq!(
            console.take_scrolled_rows(),
            0,
            "the count is reported once"
        );
        assert_eq!(console.dirty_from(), 1);
    }

    #[test]
    fn present_repaints_nothing_when_the_panel_is_current() {
        let mut console = console();
        console.write("F");
        let mut pixels = blank();
        {
            let mut surface = canvas(&mut pixels);
            console.present(&mut surface, (0, 0), FG, BG, 1);
        }
        // Stand in for a panel that changed underneath us: if `present` repainted a clean
        // console, these bytes would be overwritten with the background colour.
        pixels.fill(0x7f);
        {
            let mut surface = canvas(&mut pixels);
            console.present(&mut surface, (0, 0), FG, BG, 1);
        }
        assert!(
            pixels.iter().all(|byte| *byte == 0x7f),
            "a clean console has nothing to paint"
        );
    }
}
