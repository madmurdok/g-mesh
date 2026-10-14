# GM-569: which targets a semantic CALLS edge may accept

Design note for GM-569 (GM-519 owner decision D4 "Две задачи в бэклог
(Recommended)"). No production code changes here.

Claim markers: **[run]** verified by a command run for this note; **[code]**
read from the code at `release-4.8.0` (`03c5164`); **[GM-519]** taken from
`docs/results/GM-519-member-visibility-findings.md` / `GM-519-refusal-counts.md`
and not re-run. Line numbers are 1-based.

## 0. Summary

The kind rule refuses 82 engine answers on excalidraw and 51 on ripgrep. They
are two different things:

| group | edges | what the engine actually answered | linking it would be |
|---|---|---|---|
| TS direct call of a const-bound function (`isSomeElementSelected(...)`, `selectGroupsForSelectedElements(...)`) | 32 | the const itself, at its name | **right** |
| Rust tuple-struct constructor (`IgnoreMatch(...)`, `StandardStream(...)`, `Logger(())`) | 48 | the struct, at its name | **right** |
| TS call of a declaration g-mesh has no node for (a class field `onBlur = withBatchedUpdates(..)`, a property signature of an interface / type alias, an object-literal method `charWidth.getCache`, `isSomeElementSelected.clearCache()`) | 50 | that undeclared member; the bridge's `node_at` takes the **innermost enclosing node** (the class, interface, type alias or const) | **wrong** |
| Rust tuple enum variant (`ContextMode::Limited(..)`) | 3 | the variant; no variant nodes, so the enclosing enum | **wrong** |

So the kind rule today does two jobs. It is the linker's statement that a
call lands on something callable. And, by accident, it is the only thing that
stops the bridge's "innermost enclosing node" fallback from turning a call of
an undeclared member into a call of its owner. Relaxing the kind rule alone
would add 80 right links and 53 wrong ones. The design therefore has two
halves: the bridge sends a CALLS answer on a non-`Function` only when the
answer points at that node's own name, and core then accepts `Variable` and
`Type` targets for engine-answer CALLS, after `Function`.

Expected corpus effect (recommended options, before GM-567's changes):
excalidraw +32 semantic links, ripgrep +48, 0 links onto an owner.

## 1. What happens today

### The rule

- `LINKABLE_EDGE_KINDS` (`core/src/graph/symbol_links.rs` 330-340):
  `CALLS -> ["Function"]`, `SUPERTYPE_OF -> ["Type", "Function"]` (first
  non-empty kind wins), `REFERENCES -> any`. Read by `required_target_kind`
  (342-344). [code]
- Applied in `link` (1238-1332), in the per-edge-kind loop: the `fitting`
  list (1290-1299) keeps only candidates of the first accepted kind that has
  any. An empty list unlinks the edge (it stays on its placeholder); one
  candidate links; several go to `sole_non_member` (`name` keys) and
  `sole_accessor_getter` (`qualifiedName` keys). [code]
- `link_diff` (718) also calls `required_target_kind`, only as "is this a
  linkable edge kind" (737); it does not care which kinds are accepted.
  `seeds` publishes every declaration whatever its kind (784-791), so a
  `Variable`/`Type` arriving after its placeholder already wakes it. [code]
- Module doc, contract step 3 (57-58), and "What stays unresolved" (197)
  state the rule. `docs/architecture/multi-language-plugins.md` 318 and 687
  repeat it. [code]

### Why it exists (what wrong links it prevents)

1. **Disambiguation of `name` keys.** One name can denote a type and a
   function (`Widget` class + `Widget` function, TS declaration merging); the
   rule sends `CALLS` to the function and `SUPERTYPE_OF` to the type. Pinned by
   `each_edge_kind_picks_the_export_that_fits_it` (tests.rs 415). [code]
2. **"A type is not a thing you can call."** A structural `CALLS` onto an
   exported `Type` of that name waits rather than landing:
   `a_call_does_not_link_to_a_non_function_export` (tests.rs 371). Go relies on
   it for conversions: `pkg.T(x)` is a structural `CALLS` the kind rule never
   lands on a `Type`, and Go's semantic pass re-states it as `REFERENCES`
   (`multi-language-plugins.md` 1932-1938). [code]
3. **Accessor pairs** (GM-530): a Python `property` `Variable` beside its
   setter: the kind rule leaves the setter alone for `CALLS`
   (`gm-530-accessor-tie-break.md` 73). [code]
4. **Unstated, found here: the bridge's enclosing-node fallback.** The SDK
   bridge turns a `definition` location into a node with `node_at`
   (`plugins/sdk/src/lsp/bridge.rs` 1910-1920) -> `SdkIndex::node_at`
   (`plugins/sdk/src/index.rs` 176-184), which returns the **smallest node
   whose range contains the location**. When the engine answers a declaration
   g-mesh has no node for, the answer becomes its enclosing node. [code] For a
   `CALLS` site that enclosing node is a class, interface, type alias, const
   or enum, and the kind rule refuses it. That is the 53 "wrong" rows of the
   summary. [run, section 2]

Invariant today: every resolved `CALLS` edge on both corpora targets a
`Function` (excalidraw: arrow_function, method, function, constructor,
function_expression, generator_function; ripgrep: method, function,
trait_method, trait_impl_method, macro). [run]

### The same fallback already produces wrong links onto Functions

The kind rule hides the fallback only when the encloser is not a function.
When it is, the wrong edge links today: `try_resolve_binary` calls its nested
`fn is_exe` (no node) and gets a `CALLS` edge onto itself; `ElementCanvasButton`
calls `props.onChange()` (a property of an inline parameter type) and gets one
onto itself. Semantic self-loop `CALLS`: 4 on excalidraw, 15 on ripgrep, some
of them genuine recursion (`Error::is_partial`). [run] Out of scope here
unless D1 option B is taken; see D1.

## 2. Evidence: the 133 kind-rule refusals, classified

Method [run]: the CTEs of `docs/results/GM-519-count-refusals.sql` over the
GM-519 S2 indexes (`~/.g-mesh/projects/39fba07a57afbba6/index.db` =
excalidraw `1acf66ed`, `~/.g-mesh/projects/2e37c14f1bb87919/index.db` =
ripgrep `e89fff89`; read-only, no reindex), listing each `refused_other`
edge's visible candidate (kind, nativeKind, name) and its `fromId`, then
reading the source at the caller in the warm corpus checkouts
(`$TMPDIR/gmesh-bench-corpora/{excalidraw,ripgrep}`). Query files kept in
the session scratchpad, not committed.

### excalidraw (82, all semantic CALLS, one visible candidate each)

| target (nativeKind) | edges | call site, read in source | engine landed on | verdict |
|---|---|---|---|---|
| `isSomeElementSelected` (const) | 19 | `isSomeElementSelected(elements, appState)`; the const is `(function () {...})()` returning a function | the const's name | right, 18; 1 is `isSomeElementSelected.clearCache()` (App.tsx 3806), wrong |
| `selectGroupsForSelectedElements` (const) | 15 | direct calls; same IIFE shape | the const's name | right, 14; 1 is `.clearCache()` (App.tsx 3807), wrong |
| `charWidth` (const) | 3 | `charWidth.getCache(font)`, `charWidth.calculate(..)` | a property of the IIFE's returned object | wrong |
| `actionSaveToActiveFile` (const) | 1 | `actionSaveToActiveFile.keyTest(event)` | `keyTest` inside `register({...})` | wrong |
| `Module` (const, emscripten bindings) | 2 | generated code calling members of `Module` | a member | wrong |
| `App`, `Collab`, `ActionManager`, `FileManager`, `Portal`, `Renderer`, `LaserPointer` (class) | 29 | `this.onBlur()` (`onBlur = withBatchedUpdates(..)`), `this.scheduleImageRefresh()` (`= throttle(..)`), `this.queueSaveToFirebase()`, `this._onFileStatusChange(..)` (declared field), `this.getAppState()` (`getAppState: () => AppState` field), `h.app.updateScene(..)`, `this.app.syncActionResult(..)` | a class field that is not a node (only arrow-function fields are) | wrong; every one of the 29 classes also has an explicit `#constructor` node, and no row is a `new X()` |
| `ResolvablePromise`, `ShareDialogProps`, `LaserPointerOptions` (type_alias) | 7 | `job.promise.resolve(..)` etc. | a property signature in the alias's type literal | wrong |
| `ExcalidrawProps`, `ExcalidrawImperativeAPI`, `Action` (interface) | 6 | `props.onX(..)`, `api.updateScene(..)` | a property signature | wrong |

Right: 32. Wrong: 50.

### ripgrep (51, all semantic CALLS, one visible candidate each)

| target | edges | call site | engine landed on | verdict |
|---|---|---|---|---|
| tuple structs (`dir::IgnoreMatch` 4, `ByteSet` 3, `BitSet` 2, `wtr::StandardStream` 2, `logger::Logger` 1, `searcher::mmap::MmapChoice` 3, ...; 25 structs) | 48 | `IgnoreMatch(IgnoreMatchInner::Types(x))`, `StandardStream(StandardStreamKind::..)`, `&Logger(())` | the struct's name (the tuple struct *is* its constructor) | right |
| `flags::lowargs::ContextMode` (enum) | 3 | `ContextMode::Limited(ContextModeLimited {..})` | the variant `Limited`; variants are not nodes | wrong |

Right: 48. Wrong: 3.

## 3. The design

### 3.1 Bridge: a non-Function CALLS answer must point at the node's own name

In `record_answer`'s `Ask::Definition` branch (bridge.rs 2442-2609), after
`node_at` lands a location on a node (the agreement loop, 2480-2496): when
`site.edge_kind` is `Calls` and the node's kind is not `Function`, keep the
landing only if the location is **at the node's name**. Otherwise treat it as
"nothing this index holds" (the existing `let Some(..) = landed else` arm:
the structural edge it would replace is upheld, R1, and nothing is
recorded).

"At the node's name": the location (already in wire columns via
`wire_position`, 1898-1907) is the start of the first whole-word occurrence of
`node.name` in the file's source at or after `node.range.start`. Every
precise answer passes: vtsls and rust-analyzer point a definition at the
declaration's name (`LocationLink.targetSelectionRange`, which `locations`
already prefers, 1856-1880), and node ranges start at the declaration, after
attributes and doc comments (ripgrep `IgnoreMatch`: range starts at
`pub(crate) struct`, the `#[allow]` line above is outside). [run on the
index] Every enclosing-node answer fails: the word at the location is the
member's name (`onBlur`, `getCache`, `Limited`), and even a member that
shares its owner's name (`const handler = { handler() {} }`) fails, because
the owner's own name occurs first. A false "not at name" only drops an edge,
never adds a wrong one.

Where it lives: a new `fn lands_on_name(index, path, at: Position, node) ->
bool` beside `node_at`. Applied only to `CALLS` onto a non-`Function` under
D1 option A; to every landing under option B.

The overload path (`overload_landing`, a `ReceiverCall` onto an overload set)
only ever lands on `Function`s and is untouched. `untyped_call_answered`
(2627) keeps counting the site as answered (the server did answer; GM-497's
structural-list trimming must not bring the name back).

Go is not affected: its semantic pass (`plugins/go/semantic.go`) answers
from go/types objects, has no enclosing-node fallback, and already re-states
conversions as `REFERENCES`.

### 3.2 Core: engine-answer CALLS accept Function, then Variable, then Type

`LINKABLE_EDGE_KINDS` keeps `CALLS -> ["Function"]` for structural edges.
For an engine answer, `CALLS` accepts `["Function", "Variable", "Type"]` in
that order of preference: the first kind any candidate has decides, the same
mechanism `SUPERTYPE_OF` already uses. Preference matters only when one
`qualifiedName` names two declarations: TS `export const Foo = ..` +
`export type Foo = ..` links the const; a `function Foo` + `interface Foo`
merge links the function.

"Engine answer" is GM-567's predicate, unchanged: edge `source = 'semantic'`,
key `qualifiedName`, edge kind in `ENGINE_ANSWER_EDGE_KINDS` (`CALLS`,
`REFERENCES`). GM-569 reads it as `engine_answer && edge_kind == "CALLS"`.

## 4. Composition with GM-568, GM-567 and GM-570

Code order: GM-568, GM-567, GM-569, GM-570, all in `link`'s per-kind
decision.

- **GM-568** (for `name` keys, drop type members before the kind rule).
  Disjoint predicates: GM-568 acts on `name` keys only, GM-569 on
  `qualifiedName` keys only, so for any placeholder at most one applies.
  Textual overlap only: both sit in the `fitting` step.
- **GM-567** (semantic exact answers bypass visibility; D1 «Только точные»,
  D2 «Только CALLS/REFERENCES», D3 «Свободен только член»). GM-567 splits
  `link`'s decision per `(edge kind, edge source)`, filters
  `repoint`/`unlink` by source, puts the requester's `engine_answer` flag on
  the group, and extracts the post-candidate decision into
  `choose(key, edge_kind, candidates)`. GM-569 builds on all three:
  - `choose` gets the group's `engine_answer` flag and calls
    `required_target_kind(edge_kind, engine_answer)`;
  - **the per-source split is a prerequisite.** One placeholder can carry a
    structural and a semantic `CALLS` edge (26 TS, 28 Rust such placeholders
    [GM-519]). Without the split, a per-placeholder decision that accepts a
    `Variable` for the semantic edge would repoint the structural edge onto it
    too. If GM-567 does not land first, GM-569 must carry that split itself
    (GM-567 section 2, steps 1, 5, 6).
  - **GM-567's expected corpus delta is too high, and GM-569's bridge rule
    changes it.** Of excalidraw's 62 `file`-rule refusals, 38 are `CALLS`
    onto a non-`Function` [run]: interfaces 18 (`Window`,
    `CanvasRenderingContext2D`, `MatchListProps`), type aliases 11 (`Props`,
    `IconButtonProps`, ...), consts 9 (`Regex`, `Break`: object literals whose
    methods are called, `Regex.or(..)`, `Break.On(..)`). All 38 are
    enclosing-node answers. After GM-567 alone they pass visibility and are
    refused by the kind rule instead: GM-567 measures about +24 links, not
    +62, and 38 edges move from `refused_visibility_file` to
    `refused_other`. After GM-569, the bridge stops sending them. The GM-567
    measure slice should be told this before it runs.
- **GM-570** (Go build-tag twins: two declarations at one address). It acts
  on the "exactly one" step after the kind filter; the twins are `Function`s
  and `Type`s fitting `CALLS` / `SUPERTYPE_OF` today, and GM-569 changes no Go
  answer. Order inside `choose` after all four: kind filter (GM-569's list for
  engine answers) -> member drop (GM-568, `name` keys) -> exactly one, with
  GM-570's twin rule and the existing tie-breaks.

## 5. Owner decisions

### D1. How does g-mesh tell a precise engine answer from a "landed in the owner" answer?

- **Today:** when the engine answers a declaration g-mesh has no node for (a
  class field `onBlur = throttle(..)`, an interface property, an object-literal
  method, a Rust enum variant), the bridge takes the smallest node around the
  answer: the class, interface, const or enum. For calls, the kind rule then
  refuses it because the owner is not a function. That refusal is the only
  thing standing between those answers and a wrong link.
- **Change:** the bridge checks that the answer points at the landed node's
  own name before sending it.
- **Example:** in `class App`, `this.onBlur()` where `onBlur =
  withBatchedUpdates(..)`. vtsls answers the field `onBlur`; the bridge lands
  on `App`. With the check, the word at the answer is `onBlur`, not `App`, so
  nothing is sent. Without it, and with D2/D3 accepted, `find_callers App`
  would list 15 of App's own methods.
- **Options:**
  - **A: check only CALLS answers that land on a non-Function (Recommended).**
    Benefit: blocks all 53 wrong links on the two corpora (and the 38 behind
    GM-567); no existing link changes. Risk: the same fallback keeps producing
    the wrong self-calls it produces today onto Functions (nested `fn is_exe`
    -> its enclosing function; up to 4 + 15 semantic self-loops on the
    corpora); a follow-up task for that.
  - **B: check every definition answer, all node and edge kinds.** Benefit:
    also removes today's wrong self-calls. Risk: changes existing links on
    every bridge language, so the measure slice must classify every lost
    link; a node whose `name` is not the identifier at its declaration (a
    TS `export default` function named `default`) would lose its edges.
  - **C: no check.** Benefit: plugin untouched. Risk: about 53 wrong `CALLS`
    edges on these two corpora (50 TS, 3 Rust) plus 38 after GM-567, each one
    a call of a member reported as a call of its owner.

### D2. May an engine-answered call land on a `Variable` (a const-bound function)?

- **Today:** `export const isSomeElementSelected = (function () {...})()`
  returns a function. A call `isSomeElementSelected(elements, appState)` is
  answered by vtsls with the const; the linker refuses it because a const is
  a `Variable`. `find_callers isSomeElementSelected` shows nothing: 32 calls
  on excalidraw. Fixture t02 (`export const handler = make()`, called through
  a barrel) is the same shape.
- **Change:** engine-answered `CALLS` accept a `Variable` when no `Function`
  has that qualified name.
- **Example:** t02 `run -> handler` links; `find_callers handler` lists
  `run`. A TS `export const Foo` beside `export type Foo` links the const.
- **Options:**
  - **A: yes, as a `CALLS` edge onto the `Variable`, after `Function`
    (Recommended).** Benefit: +32 excalidraw edges, t02 links; callers of a
    memoized or wrapped function become visible. Risk: `find_callees` of a
    function can now list a const; a consumer assuming every `CALLS` target is
    a `Function` would see a `Variable` (none found in core: `find_callers`
    and `find_callees` filter by edge kind only, `find_callers_callees.rs` 68).
  - **B: re-state it as `REFERENCES` in the bridge** (Go's precedent for
    conversions). Benefit: `CALLS` stays Function -> Function. Risk: a real
    function call disappears from `find_callers`; only `find_references`
    sees it.
  - **C: keep refusing.** Benefit: no change. Risk: the 32 calls stay
    invisible.

### D3. May an engine-answered call land on a `Type` (a constructor call)?

- **Today:** Rust `IgnoreMatch(IgnoreMatchInner::Types(x))` builds a tuple
  struct; rust-analyzer answers with the struct `IgnoreMatch`, refused because
  it is a `Type`. 48 such calls on ripgrep. A TS `new X()` already links to
  `X#constructor` when the class declares one (23 structural + 7 semantic
  edges on excalidraw); a class with no explicit constructor would be
  answered with the class itself.
- **Change:** engine-answered `CALLS` accept a `Type` when no `Function` or
  `Variable` has that qualified name.
- **Example:** `find_callers IgnoreMatch` lists `IgnoreMatch::types`,
  `::hidden`, `::overrides`, `::gitignore`. With D1-A in place, an interface
  or type alias can never be such a target (no call points at a type's name).
- **Options:**
  - **A: yes, `CALLS` onto the `Type`, after `Function` and `Variable`
    (Recommended).** Benefit: +48 ripgrep edges; a construction counts as a
    call, the same as TS `new X()` onto an explicit constructor. Risk: Python
    `Foo()` and TS `new Foo()` without a constructor start producing
    `CALLS -> class` too (none measured on py-requests: 0 refusals);
    `find_callers` on a struct lists constructions, which some users read as
    "references".
  - **B: re-state as `REFERENCES`** (Go conversion precedent). Benefit:
    `CALLS` targets stay functions. Risk: a tuple-struct constructor, which
    Rust defines as a function, is not a caller edge; inconsistent with TS
    explicit constructors.
  - **C: keep refusing.** Benefit: no change. Risk: 48 edges stay missing.

### D4. Do structural (tree-sitter) CALLS get the wider kinds too?

- **Today:** a structural `CALLS` is a `name`-keyed guess from an import
  binding; the kind rule is what sends it to the function when a type shares
  the name, and what keeps Go's `pkg.T(x)` conversions off the type until the
  semantic pass re-states them.
- **Change:** none (Recommended), or the same list for structural edges.
- **Example:** Go `binding.JSON(x)` without the semantic tier: a structural
  `CALLS` onto the `Type` `JSON` would link as a call; today it waits.
- **Options:**
  - **A: engine answers only (`semantic` + `qualifiedName`, GM-567's
    predicate) (Recommended).** Benefit: the structural rules and their tests
    stay as they are; only answers the engine pinned to one declaration move.
    Risk: with no semantic tier, `handler()` of an imported const stays
    unlinked (as today).
  - **B: structural too.** Benefit: const calls link without a semantic tier.
    Risk: Go conversions and TS `name` collisions land as calls; GM-568's
    member drop and `a_call_does_not_link_to_a_non_function_export` would
    need rework.

## 6. Edit map (after rebasing on GM-568 and GM-567)

`core/src/graph/symbol_links.rs`:

| what | where (at `release-4.8.0`) | change |
|---|---|---|
| module doc, contract step 3 | 57-58 | `CALLS` lands on a `Function`; an engine answer also on a `Variable`, then a `Type` |
| module doc, "What stays unresolved" | 197 | "wrong kind" now means wrong for that edge kind *and source* |
| `LINKABLE_EDGE_KINDS` + doc | 330-340 | unchanged; add `ENGINE_ANSWER_CALL_TARGET_KINDS: [&str; 3] = ["Function", "Variable", "Type"]` beside it (doc states the invariant only; the reasoning goes to the ADR) |
| `required_target_kind` | 342-344 | take `engine_answer: bool`; `("CALLS", true)` returns the new list |
| `link_diff` | 737 | keep as "is linkable kind" (any flag value; or split a `is_linkable_edge_kind`) |
| GM-567's `choose` (today `link`'s `fitting`, 1290-1299) | 1238-1332 | pass the group's `engine_answer` to `required_target_kind` |

`plugins/sdk/src/lsp/bridge.rs`:

| what | where | change |
|---|---|---|
| `record_answer`, `Ask::Definition` agreement loop | 2480-2496 | keep the chosen location with the target; drop the landing when `site.edge_kind == Calls`, node kind is not `Function`, and `!lands_on_name` (D1-A) |
| new `lands_on_name` | beside `node_at` (1910) | first whole-word occurrence of `node.name` from `node.range.start` equals the location |
| `node_at` doc | 1909 | mention that the landing may be an encloser, checked by `lands_on_name` |

Docs: `docs/architecture/multi-language-plugins.md` 318 and 687 (kind
filter), the GM-530 note's line 73 stays true (Function first). New ADR
`docs/adr/0035-...` (re-check the next free number after GM-567's 0034):
decisions D1-D4 as taken.

Plugins to rebuild for the measure: typescript, rust, python (they embed the
SDK); go unchanged.

**Read for context, do not change:** `SdkIndex::node_at`
(`plugins/sdk/src/index.rs` 176-184), `is_addressable` (bridge.rs 1832),
`locations` (1856), `wire_position` (1898), `untyped_call_answered` (2627),
`overload_landing` (1967), `sole_non_member` (1742), `sole_accessor_getter`
(2033), `seeds` (780-814), tests helpers `usage_edge_from` (tests.rs 239),
`seed_usage` (254), bridge test
`a_placeholder_or_a_file_node_is_never_an_answers_target` (bridge.rs 3132,
the model for a `record_answer` test with an index and a definition result).

**g-mesh calls relied on** (project `g-mesh`, the main checkout: the
`g-mesh-wt-gm569` worktree's index was cold, and `core`/`plugins` differ from
`release-4.8.0` only in version strings, checked with `git diff --stat`):
`find_callers required_target_kind` -> `link`, `link_diff` (complete);
`find_definition address_of`, `find_callers lsp::bridge::address_of` ->
`refines`, `record_answer`, `record_implementor` (complete);
`find_definition record_answer` (source, 88 lines omitted, read by `sed`);
`find_definition node_at`, `find_callers lsp::bridge::node_at` ->
`overload_landing`, `record_answer`, `record_implementor` (complete);
`find_definition WireNode` (no name range on the wire node). grep was used
for line numbers in known files and for non-code.

## 7. Behaviours the tests must pin, with candidate controls

Core tests use `usage_edge_from(.., "ts-compiler")` for a semantic edge and a
`qualifiedName` placeholder (`container_placeholder` / GM-567's helpers).

| # | behaviour | control (revert in code; the test must fail) |
|---|---|---|
| K1 | a semantic exact `CALLS` onto a `Variable` links (t02 shape) | `required_target_kind` ignores `engine_answer` |
| K2 | a semantic exact `CALLS` onto a `Type` links (tuple-struct shape) | drop `"Type"` from `ENGINE_ANSWER_CALL_TARGET_KINDS` |
| K3 | with a `Function` and a `Variable` (and a `Type`) under one qualifiedName, the semantic `CALLS` links the `Function`; with `Variable` + `Type`, the `Variable` | pool the accepted kinds instead of first-non-empty, or reorder the list |
| K4 | a structural `CALLS` onto a `Variable`/`Type` stays unlinked (existing `a_call_does_not_link_to_a_non_function_export` plus a `Variable` case) | `link` passes `engine_answer = true` for every source |
| K5 | one placeholder with a structural and a semantic `CALLS`, the only candidate a `Variable`: the semantic edge links, the structural stays on the placeholder | drop GM-567's `AND source = ?` from `repoint` (shared with GM-567's B3; if the GM-567 control already covers it, skip) |
| K6 | a semantic `name`-keyed `CALLS` onto a `Variable` stays unlinked | `engine_answer` without the `qualifiedName` condition |
| K7 | bridge: a definition landing inside a class at a field's name (no node) is not recorded for a `CALLS` site; the same at the class's own name is recorded | `lands_on_name` returns `true` |
| K8 | bridge: a node whose body repeats its own name (`const handler = { handler() {} }`, answer at the inner `handler`) is not recorded | `lands_on_name` compares only the word at the location, not the first occurrence |

Eight controls; K1-K6 are in the `g-mesh` (core) lib test binary (one build group),
K7-K8 in the `g-mesh-plugin-sdk` lib (second group). Conformance fixtures
(semantic tier on, `docs/results/GM-519-fixtures/run-all.sh`, verify slice):
extend t02's `expect.toml` with `callers of handler = [run]`; add a TS
class-field fixture (`this.cb()` with `cb = wrap(..)`: `callers of the class
= []`), a Rust tuple-struct fixture (`callers of the struct = [the fn]`) and a
Rust tuple-variant fixture (`callers of the enum = []`).

## 8. Measure slice: needed

The change moves corpus edges in both directions (new links, and answers
the bridge no longer sends), so a measure is needed.

- **Arms:** before = the branch base with GM-568 and GM-567 merged (so only
  GM-569 differs); after = this branch. Plugins typescript, rust, python
  rebuilt per arm (the SDK is compiled in). Same corpus checkouts and commits
  as `GM-519-refusal-counts.md` (excalidraw `1acf66ed`, ripgrep `e89fff89`,
  plus py-requests `6e83187b` as a no-change check). `g-mesh reindex` per arm
  and corpus, semantic pass `completed: true`; record `uptime` and
  `/usr/bin/time -p` real/user/sys per reindex.
- **Counts per arm:** `GM-519-count-refusals.sql`; the section 2 query
  (`refused_other` by candidate kind/nativeKind); resolved `CALLS` by
  `source` and target `kind/nativeKind`; total semantic `CALLS` edges;
  syntactic linked edges.
- **Expected:** excalidraw semantic `CALLS` onto `Variable/const` +32 (all
  keyed `isSomeElementSelected` / `selectGroupsForSelectedElements`), onto any
  `Type` 0; ripgrep onto `Type/struct` +48, onto `Type/enum` 0; `refused_other`
  kind rows: excalidraw 82 -> 0 and ripgrep 51 -> 0 (the 53 wrong ones are no
  longer sent, the 80 right ones link); semantic `CALLS` edge totals drop by
  about 50 + 38 (excalidraw, the second figure from GM-567's refusals) and 3
  (ripgrep); syntactic linked edges change only where a dropped answer
  upholds a structural edge (expected near 0; every changed row listed).
  py-requests: no change.
- **Cheap observation where the arms must differ:** ripgrep `find_callers
  dir::IgnoreMatch`: empty before, the four `IgnoreMatch::{types, hidden,
  overrides, gitignore}` after. The wrong-link guard: excalidraw
  `find_callers App` is empty in both arms. If `IgnoreMatch` has no callers
  after, the plugins were not rebuilt (the bridge half missing still leaves the
  core half visible here; the guard half is visible only in `find_callers App`
  under a build with core changed and the bridge not, so check the plugin
  binaries' build times too).
