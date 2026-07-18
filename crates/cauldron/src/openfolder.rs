//! The picker overlay — three modes over one browsing UI:
//! - **Open Project** (Ctrl+Shift+O): choose a directory → workspace root; recents listed first.
//! - **Open File** (Ctrl+O): a proper file picker — browse directories, pick any file.
//! - **New Project** (Ctrl+Shift+N): type a path, pick a template (Empty+git / Cargo bin /
//!   Cargo lib / C flight-style), create + open.
//!
//! Navigation: type a path (`~` expands, live suggestions), Tab completes, ↑/↓ pick, Enter
//! opens/descends, Esc closes. Recents persist at `~/.local/share/cauldron/recent-projects`.

use std::path::{Path, PathBuf};

use egui::{Color32, Key};

use crate::style::{colors, sizes};

const MAX_RECENTS: usize = 15;
const MAX_SUGGESTIONS: usize = 24;
/// Template cards per row in the New Project grid (the overlay is 620px wide).
const TEMPLATE_COLS: usize = 3;
fn text() -> Color32 { colors::TEXT() }
fn dim() -> Color32 { colors::TEXT_FAINT() }

fn recents_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share/cauldron/recent-projects"))
}

/// Load the recent-projects list (missing file → empty; dead + unworthy paths filtered on load).
pub fn load_recents() -> Vec<PathBuf> {
    let Some(file) = recents_file() else { return Vec::new() };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    parse_recents(&std::fs::read_to_string(file).unwrap_or_default(), home.as_deref())
}

/// PURE parse + filter of the recents file body: dead paths dropped, and `$HOME` / `/` /
/// ancestors of `$HOME` never resurface in the picker (a stale `/home/user` line written by
/// a pre-guard build must not be re-offered as a "project"). Relative lines are dropped too:
/// `.` is a dir relative to ANY cwd and would open whatever directory the process happens to
/// sit in ($HOME on a dock launch).
fn parse_recents(body: &str, home: Option<&Path>) -> Vec<PathBuf> {
    body.lines()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir() && crate::state::project_worthy(p, home))
        .take(MAX_RECENTS)
        .collect()
}

/// Push `root` to the top of the recents (dedup + cap) and persist. Best-effort.
/// `$HOME`, `/`, ancestors of `$HOME`, and relative paths are never recorded — they are
/// not projects (roots arrive canonicalized from `App::new`; this is the backstop).
pub fn record_recent(root: &Path) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if !root.is_absolute() || !crate::state::project_worthy(root, home.as_deref()) {
        return;
    }
    let mut recents = load_recents();
    recents.retain(|p| p != root);
    recents.insert(0, root.to_path_buf());
    recents.truncate(MAX_RECENTS);
    if let Some(file) = recents_file() {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body: String = recents.iter().map(|p| format!("{}\n", p.display())).collect();
        let _ = std::fs::write(file, body);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Project,
    File,
    NewProject,
}

/// What the picker resolved to this frame.
pub enum PickAction {
    OpenProject(PathBuf),
    OpenFile(PathBuf),
}

use crate::newproject::{create_project, Template, TEMPLATES};

pub struct OpenFolder {
    open: bool,
    mode: Mode,
    input: String,
    recents: Vec<PathBuf>,
    just_opened: bool,
    selected: usize,
    template: Template,
    create_error: Option<String>,
}

impl Default for OpenFolder {
    fn default() -> Self {
        Self {
            open: false,
            mode: Mode::Project,
            input: String::new(),
            recents: Vec::new(),
            just_opened: false,
            selected: 0,
            template: Template::CargoBin,
            create_error: None,
        }
    }
}

impl OpenFolder {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self) {
        self.open_mode(Mode::Project, "");
    }

    /// Ctrl+O: file-picker mode, starting in `start_dir`.
    pub fn open_file_mode(&mut self, start_dir: &Path) {
        let seed = format!("{}/", start_dir.display());
        self.open_mode(Mode::File, &seed);
    }

    /// Ctrl+Shift+N: new-project mode.
    pub fn open_new_project(&mut self) {
        self.open_mode(Mode::NewProject, "~/RustroverProjects/");
    }

    fn open_mode(&mut self, mode: Mode, seed: &str) {
        self.open = true;
        self.mode = mode;
        self.just_opened = true;
        self.input = seed.to_string();
        self.recents = load_recents();
        self.selected = 0;
        self.create_error = None;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Candidates for the current input: recents when empty (Project mode), else entries of the
    /// deepest existing directory — dirs always; files too in File mode.
    fn candidates(&self) -> Vec<PathBuf> {
        if self.input.trim().is_empty() && self.mode == Mode::Project {
            return self.recents.clone();
        }
        let typed = expand_home(self.input.trim());
        let with_files = self.mode == Mode::File;
        if typed.is_dir() && self.input.ends_with('/') {
            return list_entries(&typed, with_files);
        }
        let parent = typed.parent().filter(|p| p.is_dir()).map(PathBuf::from);
        let needle =
            typed.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        match parent {
            Some(parent) => list_entries(&parent, with_files)
                .into_iter()
                .filter(|d| {
                    d.file_name()
                        .map(|n| n.to_string_lossy().to_lowercase().starts_with(&needle))
                        .unwrap_or(false)
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Draw the overlay if open. Returns the resolved action the frame something is chosen.
    pub fn ui(&mut self, ctx: &egui::Context) -> Option<PickAction> {
        if !self.open {
            return None;
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.close();
            return None;
        }
        let mut action: Option<PickAction> = None;
        let mut descend: Option<PathBuf> = None;

        let title = match self.mode {
            Mode::Project => "Open Project",
            Mode::File => "Open File",
            Mode::NewProject => "New Project",
        };
        let hint = match self.mode {
            Mode::Project => "~/path/to/project  (Enter opens · Tab completes)",
            Mode::File => "~/path/to/file  (Enter opens · Tab completes · Enter on a dir descends)",
            Mode::NewProject => "~/path/for/new-project",
        };

        egui::Area::new("openfolder".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(620.0);
                        crate::style::panel_header_inline(ui, title);

                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.input)
                                .hint_text(hint)
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                        if self.just_opened {
                            resp.request_focus();
                            self.just_opened = false;
                        }
                        if resp.changed() {
                            self.selected = 0;
                        }

                        // --- New Project: template grid + create button --------------------
                        if self.mode == Mode::NewProject {
                            ui.add_space(6.0);
                            // Three columns of selectable cards; the hint is the second line so a
                            // template says what it lands without the user having to create one.
                            egui::Grid::new("newproject-templates")
                                .num_columns(TEMPLATE_COLS)
                                .spacing([6.0, 4.0])
                                .show(ui, |ui| {
                                    for (n, (t, label, hint)) in TEMPLATES.iter().enumerate() {
                                        let mut job = egui::text::LayoutJob::default();
                                        job.append(
                                            label,
                                            0.0,
                                            egui::TextFormat {
                                                font_id: egui::TextStyle::Body.resolve(ui.style()),
                                                color: if self.template == *t {
                                                    colors::ACCENT_HI()
                                                } else {
                                                    text()
                                                },
                                                ..Default::default()
                                            },
                                        );
                                        job.append(
                                            &format!("\n{hint}"),
                                            0.0,
                                            egui::TextFormat {
                                                font_id: egui::TextStyle::Small.resolve(ui.style()),
                                                color: dim(),
                                                ..Default::default()
                                            },
                                        );
                                        if ui
                                            .add_sized(
                                                [190.0, 34.0],
                                                egui::SelectableLabel::new(self.template == *t, job),
                                            )
                                            .clicked_by(egui::PointerButton::Primary)
                                        {
                                            self.template = *t;
                                        }
                                        if (n + 1) % TEMPLATE_COLS == 0 {
                                            ui.end_row();
                                        }
                                    }
                                });
                            ui.add_space(6.0);
                            let target = expand_home(self.input.trim());
                            let creatable = !self.input.trim().is_empty() && !target.exists();
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(creatable, egui::Button::new("Create project"))
                                    .clicked_by(egui::PointerButton::Primary)
                                    || (creatable && ui.input(|i| i.key_pressed(Key::Enter)))
                                {
                                    match create_project(&target, self.template) {
                                        Ok(()) => {
                                            action = Some(PickAction::OpenProject(target.clone()))
                                        }
                                        Err(e) => self.create_error = Some(e),
                                    }
                                }
                                if target.exists() && !self.input.trim().is_empty() {
                                    ui.colored_label(colors::WARN(), "path already exists");
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.colored_label(dim(), "every project ships a .venv");
                                    },
                                );
                            });
                            if let Some(e) = &self.create_error {
                                ui.colored_label(colors::ERROR(), e);
                            }
                            return; // no suggestion list in create mode
                        }

                        let candidates = self.candidates();
                        let shown = candidates.len().min(MAX_SUGGESTIONS);
                        if shown > 0 {
                            if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                                self.selected = (self.selected + 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                                self.selected = (self.selected + shown - 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::Tab)) {
                                let c = &candidates[self.selected.min(shown - 1)];
                                self.input = if c.is_dir() {
                                    format!("{}/", c.display())
                                } else {
                                    c.display().to_string()
                                };
                            }
                        }
                        if ui.input(|i| i.key_pressed(Key::Enter)) {
                            let typed = expand_home(self.input.trim());
                            let target = if !self.input.trim().is_empty()
                                && (typed.is_dir() || typed.is_file())
                            {
                                Some(typed)
                            } else if shown > 0 {
                                Some(candidates[self.selected.min(shown - 1)].clone())
                            } else {
                                None
                            };
                            if let Some(t) = target {
                                match self.mode {
                                    Mode::Project if t.is_dir() => {
                                        action = Some(PickAction::OpenProject(t));
                                    }
                                    Mode::File if t.is_file() => {
                                        action = Some(PickAction::OpenFile(t));
                                    }
                                    Mode::File if t.is_dir() => descend = Some(t),
                                    _ => {}
                                }
                            }
                        }

                        ui.add_space(4.0);
                        crate::style::hairline(ui);
                        if self.input.trim().is_empty() && self.mode == Mode::Project {
                            ui.colored_label(dim(), "recent projects");
                        }
                        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            for (row, entry) in candidates.iter().take(MAX_SUGGESTIONS).enumerate()
                            {
                                let sel = row == self.selected;
                                let is_dir = entry.is_dir();
                                let name = entry
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                let shown_name =
                                    if is_dir { format!("{name}/") } else { name.clone() };
                                let parent =
                                    entry.parent().map(|p| p.to_string_lossy()).unwrap_or_default();
                                let mut job = egui::text::LayoutJob::default();
                                let font = egui::TextStyle::Monospace.resolve(ui.style());
                                job.append(
                                    &shown_name,
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font.clone(),
                                        color: if sel {
                                            colors::ACCENT_HI()
                                        } else if is_dir {
                                            colors::AMBER()
                                        } else {
                                            text()
                                        },
                                        ..Default::default()
                                    },
                                );
                                job.append(
                                    &format!("  {parent}"),
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font,
                                        color: dim(),
                                        ..Default::default()
                                    },
                                );
                                if ui.selectable_label(sel, job).clicked_by(egui::PointerButton::Primary) {
                                    match self.mode {
                                        Mode::File if entry.is_file() => {
                                            action =
                                                Some(PickAction::OpenFile(entry.clone()));
                                        }
                                        Mode::File => descend = Some(entry.clone()),
                                        _ => action = Some(PickAction::OpenProject(entry.clone())),
                                    }
                                }
                            }
                        });
                    });
            });

        if let Some(dir) = descend {
            self.input = format!("{}/", dir.display());
            self.selected = 0;
        }
        if action.is_some() {
            self.close();
        }
        action
    }
}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

/// Entries of `dir`: non-hidden subdirectories (always) and files (when `with_files`), dirs
/// first, each group sorted case-insensitively.
fn list_entries(dir: &Path, with_files: bool) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let hidden = p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(true);
        if hidden {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => dirs.push(p),
            Ok(t) if t.is_file() && with_files => files.push(p),
            _ => {}
        }
    }
    let key = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
    dirs.sort_by_key(key);
    files.sort_by_key(key);
    dirs.extend(files);
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recents_filters_home_root_and_dead_paths() {
        // Fixture: a real "project" dir and a pretend $HOME that ALSO really exists — the
        // stale `/home/user` line problem is that it IS a dir, so is_dir() alone keeps it.
        let base = std::env::temp_dir().join(format!("cauldron-recents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home/user");
        let proj = base.join("home/user/proj");
        std::fs::create_dir_all(&proj).unwrap();
        let body = format!(
            "{home}\n/\n{parent}\n{proj}\n{dead}\n",
            home = home.display(),
            parent = base.join("home").display(), // ancestor of $HOME — also a real dir
            proj = proj.display(),
            dead = base.join("gone").display(),
        );
        let got = parse_recents(&body, Some(&home));
        assert_eq!(got, vec![proj], "only the real project survives");
        // Without a HOME the existing dirs are kept, / still rejected.
        let got = parse_recents(&body, None);
        assert_eq!(got.len(), 3, "home-shaped dirs pass when HOME is unknown: {got:?}");
        // Relative lines never resurface: "." IS a dir relative to any cwd, but a recents
        // entry re-resolved against a later process's cwd is exactly the last-project
        // poison (a dock launch would open $HOME).
        let got = parse_recents(".\nsrc\n./proj\n", None);
        assert!(got.is_empty(), "relative recents must be dropped: {got:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expand_home_works() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_home("~/x"), PathBuf::from(format!("{home}/x")));
        assert_eq!(expand_home("/abs/x"), PathBuf::from("/abs/x"));
    }

    #[test]
    fn list_entries_dirs_first_files_optional() {
        let dir = std::env::temp_dir().join(format!("cauldron-of2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("beta")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let dirs_only = list_entries(&dir, false);
        assert_eq!(dirs_only.len(), 1);
        let with_files = list_entries(&dir, true);
        let names: Vec<String> = with_files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["beta", "a.txt"], "dirs first, hidden skipped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Template creation itself is covered in `newproject`, which tests every template.
}
