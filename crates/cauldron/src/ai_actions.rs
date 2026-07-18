//! AI actions beyond ghost text — a chat-style panel for "explain this diagnostic",
//! "explain this selection", "write a unit test", and freeform questions about code.
//!
//! Threading follows the cider PTY template used everywhere in this crate:
//! std::thread + std::sync::mpsc + egui::Context::request_repaint. One background
//! thread per request; the panel drains replies in [`AiPanel::pump`].
//!
//! Network goes through [`crate::ai::ask`] — the shared Claude OAuth path. The system
//! prompt is fixed (OAuth requirement); prompt building is pure and unit-tested here.

#![allow(dead_code)]

use std::sync::mpsc::{Receiver, Sender};

use crate::style::{self, colors};

/// OAuth requirement — must be exactly this string.
const SYSTEM: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
/// Quality over latency for explicit user-invoked actions.
const MODEL: &str = "claude-sonnet-5";
const MAX_TOKENS: u32 = 1500;

/// Which action the user invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiTaskKind {
    ExplainDiagnostic,
    /// "Hot fix": refactor the flagged code so the violation (NASA GSFC/JPL standard, or any
    /// diagnostic) goes away — the reply's code block is Apply-able over the flagged span.
    FixDiagnostic,
    ExplainSelection,
    WriteUnitTest,
    Ask,
}

/// Where a submitted selection came from, so a code block in the reply can be APPLIED back
/// over it. `generation` is the buffer's edit counter at submit time — apply only fires
/// against an unchanged buffer (the app falls back to insert-at-caret otherwise).
#[derive(Debug, Clone)]
pub struct Origin {
    pub path: std::path::PathBuf,
    pub range: std::ops::Range<usize>,
    pub generation: u64,
}

/// Everything the prompt builder needs about the code under discussion.
#[derive(Debug, Clone, Default)]
pub struct AiContext {
    pub file_name: String,
    /// Language name or extension ("rust", "c", "py", …). Drives test-style selection.
    pub language: String,
    /// The selection, or the enclosing context around the caret.
    pub code: String,
    /// The diagnostic message (for [`AiTaskKind::ExplainDiagnostic`]).
    pub diagnostic: Option<String>,
    /// Freeform question text (for [`AiTaskKind::Ask`]).
    pub extra: String,
    /// Selection provenance enabling Apply on reply code blocks (not part of the prompt).
    pub origin: Option<Origin>,
}

/// What the user chose to do with a reply code block.
pub enum PanelAction {
    /// Insert at the focused editor's caret.
    Insert(String),
    /// Replace the originating selection (generation-guarded by the app).
    Apply(String, Origin),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// Worker → panel wire format: replies arrive as a stream of deltas (token-by-token on the
/// local backend, one big delta on Claude) closed by Done.
enum AiMsg {
    Delta(String),
    /// `false` = the request failed with nothing (or nothing useful) streamed.
    Done(bool),
}

/// The chat panel: scrollback of turns + input box, one in-flight request at a time.
pub struct AiPanel {
    conversation: Vec<(Role, String)>,
    tx: Sender<AiMsg>,
    rx: Receiver<AiMsg>,
    in_flight: bool,
    /// The freeform input box contents.
    pub input: String,
    /// Selection provenance of the most recent submit (drives the Apply button).
    origin: Option<Origin>,
    /// Context auto-attached to freeform questions: the app refreshes this while the panel
    /// is visible (current file's selection or around-caret region + top diagnostics), so
    /// "why is this wrong?" just works — JetBrains AI chat behavior.
    pub ask_context: AiContext,
    /// Configured backend usable? (Same check the ghost-text completer makes.)
    pub available: bool,
}

impl Default for AiPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl AiPanel {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let available = crate::ai::backend_available();
        Self {
            conversation: Vec::new(),
            tx,
            rx,
            in_flight: false,
            input: String::new(),
            origin: None,
            ask_context: AiContext::default(),
            available,
        }
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Kick off one action. The built prompt is appended to the scrollback as the user
    /// turn and a background thread performs the request (cider template).
    pub fn submit(&mut self, kind: AiTaskKind, context: AiContext, ctx: &egui::Context) {
        if self.in_flight {
            return;
        }
        self.origin = context.origin.clone();
        let prompt = build_prompt(kind, &context);
        self.conversation.push((Role::User, prompt.clone()));
        // The reply bubble exists from the start; deltas stream into it as they arrive.
        self.conversation.push((Role::Assistant, String::new()));
        self.in_flight = true;
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("cauldron-ai-action".into())
            .spawn(move || {
                let reply = crate::ai::ask_stream(SYSTEM, &prompt, MODEL, MAX_TOKENS, None, &mut |d| {
                    let _ = tx.send(AiMsg::Delta(d.to_string()));
                    ctx2.request_repaint();
                });
                let _ = tx.send(AiMsg::Done(reply.is_some()));
                ctx2.request_repaint();
            });
        if spawned.is_err() {
            // No worker will ever reply — don't brick the panel with a stuck spinner.
            if let Some((Role::Assistant, text)) = self.conversation.last_mut() {
                *text = "(could not start the request thread)".into();
            }
            self.in_flight = false;
        }
    }

    /// Drain streamed deltas into the reply bubble. Call once per frame (ui() calls it too).
    pub fn pump(&mut self) {
        for msg in self.rx.try_iter().collect::<Vec<_>>() {
            match msg {
                AiMsg::Delta(d) => {
                    if let Some((Role::Assistant, text)) = self.conversation.last_mut() {
                        text.push_str(&d);
                    }
                }
                AiMsg::Done(ok) => {
                    self.in_flight = false;
                    if let Some((Role::Assistant, text)) = self.conversation.last_mut() {
                        if !ok && text.trim().is_empty() {
                            *text = "(request failed — see log; sign-in, network, or model access)".into();
                        }
                    }
                }
            }
        }
    }

    /// The chat panel body. Returns the chosen [`PanelAction`] for a fenced code block —
    /// the integrator inserts at the caret or applies over the originating selection.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<PanelAction> {
        self.pump();
        let mut insert: Option<PanelAction> = None;

        // (The host panel provides the "AI Assistant" title; only in-panel controls here.)
        ui.horizontal(|ui| {
            if self.in_flight {
                ui.spinner();
                ui.colored_label(colors::TEXT_FAINT(), "thinking…");
            }
            if !self.conversation.is_empty() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if style::tool_button(ui, "clear", false).clicked_by(egui::PointerButton::Primary) {
                        self.conversation.clear();
                    }
                });
            }
        });

        // Input row pinned to the bottom; scrollback fills the rest.
        let send = egui::TopBottomPanel::bottom(ui.id().with("ai-input"))
            .frame(egui::Frame::none().fill(colors::BG_PANEL()).inner_margin(egui::Margin::same(6.0)))
            .show_inside(ui, |ui| {
                let mut send = false;
                // Width snapshot taken BEFORE laying out the row: feeding available_width back
                // into desired_width inside the row makes the panel creep wider on repaint.
                let input_w = (ui.max_rect().width() - 72.0).max(80.0);
                ui.horizontal(|ui| {
                    let edit = egui::TextEdit::singleline(&mut self.input)
                        .hint_text("Ask about the current file…")
                        .desired_width(input_w);
                    let resp = ui.add(edit);
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        send = true;
                    }
                    if ui.add_enabled(!self.in_flight, egui::Button::new("Send")).clicked_by(egui::PointerButton::Primary) {
                        send = true;
                    }
                });
                send
            })
            .inner;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for (i, (role, text)) in self.conversation.iter().enumerate() {
                    match role {
                        Role::User => {
                            // User chip, right-ish: muted pill, truncated to the headline.
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                                ui.add_space(8.0);
                                let head = first_line(text);
                                egui::Frame::none()
                                    .fill(colors::BG_RAISED())
                                    .rounding(egui::Rounding::same(6.0))
                                    .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                                    .show(ui, |ui| {
                                        ui.label(egui::RichText::new(head).color(colors::TEXT_MUTED()));
                                    })
                                    .response
                                    .on_hover_text(text);
                            });
                        }
                        Role::Assistant => {
                            for (j, seg) in split_fenced(text).into_iter().enumerate() {
                                match seg {
                                    Segment::Text(t) => {
                                        let t = t.trim();
                                        if !t.is_empty() {
                                            ui.add(egui::Label::new(
                                                egui::RichText::new(t).color(colors::TEXT()),
                                            ).wrap());
                                        }
                                    }
                                    Segment::Code(code) => {
                                        egui::Frame::none()
                                            .fill(colors::BG_INPUT())
                                            .rounding(egui::Rounding::same(4.0))
                                            .inner_margin(egui::Margin::same(6.0))
                                            .show(ui, |ui| {
                                                ui.add(egui::Label::new(
                                                    egui::RichText::new(code.trim_end())
                                                        .monospace()
                                                        .color(colors::TEXT()),
                                                ).wrap());
                                                ui.horizontal(|ui| {
                                                    ui.push_id((i, j), |ui| {
                                                        if style::tool_button(ui, "Copy code", false).clicked_by(egui::PointerButton::Primary) {
                                                            ui.output_mut(|o| o.copied_text = code.clone());
                                                        }
                                                        if style::tool_button(ui, "Insert at caret", false).clicked_by(egui::PointerButton::Primary) {
                                                            insert = Some(PanelAction::Insert(code.clone()));
                                                        }
                                                        if let Some(origin) = &self.origin {
                                                            if style::tool_button(ui, "Apply", false)
                                                                .on_hover_text("Replace the selection this request was made from (undo-safe)")
                                                                .clicked_by(egui::PointerButton::Primary)
                                                            {
                                                                insert = Some(PanelAction::Apply(code.clone(), origin.clone()));
                                                            }
                                                        }
                                                    });
                                                });
                                            });
                                    }
                                }
                            }
                        }
                    }
                    ui.add_space(6.0);
                }
                if self.conversation.is_empty() {
                    ui.add_space(12.0);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(if self.available {
                            "Select code and invoke an AI action, or ask below."
                        } else {
                            "No AI backend available — sign into Claude Code, or pick Local (Ollama) in Settings ▸ AI."
                        }).color(colors::TEXT_FAINT()));
                    });
                }
            });

        if send && !self.in_flight && !self.input.trim().is_empty() {
            let q = std::mem::take(&mut self.input);
            let ctx = ui.ctx().clone();
            self.submit(AiTaskKind::Ask, AiContext { extra: q, ..self.ask_context.clone() }, &ctx);
        }
        insert
    }
}

/// Build the task-specific user prompt. Pure — unit-tested below.
fn build_prompt(kind: AiTaskKind, c: &AiContext) -> String {
    let file = if c.file_name.is_empty() { "(unnamed)".to_string() } else { c.file_name.clone() };
    let lang = if c.language.is_empty() { "text".to_string() } else { c.language.to_lowercase() };
    let code_block = if c.code.is_empty() {
        String::new()
    } else {
        format!("\n```{lang}\n{}\n```\n", c.code)
    };
    match kind {
        AiTaskKind::ExplainDiagnostic => {
            let diag = c.diagnostic.clone().unwrap_or_default();
            format!(
                "Explain this compiler/analyzer diagnostic from `{file}` and the likely fix, \
                 referencing the code below. Be concrete and brief.\n\nDiagnostic:\n{diag}\n\
                 Code:{code_block}"
            )
        }
        AiTaskKind::FixDiagnostic => {
            let diag = c.diagnostic.clone().unwrap_or_default();
            format!(
                "This coding-standard / analyzer violation was flagged in `{file}`:\n{diag}\n\n\
                 Refactor the {lang} code below so the violation is gone while preserving \
                 behavior exactly. Reply with one short paragraph explaining the change, then \
                 ONE fenced code block containing the FULL replacement for the code shown — \
                 same span, nothing more, nothing elided.\n\nCode:{code_block}"
            )
        }
        AiTaskKind::ExplainSelection => format!(
            "Explain what this selected {lang} code from `{file}` does. Note any pitfalls, \
             edge cases, or surprising behavior. Be brief.\n\nCode:{code_block}"
        ),
        AiTaskKind::WriteUnitTest => {
            let style_hint = test_style_hint(&lang);
            format!(
                "Write a unit test for the following {lang} code from `{file}`. {style_hint} \
                 Output the complete test code in ONE fenced code block, with a one-sentence \
                 note before it at most.\n\nCode:{code_block}"
            )
        }
        AiTaskKind::Ask => {
            let q = c.extra.trim();
            let mut p = if c.code.is_empty() {
                q.to_string()
            } else {
                format!("{q}\n\nContext — `{file}` ({lang}):{code_block}")
            };
            if let Some(d) = c.diagnostic.as_deref().filter(|d| !d.trim().is_empty()) {
                p.push_str(&format!("\nCurrent diagnostics in this file:\n{d}"));
            }
            p
        }
    }
}

/// Language-appropriate test-framework directive.
fn test_style_hint(lang: &str) -> &'static str {
    match lang {
        "c" | "h" => {
            "Use the cFS UT-assert style: UtAssert_True assertions, UT_SetDeferredRetcode for \
             stubbed return codes, stub tables for dependencies, and name each test function \
             Test_<Func> after the function under test."
        }
        "rust" | "rs" => {
            "Use idiomatic Rust: a `#[cfg(test)] mod tests` block with `#[test]` functions and \
             `assert_eq!`/`assert!` macros."
        }
        "python" | "py" => {
            "Use pytest style: plain `test_*` functions with bare `assert` statements (no \
             unittest classes)."
        }
        _ => "Use the idiomatic unit-test framework for the language.",
    }
}

/// One rendered chunk of an assistant reply.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Text(String),
    Code(String),
}

/// Split markdown-ish text into prose and fenced code blocks. The fence's info string
/// (```rust) is dropped; an unterminated fence yields a code segment to the end.
fn split_fenced(s: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut rest = s;
    loop {
        match find_fence(rest) {
            None => {
                if !rest.is_empty() {
                    out.push(Segment::Text(rest.to_string()));
                }
                return out;
            }
            Some(open) => {
                if open > 0 {
                    out.push(Segment::Text(rest[..open].to_string()));
                }
                let after_ticks = &rest[open + 3..];
                // Drop the info string (up to and including the first newline).
                let body_start = after_ticks.find('\n').map(|i| i + 1).unwrap_or(after_ticks.len());
                let body = &after_ticks[body_start..];
                match find_fence(body) {
                    Some(close) => {
                        out.push(Segment::Code(body[..close].to_string()));
                        let after = &body[close + 3..];
                        // Skip the newline right after the closing fence, if any.
                        rest = after.strip_prefix('\n').unwrap_or(after);
                    }
                    None => {
                        out.push(Segment::Code(body.to_string()));
                        return out;
                    }
                }
            }
        }
    }
}

/// Byte offset of the next ``` that starts a line (or the string), else None.
fn find_fence(s: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = s[from..].find("```") {
        let at = from + i;
        if at == 0 || s.as_bytes()[at - 1] == b'\n' {
            return Some(at);
        }
        from = at + 3;
    }
    None
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

// =================================================================================================
// tests — headless, no network, no GUI. submit() is not exercised (it would hit the network);
// the pure prompt builder and the pump/state machine are.
// =================================================================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(code: &str, lang: &str) -> AiContext {
        AiContext {
            file_name: "widget.rs".into(),
            language: lang.into(),
            code: code.into(),
            diagnostic: None,
            extra: String::new(),
            origin: None,
        }
    }

    #[test]
    fn prompt_explain_diagnostic_contains_code_and_diagnostic() {
        let mut c = ctx("let x: u8 = 300;", "rust");
        c.diagnostic = Some("error[E0308]: mismatched types".into());
        let p = build_prompt(AiTaskKind::ExplainDiagnostic, &c);
        assert!(p.contains("let x: u8 = 300;"));
        assert!(p.contains("error[E0308]: mismatched types"));
        assert!(p.contains("likely fix"));
        assert!(p.contains("widget.rs"));
    }

    #[test]
    fn prompt_explain_selection_mentions_pitfalls() {
        let p = build_prompt(AiTaskKind::ExplainSelection, &ctx("fn f() {}", "rust"));
        assert!(p.contains("fn f() {}"));
        assert!(p.contains("pitfalls"));
    }

    #[test]
    fn prompt_unit_test_c_uses_utassert_style() {
        let p = build_prompt(AiTaskKind::WriteUnitTest, &ctx("int add(int a, int b);", "c"));
        assert!(p.contains("UtAssert"));
        assert!(p.contains("UT_SetDeferredRetcode"));
        assert!(p.contains("stub tables"));
        assert!(p.contains("Test_<Func>"));
        assert!(p.contains("int add(int a, int b);"));
        assert!(p.contains("ONE fenced code block"));
    }

    #[test]
    fn prompt_unit_test_rust_uses_test_attr() {
        let p = build_prompt(AiTaskKind::WriteUnitTest, &ctx("pub fn add(a: i32, b: i32) -> i32 { a + b }", "rust"));
        assert!(p.contains("#[test]"));
        assert!(p.contains("mod tests"));
        assert!(!p.contains("UtAssert"));
    }

    #[test]
    fn prompt_unit_test_python_uses_pytest() {
        let p = build_prompt(AiTaskKind::WriteUnitTest, &ctx("def add(a, b): return a + b", "python"));
        assert!(p.contains("pytest"));
        assert!(p.contains("def add(a, b): return a + b"));
    }

    #[test]
    fn prompt_ask_carries_question_and_code() {
        let mut c = ctx("struct S;", "rust");
        c.extra = "Why is this zero-sized?".into();
        let p = build_prompt(AiTaskKind::Ask, &c);
        assert!(p.starts_with("Why is this zero-sized?"));
        assert!(p.contains("struct S;"));
    }

    #[test]
    fn prompt_ask_without_code_is_just_the_question() {
        let mut c = AiContext::default();
        c.extra = "  What is a rope data structure?  ".into();
        let p = build_prompt(AiTaskKind::Ask, &c);
        assert_eq!(p, "What is a rope data structure?");
        assert!(!p.contains("Context"));
    }

    #[test]
    fn split_fenced_extracts_blocks() {
        let s = "Intro.\n```rust\nfn a() {}\n```\nMiddle.\n```\nplain\n```\nEnd.";
        let segs = split_fenced(s);
        assert_eq!(
            segs,
            vec![
                Segment::Text("Intro.\n".into()),
                Segment::Code("fn a() {}\n".into()),
                Segment::Text("Middle.\n".into()),
                Segment::Code("plain\n".into()),
                Segment::Text("End.".into()),
            ]
        );
    }

    #[test]
    fn split_fenced_handles_no_fence_and_unterminated() {
        assert_eq!(split_fenced("just prose"), vec![Segment::Text("just prose".into())]);
        let segs = split_fenced("hi\n```c\nint x;\n");
        assert_eq!(segs, vec![Segment::Text("hi\n".into()), Segment::Code("int x;\n".into())]);
        // Inline backticks mid-line are NOT a fence.
        assert_eq!(
            split_fenced("use ```ticks``` inline"),
            vec![Segment::Text("use ```ticks``` inline".into())]
        );
    }

    #[test]
    fn pump_streams_deltas_into_reply_bubble() {
        let mut panel = AiPanel::new();
        panel.conversation.push((Role::User, "q".into()));
        panel.conversation.push((Role::Assistant, String::new())); // what submit() pushes
        panel.in_flight = true;
        // Fake worker: deltas + Done through the panel's own tx (no network).
        panel.tx.send(AiMsg::Delta("the ".into())).unwrap();
        panel.tx.send(AiMsg::Delta("answer".into())).unwrap();
        panel.pump();
        assert!(panel.in_flight, "still streaming until Done");
        assert_eq!(panel.conversation[1], (Role::Assistant, "the answer".to_string()));
        panel.tx.send(AiMsg::Done(true)).unwrap();
        panel.pump();
        assert!(!panel.in_flight);
        assert_eq!(panel.conversation.len(), 2);
        // Idempotent when empty.
        panel.pump();
        assert_eq!(panel.conversation.len(), 2);
    }

    /// A failed request with NOTHING streamed shows the error text; one that failed midway
    /// keeps the partial reply.
    #[test]
    fn pump_done_failure_fills_empty_bubble_only() {
        let mut panel = AiPanel::new();
        panel.conversation.push((Role::Assistant, String::new()));
        panel.in_flight = true;
        panel.tx.send(AiMsg::Done(false)).unwrap();
        panel.pump();
        assert!(!panel.in_flight);
        assert!(panel.conversation[0].1.contains("request failed"));

        let mut panel = AiPanel::new();
        panel.conversation.push((Role::Assistant, "partial".into()));
        panel.in_flight = true;
        panel.tx.send(AiMsg::Done(false)).unwrap();
        panel.pump();
        assert_eq!(panel.conversation[0].1, "partial");
    }
}
