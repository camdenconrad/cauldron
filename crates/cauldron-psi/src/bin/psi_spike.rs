//! psi-spike — the Phase-0 metrics harness over a real cFS tree (docs/psi-design.md,
//! "Phase-0 spike plan"). Walks the partitioned tree, extracts [`FileFacts`] in parallel,
//! builds the linkage-keyed call graph, runs Tier-1/Tier-2 Tarjan, and prints the numbers
//! table + PASS/FAIL against the Gate-B success metrics.
//!
//! Usage: psi-spike [ROOT] [--seed] [--no-arity]
//!   ROOT       cFS checkout (default /home/user/src/cFS)
//!   --seed     inject the synthetic cross-file mutual recursion on top of the real tree
//!   --no-arity print the detailed Tier-2 listing from the UNFILTERED graph (both counts are
//!              always computed)

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cauldron_psi::collect::{self, FileFacts, StubKind};
use cauldron_psi::graph::{CallGraph, FileId, Finding};
use rayon::prelude::*;

const DEFAULT_ROOT: &str = "/home/user/src/cFS";

const SEED_A: &str = "void cauldron_seed_g(void);\nvoid cauldron_seed_f(void)\n{\n    cauldron_seed_g();\n}\n";
const SEED_B: &str = "void cauldron_seed_f(void);\nvoid cauldron_seed_g(void)\n{\n    cauldron_seed_f();\n}\n";

fn main() {
    let mut root = PathBuf::from(DEFAULT_ROOT);
    let mut seed = false;
    let mut no_arity = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--seed" => seed = true,
            "--no-arity" => no_arity = true,
            other => root = PathBuf::from(other),
        }
    }
    println!("psi-spike: root={} seed={seed} no_arity={no_arity}", root.display());
    println!();

    // ---- Walk: the crate's own scan universe (project_files — gitignore respected, hidden
    // shown, .git skipped, ut-stub partition applied). No spike-private walk rules anymore.
    let (kept, skipped_partition) = cauldron_psi::project::project_files(&root, &[]);
    println!("files indexed:          {}", kept.len());
    println!("skipped by partition:   {skipped_partition}");

    // ---- Collect (rayon, pure per-file) ----
    let t = Instant::now();
    let mut per_file: Vec<(String, usize, FileFacts)> = kept
        .par_iter()
        .map(|p| {
            let bytes = std::fs::read(p).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            let rel = p.strip_prefix(&root).unwrap_or(p).display().to_string();
            (rel, text.len(), collect::file_facts(&text))
        })
        .collect();
    let collect_wall = t.elapsed();

    if seed {
        per_file.push(("<seed>/seed_a.c".into(), SEED_A.len(), collect::file_facts(SEED_A)));
        per_file.push(("<seed>/seed_b.c".into(), SEED_B.len(), collect::file_facts(SEED_B)));
    }

    // ---- Corpus stats ----
    let mut fn_defs = 0usize;
    let mut fn_decls = 0usize;
    let mut macro_fns = 0usize;
    let mut macro_objs = 0usize;
    let mut typedefs = 0usize;
    let mut direct_calls = 0usize;
    let mut mined_calls = 0usize;
    let mut indirect_sites = 0usize;
    let mut aggregates: std::collections::BTreeMap<String, usize> = Default::default();
    let mut taken_names: HashSet<&str> = HashSet::new();
    for (_, _, f) in &per_file {
        for s in &f.stubs {
            match s.kind {
                StubKind::FnDef => fn_defs += 1,
                StubKind::FnDecl => fn_decls += 1,
                StubKind::MacroFn => macro_fns += 1,
                StubKind::MacroObj => macro_objs += 1,
                StubKind::Typedef => typedefs += 1,
                // The spike reports the original five counters; aggregates and members are
                // summarized separately below rather than being folded into them.
                other => *aggregates.entry(format!("{other:?}")).or_insert(0usize) += 1,
            }
        }
        for c in &f.calls {
            if c.mined_from_macro {
                mined_calls += 1;
            } else {
                direct_calls += 1;
            }
        }
        indirect_sites += f.indirect_sites.len();
        for (n, _) in &f.address_taken {
            taken_names.insert(n);
        }
    }
    println!("functions:              {fn_defs} defs / {fn_decls} decls");
    println!("macros:                 {macro_fns} function-like / {macro_objs} object-like");
    println!("typedefs:               {typedefs}");
    println!("direct call sites:      {direct_calls}");
    println!("macro-mined calls:      {mined_calls}");
    println!("indirect call sites:    {indirect_sites}");
    println!("address-taken names:    {} (distinct, pre-filter)", taken_names.len());
    println!();

    // ---- ERROR density (index health) ----
    let mut dens: Vec<(f64, &str)> = per_file
        .iter()
        .map(|(p, len, f)| (f.error_bytes as f64 / (*len).max(1) as f64, p.as_str()))
        .collect();
    dens.sort_by(|a, b| b.0.total_cmp(&a.0));
    let dirty = dens.iter().filter(|(d, _)| *d >= 0.05).count();
    let clean_pct = 100.0 * (per_file.len() - dirty) as f64 / per_file.len().max(1) as f64;
    println!("ERROR density:          {dirty} file(s) >= 5% error bytes; {clean_pct:.2}% clean");
    for (d, p) in dens.iter().take(5) {
        if *d > 0.0 {
            println!("  worst: {:>6.2}%  {p}", d * 100.0);
        }
    }
    println!();

    // ---- Merge + graph build (single-threaded) ----
    let t = Instant::now();
    let paths: Vec<String> = per_file.iter().map(|(p, _, _)| p.clone()).collect();
    let files: Vec<(FileId, FileFacts)> = per_file
        .iter()
        .enumerate()
        .map(|(i, (_, _, f))| (FileId(i as u32), f.clone()))
        .collect();
    let graph = CallGraph::build(&files, &paths);
    let merge_wall = t.elapsed();

    let gs = graph.stats();
    println!(
        "graph:                  {} nodes ({} defs / {} macros / {} extern leaves), {} tier-1 edges",
        gs.nodes, gs.def_nodes, gs.macro_nodes, gs.extern_nodes, gs.tier1_edges
    );
    println!(
        "                        {} indirect sites in graph, {} address-taken def candidates, {} top-level calls skipped",
        gs.indirect_sites, gs.taken_candidates, gs.toplevel_calls_skipped
    );
    println!();

    // ---- Tier-1 Tarjan + witnesses ----
    let t = Instant::now();
    let tier1 = graph.tier1_findings();
    let tier1_wall = t.elapsed();
    println!("== Tier-1 findings (exact) ==");
    if tier1.is_empty() {
        println!("NONE — Tier-1 acyclic; that is the acyclicity PROOF headline");
    } else {
        for f in &tier1 {
            print_finding(f);
        }
    }
    println!();

    // ---- Tier-2 (both with and without the arity filter) ----
    let t = Instant::now();
    let t2_filtered = graph.tier2(true);
    let tier2_wall = t.elapsed();
    let t = Instant::now();
    let t2_unfiltered = graph.tier2(false);
    let tier2_nofilter_wall = t.elapsed();

    println!("== Tier-2 (indirect, sound over-approximation) ==");
    println!(
        "findings WITH arity filter:    {} (Tier-2-only SCCs; {} indirect edges added)",
        t2_filtered.findings.len(),
        t2_filtered.indirect_edges_added
    );
    println!(
        "findings WITHOUT arity filter: {} ({} indirect edges added)",
        t2_unfiltered.findings.len(),
        t2_unfiltered.indirect_edges_added
    );
    let detailed = if no_arity { &t2_unfiltered } else { &t2_filtered };
    let mut by_size: Vec<&Finding> = detailed.findings.iter().collect();
    by_size.sort_by_key(|f| std::cmp::Reverse(f.members.len()));
    let mut all_sites: HashSet<(&str, usize)> = HashSet::new();
    for f in &detailed.findings {
        for (_, _, file, off) in &f.indirect_deps {
            all_sites.insert((file.as_str(), *off));
        }
    }
    println!(
        "distinct indirect sites the findings hang on: {} (filter={})",
        all_sites.len(),
        !no_arity
    );
    println!("top {} Tier-2-only SCCs by size:", by_size.len().min(10));
    for f in by_size.iter().take(10) {
        let sites: HashSet<(&str, usize)> =
            f.indirect_deps.iter().map(|(_, _, fp, o)| (fp.as_str(), *o)).collect();
        let head: Vec<&str> = f.members.iter().take(4).map(String::as_str).collect();
        println!(
            "  size {:>4}  via {:>3} indirect site(s)  [{}{}]",
            f.members.len(),
            sites.len(),
            head.join(", "),
            if f.members.len() > 4 { ", ..." } else { "" }
        );
    }
    println!();

    // ---- Seed check ----
    let mut seed_pass = None;
    if seed {
        let found = tier1.iter().find(|f| {
            f.members.contains(&"cauldron_seed_f".to_string())
                && f.members.contains(&"cauldron_seed_g".to_string())
        });
        let ok = found.is_some_and(|f| {
            f.witness.len() == 2 && f.witness.iter().all(|h| h.file.starts_with("<seed>/"))
        });
        seed_pass = Some(ok);
        println!(
            "== Seed check == {}",
            if ok { "seeded cross-file mutual recursion CAUGHT with correct witness" } else { "seeded recursion NOT caught" }
        );
        if let Some(f) = found {
            print_finding(f);
        }
        println!();
    }

    // ---- Incremental probe: one mid-sized cfe .c ----
    let mut cfe_cs: Vec<(&PathBuf, u64)> = kept
        .iter()
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("c")
                && p.strip_prefix(&root)
                    .map(|r| r.starts_with("cfe"))
                    .unwrap_or(false)
        })
        .map(|p| (p, std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)))
        .collect();
    cfe_cs.sort_by_key(|&(_, len)| len);
    let mut incr_wall = None;
    let mut hash_probe_ok = false;
    if let Some(&(probe_path, _)) = cfe_cs.get(cfe_cs.len() / 2) {
        let rel = probe_path.strip_prefix(&root).unwrap_or(probe_path).display().to_string();
        let slot = paths.iter().position(|p| *p == rel).expect("probe file is indexed");
        let bytes = std::fs::read(probe_path).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        println!("== Incremental probe == {rel} ({} bytes)", text.len());

        // (a) Unchanged text: both hashes must match the indexed facts — zero index work.
        let t = Instant::now();
        let refacts = collect::file_facts(&text);
        let recollect_wall = t.elapsed();
        hash_probe_ok = refacts.interface_hash == files[slot].1.interface_hash
            && refacts.body_hash == files[slot].1.body_hash;
        println!(
            "unchanged re-collect:   {} in {recollect_wall:.2?} -> zero index work",
            if hash_probe_ok { "hashes equal" } else { "HASH MISMATCH (purity bug)" }
        );

        // (b) Simulated added call: re-collect + merge + Tier-1 Tarjan, timed end to end.
        let modified =
            format!("{text}\nvoid __probe(void)\n{{\n    CFE_ES_PerfLogAdd(1, 0);\n}}\n");
        let t = Instant::now();
        let newfacts = collect::file_facts(&modified);
        let changed = newfacts.body_hash != files[slot].1.body_hash
            || newfacts.interface_hash != files[slot].1.interface_hash;
        let mut files2 = files.clone();
        files2[slot].1 = newfacts;
        let graph2 = CallGraph::build(&files2, &paths);
        let findings2 = graph2.tier1_findings();
        let d = t.elapsed();
        incr_wall = Some(d);
        println!(
            "added-call re-collect + merge + Tier-1 Tarjan: {d:.2?} (hashes changed: {changed}, findings: {})",
            findings2.len()
        );
        println!();
    } else {
        println!("== Incremental probe == SKIPPED (no cfe .c files found)");
        println!();
    }

    // ---- Timings + RSS ----
    let rss_mb = peak_rss_mb();
    println!("== Timings ==");
    println!("collect (rayon):        {collect_wall:.2?}");
    println!("merge + graph build:    {merge_wall:.2?}");
    println!("Tier-1 Tarjan+witness:  {tier1_wall:.2?}");
    println!("Tier-2 Tarjan (filter): {tier2_wall:.2?}");
    println!("Tier-2 Tarjan (nofilt): {tier2_nofilter_wall:.2?}");
    match rss_mb {
        Some(mb) => println!("peak RSS (VmHWM):       {mb:.1} MB"),
        None => println!("peak RSS (VmHWM):       unavailable"),
    }
    println!();

    // ---- PASS/FAIL vs the design's success metrics ----
    let cold = collect_wall + merge_wall;
    println!("== Success metrics (docs/psi-design.md) ==");
    verdict("cold index <= 10 s", cold <= Duration::from_secs(10), &format!("{cold:.2?}"));
    match rss_mb {
        Some(mb) => verdict("peak RSS <= 500 MB", mb <= 500.0, &format!("{mb:.1} MB")),
        None => println!("[ ?? ] peak RSS <= 500 MB: VmHWM unavailable"),
    }
    verdict(
        "Tier-1 Tarjan <= 50 ms",
        tier1_wall <= Duration::from_millis(50),
        &format!("{tier1_wall:.2?}"),
    );
    verdict(
        ">= 95% files < 5% ERROR bytes",
        clean_pct >= 95.0,
        &format!("{clean_pct:.2}% clean"),
    );
    match seed_pass {
        Some(ok) => verdict("seeded recursions caught", ok, "see seed check above"),
        None => println!("[ -- ] seeded recursions caught: N/A (run with --seed)"),
    }
    match incr_wall {
        Some(d) => {
            let ok = d <= Duration::from_millis(100) && hash_probe_ok;
            verdict("incremental single-file <= 100 ms", ok, &format!("{d:.2?}"));
        }
        None => println!("[ ?? ] incremental single-file <= 100 ms: probe skipped"),
    }
}

fn print_finding(f: &Finding) {
    println!("SCC ({} member(s)): {}", f.members.len(), f.members.join(", "));
    for h in &f.witness {
        println!("    {} -> next at {}:{}", h.func, h.file, h.offset);
    }
    for (from, to, file, off) in &f.indirect_deps {
        println!("    depends on indirect edge {from} -> {to} at {file}:{off}");
    }
}

fn verdict(name: &str, ok: bool, detail: &str) {
    println!("[{}] {name}: {detail}", if ok { "PASS" } else { "FAIL" });
}

/// Peak resident set from /proc/self/status (VmHWM, kB).
fn peak_rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024.0)
}
