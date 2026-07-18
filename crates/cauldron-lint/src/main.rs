//! cauldron-lint CLI — run the Power-of-Ten checks over C files or a whole project.
//!
//! Usage:
//!   cauldron-lint <file.c>... [options]                 # single-file checks (Rules 1 & 4)
//!   cauldron-lint --project <root> [--exclude <rel>]... # whole-program recursion (Rule 1, PSI)
//!
//! Options:
//!   --json                   one JSON object per line
//!   --baseline <file>        suppress findings whose fingerprint is listed (one per line)
//!   --write-baseline <file>  write current fingerprints and exit 0
//!   --strict                 Advice findings also fail the build
//!
//! Exit code: 1 if any unsuppressed Error finding (any finding with --strict), 2 on usage/IO
//! error, else 0. All non-trivial logic lives in the library so tests never spawn the binary.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;

use cauldron_lint::Diagnostic;

struct Args {
    files: Vec<String>,
    project: Option<PathBuf>,
    compile_db: Option<PathBuf>,
    excludes: Vec<PathBuf>,
    json: bool,
    strict: bool,
    baseline: Option<PathBuf>,
    write_baseline: Option<PathBuf>,
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: cauldron-lint <file.c>... [--json] [--strict] [--baseline <f>] [--write-baseline <f>]\n       cauldron-lint --project <root> [--exclude <rel>]... [same options]\n       cauldron-lint --project <root> --compile-db <compile_commands.json>   # exact: analyze the preprocessed build"
    );
    ExitCode::from(2)
}

fn parse_args(argv: &[String]) -> Result<Args, ()> {
    let mut args = Args {
        files: Vec::new(),
        project: None,
        compile_db: None,
        excludes: Vec::new(),
        json: false,
        strict: false,
        baseline: None,
        write_baseline: None,
    };
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => args.json = true,
            "--strict" => args.strict = true,
            "--project" => args.project = Some(PathBuf::from(it.next().ok_or(())?)),
            "--compile-db" => args.compile_db = Some(PathBuf::from(it.next().ok_or(())?)),
            "--exclude" => args.excludes.push(PathBuf::from(it.next().ok_or(())?)),
            "--baseline" => args.baseline = Some(PathBuf::from(it.next().ok_or(())?)),
            "--write-baseline" => args.write_baseline = Some(PathBuf::from(it.next().ok_or(())?)),
            s if s.starts_with("--") => return Err(()),
            s => args.files.push(s.to_owned()),
        }
    }
    if args.project.is_none() && args.files.is_empty() {
        return Err(());
    }
    Ok(args)
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Ok(args) = parse_args(&argv) else { return usage() };

    // Collect findings.
    let mut diags: Vec<Diagnostic> = Vec::new();
    if let (Some(root), Some(db)) = (&args.project, &args.compile_db) {
        match cauldron_lint::check_compile_db(root, db) {
            Ok((d, cov)) => {
                diags.extend(d);
                // ALWAYS report coverage, and shout when it is partial: findings from a half-read
                // program are a false-negative machine, and "no recursion found" must never be
                // an artifact of code we failed to preprocess.
                if cov.is_complete() {
                    eprintln!("cauldron-lint: analyzed {}/{} TUs", cov.analyzed, cov.total);
                } else {
                    eprintln!(
                        "cauldron-lint: WARNING — INCOMPLETE SCAN: only {}/{} TUs preprocessed \
                         ({} failed; the build tree is likely stale or partly ungenerated). \
                         Findings below cover the analyzed TUs ONLY — absence of a finding proves \
                         nothing. Re-run the build, then re-scan.",
                        cov.analyzed, cov.total, cov.failed
                    );
                }
            }
            Err(e) => {
                eprintln!("cauldron-lint: compile db {}: {e}", db.display());
                std::process::exit(2);
            }
        }
    } else if let Some(root) = &args.project {
        diags.extend(cauldron_lint::check_project(root, &args.excludes));
    }
    for f in &args.files {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{f}: {e}");
                return ExitCode::from(2);
            }
        };
        match cauldron_lint::check_c_source(f, &src) {
            Ok(d) => diags.extend(d),
            Err(e) => {
                eprintln!("{f}: {e}");
                return ExitCode::from(2);
            }
        }
    }

    if let Some(path) = &args.write_baseline {
        let mut lines: Vec<&str> = diags.iter().map(|d| d.fingerprint.as_str()).collect();
        lines.sort_unstable();
        lines.dedup();
        if let Err(e) = std::fs::write(path, lines.join("\n") + "\n") {
            eprintln!("{}: {e}", path.display());
            return ExitCode::from(2);
        }
        eprintln!("wrote {} fingerprint(s) to {}", lines.len(), path.display());
        return ExitCode::SUCCESS;
    }

    if let Some(path) = &args.baseline {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                return ExitCode::from(2);
            }
        };
        let set: HashSet<String> = cauldron_lint::parse_baseline(&text);
        diags = cauldron_lint::apply_baseline(diags, &set);
    }

    for d in &diags {
        if args.json {
            println!("{}", serde_json::to_string(d).unwrap());
        } else {
            println!("{}:{}: [{}] {} ({}) [{}]", d.file, d.line, d.rule, d.message, d.citation, d.fingerprint);
            if let Some(w) = &d.witness {
                for hop in w {
                    println!("    {hop}");
                }
            }
        }
    }

    ExitCode::from(cauldron_lint::exit_code(&diags, args.strict))
}
