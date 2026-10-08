# GM-531: an agreeing answer through a `pub use` re-export is not a contradiction

Design note for GM-531, slice S1. Base: `release-4.2.0` at `4b6bd27`.
Background: `gm-489-structural-semantic-duplicate.md` §3 (rules R1-R3) and
§3.2 item 1; measurements from GM-494/S1 (93 CALLS) and GM-497/S12 (182
REFERENCES).

## 1. The problem, for someone with no context

g-mesh builds every `CALLS`/`REFERENCES` edge in two tiers:

1. **Structural** (the Rust plugin, `plugins/rust/src/extractor/bodies.rs`).
   For a typed field read `n.file_path` or a typed method call `p.last()`,
   `tail_in` (bodies.rs:1307) addresses the member **at the container the
   type was imported from**. For a cross-file type that is a placeholder
   node `P`, for example scope `g_mesh::protocol::types`, key
   `protocol::types::WireNode.file_path`. The edge `X = F -> P` is sent to
   core. Core's linker (`graph::symbol_links::link`, l.1237) then follows
   the re-export: `Resolver::resolve` (l.1476) walks re-export chains, and
   `Resolver::through_head` (l.1623) walks the head `WireNode` through
   `pub use g_mesh_wire::*` to the declaring item `D` (`g_mesh_wire`,
   `WireNode.file_path`). It moves `X`'s `toId` from `P` to `D` and keeps
   `P` in `edges.linkedFrom`. The final graph is correct.
2. **Semantic** (the SDK's LSP bridge, `plugins/sdk/src/lsp/bridge.rs`).
   rust-analyzer answers "this is `D`". `record_answer` (l.2420) then decides
   whether the answer agrees with `X`. Rule R2 (l.2486-2507) has two tests:
   - `lands_on_it`: `X.to_id == D.id` in the **plugin's own** copy of the
     file graph;
   - `prospective == replaced`: the edge id the answer would get, using
     `address_of(D)` (l.1790, scope `g_mesh_wire`) fed to `answer_ids`
     (l.1295), equals `X.id`.

   The plugin's copy still has `X -> P`, because linking happened in core.
   The address `address_of(D)` is `g_mesh_wire / WireNode.file_path`, which
   differs from `P`'s. Both tests fail. The bridge treats the answer as a
   **contradiction** (l.2524-2530): it records a semantic edge `E` and
   retracts `X`.

The consequence is the GM-489 window. After an edit to `F`'s file, the
reparse re-sends `X` (same id, linked to `D` again), while `E` survives
(`file_rows::widen` keeps semantic edges). So there are two rows for one read
or call until the next semantic pass retracts `X` again, and indefinitely if
that pass fails. Measured on g-mesh: 182 of 1440 typed field reads and 93 of
2695 typed receiver calls. That is every re-export-headed receiver site
rust-analyzer answers. 15 + 73 of these sites go through the cross-crate glob
`pub use g_mesh_wire::*`.

## 2. Decision: core tells the bridge what it linked, before the bridge decides

**Today.** `semanticPass` carries only `filePaths` (`wire/src/lib.rs:563`).
The bridge cannot see core's link results, and by design it never guesses
them.

**Change.** When core builds a `semanticPass` request, it reads, for the
pass's scope, every **syntactic edge that the linker moved**
(`edges.linkedFrom IS NOT NULL`) together with its current `toId`. It sends
these pairs in a new field, `linkedEdges: [{edgeId, toId}]`. The SDK stores
them on the `SdkIndex` for that pass. R2 gains a third agreement test:

> the answer agrees with `X` when core linked `X` onto exactly the answered
> declaration: `index.linked_target(X.id) == Some(D.id)`.

Nothing else in R1-R3 changes. An agreeing answer still lands in `upheld` and
`confirmed`, so R1 re-sends `X` and R3 drops any semantic edge from an
earlier pass in the same process that covers `X`.

**Example.** `core/src/storage/qualified_path.rs:49` `suffixes` calls
`path.suffix_from(start)`. `QualifiedPath` is imported from
`crate::protocol::types`, which has `pub use g_mesh_wire::*`. At the
`semanticPass`, core sends `{edgeId: X, toId: <g_mesh_wire QualifiedPath::suffix_from>}`.
rust-analyzer answers that same node, so the new test matches. The result:
no semantic edge, no retraction, one row (`X`, `source = syntactic`).

**Consequence.** Every case where core's linker and rust-analyzer agree is
now agreement, whatever re-export (named, glob, cross-crate, or multi-hop)
the linker walked. A real contradiction still behaves as today: if the linker
put `X` on `D'` and the answer is `D`, then `linked_target(X) = D' != D`.
The same holds for an `X` the linker did not link (unresolved or ambiguous):
it has no entry.

### 2.1 Placement: core and bridge, not the Rust plugin

| Option | What it is | Verdict |
|---|---|---|
| **A. Plugin addresses the declaring item** | `tail_in` follows `pub use` itself, so `P` already has `D`'s address. | Rejected. The plugin extracts one file against its own module model. It cannot see another crate's declarations behind `pub use g_mesh_wire::*`, so 88 of 275 sites (15 calls + 73 fields) would remain. It would also duplicate the linker's re-export rules (shadowing, `MAX_REEXPORT_DEPTH`, GM-490/GM-496 per-language rules) in the plugin. |
| **B. Bridge walks re-exports over `SdkIndex`** | The bridge does its own head walk. | Rejected. It is a second linker, and the bridge's design forbids it ("never guesses"). |
| **C. Core dedups after the bridge** (gm-489 §3.2 option 2) | Core drops `E` and un-retracts `X` when both land on one node after linking. | Rejected. Core would have to apply, link, compare and undo inside one unit, before it knows `E`'s target. The bridge's `emitted` set would then disagree with the store. It fixes the symptom, not the decision. |
| **D. (chosen) Core ships its link result; bridge decides with it** | gm-489 §3.2 option 1. | Uses the linker's real answer. No new resolution code, and it covers glob and cross-crate cases for free. |

"Core resolves the placeholder" needs **no new head walk**. The linker has
already walked it (`through_head`) when the reparse diff was committed. Core
only reads the result back from `edges.linkedFrom/toId`.
`Resolver::through_head` and `Resolver::resolve` stay byte-identical.

### 2.2 Ordering: when core's resolution runs relative to the bridge's decision

There are three ways a pass is reached. In all three, linking is committed
before core builds the `semanticPass` request:

1. **Per-file pass after an edit.** `watcher::apply::apply_file_change_in`
   (core/src/watcher/apply.rs:120) first runs the `fileChanged` round trip.
   `round_trip` (l.402) commits the diff through `apply_file_diff_linked` →
   `index_store::apply_and_link` → `link_diff`. Only after that does it call
   `apply_semantic_pass_in` (l.226). The new query runs at the top of
   `apply_semantic_pass_in`, before the `ControlMessage::SemanticPass` is
   built (l.242). Everything is in one open writer unit, so nothing can
   relink in between.
2. **Whole-project pass.** `daemon::semantic::run_once` →
   `PluginProcess::semantic_pass` (daemon/plugin.rs:1162) →
   `apply_semantic_pass`. It runs after the bulk walk
   (`daemon::bulk_index::run_with_progress` calls `IndexStore::link_all`) or
   after a workspace reindex (`daemon::workspace_reindex::rebuild` calls
   `link_all`).
3. **`plugin-check`.** `cli::plugin_check::session::Driver::step` (l.1382):
   `ingest_and_link` runs `link_all` before
   `Operation::WholeProjectSemanticPass`.

The bridge decides only inside the pass (`run_pass` → `record_answer`), so it
always sees the links that the plugin's own extraction of the same text
produced.

Edge ids are deterministic (`ids::edge_id(from, kind, to)`), so the plugin's
`X.id` is the id core stores. A file that changed on disk between core's
commit and the plugin's hydrate has a different `X.id`. That id finds no
entry, and the bridge falls back to today's behaviour: never worse.

### 2.3 Glob re-exports

No special case is needed. Through `pub use g_mesh_wire::*`, `X.linkedFrom =
P` and `X.toId = D`, exactly as for a named `pub use pipeline::{.., T}`. The
map is keyed by edge, not by how the linker reached `D`. A test pins the
glob shape in core (§5, T5).

### 2.4 After a failed follow-up semantic pass (AC2)

| Point (typed re-export read/call `F → D`) | Rows today | Rows after |
|---|---|---|
| Steady state after a complete pass | `E` only (`X` retracted) | `X` only |
| Edit; reparse committed, pass not yet answered | `X` + `E` (**two**) | `X` (upserted in place; no `E` exists) |
| Follow-up pass completes | `E` (X retracted again) | `X` (R2 agrees; R1 re-sends it) |
| Follow-up pass **fails** or is incomplete | `X` + `E` **until a later pass** | `X` (core keeps the reparse commit; nothing to retract) |
| Follow-up pass answers empty | `X` + `E` | `X` (R1) |
| Genuine contradiction (`D' != D`) | `E'` + wrong `X` until the pass | unchanged (gm-489 §3.2 item 2) |

AC2 holds because `E` is never created for an agreeing answer. The failed
pass then has nothing to clean up.

**Existing stores (upgrade).** A store written before this fix holds `E` with
`X` retracted. The bridge change ships in the Rust plugin binary (it links the
SDK). A changed plugin fingerprint triggers the language's staging reindex,
which writes `X` again and clears `semanticPassAt`. The whole-project pass
that follows sends no `E`, and `sweep_semantic_edges` deletes the old one.
This is the same path as gm-489 §3.2 item 5; must-confirm M4.

### 2.5 Cross-language risk

g-mesh calls behind these answers:

- `find_callers(Resolver::through_head)` → only `Resolver::resolve`, which is
  called from `link` for every language. Placeholders of **no** language
  change, because the linker is not touched.
- `find_callers(address_of)` → `record_answer`, `refines`,
  `record_implementor` (bridge.rs). Only `record_answer`'s R2 changes.
- Who sends `replaces` (grep): Rust (`ReceiverCall`, `ReceiverField`),
  TypeScript (`Reference` hop sites and `OverloadCall`,
  `plugins/typescript/src/extractor/sites.rs`), Python (`OverloadCall` only,
  `plugins/python/src/extractor/bodies.rs:934`). Go has its own tier
  (`plugins/go/semantic.go`) and never sees the bridge.

Effect by language:

- **Rust:** the intended fix.
- **TypeScript:** a `Reference` hop site (`unsettled_hop`, bridge.rs:1060)
  that core *did* link through a barrel, and that tsserver answers with the
  same node, flips from retract+`E` to agreement. This is the same bug class
  fixed the same way. The rows read `source = syntactic` instead of
  `semantic`, as in gm-489 §3.2 item 4.
- **Python:** no change. Its only `replaces` sites are `OverloadCall`, which
  goes through `refines` (unchanged).
- **Go:** ignores the new field. `encoding/json` drops unknown fields, and no
  `DisallowUnknownFields` exists in `plugins/go` (grep).

The new test can only turn a contradiction into agreement when core's
committed link already equals the semantic answer. It cannot move an edge to
a new target. **Cost:** one indexed query per pass (`idx_edges_linkedFrom` is
a partial index) plus payload. On a g-mesh index, a whole-project Rust pass
has about 10.4k linked syntactic edges (measured on
`OLD/index-cache/939d7e4`: 10,450 cross-file linked syntactic edges, 752 KB
of raw ids, so about 1 MB of JSON). A per-file pass carries tens of entries.

### 2.6 Collisions with GM-530 and GM-533

This design does **not edit `core/src/graph/symbol_links.rs`**. It only
reads `edges.linkedFrom/toId` after linking, so there is no textual
collision.

There is one semantic interaction, and it is benign:

- **GM-530** (getter tie-break) turns some ambiguous, unlinked TS/Python
  edges into linked ones. They then appear in `linkedEdges`, and an agreeing
  answer becomes agreement.
- **GM-533** (Python later binding) can move a link. Wherever the linker and
  the LSP then differ, the result is a contradiction exactly as today.

Neither task needs to know about the map.

## 3. Edit map

| # | File : fn (line) | Change |
|---|---|---|
| E1 | `wire/src/lib.rs` `ControlMessage::SemanticPass` (563) | Add `#[serde(default, skip_serializing_if = "Vec::is_empty")] linked_edges: Vec<LinkedEdge>`; new `pub struct LinkedEdge { edge_id: String, to_id: String }` (camelCase). Serde tests for "absent = empty" and "empty is not serialized". |
| E2 | `core/src/watcher/apply.rs` `apply_semantic_pass_in` (226) | Before building the request (242), `store.step(|conn| linked_edges(conn, language, &file_paths))`. New fn `linked_edges` in the same file or `storage::file_rows`: `SELECT e.id, e.toId FROM edges e JOIN nodes f ON f.id = e.fromId WHERE e.linkedFrom IS NOT NULL AND e.source = 'syntactic' AND f.language = ?1 [AND f.filePath IN (…)]`. |
| E3 | `apply_semantic_pass` (197), `apply_semantic_pass_in`, `apply_file_change_in` (153) | New `language: &str` parameter, needed to scope a whole-project pass (empty `file_paths`). Callers to update: `daemon::plugin::PluginProcess::semantic_pass` (plugin.rs:1177), `cli::plugin_check::session::Driver::step` (session.rs:1382), `core/tests/protocol_conformance.rs:162`, and `apply_file_change_in`'s callers `PluginProcess::send_one` and `Driver::step`. |
| E4 | Pattern matches on `ControlMessage::SemanticPass { file_paths }` | Add `..` or bind the new field: `core/src/watcher/staleness.rs:525`, `core/src/watcher/apply/tests.rs:108, 528`, `core/src/cli/plugin_check/session.rs:1442`. |
| E5 | `plugins/sdk/src/run.rs` `"semanticPass"` arm (435-459) | Parse `params.linkedEdges` into the index (`self.index.set_linked(..)`) after hydrating, before `engine.answer`. Always replace, so a pass without the field clears the previous map. |
| E6 | `plugins/sdk/src/index.rs` `SdkIndex` (61) | Field `linked: HashMap<String, String>`, `pub fn set_linked`, `pub fn linked_target(&self, edge_id) -> Option<&str>`. |
| E7 | `plugins/sdk/src/lsp/bridge.rs` `record_answer` R2 (2486-2507) | `let linked_on_it = index.linked_target(replaced) == Some(node.id.as_str());` and `if lands_on_it \|\| linked_on_it \|\| &prospective == replaced`. Update the R2 comment and the `LspBridge` doc's retraction section (l.440-460), present tense and no ticket id (project comment rule): "R2: … or core linked the structural edge onto it". |
| E8 | `docs/architecture/gm-489-structural-semantic-duplicate.md` §3.2 item 1 | Mark it as closed by GM-531. |
| E9 | `docs/adr/` + `wire/src/lib.rs` `SemanticPass` doc | The decision "core ships its link result, and the bridge decides with it" is recorded in an ADR, or this note is indexed in `docs/adr/README.md`. The ADR is linked once, from the `linked_edges` field doc. |

`refines` (l.2025), `unsettled_hop` (l.1060), `through_head`, `link` and
`link_diff` stay unchanged.

## 4. Must-confirm (implement or verify slice)

- **M1.** The language is reachable in `PluginProcess::semantic_pass` and in
  `Driver::step` (the manifest's language). If it is not, pass it down from
  `daemon::semantic::run_once`.
- **M2.** `linkedFrom` survives a reparse that re-sends `X` (the reparse
  upserts `resolved: false`, and `link_diff` re-links it in the same commit).
  The per-file map then carries `X`. T6 pins this.
- **M3.** The TypeScript plugin's `semanticPass` goes through the same
  `run.rs` arm, so TS gets the map with no TS-specific code.
- **M4.** A rebuilt Rust plugin changes its fingerprint, so existing stores
  are cleaned by the staging reindex and the whole-project sweep (§2.4).
- **M5.** Payload: log the `linkedEdges` count once per whole-project pass.
  S5 records count, bytes and the pass's `real`/`user`. Owner decision only
  if this exceeds about 5 MB; the fallback is a manifest capability that
  gates the field to Rust and TypeScript.
- **M6.** The `source = syntactic` relabel of 275 rows: the
  `mcp/untyped.rs:96` and `mcp/provenance.rs` readers behave as they already
  do for the 1258 R2-ok fields (gm-489 §3.2 item 4).

## 5. Tests

| T | Where | Pins |
|---|---|---|
| T1 | `plugins/sdk/tests/lsp_bridge.rs` (scripted server) | A `ReceiverCall` with `replaces = X`, placeholder `P` at a re-exporting container. The index's linked map has `X → D` and the server answers `D`. Expected: diff has no semantic edge for `F → D`, `deleteEdgeIds` lacks `X`, and `X` is re-sent. |
| T2 | same | Same for a `ReceiverField` (`REFERENCES`). |
| T3 | same | Linked map `X → D'`, answer `D`. Expected: contradiction as today (`E` recorded, `X` retracted). |
| T4 | same | AC2: pass 1 agrees as in T1. Pass 2 over the same file fails (server error or incomplete). Expected: neither diff retracts `X` or records `E`. |
| T5 | `core/src/watcher/apply/tests.rs` (fake plugin) | The `semanticPass` request after a reparse carries `{X, D}` for an edge linked through a cross-crate glob `pub use other::*`. It carries none for a semantic edge, an unlinked edge, or an edge from another file. |
| T6 | same | Whole-project request (empty `filePaths`) carries only the requested language's edges. A second edit re-sends `X` and the map still has it (M2). |
| T7 | `plugins/sdk/src/run.rs` unit | `linkedEdges` reaches `SdkIndex::linked_target`. A following pass without the field clears it. |
| T8 | `wire/src/lib.rs` | Serde: field absent → empty, empty → omitted. |

## 6. Controls (6-8; one per behaviour)

Grouped by test binary: C1-C3 are SDK `lsp_bridge` (one build), C4-C6 are
core lib (one build), and C7 is SDK lib.

| C | Revert (code only) | Must fail |
|---|---|---|
| C1 | E7: drop `linked_on_it` from the R2 condition | T1, T2, T4 |
| C2 | E7: `linked_on_it = index.linked_target(replaced).is_some()` (ignores which node) | T3 |
| C3 | E5: do not call `set_linked` (map always empty) | T1 (end-to-end through `run.rs`, if T1 drives the session), otherwise T7 |
| C4 | E2: send an empty `linked_edges` | T5 |
| C5 | E2: drop `e.source = 'syntactic'` | T5 (semantic edge appears) |
| C6 | E2: drop the `f.language = ?1` filter | T6 |
| C7 | E5: keep the previous map when the field is absent | T7 |

## 7. Measure plan (S5)

**Input.** The base is `OLD/index-cache/dc7d160/projects/725bb46a04bc4a40/index.db`
(GM-497/S12 sem arm, rust-analyzer 1.97.1, complete pass). Its hash is of the
corpus path, so recreate the corpus at the same path:
`git archive dc7d160 | tar -x -C OLD/gm497s12/corpus`. The site lists are
`OLD/GM-494-S1-sites.tsv` (93 CALLS) and GM-497/S12's dumps (182 field
keys; `OLD/gm497s12/dumps`, `analyze.py`/`transitions.py`). The
`OLD/index-cache/939d7e4` cache cannot be used here: it has a different
corpus and is syntactic-only.

**Branch arm.** Run `gm497s12/run.sh`'s `sem` mode with the branch binaries:
a fresh `G_MESH_HOME`, Rust only, no model, rerank off, and the corpus
pre-warmed with `cargo check`. Record `uptime` and `time -p`. Take one
`struct` arm as well, for the structural id set.

**Diff and pass criteria:**

1. For the 93 CALLS sites, the structural edge id is present with
   `source = syntactic`, and no semantic edge has the same
   `(fromId, toId, kind)`.
2. The same holds for the 182 field keys.
3. The syn→sem retract transitions (`struct:syn → sem:sem`) drop by at least
   275 against base. Any that remain are listed: the 26 + 56 pre-existing
   ones.
4. The keyed `(fromFile, fromQN, toFile, toQN, kind)` set equals base: no
   target is gained or lost.
5. M5: the `linkedEdges` count and bytes for the whole-project pass.

**AC2 live check.** Start a daemon on the corpus and touch
`core/src/storage/qualified_path.rs` (site `suffixes`). Take
`find_callers(QualifiedPath::suffix_from)` after the reparse and after the
pass. Repeat with the pass forced to fail: `G_MESH`'s rust-analyzer path
pointed at `/usr/bin/false` after the first pass, or the plugin killed at the
hold point. Expected: exactly one row at every reading.

**Optional TS check (M3).** One count on a TS corpus with a barrel re-export.
If the owner declines it, T1-T4 are SDK-level and cover the bridge code path
TS shares.

## g-mesh calls relied on

- `find_callers(Resolver::through_head)` → `Resolver::resolve` only.
- `get_file_outline(core/src/graph/symbol_links.rs)` → line map of `link`,
  `resolve`, `walk`, `through_head`, `hops`, `head_name`,
  `placeholder_from_row`, `link_diff`.
- `find_callers(address_of)` → `record_answer`, `refines`,
  `record_implementor`.
- `find_callers(record_answer)` → `run_pass`.
- `find_callers(apply_semantic_pass)` → `PluginProcess::semantic_pass`,
  `Driver::step`, `protocol_conformance` test.
- `find_callers(apply_semantic_pass_in)` → `apply_file_change_in`,
  `apply_semantic_pass`.
- `find_callers(PluginProcess::semantic_pass)` → `daemon::semantic::run_once`,
  `PluginSupervisor::semantic_pass`, two integration tests.
- `find_callers(IndexStore::link_all)` → `bulk_index::run_with_progress`,
  `workspace_reindex::rebuild`, `plugin_check::session::ingest_and_link`.
- `find_callers(link_diff)` → `index_store::apply_and_link`.

All of these answers were complete. g-mesh served the main checkout
(`select_project` on the worktree failed: "bootstrapped daemon did not accept
connections … within 10s"). The branch has no code changes against
`release-4.2.0`, so the main checkout's index matches it. grep was used for
non-symbol searches: the `replaces:` emitters, `SemanticPass {` pattern
sites, and `DisallowUnknownFields`.
