//! Back / forward navigation history — the jump list behind Alt+Left / Alt+Right.
//!
//! Every "go somewhere" action (go-to-definition, a search hit, goto-line, a symbol jump) records
//! where it left FROM and where it went TO, so you can retrace your steps like a browser's back
//! button. Ordinary caret motion (arrows, typing) is NOT recorded — only deliberate jumps — which
//! is what makes Back land on the handful of places you actually navigated to, not every keystroke.

use std::path::PathBuf;

/// A place the caret has been: a file and a byte offset within it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavPoint {
    pub path: PathBuf,
    pub byte: usize,
}

impl NavPoint {
    pub fn new(path: PathBuf, byte: usize) -> Self {
        Self { path, byte }
    }
}

/// A browser-style history: a linear list of visited points with a cursor at the current one.
/// Navigating truncates any forward entries (you took a new branch); Back/Forward slide the cursor.
#[derive(Default)]
pub struct NavHistory {
    entries: Vec<NavPoint>,
    /// Index of the CURRENT point in `entries`. Meaningless when `entries` is empty.
    cursor: usize,
}

/// Cap on retained jump points — plenty for real navigation, bounds memory on a long session.
const MAX_ENTRIES: usize = 100;

impl NavHistory {
    /// Record a jump from `from` to `to`. Ensures `from` is on the stack (so Back returns to where
    /// you were even for the FIRST jump), drops any forward history, then pushes `to`. A jump that
    /// does not actually move (same point), or a re-record of the point already current, is ignored
    /// so Back/Forward never stutters on a no-op.
    pub fn record(&mut self, from: NavPoint, to: NavPoint) {
        if from == to {
            return;
        }
        // Everything past the cursor is a branch we're abandoning.
        self.entries.truncate(self.cursor + 1);
        // Make sure the origin is the current entry, so Back has somewhere to return to.
        if self.entries.is_empty() {
            self.entries.push(from);
        } else if self.entries[self.cursor] != from {
            self.entries.push(from);
        }
        self.entries.push(to);
        self.cursor = self.entries.len() - 1;
        // Bound memory: drop oldest, keeping the cursor pointed at the same (now-shifted) entry.
        if self.entries.len() > MAX_ENTRIES {
            let overflow = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(0..overflow);
            self.cursor -= overflow;
        }
    }

    /// Step back one point (Alt+Left). `None` at the oldest entry.
    pub fn back(&mut self) -> Option<NavPoint> {
        if self.cursor == 0 || self.entries.is_empty() {
            return None;
        }
        self.cursor -= 1;
        self.entries.get(self.cursor).cloned()
    }

    /// Step forward one point (Alt+Right). `None` at the newest entry.
    pub fn forward(&mut self) -> Option<NavPoint> {
        if self.cursor + 1 >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        self.entries.get(self.cursor).cloned()
    }

    pub fn can_back(&self) -> bool {
        self.cursor > 0 && !self.entries.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.entries.len()
    }

    /// Every recorded point, MOST-RECENT FIRST, for the recent-locations popup (Ctrl+E).
    /// The current point leads; duplicates of the same (file, byte) are collapsed so the
    /// list reads as distinct places, not a raw jump log.
    pub fn recent(&self) -> Vec<NavPoint> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        // From the cursor outward: current first, then the rest newest-ish first.
        for np in self.entries.iter().rev() {
            if seen.insert((np.path.clone(), np.byte)) {
                out.push(np.clone());
            }
        }
        out
    }

    /// Drop history for files no longer relevant (project switch clears everything).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, byte: usize) -> NavPoint {
        NavPoint::new(PathBuf::from(name), byte)
    }

    #[test]
    fn first_jump_records_origin_so_back_returns() {
        let mut h = NavHistory::default();
        // Jump a.rs:0 -> b.rs:10.
        h.record(p("a.rs", 0), p("b.rs", 10));
        assert!(h.can_back());
        assert!(!h.can_forward());
        assert_eq!(h.back(), Some(p("a.rs", 0)), "Back returns to the origin");
        assert!(h.can_forward());
        assert_eq!(h.forward(), Some(p("b.rs", 10)), "Forward returns to the target");
        assert!(!h.can_forward());
    }

    #[test]
    fn a_new_jump_from_a_back_position_truncates_forward() {
        let mut h = NavHistory::default();
        h.record(p("a.rs", 0), p("b.rs", 1));
        h.record(p("b.rs", 1), p("c.rs", 2));
        // entries: [a, b, c], cursor=2. Go back to b.
        assert_eq!(h.back(), Some(p("b.rs", 1)));
        // Now jump somewhere NEW from b — the forward branch (c) is gone.
        h.record(p("b.rs", 1), p("d.rs", 3));
        assert!(!h.can_forward(), "the c branch was abandoned");
        assert_eq!(h.back(), Some(p("b.rs", 1)));
        assert_eq!(h.back(), Some(p("a.rs", 0)));
        assert!(!h.can_back());
    }

    #[test]
    fn no_op_jumps_are_ignored() {
        let mut h = NavHistory::default();
        h.record(p("a.rs", 5), p("a.rs", 5)); // same point
        assert!(!h.can_back());
        h.record(p("a.rs", 0), p("b.rs", 1));
        // Re-recording with the same origin as current does not duplicate it.
        let before = (h.can_back(), h.can_forward());
        h.record(p("b.rs", 1), p("c.rs", 2));
        assert_eq!(h.back(), Some(p("b.rs", 1)));
        assert_eq!(h.back(), Some(p("a.rs", 0)));
        assert_eq!(before, (true, false));
    }

    #[test]
    fn history_is_bounded() {
        let mut h = NavHistory::default();
        for i in 0..(MAX_ENTRIES + 50) {
            h.record(p("f.rs", i), p("f.rs", i + 1));
        }
        // Walk all the way back — never panics, count is capped.
        let mut steps = 0;
        while h.back().is_some() {
            steps += 1;
            assert!(steps <= MAX_ENTRIES, "history did not stay bounded");
        }
    }

    #[test]
    fn recent_is_newest_first_and_deduped() {
        let mut h = NavHistory::default();
        h.record(p("a.rs", 0), p("b.rs", 1));
        h.record(p("b.rs", 1), p("c.rs", 2));
        h.record(p("c.rs", 2), p("a.rs", 0)); // revisit a.rs:0
        let r = h.recent();
        // Newest (a.rs:0) first; the earlier a.rs:0 origin is collapsed.
        assert_eq!(r.first(), Some(&p("a.rs", 0)));
        assert_eq!(r.iter().filter(|n| **n == p("a.rs", 0)).count(), 1);
        assert!(r.contains(&p("b.rs", 1)) && r.contains(&p("c.rs", 2)));
    }

    #[test]
    fn clear_resets() {
        let mut h = NavHistory::default();
        h.record(p("a.rs", 0), p("b.rs", 1));
        h.clear();
        assert!(!h.can_back() && !h.can_forward());
    }
}
