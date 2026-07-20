//! GLOBAL (not per-project) preferences: the handful of things that follow you between projects
//! rather than belonging to one. Editor font size shouldn't depend on which repo you opened.
//!
//! One JSON at `~/.local/share/cauldron/settings.json`, beside the `sessions/` dir and the
//! `last-project` pointer.
//!
//! Everything ELSE about layout — panel widths, dock height, window geometry — is NOT here. egui
//! already tracks dragged panel sizes in its `Memory`, and eframe's `persistence` feature flushes
//! that (plus window geometry, via `persist_window`) to disk on its own. This file is only for
//! state egui has no concept of.
//!
//! This is a DTO, not the live struct: `App` owns LSP/DAP child processes, PTY handles, mpsc
//! channels and GPU textures, none of which are serializable. Same split `runconfig::StoreOnDisk`
//! already uses.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Standards;

/// The editor font's allowed range — shared with the Ctrl+± zoom handler, which clamps to the
/// same bounds.
pub const FONT_MIN: f32 = 8.0;
pub const FONT_MAX: f32 = 40.0;
pub const FONT_DEFAULT: f32 = 14.0;

fn default_font() -> f32 {
    FONT_DEFAULT
}

fn default_true() -> bool {
    true
}

/// Which backend answers AI requests (ghost text + assistant panel).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AiProvider {
    /// Anthropic Messages API via the Claude Code OAuth sign-in.
    #[default]
    Claude,
    /// A local Ollama server — fully offline, no sign-in.
    Ollama,
}

/// AI backend configuration. Lives in global settings: which model you run locally follows
/// you, not the project.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AiSettings {
    #[serde(default)]
    pub provider: AiProvider,
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,
    /// Chat/instruct model for the assistant panel (explain, tests, ask).
    #[serde(default = "default_ollama_chat")]
    pub ollama_chat_model: String,
    /// Base/FIM model for ghost-text completions (needs fill-in-the-middle support:
    /// qwen2.5-coder base, codellama:code, starcoder2, deepseek-coder base…).
    #[serde(default = "default_ollama_fim")]
    pub ollama_fim_model: String,
}

fn default_ollama_url() -> String {
    "http://localhost:11434".to_string()
}
fn default_ollama_chat() -> String {
    // qwen2.5-coder is purpose-built for code and spans 40+ languages. The 7B tier is the
    // sweet spot for refactoring/explain quality on a discrete GPU; drop to :3b on CPU-only
    // or low-VRAM machines. (Needs ollama-cuda on Arch/RuneOS to actually use the GPU —
    // the plain `ollama` package is CPU-only.)
    "qwen2.5-coder:7b".to_string()
}
fn default_ollama_fim() -> String {
    // Ghost text fires on every pause, so the FIM model favors LATENCY: the 1.5B base is
    // near-instant on a GPU and still usable on CPU. Its fill-in-the-middle template is what
    // makes suffix-aware completion work.
    "qwen2.5-coder:1.5b-base".to_string()
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            provider: AiProvider::default(),
            ollama_url: default_ollama_url(),
            ollama_chat_model: default_ollama_chat(),
            ollama_fim_model: default_ollama_fim(),
        }
    }
}

/// The persisted UI theme choice. `System` follows the OS light/dark preference (RuneOS only;
/// falls back to Dark elsewhere). The rest map directly to [`crate::style::Theme`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    Midnight,
    Amber,
    System,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    /// Editor font size (Ctrl+± zoom).
    #[serde(default = "default_font")]
    pub editor_font: f32,
    /// Coding-standards tier.
    #[serde(default)]
    pub standards: Standards,
    /// Auto-install a project's dependencies (cargo/npm/NuGet/pip/…) in the background on open.
    /// On by default — opening a project and finding it ready to build is the point. It runs
    /// package-manager install hooks (npm `postinstall`, etc.), i.e. code from the opened tree, so
    /// it is a switch the cautious can turn off; the Run menu still triggers it on demand. Missing
    /// from an older settings file → defaults to on.
    #[serde(default = "default_true")]
    pub auto_deps: bool,
    /// LSP inlay hints (types, parameter names) after each line's code.
    #[serde(default = "default_true")]
    pub inlay_hints: bool,
    /// Inline git blame (author · summary) on the caret line.
    #[serde(default = "default_true")]
    pub inline_blame: bool,
    /// UI theme (dark / light). Serialized as a string so an unknown value falls back to dark.
    #[serde(default)]
    pub theme: ThemeChoice,
    /// AI backend (Claude sign-in vs local Ollama).
    #[serde(default)]
    pub ai: AiSettings,
    /// Run clang-tidy inside clangd (C/C++ diagnostics beyond the compiler's own).
    #[serde(default = "default_true")]
    pub clang_tidy: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            editor_font: FONT_DEFAULT,
            standards: Standards::default(),
            auto_deps: true,
            inlay_hints: true,
            inline_blame: true,
            theme: ThemeChoice::Dark,
            ai: AiSettings::default(),
            clang_tidy: true,
        }
    }
}

impl Settings {
    /// Force the font back into the range the UI can actually recover from. A hand-edited or
    /// corrupt file must not be able to set a 3000pt (or 0pt, or NaN) font: the settings dialog
    /// is rendered WITH that font, so an out-of-range value would be unfixable from inside the
    /// app. NaN needs the explicit test — `clamp` passes a NaN *self* straight through rather than
    /// clamping it — while ±infinity clamps to the bounds on its own.
    fn sanitize(&mut self) {
        if self.editor_font.is_nan() {
            self.editor_font = FONT_DEFAULT;
        }
        self.editor_font = self.editor_font.clamp(FONT_MIN, FONT_MAX);
    }
}

fn settings_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share/cauldron/settings.json"))
}

/// Load global settings. Missing or corrupt → defaults; never fails, never blocks boot. Losing
/// your font size is an annoyance, failing to start the editor is not acceptable.
pub fn load() -> Settings {
    let mut s = settings_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .unwrap_or_default();
    s.sanitize();
    s
}

/// Persist global settings. Best-effort — a failed save never bothers the user.
pub fn save(s: &Settings) {
    let Some(file) = settings_file() else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(file, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize → deserialize keeps every field.
    #[test]
    fn round_trip() {
        let s = Settings {
            editor_font: 21.0,
            standards: Standards::JplPot,
            auto_deps: false,
            inlay_hints: false,
            inline_blame: false,
            theme: ThemeChoice::Light,
            ai: AiSettings {
                provider: AiProvider::Ollama,
                ollama_chat_model: "llama3:8b".into(),
                ..Default::default()
            },
            clang_tidy: false,
        };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    /// An older settings file predates `clang_tidy`; it must load and take the default rather
    /// than failing the WHOLE file (serde would otherwise reject the missing field).
    #[test]
    fn settings_file_without_clang_tidy_still_loads() {
        let s: Settings = serde_json::from_str(r#"{"editor_font": 14.0}"#).unwrap();
        assert!(s.clang_tidy, "missing field must default to on");
    }

    /// A file with no fields at all (or unknown ones) still loads, on defaults.
    #[test]
    fn empty_and_unknown_fields_fall_back_to_defaults() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        let s: Settings = serde_json::from_str(r#"{"who_is_this": 3}"#).unwrap();
        assert_eq!(s, Settings::default());
    }

    /// auto_deps defaults to ON — an older settings file predating the field must not silently
    /// DISABLE dependency install (serde's bool default is false; the `default = "default_true"`
    /// attribute is what prevents that).
    #[test]
    fn auto_deps_defaults_on_for_old_files() {
        let s: Settings = serde_json::from_str(r#"{"editor_font": 14.0}"#).unwrap();
        assert!(s.auto_deps, "a file without the field must default to enabled");
        assert!(Settings::default().auto_deps);
    }

    /// The settings dialog renders WITH editor_font, so a garbage value would be unfixable from
    /// inside the app. Out-of-range and non-finite values must be forced back into the UI's range.
    #[test]
    fn sanitize_clamps_hostile_font_sizes() {
        for (given, want) in
            [(3000.0, FONT_MAX), (0.0, FONT_MIN), (-5.0, FONT_MIN), (f32::NAN, FONT_DEFAULT), (f32::INFINITY, FONT_MAX)]
        {
            let mut s = Settings { editor_font: given, ..Default::default() };
            s.sanitize();
            assert_eq!(s.editor_font, want, "font {given} should sanitize to {want}");
        }
        // An in-range value is left alone.
        let mut s = Settings { editor_font: 17.0, ..Default::default() };
        s.sanitize();
        assert_eq!(s.editor_font, 17.0);
    }

    /// Corrupt JSON on disk → defaults, not a panic and not a failed boot.
    #[test]
    fn corrupt_file_loads_as_defaults() {
        let dir = std::env::temp_dir().join(format!("cauldron-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".local/share/cauldron")).unwrap();
        let _home = crate::testenv::HomeGuard::set(&dir);

        std::fs::write(dir.join(".local/share/cauldron/settings.json"), "{not json at all").unwrap();
        assert_eq!(load(), Settings::default());

        // And a real round-trip through the real path, including the clamp on the way back in.
        save(&Settings {
            editor_font: 9999.0,
            standards: Standards::JplPot,
            auto_deps: true,
            inlay_hints: true,
            inline_blame: true,
            theme: ThemeChoice::Dark,
            ai: AiSettings::default(),
            clang_tidy: true,
        });
        let back = load();
        assert_eq!(back.editor_font, FONT_MAX, "hostile value clamped on load");
        assert_eq!(back.standards, Standards::JplPot);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
