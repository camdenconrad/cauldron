//! term.rs — the terminal model: Alacritty's grid + VTE parser, driven by PTY bytes.
//!
//! [`Terminal`] wraps [`alacritty_terminal::Term`] (the grid, scrollback and terminal-mode state)
//! and the VTE [`Processor`] (the escape-sequence parser). PTY output goes in through
//! [`Terminal::feed`]; the Term emits [`Event`]s (write-back, title, clipboard…) which the parser's
//! handler forwards over a channel for the session to act on. Rendering and input read the Term
//! through the thin accessors here so `app.rs` never touches Alacritty internals directly.

use std::sync::mpsc::{self, Receiver, Sender};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{Config, RenderableContent, Term, TermMode, MIN_COLUMNS};
use alacritty_terminal::vte::ansi::Processor;

/// A terminal grid size in cells. Our own [`Dimensions`] impl (Alacritty only ships one behind its
/// `test` module); `total_lines == screen_lines` because the scrollback capacity is configured
/// separately via [`Config::scrolling_history`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: usize,
    pub lines: usize,
}

impl GridSize {
    fn sane(cols: usize, lines: usize) -> Self {
        Self { cols: cols.max(MIN_COLUMNS), lines: lines.max(1) }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The [`EventListener`] Alacritty calls for terminal events; it forwards each one over a channel.
/// The Term and this proxy live on the same UI thread, but the channel gives tidy interior
/// mutability behind the trait's `&self`.
#[derive(Clone)]
pub struct EventProxy(Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

/// One terminal: grid + parser + the queue of events the grid has emitted.
pub struct Terminal {
    term: Term<EventProxy>,
    parser: Processor,
    events: Receiver<Event>,
    pub size: GridSize,
}

impl Terminal {
    pub fn new(cols: usize, lines: usize, scrollback: usize) -> Self {
        let (tx, events) = mpsc::channel();
        let config = Config { scrolling_history: scrollback, ..Config::default() };
        let size = GridSize::sane(cols, lines);
        let term = Term::new(config, &size, EventProxy(tx));
        Self { term, parser: Processor::new(), events, size }
    }

    /// Feed a chunk of PTY output through the parser into the grid. vte 0.13's `advance` is
    /// byte-wise, so we drive it one byte at a time.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.parser.advance(&mut self.term, b);
        }
    }

    /// Drain the next queued terminal event (title change, write-back, clipboard store…).
    pub fn next_event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    pub fn resize(&mut self, cols: usize, lines: usize) {
        let size = GridSize::sane(cols, lines);
        if size == self.size {
            return;
        }
        self.size = size;
        self.term.resize(size);
    }

    /// The current terminal mode flags (app-cursor, bracketed-paste, alt-screen…).
    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    // --- rendering -------------------------------------------------------------------------------

    /// A snapshot of everything needed to draw the visible screen (borrows the grid).
    pub fn renderable(&self) -> RenderableContent<'_> {
        self.term.renderable_content()
    }

    /// The character currently in the cell at `point` (used to redraw the glyph under a block
    /// cursor in the background colour).
    pub fn char_at(&self, point: Point) -> char {
        self.term.grid()[point].c
    }

    // --- scrollback ------------------------------------------------------------------------------

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn scroll_delta(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_page(&mut self, up: bool) {
        self.term.scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    // --- selection -------------------------------------------------------------------------------

    pub fn begin_selection(&mut self, point: Point, side: Side) {
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, side));
    }

    pub fn update_selection(&mut self, point: Point, side: Side) {
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// The current selection as text, or None when there is no (non-empty) selection.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// Select the whole buffer — scrollback top through the last screen row — for the "Select All"
    /// menu action. `Term` implements `Dimensions`, so the bounds come straight off it.
    pub fn select_all(&mut self) {
        let cols = self.term.columns();
        let screen = self.term.screen_lines();
        let history = self.term.total_lines().saturating_sub(screen);
        let start = Point::new(Line(-(history as i32)), Column(0));
        let end = Point::new(Line(screen as i32 - 1), Column(cols.saturating_sub(1)));
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        self.term.selection = Some(sel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copying a selection must join soft-wrapped display lines (the WRAPLINE flag alacritty sets
    /// when text runs past the terminal width) into ONE logical line, while a real `\n` stays a
    /// newline — a wrapped shell command pastes back as a single line. This pins that the copy
    /// path goes through `selection_to_string`, which honours the flag.
    #[test]
    fn copy_joins_soft_wraps_but_keeps_hard_newlines() {
        let mut t = Terminal::new(10, 5, 100);
        // 15 chars on a 10-column grid → soft wrap after "1234567890"; then a hard newline.
        t.feed(b"1234567890ABCDE\r\nxyz");
        t.select_all();
        let text = t.selection_text().expect("select_all over content yields text");
        let text = text.trim_end_matches('\n');
        assert_eq!(text, "1234567890ABCDE\nxyz");
    }

    /// select_all reaches from the top of scrollback history to the last used cell.
    #[test]
    fn select_all_spans_history() {
        let mut t = Terminal::new(10, 3, 100);
        // 6 hard lines on a 3-row screen → 3 lines pushed into history.
        t.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");
        t.select_all();
        let text = t.selection_text().expect("selection text");
        assert!(text.starts_with("one"), "must include the oldest history line: {text:?}");
        assert!(text.contains("six"), "must include the last screen line: {text:?}");
    }
}
