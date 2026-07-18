//! Smoke test: run the Rust signature/call parsers over this repository's own source.
//!
//! The unit tests use small synthetic snippets; this asserts the parsers survive real code at
//! scale (macros, async, generics, where-clauses, attributes, nested modules) without panicking
//! and produce spans that actually slice the source on char boundaries.

use cauldron_psi::rustsig;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target" || n == ".git") {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn parses_this_repository_without_panicking() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    assert!(files.len() > 30, "expected the repo's crates, found {}", files.len());

    let (mut fns, mut with_self) = (0usize, 0usize);
    for f in &files {
        let Ok(src) = std::fs::read_to_string(f) else { continue };
        // One parse per file, reused across every probe — the same thing `plan` does, and the
        // reason a large refactor is not quadratic in file size.
        let Some(parsed) = rustsig::ParsedFile::new(&src) else { continue };
        // Probe every `fn ` occurrence: each should yield a signature whose spans slice cleanly.
        for (i, _) in src.match_indices("fn ") {
            let name_off = i + 3;
            let Some(sig) = parsed.signature_at_name(&src, name_off) else { continue };
            fns += 1;
            if sig.self_range.is_some() {
                with_self += 1;
            }
            assert!(
                src.get(sig.params_range.clone()).is_some(),
                "{}: params span {:?} does not slice",
                f.display(),
                sig.params_range
            );
            for p in &sig.param_ranges {
                assert!(
                    src.get(p.clone()).is_some(),
                    "{}: param span {p:?} does not slice",
                    f.display()
                );
                // A parameter span must not contain the enclosing parens.
                let t = &src[p.clone()];
                assert!(!t.starts_with('('), "{}: param span includes paren: {t:?}", f.display());
            }
        }
    }
    assert!(fns > 500, "expected many functions, parsed {fns}");
    assert!(with_self > 100, "expected many methods, found {with_self}");
    eprintln!("parsed {fns} signatures ({with_self} with self) across {} files", files.len());
}

#[test]
fn call_sites_in_real_code_have_sliceable_argument_spans() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let src = std::fs::read_to_string(root.join("cauldron-psi/src/chsig.rs")).unwrap();
    let mut found = 0usize;
    for (i, _) in src.match_indices("render_args(") {
        let Some(call) = rustsig::call_at_name(&src, i) else { continue };
        found += 1;
        assert!(src.get(call.args_range.clone()).is_some());
        for a in &call.arg_ranges {
            assert!(src.get(a.clone()).is_some(), "arg span {a:?} does not slice");
        }
    }
    assert!(found > 0, "expected to find calls to render_args in chsig.rs");
}
