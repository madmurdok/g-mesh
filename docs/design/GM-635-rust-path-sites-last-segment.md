# GM-635: ask Rust `REFERENCES` / `SUPERTYPE_OF` path sites at the last segment

Status: design (S1). Branch `fix/GM-635-rust-path-sites-last-segment`, off
release-4.12.0 (`fc6654c`). Follows GM-618, which moved only `CALLS` open
sites to the path's last segment.

Tags: [code] read in this branch; [run] probed with `rust-analyzer 1.97.1`
(goto-definition at the path start and at the last segment) on a scratch
fixture and on ripgrep `e89fff8` (`$TMPDIR/gmesh-bench-corpora/ripgrep`).
Probe script and raw output: `scratchpad/gm635-s1/{probe.py,probe-fx.out,probe-rg.out}`.
Machine during the probes: load 3.2 / 2.7 (1 min); fixture `real 11.15 user 0.14
sys 0.05`, ripgrep `real 13.81 user 0.16 sys 0.05` (the wall time is
rust-analyzer loading, the client only waits).

g-mesh calls used (project `g-mesh`, main checkout on release-4.12.0, same
code as this branch - `git diff` on `plugins/rust plugins/sdk/src/lsp` empty):

- `find_callers(path_last_segment)` -> `Bodies::open_site` (bodies.rs) and
  the unit test `the_last_segment_of_a_path_is_the_name_it_ends_in`
  (syntax.rs). No other caller.
- `find_callers(Bodies::open_site, id e64ab9a6…)` -> `receiver_call`,
  `field_access`, `emit`, `supertype_edge`, all in bodies.rs (complete).
- `find_callers(Emitter::open_site, id 05e03e8f…)` -> only `Bodies::open_site`.
- `find_definition(record_answer)` (sdk `lsp/bridge.rs`).
- grep (single file / non-code): `emit` and `open_site` call sites in
  bodies.rs, `OpenSiteKind::Implementation` in bridge.rs, open-site consumers
  across the repo (none in `crates/`).

## 1. Today

`Bodies::open_site` (bodies.rs:1384) records the position the semantic engine
is asked at:

```rust
let asked = if edge_kind == EdgeKind::Calls { path_last_segment(at) } else { at };
```

So for a `REFERENCES` site the engine is asked at the path's first character.
Example (fixture `src/lib.rs`, `inner::deep` reached only through `pub use inner::*`):

```rust
pub fn uses<T: deep::Tr>(t: T, w: deep::Wrap<u8>) -> Option<T::Item> {
    let _k = deep::K;   // REFERENCES open site asked at `deep`
    let _c = T::C;      // asked at `T`
```

rust-analyzer at `deep` answers `pub mod deep {` -> the bridge records
`uses REFERENCES deep` (a Module node). At `T` it answers the generic
parameter in `uses`'s own header -> the landing encloses the site -> dropped.
The item actually referenced (`deep::K`, `Tr::C`) is never recorded.

Which `REFERENCES` path sites are open at all [code]: `path_use` ->
`resolve_path` returns `Bound::Open` only for

- a generic-parameter qualifier: `T::C`, `S::Error`, `I::Item`
  (`resolve_type_qualified`, `scopes.binds`);
- `Self::X` with a prefix or outside a block;
- a module prefix the structural tier cannot resolve: `super::q::C` at the
  crate root, a module reached through a glob, `a::E::V` under such a prefix
  (`PathTarget::Unresolved`).

`Foo::BAR` with `Foo` a type this tier can address is a structural edge
(`member_of`), not an open site, and is unaffected. `<T as Tr>::X` produces no
site at all (`flatten_path` returns `None` for `bracketed_type`).

## 2. Per-kind decision

| Kind | Site (`at`) | Start answers [run] | Last segment answers [run] | Decision |
|---|---|---|---|---|
| REFERENCES value path, module qualifier `deep::K`, renamed `dd::K` | `scoped_identifier` | the module (`pub mod deep {`) | `const K` | **move** |
| REFERENCES value/type path, generic qualifier `T::C`, `T::Item`; ripgrep `S::Error` (sink.rs:226, core.rs:103) | `scoped_identifier` / `scoped_type_identifier` | the generic param decl (fn/impl header) -> dropped as encloser | trait's `const C` / `type Item` / `Sink::Error` (sink.rs:109), all nodes (`trait_const`, `assoc_type`) | **move** |
| REFERENCES type with generic args `deep::Wrap<u8>` | `scoped_type_identifier` (walk enters `generic_type`) | the module | `struct Wrap` | **move** |
| REFERENCES variant `deep::E::V`, pattern `deep::E::W(_)`; ripgrep `SearchMode::Count`, `Mode::Search`, `memmap::Advice::Sequential` | `scoped_identifier` | module / enum | the variant decl (no node until GM-630 -> innermost node = the enum) | **move** (see 4) |
| REFERENCES `Self::LOCAL` | `scoped_identifier` | the `impl Foo {` header | `const LOCAL` | **move** (rarely open: `self_member` binds it structurally) |
| REFERENCES `<T as Tr>::Item`, `<Self as Tr>::C` | none (`flatten_path` -> `None`) | null | trait item | no site; out of scope |
| SUPERTYPE_OF trait clause `impl deep::Tr for Foo`, `impl deep::Tr2<u8> for Foo` | trait clause (`scoped_type_identifier` / `generic_type`) | the module | the trait | **owner decision D1**: never asked, see below |
| SUPERTYPE_OF self type `impl Tr for other::Bar` (`supertype_edge` tail, bodies.rs:1502) | `self_type` | `other.rs:1:0` (file start) | `struct Bar` | same as above |
| CALLS (GM-618), receiver call, field access | unchanged | - | - | already last segment / single token |

`SUPERTYPE_OF` open sites carry `OpenSiteKind::Implementation`, and the bridge
skips them (`questions`, bridge.rs:1100: `OpenSiteKind::Implementation =>
{ skipped += 1; continue; }`); `SUPERTYPE_OF` answers come from the trait
sweep (`implementation_kinds = ["trait"]`, plugin.toml:110) asked at trait
names. Nothing else reads open sites (no consumer in `crates/`; census only
counts them). Moving them changes no edge today. **D1** for the owner:
(a, recommended) apply the last segment to every open site unconditionally -
one rule, right if the bridge ever asks these sites, zero graph effect now;
(b) widen the condition to `Calls | References` and leave `SupertypeOf` at
the start - also zero effect, keeps a special case that is wrong by the probe.

## 3. Why the range and ids do not move [code]

The bridge builds the answer's placeholder range with `site_range(source,
site.position, &site.name)` (bridge.rs:1545): it finds the occurrence of the
full written text (`site.name`, unchanged) that covers the asked column, so
the recorded range still spans `deep::K`, exactly as for GM-618's `CALLS`.
Edge case: a path written across lines (`a::\n    K`) - `written_start`
fails, range falls back to the last segment's column with the full width;
same as GM-618, rare, accepted.

`replaces` is `None` for every path site (`emit` passes `None`), so no R1/R2
retraction or upholding changes.

## 4. Interactions

- **GM-630 (variant nodes, same batch).** Before GM-630, a variant answer
  lands inside the enum with no node of its own; `node_at` takes the innermost
  node, the enum, and a `REFERENCES` landing is not held to `lands_on_name`
  (that rule is `CALLS`-only, bridge.rs:2771), so `REFERENCES -> enum` is
  recorded. After GM-630 the same answer lands on the variant node. Both are
  better than today's module landing. Tests here pin the asked column, not
  the landing, so they are order-independent; the measure slice must say
  whether GM-630 was merged in the arm it measures.
- **`use` renames** (`use inner::deep as dd; dd::K`): at the start
  rust-analyzer follows the alias to the module; at the last segment to `K`.
  No special case.
- **External items** (`I::Item` with `I: Iterator`, `fmt::Display`): the last
  segment answers std; `node_at` finds nothing; nothing recorded - same as
  today's drop.

## 5. Edit map

To change:

- `plugins/rust/src/extractor/bodies.rs:1394` `Bodies::open_site`: replace the
  `edge_kind == EdgeKind::Calls` condition per D1 - (a) `let asked =
  path_last_segment(at);`, or (b) `matches!(edge_kind, EdgeKind::Calls |
  EdgeKind::References)`. Update the doc comment (bodies.rs:1376-1382, "Other
  edge kinds keep the node's start").
- `plugins/rust/src/extractor/syntax.rs:96-100` `path_last_segment` doc:
  already kind-neutral; no change needed.
- `plugins/rust/src/extractor/tests.rs:2859`
  `single_segment_calls_and_non_call_sites_are_asked_where_they_always_were`:
  its `References` (and under D1(a) `SupertypeOf`) expectations pin the
  start today and must move to the last segment; rename the test (the
  "non-call sites" half no longer holds), keep the single-segment `ghost`
  half. Its doc comment (tests.rs:2848-2856) states the old rule ("an edge
  that is not a call keeps the node's start") and must be rewritten; its
  stated control (ask at `path_last_segment(at)` whatever the edge kind)
  becomes the fix itself under D1(a).

To read, not change: `emit` (bodies.rs:1339), `path_use` (1161),
`resolve_path`/`resolve_type_qualified` (1177/1226), `supertype_edge` (1440),
`asked_at`/`expect_at` helpers (tests.rs:2782/2797), bridge `questions`
(bridge.rs:~1090), `record_answer` landing filter (bridge.rs:2771),
`site_range` (bridge.rs:1545).

## 6. Behaviours the tests slice must pin (each with a control)

1. A `REFERENCES` path open site is asked at its last segment: module
   qualifier (`super::q::C` -> `C`), generic-param qualifier (`U::C` /
   `U::Item` in a signature -> `C` / `Item`), variant (`super::Mode::A` ->
   `A`), type with generic args (`super::W<u8>` -> `W`). Control: in
   `open_site`, restore `if edge_kind == EdgeKind::Calls { path_last_segment(at) } else { at }`.
2. (D1(a) only) `SUPERTYPE_OF` trait-clause and self-type sites at the last
   segment (`impl super::q::Tr for T` -> `Tr`; `impl super::Tr<u8> for T` ->
   `Tr`). Control: same revert as 1 (one build covers both).
3. Single-token sites unchanged (`ghost()`, field `x.f`, receiver call):
   already pinned (tests.rs:2324, the `ghost` half); no new control.
4. Placeholder range of a semantic `REFERENCES` answer still covers the whole
   written path: sdk bridge test (`plugins/sdk/tests/lsp_bridge.rs` or
   bridge unit tests) with a site asked at the last segment and `name` the full
   path. Control: in `site_range`, replace `written_start(..)` chain with
   `at.col`.

## 7. Must-confirm for the measure slice (ripgrep)

- Count of Rust `REFERENCES` open sites by shape before/after (census
  `OpenUnresolvedPath` rows with edge kind References) - the change moves
  positions only, so the count must be equal.
- Semantic `REFERENCES` edges: onto Module nodes should fall; onto
  `assoc_type` / `trait_const` nodes (expect `Sink::Error`, `Matcher::Error`
  from `S::Error` / `M::Error`) should rise. Report each delta row, not only
  totals.
- `SUPERTYPE_OF` delta must be exactly 0 (sites are never asked); a non-zero
  delta is a finding against section 2.
- State whether GM-630 is in the measured arm (variant landings: enum vs
  variant node).
- No new edge whose `from` encloses its `to` (landing filter still applied).
