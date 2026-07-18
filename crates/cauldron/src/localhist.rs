//! Local History — JetBrains-style save snapshots, independent of git.
//!
//! Every save calls [`record`], which drops a plain-text snapshot under
//! `~/.local/share/cauldron/history/<fnv1a64-of-abs-path>/<unix-millis>.snap` (plus a tiny
//! `meta.json` remembering the original path). Dedupe against the latest snapshot keeps the
//! call cheap; pruning caps the set at 100 snapshots / 30 days (the newest 10 are immortal).
//!
//! [`HistoryUi::ui`] renders the tool window: snapshot list on the left, colored line diff
//! (selected snapshot vs the CURRENT buffer) on the right, and a "Restore this version"
//! button that hands the snapshot text back to the integrator as one undoable transaction.
//!
//! No threads, no async — everything here is a handful of small file ops on the save path.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use egui::{FontId, RichText};

use crate::style::{self, colors};

const MAX_SNAPSHOTS: usize = 100;
const MAX_AGE_MILLIS: u64 = 30 * 24 * 60 * 60 * 1000; // 30 days
const KEEP_NEWEST: usize = 10; // never age-pruned
/// Above this many lines on either side, `diff` falls back to prefix/suffix trimming.
const LCS_LINE_LIMIT: usize = 20_000;
/// Cap on DP table cells (n*m). 25M cells = 100 MB of u32 — beyond that, fall back rather
/// than allocate up to ~1.6 GB at the line limit.
const LCS_CELL_LIMIT: usize = 25_000_000;

// =================================================================================================
// storage
// =================================================================================================

/// Root of the history store. `CAULDRON_HISTORY_DIR` (tests) beats the XDG default.
pub(crate) fn base_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CAULDRON_HISTORY_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".local/share/cauldron/history")
}

/// FNV-1a 64 over the absolute path string — the per-file directory name.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn abs_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map(|c| c.join(path)).unwrap_or_else(|_| path.to_path_buf())
    }
}

fn dir_for(path: &Path) -> PathBuf {
    let abs = abs_path(path);
    base_dir().join(format!("{:016x}", fnv1a64(abs.to_string_lossy().as_bytes())))
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// One stored snapshot of a file.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Unix time of the save, in milliseconds.
    pub ts_millis: u64,
    /// The snapshot file on disk.
    pub path: PathBuf,
    /// A user-applied label ("before refactor") — shown in the timeline; a labeled snapshot is
    /// immortal (pruning never touches it). Stored in a `{ts}.label` sidecar.
    pub label: Option<String>,
}

/// The `{ts}.label` sidecar path for a snapshot file.
fn label_path(snap_path: &Path) -> PathBuf {
    snap_path.with_extension("label")
}

/// Set (or clear, with `None`) the label on a snapshot — JetBrains "Put Label". A labeled
/// snapshot survives pruning.
pub fn set_label(snap: &Snapshot, label: Option<&str>) {
    let lp = label_path(&snap.path);
    match label.map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => {
            let _ = fs::write(&lp, text);
        }
        None => {
            let _ = fs::remove_file(&lp);
        }
    }
}

/// Record `content` as a new snapshot of `path`. Skips if identical to the latest snapshot;
/// prunes old snapshots afterwards. Cheap: one dir listing + at most a few file ops.
pub fn record(path: &Path, content: &str) {
    let dir = dir_for(path);
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let snaps = list(path);
    // Dedupe against the newest snapshot only — byte comparison.
    if let Some(latest) = snaps.first() {
        if let Ok(prev) = fs::read(&latest.path) {
            if prev == content.as_bytes() {
                return;
            }
        }
    }
    // meta.json: remember the original path (written once).
    let meta = dir.join("meta.json");
    if !meta.exists() {
        let json = serde_json::json!({ "path": abs_path(path).to_string_lossy() });
        let _ = fs::write(&meta, json.to_string());
    }
    // Never collide with an existing timestamp (two saves in the same millisecond).
    let mut ts = now_millis();
    if let Some(latest) = snaps.first() {
        if ts <= latest.ts_millis {
            ts = latest.ts_millis + 1;
        }
    }
    let _ = fs::write(dir.join(format!("{ts}.snap")), content);
    prune(&dir);
}

/// Delete snapshots beyond the count cap and past the age cap (newest [`KEEP_NEWEST`] immortal).
fn prune(dir: &Path) {
    let mut snaps = list_dir(dir); // newest first
    // A LABELED snapshot is immortal — a user checkpoint must never be pruned. It doesn't
    // count against the cap either (drop labeled entries from the working set first).
    snaps.retain(|s| s.label.is_none());
    // Count cap.
    while snaps.len() > MAX_SNAPSHOTS {
        if let Some(s) = snaps.pop() {
            let _ = fs::remove_file(&s.path);
        }
    }
    // Age cap — never touch the newest KEEP_NEWEST.
    let cutoff = now_millis().saturating_sub(MAX_AGE_MILLIS);
    for s in snaps.iter().skip(KEEP_NEWEST) {
        if s.ts_millis < cutoff {
            let _ = fs::remove_file(&s.path);
        }
    }
}

fn list_dir(dir: &Path) -> Vec<Snapshot> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else { return out };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().map(|e| e == "snap") != Some(true) {
            continue;
        }
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            if let Ok(ts) = stem.parse::<u64>() {
                let label = fs::read_to_string(label_path(&p))
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                out.push(Snapshot { ts_millis: ts, path: p, label });
            }
        }
    }
    out.sort_by(|a, b| b.ts_millis.cmp(&a.ts_millis)); // newest first
    out
}

/// All snapshots of `path`, newest first.
pub fn list(path: &Path) -> Vec<Snapshot> {
    list_dir(&dir_for(path))
}

/// The stored text of a snapshot, if it still exists and is valid UTF-8.
pub fn read(snap: &Snapshot) -> Option<String> {
    fs::read_to_string(&snap.path).ok()
}

// =================================================================================================
// diff — line-level LCS (O(n*m)); naive prefix/suffix trim beyond 20k lines a side
// =================================================================================================

/// Classification of one diff output line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffKind {
    Same,
    Add,
    Del,
}

/// One line of diff output.
#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// Line diff `old` → `new`. Dels come from `old`, Adds from `new`.
pub fn diff(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    // Common prefix/suffix trim — makes the LCS table tiny for typical edits.
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }
    let ma = &a[pre..a.len() - suf];
    let mb = &b[pre..b.len() - suf];

    let mut out: Vec<DiffLine> = Vec::with_capacity(a.len().max(b.len()));
    for l in &a[..pre] {
        out.push(DiffLine { kind: DiffKind::Same, text: (*l).to_string() });
    }

    if ma.len() > LCS_LINE_LIMIT
        || mb.len() > LCS_LINE_LIMIT
        || (ma.len() + 1).saturating_mul(mb.len() + 1) > LCS_CELL_LIMIT
    {
        // Guarded fallback: everything in the middle is del-then-add.
        for l in ma {
            out.push(DiffLine { kind: DiffKind::Del, text: (*l).to_string() });
        }
        for l in mb {
            out.push(DiffLine { kind: DiffKind::Add, text: (*l).to_string() });
        }
    } else {
        lcs_diff(ma, mb, &mut out);
    }

    for l in &a[a.len() - suf..] {
        out.push(DiffLine { kind: DiffKind::Same, text: (*l).to_string() });
    }
    out
}

/// Classic DP LCS over the trimmed middle, emitting Same/Del/Add lines.
fn lcs_diff(a: &[&str], b: &[&str], out: &mut Vec<DiffLine>) {
    let (n, m) = (a.len(), b.len());
    // dp[i][j] = LCS length of a[i..], b[j..]; flattened.
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[idx(i, j)] = if a[i] == b[j] {
                dp[idx(i + 1, j + 1)] + 1
            } else {
                dp[idx(i + 1, j)].max(dp[idx(i, j + 1)])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(DiffLine { kind: DiffKind::Same, text: a[i].to_string() });
            i += 1;
            j += 1;
        } else if dp[idx(i + 1, j)] >= dp[idx(i, j + 1)] {
            out.push(DiffLine { kind: DiffKind::Del, text: a[i].to_string() });
            i += 1;
        } else {
            out.push(DiffLine { kind: DiffKind::Add, text: b[j].to_string() });
            j += 1;
        }
    }
    for l in &a[i..] {
        out.push(DiffLine { kind: DiffKind::Del, text: (*l).to_string() });
    }
    for l in &b[j..] {
        out.push(DiffLine { kind: DiffKind::Add, text: (*l).to_string() });
    }
}

// =================================================================================================
// time formatting — "5 min ago / 14:02 / Jul 12" without chrono (UTC civil-date math)
// =================================================================================================

/// Days-since-epoch → (year, month, day). Howard Hinnant's civil_from_days.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Human label for a snapshot: relative when recent, clock time earlier today, date otherwise.
fn human_time(ts_millis: u64, now: u64) -> String {
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let ago = now.saturating_sub(ts_millis) / 1000; // seconds
    if ago < 60 {
        return "just now".into();
    }
    if ago < 3600 {
        return format!("{} min ago", ago / 60);
    }
    let secs = ts_millis / 1000;
    let (h, min) = ((secs / 3600) % 24, (secs / 60) % 60);
    if secs / 86_400 == (now / 1000) / 86_400 {
        return format!("{h:02}:{min:02}");
    }
    let (_, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{} {} {h:02}:{min:02}", MONTHS[(m - 1) as usize], d)
}

// =================================================================================================
// UI — snapshot list + colored diff vs the current buffer
// =================================================================================================

/// The Local History tool window. Returns `Some(text)` from [`HistoryUi::ui`] when the user
/// clicks "Restore this version" — apply it as one undoable transaction.
#[derive(Default)]
pub struct HistoryUi {
    selected: Option<usize>,
    /// (file, snapshot ts) the cached diff was computed for.
    diff_key: Option<(PathBuf, u64)>,
    /// Hash of the current buffer the cached diff was computed against.
    diff_cur: u64,
    diff_cache: Vec<DiffLine>,
    /// Draft label text for the selected snapshot (the inline "Put Label" editor).
    label_draft: String,
    /// The snapshot ts `label_draft` was seeded for (reseed when the selection changes).
    label_for: Option<u64>,
}

impl HistoryUi {
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        current_text: &str,
        file: &Path,
    ) -> Option<String> {
        let snaps = list(file);
        let mut restored: Option<String> = None;
        if let Some(sel) = self.selected {
            if sel >= snaps.len() {
                self.selected = None;
            }
        }
        let now = now_millis();

        ui.horizontal_top(|ui| {
            // ---- left: snapshot list --------------------------------------------------------
            ui.vertical(|ui| {
                ui.set_width(170.0);
                style::panel_header_inline(ui, "Local History");
                ui.add_space(4.0);
                if snaps.is_empty() {
                    ui.label(RichText::new("no snapshots yet").color(colors::TEXT_FAINT()));
                }
                egui::ScrollArea::vertical().id_salt("localhist_list").show(ui, |ui| {
                    for (i, s) in snaps.iter().enumerate() {
                        let active = self.selected == Some(i);
                        // A labeled snapshot leads with its name (🏷) and the time in fainter
                        // text; unlabeled ones show just the time.
                        let btn_label = match &s.label {
                            Some(l) => format!("🏷 {l}"),
                            None => human_time(s.ts_millis, now),
                        };
                        let resp = style::tool_button(ui, &btn_label, active);
                        let resp = if s.label.is_some() {
                            resp.on_hover_text(human_time(s.ts_millis, now))
                        } else {
                            resp
                        };
                        if resp.clicked_by(egui::PointerButton::Primary) {
                            self.selected = Some(i);
                        }
                    }
                });
            });
            ui.add(egui::Separator::default().vertical());

            // ---- right: diff of selected snapshot vs the CURRENT buffer ---------------------
            ui.vertical(|ui| {
                let Some(sel) = self.selected else {
                    ui.label(
                        RichText::new("Select a snapshot to compare with the current buffer.")
                            .color(colors::TEXT_FAINT()),
                    );
                    return;
                };
                let snap = &snaps[sel];
                let Some(old) = read(snap) else {
                    ui.label(RichText::new("snapshot unreadable").color(colors::ERROR()));
                    return;
                };

                ui.horizontal(|ui| {
                    style::panel_header_inline(ui, &human_time(snap.ts_millis, now));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Restore this version").clicked_by(egui::PointerButton::Primary) {
                            restored = Some(old.clone());
                        }
                    });
                });
                // Inline label editor ("Put Label"): seed from the snapshot's current label when
                // the selection changes, then write on Enter/Set. A labeled snapshot is immortal.
                if self.label_for != Some(snap.ts_millis) {
                    self.label_draft = snap.label.clone().unwrap_or_default();
                    self.label_for = Some(snap.ts_millis);
                }
                ui.horizontal(|ui| {
                    ui.colored_label(colors::TEXT_FAINT(), "🏷");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.label_draft)
                            .hint_text("label this version (kept forever)…")
                            .desired_width(240.0),
                    );
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Set").clicked_by(egui::PointerButton::Primary) || enter {
                        set_label(snap, Some(&self.label_draft));
                    }
                    if snap.label.is_some()
                        && ui.button("Clear").clicked_by(egui::PointerButton::Primary)
                    {
                        set_label(snap, None);
                        self.label_draft.clear();
                    }
                });
                style::hairline(ui);

                // Cache the diff — recompute only when snapshot or buffer changes.
                let cur_hash = fnv1a64(current_text.as_bytes());
                let key = (file.to_path_buf(), snap.ts_millis);
                if self.diff_key.as_ref() != Some(&key) || self.diff_cur != cur_hash {
                    self.diff_cache = diff(&old, current_text);
                    self.diff_key = Some(key);
                    self.diff_cur = cur_hash;
                }

                let font = FontId::monospace(13.0);
                egui::ScrollArea::both().id_salt("localhist_diff").show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for line in &self.diff_cache {
                        let (prefix, color) = match line.kind {
                            DiffKind::Same => (' ', colors::TEXT_MUTED()),
                            DiffKind::Add => ('+', colors::MOSS()),
                            DiffKind::Del => ('-', colors::ERROR()),
                        };
                        ui.label(
                            RichText::new(format!("{prefix} {}", line.text))
                                .font(font.clone())
                                .color(color),
                        );
                    }
                });
            });
        });
        restored
    }
}

// =================================================================================================
// tests
// =================================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};

    /// CAULDRON_HISTORY_DIR is process-global — serialize the tests that set it.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn temp_store(name: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cauldron-localhist-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CAULDRON_HISTORY_DIR", &dir);
        dir
    }

    /// A labeled snapshot round-trips its label and survives pruning (the count cap can't evict
    /// it even when far more than MAX_SNAPSHOTS newer snapshots exist).
    #[test]
    fn labels_persist_and_survive_pruning() {
        let _g = env_lock().lock().unwrap();
        let store = temp_store("labels");
        let f = Path::new("/proj/src/lib.rs");
        record(f, "v0\n");
        let first = list(f).into_iter().next().unwrap();
        set_label(&first, Some("before refactor"));
        // The label is read back.
        let relisted = list(f);
        assert_eq!(relisted[0].label.as_deref(), Some("before refactor"));
        // Bury it under many more snapshots — labeled one must remain (immortal past the cap).
        for i in 1..(MAX_SNAPSHOTS + 20) {
            record(f, &format!("v{i}\n"));
        }
        let all = list(f);
        assert!(
            all.iter().any(|s| s.label.as_deref() == Some("before refactor")),
            "labeled snapshot must survive pruning"
        );
        // Clearing removes the label.
        let labeled = all.into_iter().find(|s| s.label.is_some()).unwrap();
        set_label(&labeled, None);
        assert!(list(f).iter().all(|s| s.label.is_none()));
        let _ = fs::remove_dir_all(store);
    }

    #[test]
    fn record_dedupes_identical_content() {
        let _g = env_lock().lock().unwrap();
        let store = temp_store("dedupe");
        let f = Path::new("/proj/src/main.rs");
        record(f, "hello\n");
        record(f, "hello\n"); // identical → no new file
        assert_eq!(list(f).len(), 1);
        record(f, "hello world\n");
        let snaps = list(f);
        assert_eq!(snaps.len(), 2);
        assert_eq!(read(&snaps[0]).unwrap(), "hello world\n");
        let _ = fs::remove_dir_all(store);
    }

    #[test]
    fn list_is_newest_first_and_meta_written() {
        let _g = env_lock().lock().unwrap();
        let store = temp_store("order");
        let f = Path::new("/proj/lib.rs");
        record(f, "v1");
        record(f, "v2");
        record(f, "v3");
        let snaps = list(f);
        assert_eq!(snaps.len(), 3);
        assert!(snaps[0].ts_millis > snaps[1].ts_millis);
        assert!(snaps[1].ts_millis > snaps[2].ts_millis);
        assert_eq!(read(&snaps[0]).unwrap(), "v3");
        let meta = dir_for(f).join("meta.json");
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(meta).unwrap()).unwrap();
        assert_eq!(json["path"], "/proj/lib.rs");
        let _ = fs::remove_dir_all(store);
    }

    #[test]
    fn prune_count_and_age() {
        let _g = env_lock().lock().unwrap();
        let store = temp_store("prune");
        let f = Path::new("/proj/big.rs");
        let dir = dir_for(f);
        fs::create_dir_all(&dir).unwrap();
        let now = now_millis();
        // 105 fresh snapshots → count cap keeps 100 newest.
        for i in 0..105u64 {
            fs::write(dir.join(format!("{}.snap", now - i * 1000)), format!("c{i}")).unwrap();
        }
        prune(&dir);
        let snaps = list(f);
        assert_eq!(snaps.len(), 100);
        assert_eq!(snaps[0].ts_millis, now); // newest survived

        // Age: 15 ancient snapshots → only the newest 10 survive despite age.
        let store2 = temp_store("prune-age");
        let f2 = Path::new("/proj/old.rs");
        let dir2 = dir_for(f2);
        fs::create_dir_all(&dir2).unwrap();
        let ancient = now - MAX_AGE_MILLIS - 1_000_000;
        for i in 0..15u64 {
            fs::write(dir2.join(format!("{}.snap", ancient - i * 1000)), "x").unwrap();
        }
        prune(&dir2);
        assert_eq!(list(f2).len(), KEEP_NEWEST);
        let _ = fs::remove_dir_all(store);
        let _ = fs::remove_dir_all(store2);
    }

    #[test]
    fn diff_add_remove_modify() {
        // add
        let d = diff("a\nb\n", "a\nb\nc\n");
        assert_eq!(
            d.iter().map(|l| (l.kind, l.text.as_str())).collect::<Vec<_>>(),
            vec![(DiffKind::Same, "a"), (DiffKind::Same, "b"), (DiffKind::Add, "c")]
        );
        // remove
        let d = diff("a\nb\nc\n", "a\nc\n");
        assert_eq!(
            d.iter().map(|l| (l.kind, l.text.as_str())).collect::<Vec<_>>(),
            vec![(DiffKind::Same, "a"), (DiffKind::Del, "b"), (DiffKind::Same, "c")]
        );
        // modify
        let d = diff("a\nb\nc\n", "a\nB\nc\n");
        let kinds: Vec<DiffKind> = d.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&DiffKind::Del) && kinds.contains(&DiffKind::Add));
        assert_eq!(kinds.iter().filter(|k| **k == DiffKind::Same).count(), 2);
        // identical
        assert!(diff("x\ny\n", "x\ny\n").iter().all(|l| l.kind == DiffKind::Same));
        // Reconstruction invariants: Same+Del == old lines, Same+Add == new lines.
        let d = diff("one\ntwo\nthree\n", "zero\ntwo\nfour\n");
        let olds: Vec<&str> = d
            .iter()
            .filter(|l| l.kind != DiffKind::Add)
            .map(|l| l.text.as_str())
            .collect();
        let news: Vec<&str> = d
            .iter()
            .filter(|l| l.kind != DiffKind::Del)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(olds, vec!["one", "two", "three"]);
        assert_eq!(news, vec!["zero", "two", "four"]);
    }

    #[test]
    fn diff_huge_falls_back() {
        // Disjoint contents: prefix/suffix trim removes nothing, so the middle really is
        // >20k lines a side and MUST take the fallback path (no O(n*m) blow-up).
        let old: String = (0..LCS_LINE_LIMIT + 5).map(|i| format!("l{i}\n")).collect();
        let new: String = (0..LCS_LINE_LIMIT + 5).map(|i| format!("r{i}\n")).collect();
        let d = diff(&old, &new);
        let dels = d.iter().filter(|l| l.kind == DiffKind::Del).count();
        let adds = d.iter().filter(|l| l.kind == DiffKind::Add).count();
        assert_eq!(dels, LCS_LINE_LIMIT + 5);
        assert_eq!(adds, LCS_LINE_LIMIT + 5);
        assert!(d.iter().all(|l| l.kind != DiffKind::Same));
    }

    #[test]
    fn diff_cell_cap_falls_back() {
        // Two disjoint ~6k-line sides: under the 20k line limit but over the 25M-cell cap.
        let n = 6_000usize;
        assert!((n + 1) * (n + 1) > LCS_CELL_LIMIT && n < LCS_LINE_LIMIT);
        let old: String = (0..n).map(|i| format!("a{i}\n")).collect();
        let new: String = (0..n).map(|i| format!("b{i}\n")).collect();
        let d = diff(&old, &new);
        assert_eq!(d.len(), 2 * n);
        assert!(d.iter().all(|l| l.kind != DiffKind::Same));
    }

    #[test]
    fn restore_round_trip() {
        let _g = env_lock().lock().unwrap();
        let store = temp_store("restore");
        let f = Path::new("/proj/rt.rs");
        let original = "fn main() {\n    println!(\"v1\");\n}\n";
        record(f, original);
        record(f, "fn main() {}\n");
        let snaps = list(f);
        assert_eq!(read(&snaps[1]).unwrap(), original); // oldest = original, byte-exact
        let _ = fs::remove_dir_all(store);
    }

    #[test]
    fn human_time_buckets() {
        let now = 1_752_000_000_000u64; // some 2025 date
        assert_eq!(human_time(now - 30_000, now), "just now");
        assert_eq!(human_time(now - 5 * 60_000, now), "5 min ago");
        let today = human_time(now - 5 * 3_600_000, now);
        assert!(today.len() == 5 && today.contains(':'), "{today}");
        let old = human_time(now - 40 * 86_400_000, now);
        assert!(old.chars().next().unwrap().is_ascii_alphabetic(), "{old}"); // "Jun 29 …"
    }
}
