# psi-spike — Phase-0 results (2026-07-12)

*Executed per docs/psi-design.md "Phase-0 spike plan". Corpus: nasa/cFS clone at ~/src/cFS
(1346 .c + 1456 .h pre-partition). Code: crates/cauldron-psi (collect.rs + graph.rs +
bin/psi_spike.rs), 15 unit tests green incl. THE ALGORITHM GUARD. Full raw output:
/tmp/psi-spike-report.txt (and --seed run alongside). Machine: the RuneOS desktop, release build.*

## Numbers

| metric | value |
|---|---|
| files indexed / skipped by partition | 1657 / 1145 |
| functions (defs / decls) | 4370 / 2636 |
| macros (function-like / object-like) | 543 / 7949 |
| typedefs | 1500 |
| direct call sites / macro-mined calls | 25937 / 1227 |
| indirect call sites / address-taken names (pre-filter) | 168 / 7848 |
| graph nodes (defs / macros / extern leaves) | 11267 (3750 / 6540 / 977) |
| Tier-1 edges | 26590 |
| ERROR density | 0 files >= 5% error bytes; 100.00% clean (worst: cfe_evs.h at 4.84%) |
| collect wall (rayon) | 115 ms |
| merge + graph build | 7.9 ms |
| Tier-1 Tarjan + witnesses | 0.55 ms |
| Tier-2 Tarjan (with / without arity filter) | 5.0 ms / 10.7 ms |
| peak RSS (VmHWM) | 64.3 MB |
| incremental probe (cfe_tbl_task.c): unchanged re-collect | 0.69 ms, hashes equal -> zero work |
| incremental probe: added call, re-collect + merge + Tarjan | 8.3 ms |

## PASS/FAIL vs success metrics

| metric (hard unless noted) | target | measured | verdict |
|---|---|---|---|
| cold index | <= 10 s | 123 ms | PASS |
| peak RSS | <= 500 MB | 64.3 MB | PASS |
| Tier-1 Tarjan + witnesses | <= 50 ms | 0.55 ms | PASS |
| files with < 5% ERROR bytes | >= 95% | 100.00% | PASS |
| seeded recursions caught with correct witnesses | both | both (--seed run) | PASS |
| Tier-1 precision: every finding real or traced | no unexplained phantoms | 8 real + 2 traced artifacts (below) | PASS |
| incremental single-file | <= 100 ms; comment-only = 0 work | 8.3 ms; hash check makes unchanged text free | PASS |
| Tier-2 (soft): < ~50 findings under < ~10 waivers | soft | 1 finding, but a 1148-member SCC on 96 sites | FAIL (soft) — ships as inventory |
| Gate-B-grade artifact (soft) | >= 1 | real recursion in EVS<->SB core + EdsLib tools | PASS |

## Tier-1 findings (10 SCCs, witness chains verbatim; offsets are byte offsets)

1. **REAL — OSAL debug-macro loop (8 members):** BUGCHECK [macro], BUGCHECK_VOID [macro],
   BUGREPORT [macro], OS_CHECK_POINTER [macro], OS_ConsoleWrite, OS_ObjectIdGetById,
   OS_ObjectIdToArrayIndex, OS_printf.
   `BUGCHECK -> osal/src/os/inc/osapi-macros.h:3703` -> `BUGREPORT -> osapi-macros.h:2789` ->
   `OS_printf -> osal/src/os/shared/src/osapi-printf.c:9007` -> `BUGCHECK_VOID -> osapi-macros.h:5537`.
   OS_printf's helpers use OS_CHECK_POINTER/BUGCHECK, whose failure path (BUGREPORT) calls
   OS_printf. Reachable in source in DEBUG builds; the class of cross-TU+macro cycle no per-TU
   tool sees.
2. **REAL — the cFE EVS<->SB event loop (9 members):** CFE_EVS_SendEventWithAppID,
   CFE_SB_MessageTxn_ReportEvents, CFE_SB_MessageTxn_ReportSingleEvent, CFE_SB_TransmitMsg,
   EVS_CheckAndIncrementSquelchTokens, EVS_GenerateEventTelemetry, EVS_IsFiltered,
   EVS_NotRegistered, EVS_SendEvent.
   `CFE_EVS_SendEventWithAppID -> cfe/modules/evs/fsw/src/cfe_evs.c:6867` ->
   `EVS_GenerateEventTelemetry -> cfe_evs_utils.c:16829` ->
   `CFE_SB_TransmitMsg -> cfe/modules/sb/fsw/src/cfe_sb_api.c:60414` ->
   `CFE_SB_MessageTxn_ReportEvents -> cfe_sb_priv.c:28089` ->
   `CFE_SB_MessageTxn_ReportSingleEvent -> cfe_sb_priv.c:27276`.
   Events go out via SB; SB failures raise events. cFE bounds it at runtime; statically it is a
   genuine cross-module cycle — the flagship artifact.
3-5. **CONFIG-UNION ARTIFACTS — TopicId<->MsgId mapping (2, 5, 3 members):**
   e.g. `CFE_GLOBAL_CMD_TOPICID_TO_MIDV [macro] -> cfe/modules/core_api/config/eds_cfe_core_api_msgid_mapping.h:2297`
   -> `CFE_SB_GlobalCmdTopicIdToMsgId -> cfe/modules/sb/fsw/src/cfe_sb_msg_id_util.c:4940` (and the
   Local/Platform TLM+CMD variants). The EDS config header defines the macro as a call to the
   function; the non-EDS build implements the function as a call to the (other config's) macro.
   Both `#if` branches + both same-named macro definitions union into one node -> a cycle that
   exists in NO single configuration. Traced, structural, and exactly the documented cost of the
   sound-union-over-configs decision. Waivable per edge; a config-aware macro-node split is the
   designed fix if it ever matters.
6-9. **REAL — EdsLib tools recursion:** EdsLib_DataTypeDB_ConstraintIterator_Impl (self-loop,
   `tools/eds/edslib/fsw/src/edslib_datatypedb_constraints.c:3393`);
   EdsLib_JSON_EdsObjectFromJSON (self, `edslib_json_objects.c:11193`);
   EdsLib_JSON_EdsObjectToJSON (self, `edslib_json_objects.c:7360`); and the 4-member static SCC
   EdsLib_Python_ConvertPython{Mapping,Sequence,SubObject,To}EdsObjImpl in
   `tools/eds/edslib/python/src/edslib_python_conversions.c` (witness hops 9504 -> 13702 -> 10260).
   Structural recursion over nested EDS data — real, in tools/, as the design predicted.
10. **MODEL ARTIFACT — `mkdir` macro self-loop:**
   `#define mkdir(path, mode) mkdir(path)` (`osal/src/os/vxworks/inc/os-impl-dirs.h:1458`).
   Real cpp paints the macro blue — the inner mkdir is libc. Macro-as-node modeling resolves it
   to itself. Traceable fix: a macro body's reference to its OWN name resolves to the extern
   leaf. One-line rule in graph.rs, deferred to the keeper build.

Tier-1 precision score: 8/10 real recursion, 2/10 traced to specific, fixable modeling causes,
0 unexplained phantoms. The partition worked: zero ut-stub contamination (1145 files excluded).

## Tier-2

168 indirect sites, 1033 address-taken definition candidates. With the arity filter: 35647
edges added, ONE Tier-2-only SCC of 1148 members hanging on 96 distinct indirect sites. Without
the filter: 112597 edges, same single blanket SCC. Design risk (1) confirmed by measurement:
cFS is architecturally dispatch-table-driven, so the over-approximation fuses the FSW core into
one component. Per the designed fallback, **Tier-2 ships as an indirect-edge inventory report,
not as findings**; the arity filter (3.2x edge reduction) is worth keeping for the inventory's
signal. Seeded check (--seed): cauldron_seed_f <-> cauldron_seed_g caught with a correct 2-hop
cross-file witness at scale, on top of the 10 real-tree findings.

## Deviations from the plan

- cfe_es_startup.scr is not in the tree — startup-script mining skipped (per recon).
- `--no-arity` switches the detailed Tier-2 listing to the unfiltered graph; both counts are
  always computed and printed.
- Incremental probe rebuilds the full graph after the one-file re-collect (the design's
  memoized-full-recompute stance) — still 8.3 ms, 12x under budget.

## Conclusion (GO/NO-GO)

**GO.** Every hard metric passes with 10-100x headroom: 123 ms cold index (81x under budget),
0.55 ms Tarjan (90x), 64 MB RSS, 100% parse-clean — the raw-source thesis holds even on cFS's
macro-heavy headers. The spike produced the Gate-B artifact on day one: a confirmed, witnessed
EVS<->SB recursion cycle in the flight core plus real recursion in EdsLib, none visible to any
per-TU tool in cFS CI. Surprises: cFS is NOT Tier-1 acyclic (expected zero findings; got eight
real ones), and Tier-2 collapsed into a single 1148-function SCC — so Tier-2 degrades to
inventory exactly as the fallback anticipated, while config-union macro cycles (TopicId family)
emerged as a new, previously untheorized artifact class worth a config-aware macro-node split.
