# GM-596: a direct named import of a const-bound function never links

Status: design, pending owner review. Base: `release-4.10.0` (d8fcc50, which
contains GM-569 and GM-593).

## 1. Today

```ts
// src/x.ts
function make(): () => number { return () => 1; }
export const handler = make();          // a Variable node, nativeKind const

// src/main.ts
import { handler } from "./x";
export function run(): number { return handler(); }
```

This is fixture `docs/results/GM-519-fixtures/typescript/t01-import-var-vs-member`,
whose `expect.toml` checks only that `Router#handler` has no callers. It does
not check `handler` itself. `find_callers(handler)` returns `[]`.

The TS plugin emits a syntactic `CALLS run -> P`. `P` is the placeholder
`pending_symbol` with target `{scope: file src/x.ts, key: name "handler"}`
and label `src/x.ts#handler`. The call also gets two open sites
(`plugins/typescript/src/extractor/bodies.rs:457-458`): an `OverloadCall` and
a `Reference` with `replaces = CALLS run -> P`.

Two independent gates stop the engine answer. Both have to be fixed.

**Gate A: the bridge never asks.** `questions` (`plugins/sdk/src/lsp/bridge.rs:1050-1140`)
drops the `OverloadCall` site because `handler` is not an overload set. It
sends the `Reference` site to `unsettled_hop` (1156-1173). That function
answers "the linker settles it" whenever `src/x.ts` declares any addressable
node named `handler`. Here it does, a Variable. But the linker does not
settle it: a syntactic `CALLS` on a name key accepts only `Function`
(`required_target_kind`, `core/src/graph/symbol_links.rs:386`, with
`is_engine_answer` false at 372 because the key is `name` and the source is
`syntactic`). The edge stays `resolved = 0` for good.

**Gate B: an asked answer would be dropped as agreement.** Suppose the site
were asked. The server points at `export const handler`, so `lands_on_name`
passes (GM-569). `address_of` (1895) builds `{file src/x.ts, qualifiedName
"handler"}`. `render_target` (`plugins/sdk/src/graph.rs:686-696`) ignores the
key kind, so both keys render `src/x.ts#handler`. `placeholder_id` (706) then
gives the answer's placeholder the structural placeholder's id, and
`edge_id` gives the answer's edge the structural edge's id. R2 in
`record_answer` (2640-2663) sees `prospective == replaced` and treats that as
`Bound::There` agreement: upheld, nothing recorded. The linker never sees an
engine answer, so the Variable widening from ADR 0035 D2 never applies.

GM-569's +33 Variable edges on excalidraw all came from shapes where neither
gate fires. In a barrel, `src/index.ts#handler` is not `src/x.ts#handler`, and
`index.ts` declares no `handler`. Namespace sites carry no `replaces`.
Same-file calls have no placeholder.

The same id collision also exists today outside this shape. A semantic edge
can sit on a placeholder that the structural tier keyed by `name`. Counts in
the cached d8fcc50 indexes:

- excalidraw: 18 such CALLS: 17 overload-bound `pointFrom` and
  `useAppStateValue`, plus 1 `throttleRAF`.
- ripgrep: 8 such SUPERTYPE_OF onto `Matcher` and `Captures`.

They link today because the shared node's key is `name` and the target is a
Function or Type. That contradicts the SDK's own stated invariant
(`add_placeholder` doc, graph.rs:524-536): "two placeholders waiting on
different things never share an id".

## 2. What is in reach (measured, cached d8fcc50 indexes, sqlite)

Syntactic `CALLS` with `resolved = 0` onto a file-scoped, name-keyed
`pending_symbol`, grouped by what the scope file declares under that name:

| corpus | scope file declares | edges |
|---|---|---|
| excalidraw | nothing (barrel, `default`, missing) | 66, not this task |
| excalidraw | Variable/const | **11**, this task |
| ripgrep | any non-placeholder node, name key, any scope | 1 (`NiceDuration`, see below) |

The 11 edges have 8 distinct targets. I read each declaration in
`~/Projects/excalidraw`, and every one is callable: `throttleRAF(...)`,
`(function () {...})()`, `getMenuComponent(...)`, `React.useTransition || polyfill`
and `(() => {...})()`. All 11 are right.

| caller | target |
|---|---|
| `excalidraw-app/App.tsx:ExcalidrawWrapper` | `DebugCanvas.tsx#debugRenderer` |
| `element/src/selection.ts:getSelectionStateForElements` | `groups.ts#selectGroupsForSelectedElements` |
| `element/tests/zindex.test.tsx:populateElements` | `groups.ts#selectGroupsForSelectedElements` |
| `DropdownMenu.tsx:DropdownMenu` (x2) | `dropdownMenuUtils.ts#getMenu{Trigger,Content}Component` |
| `DropdownMenuSub.tsx:DropdownMenuSub` (x2) | `dropdownMenuUtils.ts#getSubMenu{Trigger,Content}Component` |
| `LibraryMenuSection.tsx:LibraryMenuSection` | `hooks/useTransition.ts#useTransition` |
| `NewElementCanvas`, `StaticCanvas`, `animation.ts:scheduleNextFrame` | `reactUtils.ts#isRenderThrottlingEnabled` |

The ripgrep `NiceDuration(...)` case is a Rust tuple struct reached through a
container-scoped name key. Its node's qualifiedName is not the bare key, so
there is no id collision. `unsettled_hop` never asks container scopes either.
It is a different gap, out of scope here.

Collision check across every edge kind: unresolved edges on name-keyed
placeholders where the declaration's qualifiedName equals the key.
excalidraw has 11 CALLS and 1 REFERENCES (onto a Type). ripgrep has 0. The
REFERENCES edge is never asked, because Gate A stays unchanged for
REFERENCES.

## 3. Options

| | Change | Benefit | Risk | Expected on excalidraw |
|---|---|---|---|---|
| **A** | Gate A only: `unsettled_hop` counts a `CALLS` hop as settled only if the file declares `n` as a `Function` | Small, bridge-only | Fixes nothing alone: the answer is then dropped by R2 (Gate B). 11 extra questions per pass | 0 |
| **B** | Gate B only: the key kind goes into the placeholder label, so into its id (option (1) of the brief) | Restores the SDK id invariant | Fixes nothing alone, because the site is still not asked. Id churn on every QN-keyed placeholder | 0 links. 18 semantic edges move to their own placeholder |
| **C (recommended)** | A + B, B applied to both scope kinds | Fixes the shape. Uses the existing engine-answer path, so no new linker rule (ADR 0035 D2 as decided) | Changes ids of every QN-keyed placeholder and its edges (opaque, self-consistent). One automatic full reindex per project. Agreement for Name-equals-QN Function cases now rests on `linked_on_it` instead of id equality | +11 linked CALLS onto Variable (8 targets, all right), -11 unresolved syntactic. 18 moved semantic edges keep their targets |
| C' | A + B, B only for `File` scope | Rust and Python placeholder ids unchanged, so ripgrep and requests are untouched | Leaves the invariant broken for container scopes. The 8 ripgrep SUPERTYPE_OF stay on shared nodes. Two rules for one label | same as C on excalidraw |
| D | Linker: a syntactic `CALLS` on a name key also accepts a uniquely declared `Variable` | No bridge change, so it works without an LSP | Contradicts GM-569 D4 ("Только ответы сервера"). Structurally a const holding an object or number looks the same as a callable one | +11 here. Unknown wrong links elsewhere |

Why B alone is not enough, and why a narrower Gate B fix is not viable:

- **R2 by key kind instead of by id.** Comparing the structural target's key
  kind in R2 instead of the ids would record an edge whose id is the
  structural edge's own id.
- What follows from that: core upserts it over the structural row, and the
  bridge's node upsert rewrites the shared placeholder's key to
  `qualifiedName`. The contradiction then retracts `replaced`, which is the
  very id just recorded.
- So the ids have to differ.
- **B only inside the bridge** (`answer_ids`). This would give a Rust
  structural QN placeholder and an agreeing engine QN answer for the same
  address different ids. The GM-489 `Bound::There` agreement would be lost,
  and duplicates would come back on ripgrep.
- Applying B in `render_target` keeps "same address, same id" and splits
  only Name from QN.

## 4. Recommendation

**C.** It is the smallest change that makes the shape link, and it does so
through the engine-answer path ADR 0035 already decided. The label change is
needed for correctness, not just for neatness: without it, nothing about this
shape can be recorded. Applying it to both scopes keeps one rendering rule
and fixes the latent sharing on ripgrep too.

Label spelling: a `name` key renders as today (`<file>#<key>`,
`<container>::<key>`). A `qualifiedName` key renders with a `=` sigil after
the separator (`src/x.ts#=handler`, `grep_printer::jsont::=jsont::Message`).

- No identifier in TS, Rust, Python or Go starts with `=`, and neither do the
  name keys `*` and `default`. So the two renderings never collide.
- Core never parses the label: no `rsplit('#')` in `core/src`. It reads only
  the `placeholder_targets` row.

### Blast radius of B

| Area | Effect |
|---|---|
| Wire / plugin protocol | None. Ids are opaque, and `target.key` already carries the kind on the wire. |
| Core (linker, storage, MCP) | No code change. Core never derives or checks a placeholder id (`protocol/conformance.rs` checks only that the target is present). |
| Stored indexes | No migration and no schema bump. The TS, Rust and Python plugin binaries link the SDK, so their fingerprints change. `indexer_version` = `CURRENT_INDEXER_VERSION + plugins_digest` (`daemon/registry.rs:285-321`) then differs, and `schema::ensure_current` resets the index, followed by one full reindex. That is the same as any plugin release. No manual `CURRENT_INDEXER_VERSION` bump. |
| Go plugin | Untouched: it has its own native semantic tier and does not link the SDK. gin should be identical. |
| Python plugin | Its private `render_target` copy (`plugins/python/src/extractor/emit.rs:437`) labels only `reexport` nodes, and nothing there collides with `pending_symbol` answers. Leave it alone. Python Definition sites never carry `replaces`, so R2 does not apply to them. Only overload-bound edges can move placeholder. |
| Rust plugin | `emit.rs:248` embeds `render_target` in re-export labels, so those ids change consistently. Structural QN placeholders (`bodies.rs:1329`) and the bridge's QN answers render the same as each other, so GM-489 agreement holds. |
| Id-pinned tests | `plugins/sdk/tests/id_scheme.rs` pins a name-keyed `src/b.ts#helper` and stays unchanged. `bridge.rs:3441` pins a QN label, `rg::flags::lowargs::flags::lowargs::SearchMode`, which becomes `rg::flags::lowargs::=flags::lowargs::SearchMode`. That is an expected update, not a weakening. |
| Bench / cross-version comparisons | Node and edge ids of every QN-keyed placeholder change: about 2060 semantic edges on excalidraw, and about 1986 semantic + 1934 syntactic edges on ripgrep. Compare by address, not by id. |

## 5. Edit map

All line numbers are 1-based, at d8fcc50.

1. `plugins/sdk/src/graph.rs`
   - `render_target` (675-696, doc and fn): render a `TargetKey::QualifiedName`
     key with the `=` sigil, and in the doc state the injectivity argument
     from section 4.
   - `add_placeholder` doc (524-536) and `placeholder_id` doc (698-708):
     say that the key kind is part of the address.
   - Test `placeholders_waiting_on_different_things_get_different_ids` (748):
     add Name vs QN with the same string, for both scope kinds.
2. `plugins/sdk/src/lsp/bridge.rs`
   - `unsettled_hop` (1156-1173): for an edge of kind `Calls`, it is settled
     only if the scope file declares an addressable `n` of kind `Function`.
     Every other edge kind keeps today's "any addressable `n`" rule.
     Update the doc.
   - `questions` hop arm comment (1082-1092): say that a CALLS hop onto a
     non-Function declaration is asked.
   - R2 comment in `record_answer` (2640-2648): `Bound::There` now means the
     same address *and key kind*. A name-keyed structural placeholder whose
     edge the linker left unlinked is contradicted, not upheld. No code
     change there.
   - Tests: the table in `a_hop_is_asked_only_while_the_linker_cannot_settle_its_placeholder`
     (4128), using `Declares` at 3992 / `declaring_file` at 3998, which needs
     a Variable variant. Add a test next to
     `a_call_answered_at_a_variable_or_type_name_is_recorded` (3771) for
     K3 and K5 below. Update the label at 3441.
3. `docs/results/GM-519-fixtures/typescript/t01-import-var-vs-member/expect.toml`:
   add `symbol = "handler"`, `file = "src/x.ts"`, `kind = "Variable"`,
   `expect = ["src/main.ts:run"]`.
4. Docs:
   - ADR 0035 Consequences: the direct-import shape now links. Name and QN
     keys never share a placeholder.
   - `docs/architecture/gm-474-qualified-name-segments.md:77,442` and
     `gm-324-typescript-rust-port.md:186`: the label rule.

There are no core, wire, Python or Go edits.

## 6. Behaviours the tests must pin, with controls

| # | Behaviour | Where |
|---|---|---|
| K1 | A Name key and a QN key with the same string, in the same scope, get different placeholder ids. Checked for file scope and for container scope. | SDK `graph.rs` unit |
| K2 | A `Reference` hop onto a file-scoped name-keyed placeholder is asked when the edge is `CALLS` and the file declares `n` only as a Variable (or Type). It is not asked when `n` is a Function, nor for a `REFERENCES` edge onto a Variable. | bridge table test |
| K3 | An answer to such a site that lands on the const's own name is recorded onto a QN placeholder whose id differs from the structural one, and the structural edge is contradicted and retracted. | bridge unit |
| K4 | A QN structural key and an agreeing QN answer for the same address still get one id, so nothing is recorded (GM-489 `Bound::There`). | existing GM-489 tests stay green |
| K5 | An answer agreeing with a name-keyed structural edge that core linked onto the same node (`linked_target`) records nothing. | bridge unit, new |
| K6 | End to end, t01: `handler` (Variable) has callers `[src/main.ts:run]`, and `Router#handler` still has `[]`. t02, t10 and t13 stay green. | fixtures |

Controls (6). Build them in a throwaway worktree. Each one must fail exactly
its named tests:

- C1, for K1 file and K3: the sigil is removed for `File` scope only.
- C2, for K1 container: the sigil is removed for `Container` scope only.
- C3, for K2 Variable row and t01: `unsettled_hop` goes back to "any
  addressable `n`".
- C4, for K2 REFERENCES row: `unsettled_hop` applies the Function rule to
  every edge kind.
- C5, for K5: `linked_on_it` is dropped from R2.
- C6, for K3 retraction: `contradicted`/`retract` are not inserted after
  `record`.

C1 and C3 also make t01 fail. Dry-run it on the fixture once, since fixture
runs need fresh plugin binaries and vtsls.

## 7. Measure plan

- **Arms.** Before: d8fcc50, whose index is cached at
  `scratchpad/index-cache/d8fcc50/index-{excalidraw,ripgrep}.db`. After: the
  branch head. Both are release builds; record the sha256 of the binaries,
  plus `uptime` and `/usr/bin/time -p`.
- **Corpora.** Index excalidraw and ripgrep for the after arm only. requests
  is optional: Python changes only for overload-bound edges. Skip gin,
  because the Go plugin is unchanged; one sanity run is enough if it is cheap.
- **Compare by address, not id.** Key each edge as (from file + qualifiedName,
  kind, source, final target file + qualifiedName, or the placeholder's
  scope + key when unresolved).
- **Expected:**
  - excalidraw: +11 resolved semantic CALLS onto Variable, for the 8 targets
    in section 2; -11 unresolved syntactic CALLS; every other address
    unchanged. The 18 semantic CALLS now on their own QN placeholders keep
    their targets.
  - ripgrep: every address unchanged; the 8 SUPERTYPE_OF keep their targets.
  - Lost edges: 0. Duplicates: 0. A duplicate is a (from, kind, final
    target) with both a syntactic and a semantic row, and it must not rise.
- **Cheap observation.** `find_callers(isRenderThrottlingEnabled)` returns 0
  rows before and 3 after.
- **Classify** every address that moved beyond these as right, wrong or
  owner-landing.

## 8. Decisions for the owner

- **D1.** C (recommended), C', or D.
- **D2.** Spelling of the `=` sigil in the QN label, or another injective
  marker.
- **D3.** Gate A only for `CALLS` (recommended), or also for `SUPERTYPE_OF`.
  REFERENCES needs no change, because the linker accepts any kind for it.
