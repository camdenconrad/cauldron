//! Headless end-to-end against the scripted `fake-lsp` binary (src/bin/fake_lsp.rs).
//!
//! The manager spawns servers by PATH name, so the test shims `clangd` → fake-lsp via a
//! symlink in a temp dir prepended to PATH. Everything lives in ONE #[test] because PATH and
//! FAKE_LSP_SCENARIO are process-global — no parallel-test races by construction.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cauldron_editor::syntax::Lang;
use cauldron_lsp::{lsp_types, txsync, LspEvent, LspManager, ServerState};
use ropey::Rope;

/// Pump until `pred` matches an event or the timeout elapses; returns the matched event.
fn pump_until(
    lsp: &mut LspManager,
    timeout: Duration,
    mut pred: impl FnMut(&LspEvent) -> bool,
) -> Option<LspEvent> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let (events, _) = lsp.pump();
        for (_, ev) in events {
            if pred(&ev) {
                return Some(ev);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

#[test]
fn code_actions_end_to_end_via_fake_server() {
    // --- shim the fake binary onto PATH as `clangd` -----------------------------------------
    let fake = env!("CARGO_BIN_EXE_fake-lsp");
    let dir = std::env::temp_dir().join(format!("cauldron-lsp-fake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(fake, dir.join("clangd")).unwrap();
    let path_var =
        format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
    std::env::set_var("PATH", path_var);
    std::env::set_var("FAKE_LSP_SCENARIO", "code-actions");

    let root = dir.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("compile_flags.txt"), "-std=c11\n").unwrap();
    let src = "int main(void) {\n    int oops = 0;\n    return 0;\n}\n";
    let main_c = root.join("main.c");
    std::fs::write(&main_c, src).unwrap();

    let mut lsp = LspManager::new(Arc::new(|| {}));
    lsp.open_doc(Lang::C, &root, &main_c, src);
    pump_until(&mut lsp, Duration::from_secs(10), |ev| {
        matches!(ev, LspEvent::State(ServerState::Ready))
    })
    .expect("fake server never finished the initialize handshake");

    // --- request code actions for the `oops` token's byte range ------------------------------
    let rope = Rope::from_str(src);
    let start = src.find("oops").unwrap();
    lsp.request_code_actions(&main_c, &rope, start..start + 4, &[], 7);

    let ev = pump_until(&mut lsp, Duration::from_secs(10), |ev| {
        matches!(ev, LspEvent::CodeActions { .. })
    })
    .expect("code-actions response never arrived");
    let LspEvent::CodeActions { generation, path, actions } = ev else { unreachable!() };
    assert_eq!(generation, 7, "generation stamped at request time");
    assert_eq!(path, main_c, "path stamped at request time");
    assert_eq!(actions.len(), 2, "one CodeAction + one bare Command");

    // Action 0: the quickfix CodeAction with a changes-map WorkspaceEdit.
    let lsp_types::CodeActionOrCommand::CodeAction(fix) = &actions[0] else {
        panic!("first action must decode as a CodeAction literal: {:?}", actions[0]);
    };
    assert_eq!(fix.title, "replace oops with 42");
    assert_eq!(fix.kind.as_ref().map(|k| k.as_str()), Some("quickfix"));
    assert_eq!(fix.is_preferred, Some(true));
    let edit = fix.edit.as_ref().expect("quickfix carries a WorkspaceEdit");
    let files = txsync::workspace_edit_to_file_edits(edit);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, main_c);
    let edits = &files[0].1;
    assert_eq!(edits.len(), 2);
    // The fake sends them ASCENDING; the flattener must return DESCENDING (back-to-front).
    assert_eq!(edits[0].range.start.line, 1);
    assert_eq!(edits[0].new_text, "fortytwo");
    assert_eq!(edits[1].range.start.line, 0);
    assert_eq!(edits[1].new_text, "/* fixed */ ");

    // Action 1: the bare Command → executeCommand → the fake pushes a workspace/applyEdit
    // back, which must surface as LspEvent::ApplyEdit (the auto-reply happens underneath).
    let lsp_types::CodeActionOrCommand::Command(cmd) = &actions[1] else {
        panic!("second action must decode as a bare Command: {:?}", actions[1]);
    };
    assert_eq!(cmd.command, "fake.fix");
    lsp.execute_command(&main_c, cmd);
    let ev = pump_until(&mut lsp, Duration::from_secs(10), |ev| {
        matches!(ev, LspEvent::ApplyEdit(_))
    })
    .expect("workspace/applyEdit never surfaced after executeCommand");
    let LspEvent::ApplyEdit(params) = ev else { unreachable!() };
    let files = txsync::workspace_edit_to_file_edits(&params.edit);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, PathBuf::from(&main_c));
    assert_eq!(files[0].1[0].new_text, "// via executeCommand\n");

    // --- graceful shutdown must stay snappy ---------------------------------------------------
    let t0 = Instant::now();
    lsp.shutdown_all();
    assert!(t0.elapsed() < Duration::from_secs(2), "graceful shutdown too slow");
    let _ = std::fs::remove_dir_all(&dir);
}
