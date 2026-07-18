//! Boot-timing instrumentation, opt-in via `CAULDRON_BOOT_TRACE=1`.
//!
//! Lands FIRST in the boot-time wave so every later fix is measurable. When the env var is
//! unset this whole module is a no-op behind one relaxed [`AtomicBool`] load — no formatting,
//! no locking, no allocation on the hot path.
//!
//! Lines go to STDERR in the shape `[boot +{ms:>7.1}ms] {label}`, timed from [`init`] at the
//! top of `main()`. Counters (entries visited, bytes read, subprocess spawns) matter as much
//! as times: workers batch into [`count`] and marks report deltas.
//!
//! Frame bookkeeping: `update()` takes a [`frame_guard`] per frame — frame 1 emits
//! `first-update-entry`/`first-update-exit` (the exit records the first-frame time), frame 2
//! emits `second-update-entry` (~first pixel on Wayland) plus the one-line boot summary.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Formatted marks without paying `format!` when the trace is off:
/// `boot_mark!("usage-scan-done files={} bytes={}", files, bytes)`.
macro_rules! boot_mark {
    ($($arg:tt)*) => {
        if crate::boot_trace::enabled() {
            crate::boot_trace::mark(&format!($($arg)*));
        }
    };
}
pub(crate) use boot_mark;

/// Phase keys the boot summary reports (order = display order). `end`/`span` calls with these
/// keys feed the `fonts= workspace= runconfig= session=` fields; everything else is "other".
const SUMMARY_PHASES: [(&str, &str); 4] = [
    ("fonts", "fonts"),
    ("workspace-open", "workspace"),
    ("runconfig-load", "runconfig"),
    ("restore-session", "session"),
];

/// All mutable trace state, instantiable so unit tests never race the process global.
struct Trace {
    enabled: AtomicBool,
    t0: Mutex<Option<Instant>>,
    /// (phase key, duration ms) — recorded by `end`; read by the summary.
    phases: Mutex<Vec<(&'static str, f64)>>,
    counters: Mutex<BTreeMap<&'static str, u64>>,
    frames: AtomicU32,
    first_frame_ms: Mutex<Option<f64>>,
}

impl Trace {
    const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            t0: Mutex::new(None),
            phases: Mutex::new(Vec::new()),
            counters: Mutex::new(BTreeMap::new()),
            frames: AtomicU32::new(0),
            first_frame_ms: Mutex::new(None),
        }
    }

    fn init(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        *lock(&self.t0) = Some(Instant::now());
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// ms since `init` — 0.0 if `init` was never called (defensive; marks stay well-formed).
    fn elapsed_ms(&self) -> f64 {
        lock(&self.t0).map(|t| t.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0)
    }

    fn mark_line(&self, label: &str) -> Option<String> {
        self.enabled().then(|| format_mark_line(self.elapsed_ms(), label))
    }

    fn begin(&self) -> Option<Instant> {
        self.enabled().then(Instant::now)
    }

    /// Close a span opened by `begin`: record the phase duration and return the
    /// `{phase}-done (…)` mark line. `extra` carries counters ("tabs=3 git-subprocesses=3").
    fn end_line(&self, start: Option<Instant>, phase: &'static str, extra: &str) -> Option<String> {
        let start = start?;
        if !self.enabled() {
            return None;
        }
        let dur_ms = start.elapsed().as_secs_f64() * 1000.0;
        lock(&self.phases).push((phase, dur_ms));
        Some(format_mark_line(self.elapsed_ms(), &format_done(phase, dur_ms, extra)))
    }

    fn count(&self, key: &'static str, n: u64) {
        if self.enabled() {
            *lock(&self.counters).entry(key).or_insert(0) += n;
        }
    }

    fn counter(&self, key: &'static str) -> u64 {
        lock(&self.counters).get(key).copied().unwrap_or(0)
    }

    /// Per-frame bookkeeping: returns (lines to print now, emit `first-update-exit` on drop).
    fn frame_entry(&self) -> (Vec<String>, bool) {
        if !self.enabled() {
            return (Vec::new(), false);
        }
        match self.frames.fetch_add(1, Ordering::Relaxed) + 1 {
            1 => (self.mark_line("first-update-entry").into_iter().collect(), true),
            2 => {
                let mut lines: Vec<String> =
                    self.mark_line("second-update-entry").into_iter().collect();
                lines.extend(self.mark_line(&self.summary()));
                (lines, false)
            }
            _ => (Vec::new(), false),
        }
    }

    /// First frame finished painting: the boot "first-frame" figure the summary reports.
    fn frame_exit(&self) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        let ms = self.elapsed_ms();
        *lock(&self.first_frame_ms) = Some(ms);
        self.mark_line("first-update-exit")
    }

    fn summary(&self) -> String {
        // Fallback: killed before the first frame exit was seen — use "now".
        let first_frame = lock(&self.first_frame_ms).unwrap_or_else(|| self.elapsed_ms());
        let phases = lock(&self.phases);
        let ms_for = |key: &str| -> f64 {
            phases.iter().filter(|(k, _)| *k == key).map(|(_, ms)| ms).sum()
        };
        let named: Vec<(&str, f64)> =
            SUMMARY_PHASES.iter().map(|(key, show)| (*show, ms_for(key))).collect();
        format_summary(first_frame, &named)
    }
}

/// Never poison-panic inside instrumentation: take the inner value either way.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---- pure formatting (unit-tested) -----------------------------------------------------------

/// `[boot +{ms:>7.1}ms] {label}`
fn format_mark_line(ms: f64, label: &str) -> String {
    format!("[boot +{ms:>7.1}ms] {label}")
}

/// `{phase}-done (12.3ms)` / `{phase}-done (12.3ms, tabs=3 git-subprocesses=3)`
fn format_done(phase: &str, dur_ms: f64, extra: &str) -> String {
    if extra.is_empty() {
        format!("{phase}-done ({dur_ms:.1}ms)")
    } else {
        format!("{phase}-done ({dur_ms:.1}ms, {extra})")
    }
}

/// `boot: first-frame {X}ms (fonts= workspace= runconfig= session= other=)`.
/// `other` = first-frame minus the named phases, clamped at 0 (background spans can overlap).
fn format_summary(first_frame_ms: f64, named: &[(&str, f64)]) -> String {
    let accounted: f64 = named.iter().map(|(_, ms)| ms).sum();
    let other = (first_frame_ms - accounted).max(0.0);
    let mut parts: Vec<String> =
        named.iter().map(|(name, ms)| format!("{name}={ms:.1}ms")).collect();
    parts.push(format!("other={other:.1}ms"));
    format!("boot: first-frame {first_frame_ms:.1}ms ({})", parts.join(" "))
}

/// `CAULDRON_BOOT_TRACE` truthiness: any non-empty value except "0" enables the trace.
fn parse_enabled(v: Option<std::ffi::OsString>) -> bool {
    v.is_some_and(|v| !v.is_empty() && v != *"0")
}

// ---- process-global API ------------------------------------------------------------------------

static GLOBAL: Trace = Trace::new();

/// Call ONCE at the very top of `main()`: captures t0 and reads `CAULDRON_BOOT_TRACE`.
pub fn init() {
    GLOBAL.init(parse_enabled(std::env::var_os("CAULDRON_BOOT_TRACE")));
}

/// Cheap check for call sites that want to skip building counter strings when off.
pub fn enabled() -> bool {
    GLOBAL.enabled()
}

/// Print `[boot +{ms}ms] {label}` to stderr (no-op when disabled).
pub fn mark(label: &str) {
    if let Some(line) = GLOBAL.mark_line(label) {
        eprintln!("{line}");
    }
}

/// Open a span; pair with [`end`]. `None` when disabled (making `end` a no-op too).
pub fn begin() -> Option<Instant> {
    GLOBAL.begin()
}

/// Close a span: records `phase` for the boot summary and prints `{phase}-done (…, extra)`.
pub fn end(start: Option<Instant>, phase: &'static str, extra: &str) {
    if let Some(line) = GLOBAL.end_line(start, phase, extra) {
        eprintln!("{line}");
    }
}

/// Time a closure as a summary phase: prints `{phase}-done ({ms}ms)`, returns the value.
pub fn span<T>(phase: &'static str, f: impl FnOnce() -> T) -> T {
    let t = begin();
    let out = f();
    end(t, phase, "");
    out
}

/// Bump a named counter (entries walked, bytes read, subprocess spawns). Batch per call site
/// where possible; marks report deltas via [`counter`]. No-op when disabled.
pub fn count(key: &'static str, n: u64) {
    GLOBAL.count(key, n);
}

/// Current value of a named counter (0 when disabled or never bumped).
pub fn counter(key: &'static str) -> u64 {
    GLOBAL.counter(key)
}

/// RAII frame bookkeeping for `App::update` — take one at the top of every frame; the guard's
/// Drop emits `first-update-exit` on any return path of frame 1.
pub struct FrameGuard {
    emit_exit: bool,
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        if self.emit_exit {
            if let Some(line) = GLOBAL.frame_exit() {
                eprintln!("{line}");
            }
        }
    }
}

pub fn frame_guard() -> FrameGuard {
    let (lines, emit_exit) = GLOBAL.frame_entry();
    for line in lines {
        eprintln!("{line}");
    }
    FrameGuard { emit_exit }
}

// -------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn parse_enabled_truth_table() {
        assert!(!parse_enabled(None));
        assert!(!parse_enabled(Some(OsString::from(""))));
        assert!(!parse_enabled(Some(OsString::from("0"))));
        assert!(parse_enabled(Some(OsString::from("1"))));
        assert!(parse_enabled(Some(OsString::from("true"))));
    }

    #[test]
    fn mark_line_format_right_aligned_ms() {
        assert_eq!(format_mark_line(3.14159, "main-entry"), "[boot +    3.1ms] main-entry");
        assert_eq!(format_mark_line(0.0, "x"), "[boot +    0.0ms] x");
        // Wider-than-field values must not truncate.
        assert_eq!(
            format_mark_line(123456.78, "late"),
            "[boot +123456.8ms] late"
        );
    }

    #[test]
    fn done_format_with_and_without_extra() {
        assert_eq!(format_done("fonts", 42.06, ""), "fonts-done (42.1ms)");
        assert_eq!(
            format_done("restore-session", 7.0, "tabs=3 git-subprocesses=3"),
            "restore-session-done (7.0ms, tabs=3 git-subprocesses=3)"
        );
    }

    #[test]
    fn summary_format_and_other_derivation() {
        let named = [("fonts", 40.0), ("workspace", 8.25), ("runconfig", 30.0), ("session", 20.0)];
        assert_eq!(
            format_summary(120.0, &named),
            "boot: first-frame 120.0ms (fonts=40.0ms workspace=8.2ms runconfig=30.0ms \
             session=20.0ms other=21.8ms)"
        );
    }

    #[test]
    fn summary_other_clamps_at_zero() {
        // Background spans can overlap the frame; "other" must never go negative.
        let named = [("fonts", 100.0), ("workspace", 100.0)];
        let s = format_summary(50.0, &named);
        assert!(s.ends_with("other=0.0ms)"), "{s}");
    }

    #[test]
    fn disabled_trace_is_a_no_op() {
        let t = Trace::new(); // never init'd/enabled
        assert!(!t.enabled());
        assert!(t.mark_line("anything").is_none());
        assert!(t.begin().is_none());
        assert!(t.end_line(t.begin(), "phase", "").is_none());
        // A stale Some(start) from an enabled window still stays silent once disabled.
        assert!(t.end_line(Some(Instant::now()), "phase", "").is_none());
        t.count("k", 5);
        assert_eq!(t.counter("k"), 0);
        let (lines, emit_exit) = t.frame_entry();
        assert!(lines.is_empty() && !emit_exit);
        assert!(t.frame_exit().is_none());
        assert!(lock(&t.phases).is_empty());
    }

    #[test]
    fn disabled_span_still_returns_value() {
        // The public span() wraps the (disabled-by-default) global: closure must run,
        // value must pass through, nothing recorded.
        let before = counter("test-span-counter");
        let v = span("test-span-phase", || 41 + 1);
        assert_eq!(v, 42);
        assert_eq!(counter("test-span-counter"), before);
    }

    #[test]
    fn enabled_trace_marks_counts_and_records_phases() {
        let t = Trace::new();
        t.init(true);
        let line = t.mark_line("main-entry").expect("enabled → line");
        assert!(line.starts_with("[boot +"), "{line}");
        assert!(line.ends_with("ms] main-entry"), "{line}");

        t.count("git-subprocess", 2);
        t.count("git-subprocess", 1);
        assert_eq!(t.counter("git-subprocess"), 3);
        assert_eq!(t.counter("never-bumped"), 0);

        let s = t.begin();
        assert!(s.is_some());
        let done = t.end_line(s, "restore-session", "tabs=2 git-subprocesses=3").unwrap();
        assert!(done.contains("restore-session-done ("), "{done}");
        assert!(done.ends_with("ms, tabs=2 git-subprocesses=3)"), "{done}");
        assert_eq!(lock(&t.phases).len(), 1);
        assert_eq!(lock(&t.phases)[0].0, "restore-session");
    }

    #[test]
    fn frame_sequence_first_exit_then_second_summary() {
        let t = Trace::new();
        t.init(true);

        let (lines, emit_exit) = t.frame_entry();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("first-update-entry"), "{}", lines[0]);
        assert!(emit_exit);

        let exit = t.frame_exit().expect("first frame exit line");
        assert!(exit.ends_with("first-update-exit"), "{exit}");
        assert!(lock(&t.first_frame_ms).is_some());

        let (lines, emit_exit) = t.frame_entry();
        assert!(!emit_exit);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("second-update-entry"), "{}", lines[0]);
        assert!(lines[1].contains("boot: first-frame "), "{}", lines[1]);
        // All four named phases + other appear even when never recorded (0.0ms).
        for field in ["fonts=", "workspace=", "runconfig=", "session=", "other="] {
            assert!(lines[1].contains(field), "missing {field} in {}", lines[1]);
        }

        // Frames 3+ are silent.
        let (lines, emit_exit) = t.frame_entry();
        assert!(lines.is_empty() && !emit_exit);
    }

    #[test]
    fn summary_uses_recorded_phase_durations() {
        let t = Trace::new();
        t.init(true);
        lock(&t.phases).push(("fonts", 40.0));
        lock(&t.phases).push(("workspace-open", 10.0));
        lock(&t.phases).push(("runconfig-load", 5.0));
        lock(&t.phases).push(("restore-session", 20.0));
        *lock(&t.first_frame_ms) = Some(100.0);
        assert_eq!(
            t.summary(),
            "boot: first-frame 100.0ms (fonts=40.0ms workspace=10.0ms runconfig=5.0ms \
             session=20.0ms other=25.0ms)"
        );
    }
}
