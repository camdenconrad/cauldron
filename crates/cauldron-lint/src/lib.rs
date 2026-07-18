//! cauldron-lint — NASA/JPL Power-of-Ten enforcement (Gate-B skeleton).
//!
//! Phase 0 ships two REAL tree-sitter checks so the Gate-B question ("can we surface a finding
//! cFS's own CI misses?") is answerable early, plus the unified Diagnostic model everything —
//! clang-tidy YAML, cppcheck XML, and the custom analyzer — parses into. The full rule table
//! lives in docs/phase0.md.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
    /// Below Warning: never fails the build unless `--strict` (Tier-2 recursion findings).
    Advice,
}

/// The unified diagnostic every backend (tree-sitter, clang-tidy, cppcheck, PSI) maps into.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    /// e.g. "pot-4-function-length"
    pub rule: String,
    pub severity: Severity,
    pub file: String,
    /// 1-based.
    pub line: usize,
    pub message: String,
    /// Standard citation, e.g. "JPL Power of Ten, Rule 4".
    pub citation: String,
    /// Baseline-stable identity: 16 hex chars of FNV-1a 64 over line-INDEPENDENT facts
    /// (rule + file basename + function/snippet for single-file checks; rule + rotation-canonical
    /// cycle member names for recursion findings). Edits above a finding never churn it.
    pub fingerprint: String,
    /// Witness cycle hops ("file:line func → next") for recursion findings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witness: Option<Vec<String>>,
}

/// FNV-1a 64-bit — hand-rolled so fingerprints are stable across Rust versions and runs
/// (std's DefaultHasher makes no such guarantee).
fn fnv1a64(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for part in parts {
        for b in part.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        // Separator byte so ["ab","c"] != ["a","bc"].
        h ^= 0x1f;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 16-hex-char fingerprint over `rule` + `parts`.
pub fn fingerprint(rule: &str, parts: &[&str]) -> String {
    let mut all = vec![rule];
    all.extend_from_slice(parts);
    format!("{:016x}", fnv1a64(&all))
}

fn basename(file: &str) -> &str {
    Path::new(file).file_name().and_then(|f| f.to_str()).unwrap_or(file)
}

/// Canonicalize a cycle's member list: rotate so the lexicographically-smallest member comes
/// first. The cycle order is preserved (A→B→C stays distinct from A→C→B), but every rotation of
/// the same cycle — i.e. every possible discovery order — canonicalizes identically, so the
/// fingerprint is stable across scans.
pub fn canonical_cycle(members: &[String]) -> Vec<String> {
    if members.is_empty() {
        return Vec::new();
    }
    let min = (0..members.len()).min_by_key(|&i| &members[i]).unwrap();
    members[min..].iter().chain(members[..min].iter()).cloned().collect()
}

/// Power of Ten Rule 4: functions no longer than ~60 lines.
pub const MAX_FUNCTION_LINES: usize = 60;

/// Run the phase-0 tree-sitter checks (PoT Rules 1 & 4) over one C source.
pub fn check_c_source(file: &str, source: &str) -> anyhow::Result<Vec<Diagnostic>> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_c::language())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;

    let mut out = Vec::new();
    walk(tree.root_node(), &mut |node| {
        match node.kind() {
            // Rule 1: no goto (setjmp/longjmp caught as call idents below), no recursion (needs
            // the call-graph pass — deferred past phase 0).
            "goto_statement" => {
                // Fingerprint on basename + the normalized statement text — line-independent,
                // so edits above the goto don't churn baselines.
                let snippet: String = source[node.byte_range()].split_whitespace().collect::<Vec<_>>().join(" ");
                out.push(Diagnostic {
                    rule: "pot-1-goto".into(),
                    severity: Severity::Error,
                    file: file.into(),
                    line: node.start_position().row + 1,
                    message: "goto is forbidden (restrict control flow to simple constructs)".into(),
                    citation: "JPL Power of Ten, Rule 1".into(),
                    fingerprint: fingerprint("pot-1-goto", &[basename(file), &snippet]),
                    witness: None,
                });
            }
            "function_definition" => {
                let lines = node.end_position().row - node.start_position().row + 1;
                if lines > MAX_FUNCTION_LINES {
                    let name = function_name(node, source).unwrap_or("<anonymous>");
                    out.push(Diagnostic {
                        rule: "pot-4-function-length".into(),
                        severity: Severity::Warning,
                        file: file.into(),
                        line: node.start_position().row + 1,
                        message: format!("function `{name}` is {lines} lines (limit {MAX_FUNCTION_LINES}) — should fit on one page"),
                        citation: "JPL Power of Ten, Rule 4".into(),
                        fingerprint: fingerprint("pot-4-function-length", &[basename(file), name]),
                        witness: None,
                    });
                }
                // Rule 5: assertion density — a non-trivial function with ZERO assertions
                // can't self-check its invariants. Advice-level (the 2-per-function target
                // is a review policy; zero is what a machine can flag honestly).
                if lines >= MIN_LINES_FOR_ASSERT_CHECK && count_asserts(node, source) == 0 {
                    let name = function_name(node, source).unwrap_or("<anonymous>");
                    out.push(Diagnostic {
                        rule: "pot-5-no-assertions".into(),
                        severity: Severity::Advice,
                        file: file.into(),
                        line: node.start_position().row + 1,
                        message: format!(
                            "function `{name}` ({lines} lines) contains no assertions — \
                             target ≥2 per function to catch impossible states"
                        ),
                        citation: "JPL Power of Ten, Rule 5".into(),
                        fingerprint: fingerprint("pot-5-no-assertions", &[basename(file), name]),
                        witness: None,
                    });
                }
            }
            "call_expression" => {
                if let Some(f) = node.child_by_field_name("function") {
                    let callee = &source[f.byte_range()];
                    if callee == "setjmp" || callee == "longjmp" {
                        out.push(Diagnostic {
                            rule: "pot-1-setjmp".into(),
                            severity: Severity::Error,
                            file: file.into(),
                            line: node.start_position().row + 1,
                            message: format!("{callee} is forbidden"),
                            citation: "JPL Power of Ten, Rule 1".into(),
                            fingerprint: fingerprint("pot-1-setjmp", &[basename(file), callee]),
                            witness: None,
                        });
                    }
                    // Rule 3: no dynamic memory after initialization. A single-file pass can't
                    // see the init boundary, so every allocator call is surfaced (cFS style
                    // forbids them outright anyway).
                    if matches!(callee, "malloc" | "calloc" | "realloc" | "free" | "alloca" | "sbrk") {
                        out.push(Diagnostic {
                            rule: "pot-3-heap".into(),
                            severity: Severity::Warning,
                            file: file.into(),
                            line: node.start_position().row + 1,
                            message: format!(
                                "dynamic memory ({callee}) — use static/pool allocation sized at init"
                            ),
                            citation: "JPL Power of Ten, Rule 3".into(),
                            fingerprint: fingerprint(
                                "pot-3-heap",
                                &[basename(file), callee, &node.start_position().row.to_string()],
                            ),
                            witness: None,
                        });
                    }
                }
            }
            // Rule 2: loops must have a statically verifiable bound. Constant-true loops with
            // no break/return/goto in their body can never terminate.
            "while_statement" | "for_statement" => {
                let cond_true = match node.kind() {
                    "while_statement" => node
                        .child_by_field_name("condition")
                        .map(|c| {
                            let t: String = source[c.byte_range()]
                                .chars()
                                .filter(|ch| !ch.is_whitespace() && *ch != '(' && *ch != ')')
                                .collect();
                            t == "1" || t == "true" || t == "TRUE"
                        })
                        .unwrap_or(false),
                    // `for (;;)` — no condition node at all.
                    _ => node.child_by_field_name("condition").is_none(),
                };
                if cond_true && !subtree_has_exit(node) {
                    let snippet: String =
                        source[node.byte_range()].split_whitespace().take(6).collect::<Vec<_>>().join(" ");
                    out.push(Diagnostic {
                        rule: "pot-2-unbounded-loop".into(),
                        severity: Severity::Warning,
                        file: file.into(),
                        line: node.start_position().row + 1,
                        message: "constant-true loop with no break/return — every loop needs a \
                                  statically provable bound (task main loops excepted by design review)"
                            .into(),
                        citation: "JPL Power of Ten, Rule 2".into(),
                        fingerprint: fingerprint("pot-2-unbounded-loop", &[basename(file), &snippet]),
                        witness: None,
                    });
                }
            }
            // Rule 9: no more than one level of pointer indirection.
            "pointer_declarator" => {
                let nested = node
                    .child_by_field_name("declarator")
                    .is_some_and(|d| d.kind() == "pointer_declarator");
                // Only the OUTERMOST pointer_declarator reports (its parent is not one).
                let outer = node.parent().is_none_or(|p| p.kind() != "pointer_declarator");
                if nested && outer {
                    let snippet: String =
                        source[node.byte_range()].split_whitespace().collect::<Vec<_>>().join(" ");
                    out.push(Diagnostic {
                        rule: "pot-9-multi-indirection".into(),
                        severity: Severity::Warning,
                        file: file.into(),
                        line: node.start_position().row + 1,
                        message: format!(
                            "`{snippet}`: more than one level of pointer indirection — \
                             restrict to a single `*`"
                        ),
                        citation: "JPL Power of Ten, Rule 9".into(),
                        fingerprint: fingerprint("pot-9-multi-indirection", &[basename(file), &snippet]),
                        witness: None,
                    });
                }
            }
            // Rule 8: preprocessor confined to includes and simple definitions — token
            // pasting builds identifiers the analyzers (and reviewers) can't see.
            "preproc_def" | "preproc_function_def" => {
                let text = &source[node.byte_range()];
                if text.contains("##") {
                    let name = node
                        .child_by_field_name("name")
                        .map(|n| &source[n.byte_range()])
                        .unwrap_or("<macro>");
                    out.push(Diagnostic {
                        rule: "pot-8-token-paste".into(),
                        severity: Severity::Advice,
                        file: file.into(),
                        line: node.start_position().row + 1,
                        message: format!(
                            "macro `{name}` uses token pasting (##) — generated identifiers \
                             defeat static analysis and grep"
                        ),
                        citation: "JPL Power of Ten, Rule 8".into(),
                        fingerprint: fingerprint("pot-8-token-paste", &[basename(file), name]),
                        witness: None,
                    });
                }
            }
            _ => {}
        }
    });
    Ok(out)
}

/// Rule 5 fires only on functions at least this long — a 3-line getter needs no assert.
pub const MIN_LINES_FOR_ASSERT_CHECK: usize = 20;

/// Does any descendant leave the loop? (break / return / goto — goto is separately flagged
/// by Rule 1, but it still bounds THIS loop.)
fn subtree_has_exit(node: Node) -> bool {
    let mut found = false;
    walk(node, &mut |n| {
        if matches!(n.kind(), "break_statement" | "return_statement" | "goto_statement") {
            found = true;
        }
    });
    found
}

/// Count assertion-style calls under `node`: `assert(...)` and the all-caps `*ASSERT*`
/// macros flight code actually uses (CFE_ES_ASSERT, OS_ASSERT, STATIC_ASSERT…).
fn count_asserts(node: Node, source: &str) -> usize {
    let mut count = 0;
    walk(node, &mut |n| {
        if n.kind() == "call_expression" {
            if let Some(f) = n.child_by_field_name("function") {
                let callee = &source[f.byte_range()];
                if callee == "assert" || callee.to_ascii_uppercase().contains("ASSERT") {
                    count += 1;
                }
            }
        }
    });
    count
}

fn function_name<'a>(def: Node, source: &'a str) -> Option<&'a str> {
    // function_definition > declarator (function_declarator) > declarator (identifier)
    let mut d = def.child_by_field_name("declarator")?;
    loop {
        match d.kind() {
            "identifier" => return Some(&source[d.byte_range()]),
            _ => d = d.child_by_field_name("declarator")?,
        }
    }
}

/// Whole-project PoT Rule 1 (no recursion): run the PSI one-shot scan and map every SCC finding
/// into the unified [`Diagnostic`]. Function cycles = `pot-1-recursion` (Error); macro-only
/// cycles (usually config artifacts) = `pot-1-recursion-possible` (Advice).
pub fn check_project(root: &Path, extra_excludes: &[PathBuf]) -> Vec<Diagnostic> {
    let scan = cauldron_psi::project::scan_project(root, extra_excludes);
    scan.findings.iter().map(recursion_diagnostic).collect()
}

/// EXACT whole-program PoT Rule 1: same check, but over the preprocessed TUs a compile database
/// says the build really compiles. No macro nodes, no inactive `#if` branches, no rival
/// definitions — so the macro-textual and config-dependent tiers come out empty by construction
/// and every remaining cycle is one the compiler emits. See [`cauldron_psi::tu`].
pub fn check_compile_db(
    root: &Path,
    db: &Path,
) -> anyhow::Result<(Vec<Diagnostic>, cauldron_psi::tu::DbCoverage)> {
    let (scan, coverage) = cauldron_psi::tu::scan_compile_db(root, db)?;
    Ok((scan.findings.iter().map(recursion_diagnostic).collect(), coverage))
}

/// Map one PSI Rule-1 finding into a [`Diagnostic`]. The fingerprint hashes the rule id +
/// the rotation-canonical cycle member names (see [`canonical_cycle`]) — no file paths, no
/// line numbers — so the same cycle fingerprints identically regardless of discovery order
/// or unrelated edits.
pub fn recursion_diagnostic(f: &cauldron_psi::project::Rule1Finding) -> Diagnostic {
    // Three tiers, keyed on what the code actually DOES, not on how it is spelled:
    //   - survives macro expansion, no guard  -> the real alarm
    //   - survives expansion, but every back-edge sits behind a re-entry guard -> bounded by
    //     design (cFE's CFE_SB_Global.StopRecurseFlags is the canonical case); still a Rule-1
    //     finding, but it is a waiver decision, not a bug
    //   - evaporates under expansion -> advice only
    let (rule, severity) = if f.macro_textual {
        ("pot-1-recursion-textual", Severity::Advice)
    } else if f.config_dependent {
        ("pot-1-recursion-config", Severity::Warning)
    } else if f.guarded {
        ("pot-1-recursion-guarded", Severity::Warning)
    } else {
        ("pot-1-recursion", Severity::Error)
    };
    let cycle = canonical_cycle(&f.members);
    let chain = cycle.join(" → ");
    // Guarded cycles stay findings (PoT Rule 1 forbids recursion, bounded or not) but the
    // message cites the guard so triage starts from the truth.
    let guard_note = if f.guarded {
        let g = f.hops.iter().find_map(|h| h.guard.as_deref()).unwrap_or("");
        format!(" GUARDED: an edge is gated by a recognized re-entry guard ({g}).")
    } else {
        String::new()
    };
    // A config-dependent cycle is NOT a recursion claim, so it must not read like one: say
    // which multiply-defined macro fabricates it and that no single build has this cycle.
    let config_note = if f.config_dependent {
        format!(
            " CONFIG-DEPENDENT: closes only through conflicting definitions of {} — no single \
             build configuration contains this cycle.",
            f.config_macros.join(", ")
        )
    } else {
        String::new()
    };
    let message = format!(
        "{}-member recursion cycle: {}. JPL Power of Ten, Rule 1.{}{}",
        cycle.len(),
        chain,
        guard_note,
        config_note
    );
    let witness: Vec<String> = f
        .hops
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let next = &f.hops[(i + 1) % f.hops.len()].func;
            format!("{}:{} {} → {}", h.file.display(), h.line + 1, h.func, next)
        })
        .collect();
    let cycle_refs: Vec<&str> = cycle.iter().map(|s| s.as_str()).collect();
    Diagnostic {
        rule: rule.into(),
        severity,
        file: f.hops.first().map(|h| h.file.to_string_lossy().into_owned()).unwrap_or_default(),
        line: f.hops.first().map(|h| h.line + 1).unwrap_or(1),
        message,
        citation: "JPL Power of Ten, Rule 1".into(),
        // The fingerprint deliberately uses "pot-1-recursion" for BOTH tiers: a cycle keeps its
        // identity (and baseline entry) even if macro classification flips its severity.
        fingerprint: fingerprint("pot-1-recursion", &cycle_refs),
        witness: if witness.is_empty() { None } else { Some(witness) },
    }
}

/// Parse a baseline file: one fingerprint per line, blank lines and `#` comments ignored.
pub fn parse_baseline(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Drop findings whose fingerprint appears in the baseline.
pub fn apply_baseline(diags: Vec<Diagnostic>, baseline: &HashSet<String>) -> Vec<Diagnostic> {
    diags.into_iter().filter(|d| !baseline.contains(&d.fingerprint)).collect()
}

/// CI exit code: 1 if any unsuppressed Error (or, with `strict`, any finding at all), else 0.
pub fn exit_code(diags: &[Diagnostic], strict: bool) -> u8 {
    let fails = diags.iter().any(|d| d.severity == Severity::Error || strict);
    if fails { 1 } else { 0 }
}

/// Iterative pre-order DFS with an explicit stack — recursion depth would track AST nesting,
/// and a pathological source (thousands of nested parens) must not overflow OUR stack while
/// we lint someone else's Rule-1 violations.
fn walk<'t>(node: Node<'t>, f: &mut impl FnMut(Node<'t>)) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        f(n);
        let mut cursor = n.walk();
        let children: Vec<Node<'t>> = n.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phase-0 additions: Rules 2 (unbounded loop), 3 (heap), 5 (assertions),
    /// 8 (token pasting), 9 (multi-indirection) — each fires on its violation and
    /// stays quiet on the compliant twin.
    #[test]
    fn flags_new_power_of_ten_rules() {
        let bad = r#"
#define GLUE(a, b) a##b
int **table_of_ptrs;
static void spin(void) {
    while (1) { do_work(); }
}
static void alloc_it(void) {
    char *p = malloc(64);
    free(p);
}
static int no_asserts_here(int a) {
    int x = a;
    x += 1;
    x += 2;
    x += 3;
    x += 4;
    x += 5;
    x += 6;
    x += 7;
    x += 8;
    x += 9;
    x += 10;
    x += 11;
    x += 12;
    x += 13;
    x += 14;
    x += 15;
    x += 16;
    x += 17;
    return x;
}
"#;
        let d = check_c_source("bad.c", bad).unwrap();
        assert!(d.iter().any(|d| d.rule == "pot-2-unbounded-loop"));
        assert!(d.iter().filter(|d| d.rule == "pot-3-heap").count() >= 2, "malloc AND free");
        assert!(d.iter().any(|d| d.rule == "pot-5-no-assertions" && d.message.contains("no_asserts_here")));
        assert!(d.iter().any(|d| d.rule == "pot-8-token-paste" && d.message.contains("GLUE")));
        assert!(d.iter().any(|d| d.rule == "pot-9-multi-indirection"));

        let good = r#"
#define LIMIT 8
int *one_star;
static void bounded(void) {
    int i;
    for (i = 0; i < LIMIT; i++) { do_work(); }
    while (1) { if (done()) { break; } }
}
static int with_asserts(int a) {
    int x = a;
    assert(a >= 0);
    x += 1;
    x += 2;
    x += 3;
    x += 4;
    x += 5;
    x += 6;
    x += 7;
    x += 8;
    x += 9;
    x += 10;
    x += 11;
    x += 12;
    x += 13;
    x += 14;
    x += 15;
    CFE_ES_ASSERT(x > a);
    return x;
}
"#;
        let d = check_c_source("good.c", good).unwrap();
        for rule in ["pot-2-unbounded-loop", "pot-3-heap", "pot-5-no-assertions", "pot-8-token-paste", "pot-9-multi-indirection"] {
            assert!(!d.iter().any(|x| x.rule == rule), "{rule} misfired on compliant code");
        }
    }

    #[test]
    fn flags_goto_and_long_function() {
        let mut src = String::from("void bad(void) {\n");
        for i in 0..70 {
            src.push_str(&format!("    int x{i} = {i};\n"));
        }
        src.push_str("    goto out;\nout:\n    return;\n}\n");
        let d = check_c_source("bad.c", &src).unwrap();
        assert!(d.iter().any(|d| d.rule == "pot-1-goto"));
        assert!(d.iter().any(|d| d.rule == "pot-4-function-length" && d.message.contains("`bad`")));
    }

    #[test]
    fn clean_code_is_clean() {
        let d = check_c_source("ok.c", "int add(int a, int b) { return a + b; }\n").unwrap();
        assert!(d.is_empty());
    }

    #[test]
    fn fingerprint_survives_edits_above() {
        let src = "void f(void) { goto out;\nout: return; }\n";
        let shifted = format!("/* new\n comment\n block */\nint unrelated;\n{src}");
        let a = check_c_source("x.c", src).unwrap();
        let b = check_c_source("subdir/x.c", &shifted).unwrap();
        let fa = a.iter().find(|d| d.rule == "pot-1-goto").unwrap();
        let fb = b.iter().find(|d| d.rule == "pot-1-goto").unwrap();
        assert_ne!(fa.line, fb.line, "the finding moved");
        assert_eq!(fa.fingerprint, fb.fingerprint, "…but its fingerprint did not");
        assert_eq!(fa.fingerprint.len(), 16);
        assert!(fa.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cycle_fingerprint_is_rotation_canonical() {
        fn fp(members: &[&str]) -> String {
            let cycle = canonical_cycle(&members.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            let refs: Vec<&str> = cycle.iter().map(|s| s.as_str()).collect();
            fingerprint("pot-1-recursion", &refs)
        }
        // Every rotation of the same cycle fingerprints identically…
        assert_eq!(fp(&["B", "C", "A"]), fp(&["A", "B", "C"]));
        assert_eq!(fp(&["C", "A", "B"]), fp(&["A", "B", "C"]));
        // …but a genuinely different cycle (reversed direction) does not.
        assert_ne!(fp(&["A", "C", "B"]), fp(&["A", "B", "C"]));
        // Nor does a different member set.
        assert_ne!(fp(&["A", "B"]), fp(&["A", "B", "C"]));
    }

    #[test]
    fn baseline_suppresses_listed_fingerprints() {
        let d = check_c_source("bl.c", "void f(void) { goto out;\nout: return; }\n").unwrap();
        assert_eq!(d.len(), 1);
        let fp = d[0].fingerprint.clone();
        let baseline = parse_baseline(&format!("# comment\n\n{fp}\n"));
        assert!(apply_baseline(d.clone(), &baseline).is_empty());
        let other = parse_baseline("deadbeefdeadbeef\n");
        assert_eq!(apply_baseline(d, &other).len(), 1);
    }

    #[test]
    fn json_line_shape() {
        let d = check_c_source("j.c", "void f(void) { goto out;\nout: return; }\n").unwrap();
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&d[0]).unwrap()).unwrap();
        assert_eq!(v["rule"], "pot-1-goto");
        assert_eq!(v["severity"], "error");
        assert_eq!(v["file"], "j.c");
        assert_eq!(v["line"], 1);
        assert!(v["message"].is_string());
        assert_eq!(v["fingerprint"].as_str().unwrap().len(), 16);
        assert!(v.get("witness").is_none(), "witness omitted when absent");
    }

    #[test]
    fn exit_code_semantics() {
        let error = check_c_source("e.c", "void f(void) { goto out;\nout: return; }\n").unwrap();
        assert_eq!(exit_code(&error, false), 1);
        assert_eq!(exit_code(&[], false), 0);
        let advice = vec![Diagnostic {
            rule: "pot-1-recursion-possible".into(),
            severity: Severity::Advice,
            file: "a.c".into(),
            line: 1,
            message: "m".into(),
            citation: "JPL Power of Ten, Rule 1".into(),
            fingerprint: "0000000000000000".into(),
            witness: None,
        }];
        assert_eq!(exit_code(&advice, false), 0, "Advice never fails the build");
        assert_eq!(exit_code(&advice, true), 1, "…unless --strict");
    }

    #[test]
    fn project_mode_finds_cross_file_recursion() {
        let dir = std::env::temp_dir().join(format!("cauldron-lint-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.c"), "void g(void);\nvoid f(void) { g(); }\n").unwrap();
        std::fs::write(dir.join("b.c"), "void f(void);\nvoid g(void) { f(); }\n").unwrap();
        let diags = check_project(&dir, &[]);
        assert_eq!(diags.len(), 1, "exactly the f<->g cycle: {diags:?}");
        let d = &diags[0];
        assert_eq!(d.rule, "pot-1-recursion");
        assert_eq!(d.severity, Severity::Error);
        assert!(d.message.contains("2-member recursion cycle"));
        assert!(d.message.contains("JPL Power of Ten, Rule 1."));
        let w = d.witness.as_ref().unwrap();
        assert_eq!(w.len(), 2);
        assert!(w.iter().all(|h| h.contains(" → ") && h.contains(":2 ")), "hops: {w:?}");
        assert_eq!(exit_code(&diags, false), 1);

        // Baseline round-trip: suppressing the fingerprint flips CI green.
        let baseline: HashSet<String> = diags.iter().map(|d| d.fingerprint.clone()).collect();
        let after = apply_baseline(check_project(&dir, &[]), &baseline);
        assert!(after.is_empty(), "re-scan produced the same fingerprint");
        assert_eq!(exit_code(&after, false), 0);

        // --exclude removes the cycle entirely.
        std::fs::create_dir_all(dir.join("vendor")).unwrap();
        std::fs::rename(dir.join("b.c"), dir.join("vendor/b.c")).unwrap();
        let excluded = check_project(&dir, &[std::path::PathBuf::from("vendor")]);
        assert!(excluded.is_empty(), "{excluded:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
