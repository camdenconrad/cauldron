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

/// The full Change Signature chain for Rust, end to end against a real rust-analyzer:
/// `textDocument/references` → LSP positions → byte offsets → `rustsig::plan` → applied text.
///
/// This is the integration the unit tests cannot cover: everything upstream of `plan` is
/// synthetic there, so a wrong encoding, a wrong position field, or a reference set that omits
/// the declaration would all pass the unit tests and fail here.
#[test]
#[ignore = "needs rust-analyzer on PATH + first index; run with -- --ignored"]
fn live_rust_change_signature_end_to_end() {
    use cauldron_psi::chsig::{ParamOp, SignatureChange};
    use cauldron_psi::rustsig;

    let root = temp_project("ra-chsig");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"chsig\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    // Declaration in one module, callers in another — the layout that makes local seeding
    // impossible and forces the reference set to supply the parameter list.
    let lib = "\
pub mod api;
pub fn go() {
    let t = api::T;
    t.send(1, 2);
    api::T::send(&t, 3, 4);
}
";
    let api = "\
pub struct T;
impl T {
    pub fn send(&self, msg: i32, len: usize) -> i32 { msg + len as i32 }
}
";
    let lib_rs = root.join("src/lib.rs");
    let api_rs = root.join("src/api.rs");
    std::fs::write(&lib_rs, lib).unwrap();
    std::fs::write(&api_rs, api).unwrap();

    let mut lsp = LspManager::new(Arc::new(|| {}));
    lsp.open_doc(Lang::Rust, &root, &lib_rs, lib);
    lsp.open_doc(Lang::Rust, &root, &api_rs, api);

    // Ask for references at the `send` declaration.
    let decl_off = api.find("send").expect("declaration present");
    let api_rope = Rope::from_str(api);
    let gen = 7;
    // Give r-a time to finish its first index before the request, then retry until it answers
    // with more than just the declaration.
    let mut locations = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline && locations.len() < 3 {
        lsp.request_references(&api_rs, &api_rope, decl_off, gen);
        if let Some(ev) = pump_until(&mut lsp, Duration::from_secs(20), |ev| {
            matches!(ev, LspEvent::References { generation, .. } if *generation == gen)
        }) {
            if let LspEvent::References { locations: got, .. } = ev {
                if got.len() > locations.len() {
                    locations = got;
                }
            }
        }
    }
    assert!(
        locations.len() >= 3,
        "expected the declaration + both calls, got {} location(s)",
        locations.len()
    );

    // LSP positions -> byte offsets, exactly as the app does it.
    let texts: std::collections::HashMap<PathBuf, String> =
        [(lib_rs.clone(), lib.to_string()), (api_rs.clone(), api.to_string())].into();
    let mut refs = Vec::new();
    for loc in &locations {
        let path = cauldron_lsp::capabilities::uri_to_path(&loc.uri).expect("file uri");
        let Some(text) = texts.get(&path) else { continue };
        let enc = lsp.encoding_for(&path).unwrap_or(cauldron_lsp::Encoding::Utf16);
        let rope = Rope::from_str(text);
        // Same conversion the app performs, including the encoding the server negotiated.
        let point = cauldron_editor::position::Point {
            line: loc.range.start.line as usize,
            col: loc.range.start.character as usize,
        };
        let offset = match enc {
            cauldron_lsp::Encoding::Utf8 => {
                cauldron_editor::position::point_to_byte_clamped(&rope, point)
            }
            cauldron_lsp::Encoding::Utf16 => {
                cauldron_editor::position::utf16_to_byte(&rope, point)
            }
        };
        refs.push(rustsig::Reference { path, offset });
    }

    // The rows the dialog would seed itself with, from the declaration among the references.
    let params = rustsig::params_from_references(&refs, "send", |p| texts.get(p).cloned())
        .expect("declaration found among references");
    assert_eq!(params, ["msg: i32", "len: usize"], "seeded rows come from the declaration");

    // Swap the two parameters.
    let change = SignatureChange {
        function: "send".into(),
        params: vec![
            ParamOp::Keep { from: 1, text: None },
            ParamOp::Keep { from: 0, text: None },
        ],
    };
    let plan = rustsig::plan(&refs, &change, |p| texts.get(p).cloned()).expect("plan");
    assert_eq!(plan.declarations_rewritten, 1);
    assert_eq!(plan.call_sites_rewritten, 2, "the method call and the UFCS call");

    let mut out = texts.clone();
    for fe in &plan.files {
        let s = out.get_mut(&fe.path).expect("planned file");
        for e in &fe.edits {
            s.replace_range(e.range.clone(), &e.text);
        }
    }
    let new_api = &out[&api_rs];
    let new_lib = &out[&lib_rs];
    assert!(
        new_api.contains("pub fn send(&self, len: usize, msg: i32)"),
        "declaration not rewritten: {new_api}"
    );
    // Method call: receiver is NOT an argument, so both arguments swap.
    assert!(new_lib.contains("t.send(2, 1)"), "method call wrong: {new_lib}");
    // UFCS: the receiver keeps argument slot 0 and only the rest swap.
    assert!(new_lib.contains("api::T::send(&t, 4, 3)"), "UFCS call wrong: {new_lib}");

    eprintln!("live Rust Change Signature OK: {new_lib}");
    lsp.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}
