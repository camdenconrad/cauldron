//! Ctrl+Shift+P command palette — JetBrains "Find Action" / VS Code command palette.
//!
//! A searchable list of every command in the app, fuzzy-matched by `nucleo-matcher` (the same
//! engine as quick-open), so any action is reachable — and DISCOVERABLE — without hunting through
//! menus or memorizing a shortcut. The palette itself only chooses a [`Command`]; the app's
//! `run_command` does the work, so there is exactly one place that knows how to perform each action.

use egui::{Color32, Key};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

const ORANGE: Color32 = Color32::from_rgb(233, 110, 44);
const TEXT: Color32 = Color32::from_rgb(238, 235, 232);
const DIM: Color32 = Color32::from_rgb(150, 145, 140);
const MAX_RESULTS: usize = 40;

/// Every action reachable from the palette. Adding one here + a row in [`COMMANDS`] + an arm in the
/// app's `run_command` is all it takes — the fuzzy search and rendering are automatic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    // File / project
    QuickOpenFile,
    RecentFiles,
    RecentLocations,
    OpenFile,
    OpenProject,
    NewProject,
    SaveFile,
    SearchInFiles,
    // Editing
    FormatFile,
    CommentLines,
    CommentBlock,
    MoveLineUp,
    MoveLineDown,
    JoinLines,
    JumpMatchingBracket,
    FoldRegion,
    ToggleWrap,
    // Language / navigation
    GoToDefinition,
    GoToImplementation,
    FindUsages,
    CallHierarchy,
    RenameSymbol,
    QuickFix,
    GoToLine,
    ToggleBookmark,
    ShowBookmarks,
    GoToFileSymbol,
    NavBack,
    NavForward,
    ShowDiff,
    ToggleBlame,
    ShowHistory,
    ShowPullRequests,
    RunCoverage,
    ClearCoverage,
    // Run / debug / deps
    Run,
    RunCurrentFile,
    Build,
    StopRun,
    InstallDependencies,
    // View
    ToggleTerminal,
    SwitchHeaderSource,
    ExtractVariable,
    ExtractFunction,
    CreateFunctionFromUsage,
    CompleteStatement,
    HighlightUsagesInFile,
    LastEditLocation,
    RevealInProject,
    ExpandSelection,
    ShrinkSelection,
    SortLines,
    DeleteLine,
    ToggleCase,
    ToggleProjectPanel,
    MarkdownPreview,
    WebPreview,
    RunWebDevServer,
    ResolveConflicts,
    SplitRight,
    Settings,
}

/// `(command, label, keybind hint)` — the palette's rows, in a sensible default order (shown when
/// the query is empty). The label is what the fuzzy match runs against.
pub const COMMANDS: &[(Command, &str, &str)] = &[
    (Command::QuickOpenFile, "Go to File", "Ctrl+P"),
    (Command::RecentFiles, "Recent Files", "Ctrl+E"),
    (Command::RecentLocations, "Recent Locations", "Ctrl+Shift+E"),
    (Command::SearchInFiles, "Find in Files", "Ctrl+Shift+F"),
    (Command::GoToDefinition, "Go to Definition", "Ctrl+B"),
    (Command::GoToImplementation, "Go to Implementation", "Ctrl+Alt+B"),
    (Command::FindUsages, "Find Usages", "Alt+F7"),
    (Command::CallHierarchy, "Call Hierarchy (callers)", "Ctrl+Alt+H"),
    (Command::RenameSymbol, "Rename Symbol", "Shift+F6"),
    (Command::QuickFix, "Quick Fix", "Alt+Enter"),
    (Command::GoToLine, "Go to Line", "Ctrl+G"),
    (Command::ToggleBookmark, "Toggle Bookmark", "F11"),
    (Command::ShowBookmarks, "Show Bookmarks", "Shift+F11"),
    (Command::GoToFileSymbol, "Go to Symbol in File", "Ctrl+F12"),
    (Command::NavBack, "Navigate Back", "Alt+Left"),
    (Command::NavForward, "Navigate Forward", "Alt+Right"),
    (Command::ShowDiff, "Show Diff (vs HEAD)", ""),
    (Command::ToggleBlame, "Toggle Inline Blame", ""),
    (Command::ShowHistory, "Show Git History", ""),
    (Command::ShowPullRequests, "Show Pull Requests", ""),
    (Command::RunCoverage, "Run Tests with Coverage", ""),
    (Command::ClearCoverage, "Clear Coverage Marks", ""),
    (Command::FormatFile, "Reformat File", "Ctrl+Alt+L"),
    (Command::CommentLines, "Comment Lines", "Ctrl+/"),
    (Command::CommentBlock, "Comment Block", "Ctrl+Shift+/"),
    (Command::MoveLineUp, "Move Line Up", "Alt+Shift+Up"),
    (Command::MoveLineDown, "Move Line Down", "Alt+Shift+Down"),
    (Command::JoinLines, "Join Lines", "Ctrl+Shift+J"),
    (Command::JumpMatchingBracket, "Jump to Matching Bracket", "Ctrl+Shift+\\"),
    (Command::FoldRegion, "Fold / Unfold Region", "Ctrl+."),
    (Command::ToggleWrap, "Toggle Soft Wrap", "Alt+Z"),
    (Command::Run, "Run", "Shift+F10"),
    (Command::RunCurrentFile, "Run Current File", "Ctrl+Shift+F10"),
    (Command::Build, "Build", "Ctrl+F9"),
    (Command::StopRun, "Stop", "Ctrl+F2"),
    (Command::InstallDependencies, "Install Dependencies", ""),
    (Command::ToggleTerminal, "Toggle Terminal", "Alt+F12"),
    (Command::SwitchHeaderSource, "Switch Header/Source", "Ctrl+Alt+Home"),
    (Command::ExtractVariable, "Extract Variable", "Ctrl+Alt+V"),
    (Command::ExtractFunction, "Extract Function", "Ctrl+Alt+M"),
    (Command::CreateFunctionFromUsage, "Create Function from Usage", ""),
    (Command::CompleteStatement, "Complete Current Statement", "Ctrl+Shift+Enter"),
    (Command::HighlightUsagesInFile, "Highlight Usages in File", "Ctrl+Shift+F7"),
    (Command::LastEditLocation, "Last Edit Location", "Ctrl+Shift+Backspace"),
    (Command::RevealInProject, "Select in Project View", "Alt+F1"),
    (Command::ExpandSelection, "Extend Selection", "Ctrl+W"),
    (Command::ShrinkSelection, "Shrink Selection", "Ctrl+Shift+W"),
    (Command::SortLines, "Sort Lines", ""),
    (Command::DeleteLine, "Delete Line", "Ctrl+Y"),
    (Command::ToggleCase, "Toggle Case", "Ctrl+Shift+U"),
    (Command::ToggleProjectPanel, "Toggle Project Panel", "Alt+1"),
    (Command::MarkdownPreview, "Toggle Markdown Preview", ""),
    (Command::WebPreview, "Open Web Preview in Browser (live)", ""),
    (Command::RunWebDevServer, "Run Web Dev Server (npm/pnpm/yarn/bun)", ""),
    (Command::ResolveConflicts, "Resolve Merge Conflicts (current file)", ""),
    (Command::SplitRight, "Split Editor Right", "Ctrl+\\"),
    (Command::SaveFile, "Save File", "Ctrl+S"),
    (Command::OpenFile, "Open File…", "Ctrl+O"),
    (Command::OpenProject, "Open Project…", "Ctrl+Shift+O"),
    (Command::NewProject, "New Project…", "Ctrl+Shift+N"),
    (Command::Settings, "Settings", "Ctrl+Alt+S"),
];

pub struct CommandPalette {
    open: bool,
    query: String,
    just_opened: bool,
    matcher: Matcher,
    /// Indices into [`COMMANDS`], best-first for the current query.
    results: Vec<usize>,
    results_for: Option<String>,
    selected: usize,
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            just_opened: false,
            matcher: Matcher::new(Config::DEFAULT),
            results: Vec::new(),
            results_for: None,
            selected: 0,
        }
    }
}

impl CommandPalette {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self) {
        self.open = true;
        self.just_opened = true;
        self.query.clear();
        self.results_for = None;
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Draw the palette if open. Returns the chosen [`Command`] the frame one is picked (which also
    /// closes it). Esc dismisses.
    pub fn ui(&mut self, ctx: &egui::Context) -> Option<Command> {
        if !self.open {
            return None;
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.close();
            return None;
        }
        let mut chosen: Option<Command> = None;

        egui::Area::new("command-palette".into())
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(crate::style::sizes::OVERLAY_PAD))
                    .show(ui, |ui| {
                        ui.set_width(560.0);
                        let edit = egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Type an action…")
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace);
                        let resp = ui.add(edit);
                        if self.just_opened {
                            resp.request_focus();
                            self.just_opened = false;
                        }
                        self.recompute_if_changed();

                        let shown = self.results.len().min(MAX_RESULTS);
                        if shown > 0 {
                            if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                                self.selected = (self.selected + 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                                self.selected = (self.selected + shown - 1) % shown;
                            }
                            if ui.input(|i| i.key_pressed(Key::Enter)) {
                                chosen = Some(COMMANDS[self.results[self.selected]].0);
                            }
                        }

                        ui.separator();
                        egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                            for (row, &idx) in self.results.iter().take(MAX_RESULTS).enumerate() {
                                let (_, label, hint) = COMMANDS[idx];
                                let selected = row == self.selected;
                                ui.horizontal(|ui| {
                                    let color = if selected { ORANGE } else { TEXT };
                                    if ui.selectable_label(selected, egui::RichText::new(label).color(color)).clicked_by(egui::PointerButton::Primary) {
                                        chosen = Some(COMMANDS[idx].0);
                                    }
                                    if !hint.is_empty() {
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(egui::RichText::new(hint).color(DIM).monospace());
                                        });
                                    }
                                });
                            }
                            if self.results.is_empty() {
                                ui.colored_label(DIM, "no matching action");
                            }
                        });
                    });
            });

        if chosen.is_some() {
            self.close();
        }
        chosen
    }

    fn recompute_if_changed(&mut self) {
        if self.results_for.as_deref() == Some(self.query.as_str()) {
            return;
        }
        self.results_for = Some(self.query.clone());
        self.selected = 0;
        if self.query.is_empty() {
            self.results = (0..COMMANDS.len()).collect();
            return;
        }
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, usize)> = COMMANDS
            .iter()
            .enumerate()
            .filter_map(|(i, (_, label, _))| {
                let hay = Utf32Str::new(label, &mut buf);
                pattern.score(hay, &mut self.matcher).map(|s| (s, i))
            })
            .collect();
        // Best score first; ties keep the declared order (lower index).
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.results = scored.into_iter().map(|(_, i)| i).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every declared command has a unique label — a duplicate would make the palette ambiguous.
    #[test]
    fn command_labels_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (_, label, _) in COMMANDS {
            assert!(seen.insert(*label), "duplicate palette label: {label}");
        }
    }

    fn palette_with(query: &str) -> CommandPalette {
        CommandPalette { query: query.to_string(), ..Default::default() }
    }

    #[test]
    fn empty_query_lists_every_command() {
        let mut p = palette_with("");
        p.recompute_if_changed();
        assert_eq!(p.results.len(), COMMANDS.len());
    }

    #[test]
    fn fuzzy_finds_by_subsequence_and_abbreviation() {
        // "rcf" should surface "Run Current File".
        let mut p = palette_with("rcf");
        p.recompute_if_changed();
        assert!(!p.results.is_empty(), "abbreviation matched nothing");
        let (cmd, _, _) = COMMANDS[p.results[0]];
        assert_eq!(cmd, Command::RunCurrentFile);

        // "comment" surfaces the comment commands.
        let mut p = palette_with("comment");
        p.recompute_if_changed();
        let top: Vec<Command> = p.results.iter().map(|&i| COMMANDS[i].0).take(2).collect();
        assert!(top.contains(&Command::CommentLines), "comment query: {top:?}");
    }

    #[test]
    fn nonsense_query_matches_nothing() {
        let mut p = palette_with("zzzxqqq");
        p.recompute_if_changed();
        assert!(p.results.is_empty());
    }
}
