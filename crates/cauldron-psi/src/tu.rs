//! Exact mode: build the call graph from what the COMPILER sees, not from what the text says.
//!
//! The heuristic scan ([`crate::project::scan_project`]) reads sources as they sit on disk. That
//! is the right trade for an IDE — no build required, instant, config-agnostic — but it has to
//! guess at three things the preprocessor decides, and each guess is a false-positive class:
//!
//!   1. MACROS AS FRAMES. A macro is a textual substitution, so a cycle through a macro node is
//!      only real if it survives expansion (`#define mkdir(p, m) mkdir(p)` is not recursion;
//!      OS_printf -> BUGCHECK_VOID -> BUGCHECK -> BUGREPORT -> OS_printf is).
//!   2. `#if` BRANCHES. The scan walks every branch, so it sees the union of configurations.
//!   3. CROSS-FILE REDEFINITION. cFS compiles EITHER cfe_sb_msg_id_util.c OR its EDS twin; both
//!      define CFE_SB_TlmTopicIdToMsgId, and merging the two variants fabricates a cycle that
//!      neither build contains.
//!
//! Given a compile_commands.json, all three stop being guesses. We run the real preprocessor with
//! the real flags over the TUs the build really compiles, and extract facts from the expanded
//! text. There are no macro nodes (nothing to collapse), no inactive branches (cpp dropped them),
//! and no rival definitions (only one variant is in the TU list). What is left is the call graph
//! of the emitted program.
//!
//! Facts are mapped back to ORIGIN files through cpp's `# <line> "<file>"` linemarkers, so every
//! finding still points at a line a human can open — the expansion is an implementation detail,
//! never something the user has to read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;

use crate::collect::{self, CallSite, FileFacts, Stub, TOP_LEVEL};
use crate::index::Index;
use crate::project::{rule1_findings, ProjectScan};

/// One compile_commands.json entry, reduced to what preprocessing needs.
#[derive(Debug, Clone)]
pub struct CompileEntry {
    pub directory: PathBuf,
    pub file: PathBuf,
    /// The compiler argv, already stripped of `-c` / `-o <path>` and with `-E` inserted.
    pub args: Vec<String>,
}

/// Parse a compile_commands.json into preprocessor invocations.
///
/// Point this at a per-TARGET database (cFS: `build-native_std/native/default_cpu1/`), never at a
/// top-level one that concatenates several targets — a merged database re-creates the very
/// cross-variant union this mode exists to eliminate.
pub fn load_compile_db(path: &Path) -> anyhow::Result<Vec<CompileEntry>> {
    let text = std::fs::read_to_string(path)?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let arr = json.as_array().ok_or_else(|| anyhow::anyhow!("compile db is not an array"))?;

    let mut out = Vec::new();
    for e in arr {
        let directory = PathBuf::from(e["directory"].as_str().unwrap_or_default());
        let file = PathBuf::from(e["file"].as_str().unwrap_or_default());

        // Either form is legal per the spec: "arguments" (a list) or "command" (a shell string).
        let raw: Vec<String> = match (&e["arguments"], &e["command"]) {
            (serde_json::Value::Array(a), _) => {
                a.iter().filter_map(|v| v.as_str().map(String::from)).collect()
            }
            (_, serde_json::Value::String(c)) => shell_split(c),
            _ => continue,
        };
        if raw.is_empty() {
            continue;
        }

        // -c compiles, -o writes an object: we want neither. -E stops after preprocessing, and
        // -P would strip the linemarkers we navigate by, so it must NOT be added.
        let mut args: Vec<String> = Vec::with_capacity(raw.len());
        let mut skip_next = false;
        for (i, a) in raw.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            match a.as_str() {
                "-c" => {}
                "-o" => skip_next = true,
                _ if i == 0 => args.push(a.clone()),
                _ => args.push(a.clone()),
            }
        }
        args.insert(1, "-E".into());
        out.push(CompileEntry { directory, file, args });
    }
    Ok(out)
}

/// Minimal POSIX-ish argv split for the `command` form: honors single/double quotes and
/// backslash escapes, which is everything a compile database realistically contains.
fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                cur.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Run the preprocessor for one TU. `None` if it fails — a TU that does not preprocess cannot be
/// analyzed, and inventing facts for it would be worse than admitting the gap (the caller counts
/// these and reports them).
fn preprocess(entry: &CompileEntry) -> Option<String> {
    let out = Command::new(&entry.args[0])
        .args(&entry.args[1..])
        .current_dir(&entry.directory)
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Where one line of preprocessed text came from: an index into the origin-path table, plus the
/// 0-based line within that file.
#[derive(Clone, Copy)]
struct Origin {
    file: u32,
    line: u32,
}

/// Strip cpp linemarkers out of `text`, replacing each with an EMPTY line so line numbering is
/// preserved exactly, and return (clean text, per-line origin, origin path table).
///
/// Blanking rather than deleting is load-bearing twice over. It keeps the clean text's line `i`
/// aligned with `origins[i]`, and it keeps tree-sitter out of trouble: cpp emits markers in the
/// middle of expressions (`if (!(String != \n# 269 "..." 3\n ((void *)0)\n...))`), and a stray
/// `#` directive inside an expression parses as garbage.
fn split_linemarkers(text: &str) -> (String, Vec<Origin>, Vec<PathBuf>) {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut path_ix: HashMap<String, u32> = HashMap::new();
    let mut origins: Vec<Origin> = Vec::new();
    let mut clean = String::with_capacity(text.len());

    let mut cur = Origin { file: u32::MAX, line: 0 };
    for raw in text.lines() {
        if let Some((file, line)) = parse_linemarker(raw) {
            let id = *path_ix.entry(file.to_string()).or_insert_with(|| {
                paths.push(PathBuf::from(file));
                (paths.len() - 1) as u32
            });
            cur = Origin { file: id, line: line.saturating_sub(1) }; // markers are 1-based
            clean.push('\n');
            origins.push(cur);
            continue;
        }
        clean.push_str(raw);
        clean.push('\n');
        origins.push(cur);
        cur.line += 1;
    }
    (clean, origins, paths)
}

/// `# 269 "/path/to/file.c" 3` (or the `#line 269 "..."` spelling) -> ("/path/to/file.c", 269).
fn parse_linemarker(line: &str) -> Option<(&str, u32)> {
    let rest = line.strip_prefix('#')?.trim_start();
    let rest = rest.strip_prefix("line ").unwrap_or(rest);
    let mut it = rest.splitn(2, '"');
    let num: u32 = it.next()?.trim().parse().ok()?;
    let file = it.next()?.split('"').next()?;
    Some((file, num))
}

/// Byte offset of the start of each 0-based line, for mapping an origin line back to a real
/// offset in the real file (the offset every witness/guard consumer re-reads from disk).
fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0usize];
    v.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    v
}

/// Accumulates facts from every TU, keyed by ORIGIN file, deduplicating as it goes: a header's
/// static inline is extracted once per TU that includes it (1400+ times, in cFS), and the same
/// definition at the same origin line is the same definition.
#[derive(Default)]
struct Accum {
    facts: HashMap<PathBuf, FileFacts>,
    /// (origin file, name, origin line) -> index into that file's `stubs`.
    stub_ix: HashMap<(PathBuf, String, usize), u32>,
    /// (origin file, caller stub idx, callee, offset) — call-site dedup across TUs.
    call_seen: std::collections::HashSet<(PathBuf, u32, String, usize)>,
}

/// Extract facts from ONE preprocessed TU and fold them into `acc`, mapped back to origin files.
/// `line_cache` memoizes each origin file's text + line-start table across TUs.
fn fold_tu(
    acc: &mut Accum,
    expanded: &str,
    root: &Path,
    line_cache: &mut HashMap<PathBuf, Option<Vec<usize>>>,
) {
    let (clean, origins, paths) = split_linemarkers(expanded);
    let facts = collect::file_facts(&clean);
    let clean_starts = line_starts(&clean);

    let line_of = |off: usize| -> usize {
        clean_starts.partition_point(|&s| s <= off).saturating_sub(1)
    };

    // An origin we can place in a real file under the project root, and one the scan is allowed
    // to look at. System headers are dropped (their bodies are not ours to audit, and their calls
    // degrade to extern leaves — exactly how the graph already models anything undefined), and so
    // is the ut-stub PARTITION: cFS's test stubs deliberately redefine real symbols, so admitting
    // them merges the stub OS_MutSemTake with the real one and manufactures enormous phantom
    // SCCs through UtAssert. A compile database happily lists the unit-test targets, so this
    // filter is just as necessary here as in the heuristic walk.
    let resolve = |off: usize,
                   line_cache: &mut HashMap<PathBuf, Option<Vec<usize>>>|
     -> Option<(PathBuf, usize, usize)> {
        let o = *origins.get(line_of(off))?;
        if o.file == u32::MAX {
            return None;
        }
        let path = paths.get(o.file as usize)?.clone();
        if !path.starts_with(root) || !crate::project::is_scan_source(root, &path, &[]) {
            return None;
        }
        let starts = line_cache
            .entry(path.clone())
            .or_insert_with(|| std::fs::read_to_string(&path).ok().map(|t| line_starts(&t)))
            .as_ref()?;
        let line = (o.line as usize).min(starts.len().saturating_sub(1));
        Some((path, line, starts[line]))
    };

    // Pass 1: stubs. Remember where each TU-local stub index landed, so calls can find their
    // caller after the split across origin files.
    let mut placed: Vec<Option<(PathBuf, u32)>> = Vec::with_capacity(facts.stubs.len());
    for s in &facts.stubs {
        let Some((path, line, off)) = resolve(s.name_range.start, line_cache) else {
            placed.push(None);
            continue;
        };
        let key = (path.clone(), s.name.clone(), line);
        if let Some(&ix) = acc.stub_ix.get(&key) {
            placed.push(Some((path, ix)));
            continue;
        }
        let f = acc.facts.entry(path.clone()).or_default();
        let ix = f.stubs.len() as u32;
        f.stubs.push(Stub {
            name: s.name.clone(),
            kind: s.kind,
            is_static: s.is_static,
            // Spans are origin-line anchored: the expanded byte ranges are meaningless in the
            // real file, and every consumer of these wants "the line this thing is on".
            byte_range: off..off,
            name_range: off..off,
            name_line: line,
            arity: s.arity,
            // Same reason as the ranges above: expanded-TU spans do not exist in the real file,
            // and a rewriter must not be handed coordinates that look usable but are not.
            params_range: None,
            param_ranges: Vec::new(),
        });
        acc.stub_ix.insert(key, ix);
        placed.push(Some((path, ix)));
    }

    // Pass 2: calls, attributed to the caller's origin file. A call expanded from a macro carries
    // the linemarker of the EXPANSION SITE — cpp says the recursive OS_printf() is at
    // osapi-printf.c:269 — which is precisely the line we want to show.
    for c in &facts.calls {
        if c.caller_stub == TOP_LEVEL {
            continue;
        }
        let Some(Some((caller_path, caller_ix))) = placed.get(c.caller_stub as usize).cloned()
        else {
            continue;
        };
        // Use the call's own origin line when it lands in the caller's file; otherwise fall back
        // to the caller's own line so the offset is always valid in the file it is filed under.
        let off = match resolve(c.offset, line_cache) {
            Some((p, _, off)) if p == caller_path => off,
            _ => acc.facts[&caller_path].stubs[caller_ix as usize].name_range.start,
        };
        if !acc.call_seen.insert((caller_path.clone(), caller_ix, c.callee.clone(), off)) {
            continue;
        }
        acc.facts.entry(caller_path).or_default().calls.push(CallSite {
            caller_stub: caller_ix,
            callee: c.callee.clone(),
            offset: off,
            mined_from_macro: false, // nothing is "mined" here — cpp already expanded it
            // Offsets here are RE-ATTRIBUTED from the translation unit back to an including
            // file, so any span from the TU text would point at the wrong bytes. Withhold them
            // rather than hand a rewriter coordinates that look valid and are not.
            args_range: None,
            arg_ranges: Vec::new(),
        });
    }

    // Pass 3: indirect sites, same attribution.
    for &(caller, off, arity) in &facts.indirect_sites {
        if caller == TOP_LEVEL {
            continue;
        }
        let Some(Some((path, ix))) = placed.get(caller as usize).cloned() else { continue };
        let off = match resolve(off, line_cache) {
            Some((p, _, o)) if p == path => o,
            _ => acc.facts[&path].stubs[ix as usize].name_range.start,
        };
        let f = acc.facts.entry(path).or_default();
        if !f.indirect_sites.contains(&(ix, off, arity)) {
            f.indirect_sites.push((ix, off, arity));
        }
    }

    // Pass 4: address-taken names. The graph unions these into one project-wide set, so which
    // file they are filed under has no effect on the result — park them on the TU's own file.
    if let Some((path, ..)) = resolve(0, line_cache) {
        let f = acc.facts.entry(path).or_default();
        for (name, ctx) in &facts.address_taken {
            match f.address_taken.iter_mut().find(|(n, _)| n == name) {
                Some((_, c)) => *c |= ctx,
                None => f.address_taken.push((name.clone(), *ctx)),
            }
        }
    }
}

/// How much of the build this scan actually saw. A Rule-1 checker that quietly analyzes half a
/// program and reports "2 findings" is worse than useless — the answer looks clean because the
/// code was never read. Coverage is returned alongside the findings and reported unconditionally,
/// so an incomplete scan can never be mistaken for a clean one.
#[derive(Debug, Clone, Copy)]
pub struct DbCoverage {
    /// TUs in the database, after the ut-stub partition.
    pub total: usize,
    /// TUs the preprocessor accepted — the ones actually analyzed.
    pub analyzed: usize,
    /// TUs the preprocessor rejected (usually a stale build tree with missing generated headers).
    pub failed: usize,
}

impl DbCoverage {
    pub fn is_complete(&self) -> bool {
        self.failed == 0
    }
}

/// Exact whole-program scan driven by a compile database.
///
/// Same [`ProjectScan`] shape as the heuristic path, so every consumer (lint, LSP, panels) works
/// unchanged — the findings are simply true of the built program rather than of the text.
pub fn scan_compile_db(root: &Path, db: &Path) -> anyhow::Result<(ProjectScan, DbCoverage)> {
    let started = Instant::now();
    let all = load_compile_db(db)?;

    // Drop the unit-test TUs before we even preprocess them (see `resolve`): the stub library is
    // a parallel universe of same-named functions, and none of it ships.
    let entries: Vec<CompileEntry> = all
        .into_iter()
        .filter(|e| crate::project::is_scan_source(root, &e.file, &[]))
        .collect();

    // Preprocessing is pure and independent per TU: fan out, fold in deterministic (sorted) order
    // so FileIds and witness selection do not depend on thread scheduling.
    let mut expanded: Vec<(PathBuf, Option<String>)> = entries
        .par_iter()
        .map(|e| (e.file.clone(), preprocess(e)))
        .collect();
    expanded.sort_by(|a, b| a.0.cmp(&b.0));

    let failed = expanded.iter().filter(|(_, t)| t.is_none()).count();
    let coverage = DbCoverage {
        total: entries.len(),
        analyzed: entries.len() - failed,
        failed,
    };

    let mut acc = Accum::default();
    let mut line_cache: HashMap<PathBuf, Option<Vec<usize>>> = HashMap::new();
    for (_, text) in expanded.iter() {
        if let Some(text) = text {
            fold_tu(&mut acc, text, root, &mut line_cache);
        }
    }

    let mut files: Vec<(PathBuf, Arc<FileFacts>)> =
        acc.facts.into_iter().map(|(p, f)| (p, Arc::new(f))).collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let indexed = files.len();

    let index = Arc::new(Index::build(files));
    let findings = rule1_findings(&index, root);
    Ok((
        ProjectScan {
            findings,
            index,
            files_indexed: indexed,
            // TUs the preprocessor rejected: a real coverage gap, counted rather than papered over.
            files_skipped: failed,
            elapsed: started.elapsed(),
        },
        coverage,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linemarkers_are_blanked_and_mapped() {
        // Exactly cpp's shape: a marker splitting an expression mid-line.
        let tu = "# 1 \"/proj/a.c\"\nint f(void)\n{\n    return\n# 9 \"/proj/b.h\"\n   g();\n}\n";
        let (clean, origins, paths) = split_linemarkers(tu);

        // Line count preserved (markers -> blank lines), so origins[i] indexes clean's line i.
        assert_eq!(clean.lines().count(), tu.lines().count());
        assert!(!clean.contains('#'), "markers must not reach tree-sitter: {clean:?}");
        // ...and the expression still parses, which is the whole point of blanking.
        assert!(collect::file_facts(&clean).calls.iter().any(|c| c.callee == "g"));

        assert_eq!(paths, vec![PathBuf::from("/proj/a.c"), PathBuf::from("/proj/b.h")]);
        // `int f(void)` is line 1 of the clean text and line 0 (0-based) of a.c.
        assert_eq!((origins[1].file, origins[1].line), (0, 0));
        // `   g();` follows the b.h marker: file 1, line 8 (0-based for the marker's `9`).
        assert_eq!((origins[5].file, origins[5].line), (1, 8));
    }

    #[test]
    fn shell_split_handles_quotes_and_escapes() {
        assert_eq!(
            shell_split(r#"cc -DX="a b" -I/p\ q file.c"#),
            vec!["cc", "-DX=a b", "-I/p q", "file.c"]
        );
    }

    #[test]
    fn compile_db_entry_becomes_a_preprocessor_invocation() {
        let dir = std::env::temp_dir().join(format!("cauldron-tu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("compile_commands.json");
        std::fs::write(
            &db,
            r#"[{"directory":"/b","file":"/s/a.c","command":"gcc -c -o CMakeFiles/a.o -I/inc /s/a.c"}]"#,
        )
        .unwrap();

        let e = &load_compile_db(&db).unwrap()[0];
        // -E inserted; -c and `-o <path>` both gone (the -o VALUE must not survive as an input).
        assert_eq!(e.args, vec!["gcc", "-E", "-I/inc", "/s/a.c"]);
        assert_eq!(e.directory, PathBuf::from("/b"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
