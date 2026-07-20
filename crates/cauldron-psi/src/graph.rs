//! Linkage-keyed call graph + explicit-stack Tarjan + witness cycles — the Rule-1 (no recursion)
//! engine (docs/psi-design.md, "Rule 1 (no recursion) check").
//!
//! Nodes are linkage-keyed function definitions ([`SymKey`] — statics can never cross-link, so
//! phantom SCCs from duplicate static names are structurally impossible), function-like AND
//! object-like macros as first-class nodes, and external leaves for called-but-undefined names.
//! Tier-1 edges are exact (direct + macro-mined, name/linkage-resolved); Tier-2 adds the sound
//! over-approximation (every indirect site -> the arity-filtered address-taken set). Tarjan is
//! iterative — the linter must not itself recurse.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::collect::{FileFacts, StubKind, TOP_LEVEL};

/// Index into the path table handed to [`CallGraph::build`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// Interned symbol name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sym(pub u32);

/// String interner for symbol names. Clone so a retained [`crate::index::Index`] can be
/// copy-on-write snapshotted (`Arc::make_mut`) by the incremental invalidation path.
#[derive(Default, Clone)]
pub struct Interner {
    map: HashMap<String, u32>,
    names: Vec<String>,
}

impl Interner {
    pub fn intern(&mut self, s: &str) -> Sym {
        if let Some(&i) = self.map.get(s) {
            return Sym(i);
        }
        let i = self.names.len() as u32;
        self.names.push(s.to_string());
        self.map.insert(s.to_string(), i);
        Sym(i)
    }

    pub fn resolve(&self, s: Sym) -> &str {
        &self.names[s.0 as usize]
    }

    /// Non-mutating lookup: the [`Sym`] of `s` if it was ever interned.
    pub fn get(&self, s: &str) -> Option<Sym> {
        self.map.get(s).map(|&i| Sym(i))
    }
}

/// Linkage-aware symbol identity — the single most load-bearing type in the design: C has no
/// overloading, so name+linkage resolution is EXACT for direct calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymKey {
    External(Sym),
    Internal(FileId, Sym),
}

/// Graph node identity: linkage-keyed definitions, macros by name, external leaves by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum NodeKey {
    Def(SymKey),
    Macro(Sym),
    Extern(Sym),
}

/// Merged definition arity for the Tier-2 filter. `Unknown` disables the filter for that node —
/// dropping the filter only ADDS edges (always fail toward over-approximation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    Unset,
    Known(u8),
    Unknown,
}

struct NodeInfo {
    key: NodeKey,
    name: Sym,
    /// First definition site (file, name offset) for reporting.
    def_site: Option<(FileId, usize)>,
    arity: Arity,
    /// Files that define this name. Macro nodes are keyed by NAME ALONE (a macro has no linkage
    /// to key on), so rival config headers collapse into one node whose expansion is a UNION of
    /// bodies no single build ever sees.
    ///
    /// The distinction that matters is CROSS-FILE, not multiplicity. `BUGREPORT` is defined three
    /// times inside osapi-macros.h — three `#if` branches of one header, and walking all branches
    /// is this tool's deliberate policy; the default branch really does ship, so cycles through it
    /// are real. `CFE_PLATFORM_CMD_TOPICID_TO_MIDV` is defined in three DIFFERENT files (the
    /// default header, the EDS header, and a .c fallback); a TU includes exactly one, and which
    /// one is a build-config decision this tool cannot see. Only the cross-file case is a phantom.
    def_files: Vec<FileId>,
}

/// One resolved call edge, carrying its witness site.
#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub to: u32,
    pub file: FileId,
    pub offset: usize,
    pub mined: bool,
    pub indirect: bool,
    /// Set on edges produced by macro collapse: the macro node this edge was expanded through,
    /// when that macro has conflicting definitions. Such an edge exists only in the UNION of
    /// build configurations, so a cycle that needs one is not a cycle any compiler emits.
    via_config_macro: Option<u32>,
}

/// One hop of a witness cycle: `func` calls the NEXT function in the chain at `file:offset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitnessHop {
    pub func: String,
    pub file: String,
    pub offset: usize,
}

/// What kind of cycle a [`Finding`] is — the severity split. A macro is not a runtime stack
/// frame, so a cycle through macro NODES is only real if it survives expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleKind {
    /// Survives macro collapse: a cycle between actual function definitions in the code the
    /// compiler emits. The real Rule-1 violation.
    PostExpansion,
    /// Exists only in the raw macro-reference graph and evaporates under C's expansion rules
    /// (a macro cycle cannot expand — see blue paint in [`CallGraph::build`]). Reported at
    /// advice weight so the textual chain is visible, never as a recursion alarm.
    MacroTextual,
    /// Survives collapse, but ONLY through a macro with conflicting definitions — so the cycle
    /// exists in the union of build configurations and in no single one of them. cFS's
    /// `CFE_SB_CmdTopicIdToMsgId <-> CFE_SB_LocalCmdTopicIdToMsgId` is this: it needs the EDS
    /// body of `CFE_PLATFORM_CMD_TOPICID_TO_MIDV` paired with the function from the NON-EDS
    /// source file, two mutually exclusive builds. Warning weight, naming the macro to blame.
    ConfigDependent,
}

/// One Rule-1 finding: a nontrivial SCC or self-loop, with ONE witness cycle.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Display names of every SCC member, sorted.
    pub members: Vec<String>,
    pub witness: Vec<WitnessHop>,
    /// Tier-2 only: the indirect edges inside the SCC that the cycle depends on,
    /// as (from, to, file, offset).
    pub indirect_deps: Vec<(String, String, String, usize)>,
    pub kind: CycleKind,
    /// [`CycleKind::ConfigDependent`] only: the multiply-defined macros the cycle needs, so the
    /// report can name what to look at instead of leaving a reader to hunt for the phantom.
    pub config_macros: Vec<String>,
    /// SCC member node ids, sorted — graph-internal identity, used to tell a raw macro cycle
    /// apart from the collapsed cycle it is merely the textual shadow of.
    nodes: Vec<u32>,
}

/// Tier-2 result: SCCs that close only through indirect edges (not already Tier-1 findings).
pub struct Tier2Report {
    pub findings: Vec<Finding>,
    /// Indirect edges added on top of Tier-1 (after per-(owner,target) dedup).
    pub indirect_edges_added: usize,
}

/// Aggregate counts for the spike report.
pub struct GraphStats {
    pub nodes: usize,
    pub def_nodes: usize,
    pub macro_nodes: usize,
    pub extern_nodes: usize,
    pub tier1_edges: usize,
    pub indirect_sites: usize,
    pub taken_candidates: usize,
    pub toplevel_calls_skipped: usize,
}

/// The derived whole-program artifact. Never incrementally maintained — full rebuild on demand
/// is ms-scale at cFS size (docs/psi-design.md: incremental SCC is a research problem we refuse
/// to have).
pub struct CallGraph {
    syms: Interner,
    paths: Vec<String>,
    nodes: Vec<NodeInfo>,
    tier1: Vec<Vec<Edge>>,
    /// Tier-1 with every macro node spliced out (see [`CallGraph::collapse_macros`]) — the
    /// adjacency that models what the COMPILER sees. All findings are computed over this.
    collapsed: Vec<Vec<Edge>>,
    /// (owner node, file, offset, best-effort arity) per indirect call site.
    indirect_sites: Vec<(u32, FileId, usize, Option<u8>)>,
    /// Def nodes whose name appears in any file's address-taken set, with merged arity.
    taken_candidates: Vec<(u32, Arity)>,
    toplevel_calls_skipped: usize,
}

impl CallGraph {
    /// Build Tier-1 from per-file facts. `paths[fid.0]` is the display path for `fid`.
    /// Generic over `Borrow<FileFacts>` so callers can hand over owned facts (the spike) or the
    /// retained index's `Arc<FileFacts>` without cloning.
    pub fn build<F: std::borrow::Borrow<FileFacts>>(
        files: &[(FileId, F)],
        paths: &[String],
    ) -> CallGraph {
        let mut g = CallGraph {
            syms: Interner::default(),
            paths: paths.to_vec(),
            nodes: Vec::new(),
            tier1: Vec::new(),
            collapsed: Vec::new(),
            indirect_sites: Vec::new(),
            taken_candidates: Vec::new(),
            toplevel_calls_skipped: 0,
        };
        let mut node_of: HashMap<NodeKey, u32> = HashMap::new();
        // (file slot, stub idx) -> node, for caller attribution.
        let mut stub_node: Vec<HashMap<u32, u32>> = Vec::with_capacity(files.len());

        // Pass 1: nodes for every FnDef (linkage-keyed; same-named external defs merge — union
        // is the safe direction) and every macro.
        for (fid, facts) in files {
            let facts = facts.borrow();
            let mut per_file: HashMap<u32, u32> = HashMap::new();
            for (si, stub) in facts.stubs.iter().enumerate() {
                let key = match stub.kind {
                    StubKind::FnDef => {
                        let sym = g.syms.intern(&stub.name);
                        NodeKey::Def(if stub.is_static {
                            SymKey::Internal(*fid, sym)
                        } else {
                            SymKey::External(sym)
                        })
                    }
                    StubKind::MacroFn | StubKind::MacroObj => {
                        NodeKey::Macro(g.syms.intern(&stub.name))
                    }
                    _ => continue,
                };
                let id = Self::ensure_node(&mut g.nodes, &mut g.tier1, &mut node_of, key);
                per_file.insert(si as u32, id);
                let seen = &mut g.nodes[id as usize].def_files;
                if !seen.contains(fid) {
                    seen.push(*fid);
                }
                if stub.kind == StubKind::FnDef {
                    let info = &mut g.nodes[id as usize];
                    if info.def_site.is_none() {
                        info.def_site = Some((*fid, stub.name_range.start));
                    }
                    info.arity = match (info.arity, stub.arity) {
                        (Arity::Unset, Some(a)) => Arity::Known(a),
                        (Arity::Unset, None) => Arity::Unknown,
                        (Arity::Known(a), Some(b)) if a == b => Arity::Known(a),
                        _ => Arity::Unknown,
                    };
                }
            }
            stub_node.push(per_file);
        }

        // Pass 2: Tier-1 edges (resolution: static-in-file > external-defs > macro > extern
        // leaf), indirect sites, and the address-taken name set.
        let mut taken_names: HashSet<Sym> = HashSet::new();
        for (slot, (fid, facts)) in files.iter().enumerate() {
            let facts = facts.borrow();
            for call in &facts.calls {
                if call.caller_stub == TOP_LEVEL {
                    g.toplevel_calls_skipped += 1;
                    continue;
                }
                let Some(&caller) = stub_node[slot].get(&call.caller_stub) else { continue };
                let sym = g.syms.intern(&call.callee);
                // BLUE PAINT (C11 6.10.3.4p2): a macro's replacement list is NOT rescanned for
                // its own name, so `#define mkdir(p, m) mkdir(p)` calls the FUNCTION mkdir, not
                // itself. Suppressing the Macro candidate here is what makes that wrapper
                // resolve to the extern leaf instead of forging a self-loop.
                let self_ref = g.nodes[caller as usize].key == NodeKey::Macro(sym);
                let target = {
                    let internal = NodeKey::Def(SymKey::Internal(*fid, sym));
                    let external = NodeKey::Def(SymKey::External(sym));
                    let mac = NodeKey::Macro(sym);
                    if let Some(&id) = node_of.get(&internal) {
                        id
                    } else if let Some(&id) = node_of.get(&external) {
                        id
                    } else if let Some(&id) = node_of.get(&mac).filter(|_| !self_ref) {
                        id
                    } else {
                        Self::ensure_node(
                            &mut g.nodes,
                            &mut g.tier1,
                            &mut node_of,
                            NodeKey::Extern(sym),
                        )
                    }
                };
                g.tier1[caller as usize].push(Edge {
                    to: target,
                    file: *fid,
                    offset: call.offset,
                    mined: call.mined_from_macro,
                    indirect: false,
                    via_config_macro: None,
                });
            }
            for &(caller_stub, offset, arity) in &facts.indirect_sites {
                if caller_stub == TOP_LEVEL {
                    continue;
                }
                let Some(&owner) = stub_node[slot].get(&caller_stub) else { continue };
                g.indirect_sites.push((owner, *fid, offset, arity));
            }
            for (name, _ctx) in &facts.address_taken {
                taken_names.insert(g.syms.intern(name));
            }
        }

        // Tier-2 candidate set: definitions whose name is address-taken anywhere.
        for (id, info) in g.nodes.iter().enumerate() {
            if matches!(info.key, NodeKey::Def(_)) && taken_names.contains(&info.name) {
                g.taken_candidates.push((id as u32, info.arity));
            }
        }
        g.taken_candidates.sort_by_key(|&(id, _)| id);
        g.collapsed = g.collapse_macros();
        g
    }

    /// Splice every macro node out of Tier-1, producing the adjacency the COMPILER sees.
    ///
    /// A macro is a textual substitution, not a stack frame: if `f` "calls" macro `M` and `M`'s
    /// body calls `g`, the emitted code has `f` calling `g` at f's expansion site. So each
    /// macro target is replaced by its transitive non-macro expansion, attributed to the call
    /// site where the expansion textually lands.
    ///
    /// Macro nodes keep their own out-edges (harmless — nothing points at them any more), so
    /// [`CallGraph::tier1_successors`] and the raw graph stay intact for diagnostics.
    ///
    /// Blue paint again: a name already being expanded is not re-expanded, so a macro cycle is
    /// unreachable by construction. The DFS drops any edge back to a macro on the active stack,
    /// which both models that rule and makes termination unconditional.
    fn collapse_macros(&self) -> Vec<Vec<Edge>> {
        let is_macro = |id: u32| matches!(self.nodes[id as usize].key, NodeKey::Macro(_));

        // expansion[m] = non-macro targets of macro m, transitively. Memoized; iterative DFS,
        // post-order (a macro is finalized only once every macro it references is).
        let mut expansion: Vec<Option<Vec<u32>>> = vec![None; self.nodes.len()];
        // taint[m] = the multiply-defined macro m's expansion depends on, if any.
        let mut taint: Vec<Option<u32>> = vec![None; self.nodes.len()];
        let mut on_stack = vec![false; self.nodes.len()];
        let mut work: Vec<(u32, usize)> = Vec::new();

        for m in 0..self.nodes.len() as u32 {
            if !is_macro(m) || expansion[m as usize].is_some() {
                continue;
            }
            work.push((m, 0));
            on_stack[m as usize] = true;
            while let Some(&(v, ci)) = work.last() {
                if let Some(e) = self.tier1[v as usize].get(ci) {
                    work.last_mut().expect("just peeked").1 += 1;
                    // Descend into an unexpanded macro; skip ones already on the stack (paint).
                    if is_macro(e.to) && expansion[e.to as usize].is_none() && !on_stack[e.to as usize] {
                        on_stack[e.to as usize] = true;
                        work.push((e.to, 0));
                    }
                } else {
                    work.pop();
                    on_stack[v as usize] = false;
                    let mut out: Vec<u32> = Vec::new();
                    for e in &self.tier1[v as usize] {
                        match (is_macro(e.to), expansion[e.to as usize].as_ref()) {
                            (false, _) => out.push(e.to),
                            // Painted (still on the stack) => contributes nothing.
                            (true, None) => {}
                            (true, Some(inner)) => out.extend(inner.iter().copied()),
                        }
                    }
                    out.sort_unstable();
                    out.dedup();
                    expansion[v as usize] = Some(out);
                    // A macro's expansion is config-dependent if IT has conflicting definitions,
                    // or if anything it expands through does. Remember which macro to blame.
                    taint[v as usize] = if self.nodes[v as usize].def_files.len() > 1 {
                        Some(v)
                    } else {
                        self.tier1[v as usize]
                            .iter()
                            .filter(|e| is_macro(e.to))
                            .find_map(|e| taint[e.to as usize])
                    };
                }
            }
        }

        // Rewrite every non-macro node's out-edges through the expansions.
        let mut adj = self.tier1.clone();
        for (v, edges) in adj.iter_mut().enumerate() {
            if is_macro(v as u32) {
                continue;
            }
            let mut out: Vec<Edge> = Vec::new();
            for e in self.tier1[v].iter() {
                if !is_macro(e.to) {
                    out.push(*e);
                    continue;
                }
                // The expansion happens HERE, at v's site — that is the line a reader must look
                // at to see the recursive call, so every spliced edge carries e's site.
                let via = taint[e.to as usize];
                for &t in expansion[e.to as usize].iter().flatten() {
                    out.push(Edge { to: t, mined: true, via_config_macro: via, ..*e });
                }
            }
            *edges = out;
        }
        adj
    }

    fn ensure_node(
        nodes: &mut Vec<NodeInfo>,
        adj: &mut Vec<Vec<Edge>>,
        node_of: &mut HashMap<NodeKey, u32>,
        key: NodeKey,
    ) -> u32 {
        if let Some(&id) = node_of.get(&key) {
            return id;
        }
        let id = nodes.len() as u32;
        let name = match key {
            NodeKey::Def(SymKey::External(s))
            | NodeKey::Def(SymKey::Internal(_, s))
            | NodeKey::Macro(s)
            | NodeKey::Extern(s) => s,
        };
        nodes.push(NodeInfo { key, name, def_site: None, arity: Arity::Unset, def_files: Vec::new() });
        adj.push(Vec::new());
        node_of.insert(key, id);
        id
    }

    pub fn stats(&self) -> GraphStats {
        let mut def = 0;
        let mut mac = 0;
        let mut ext = 0;
        for n in &self.nodes {
            match n.key {
                NodeKey::Def(_) => def += 1,
                NodeKey::Macro(_) => mac += 1,
                NodeKey::Extern(_) => ext += 1,
            }
        }
        GraphStats {
            nodes: self.nodes.len(),
            def_nodes: def,
            macro_nodes: mac,
            extern_nodes: ext,
            tier1_edges: self.tier1.iter().map(Vec::len).sum(),
            indirect_sites: self.indirect_sites.len(),
            taken_candidates: self.taken_candidates.len(),
            toplevel_calls_skipped: self.toplevel_calls_skipped,
        }
    }

    /// Tier-1 findings: every nontrivial SCC or self-loop over exact edges, each with ONE
    /// witness cycle. Empty result = the acyclicity proof (over Tier-1).
    ///
    /// Computed over the MACRO-COLLAPSED graph, so members are functions and a cycle here is a
    /// cycle in the emitted code. Cycles that live only in the raw macro-reference graph are
    /// appended as [`CycleKind::MacroTextual`] rather than silently dropped.
    pub fn tier1_findings(&self) -> Vec<Finding> {
        // The three tiers are nested subgraphs, each strictly weaker than the last, and each
        // cycle is attributed to the STRONGEST graph it survives in:
        //   real      = collapsed minus config-dependent edges — one build actually emits this
        //   collapsed = + edges through multiply-defined macros — only the union of builds does
        //   tier1     = + macro nodes as frames — only the raw text does
        let real: Vec<Vec<Edge>> = self
            .collapsed
            .iter()
            .enumerate()
            .map(|(v, es)| {
                if self.is_config_node(v as u32) {
                    return Vec::new(); // body is ambiguous — see is_config_node
                }
                es.iter().filter(|e| e.via_config_macro.is_none()).copied().collect()
            })
            .collect();

        let mut out = self.findings_on(&real, None, CycleKind::PostExpansion);
        let mut claimed: HashSet<u32> = out.iter().flat_map(|f| f.nodes.iter().copied()).collect();

        // A weaker tier only reports a cycle whose members are ALL still unclaimed — otherwise
        // it is the same cycle seen through a blurrier lens, and re-reporting it at lower
        // severity reads as a downgrade of a real finding.
        for (adj, kind) in
            [(&self.collapsed, CycleKind::ConfigDependent), (&self.tier1, CycleKind::MacroTextual)]
        {
            for mut f in self.findings_on(adj, None, kind) {
                if f.nodes.iter().any(|n| claimed.contains(n)) {
                    continue;
                }
                claimed.extend(f.nodes.iter().copied());
                if kind == CycleKind::ConfigDependent {
                    let in_scc: HashSet<u32> = f.nodes.iter().copied().collect();
                    let mut blame: Vec<String> = f
                        .nodes
                        .iter()
                        .flat_map(|&v| adj[v as usize].iter())
                        .filter(|e| in_scc.contains(&e.to))
                        .filter_map(|e| e.via_config_macro)
                        .chain(f.nodes.iter().copied().filter(|&v| self.is_config_node(v)))
                        .map(|m| self.display(m))
                        .collect();
                    blame.sort();
                    blame.dedup();
                    f.config_macros = blame;
                }
                out.push(f);
            }
        }
        out.sort_by(|a, b| a.members.cmp(&b.members));
        out
    }

    /// Tier-2: add indirect-site -> address-taken-candidate edges (arity-filtered when both ends
    /// are known and `arity_filter` is set), re-run Tarjan, and report SCCs that are NOT already
    /// Tier-1 findings — each naming the indirect edges it depends on.
    pub fn tier2(&self, arity_filter: bool) -> Tier2Report {
        let (adj, added) = self.tier2_adj(arity_filter);
        let tier1_sets: HashSet<Vec<u32>> =
            scc_node_sets(&find_sccs(&self.collapsed), &self.collapsed);
        let findings = self.findings_on(&adj, Some(&tier1_sets), CycleKind::PostExpansion);
        Tier2Report { findings, indirect_edges_added: added }
    }

    /// Tier-1 adjacency plus indirect edges, deduped per (owner, target) — one representative
    /// site per edge is enough for a witness.
    fn tier2_adj(&self, arity_filter: bool) -> (Vec<Vec<Edge>>, usize) {
        let mut adj = self.collapsed.clone();
        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        let mut added = 0usize;
        for &(owner, file, offset, site_arity) in &self.indirect_sites {
            for &(cand, cand_arity) in &self.taken_candidates {
                if arity_filter {
                    if let (Some(a), Arity::Known(b)) = (site_arity, cand_arity) {
                        if a != b {
                            continue; // both ends known and disagree — drop, else over-approximate
                        }
                    }
                }
                if seen.insert((owner, cand)) {
                    adj[owner as usize].push(Edge {
                        to: cand,
                        file,
                        offset,
                        mined: false,
                        indirect: true,
                        via_config_macro: None,
                    });
                    added += 1;
                }
            }
        }
        (adj, added)
    }

    /// Is this node's BODY config-dependent — i.e. is the same name defined in more than one
    /// file? For an external function that means mutually exclusive build variants (cFS compiles
    /// EITHER cfe_sb_msg_id_util.c OR cfe_sb_eds_msg_id_util.c, and both define
    /// CFE_SB_TlmTopicIdToMsgId), so the merged node's out-edges are a union of bodies no single
    /// build has. Statics are file-keyed and can never collide, so this only ever fires on
    /// External defs and on macros.
    fn is_config_node(&self, id: u32) -> bool {
        self.nodes[id as usize].def_files.len() > 1
    }

    /// Human-readable node name; statics carry their defining file for disambiguation.
    fn display(&self, id: u32) -> String {
        let info = &self.nodes[id as usize];
        let name = self.syms.resolve(info.name);
        match info.key {
            NodeKey::Def(SymKey::Internal(fid, _)) => {
                format!("{name} [static {}]", self.path(fid))
            }
            NodeKey::Macro(_) => format!("{name} [macro]"),
            NodeKey::Extern(_) => format!("{name} [extern]"),
            NodeKey::Def(SymKey::External(_)) => name.to_string(),
        }
    }

    fn path(&self, fid: FileId) -> &str {
        self.paths.get(fid.0 as usize).map_or("<unknown>", |p| p.as_str())
    }

    /// Findings over one adjacency; `exclude` drops SCCs whose exact node set already appears in
    /// Tier-1 (used by Tier-2 to report Tier-2-ONLY cycles).
    fn findings_on(
        &self,
        adj: &[Vec<Edge>],
        exclude: Option<&HashSet<Vec<u32>>>,
        kind: CycleKind,
    ) -> Vec<Finding> {
        let mut out = Vec::new();
        for scc in find_sccs(adj) {
            let nontrivial =
                scc.len() > 1 || adj[scc[0] as usize].iter().any(|e| e.to == scc[0]);
            if !nontrivial {
                continue;
            }
            let mut sorted = scc.clone();
            sorted.sort_unstable();
            if exclude.is_some_and(|t1| t1.contains(&sorted)) {
                continue;
            }
            let in_scc: HashSet<u32> = sorted.iter().copied().collect();
            let mut members: Vec<String> = sorted.iter().map(|&v| self.display(v)).collect();
            members.sort();
            let mut indirect_deps = Vec::new();
            for &v in &sorted {
                for e in &adj[v as usize] {
                    if e.indirect && in_scc.contains(&e.to) {
                        indirect_deps.push((
                            self.display(v),
                            self.display(e.to),
                            self.path(e.file).to_string(),
                            e.offset,
                        ));
                    }
                }
            }
            let witness = self.witness_cycle(adj, &sorted);
            out.push(Finding { members, witness, indirect_deps, kind, config_macros: Vec::new(), nodes: sorted });
        }
        out.sort_by(|a, b| a.members.cmp(&b.members));
        out
    }

    /// ONE witness cycle for an SCC: shortest loop through the smallest member, as the chain of
    /// actual call sites closing it. Iterative BFS — no recursion anywhere in this crate.
    fn witness_cycle(&self, adj: &[Vec<Edge>], scc: &[u32]) -> Vec<WitnessHop> {
        let in_scc: HashSet<u32> = scc.iter().copied().collect();
        let start = *scc.iter().min().expect("SCC is never empty");
        if let Some(e) = adj[start as usize].iter().find(|e| e.to == start) {
            return vec![self.hop(start, e)];
        }
        let mut parent: HashMap<u32, (u32, Edge)> = HashMap::new();
        let mut visited: HashSet<u32> = HashSet::from([start]);
        let mut queue: VecDeque<u32> = VecDeque::from([start]);
        while let Some(v) = queue.pop_front() {
            for e in &adj[v as usize] {
                if !in_scc.contains(&e.to) {
                    continue;
                }
                if e.to == start {
                    // Close the loop: start -> ... -> v -> start.
                    let mut rev = vec![self.hop(v, e)];
                    let mut cur = v;
                    while cur != start {
                        let (p, pe) = parent[&cur];
                        rev.push(self.hop(p, &pe));
                        cur = p;
                    }
                    rev.reverse();
                    return rev;
                }
                if visited.insert(e.to) {
                    parent.insert(e.to, (v, *e));
                    queue.push_back(e.to);
                }
            }
        }
        Vec::new() // unreachable for a real SCC; empty witness is tolerated by callers
    }

    fn hop(&self, from: u32, e: &Edge) -> WitnessHop {
        WitnessHop {
            func: self.display(from),
            file: self.path(e.file).to_string(),
            offset: e.offset,
        }
    }

    /// Tier-1 successor display names of every node named `func` (test/diagnostic aid).
    pub fn tier1_successors(&self, func: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (id, info) in self.nodes.iter().enumerate() {
            if self.syms.resolve(info.name) == func {
                for e in &self.tier1[id] {
                    out.push(self.display(e.to));
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// Node sets (sorted) of the nontrivial SCCs in `adj`, for Tier-1/Tier-2 set comparison.
fn scc_node_sets(sccs: &[Vec<u32>], adj: &[Vec<Edge>]) -> HashSet<Vec<u32>> {
    sccs.iter()
        .filter(|scc| scc.len() > 1 || adj[scc[0] as usize].iter().any(|e| e.to == scc[0]))
        .map(|scc| {
            let mut s = scc.clone();
            s.sort_unstable();
            s
        })
        .collect()
}

/// Explicit-stack Tarjan (iterative — a Power-of-Ten tool must not itself recurse).
fn find_sccs(adj: &[Vec<Edge>]) -> Vec<Vec<u32>> {
    const UNSET: u32 = u32::MAX;
    let n = adj.len();
    let mut index = vec![UNSET; n];
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<u32> = Vec::new();
    let mut next = 0u32;
    let mut sccs: Vec<Vec<u32>> = Vec::new();
    // The recursion stack, made explicit: (node, next child edge to examine).
    let mut work: Vec<(u32, usize)> = Vec::new();

    for root in 0..n as u32 {
        if index[root as usize] != UNSET {
            continue;
        }
        work.push((root, 0));
        while let Some(&(v, ci)) = work.last() {
            let vu = v as usize;
            if ci == 0 {
                index[vu] = next;
                low[vu] = next;
                next += 1;
                stack.push(v);
                on_stack[vu] = true;
            }
            if let Some(e) = adj[vu].get(ci) {
                work.last_mut().expect("just peeked").1 += 1;
                let w = e.to as usize;
                if index[w] == UNSET {
                    work.push((e.to, 0));
                } else if on_stack[w] {
                    low[vu] = low[vu].min(index[w]);
                }
            } else {
                work.pop();
                if let Some(&(p, _)) = work.last() {
                    let pu = p as usize;
                    low[pu] = low[pu].min(low[vu]);
                }
                if low[vu] == index[vu] {
                    let mut scc = Vec::new();
                    loop {
                        let w = stack.pop().expect("Tarjan stack underflow");
                        on_stack[w as usize] = false;
                        scc.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(scc);
                }
            }
        }
    }
    sccs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::{CallSite, Stub};

    fn stub(name: &str, kind: StubKind, is_static: bool, arity: Option<u8>) -> Stub {
        Stub {
            name: name.into(),
            kind,
            is_static,
            byte_range: 0..0,
            name_range: 0..0,
            name_line: 0,
            arity,
            params_range: None,
            param_ranges: Vec::new(),
            parent: None,
            ty: None,
        }
    }

    fn call(caller: u32, callee: &str, offset: usize, mined: bool) -> CallSite {
        CallSite {
            caller_stub: caller,
            callee: callee.into(),
            offset,
            mined_from_macro: mined,
            args_range: None,
            arg_ranges: Vec::new(),
        }
    }

    fn facts(stubs: Vec<Stub>, calls: Vec<CallSite>) -> FileFacts {
        FileFacts { stubs, calls, ..FileFacts::default() }
    }

    /// THE ALGORITHM GUARD (docs/psi-design.md, Day 2-3): cross-file mutual recursion, a direct
    /// self-loop, same-named statics that must NOT merge, and a macro-transitive chain.
    #[test]
    fn algorithm_guard() {
        // a.c: f() calls g; h() calls h; static dup() calls ext_target; k() calls M;
        //      macro M's body calls f.
        let a = facts(
            vec![
                stub("f", StubKind::FnDef, false, Some(0)),
                stub("h", StubKind::FnDef, false, Some(0)),
                stub("dup", StubKind::FnDef, true, Some(0)),
                stub("k", StubKind::FnDef, false, Some(0)),
                stub("M", StubKind::MacroFn, false, Some(0)),
            ],
            vec![
                call(0, "g", 10, false),
                call(1, "h", 20, false),
                call(2, "ext_target", 30, false),
                call(3, "M", 40, false),
                call(4, "f", 50, true),
            ],
        );
        // b.c: g() calls f (closing the cross-file cycle); ext_target() calls dup — which must
        // resolve to b.c's OWN static dup (no outgoing calls), never a.c's.
        let b = facts(
            vec![
                stub("g", StubKind::FnDef, false, Some(0)),
                stub("ext_target", StubKind::FnDef, false, Some(0)),
                stub("dup", StubKind::FnDef, true, Some(0)),
            ],
            vec![call(0, "f", 100, false), call(1, "dup", 110, false)],
        );
        let files = vec![(FileId(0), a), (FileId(1), b)];
        let paths = vec!["a.c".to_string(), "b.c".to_string()];
        let g = CallGraph::build(&files, &paths);
        let findings = g.tier1_findings();

        // 1. Cross-file mutual recursion f <-> g, with the correct 2-step witness.
        let mutual = findings
            .iter()
            .find(|f| f.members.contains(&"f".to_string()) && f.members.contains(&"g".to_string()))
            .expect("f<->g SCC not found");
        assert_eq!(mutual.members.len(), 2);
        assert_eq!(mutual.witness.len(), 2);
        assert_eq!(
            (mutual.witness[0].func.as_str(), mutual.witness[0].file.as_str(), mutual.witness[0].offset),
            ("f", "a.c", 10)
        );
        assert_eq!(
            (mutual.witness[1].func.as_str(), mutual.witness[1].file.as_str(), mutual.witness[1].offset),
            ("g", "b.c", 100)
        );

        // 2. Direct self-recursion h -> h.
        let selfloop = findings
            .iter()
            .find(|f| f.members == vec!["h".to_string()])
            .expect("h self-loop not found");
        assert_eq!(selfloop.witness.len(), 1);
        assert_eq!(selfloop.witness[0].offset, 20);

        // 3. NO phantom SCC through the two same-named statics.
        assert!(
            !findings.iter().any(|f| f.members.iter().any(|m| m.contains("dup"))),
            "phantom static SCC: {findings:?}"
        );
        assert_eq!(findings.len(), 2);

        // 4. Macro-transitive chain resolves: k -> M -> f.
        assert!(g.tier1_successors("k").contains(&"M [macro]".to_string()));
        assert!(g.tier1_successors("M").contains(&"f".to_string()));
    }

    /// Tier-2: an indirect site + an address-taken callee closing a cycle is reported as a
    /// Tier-2-only finding naming the indirect edge; the arity filter suppresses it when the
    /// counts disagree.
    #[test]
    fn tier2_indirect_cycle_and_arity_filter() {
        // c.c: t1() has an indirect call site of arity 2; rt(x, y) calls t1; rt is in a
        // dispatch table (address-taken). Tier-1 is acyclic; Tier-2 closes t1 -> rt -> t1.
        let c = FileFacts {
            stubs: vec![
                stub("t1", StubKind::FnDef, false, Some(0)),
                stub("rt", StubKind::FnDef, false, Some(2)),
            ],
            calls: vec![call(1, "t1", 200, false)],
            address_taken: vec![("rt".into(), crate::collect::CTX_INIT_LIST)],
            indirect_sites: vec![(0, 210, Some(2))],
            ..FileFacts::default()
        };
        let files = vec![(FileId(0), c)];
        let paths = vec!["c.c".to_string()];
        let g = CallGraph::build(&files, &paths);
        assert!(g.tier1_findings().is_empty(), "Tier-1 must be acyclic here");

        let with = g.tier2(true);
        assert_eq!(with.findings.len(), 1, "arity 2 site matches arity-2 rt");
        let f = &with.findings[0];
        assert!(f.members.contains(&"rt".to_string()) && f.members.contains(&"t1".to_string()));
        assert_eq!(f.indirect_deps.len(), 1);
        assert_eq!(f.indirect_deps[0].3, 210, "finding names the indirect edge site");

        // Same graph but the site takes 3 args: filtered out WITH the filter, kept WITHOUT.
        let mut c2 = files[0].1.clone();
        c2.indirect_sites = vec![(0, 210, Some(3))];
        let files2 = vec![(FileId(0), c2)];
        let g2 = CallGraph::build(&files2, &paths);
        assert!(g2.tier2(true).findings.is_empty());
        assert_eq!(g2.tier2(false).findings.len(), 1);
    }

    /// THE MACRO SEVERITY SPLIT, both halves, taken verbatim from cFS.
    ///
    /// Half 1 (blue paint): `#define mkdir(path, mode) mkdir(path)` — a macro wrapping the
    /// same-named function. C never rescans a replacement list for the macro's own name, so this
    /// calls the FUNCTION. It must produce NO finding at all.
    ///
    /// Half 2 (collapse): OS_printf calls BUGCHECK_VOID -> BUGCHECK -> BUGREPORT -> OS_printf.
    /// Three macro hops, zero macro cycles — but post-expansion OS_printf calls ITSELF, and that
    /// recursion is real. It must survive as a PostExpansion self-loop naming the FUNCTION.
    #[test]
    fn macro_cycles_split_by_expansion_not_by_spelling() {
        let f = facts(
            vec![
                stub("mkdir", StubKind::MacroFn, false, Some(2)),
                stub("OS_printf", StubKind::FnDef, false, Some(1)),
                stub("BUGCHECK_VOID", StubKind::MacroFn, false, Some(1)),
                stub("BUGCHECK", StubKind::MacroFn, false, Some(2)),
                stub("BUGREPORT", StubKind::MacroFn, false, Some(1)),
            ],
            vec![
                call(0, "mkdir", 10, true),          // macro body -> ITSELF, textually
                call(1, "BUGCHECK_VOID", 20, false), // OS_printf's body
                call(2, "BUGCHECK", 30, true),
                call(3, "BUGREPORT", 40, true),
                call(4, "OS_printf", 50, true), // closes it, post-expansion
            ],
        );
        let g = CallGraph::build(&[(FileId(0), f)], &["osal.c".into()]);
        let findings = g.tier1_findings();

        // Blue paint: the wrapper resolved to the extern function, so there is no cycle to find.
        assert!(
            !findings.iter().any(|f| f.members.iter().any(|m| m.contains("mkdir"))),
            "mkdir wrapper must not be a cycle: {findings:?}"
        );
        assert!(g.tier1_successors("mkdir").contains(&"mkdir [extern]".to_string()));

        // Collapse: exactly one finding, a REAL self-loop on the function.
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].members, vec!["OS_printf".to_string()]);
        assert_eq!(findings[0].kind, CycleKind::PostExpansion);
        // The witness points at the BUGCHECK_VOID site in OS_printf — the line a human must read.
        assert_eq!(findings[0].witness.len(), 1);
        assert_eq!(findings[0].witness[0].offset, 20);
    }

    /// CONFIG-UNION, both halves, from cFS's SB topic-id mapping. cFS compiles EITHER
    /// cfe_sb_msg_id_util.c OR cfe_sb_eds_msg_id_util.c, never both, and both define
    /// CFE_SB_TlmTopicIdToMsgId / CFE_SB_GlobalTlmTopicIdToMsgId. The non-EDS body has
    /// Tlm -> Global; the EDS body has Global -> Tlm. Merge the mutually exclusive variants and a
    /// cycle appears that NEITHER build contains — so it must not be an error.
    ///
    /// The contrast that keeps this honest: BUGREPORT is *also* multiply defined, but all three
    /// definitions are `#if` branches of ONE file, the default branch ships, and cycles through
    /// it are real. Cross-file is the phantom signal, not multiplicity — see `is_config_node`.
    #[test]
    fn cross_file_redefinition_is_config_dependent_not_recursion() {
        // Non-EDS variant: Tlm() calls Global().
        let plain = facts(
            vec![
                stub("CFE_SB_TlmTopicIdToMsgId", StubKind::FnDef, false, Some(2)),
                stub("CFE_SB_GlobalTlmTopicIdToMsgId", StubKind::FnDef, false, Some(1)),
            ],
            vec![call(0, "CFE_SB_GlobalTlmTopicIdToMsgId", 10, false)],
        );
        // EDS variant: SAME two functions, but Global() calls Tlm() — closing a phantom loop.
        let eds = facts(
            vec![
                stub("CFE_SB_TlmTopicIdToMsgId", StubKind::FnDef, false, Some(2)),
                stub("CFE_SB_GlobalTlmTopicIdToMsgId", StubKind::FnDef, false, Some(1)),
            ],
            vec![call(1, "CFE_SB_TlmTopicIdToMsgId", 20, false)],
        );
        let g = CallGraph::build(
            &[(FileId(0), plain), (FileId(1), eds)],
            &["msg_id_util.c".into(), "eds_msg_id_util.c".into()],
        );
        let findings = g.tier1_findings();

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            findings[0].kind,
            CycleKind::ConfigDependent,
            "a cycle that needs two mutually exclusive build variants is not recursion"
        );
        // And it names what to blame, so nobody has to hunt for the phantom.
        assert!(
            findings[0].config_macros.iter().any(|m| m.contains("TlmTopicIdToMsgId")),
            "{:?}",
            findings[0].config_macros
        );
    }

    /// Same-named EXTERNAL defs merge into one node (union — the safe direction).
    #[test]
    fn external_defs_union() {
        let a = facts(vec![stub("osal_impl", StubKind::FnDef, false, Some(0))], vec![]);
        let b = facts(
            vec![
                stub("osal_impl", StubKind::FnDef, false, Some(0)),
                stub("user", StubKind::FnDef, false, Some(0)),
            ],
            vec![call(1, "osal_impl", 5, false)],
        );
        let g = CallGraph::build(
            &[(FileId(0), a), (FileId(1), b)],
            &["impl_a.c".into(), "impl_b.c".into()],
        );
        let s = g.stats();
        assert_eq!(s.def_nodes, 2, "two osal_impl defs merge into one External node");
        assert_eq!(s.tier1_edges, 1);
        assert_eq!(s.extern_nodes, 0);
    }
}
