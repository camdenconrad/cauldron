//! Claude-usage meter for the status bar: token totals over the two windows
//! that matter for Claude plan limits — the rolling 5-hour session window and
//! the rolling 7-day window.
//!
//! # Refresh model (boot-wave item 5, recon B1)
//!
//! Every 120s cycle runs API-FIRST: the OAuth usage endpoint (the same numbers
//! Claude Code's `/usage` shows) is fetched first, and on success the local
//! transcript scan is SKIPPED ENTIRELY — the API result already wins at
//! display time, so re-reading ~400-495MB of JSONL for a number nobody sees
//! was pure waste.
//!
//! The offline fallback is INCREMENTAL: a per-file cache keyed by
//! `(path, mtime, size)` stores each transcript's deduplicated in-window turn
//! records, persisted at `~/.local/state/cauldron/usage-cache.json`. An
//! unchanged file is never re-read; a grown file is tail-read from the last
//! complete line; a shrunk (or otherwise rewritten) file is invalidated and
//! rescanned from byte 0. Steady-state rescans therefore touch only the
//! actively-growing transcripts.
//!
//! The FIRST kick is delayed ~5s after meter construction so the cold
//! fallback read never competes with LSP / index cold-start I/O at boot; the
//! 120s cadence is unchanged after that.
//!
//! # Data source
//!
//! Claude Code transcripts: `~/.claude/projects/<project-dir>/*.jsonl`, one
//! JSON object per line.
//!
//! # Observed transcript schema (verified empirically on this box, 2026-07-12,
//! across multiple project dirs)
//!
//! ```json
//! {
//!   "type": "assistant",
//!   "timestamp": "2026-07-12T06:31:40.586Z",
//!   "message": {
//!     "id": "msg_011CcwckUySerZqvxfaArDxj",
//!     "role": "assistant",
//!     "model": "claude-opus-4-8",
//!     "usage": {
//!       "input_tokens": 2,
//!       "output_tokens": 349,
//!       "cache_creation_input_tokens": 14405,
//!       "cache_read_input_tokens": 19228,
//!       "...": "extra fields (service_tier, cache_creation breakdown, iterations, …) ignored"
//!     }
//!   }
//! }
//! ```
//!
//! Key facts the implementation depends on:
//!
//! * Token usage nests under `message.usage`. There is **no** top-level
//!   `usage` and no populated `costUSD` on these entries.
//! * `timestamp` is ISO 8601 UTC with millisecond precision and a `Z` suffix.
//! * **One API turn is emitted as multiple JSONL lines** — one line per
//!   content block — each repeating the *same* `message.id` and the *same*
//!   usage object (verified identical within every id group). Summing lines
//!   naively roughly double-counts; we deduplicate by `message.id` globally
//!   across all files (this also collapses forked-session copies). Cached
//!   turns carry a stable 64-bit FNV-1a hash of the id; deduplication happens
//!   at aggregation time so tail-read boundaries and forked copies collapse
//!   identically to a full rescan.
//! * Transcripts are APPEND-ONLY in practice: a size increase is treated as
//!   an append (tail read); a size decrease always invalidates.
//! * Only `type == "assistant"` entries carry usage; `user`, `ai-title`,
//!   `attachment`, `file-history-snapshot`, `mode`, … do not.
//!
//! # Integration
//!
//! ```ignore
//! // main.rs:            mod usage;
//! // app struct:         usage_meter: usage::UsageMeter,   // usage::UsageMeter::new()
//! // status bar (each frame, bottom right):
//! let _ = self.usage_meter.poll();                  // non-blocking; kicks rescans
//! if let Some(line) = self.usage_meter.status_line() {
//!     ui.label(line);                               // "claude 5h 12% · 7d 34%"
//! }
//! ```

// The module is not wired into the status bar yet; drop this once the
// integrator adds the `poll()`/`status_line()` calls.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How long a computed snapshot stays fresh before `poll` kicks a rescan.
const REFRESH_INTERVAL: Duration = Duration::from_secs(120);
/// Boot shield: the FIRST refresh waits this long after meter construction so
/// the cold offline fallback (a ~400MB transcript read on this box) never
/// competes with LSP / index cold-start I/O. Subsequent refreshes follow the
/// normal [`REFRESH_INTERVAL`] cadence.
const FIRST_KICK_DELAY: Duration = Duration::from_secs(5);
/// The 5-hour session window, in seconds.
const WINDOW_5H_SECS: i64 = 5 * 3600;
/// The 7-day window, in seconds.
const WINDOW_7D_SECS: i64 = 7 * 86_400;
/// Directory-walk depth bound below the scan root (projects/<dir>/*.jsonl is
/// depth 2; the slack tolerates nested layouts without unbounded recursion).
const MAX_WALK_DEPTH: usize = 4;
/// Hard per-file line bound so a pathological transcript cannot spin forever.
const MAX_LINES_PER_FILE: u64 = 5_000_000;
/// On-disk cache schema version; any mismatch discards the cache (cheap: one
/// full rescan rebuilds it).
const CACHE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Totals
// ---------------------------------------------------------------------------

/// Token totals for one time window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UsageTotals {
    pub output_tokens: u64,
    pub input_tokens: u64,
    /// `cache_read_input_tokens` — cheap, kept separate from `total_ish`.
    pub cache_read: u64,
    /// `cache_creation_input_tokens`.
    pub cache_write: u64,
    /// Deduplicated assistant API turns.
    pub turns: u64,
}

impl UsageTotals {
    /// The tokens that "count" roughly toward plan limits:
    /// output + input + cache writes. Cache reads are cheap and excluded.
    pub fn total_ish(&self) -> u64 {
        self.output_tokens
            .saturating_add(self.input_tokens)
            .saturating_add(self.cache_write)
    }

    fn add(&mut self, u: &TurnRec) {
        self.output_tokens = self.output_tokens.saturating_add(u.output);
        self.input_tokens = self.input_tokens.saturating_add(u.input);
        self.cache_read = self.cache_read.saturating_add(u.cache_read);
        self.cache_write = self.cache_write.saturating_add(u.cache_write);
        self.turns = self.turns.saturating_add(1);
    }
}

/// Totals for the two windows that matter for Claude plan limits.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UsageWindows {
    /// Rolling 5-hour session window.
    pub h5: UsageTotals,
    /// Rolling 7-day window.
    pub d7: UsageTotals,
}

// ---------------------------------------------------------------------------
// Per-file incremental cache
// ---------------------------------------------------------------------------

/// One deduplicated assistant turn as cached: timestamp + usage + a stable
/// hash of the message id (aggregation-time dedup key). Field names are
/// single letters on disk — the cache holds thousands of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct TurnRec {
    /// Unix seconds (UTC) of the turn's `timestamp`.
    #[serde(rename = "t")]
    ts: i64,
    #[serde(rename = "i")]
    input: u64,
    #[serde(rename = "o")]
    output: u64,
    #[serde(rename = "cr")]
    cache_read: u64,
    #[serde(rename = "cw")]
    cache_write: u64,
    /// FNV-1a 64 of `message.id`; `None` for id-less entries (always counted).
    #[serde(rename = "h", default, skip_serializing_if = "Option::is_none")]
    id_hash: Option<u64>,
}

/// Cached scan state for one transcript file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FileEntry {
    /// mtime as nanoseconds since the epoch (0 when unreadable/pre-epoch).
    #[serde(rename = "m")]
    mtime_ns: u64,
    /// File size at scan time.
    #[serde(rename = "s")]
    size: u64,
    /// Bytes covered by COMPLETE (newline-terminated) parsed lines; a tail
    /// read resumes here so a partially-flushed last line is re-read whole.
    #[serde(rename = "p")]
    prefix: u64,
    /// In-window turns extracted from this file (per-file id-deduped).
    #[serde(rename = "u")]
    turns: Vec<TurnRec>,
}

/// The whole per-file cache, persisted as JSON at
/// `~/.local/state/cauldron/usage-cache.json` (or `$XDG_STATE_HOME`).
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct UsageCache {
    v: u32,
    files: HashMap<String, FileEntry>,
}

impl Default for UsageCache {
    fn default() -> Self {
        Self { v: CACHE_VERSION, files: HashMap::new() }
    }
}

impl UsageCache {
    /// Load from disk; any error / schema mismatch yields an empty cache
    /// (the next scan simply rebuilds it).
    fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<UsageCache>(&t).ok())
            .filter(|c| c.v == CACHE_VERSION)
            .unwrap_or_default()
    }

    /// Best-effort persist (tmp + rename so a crash never leaves a torn
    /// file); all errors are swallowed — the cache is a pure optimization.
    fn save(&self, path: &Path) {
        let Some(parent) = path.parent() else { return };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let Ok(json) = serde_json::to_string(self) else { return };
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, path);
        }
    }
}

/// `~/.local/state/cauldron/usage-cache.json`, honoring `$XDG_STATE_HOME`.
fn default_cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("cauldron").join("usage-cache.json"))
}

/// What a cycle must do for one file, given its cached entry and current
/// `(mtime, size)`. Pure — the unit-testable heart of the incremental scan.
#[derive(Debug, PartialEq, Eq)]
enum ScanPlan {
    /// `(mtime, size)` both match: reuse the cached turns, read nothing.
    Reuse,
    /// File grew (append-only assumption): read from this byte offset only.
    Tail(u64),
    /// New file, size decrease (truncation), or same-size rewrite: rescan
    /// from byte 0 and replace the entry.
    Full,
}

fn plan_for(entry: Option<&FileEntry>, mtime_ns: u64, size: u64) -> ScanPlan {
    match entry {
        None => ScanPlan::Full,
        Some(e) if e.mtime_ns == mtime_ns && e.size == size => ScanPlan::Reuse,
        // Growth = append (transcripts are append-only); resume at the last
        // complete line. Anything else — size DECREASE or a same-size
        // rewrite (mtime changed) — invalidates the whole entry.
        Some(e) if size > e.size && e.prefix <= size => ScanPlan::Tail(e.prefix),
        Some(_) => ScanPlan::Full,
    }
}

/// Stable 64-bit FNV-1a — the message-id dedup key must survive persistence
/// across runs and compiler versions, so no `DefaultHasher`.
fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// Scan (pure with respect to `now`; incremental via UsageCache)
// ---------------------------------------------------------------------------

/// What one refresh cycle actually read — boot-trace evidence and the test
/// hook proving cache hits read nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanStats {
    /// Files read this cycle (full or tail).
    pub files_scanned: u64,
    /// Files satisfied entirely from the cache (zero bytes read).
    pub files_cached: u64,
    /// Bytes actually read this cycle.
    pub bytes_read: u64,
}

/// Full (cache-less) scan — the original semantics, kept for tests and as
/// documentation of ground truth: a fresh throwaway cache makes every file a
/// `Full` scan.
pub fn compute_windows(root: &Path, now: SystemTime) -> UsageWindows {
    compute_windows_incremental(root, now, &mut UsageCache::default()).0
}

/// Walk `root` (normally `~/.claude/projects`) and sum assistant-turn usage
/// into the 5-hour / 7-day windows ending at `now`, reading only files the
/// cache cannot answer for (see [`ScanPlan`]). The cache is updated in place:
/// vanished / mtime-gated files are dropped, out-of-window turns are pruned
/// (they can never re-enter a rolling window), so it never grows unboundedly.
///
/// Pure with respect to `now` so tests can inject a fake clock. Missing or
/// unreadable directories/files yield zero totals rather than errors.
fn compute_windows_incremental(
    root: &Path,
    now: SystemTime,
    cache: &mut UsageCache,
) -> (UsageWindows, ScanStats) {
    let mut stats = ScanStats::default();
    let now_secs = match now.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => return (UsageWindows::default(), stats),
    };
    let cutoff_5h = now_secs - WINDOW_5H_SECS;
    let cutoff_7d = now_secs - WINDOW_7D_SECS;
    // mtime gate: a file untouched for >7d cannot contain in-window entries.
    let mtime_cutoff = now.checked_sub(Duration::from_secs(WINDOW_7D_SECS as u64));

    // Files seen this walk; cache entries for anything else are dropped.
    let mut seen_paths: HashSet<String> = HashSet::new();
    // Turns parsed from unterminated final lines: counted this cycle, never
    // cached (see [`FileScan::partial`]).
    let mut partials: Vec<TurnRec> = Vec::new();
    let now_ns = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(u64::MAX);

    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else { continue };
            let path = entry.path();
            if file_type.is_dir() {
                if depth < MAX_WALK_DEPTH {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !file_type.is_file()
                || path.extension().and_then(|e| e.to_str()) != Some("jsonl")
            {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if let (Some(cutoff), Ok(modified)) = (mtime_cutoff, meta.modified()) {
                if modified < cutoff {
                    continue; // mtime-gated: too old to matter (entry dropped below)
                }
            }
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                // Unreadable mtime: stamp with `now` so the entry can never
                // look unchanged — a constant fallback made a same-size
                // rewrite on such a file classify as Reuse forever.
                .unwrap_or(now_ns);
            let size = meta.len();
            let key = path.to_string_lossy().into_owned();
            match plan_for(cache.files.get(&key), mtime_ns, size) {
                ScanPlan::Reuse => stats.files_cached += 1,
                ScanPlan::Tail(from) => match scan_file_turns(&path, from, cutoff_7d) {
                    Some(mut scan) => {
                        stats.files_scanned += 1;
                        stats.bytes_read += scan.bytes;
                        let e = cache.files.entry(key.clone()).or_default();
                        e.turns.append(&mut scan.turns);
                        e.mtime_ns = mtime_ns;
                        e.size = size;
                        e.prefix = scan.prefix;
                        partials.extend(scan.partial);
                    }
                    // Transient open/seek failure (EMFILE, permissions): the
                    // entry stays EXACTLY as it was, so the plan re-derives
                    // and retries next cycle. Stamping the new (mtime, size)
                    // would silently drop the unread bytes.
                    None => stats.files_scanned += 1,
                },
                ScanPlan::Full => match scan_file_turns(&path, 0, cutoff_7d) {
                    Some(scan) => {
                        stats.files_scanned += 1;
                        stats.bytes_read += scan.bytes;
                        cache.files.insert(
                            key.clone(),
                            FileEntry { mtime_ns, size, prefix: scan.prefix, turns: scan.turns },
                        );
                        partials.extend(scan.partial);
                    }
                    // Do NOT cache the failure as an authoritative empty
                    // entry — that froze the file at zero turns until its
                    // (mtime, size) happened to change, which never happens
                    // for a finished session's transcript. Keep any stale
                    // entry (stale beats permanently-zero) and retry.
                    None => stats.files_scanned += 1,
                },
            }
            seen_paths.insert(key);
        }
    }

    // Deleted / aged-out files leave; out-of-window turns are pruned (time
    // only moves forward — a turn older than 7d never re-enters a window).
    cache.files.retain(|k, _| seen_paths.contains(k));
    for e in cache.files.values_mut() {
        e.turns.retain(|t| t.ts >= cutoff_7d);
    }

    let windows = aggregate_windows(cache, &partials, cutoff_5h, cutoff_7d);
    crate::boot_trace::boot_mark!(
        "usage-scan-done files={} cached={} bytes={}",
        stats.files_scanned,
        stats.files_cached,
        stats.bytes_read
    );
    (windows, stats)
}

/// Sum the cached turns — plus this cycle's uncached partial-line turns —
/// into the two windows, deduplicating by message-id hash GLOBALLY (across
/// files, across tail-read boundaries, and between cache and partials) —
/// identical collapse behavior to the original single-pass scan, because
/// usage is verified identical within every id group.
fn aggregate_windows(
    cache: &UsageCache,
    partials: &[TurnRec],
    cutoff_5h: i64,
    cutoff_7d: i64,
) -> UsageWindows {
    let mut out = UsageWindows::default();
    let mut seen: HashSet<u64> = HashSet::new();
    let all = cache.files.values().flat_map(|e| e.turns.iter()).chain(partials);
    for t in all {
        if t.ts < cutoff_7d {
            continue;
        }
        if let Some(h) = t.id_hash {
            if !seen.insert(h) {
                continue;
            }
        }
        out.d7.add(t);
        if t.ts >= cutoff_5h {
            out.h5.add(t);
        }
    }
    out
}

/// One [`scan_file_turns`] read, split by cacheability.
struct FileScan {
    /// Turns from COMPLETE (newline-terminated) lines — safe to persist in the
    /// cache: `prefix` covers them, so no tail read ever re-parses them.
    turns: Vec<TurnRec>,
    /// A turn parsed from an UNTERMINATED final line (partial flush). Counted
    /// THIS cycle only and never cached: `prefix` stops before the line, so
    /// the next tail read re-parses it whole — caching it too would append a
    /// second record, and an id-less turn (id_hash None is always counted)
    /// would double-count in the totals until the 7-day prune.
    partial: Option<TurnRec>,
    /// Byte offset just past the last complete line — the tail resume point.
    prefix: u64,
    bytes: u64,
}

/// Stream one transcript from byte `from` (bounded memory even for 100MB+
/// files) and collect in-window turns, id-deduped within this read.
///
/// `None` means the file could not be opened/seeked AT ALL (EMFILE,
/// permissions) — distinct from a genuinely empty file, so the caller can
/// leave the cache untouched and retry next cycle instead of freezing an
/// authoritative empty entry in. An I/O error MID-file still yields what was
/// read (prefix only covers complete lines, so nothing is lost).
fn scan_file_turns(path: &Path, from: u64, cutoff_7d: i64) -> Option<FileScan> {
    let mut turns: Vec<TurnRec> = Vec::new();
    let mut partial: Option<TurnRec> = None;
    let mut file = fs::File::open(path).ok()?;
    if from > 0 {
        file.seek(SeekFrom::Start(from)).ok()?;
    }
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut buf: Vec<u8> = Vec::new();
    let mut lines: u64 = 0;
    let mut bytes: u64 = 0;
    let mut prefix = from;
    let mut seen_here: HashSet<u64> = HashSet::new();
    while lines < MAX_LINES_PER_FILE {
        lines += 1;
        buf.clear();
        // read_until (not read_line) so invalid UTF-8 can't error the stream.
        let terminated = match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                bytes += n as u64;
                let t = buf.last() == Some(&b'\n');
                if t {
                    prefix = from + bytes;
                }
                t
            }
            Err(_) => break,
        };
        let Ok(line) = std::str::from_utf8(&buf) else { continue };
        // Cheap prefilter: every usage-bearing entry observed has
        // "type":"assistant" (and a "role":"assistant" message). False
        // positives fall through to the real JSON parse below.
        if !line.contains("assistant") {
            continue;
        }
        if let Some(turn) = parse_turn(line, cutoff_7d) {
            if let Some(h) = turn.id_hash {
                if !seen_here.insert(h) {
                    continue; // multi-content-block repeat of the same turn
                }
            }
            if terminated {
                turns.push(turn);
            } else {
                // Only the FINAL line can be unterminated: a fully-flushed-
                // but-unfsynced turn still counts now, without entering the cache.
                partial = Some(turn);
            }
        }
    }
    Some(FileScan { turns, partial, prefix, bytes })
}

/// Parse one JSONL line into a turn record if it is an in-window assistant
/// turn. Malformed lines yield `None`.
fn parse_turn(line: &str, cutoff_7d: i64) -> Option<TurnRec> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if value.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let ts = value
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(parse_timestamp)?;
    if ts < cutoff_7d {
        return None;
    }
    let message = value.get("message")?;
    let usage = message.get("usage").filter(|u| u.is_object())?;
    let id_hash = message.get("id").and_then(|i| i.as_str()).map(fnv1a_64);
    let get = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    Some(TurnRec {
        ts,
        input: get("input_tokens"),
        output: get("output_tokens"),
        cache_read: get("cache_read_input_tokens"),
        cache_write: get("cache_creation_input_tokens"),
        id_hash,
    })
}

// ---------------------------------------------------------------------------
// Refresh cycle (API-first)
// ---------------------------------------------------------------------------

/// Outcome of one refresh cycle.
enum Refresh {
    /// The OAuth endpoint answered — display uses it; NO local scan ran.
    Api(ApiUsage),
    /// Offline fallback: incremental transcript scan.
    Scanned(UsageWindows, ScanStats),
}

/// One refresh cycle, API-FIRST (recon B1): a successful API fetch
/// short-circuits the local transcript scan entirely — the cache is not even
/// touched. Pure with respect to its inputs (the caller does the fetch).
fn refresh(root: &Path, now: SystemTime, api: Option<ApiUsage>, cache: &mut UsageCache) -> Refresh {
    if let Some(api) = api {
        return Refresh::Api(api);
    }
    let (windows, stats) = compute_windows_incremental(root, now, cache);
    Refresh::Scanned(windows, stats)
}

// ---------------------------------------------------------------------------
// Timestamp parsing (std-only ISO 8601 → unix seconds)
// ---------------------------------------------------------------------------

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff…][Z|±HH:MM]` to unix seconds (UTC).
/// The observed transcript form is `2026-07-12T06:31:40.586Z`; offsets and
/// missing fractions are tolerated. Returns `None` on anything else.
fn parse_timestamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let mut n: i64 = 0;
        for &c in &b[range] {
            if !c.is_ascii_digit() {
                return None;
            }
            n = n * 10 + i64::from(c - b'0');
        }
        Some(n)
    };
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut secs =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;

    // Optional fractional seconds (ignored) then optional zone.
    let mut i = 19;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() {
        match b[i] {
            b'Z' | b'z' => {}
            sign @ (b'+' | b'-') => {
                if b.len() < i + 6 || b[i + 3] != b':' {
                    return None;
                }
                let oh = num(i + 1..i + 3)?;
                let om = num(i + 4..i + 6)?;
                let offset = oh * 3600 + om * 60;
                // "+02:00" means local = UTC+2, so UTC = local - offset.
                secs += if sign == b'+' { -offset } else { offset };
            }
            _ => return None,
        }
    }
    Some(secs)
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date
/// (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Human token count: `999` → "999", `1500` → "1.5k", `2_400_000` → "2.4M",
/// billions → "B". One decimal, trailing `.0` trimmed, round-up promoted
/// across unit boundaries (`999_999` → "1M", never "1000k").
pub fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let (mut value, mut suffix) = if n >= 1_000_000_000 {
        (n as f64 / 1e9, "B")
    } else if n >= 1_000_000 {
        (n as f64 / 1e6, "M")
    } else {
        (n as f64 / 1e3, "k")
    };
    // {:.1} would render 999.95..=999.999… as "1000.0" — promote instead.
    if value >= 999.95 {
        value /= 1000.0;
        suffix = match suffix {
            "k" => "M",
            _ => "B",
        };
    }
    let mut s = format!("{value:.1}");
    if s.ends_with(".0") {
        s.truncate(s.len() - 2);
    }
    s.push_str(suffix);
    s
}

// ---------------------------------------------------------------------------
// Meter (non-blocking poll + background rescan thread)
// ---------------------------------------------------------------------------

struct Shared {
    windows: Option<UsageWindows>,
    api: Option<ApiUsage>,
    refreshed_at: Option<Instant>,
}

/// Clears the "a scan is running" flag even if the scan thread panics or the
/// spawn itself fails (the closure is dropped, which drops this guard).
struct ScanGuard(Arc<AtomicBool>);

impl Drop for ScanGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Status-bar meter. `poll()` never blocks on I/O: it returns the latest
/// computed snapshot and, when that snapshot is stale (>[`REFRESH_INTERVAL`])
/// or absent, kicks a background `std::thread` refresh — API-first, with the
/// incremental transcript scan as the offline fallback. The very first kick
/// is delayed [`FIRST_KICK_DELAY`] after construction (boot I/O shield).
/// Safe to call every frame.
pub struct UsageMeter {
    /// Fetch real plan %s from the OAuth endpoint (disabled in fixture tests — no network).
    use_api: bool,
    root: Option<PathBuf>,
    shared: Arc<Mutex<Shared>>,
    scanning: Arc<AtomicBool>,
    /// Per-file incremental scan cache, lazily loaded on the first fallback
    /// scan (an API-only life never touches it) and owned by the scan thread
    /// while it runs.
    cache: Arc<Mutex<Option<UsageCache>>>,
    /// Where the cache persists; `None` = in-memory only (fixture tests).
    cache_path: Option<PathBuf>,
    created: Instant,
    first_kick_delay: Duration,
}

impl UsageMeter {
    /// Meter over `~/.claude/projects`. A missing `$HOME` (or, later, a
    /// missing projects dir) simply makes `poll`/`status_line` return `None`.
    pub fn new() -> Self {
        let root = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".claude").join("projects"));
        Self::with_root_opt(root)
    }

    /// Meter over an explicit root (tests, non-standard layouts). Offline: fixture runs must
    /// never hit the network, so the OAuth fetch is disabled here; the cache stays in-memory
    /// (no cross-test disk state) and the boot delay is zero.
    pub fn with_root(root: PathBuf) -> Self {
        let mut m = Self::with_root_opt(Some(root));
        m.use_api = false;
        m.cache_path = None;
        m.first_kick_delay = Duration::ZERO;
        m
    }

    fn with_root_opt(root: Option<PathBuf>) -> Self {
        Self {
            root,
            use_api: true,
            shared: Arc::new(Mutex::new(Shared {
                windows: None,
                api: None,
                refreshed_at: None,
            })),
            scanning: Arc::new(AtomicBool::new(false)),
            cache: Arc::new(Mutex::new(None)),
            cache_path: default_cache_path(),
            created: Instant::now(),
            first_kick_delay: FIRST_KICK_DELAY,
        }
    }

    /// Latest snapshot (possibly slightly stale), or `None` before the first
    /// refresh completes or when `~/.claude/projects` does not exist. Kicks a
    /// background refresh when the snapshot is older than 120s; the work
    /// never runs on the caller's thread, and the FIRST kick waits
    /// [`FIRST_KICK_DELAY`] so boot I/O is never contended.
    pub fn poll(&mut self) -> Option<UsageWindows> {
        let root = self.root.as_ref()?;
        if !root.is_dir() {
            return None; // missing ~/.claude — stay quiet, no thread churn
        }
        let (snapshot, fresh, never_refreshed) = {
            let shared = lock(&self.shared);
            let fresh = shared
                .refreshed_at
                .is_some_and(|t| t.elapsed() < REFRESH_INTERVAL);
            (shared.windows, fresh, shared.refreshed_at.is_none())
        };
        if never_refreshed && self.created.elapsed() < self.first_kick_delay {
            return snapshot; // boot shield: don't compete with cold-start I/O
        }
        if !fresh && !self.scanning.swap(true, Ordering::AcqRel) {
            let guard = ScanGuard(Arc::clone(&self.scanning));
            let shared = Arc::clone(&self.shared);
            let cache = Arc::clone(&self.cache);
            let cache_path = self.cache_path.clone();
            let use_api = self.use_api;
            let root = root.clone();
            // If spawn fails, the dropped closure drops `guard` → flag clears.
            let _ = std::thread::Builder::new()
                .name("cauldron-usage-scan".to_owned())
                .spawn(move || {
                    let _guard = guard;
                    // API FIRST: when it answers, the transcript scan (and
                    // even loading the cache) is skipped entirely.
                    let api = if use_api { fetch_api_usage() } else { None };
                    let mut slot = cache.lock().unwrap_or_else(|p| p.into_inner());
                    let mut scratch = UsageCache::default(); // API path: never load the real cache
                    let cache_ref: &mut UsageCache = if api.is_some() {
                        &mut scratch
                    } else {
                        slot.get_or_insert_with(|| {
                            cache_path.as_deref().map(UsageCache::load).unwrap_or_default()
                        })
                    };
                    match refresh(&root, SystemTime::now(), api, cache_ref) {
                        Refresh::Api(api) => {
                            crate::boot_trace::boot_mark!(
                                "usage-api-hit (transcript scan skipped)"
                            );
                            let mut shared = lock(&shared);
                            shared.api = Some(api);
                            shared.refreshed_at = Some(Instant::now());
                        }
                        Refresh::Scanned(windows, _stats) => {
                            if let Some(p) = &cache_path {
                                cache_ref.save(p);
                            }
                            let mut shared = lock(&shared);
                            shared.windows = Some(windows);
                            shared.refreshed_at = Some(Instant::now());
                        }
                    }
                });
        }
        snapshot
    }

    /// Compact bottom-right label, e.g. `claude 5h 12% · 7d 34%`.
    /// `None` until a refresh has run.
    pub fn status_line(&self) -> Option<String> {
        let shared = lock(&self.shared);
        // REAL plan utilization from the OAuth endpoint when available (exactly what Claude
        // Code's /usage shows); the local-scan estimate is only the offline fallback.
        if let Some(api) = &shared.api {
            return Some(format!("claude 5h {:.0}% · 7d {:.0}%", api.h5_pct, api.d7_pct));
        }
        let windows = shared.windows?;
        let b = budget();
        let pct = |used: u64, budget: u64| ((used as f64 / budget.max(1) as f64) * 100.0).round();
        Some(format!(
            "claude 5h ~{:.0}% · 7d ~{:.0}%",
            pct(windows.h5.total_ish(), b.h5),
            pct(windows.d7.total_ish(), b.d7),
        ))
    }

    /// Detail for the tooltip behind the percentage line. With a live API
    /// snapshot this shows reset times + scoped limits (the local scan no
    /// longer runs when the API answers); offline it shows the raw numbers.
    pub fn detail_line(&self) -> Option<String> {
        let shared = lock(&self.shared);
        if let Some(api) = &shared.api {
            let mut s = format!(
                "5h {:.0}% resets {} · 7d {:.0}% resets {}",
                api.h5_pct, api.h5_resets, api.d7_pct, api.d7_resets
            );
            for (name, pct) in &api.scoped {
                s.push_str(&format!("\n{name}: {pct:.0}%"));
            }
            return Some(s);
        }
        let windows = shared.windows?;
        let b = budget();
        Some(format!(
            "5h: {} of {} · 7d: {} of {}
budgets: ~/.config/cauldron/claude-budget.toml              (set to the value shown when Claude Code /usage hits 100%)",
            format_tokens(windows.h5.total_ish()),
            format_tokens(b.h5),
            format_tokens(windows.d7.total_ish()),
            format_tokens(b.d7),
        ))
    }
}

/// REAL plan usage from Anthropic's OAuth usage endpoint — the same numbers Claude Code's
/// /usage shows. Reads the existing sign-in via [`crate::ai::token`] (mtime-memoized — the
/// credentials file is NOT re-read every cycle); fetched via `curl` on the background
/// thread. `None` when offline / signed out.
#[derive(Debug, Clone)]
pub struct ApiUsage {
    pub h5_pct: f64,
    pub d7_pct: f64,
    pub h5_resets: String,
    pub d7_resets: String,
    /// Scoped rows from the `limits` array, e.g. ("Fable weekly", 31.0).
    pub scoped: Vec<(String, f64)>,
}

fn fetch_api_usage() -> Option<ApiUsage> {
    let token = crate::ai::token()?;
    // Token hygiene matches ai.rs `ask`: the bearer token travels via
    // `curl --config -` on STDIN — never argv (argv is world-readable in
    // /proc), never logged.
    let cfg = format!(
        concat!(
            "url = \"https://api.anthropic.com/api/oauth/usage\"\n",
            "header = \"Authorization: Bearer {}\"\n",
            "header = \"anthropic-beta: oauth-2025-04-20\"\n",
        ),
        token
    );
    let mut child = std::process::Command::new("curl")
        .args(["-sS", "-m", "10", "--config", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Never early-return between spawn and wait: that would leak a zombie
    // curl. Record the write outcome and always reap.
    let wrote = child
        .stdin
        .take()
        .map(|mut s| std::io::Write::write_all(&mut s, cfg.as_bytes()).is_ok())
        .unwrap_or(false);
    let out = child.wait_with_output().ok()?;
    if !wrote || !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let pct = |k: &str| v.get(k)?.get("utilization")?.as_f64();
    let resets = |k: &str| {
        v.get(k)
            .and_then(|x| x.get("resets_at"))
            .and_then(|x| x.as_str())
            .map(|s| s.chars().take(16).collect::<String>().replace('T', " "))
            .unwrap_or_default()
    };
    let mut scoped = Vec::new();
    if let Some(limits) = v.get("limits").and_then(|l| l.as_array()) {
        for l in limits {
            let percent = l.get("percent").and_then(|p| p.as_f64()).unwrap_or(0.0);
            let kind = l.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            if kind == "weekly_scoped" {
                let name = l
                    .get("scope")
                    .and_then(|s| s.get("model"))
                    .and_then(|m| m.get("display_name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("scoped");
                scoped.push((format!("{name} weekly"), percent));
            }
        }
    }
    Some(ApiUsage {
        h5_pct: pct("five_hour")?,
        d7_pct: pct("seven_day")?,
        h5_resets: resets("five_hour"),
        d7_resets: resets("seven_day"),
        scoped,
    })
}

/// Plan budgets the percentages are computed against. Anthropic does not expose plan limits
/// locally, so these are CALIBRATABLE: `~/.config/cauldron/claude-budget.toml` with
/// `h5_tokens = N` / `d7_tokens = N` overrides the defaults (rough Max-plan scale).
struct Budget {
    h5: u64,
    d7: u64,
}

fn budget() -> Budget {
    let mut b = Budget { h5: 12_000_000, d7: 140_000_000 };
    if let Some(home) = std::env::var_os("HOME") {
        let path = std::path::PathBuf::from(home).join(".config/cauldron/claude-budget.toml");
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if let Some((k, v)) = line.split_once('=') {
                    let v: String = v.trim().chars().filter(|c| c.is_ascii_digit()).collect();
                    if let Ok(n) = v.parse::<u64>() {
                        match k.trim() {
                            "h5_tokens" => b.h5 = n,
                            "d7_tokens" => b.d7 = n,
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    b
}

impl Default for UsageMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock that shrugs off poisoning (a panicked scan must not kill the UI).
fn lock(mutex: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::atomic::AtomicU64;

    /// Inverse of `days_from_civil` (Hinnant's `civil_from_days`), test-only,
    /// for building fixture timestamps relative to an arbitrary `now`.
    fn civil_from_days(z: i64) -> (i64, i64, i64) {
        let z = z + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    fn iso_from_unix(secs: i64) -> String {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60
        )
    }

    fn unix_secs(t: SystemTime) -> i64 {
        t.duration_since(UNIX_EPOCH).expect("post-epoch").as_secs() as i64
    }

    /// A transcript line in the observed schema.
    fn assistant_line(ts: &str, id: &str, inp: u64, out: u64, cw: u64, cr: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","role":"assistant","model":"claude-opus-4-8","usage":{{"input_tokens":{inp},"output_tokens":{out},"cache_creation_input_tokens":{cw},"cache_read_input_tokens":{cr},"service_tier":"standard"}}}}}}"#
        )
    }

    static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Unique per-test fixture root under the system temp dir.
    fn fixture_root(tag: &str) -> PathBuf {
        let n = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cauldron-usage-test-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create fixture root");
        dir
    }

    fn write_jsonl(dir: &Path, name: &str, lines: &[String]) {
        fs::create_dir_all(dir).expect("create project dir");
        let mut f = fs::File::create(dir.join(name)).expect("create fixture");
        for line in lines {
            writeln!(f, "{line}").expect("write fixture line");
        }
    }

    fn append_jsonl(dir: &Path, name: &str, lines: &[String]) -> u64 {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(dir.join(name))
            .expect("open fixture for append");
        let mut appended = 0u64;
        for line in lines {
            writeln!(f, "{line}").expect("append fixture line");
            appended += line.len() as u64 + 1;
        }
        appended
    }

    // -- parse_timestamp ----------------------------------------------------

    #[test]
    fn parse_timestamp_known_values() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        // Independently computed: 2026-07-12T00:00:00Z = 1_783_814_400.
        assert_eq!(
            parse_timestamp("2026-07-12T06:31:40.586Z"),
            Some(1_783_814_400 + 6 * 3600 + 31 * 60 + 40)
        );
        // Offset form: 02:00 east of UTC → subtract.
        assert_eq!(
            parse_timestamp("2026-07-12T02:00:00+02:00"),
            Some(1_783_814_400)
        );
        assert_eq!(parse_timestamp("not a time"), None);
        assert_eq!(parse_timestamp("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_timestamp(""), None);
    }

    #[test]
    fn iso_round_trip() {
        for secs in [0_i64, 1_783_837_900, 86_399, 1_900_000_000] {
            assert_eq!(parse_timestamp(&iso_from_unix(secs)), Some(secs));
        }
    }

    // -- format_tokens ------------------------------------------------------

    #[test]
    fn format_tokens_cases() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(999_999), "1M"); // promoted, not "1000k"
        assert_eq!(format_tokens(2_400_000), "2.4M");
        assert_eq!(format_tokens(18_400_000), "18.4M");
        assert_eq!(format_tokens(3_000_000_000), "3B");
        assert_eq!(format_tokens(1_234_567_890), "1.2B");
    }

    // -- compute_windows ----------------------------------------------------

    #[test]
    fn compute_windows_filters_sums_and_dedupes() {
        let root = fixture_root("windows");
        // Fixed fake now (fixture mtimes are "real now", which is always
        // newer than fake_now - 7d, so the mtime gate never bites here).
        let now_secs: i64 = 1_783_857_600; // 2026-07-12T12:00:00Z
        let now = UNIX_EPOCH + Duration::from_secs(now_secs as u64);

        let in_5h = iso_from_unix(now_secs - 2 * 3600);
        let in_7d = iso_from_unix(now_secs - 4 * 86_400);
        let out_7d = iso_from_unix(now_secs - 8 * 86_400);

        let dup = assistant_line(&in_5h, "msg_dup", 10, 100, 1000, 5000);
        write_jsonl(
            &root.join("proj-a"),
            "a.jsonl",
            &[
                dup.clone(),
                dup, // same message id: one API turn, two content-block lines
                assistant_line(&in_7d, "msg_old", 20, 200, 2000, 6000),
                assistant_line(&out_7d, "msg_ancient", 1, 999_999, 1, 1),
                r#"{"type":"user","timestamp":"2026-07-12T11:00:00.000Z","message":{"role":"user","content":"assistant chatter, no usage"}}"#.to_owned(),
                "{ this line is not JSON".to_owned(),
            ],
        );
        write_jsonl(
            &root.join("proj-b"),
            "b.jsonl",
            &[assistant_line(&in_5h, "msg_b", 5, 50, 500, 100)],
        );
        // Non-transcript files are ignored entirely (wrong extension).
        let memory_dir = root.join("proj-a").join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(memory_dir.join("MEMORY.md"), "assistant").unwrap();

        let w = compute_windows(&root, now);

        assert_eq!(
            w.h5,
            UsageTotals {
                output_tokens: 150,
                input_tokens: 15,
                cache_read: 5100,
                cache_write: 1500,
                turns: 2,
            }
        );
        assert_eq!(
            w.d7,
            UsageTotals {
                output_tokens: 350,
                input_tokens: 35,
                cache_read: 11_100,
                cache_write: 3500,
                turns: 3,
            }
        );
        assert_eq!(w.h5.total_ish(), 150 + 15 + 1500);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn compute_windows_mtime_gates_old_files() {
        let root = fixture_root("mtime");
        // Fake "now" 30 days in the future: the fixture's real mtime is
        // older than fake_now - 7d, so the file must be skipped even though
        // its entry timestamps are inside the (future) windows.
        let future = SystemTime::now() + Duration::from_secs(30 * 86_400);
        let ts = iso_from_unix(unix_secs(future) - 3600);
        write_jsonl(
            &root.join("proj"),
            "s.jsonl",
            &[assistant_line(&ts, "msg_future", 1, 1000, 1, 1)],
        );

        let gated = compute_windows(&root, future);
        assert_eq!(gated, UsageWindows::default(), "old-mtime file not skipped");

        // Sanity: with a present-day `now` the same entries are unreachable
        // by timestamp (future), so totals are still zero — but the file IS
        // read; prove the gate itself by re-dating the entry to now-1h.
        let ts_now = iso_from_unix(unix_secs(SystemTime::now()) - 3600);
        write_jsonl(
            &root.join("proj"),
            "s.jsonl",
            &[assistant_line(&ts_now, "msg_now", 1, 1000, 1, 1)],
        );
        let fresh = compute_windows(&root, SystemTime::now());
        assert_eq!(fresh.h5.output_tokens, 1000);
        assert_eq!(fresh.h5.turns, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn compute_windows_missing_root_is_zero() {
        let bogus = std::env::temp_dir().join("cauldron-usage-test-definitely-missing");
        let _ = fs::remove_dir_all(&bogus);
        assert_eq!(
            compute_windows(&bogus, SystemTime::now()),
            UsageWindows::default()
        );
    }

    #[test]
    fn compute_windows_survives_binary_garbage() {
        let root = fixture_root("garbage");
        let dir = root.join("proj");
        fs::create_dir_all(&dir).unwrap();
        let mut f = fs::File::create(dir.join("junk.jsonl")).unwrap();
        f.write_all(&[0xFF, 0xFE, 0x00, b'a', b's', b's', b'i', b'\n']).unwrap();
        let ts = iso_from_unix(unix_secs(SystemTime::now()) - 60);
        writeln!(f, "{}", assistant_line(&ts, "msg_ok", 2, 20, 200, 0)).unwrap();
        drop(f);

        let w = compute_windows(&root, SystemTime::now());
        assert_eq!(w.h5.output_tokens, 20);
        assert_eq!(w.h5.turns, 1);

        let _ = fs::remove_dir_all(&root);
    }

    // -- incremental cache: plan_for (pure) ----------------------------------

    #[test]
    fn plan_for_hit_miss_truncate_grow() {
        let e = FileEntry { mtime_ns: 100, size: 50, prefix: 50, turns: Vec::new() };
        // Miss: unknown file → full scan.
        assert_eq!(plan_for(None, 100, 50), ScanPlan::Full);
        // Hit: (mtime, size) both match → read nothing.
        assert_eq!(plan_for(Some(&e), 100, 50), ScanPlan::Reuse);
        // Growth: append-only assumption → tail read from the last complete line.
        assert_eq!(plan_for(Some(&e), 200, 80), ScanPlan::Tail(50));
        // Truncation: size DECREASE always invalidates.
        assert_eq!(plan_for(Some(&e), 200, 20), ScanPlan::Full);
        // Same-size rewrite (mtime changed): invalidates.
        assert_eq!(plan_for(Some(&e), 200, 50), ScanPlan::Full);
        // Growth resumes at prefix, not size, when the last line was partial.
        let partial = FileEntry { mtime_ns: 100, size: 50, prefix: 42, turns: Vec::new() };
        assert_eq!(plan_for(Some(&partial), 200, 80), ScanPlan::Tail(42));
    }

    // -- incremental cache: end-to-end ---------------------------------------

    /// Fake `now` close to real time so real fixture mtimes pass the 7d gate.
    fn test_now() -> (SystemTime, i64) {
        let now = SystemTime::now();
        (now, unix_secs(now))
    }

    #[test]
    fn second_scan_reads_nothing_when_files_unchanged() {
        let root = fixture_root("inc-hit");
        let (now, now_secs) = test_now();
        let ts = iso_from_unix(now_secs - 3600);
        write_jsonl(&root.join("p1"), "a.jsonl", &[assistant_line(&ts, "m1", 1, 10, 0, 0)]);
        write_jsonl(&root.join("p2"), "b.jsonl", &[assistant_line(&ts, "m2", 2, 20, 0, 0)]);

        let mut cache = UsageCache::default();
        let (w1, s1) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(s1.files_scanned, 2);
        assert_eq!(s1.files_cached, 0);
        assert!(s1.bytes_read > 0);
        assert_eq!(w1.h5.output_tokens, 30);

        let (w2, s2) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(s2, ScanStats { files_scanned: 0, files_cached: 2, bytes_read: 0 });
        assert_eq!(w2, w1, "cached totals must equal freshly scanned totals");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn appended_file_is_tail_read_only() {
        let root = fixture_root("inc-tail");
        let (now, now_secs) = test_now();
        let ts = iso_from_unix(now_secs - 3600);
        let dir = root.join("proj");
        write_jsonl(&dir, "s.jsonl", &[assistant_line(&ts, "m_head", 1, 10, 0, 0)]);
        write_jsonl(&dir, "quiet.jsonl", &[assistant_line(&ts, "m_quiet", 0, 5, 0, 0)]);

        let mut cache = UsageCache::default();
        let (w1, _) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(w1.h5.output_tokens, 15);

        // Append: one duplicate of an already-counted turn (a content-block
        // line landing after the scan boundary) + one genuinely new turn.
        let appended = append_jsonl(
            &dir,
            "s.jsonl",
            &[
                assistant_line(&ts, "m_head", 1, 10, 0, 0),
                assistant_line(&ts, "m_new", 3, 100, 0, 0),
            ],
        );

        let (w2, s2) = compute_windows_incremental(&root, now, &mut cache);
        // Only the grown file is touched, and only its tail is read.
        assert_eq!(s2.files_scanned, 1);
        assert_eq!(s2.files_cached, 1);
        assert_eq!(s2.bytes_read, appended, "tail read must cover ONLY the appended bytes");
        // The straddling duplicate id collapses at aggregation time.
        assert_eq!(w2.h5.output_tokens, 15 + 100);
        assert_eq!(w2.h5.turns, 3);

        let _ = fs::remove_dir_all(&root);
    }

    /// An id-less assistant turn first seen on an UNTERMINATED final line (partial flush)
    /// must count exactly once — before the fix it was cached AND re-read by the next tail
    /// scan (prefix stops before the partial line), double-counting permanently: id_hash
    /// None turns are exempt from the aggregation-time id dedup by design.
    #[test]
    fn idless_partial_tail_line_counts_exactly_once() {
        let root = fixture_root("inc-partial");
        let (now, now_secs) = test_now();
        let ts = iso_from_unix(now_secs - 3600);
        let dir = root.join("proj");
        // One complete id-carrying line, then an id-less line WITHOUT a trailing newline.
        let idless = format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","usage":{{"input_tokens":1,"output_tokens":5}}}}}}"#
        );
        write_jsonl(&dir, "s.jsonl", &[assistant_line(&ts, "m1", 1, 10, 0, 0)]);
        {
            let mut f = fs::OpenOptions::new().append(true).open(dir.join("s.jsonl")).unwrap();
            write!(f, "{idless}").unwrap(); // NO newline — partial flush
        }

        let mut cache = UsageCache::default();
        let (w1, _) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(w1.h5.output_tokens, 15, "partial-line turn counts the cycle it appears");
        assert_eq!(w1.h5.turns, 2);
        let entry = cache.files.values().next().unwrap();
        assert_eq!(entry.turns.len(), 1, "the partial-line turn must NOT enter the cache");

        // The line completes + a new turn lands: the finished id-less turn is tail-read
        // whole and must still count once, not once-per-cycle-it-straddled.
        let appended = {
            let mut f = fs::OpenOptions::new().append(true).open(dir.join("s.jsonl")).unwrap();
            let tail = format!("\n{}\n", assistant_line(&ts, "m2", 3, 100, 0, 0));
            write!(f, "{tail}").unwrap();
            tail.len() as u64
        };
        let (w2, s2) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(s2.files_scanned, 1);
        assert_eq!(
            s2.bytes_read,
            idless.len() as u64 + appended,
            "tail read resumes before the previously-partial line"
        );
        assert_eq!(w2.h5.output_tokens, 115, "id-less turn must not double-count");
        assert_eq!(w2.h5.turns, 3);

        // Steady state: nothing grew — pure cache reuse, totals identical.
        let (w3, s3) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(s3, ScanStats { files_scanned: 0, files_cached: 1, bytes_read: 0 });
        assert_eq!(w3, w2);

        let _ = fs::remove_dir_all(&root);
    }

    /// A transiently unreadable transcript (EMFILE/permissions — this box has documented
    /// NOFILE incidents) must NOT be cached as an authoritative empty entry: before the
    /// fix, one failed open froze the file at zero turns until its (mtime, size) changed —
    /// which never happens for a finished session's transcript.
    #[test]
    fn unreadable_file_is_retried_not_cached_empty() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture_root("inc-eacces");
        let (now, now_secs) = test_now();
        let ts = iso_from_unix(now_secs - 3600);
        let dir = root.join("proj");
        write_jsonl(&dir, "s.jsonl", &[assistant_line(&ts, "m1", 1, 10, 0, 0)]);
        let file = dir.join("s.jsonl");

        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&file).is_ok() {
            // Running as root: the failure cannot be simulated — skip.
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let mut cache = UsageCache::default();
        let (w1, _) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(w1.h5.output_tokens, 0, "unreadable file contributes nothing this cycle");
        assert!(
            !cache.files.values().any(|e| e.turns.is_empty() && e.size > 0),
            "failure must not be frozen in as an authoritative empty entry"
        );

        // The hiccup passes (mtime and size UNCHANGED) → the very next cycle recovers.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let (w2, s2) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(s2.files_scanned, 1, "must retry, not Reuse a frozen entry");
        assert_eq!(w2.h5.output_tokens, 10, "turns recovered after the transient failure");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn truncated_file_is_invalidated_and_rescanned() {
        let root = fixture_root("inc-trunc");
        let (now, now_secs) = test_now();
        let ts = iso_from_unix(now_secs - 3600);
        let dir = root.join("proj");
        write_jsonl(
            &dir,
            "s.jsonl",
            &[
                assistant_line(&ts, "m_a", 1, 10, 0, 0),
                assistant_line(&ts, "m_b", 2, 20, 0, 0),
            ],
        );

        let mut cache = UsageCache::default();
        let (w1, _) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(w1.h5.output_tokens, 30);

        // Rewrite SMALLER (size decrease): stale cached turns must vanish.
        write_jsonl(&dir, "s.jsonl", &[assistant_line(&ts, "m_c", 0, 7, 0, 0)]);

        let (w2, s2) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(s2.files_scanned, 1, "truncated file must be rescanned in full");
        assert_eq!(w2.h5.output_tokens, 7, "old turns from the invalidated file must be gone");
        assert_eq!(w2.h5.turns, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn deleted_file_drops_from_cache_and_totals() {
        let root = fixture_root("inc-del");
        let (now, now_secs) = test_now();
        let ts = iso_from_unix(now_secs - 3600);
        write_jsonl(&root.join("p1"), "a.jsonl", &[assistant_line(&ts, "m1", 1, 10, 0, 0)]);
        write_jsonl(&root.join("p2"), "b.jsonl", &[assistant_line(&ts, "m2", 2, 20, 0, 0)]);

        let mut cache = UsageCache::default();
        let (w1, _) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(w1.h5.output_tokens, 30);
        assert_eq!(cache.files.len(), 2);

        fs::remove_file(root.join("p1").join("a.jsonl")).unwrap();

        let (w2, _) = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(w2.h5.output_tokens, 20);
        assert_eq!(cache.files.len(), 1, "vanished file's entry must leave the cache");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn out_of_window_turns_are_pruned_from_cache() {
        let root = fixture_root("inc-prune");
        let (now, now_secs) = test_now();
        let dir = root.join("proj");
        write_jsonl(
            &dir,
            "s.jsonl",
            &[assistant_line(&iso_from_unix(now_secs - 3600), "m1", 1, 10, 0, 0)],
        );

        let mut cache = UsageCache::default();
        let _ = compute_windows_incremental(&root, now, &mut cache);
        assert_eq!(cache.files.values().map(|e| e.turns.len()).sum::<usize>(), 1);

        // 3 days later (file unchanged, real mtime still passes the +3d gate):
        // the cached entry is REUSED, the turn has left the 5h window but is
        // still inside 7d — rolling windows recompute correctly from cache.
        let (w, s) = compute_windows_incremental(&root, now + Duration::from_secs(3 * 86_400), &mut cache);
        assert_eq!(s.files_cached, 1);
        assert_eq!(w.h5.turns, 0, "turn left the 5h window");
        assert_eq!(w.d7.turns, 1, "still inside 7d at +3d");

        // Beyond +7d the whole FILE is mtime-gated (an untouched file cannot
        // hold in-window turns), so its entry leaves the cache entirely —
        // aging out never leaves stale turns behind.
        let (w8, _) = compute_windows_incremental(&root, now + Duration::from_secs(8 * 86_400), &mut cache);
        assert_eq!(w8, UsageWindows::default());
        assert!(cache.files.is_empty(), "aged-out file's entry must leave the cache");

        let _ = fs::remove_dir_all(&root);
    }

    // -- cache persistence ----------------------------------------------------

    #[test]
    fn cache_save_load_round_trip() {
        let root = fixture_root("cache-io");
        let path = root.join("state").join("usage-cache.json");

        let mut cache = UsageCache::default();
        cache.files.insert(
            "/some/transcript.jsonl".to_owned(),
            FileEntry {
                mtime_ns: 123_456_789,
                size: 42,
                prefix: 40,
                turns: vec![
                    TurnRec { ts: 1_783_857_600, input: 1, output: 2, cache_read: 3, cache_write: 4, id_hash: Some(fnv1a_64("msg_x")) },
                    TurnRec { ts: 1_783_857_601, input: 5, output: 6, cache_read: 7, cache_write: 8, id_hash: None },
                ],
            },
        );
        cache.save(&path);
        assert_eq!(UsageCache::load(&path), cache);

        // Corrupted file → empty default, never an error.
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(UsageCache::load(&path), UsageCache::default());

        // Version mismatch → discarded.
        let mut old = cache;
        old.v = CACHE_VERSION + 1;
        let Ok(json) = serde_json::to_string(&old) else { panic!() };
        fs::write(&path, json).unwrap();
        assert_eq!(UsageCache::load(&path), UsageCache::default());

        // Missing file → default.
        assert_eq!(UsageCache::load(&root.join("nope.json")), UsageCache::default());

        let _ = fs::remove_dir_all(&root);
    }

    // -- API-first short-circuit ----------------------------------------------

    fn fake_api() -> ApiUsage {
        ApiUsage {
            h5_pct: 12.0,
            d7_pct: 34.0,
            h5_resets: "2026-07-12 17:00".into(),
            d7_resets: "2026-07-15 00:00".into(),
            scoped: vec![("Fable weekly".into(), 31.0)],
        }
    }

    #[test]
    fn api_result_short_circuits_the_local_scan() {
        let root = fixture_root("api-first");
        let (now, now_secs) = test_now();
        let ts = iso_from_unix(now_secs - 3600);
        write_jsonl(&root.join("proj"), "s.jsonl", &[assistant_line(&ts, "m1", 1, 10, 0, 0)]);

        // API answered: the scan must not run — the cache stays untouched.
        let mut cache = UsageCache::default();
        match refresh(&root, now, Some(fake_api()), &mut cache) {
            Refresh::Api(api) => assert_eq!(api.h5_pct, 12.0),
            Refresh::Scanned(..) => panic!("API success must skip the transcript scan"),
        }
        assert!(cache.files.is_empty(), "API-first refresh must not touch the scan cache");

        // Offline: the incremental scan runs.
        match refresh(&root, now, None, &mut cache) {
            Refresh::Scanned(w, s) => {
                assert_eq!(w.h5.output_tokens, 10);
                assert_eq!(s.files_scanned, 1);
            }
            Refresh::Api(_) => panic!("no API result was supplied"),
        }

        let _ = fs::remove_dir_all(&root);
    }

    // -- UsageMeter ---------------------------------------------------------

    #[test]
    fn meter_polls_in_background_and_formats() {
        let root = fixture_root("meter");
        let ts = iso_from_unix(unix_secs(SystemTime::now()) - 120);
        write_jsonl(
            &root.join("proj"),
            "s.jsonl",
            &[assistant_line(&ts, "msg_m", 100_000, 1_100_000, 300_000, 9)],
        );

        let mut meter = UsageMeter::with_root(root.clone());
        assert_eq!(meter.status_line(), None, "no snapshot before first scan");

        // First poll returns None but kicks the background scan.
        let mut snapshot = meter.poll();
        for _ in 0..200 {
            if snapshot.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
            snapshot = meter.poll();
        }
        let w = snapshot.expect("background scan should complete");
        assert_eq!(w.h5.total_ish(), 1_500_000);
        assert_eq!(
            meter.status_line().as_deref(),
            Some("claude 5h ~13% · 7d ~1%")
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn meter_missing_root_returns_none() {
        let bogus = std::env::temp_dir().join("cauldron-usage-test-no-such-root");
        let _ = fs::remove_dir_all(&bogus);
        let mut meter = UsageMeter::with_root(bogus);
        assert_eq!(meter.poll(), None);
        assert_eq!(meter.status_line(), None);
    }

    #[test]
    fn first_kick_is_delayed_at_boot() {
        let root = fixture_root("kick-delay");
        let ts = iso_from_unix(unix_secs(SystemTime::now()) - 120);
        write_jsonl(&root.join("proj"), "s.jsonl", &[assistant_line(&ts, "m", 1, 10, 0, 0)]);

        let mut meter = UsageMeter::with_root(root.clone());
        meter.first_kick_delay = Duration::from_secs(3600); // "still booting"

        for _ in 0..5 {
            assert_eq!(meter.poll(), None);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !meter.scanning.load(Ordering::Acquire),
            "no scan may be kicked inside the boot-delay window"
        );

        // Delay elapsed (simulated): the normal kick + 120s cadence resumes.
        meter.first_kick_delay = Duration::ZERO;
        let mut snapshot = meter.poll();
        for _ in 0..200 {
            if snapshot.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
            snapshot = meter.poll();
        }
        assert_eq!(snapshot.expect("scan runs after the delay").h5.output_tokens, 10);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn api_snapshot_wins_at_display_time() {
        let bogus = std::env::temp_dir().join("cauldron-usage-test-api-display");
        let mut meter = UsageMeter::with_root(bogus);
        {
            let mut shared = lock(&meter.shared);
            shared.api = Some(fake_api());
            shared.refreshed_at = Some(Instant::now());
        }
        assert_eq!(
            meter.status_line().as_deref(),
            Some("claude 5h 12% · 7d 34%")
        );
        let detail = meter.detail_line().expect("api detail");
        assert!(detail.contains("resets 2026-07-12 17:00"), "{detail}");
        assert!(detail.contains("Fable weekly: 31%"), "{detail}");
        // poll() still returns no WINDOWS snapshot — none was ever scanned.
        assert_eq!(meter.poll(), None);
    }
}
