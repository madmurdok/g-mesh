# GM-502: `find_callers` names the base member an override's callers may sit on

Status: design (GM-502/S1). Delivers owed item (b) of
[ADR 0022](../adr/0022-instructions-coverage-states.md) (statement 9).

## 1. Problem

A receiver call `x.m()` binds to the declared or inferred type of `x`, never
to the type it holds at run time. So when `C.m` overrides `Base.m`, a call
made through a `Base`-typed receiver is an edge onto `Base.m`, and
`find_callers(C.m)` does not list it. Today every session is told this in the
instructions (`P4_STATIC`, 343 bytes, plus the 129-byte `S_PASS` chained to it
by "such a call"). Under ADR 0022's channel rule this is an
"once an answer exists" fact: it matters only on a page whose anchor
overrides or implements something. It belongs on that page.

Example of the gap (Python):

```python
class Base:
    def describe(self): ...
class Sub(Base):
    def describe(self): ...          # anchor
def show(item: Base):
    item.describe()                  # edge -> Base.describe, not Sub.describe
```

`find_callers(Sub.describe)` today: `results: []`, `hasMore: false`. Nothing on
the page says `show` may reach it.

## 2. What exists (recon, verified)

| Question | Answer | Produced by |
|---|---|---|
| Callers of `handle_callers` | 30 callers: tests in `find_callers_callees.rs` (21) and test helpers in `semantic_pending_tests`, `answer_tests`, `untyped_tests`, `unlinked_tests`, `member_name_collision_tests`, `response_bound_tests`, `find_references` tests, `cli/plugin_check/expectations.rs` (`SymbolTool::call`). No production caller besides the MCP router | g-mesh `find_callers(symbol_name="handle_callers")`, `hasMore: false` |
| Callers of `handle_callers_in` | only `handle_callers` | g-mesh `find_callers(symbol_name="handle_callers_in")` |
| References to `P4_STATIC` | only `instructions::receiver_paragraph` (production). Tests in `instructions/tests.rs` (lines 681, 1048, 1067, 1105, 1447) are not indexed as references (`use super::*`); found by grep. Docs: `manifest.rs:157` and `docs/architecture/gm-325-typescript-lsp-semantics.md:300,619` | g-mesh `find_references(symbol_name="P4_STATIC")`; grep for tests and docs |
| References to `UntypedReceiverCalls` (the precedent) | `untyped::probe`; `find_callers_callees::{CallerPage, CallerParts, CallerDisclosures}` + a file-level use; `find_references::{ReferencePage, ReferenceParts, ReferenceDisclosures}` + a file-level use | g-mesh `find_references(symbol_name="UntypedReceiverCalls")`, `hasMore: false` |
| Who reads `SUPERTYPE_OF` | g-mesh has no symbol for it (a string literal; it returned semantic neighbours only). grep, production readers in core: `mcp::find_implementations` (incoming walk, single-hop or `transitive`), `mcp::find_references` (`USAGE_EDGE_KINDS`), `graph::symbol_links` (kind filter at line 336: `SUPERTYPE_OF` lands only on a `Type`). Writers: Rust and Python plugins structurally, TypeScript structurally (`decls.rs`/`bodies.rs`), Go **only in its semantic pass** (`semantic.go::answerImplements`), the SDK LSP bridge for `implementation_kinds` | g-mesh `find_references(symbol_name="SUPERTYPE_OF")` (no match); grep |
| `is_type_member` / `is_method` | `unlinked::is_type_member` already derives a member's owner as "the qualifiedPath minus its last segment, joined" and checks it names a `Type`. `untyped::METHOD_NATIVE_KINDS` lists Rust's kinds because a trait-impl method's parent is `<T as Tr>`, which names no node | read by symbol |

Facts the design rests on:

- There is no OVERRIDES edge and no member edge: `DEFINES` runs only
  `File -> declaration` (`cli/plugin_check/checks.rs:470` enforces it).
- `core/src/graph/containers.rs` is about *logical* containers (Go package,
  Rust module, Python module): `nodes.container`. It does not model a type's
  members. A member's owner is readable only from its `qualifiedPath`
  segments, which core may concatenate but never split (ADR 0015).
- Member naming per plugin: Python `Outer.Inner.m` (container = module), Go
  `T.M` and interface methods `I.M` (container = package), TypeScript `C.m`
  (no container: owner must be in the same file), Rust inherent
  `krate::a::T::m`, Rust trait impl `krate::a::<T as Tr>::m` with alias path
  `krate::a::T::m` (aliases are stored only as `qualified_suffixes` text).
- All four shipped plugins are now pass-dependent
  (`receiver_calls = "resolved"`, `receiver_calls_structural = "unresolved"`,
  `semantic_pass = true`; TypeScript since GM-325). So with shipped plugins
  `P4_STATIC + S_PASS` is the text every session gets; `P4_PERM` renders only
  for a third-party plugin.

## 3. Decisions

### D1. Two sources for the field, chosen per language by a manifest capability

New `Capabilities::member_overrides` (plugin.toml `member_overrides`), an enum:

- `"by_name"`: a method overrides/implements the same-named member of a
  supertype. Core derives it (D2). True for Python, TypeScript and Go.
- `"declared"`: the plugin itself states which member a method implements, as
  a `SUPERTYPE_OF` edge from the method to the base member. Core only reads
  that edge. Intended for Rust (see Q1).
- `"none"` (default, also for a missing key): core says nothing for this
  language, and the instructions keep the sentence for it (D7).

Why not derive for every language: in Rust, `impl Square { fn area() }` is an
inherent method that implements nothing, yet `Square -SUPERTYPE_OF-> Shape`
and `Shape::area` exist; a by-name rule would name `Shape::area` on the
inherent method's page, which is false. And a trait-impl method's owner
`<Square as Shape>` names no node, so by-name finds nothing for exactly the
methods that do implement something. Rust's rule is "implements what its impl
block says", which only the plugin knows.

### D2. The by-name derivation (core)

For anchor `a` (kind `Function`, its language `by_name`, `qualifiedPath`
with at least 2 segments):

1. **Owner.** Join `a`'s segments except the last (same as
   `unlinked::is_type_member`). The owner is the `Type` node with that
   `qualifiedName`, same `language`, and the same `container` as `a`; when
   `a.container` is NULL (TypeScript), the same `filePath`. No owner: absent.
   The container/file filter matters: Go has a `T` in every package.
2. **Walk up.** Breadth-first over outgoing `SUPERTYPE_OF` edges from the
   owner, `resolved = 1` and `toId` a `Type` (a placeholder base, e.g. an
   external `unittest.TestCase`, is skipped: there is no member to name).
   Visited set (cycles in bad data), depth cap 8, visited-type cap 64.
3. **Member of a supertype `B`.** A `Function` node, not a placeholder, same
   language, named `a.name`, whose `qualifiedPath` minus its last segment
   equals `B`'s `qualifiedPath` (segment-wise, separators included), and in
   `B`'s container (or file when NULL). Lookup: `nodes.qualifiedName` range
   scan `>= B.qualifiedName` with a `name = ?` filter, then the segment
   comparison in Rust (no separator is guessed).
4. **Stop rule: nearest declaring ancestor per branch.** On a branch, the
   first supertype that declares the member is reported and the walk does not
   climb past it; a supertype that does not declare it is walked through.
   (Q2 offers "all ancestors" instead.)

So the walk is transitive through non-declaring types, but reports the
nearest declaration. A reported base that itself overrides something carries
its own `overrides` on its own caller page, so the chain is followable.

### D3. Rust (`declared`)

Recommended (Q1 option A): the Rust plugin emits, for each member `m` of
`impl Tr for T` whose trait path resolves to a project declaration, a
`SUPERTYPE_OF` edge from the member node to a placeholder addressing `Tr::m`
(the trait clause's resolved path plus one `::m` segment), through the same
`emit` path `supertype_to` uses (open site when cross-file). Core:

- `graph::symbol_links` kind filter: `SUPERTYPE_OF` may land on `Type` or
  `Function` (line 336).
- The field for a `declared` language = the anchor's outgoing resolved
  `SUPERTYPE_OF` edges to `Function` nodes. Exact, one row per impl: `<Circle
  as Loud>::speak` names `Loud::speak` only, never `Quiet::speak`.
- Side effects, both arguably correct: `find_implementations(Shape::area)`
  now lists the impl methods; `find_references(Shape::area)` lists them with
  `referenceKind: SUPERTYPE_OF` (which a trait-method rename needs anyway).
- `CURRENT_INDEXER_VERSION` bump (the extractor emits new edges).

### D4. Behaviour per language

| Language | Mode | Field appears | Absent means |
|---|---|---|---|
| Python | `by_name` | `class Sub(Base)` with `Sub.m`, `Base.m` (or an ancestor's `m`) both in the project. Structural, from the first index | no project base declares `m`, or the base is not in the project (external/unresolved) |
| TypeScript | `by_name` | `class C extends B` / `implements I`, `interface J extends I`, members `m` on both (interface method signatures are `Function` nodes). Structural | same as Python |
| Go | `by_name` | `T.M` where `T -SUPERTYPE_OF-> I` and `I.M` exists. Those edges exist **only after Go's semantic pass** (`answerImplements`) | before the pass: not known yet, and the page already carries `provenance` (Go semantic tier pending), so absence is disclosed; after the pass: `T` satisfies no project interface with `M`. Struct embedding is not a supertype, so a promoted method is never "overridden" |
| Rust | `declared` (Q1 A) | trait-impl methods whose trait is in the project | an inherent method, a free function, or a trait outside the project (`Display::fmt`) |
| any other | `none` | never | the instructions keep the sentence for this language (D7) |

Rust limitation (closed by GM-537): the plugin emits the edge only when the
structural tier resolves the impl's trait clause. The case seen in verify, a
trait reached through a glob import of another crate's re-export
(`use alpha::prelude::*;` then `impl Loud for Megaphone`), resolved to
nothing and got no `overrides`, even after rust-analyzer's pass. GM-537
addresses such a clause through the module's glob, so it now gets both edges
structurally; see `gm-537-rust-unresolved-trait-overrides.md`.

Absent field, in general: "this anchor overrides nothing g-mesh can name in
this project". It never means "no caller can reach it through a base outside
the project".

### D5. Field name and shape

On `find_callers` pages (`CallerPage`) and on the `answer` summary path
(`CallerDisclosures`), absent (not `[]`) when there is nothing:

```json
"overrides": [
  {"id": "9f0c…32 hex…", "qualifiedName": "Base.describe", "filePath": "pkg/base.py", "startLine": 11}
]
```

- `overrides`: an array, one row per base member, ordered by walk depth then
  `qualifiedName`; capped at 8 rows, with `overridesTruncated: true` when cut
  (only reachable with an unusual Go type satisfying many interfaces).
- `id` is what `find_callers(symbol_id=…)` takes, so the follow-up is one
  call. `startLine` zero-based, like every other row.
- The explanation travels once per session as a hint (`HintKey::Overrides`,
  `session_hints::OVERRIDES`), like `provenance`, triggered by the field's
  presence:
  "`overrides` names the base members this method overrides or implements. A
  call through a receiver typed as the base binds to the base member, so it
  is on that member's caller page, not this one: ask find_callers for each
  `id`." (226 bytes; Q3 offers an inline per-field hint instead.)

### D6. Cost on a page

- Bytes: ~128 per row with a 32-hex id (`,"overrides":[{…}]` for
  `Base.describe` in `pkg/base.py`), one row in the common case; the hint 226
  bytes once per session. Zero bytes on every anchor that overrides nothing,
  which is most of them.
- Queries: one owner lookup (`idx_nodes_qualifiedName`), one `fromId` scan
  per visited type (`idx_edges_fromId`), one bounded `qualifiedName` range
  scan per visited type. `declared`: one `fromId` scan. Same order as
  `untyped::probe`. Errors are swallowed to `None`, as in `untyped::probe`: a
  footnote must not fail an answer.
- `bound_page_in_response` measures the page with the field, so a page cut by
  the response bound stays within it.

### D7. What replaces `P4_STATIC`

All shipped plugins are pass-dependent and (after D1/D3) report overrides, so
for them the static sentence goes and `S_PASS` stands alone:

- New `S_PASS_ALONE` (216 bytes): "The one legitimate reason to grep
  afterward: until a language's semantic pass has run, a method call through
  a variable receiver (`x.foo()`) has no edge at all there; a page answered
  before then carries `provenance`."
- `receiver_paragraph`:
  - a `Never` language present: `p4_perm`/`P4_PERM_FALLBACK` as today, plus
    `S_PASS` (its "such a call" refers to `p4_perm`'s receiver call);
  - else, any covered language with `member_overrides = "none"`: `P4_STATIC`
    as today (+ `S_PASS`). It stays as the fallback for a plugin that does
    not report overrides;
  - else, any `PassDependent`: `S_PASS_ALONE`;
  - else (all `Static`, no shipped plugin): no receiver paragraph, and P2's
    last sentence ("Only fall back to grep for the one specific gap below")
    is not rendered, so P2 never points at a missing paragraph. P2 is split
    into `P2` (head) and `P2_GAP` for this.
- Saving for every shipped combination: `P4_STATIC + " " + S_PASS` = 473
  bytes -> 216 bytes, **-257 bytes**. The tests slice re-measures the totals
  with `cargo test -p g-mesh --lib mcp::instructions:: -- --nocapture` and the
  note's ADR 0022 table row 9 is marked delivered with the new numbers.
- Doc fixes: `P4_PERM_FALLBACK`'s doc (instructions.rs ~378) says TypeScript
  reports no per-page field; TypeScript now reports `untypedCalls`. New
  wording: the fallback names no language because at that ladder step the
  list is what was cut. `manifest.rs:157` (on `receiver_calls`) cites
  `P4_STATIC`; reword to cite the `overrides` field and `member_overrides`.

## 4. Edit map

| Function / item | File:lines (at c1ef86b) | Change |
|---|---|---|
| new module `overrides` (`probe(conn, anchor, mode) -> Option<Overrides>`, `owner_of`, `walk_up`, `member_of`, `declared`) | new `core/src/mcp/overrides.rs`, tests in `overrides_tests.rs` via `#[path]` | D2, D3 core read |
| `mod untyped;` | `core/src/mcp/mod.rs:75-76` | add `mod overrides;` |
| `CallerPage` | `core/src/mcp/find_callers_callees.rs:173-238` | field `overrides`, `overrides_truncated` |
| `CallerDisclosures` | same file `400-414` | same two fields |
| `CallerParts` / `CallerParts::response` | `448-528` | carry and emit; hint `once(overrides.is_some(), HintKey::Overrides, OVERRIDES)` |
| `handle_callers_in_covered` | `544-650` | `overrides::probe` beside `untyped::probe` (~line 573); both the `answer::respond` path and the page path |
| `HintKey`, new `OVERRIDES` | `core/src/mcp/session_hints.rs:13-20`, consts `72-146` | new key and sentence |
| `Capabilities` (+ new `MemberOverrides` enum, `Display` for `cli::plugins`) | `core/src/daemon/manifest.rs:124-165`; doc at `157` | D1; doc fix |
| `P2`, `P4_PERM_FALLBACK` doc, `P4_STATIC`, `S_PASS`, new `S_PASS_ALONE`, `receiver_paragraph`, `render` | `core/src/mcp/instructions.rs:208-212, 376-398, 404-441` | D7 |
| instruction tests asserting `{P4_STATIC} {S_PASS}` | `core/src/mcp/instructions/tests.rs:681, 1038-1067, 1090-1106, 1431-1447` | expectations per D7 |
| plugin manifests | `plugins/{python,go,typescript}/plugin.toml` (`by_name`), `plugins/rust/plugin.toml` (`declared`, Q1 A) | D1 |
| Rust member edge (Q1 A) | `plugins/rust/src/extractor/bodies.rs` `supertype_to` (~1488) and the impl-member declaration path (`decls.rs` `impl_block` 250-275, member declare ~502) | D3 |
| linker kind filter (Q1 A) | `core/src/graph/symbol_links.rs:333-336` | `SUPERTYPE_OF` -> `Type` or `Function` |
| `CURRENT_INDEXER_VERSION` (Q1 A) | `core/src/storage/schema.rs:161` | bump |
| ADR 0022 row 9 / owed (b); gm-325 doc mentions | `docs/adr/0022-instructions-coverage-states.md:56,64`; `docs/architecture/gm-325-typescript-lsp-semantics.md:300,619` | mark delivered, point here |

Read for context: `untyped::probe`/`is_method`, `unlinked::is_type_member`,
`unlinked::CandidateTally`, `find_implementations::list_implementations`,
`storage::qualified_path::decode`, `provenance::Resolved::disclose`.

## 5. Behaviour list and test plan (one control each)

| # | Behaviour | Test | Control (revert in code, test must fail) |
|---|---|---|---|
| B1 | Python `Sub.describe` overriding `Base.describe` (other file): field names `Base.describe` with its id | `overrides_tests`: by_name, cross-file base | `probe` returns `None` unconditionally |
| B2 | Absent for: a free function, a method of a class with no supertype, a method no supertype declares, a base that is an unresolved placeholder; JSON has no `overrides` key | same module, one test with four anchors | drop the `name = a.name` filter in `member_of` (any supertype member is reported) |
| B3 | Nearest declaring ancestor: `A.m`, `B(A)` without `m`, `C(B).m` -> `[A.m]`; with `B.m` added -> `[B.m]` only | two fixtures | (i) depth cap 1 (fails first fixture); (ii) do not stop at a declaring type (fails second) |
| B4 | Owner scoping: two `T` types in different Go packages (or two TS files) with different supertypes; the anchor's field names only its own package's base | fixture with hand-written nodes/edges | drop the container/file filter in `owner_of` |
| B5 | Capability gating: a Rust inherent `Square::area` with `Square -SUPERTYPE_OF-> Shape` and `Shape::area` -> absent; the same graph under `by_name` -> present | one fixture, two capability maps | ignore `member_overrides` (always by_name) |
| B6 | `declared`: `<Circle as Loud>::speak -SUPERTYPE_OF-> Loud::speak` -> `[Loud::speak]`, not `Quiet::speak`; plus a Rust plugin extractor test that the edge is emitted with the right placeholder | core fixture + `plugins/rust/src/extractor/tests.rs` | core: skip the declared source; plugin: remove the member edge emission |
| B7 | Wire: both the page path and `answer` summary path carry the field; the session hint is sent once (second call has the field, no hint sentence) | `find_callers_callees` tests via `handle_callers` | omit the field from `CallerDisclosures` (answer path) / use `peek` instead of `offer` |
| B8 | Instructions: go+python+rust+typescript all `by_name`/`declared` -> no `P4_STATIC`, contains `S_PASS_ALONE`, contains P2 head; a covered language with `member_overrides = "none"` -> `P4_STATIC + S_PASS` as today; all-`Static` -> no receiver paragraph and no `P2_GAP`; byte totals printed | `instructions/tests.rs` | restore the old `(true, _) => P4_STATIC` arm |

Eight controls (B3 has two). No test involves processes, threads or timers.

## 6. Must confirm (implement slice)

- `manifest.rs:342`: whether an unknown manifest key is rejected. If it is, a
  new key makes an older core refuse a newer plugin manifest; shipped plugins
  ride with core, so this matters only for mixed versions. State it.
- Python `__init__` overriding `Base.__init__` gets the field. Calls through
  `super().__init__()` bind to the base, so it is true; keep it unless a test
  shows noise.
- Go: confirm the semantic pass's `SUPERTYPE_OF` edges are `resolved = 1`
  after linking (they target a placeholder addressing the interface).
- TypeScript: confirm `interface` method signatures and class methods share
  the `C.m` path shape and that heritage edges are `resolved` same-file and
  linked cross-file.
- Rust (Q1 A): confirm a cross-file trait clause yields an open site the LSP
  bridge answers for the member edge too, or limit the member edge to traits
  the structural tier resolves and say so.

## 7. Questions for the owner

Owner's answers (2026-10-08):
- Q1: "A: плагин Rust сам ставит связь (Recommended)".
- Q2: "Ближайший объявивший предок (Recommended)".
- Q3: "Один раз за сессию (Recommended)".
- Q4: "Не сейчас, задача в бэклог (Recommended)".
  Delivered by GM-536: `find_references` runs the same `probe` and carries
  `overrides`/`overridesTruncated` on its page and `answer` summary; the hint
  key is shared, so the sentence (reworded to name both tools, 217 bytes) is
  sent once per session across `find_callers` and `find_references`.
- Note: "Утверждаю (Recommended)".

### Q1. Rust: how does a Rust trait-impl method get the field?

*Today:* every session reads, in the instructions, "an override's caller page
under-reports … find_implementations is the way across". For Rust this is the
trait case: `fn total(s: &dyn Shape) { s.area() }` is an edge onto
`Shape::area`, so `find_callers(<Square as Shape>::area)` does not list
`total`.

*The difficulty:* Python, TypeScript and Go can be derived by name ("a method
named like a supertype's member overrides it"). Rust cannot: the trait-impl
method's owner `<Square as Shape>` is not a node core can look up, and an
*inherent* `impl Square { fn area() }` implements nothing even though
`Shape::area` exists, so a by-name rule would claim something false.

*Options:*

- **A (Recommended): the Rust plugin states it.** For each method in
  `impl Shape for Square`, the plugin emits an edge `area -> Shape::area`.
  Output of `find_callers(<Square as Shape>::area)`:
  `"overrides": [{"id": "…", "qualifiedName": "crate::shapes::Shape::area", …}]`;
  for `<Circle as Loud>::speak` only `Loud::speak`.
  Benefit: exact, all four languages covered, so the 343-byte sentence leaves
  every shipped session (the acceptance criterion as written). Bonus:
  `find_implementations(Shape::area)` lists the impls. Risk: more work (plugin
  change, linker change, index rebuild on upgrade);
  `find_references(Shape::area)` gains impl rows (`referenceKind:
  SUPERTYPE_OF`), a visible change, though a rename needs them.
- **B: Rust waits for a follow-up task.** Rust's manifest says
  `member_overrides = "none"`; Rust trait impls get no field.
  Output: no `overrides` on any Rust page; any session indexing Rust keeps
  today's `P4_STATIC` text. Benefit: GM-502 stays core-only and small. Risk:
  the acceptance criterion "the sentence leaves the instructions" holds only
  for projects without Rust, and g-mesh's own repo is Rust.
- **C: core guesses from Rust's alias path.** Store alias paths, take the
  owner `Square` from `crate::shapes::Square::area`, and name every
  same-named member of `Square`'s traits. Output for `<Circle as
  Loud>::speak`: `[Loud::speak, Quiet::speak]` (one wrong). Benefit: no plugin
  change. Risk: a schema change, false rows when two traits share a method
  name, and inherent methods still need excluding.

### Q2. Which base members does the field name?

*Example:* `class A: def m`, `class B(A): def m`, `class C(B): def m`; anchor
`C.m`. A call through an `A`-typed receiver holding a `C` lands on `A.m`;
through a `B`-typed one, on `B.m`.

- **Nearest declaring ancestor (Recommended):** `"overrides": [B.m]`. `B.m`'s
  own page carries `[A.m]`. Benefit: smallest page, each hop a single fact.
  Risk: reaching `A.m` takes one more call.
- **Every declaring ancestor:** `"overrides": [B.m, A.m]`. Benefit: one call
  shows everything. Risk: longer rows on deep hierarchies (Python mixins, Go
  types satisfying many interfaces).
- **Direct supertypes only:** `[B.m]`, but with `B` lacking `m`, nothing at
  all, although `A.m` is overridden. Benefit: one hop, cheapest query. Risk:
  silently misses a real override, which is the failure this task removes.

### Q3. Where does the explanation of the field live?

- **Once per session as a `hint` (Recommended):** first page with the field
  carries the 226-byte sentence, later ones only the rows (as `provenance`
  does). Benefit: ~128 bytes per later page. Risk: an agent that dropped the
  first page's hint from its context sees a bare field (its name and `id` are
  still self-explanatory).
- **Inline on every field** (`{"members": […], "hint": "…"}`, as
  `untypedReceiverCalls` does). Benefit: every page is self-contained. Risk:
  +~230 bytes on every override page, and a changed shape (object, not array).

### Q4. `find_references` too?

*Today and after:* `find_references(Sub.describe)` under-reports the same
way. The acceptance criteria name only `find_callers`.

- **Not now (Recommended):** ship on `find_callers`, record a follow-up.
  Benefit: scope as approved. Risk: the references page stays silent.
- **Also `find_references`:** the same `probe` on `ReferencePage`. Benefit:
  consistent. Risk: an unapproved scope change, one more test set.
