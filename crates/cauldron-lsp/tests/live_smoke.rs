//! Live end-to-end smokes against the REAL servers on this machine. `#[ignore]`d — run with
//! `cargo test -p cauldron-lsp -- --ignored` (needs clangd / rust-analyzer on PATH + wall-clock).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cauldron_editor::buffer::Transaction;
use cauldron_editor::syntax::Lang;
use cauldron_lsp::{LspEvent, LspManager};
use ropey::Rope;

fn temp_project(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cauldron-lsp-smoke-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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
        std::thread::sleep(Duration::from_millis(25));
    }
    None
}

#[test]
#[ignore = "needs clangd on PATH; run with -- --ignored"]
fn live_clangd_diagnostics_end_to_end() {
    let root = temp_project("clangd");
    std::fs::write(root.join("compile_flags.txt"), "-std=c11\n").unwrap();
    let src = "int main(void) {\n    int x = \"oops\";\n    return x;\n}\n";
    let main_c = root.join("main.c");
    std::fs::write(&main_c, src).unwrap();

    let mut lsp = LspManager::new(Arc::new(|| {}));
    lsp.open_doc(Lang::C, &root, &main_c, src);

    // A diagnostic about the int/string mismatch must arrive by push.
    let ev = pump_until(&mut lsp, Duration::from_secs(15), |ev| {
        matches!(ev, LspEvent::Diagnostics { path, diags, .. } if path == &main_c && !diags.is_empty())
    })
    .expect("clangd never published a diagnostic");
    let LspEvent::Diagnostics { diags, .. } = &ev else { unreachable!() };
    let d = &diags[0];
    assert_eq!(d.range.start.line, 1, "error is on line 2 (0-based 1): {d:?}");
    eprintln!("clangd diagnostic OK: {}", d.message);

    // Type an edit ABOVE the error (insert a line) — the republished diagnostic must move down.
    let pre = Rope::from_str(src);
    let tx = Transaction::insert(0, "/* pad */\n");
    let post = {
        let mut r = pre.clone();
        r.insert(0, "/* pad */\n");
        r
    };
    lsp.did_change(&main_c, &pre, &post, &tx);
    let ev = pump_until(&mut lsp, Duration::from_secs(15), |ev| {
        matches!(ev, LspEvent::Diagnostics { path, diags, .. }
            if path == &main_c && diags.iter().any(|d| d.range.start.line == 2))
    })
    .expect("diagnostic did not move after the edit");
    drop(ev);
    eprintln!("clangd incremental didChange OK — diagnostic tracked the edit");

    let t0 = Instant::now();
    lsp.shutdown_all();
    assert!(t0.elapsed() < Duration::from_secs(2), "graceful shutdown too slow");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "needs pyright-langserver (npm i -g pyright); run with -- --ignored"]
fn live_pyright_publish_diagnostics() {
    let root = temp_project("pyright");
    let src = "def f(x: int) -> int:\n    return x\n\nf(\"oops\")\n";
    let app_py = root.join("app.py");
    std::fs::write(&app_py, src).unwrap();

    let mut lsp = LspManager::new(Arc::new(|| {}));
    lsp.open_doc(Lang::Python, &root, &app_py, src);

    // pyright PUSHES publishDiagnostics for open files (diagnosticMode: openFilesOnly).
    let ev = pump_until(&mut lsp, Duration::from_secs(30), |ev| {
        if let LspEvent::Diagnostics { path, diags, .. } = ev {
            if path == &app_py {
                for d in diags {
                    eprintln!("pyright: {}", d.message);
                }
                // pyright words it: `Argument of type "Literal['oops']" cannot be assigned
                // to parameter "x" of type "int"` … `is not assignable to "int"`.
                return diags.iter().any(|d| {
                    let m = d.message.to_lowercase();
                    m.contains("assignable") || m.contains("assigned")
                });
            }
        }
        false
    })
    .expect("pyright never published the str→int type error");
    drop(ev);
    eprintln!("pyright publishDiagnostics OK");

    lsp.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "needs rust-analyzer on PATH + ~1-2 min first index; run with -- --ignored"]
fn live_rust_analyzer_pull_diagnostics() {
    let root = temp_project("ra");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let src = "fn main() {\n    let x: i32 = \"oops\";\n    let _ = x;\n}\n";
    let main_rs = root.join("src/main.rs");
    std::fs::write(&main_rs, src).unwrap();

    let mut lsp = LspManager::new(Arc::new(|| {}));
    lsp.open_doc(Lang::Rust, &root, &main_rs, src);

    // Native diagnostics arrive by PULL only (after quiescent + delay); allow a long first index.
    // r-a's native wording is "expected i32, found &str" (rustc's flycheck says "mismatched
    // types") — accept either, and log what actually arrived for diagnosis.
    let ev = pump_until(&mut lsp, Duration::from_secs(120), |ev| {
        if let LspEvent::PullDiagnostics { path, diags, .. } = ev {
            if path == &main_rs {
                for d in diags {
                    eprintln!("pulled: {}", d.message);
                }
                return diags.iter().any(|d| {
                    let m = d.message.to_lowercase();
                    m.contains("mismatch") || m.contains("expected")
                });
            }
        }
        false
    })
    .expect("rust-analyzer never pulled the type error");
    drop(ev);
    eprintln!("rust-analyzer pull diagnostics OK");

    lsp.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}
