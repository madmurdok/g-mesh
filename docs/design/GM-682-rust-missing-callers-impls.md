# GM-682: Rust `find_callers` / `find_implementations` miss results with no incompleteness flag

Slice S1 (diagnosis). No code changed. Both shapes reproduce on the live
index of this checkout (release-4.22.0) and on a minimal fixture with its own
`G_MESH_HOME`. Neither is about `cfg(test)`, `tests.rs` handling or a stale
index. Both are structural-tier (Rust plugin) gaps that do not show up in the
response: there is no edge, no open site and no `resolved: false` row.

| Shape | Cause, in one line |
|---|---|
| 1. missing callers | `m::f()`, where `m` reaches the module only through a glob `use` (`use super::*;`), is classified as an external-crate path. It gets no edge and **no open site**, so rust-analyzer is never asked about it. |
| 2. missing implementor | Two `impl Tr for Local` blocks, each inside a different fn of one module, get the same impl-block node id. `Emitter::declare` silently drops the second one. |

## 1. Reproduction

### Fixture

Path: `/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/127ab2bf-df91-4629-9ee4-11d79280bbed/scratchpad/gm682-fixture`
(a crate `fx`, edition 2021, no dependencies; `cargo check --tests` is clean).

- `src/storage/schema.rs`: `pub fn set_facts(x: u32) -> u32`.
- `src/daemon/seed.rs`: `use crate::storage::schema;` and:
  - `direct()` calls `schema::set_facts(1)`. Control: the module's own import binds the name.
  - inline `pub mod inline { use super::*; … }` (not `cfg(test)`) holds:
    - `via_glob_inline()` calls `schema::set_facts(2)` (shape 1);
    - `bare_via_glob()` calls `direct()` (control: a bare name through the same glob).
  - `#[cfg(test)] mod explicit_tests { use crate::storage::schema; … }`: `explicit_in_test` calls `schema::set_facts(3)` (control for `cfg(test)`).
  - `#[cfg(test)] mod tests;` (file-backed).
- `src/daemon/seed/tests.rs`: `use super::*;`, `via_glob_file()` calls
  `schema::set_facts(4)` (shape 1, the GM-670 shape), and `bare_via_glob_file()`
  calls `direct()` (control).
- `src/extract.rs`: `pub trait Extractor`, plus these implementors:
  - `Real` at module level;
  - fn-local `struct Nothing` + `impl Extractor for Nothing` inside `first()` **and again inside `second()`** (shape 2);
  - fn-local `Other` inside `third()` (control: a unique name);
  - fn-local `Nothing` inside `elsewhere::fourth()` (control: the same name in another module).

Driver: `scratchpad/gm682-query.py` (full path:
`/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/127ab2bf-df91-4629-9ee4-11d79280bbed/scratchpad/gm682-query.py`).
It runs `target/debug/g-mesh mcp-shim` (built from this checkout) with
`G_MESH_HOME=$TMPDIR/gm682h`, `G_MESH_PLUGIN_ROOTS_OVERRIDE=<repo>/plugins`
and no embedding model. It waits for the Rust semantic pass
(`language_state.semanticPassAt`) and then calls the MCP tools. Raw answers
are in `scratchpad/gm682-out.json`.

`G_MESH_HOME` is not in the scratchpad. The scratchpad path makes the daemon
socket path 170 bytes, and the platform limit is 103. The daemon was stopped
with `g-mesh stop` (pids 70820 and 72658, plugin 72717); no processes were
left.

### Output (rust-analyzer pass completed: `semanticPassAt 2026-10-31 22:27:15`, no error)

```
find_callers(set_facts)          hasMore=false, no truncated flag
    daemon::seed::direct                           (control, found)
    daemon::seed::explicit_tests::explicit_in_test (cfg(test) control, found)
    -- missing: inline::via_glob_inline, tests::via_glob_file
find_callees(via_glob_inline)    -> []
find_callees(via_glob_file)      -> []
find_callees(bare_via_glob)      -> daemon::seed::direct   (edge source: semantic / rust-analyzer)
find_callees(bare_via_glob_file) -> daemon::seed::direct   (semantic / rust-analyzer, in the cfg(test) file)

find_implementations(Extractor)  hasMore=false, no truncated flag
    extract::Real
    extract::<Nothing as Extractor>            (startLine 15: the impl in first())
    extract::<Other as Extractor>
    extract::elsewhere::<Nothing as Extractor>
    -- missing: the impl in second()
```

The index has edges only for the rows above, with no unresolved rows (`edges`
joined to `nodes`, read-only):
`daemon::seed::tests::bare_via_glob_file|CALLS|semantic|rust-analyzer|daemon::seed::direct`.
`via_glob_*` has none.

### The same on this repo's live index (MCP, project `g-mesh`)

- `find_callers(set_resolution_facts)` returns 11 callers, with
  `hasMore:false` and no flag. grep finds 5 more call sites, all of them
  `schema::set_resolution_facts(...)` written in a module whose `schema`
  comes from `use super::*;`:
  - `core/src/storage/language_swap.rs:1236,1237,1256` (inline
    `#[cfg(test)] mod tests { use super::*; }`; the parent has
    `use crate::storage::schema;` at line 59);
  - `core/src/daemon/seed/tests.rs:120,268` (file-backed; `seed.rs:27`
    imports `schema`).

  The brief named these paths `core/src/daemon/language_swap.rs` and
  `core/src/seed/tests.rs`. The real paths are the ones above.
- `find_callees(swap_imports_facts_and_paths)` contains `plan` and `swap`,
  bare names that came through the same glob, as **semantic** edges. It has
  no `set_resolution_facts` row. `find_callees(source_index)` contains only
  `create_index`.
- `find_implementations(Extractor)` returns 11 rows. The impl at
  `plugins/sdk/src/run.rs:1302`
  (`a_prepare_frame_starts_the_engine_before_any_pass`) is missing. It has
  the same name as the impl at `:1226`
  (`a_workspace_changed_frame_reaches_the_started_engine`). `nodes` holds
  exactly one `run::tests::<Nothing as Extractor>` row (id
  `b0bca015…`, line 1225).

## 2. Ruled out, and how

| Hypothesis | Verdict | Evidence |
|---|---|---|
| `cfg(test)` / `tests.rs` is skipped or handled differently | Ruled out | The fixture's `explicit_in_test` (`cfg(test)`, explicit import) is found. `bare_via_glob_file` in the file-backed `cfg(test)` `tests.rs` gets a rust-analyzer edge. The non-test inline module `inline::via_glob_inline` misses exactly like the test one. On the live index, `daemon/tests.rs` and `registry/tests.rs` callers (explicit path / explicit import) are found. |
| Index staleness after branch edits | Ruled out | The fixture index was built fresh in an empty `G_MESH_HOME` and misses the same calls. Live: rust `semantic_pending`, `semantic_owed_files` and `semantic_gap_files` are empty, and the language state is complete. |
| Semantic pass did not run / failed | Ruled out | Fixture and live both have `semanticPassAt` set and `semanticPassError` NULL. Bare glob calls get semantic edges in both. |
| A call inside a macro | Not the cause here | The live sites are plain statements. While building the fixture, a call inside `assert_eq!(…)` also produced no edge. That is the separate, documented macro token-tree gap (`bodies.rs` `macro_invocation`, 1064-1078), so the fixture's controls avoid it. |
| Symbol id collision for shape 1 | Ruled out | `set_resolution_facts` has one declaration, and the anchor resolves (`resolvedBy: name`, a unique id). |
| Duplicate local names (id collision) for shape 2 | **Confirmed** | See §3.2. |
| Pagination / limit | Ruled out | `limit` was 100 or 50, every result came back with `hasMore:false`, and the `files` totals match the rows. |

## 3. Causes

### 3.1 Shape 1: a glob-reached module qualifier is read as an external crate

`Bodies::call` (`plugins/rust/src/extractor/bodies.rs:555-583`) sends
`schema::set_facts` to `path_use` (1169-1183) and then to `resolve_path`
(1185-1232). That calls `keys::resolve_module_path`
(`plugins/rust/src/extractor/keys.rs:251-305`) on `["schema"]`, which tries
these rules in order:

1. a workspace crate name? No.
2. a `mod` this module declares? No.
3. a named `use` item of **this** module? No. `schema` is imported by the
   *parent*, and only `use super::*;` reaches it here.
4. Otherwise it is taken for an external crate:
   `PathTarget::ExternalCrate("schema")` (keys.rs:286).

`resolve_path`'s `ExternalCrate` arm (bodies.rs:1213-1217) returns
`Bound::Nothing`, and `emit` (1382-1401) writes **nothing** for that: no edge,
no placeholder, no open site. The semantic tier only answers open sites, so
rust-analyzer is never asked. The answer has no gap marker, because the
structural tier believes the path left the project.

Compare a *bare* call through the same glob (`plan()`, `direct()`).
`resolve_bare` (1097-1167) returns `Bound::Open` for a function name that is
neither declared nor imported, which is exactly the "came through a glob or
the prelude" case. The bridge then answers it. Path calls have no
equivalent rule. The code comments already list a narrower form of this gap
as known: a capitalized module reached only through a glob (`names_module`
doc, bodies.rs:1248-1252).

Scale on this repo (rough upper bound from grep): 157 files have
`use super::*;`. In them, `schema::` appears 232 times, `test_plugin::` 298
times, and `vectors::`, `connection::`, `manifest::`, `queries::` and others
tens of times each. Some of those files import the name themselves, but many
caller edges of `core` storage functions in test modules are likely missing
today.

### 3.2 Shape 2: fn-local impl blocks with the same name share one node id

A struct declared inside a fn body is not a declaration in this plugin: the
declaration pass `decls::collect` (`decls.rs:345-390`) walks only module and
impl/trait item lists. When the body pass reaches `impl Extractor for Nothing`
inside `first()`, `supertype_edge` (bodies.rs:1484-1553) finds no `Nothing`,
so `resolve_path` gives `Bound::Nothing`. It then declares a node for the
block itself (Decision 8, `declare_impl_block`, bodies.rs:1570-1589). The
node's name is the block prefix `<Nothing as Extractor>`
(`decls::impl_block`, decls.rs:257-285), qualified by the **module** only.

The node id is `node_id(file, kind, qualifiedName, nativeKind)`
(`emit.rs:182`). The impl in `second()` has the same file, kind `Type`,
qualified name `extract::<Nothing as Extractor>` and native kind `impl`, so
it gets the same id. `Emitter::declare` (`emit.rs:181-188`) keeps the first
node and silently returns the existing id for the second. The second
`SUPERTYPE_OF` edge has the same id as the first, so it is deduplicated too.
One implementor disappears, and nothing flags it.

The same name in another module (`elsewhere::fourth`) is a different
qualified name, so it is unaffected. The rust-analyzer implementation sweep
does not fill the gap, because Decision 8 deliberately records no open site
for a self type that names nothing.

## 4. Decisions for the owner

### Decision A: how shape 1 gets its edge

**Today:** in a module with `use super::*;`, the call `schema::set_facts()`
gets no edge and no open site when `schema` is imported by the parent. The
call simply is not in the index, and the answer says it is complete.

**Options:**

- **A1. Ask rust-analyzer.** When the first segment is not a crate, a
  declared `mod` or a named import, *and the module has a glob `use`*, emit
  an open site (`Bound::Open`) instead of `Bound::Nothing`. `std`, `core`
  and `alloc` stay external.
  - Example: `via_glob_file` gets the semantic edge to `set_facts` after the
    pass. Before the pass, the page carries `provenance`, the existing
    "answered before the semantic pass" signal, so the response does say it
    may be incomplete.
  - Benefit: small (one arm in `resolve_path`), and it covers both inline
    and file-backed glob modules.
  - Risk: every external-crate path in a glob module (`tempfile::tempdir()`,
    `serde_json::from_str()`, `anyhow::bail!`) also becomes an open site. On
    this repo that is on the order of 1,000 extra rust-analyzer `definition`
    requests per whole-project pass. Their answers land outside the project
    and are dropped. That costs pass time only, and is far below the
    `max_sites` limit of 1,000,000.
- **A2 (recommended). A1, plus the crate's Cargo dependency names count as
  external.** The Rust plugin's project context would also read
  `[dependencies]`, `[dev-dependencies]` and `[build-dependencies]`
  (including `target.*.` tables and `package =` renames, with `-` changed
  to `_`). A first segment that names a dependency stays
  `ExternalCrate`, so it gets no open site, as today.
  - Example: `tempfile::tempdir()` in a test module is still not asked;
    `schema::set_facts()` is.
  - Benefit: the open sites are only the real "project or unknown" cases.
  - Risk: more code (the `cargo_manifest` reader currently ignores
    dependency tables). A dependency spelled differently from its crate
    name, such as a `lib.name` override, would still become an open site,
    which is harmless.
- **A3. Follow the glob statically, only for a parent in the same file.**
  The file model already knows the parent's `use` items, so
  `mod tests { use super::*; schema::f() }` resolves to
  `storage::schema::f` with no rust-analyzer involved.
  - Benefit: exact, and works with the semantic tier off.
  - Risk: it does not fix the file-backed `seed/tests.rs` case (the
    parent's imports are in another file), which is half of the observed
    misses. It only makes sense as an addition to A1 or A2.

**Consequence of A2:** Rust caller and reference answers gain the
glob-reached path calls after the semantic pass. Whole-project semantic
passes get slightly longer in glob-heavy crates (to be measured, see §7).
The `names_module` doc's "known limit" sentence and the module doc's
Decision 7 list must be updated.

### Decision B: how shape 2's impl blocks get distinct identities

**Today:** two `impl Tr for Local` blocks in different fns of one module
are one node, and the second is invisible to `find_implementations`.

**Options:**

- **B1 (recommended). Qualify a fn-local impl block by its enclosing fn,
  the way rustc's def paths do.** The block in `second()` becomes
  `extract::second::<Nothing as Extractor>`, and the one in `first()`
  becomes `extract::first::<Nothing as Extractor>`. `Bodies` keeps a stack
  of enclosing fn names, pushed in `function`. Only the Decision 8 node
  (and its `qualifiedPath`) changes; module-level impl blocks keep today's
  names.
  - Example: `find_implementations(Extractor)` on this repo lists
    `run::tests::a_workspace_changed_frame_reaches_the_started_engine::<Nothing as Extractor>`
    and `run::tests::a_prepare_frame_starts_the_engine_before_any_pass::<Nothing as Extractor>`.
  - Benefit: readable names, stable under edits elsewhere in the file, and
    the reader sees which test owns each impl.
  - Risk: every existing fn-local impl block is renamed. The plugin's
    fingerprint changes, so indexes rebuild; nothing links to these nodes
    by name from other files. The rare case that remains is two sibling
    `{ }` blocks in one fn, each declaring the same local type and impl,
    which would still collide. B3 below covers it.
- **B2. A collision counter.** In `declare_impl_block`, when the id is
  already taken in this file, retry with `#2`, `#3` and so on.
  - Example: `extract::<Nothing as Extractor>#2`.
  - Benefit: smallest change; first-seen names do not change.
  - Risk: an artificial name, whose number depends on source order, so
    reordering two tests swaps their ids. It does not say which fn the
    impl is in.
- **B3. B1, plus B2 as a guard** for the residual same-fn collision.

**Consequence of B1/B3:** `find_implementations` returns one row per impl
block. Names of fn-local impl blocks change shape. That is visible in
`get_file_outline`, and needs a changelog line.

**Out of scope, noted:** `Emitter::declare`'s silent "first wins" also
merges other same-named declarations in one file, for example
`#[cfg(unix)] fn f` and `#[cfg(windows)] fn f`. For those, merging into one
symbol is arguably right, so this task does not change `declare` globally.

## 5. Edit map

| File | Function / lines | Change |
|---|---|---|
| `plugins/rust/src/extractor/bodies.rs` | `resolve_path` 1185-1232 (the `ExternalCrate` arm, 1213-1217) | A1/A2: with `self.model.has_glob(&module.key)`, a first segment other than `std`/`core`/`alloc` (A2: and not a known dependency) gives `Bound::Open`. Keep the census reason distinct (new `Reason::OpenGlobPath`). |
| `plugins/rust/src/extractor/keys.rs` | `resolve_module_path` 251-305 and its rules doc 225-250 | Keep it pure. It is shared by `use` resolution, `names_module`, `call_type`, `struct_address` and `resolve_written`, so do not change rule 5 there. Make the glob decision in `resolve_path` only. If A2 needs the dependency list, pass it through `ProjectContext`. |
| `plugins/rust/src/project/cargo_manifest.rs` | `read` 74 and `RawCargoToml` | A2: read the dependency table keys and `package =` renames. |
| `plugins/rust/src/project/mod.rs` | `ProjectContext` (`load` 222, `crates` 461) | A2: store per-crate external names, and add an accessor such as `is_external_crate(crate_key, name)`. |
| `plugins/rust/src/extractor/bodies.rs` | module doc 1-80 (Decision 7 list), `names_module` doc 1248-1252 | Document the glob-path open site, and narrow the "known limit" text. |
| `plugins/rust/src/extractor/bodies.rs` | `Bodies` struct 118+, `function` 412-426 | B1: push and pop the enclosing fn name around the body walk. |
| `plugins/rust/src/extractor/bodies.rs` | `declare_impl_block` 1570-1589 (called from `supertype_edge` 1484-1553) | B1: prefix the name and `qualifiedPath` with the enclosing fn segment(s). B3: add the collision suffix. |
| `plugins/rust/src/extractor/emit.rs` | `declare` 181-188 | B2/B3 only: expose whether the id was new (for example `declare_new -> Option<String>`) so the caller can retry. |
| `plugins/rust/src/extractor/tests.rs` | next to `a_blanket_impl_is_a_supertype_edge_from_the_impl_block_itself` (1568) and the glob tests (1741-1822) | Unit tests, see §6. |
| `core/tests/it/` | new file modelled on `rust_glob_trait_overrides.rs` (real shim, daemon and plugin) | End-to-end test for shape 1 **with** rust-analyzer, plus shape 2 structural. Add a `mod` line in `core/tests/it/main.rs`. |

Callers and references this relied on: the live MCP `find_callers` /
`find_callees` / `find_implementations` answers (§1); the call chain
`call → path_use → resolve_path → resolve_module_path` and
`impl_item → supertype_edge → declare_impl_block → Emitter::declare`, read
directly in those files. No g-mesh call failed.

## 6. Behaviours the tests must pin

1. In an inline `mod m { use super::*; }`, where the parent imports
   `use crate::a::b;`, the call `b::f()` emits an open site (A1/A2) and no
   edge. Unit test, `plugins/rust/src/extractor/tests.rs`.
2. The same in a file-backed child (`mod tests;` + `tests.rs` with
   `use super::*;`): an open site.
3. In a glob module, `std::mem::take()` emits no open site. A2 adds: a path
   whose first segment names a Cargo dependency (for example
   `tempfile::tempdir()`) emits no open site either.
4. Without a glob, an unknown first segment still emits nothing, as today.
5. End to end with rust-analyzer: `find_callers(set_facts)` on the fixture
   shape returns `direct`, `explicit_in_test`, `via_glob_inline` and
   `via_glob_file`. The two glob rows come from the semantic tier.
6. Two fn-local `impl Tr for Nothing` in two fns of one module give two
   impl-block nodes and two `SUPERTYPE_OF` edges, and
   `find_implementations(Tr)` lists both (B1 names: `m::first::<Nothing as Tr>`
   and `m::second::<Nothing as Tr>`).
7. A module-level `impl Sink for Box<S>` keeps its Decision 8 name
   `<Box<S> as Sink>`, so the existing test
   `a_blanket_impl_is_a_supertype_edge_from_the_impl_block_itself` is
   unchanged.
8. (B3) Two sibling blocks in one fn with the same local impl give two
   nodes.

Controls (revert the code, keep the test): restoring `Bound::Nothing` in
the `ExternalCrate` arm must fail tests 1, 2 and 5; dropping the
enclosing-fn prefix must fail test 6.

## 7. Must confirm before or while implementing

- **Open-site cost (A1 vs A2).** Count the extra open sites, and the change
  in whole-project semantic pass time, on this repo (157 glob modules) and on
  ripgrep. Use one run each and record `uptime` and `user`/`sys` against
  `real`. If A1's overhead is negligible, A2's manifest work may be dropped.
- **rust-analyzer resolves a module-qualified open site from a glob.** The
  site is asked at the path's last segment (`open_site`, bodies.rs:1428-1450).
  The fixture shows rust-analyzer answers *bare* glob names. A
  `schema::set_facts` site has not been observed end to end yet, because no
  code was changed in this slice. Behaviour 5 is the check.
- **`replaces` / retraction.** The new open site has no structural edge, so
  `replaces: None`. Confirm the bridge accepts a `Calls` site with
  `OpenSiteKind::Reference` (the same as `resolve_bare`'s glob case today).
- **B1 renames.** Confirm nothing outside the Rust plugin depends on the
  exact `<T as Tr>` name of a fn-local block (the bridge implementation
  sweep matches by position, not by name). Confirm the plugin fingerprint
  changes so existing indexes rebuild.
- **Owner choices:** Decision A (A2 recommended; A3 optional on top) and
  Decision B (B1 recommended, or B3 for the same-fn residual).

## 8. Review fixes (GM-682/S6)

- **F1: a Cargo dependency edit re-extracts its package.** A2 made each
  package's dependency names an input of extraction (`name::f()` in a
  glob-importing module is an open site only while `name` is no
  dependency), but `RustFacts` did not carry them, so adding, removing or
  renaming a dependency answered `Unchanged` and kept stale open sites (or
  their absence) until the `.rs` file was edited. `RustFacts` now holds
  `dependencies` (package dir -> names, as `ProjectContext::is_dependency`
  reads them); `facts::delta` adds, through `with_dependency_changes`, a
  `files` scope for every package whose set changed (packages nested under
  it excluded). Facts `FORMAT` is 3, so stored format-2 facts answer
  `Unknown` once and refresh. The tests that pinned "a dependency edit
  re-extracts nothing" now edit only the version and features.
- **F2: colliding Decision-8 impl blocks get an ordinal.** Two
  `const _: () = { struct K; impl Tr for K {} };` in one module (or two
  `{}` blocks in one fn) qualified to the same `m::<K as Tr>`, and the
  second was dropped by the emitter's first-wins rule. `declare_impl_block`
  now checks `Emitter::is_declared` and retries with `#2`, `#3`, ... on the
  name and qualified name (`m::<K as Tr>#2`), in source order. A
  non-colliding block keeps its B1 name. Residual: the second block's
  methods still share the first's prefix (`<K as Tr>::f`), and two `cfg`
  alternatives of one such block are now two nodes rather than one.
