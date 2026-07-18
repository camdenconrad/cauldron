//! The Change Signature dialog: edit a C function's parameter list, preview the blast radius,
//! then apply.
//!
//! The dialog owns only the *intent* (a list of rows). Turning that into edits is
//! [`cauldron_psi::chsig::plan`]'s job, which is pure and tested — this module never computes an
//! edit itself. A preview is recomputed whenever the intent changes, so the counts and warnings
//! on screen always describe the change as currently spelled.
//!
//! Widget ids are explicit and stable (`egui::Id::new(("chsig-param", i))`): rows are added and
//! removed while the dialog is open, and egui's auto-ids shift when the widget sequence changes,
//! which silently moves keyboard focus to the wrong text box.

use cauldron_psi::chsig::{ParamOp, Plan, SignatureChange};
use cauldron_psi::rustsig;
use std::path::PathBuf;

/// Which engine plans this change. C is index-driven ([`cauldron_psi::chsig`]); Rust is driven by
/// rust-analyzer's reference set ([`cauldron_psi::rustsig`]) because a name-keyed Rust index
/// would resolve the wrong `new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    C,
    /// References from rust-analyzer, as (file, byte offset of the name token). `None` while the
    /// request is still in flight.
    Rust(Option<Vec<rustsig::Reference>>),
}

use crate::style;

/// One row of the parameter editor.
#[derive(Debug, Clone)]
pub struct Row {
    /// Index into the ORIGINAL parameter list, or `None` for a parameter being added.
    pub from: Option<usize>,
    /// The parameter declaration as it will appear (`int flags`).
    pub text: String,
    /// For a new parameter: what every existing call site should pass. Unused for kept ones.
    pub default_arg: String,
}

/// What the app should do after drawing the dialog this frame.
pub enum Action {
    /// Still open, nothing to do.
    None,
    Close,
    /// Apply this plan. Carries the change too, for the status message.
    Apply(Box<Plan>, SignatureChange),
}

pub struct ChangeSigUi {
    pub engine: Engine,
    pub function: String,
    /// The file the caret was in — the anchor for resolving which `static` is meant.
    pub path: PathBuf,
    pub rows: Vec<Row>,
    /// Original parameter texts, indexed by original position.
    pub original: Vec<String>,
    /// Recomputed whenever `rows` changes.
    plan: Option<Plan>,
    error: Option<String>,
    /// Set when `rows` changed and the preview is stale.
    dirty: bool,
}

impl ChangeSigUi {
    pub fn new(engine: Engine, function: String, path: PathBuf, params: Vec<String>) -> Self {
        let rows = params
            .iter()
            .enumerate()
            .map(|(i, p)| Row { from: Some(i), text: p.clone(), default_arg: String::new() })
            .collect();
        Self {
            engine,
            function,
            path,
            rows,
            original: params,
            plan: None,
            error: None,
            dirty: true,
        }
    }

    /// The change as currently spelled in the dialog.
    pub fn change(&self) -> SignatureChange {
        SignatureChange {
            function: self.function.clone(),
            params: self
                .rows
                .iter()
                .map(|r| match r.from {
                    // Only pass `text` when the user actually edited it, so an untouched
                    // parameter keeps its exact original spelling (including comments/spacing).
                    Some(from) => {
                        let unchanged =
                            self.original.get(from).is_some_and(|o| o.trim() == r.text.trim());
                        ParamOp::Keep {
                            from,
                            text: (!unchanged).then(|| r.text.clone()),
                        }
                    }
                    None => ParamOp::New {
                        text: r.text.clone(),
                        default_arg: r.default_arg.clone(),
                    },
                })
                .collect(),
        }
    }

    /// Force the next frame to recompute the preview — used when the reference set arrives.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Has the intent changed since the last [`Self::set_preview`]?
    pub fn needs_preview(&self) -> bool {
        self.dirty
    }

    pub fn set_preview(&mut self, plan: Result<Plan, String>) {
        match plan {
            Ok(p) => {
                self.plan = Some(p);
                self.error = None;
            }
            Err(e) => {
                self.plan = None;
                self.error = Some(e);
            }
        }
        self.dirty = false;
    }

    /// Signature preview line, e.g. `add(char *b, int a)`.
    fn signature_line(&self) -> String {
        let params: Vec<&str> = self.rows.iter().map(|r| r.text.trim()).collect();
        if params.is_empty() {
            // C spells "no parameters" as `(void)`; in Rust that is just `()`.
            match self.engine {
                Engine::C => format!("{}(void)", self.function),
                Engine::Rust(_) => format!("{}()", self.function),
            }
        } else {
            format!("{}({})", self.function, params.join(", "))
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Action {
        let mut action = Action::None;
        let mut open = true;
        egui::Window::new(format!("Change Signature — {}", self.function))
            .id(egui::Id::new("chsig-window"))
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(self.signature_line())
                        .monospace()
                        .size(13.0)
                        .color(style::colors::ACCENT()),
                );
                ui.add_space(6.0);

                // ---- parameter rows -------------------------------------------------------
                let mut remove: Option<usize> = None;
                let mut swap: Option<(usize, usize)> = None;
                let n = self.rows.len();
                for i in 0..n {
                    ui.horizontal(|ui| {
                        let row = &mut self.rows[i];
                        let is_new = row.from.is_none();
                        ui.add(
                            egui::TextEdit::singleline(&mut row.text)
                                .id(egui::Id::new(("chsig-param", i)))
                                .desired_width(190.0)
                                .hint_text("int x"),
                        );
                        // A new parameter needs a value for the callers that predate it.
                        if is_new {
                            ui.add(
                                egui::TextEdit::singleline(&mut row.default_arg)
                                    .id(egui::Id::new(("chsig-default", i)))
                                    .desired_width(130.0)
                                    .hint_text("value at call sites"),
                            );
                        } else {
                            ui.label(
                                egui::RichText::new(
                                    self.original
                                        .get(row.from.unwrap_or(0))
                                        .map(|o| format!("was: {o}"))
                                        .unwrap_or_default(),
                                )
                                .size(11.0)
                                .color(style::colors::TEXT_MUTED()),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(true, egui::Button::new("✕"))
                                .on_hover_text("Remove parameter")
                                .clicked_by(egui::PointerButton::Primary)
                            {
                                remove = Some(i);
                            }
                            if ui
                                .add_enabled(i + 1 < n, egui::Button::new("↓"))
                                .clicked_by(egui::PointerButton::Primary)
                            {
                                swap = Some((i, i + 1));
                            }
                            if ui
                                .add_enabled(i > 0, egui::Button::new("↑"))
                                .clicked_by(egui::PointerButton::Primary)
                            {
                                swap = Some((i, i - 1));
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    self.rows.remove(i);
                    self.dirty = true;
                }
                if let Some((a, b)) = swap {
                    self.rows.swap(a, b);
                    self.dirty = true;
                }

                ui.add_space(4.0);
                if ui
                    .button("+ Add Parameter")
                    .clicked_by(egui::PointerButton::Primary)
                {
                    self.rows.push(Row {
                        from: None,
                        text: String::new(),
                        default_arg: String::new(),
                    });
                    self.dirty = true;
                }

                ui.separator();

                // ---- preview --------------------------------------------------------------
                match (&self.error, &self.plan) {
                    (Some(e), _) => {
                        ui.label(
                            egui::RichText::new(e).color(style::colors::ERROR()).size(12.0),
                        );
                    }
                    (None, Some(plan)) => {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} call site(s), {} declaration(s), {} file(s)",
                                plan.call_sites_rewritten,
                                plan.declarations_rewritten,
                                plan.files_touched()
                            ))
                            .size(12.0),
                        );
                        if !plan.warnings.is_empty() {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "⚠ {} site(s) need manual review",
                                    plan.warnings.len()
                                ))
                                .color(style::colors::WARN())
                                .size(12.0),
                            );
                            egui::ScrollArea::vertical()
                                .id_salt("chsig-warnings")
                                .max_height(120.0)
                                .show(ui, |ui| {
                                    for w in &plan.warnings {
                                        ui.label(
                                            egui::RichText::new(w.message())
                                                .size(11.0)
                                                .color(style::colors::TEXT_MUTED()),
                                        );
                                    }
                                });
                        }
                    }
                    (None, None) if matches!(self.engine, Engine::Rust(None)) => {
                        ui.label(
                            egui::RichText::new("finding references (rust-analyzer)…")
                                .size(12.0)
                                .color(style::colors::TEXT_MUTED()),
                        );
                    }
                    (None, None) => {
                        ui.label(
                            egui::RichText::new("computing…")
                                .size(12.0)
                                .color(style::colors::TEXT_MUTED()),
                        );
                    }
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let can_apply = self
                        .plan
                        .as_ref()
                        .is_some_and(|p| !p.is_empty())
                        && self.error.is_none()
                        // An empty declaration would produce `f(, int b)`.
                        && self.rows.iter().all(|r| !r.text.trim().is_empty());
                    if ui
                        .add_enabled(can_apply, egui::Button::new("Refactor"))
                        .clicked_by(egui::PointerButton::Primary)
                    {
                        if let Some(plan) = self.plan.clone() {
                            action = Action::Apply(Box::new(plan), self.change());
                        }
                    }
                    if ui.button("Cancel").clicked_by(egui::PointerButton::Primary) {
                        action = Action::Close;
                    }
                });
            });

        // Track edits to the text boxes: any change invalidates the preview.
        let sig_now = self.signature_line();
        let defaults: String = self.rows.iter().map(|r| r.default_arg.as_str()).collect();
        let fingerprint = format!("{sig_now}|{defaults}");
        if ctx.memory_mut(|m| {
            let id = egui::Id::new("chsig-fingerprint");
            let prev: Option<String> = m.data.get_temp(id);
            let changed = prev.as_deref() != Some(fingerprint.as_str());
            if changed {
                m.data.insert_temp(id, fingerprint.clone());
            }
            changed
        }) {
            self.dirty = true;
        }

        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            return Action::Close;
        }
        action
    }
}
