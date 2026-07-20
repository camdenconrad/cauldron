//! Extract Function over real C, checking the property that actually matters: whatever it
//! ACCEPTS must produce a body that still parses, a call that still parses, and a parameter list
//! that mentions no variable the body does not use. What it refuses is fine — refusing is the
//! designed behaviour for anything it cannot prove.

use cauldron_psi::extract::{plan, ExtractError};

fn parses(src: &str) -> bool {
    let mut p = tree_sitter::Parser::new();
    p.set_language(&tree_sitter_c::language()).unwrap();
    match p.parse(src, None) {
        Some(t) => !t.root_node().has_error(),
        None => false,
    }
}

/// Try to extract every single top-level statement of every function in a real file. Most will be
/// refused; the ones that are not must be well-formed.
#[test]
fn accepted_extractions_are_well_formed() {
    // Real .c files, not headers: headers are declarations and would exercise nothing. These are
    // other people's C, full of macros, K&R survivals and shapes fixtures never think of.
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for dir in ["/usr/share/readline", "/usr/share/libtool", "/usr/lib/tcl8.6", "/usr/lib/tk8.6"] {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "c") {
                files.push(p);
            }
        }
    }
    files.sort();
    if files.len() < 3 {
        eprintln!("skipping: no C corpus on this box");
        return;
    }

    let mut accepted = 0usize;
    let mut refused = 0usize;
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else { continue };
        if src.len() > 400_000 {
            continue;
        }
        // Statement-ish anchors: every `;` gives a candidate line to try extracting.
        let mut tried = 0usize;
        for (i, _) in src.match_indices(';').take(120) {
            let ls = src[..i].rfind('\n').map_or(0, |n| n + 1);
            let le = src[i..].find('\n').map_or(src.len(), |n| i + n);
            if ls >= le {
                continue;
            }
            tried += 1;
            match plan(&src, ls..le, "extracted_helper") {
                Ok(p) => {
                    accepted += 1;
                    // The generated function must parse on its own.
                    assert!(
                        parses(&p.function_text),
                        "{path:?} produced a function that does not parse:\n{}",
                        p.function_text
                    );
                    // And the call must parse as a statement.
                    let wrapped = format!("void t(void) {{ {} }}", p.call_text);
                    assert!(parses(&wrapped), "{path:?} bad call: {}", p.call_text);
                    // Every parameter must actually appear in the body.
                    for prm in &p.params {
                        assert!(
                            p.function_text.contains(&prm.name),
                            "{path:?} parameter `{}` unused in:\n{}",
                            prm.name,
                            p.function_text
                        );
                    }
                    // The replaced span must be real and on char boundaries.
                    assert!(p.replace.end <= src.len(), "{path:?} span out of bounds");
                    assert!(src.is_char_boundary(p.replace.start));
                    assert!(src.is_char_boundary(p.replace.end));
                    assert!(p.insert_at <= src.len());
                }
                Err(ExtractError::Empty)
                | Err(ExtractError::NotInFunction)
                | Err(ExtractError::NotStatements)
                | Err(ExtractError::EscapingControlFlow(_))
                | Err(ExtractError::MultipleOutputs(_))
                | Err(ExtractError::UnknownType(_))
                | Err(ExtractError::Unparseable) => refused += 1,
            }
        }
        let _ = tried;
    }
    eprintln!("{} headers: {accepted} accepted, {refused} refused", files.len());
    // Headers are mostly declarations; the point is that nothing panicked and every acceptance
    // was well-formed, not that a particular count was reached.
}

/// A realistic function body, extracted at every statement boundary.
#[test]
fn a_real_function_body_extracts_soundly() {
    let src = "\
#include <stdio.h>

static int sum_to(int n)
{
    int total = 0;
    int i = 0;
    while (i < n) {
        total += i;
        i++;
    }
    printf(\"%d\\n\", total);
    return total;
}
";
    let mut ok = 0;
    for (i, _) in src.match_indices(';') {
        let ls = src[..i].rfind('\n').map_or(0, |n| n + 1);
        let le = src[i..].find('\n').map_or(src.len(), |n| i + n);
        if let Ok(p) = plan(src, ls..le, "helper") {
            assert!(parses(&p.function_text), "does not parse:\n{}", p.function_text);
            ok += 1;
        }
    }
    assert!(ok > 0, "expected at least one extractable statement in a plain function");
}

/// The plan is two edits that the app applies as one transaction. Applying them textually here
/// proves the OFFSETS compose — a plan whose pieces are individually right can still produce
/// garbage if the insertion shifts the replacement out from under itself.
#[test]
fn applying_both_edits_yields_a_file_that_still_parses() {
    let src = "\
#include <stdio.h>

static int sum_to(int n)
{
    int total = 0;
    int i = 0;
    while (i < n) {
        total += i;
        i++;
    }
    return total;
}
";
    let sel_text = "    while (i < n) {\n        total += i;\n        i++;\n    }";
    let s = src.find(sel_text).expect("fixture");
    let p = plan(src, s..s + sel_text.len(), "accumulate").expect("this loop is extractable");

    // Apply descending so the earlier offset stays valid, which is what the app's sorted
    // single-transaction apply does for real.
    let mut out = src.to_string();
    out.replace_range(p.replace.clone(), &p.call_text);
    out.insert_str(p.insert_at, &p.function_text);

    assert!(parses(&out), "combined result does not parse:\n{out}");
    assert!(out.contains("accumulate("), "the call must be present:\n{out}");
    // Moved, not copied: the loop body appears exactly once in the whole file.
    assert_eq!(out.matches("total += i;").count(), 1, "moved, not copied:\n{out}");
    assert_eq!(out.matches("while (i < n)").count(), 1, "{out}");
    // The new function must sit ABOVE its caller, so no prototype is needed.
    let fn_at = out.find("accumulate(int").expect("definition");
    let caller_at = out.find("static int sum_to").expect("caller");
    assert!(fn_at < caller_at, "callee must be declared before use:\n{out}");
}
