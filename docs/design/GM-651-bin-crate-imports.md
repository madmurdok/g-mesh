# GM-651: a bin crate's own modules are orphans when the package also has a lib

Status: design, pending owner review. Base: `release-4.18.0` (30c7aaa).
No production code changes in this slice.

## 1. Today

### 1.1 The reproduction

A package with both targets, the shape almost every Rust CLI has:

```text
Cargo.toml          [package] name = "tool"
src/lib.rs          pub mod util;
src/util.rs         pub fn u() {}
src/main.rs         mod config; use crate::config::Settings; use tool::util::u;
src/config.rs       pub mod inner; pub struct Settings;
src/config/inner.rs use super::Settings;
```

Measured with the installed plugin (`~/.local/share/g-mesh/bin/plugins/rust/g-mesh-plugin-rust
--bulk-index <fixture>`, g-mesh 4.16.0; the code paths below are unchanged in
4.18.0). The containers it emits:

| file | container key | `qualifiedName` of its items |
|---|---|---|
| `src/lib.rs` | `tool` | `lib_fn` |
| `src/util.rs` | `tool::util` | `util::u` |
| `src/main.rs` | `orphan:src/main.rs` | `main` |
| `src/config.rs` | `orphan:src/config.rs` | `Settings` (should be `config::Settings`) |
| `src/config/inner.rs` | `orphan:src/config/inner.rs` | `i` (should be `config::inner::i`) |

And the `IMPORTS` edges:

| from | target container | specifier | links? |
|---|---|---|---|
| `src/main.rs` | `orphan:src/main.rs::config` | `crate::config` | no: no file has that key |
| `src/main.rs` | `tool::util` | `tool::util` | yes |
| `src/config/inner.rs` | - | (`use super::Settings`) | **no row at all** |

So `get_dependencies(src/main.rs)` and `deps --json` show an unresolved import
where the edge to `src/config.rs` should be, and `inner.rs` shows no import.
`crate::config` from a *bin-only* package works (`core/tests/it/cli_deps.rs`'s
`FILES` fixture pins `src/main.rs -> src/config.rs`, key `app::config`); the
failure needs the lib to exist.

### 1.2 Why: the bin crate loses a key collision and its whole tree is dropped

`ProjectContext::load` (`plugins/rust/src/project/mod.rs:214-292`) registers
one crate per target (`resolve_targets`, `:546-582`): the lib (`src/lib.rs`,
named after the package) first, then every bin (`src/main.rs`, also named
after the package by default). A crate's container key is its normalized name
(Decision 6). Both targets are `tool`, so the bin hits

```rust
if !used_keys.insert(key.clone()) {
    notes.push(format!("crate key {key:?} ... collides with an earlier crate of the same name - keeping the first"));
    continue;                       // the bin crate is never scanned
}
```

(`mod.rs:266-272`). The module doc ("One package, several crates",
`mod.rs:48-63`) records this as the intended "first registered wins" rule; the
test `a_lib_and_bin_defaulting_to_the_same_name_do_not_merge_their_containers`
(`mod.rs:829-848`) pins it. Its purpose was right (two crates must not share
one container), its consequence is not: the bin's `module_tree::scan_crate`
never runs, so `main.rs` and every file it reaches fall to
`container_for`'s `Orphan` arm (`mod.rs:447-452`).

The extractor then works from those orphan keys (`extractor/keys.rs`):

- `ModuleCtx::for_file` (`keys.rs:101-111`) makes an orphan file its own crate
  root, so in `main.rs` `crate` = `orphan:src/main.rs`, and
  `resolve_module_path` (`keys.rs:247-316`) turns `crate::config` into
  `orphan:src/main.rs::config`. No file carries that key (`config.rs` is
  `orphan:src/config.rs`), so core's import linker finds no container and the
  edge stays an unlinked `resolved_module`.
- In `inner.rs` (key `orphan:src/config/inner.rs`) `super` is
  `parent_key(key)` = `None` (`keys.rs:181-183`): an orphan's top level has no
  modelled parent. `resolve_module_path` answers `Unresolved`, and
  `Declarer::use_leaf` (`extractor/decls.rs:709-800`) returns before
  `import_edge` (`decls.rs:828-840`), which emits nothing for `Unresolved`.

### 1.3 `use super::` is not broken on its own

In a module the tree does reach, `use super::` already draws an edge. Same
fixture, plus `src/util.rs: pub mod deep;` and
`src/util/deep.rs: use super::u; use super::super::lib_fn;`:

| from | target | specifier |
|---|---|---|
| `src/util/deep.rs` | `tool::util` | `super` |
| `src/util/deep.rs` | `tool` | `super::super` |

So the task's "`use super::...` produces no IMPORTS row at all" is the same bug
seen from a child file: it holds exactly for files that are orphans, and every
file below a colliding `main.rs` is one.

## 2. Decision 1: give the colliding bin crate its own key, `tool[bin]`

### What changes

When a **bin** target's key is already taken, register it under
`<key>[bin]` instead of dropping it. Only if that key is taken too (two
packages whose bins share one name) keep today's note-and-skip.

| file | today | after |
|---|---|---|
| `src/main.rs` | `orphan:src/main.rs` | `tool[bin]` (crate root, no parent) |
| `src/config.rs` | `orphan:src/config.rs` | `tool[bin]::config`, parent `tool[bin]` |
| `src/config/inner.rs` | `orphan:src/config/inner.rs` | `tool[bin]::config::inner` |

Every rule of the extractor then gives the right answer **with no extractor
code change**, because each one already works from the key:

- `crate::config` in `main.rs`: `crate_root_of("tool[bin]")` = `tool[bin]`, so
  the target is `tool[bin]::config`, which `config.rs` carries. Linked; `deps
  --json` prints `src/main.rs -> src/config.rs`, module `tool[bin]::config`.
- `use super::Settings` in `inner.rs`: `parent_key("tool[bin]::config::inner")`
  = `tool[bin]::config`. An `IMPORTS` row, linked to `config.rs`.
- `use tool::util::u` in `main.rs`: rule 2 of `resolve_module_path` matches a
  bare first segment against `crates()` keys; `tool` is the lib's key, never
  the bin's, so it still names the lib - which is what Rust means (a bin
  refers to its package's lib by the crate name).
- `pub(crate)` in a bin module: `Visibility::Container("tool[bin]")`. The bin's
  modules are on that parent chain and the lib's are not, which is Rust's rule
  (the lib cannot see the bin's `pub(crate)` items, and vice versa).
- `qualifiedName`s drop the crate name (`keys.rs` Decision 2), so they become
  `config::Settings` and `config::inner::i`, the same shape as a lib's.

### Why this spelling

A key must (a) never equal a real `<crate>::<path>` key, (b) never be matched
by a bare path segment (the bin cannot be named from Rust), (c) contain no
`::` (it is a crate root: `crate_root_of`, `module_path_of`, `parent_key` and
the facts' `container_delta` all split on `::`). `tool[bin]` is not an
identifier, so (a) and (b) hold by construction, like `orphan:` keys
(Decision 5); it has no `::`. Core treats keys as opaque (checked:
`graph::containers::own_module_files`, `graph::imports`, `cli::deps`'s
`fallback_targets` look keys up, never parse them; the SDK does not validate
key characters). It also keeps the crate name first, so the lib's and the
bin's containers sort next to each other in `deps --json` and a user typing
`get_dependencies file_path: "tool[bin]::config"` reads what it is. Cargo
itself says the same thing in its messages (`tool (bin "tool")`).

### Consequences

- The collision note disappears for this case: nothing is lost any more.
- **Creating or deleting `src/lib.rs` in a package with `src/main.rs` now
  re-keys the bin** (`tool` <-> `tool[bin]`). The facts' crate set changes
  (`{tool}` -> `{tool, tool[bin]}`), so `facts::delta` answers `Unknown`
  (re-extract everything) where deleting `lib.rs` answered `Affected` before.
  Creating `lib.rs` was already `Unknown`. A rare event; correct, and the
  same cost class as any `Cargo.toml` target edit.
- A bin-only package keeps the plain key `tool`: no churn for existing
  bin-only indexes, the conformance fixture (`crates/beta`, bin-only) or the
  `cli_deps.rs` fixture as it stands.
- Node ids of everything below `main.rs` change (their `container` and
  `qualifiedName` change). That is the fix, and it reaches existing indexes
  through the normal reindex of a plugin upgrade.

### Known limit, unchanged

A file both crates declare (`mod util;` in `lib.rs` **and** in `main.rs`) is
compiled twice by cargo, but the index maps one file to one container:
`module_tree`'s `taken` set gives it to the lib (first registered), the bin's
`crate::util` targets `tool[bin]::util`, which no file has, and stays
unresolved, as today. Idiomatic bin+lib code writes `use tool::util` instead,
which links. Not in scope.

### Benefits / risks

- Benefit: one change in the project model fixes both acceptance criteria;
  extractor, core and the semantic tier are untouched (the semantic tier is
  the SDK's position-based LSP bridge and reads no container keys).
- Risk: a user-visible key with a new shape (`[bin]`). Anything outside the
  plugin that assumed "a Rust key is `::`-joined identifiers or `orphan:`"
  would misread it; I found no such reader (section 6).
- Risk: the lib/bin key flip on `lib.rs` create/delete costs a full
  re-extract (above).

### Rejected alternatives

1. **Always key bins distinctly** (`tool[bin]` even with no lib). Stable key
   across `lib.rs` create/delete, but re-keys every bin-only project in every
   existing index and fixture for no answer it improves.
2. **`bin:tool`** (a prefix, like `orphan:`). Works equally (no `::`, not an
   identifier). Rejected only for legibility: `orphan:` means "not modelled",
   and a modelled crate reading like one invites confusion; the suffix keeps
   lib and bin adjacent. Owner may prefer it - section 8, Q1.
3. **Nest the bin under the lib** (`tool::main`-style key). Wrong visibility
   (the lib's `pub(crate)` would cover the bin) and it can collide with a real
   lib module of that name.
4. **Merge bin into the lib's container `tool`** (treat `main.rs` as part of
   the lib). Wrong for `crate::` (a bin's `crate::config` is not the lib's) and
   for `pub(crate)`; the module doc already rejects merging.
5. **Fix in the extractor only** (resolve `crate::` in an orphan `main.rs`
   against `orphan:` keys of the files it declares). Would need the extractor
   to rebuild the module tree the project model already knows how to build;
   all qualifiedNames below stay wrong.

## 3. Decision 2: `use super::` - no separate change

As section 1.3 shows, `use super::` already emits `IMPORTS` wherever the
module tree reaches the file; Decision 1 makes the bin's files reachable, so
they gain the rows. What stays without a row, on purpose:

- **`use super::x` in a genuinely unreachable file** (a `.rs` no crate
  declares, `orphan:<path>`). Nothing above it is modelled, so there is no
  container to point at. The alternative, an `IMPORTS` edge onto an unlinked
  placeholder so `deps --json` counts it "unresolved", was rejected: it would
  invent an address (`orphan:<path>` has no parent key to give it) only to
  count it, and the file's own orphan status already says the model does not
  know where it sits.
- **`super` above a crate root** (`use super::x` in `lib.rs`/`main.rs`): a
  compile error in Rust; `super_above_a_crate_root_resolves_to_nothing`
  (`keys.rs:479`) pins it.

## 4. Edit map

| file | function / lines | change |
|---|---|---|
| `plugins/rust/src/project/mod.rs` | `struct Target` `:584-590` | add `kind: TargetKind` (`Lib` / `Bin`) |
| same | `resolve_targets` `:546-582` | the `push` closure takes the kind: `Lib` for the `[lib]` push, `Bin` for each bin |
| same | `ProjectContext::load` `:258-275` (the `used_keys.insert` branch) | on collision of a `Bin` target, try `bin_key(&key)`; register under it if free; else today's note + `continue`. A `Lib` collision is unchanged. |
| same | new `fn bin_key(key: &str) -> String` beside `normalize_crate_name` `:593-595` | `format!("{key}[bin]")`, with the doc saying why it cannot collide (Decision 1's (a)-(c)) |
| same (doc edits follow the repo's comment rule: present tense, no ticket ids; the decision itself is this note, link it once at module level) | module doc `:48-63` ("One package, several crates") and Decision 6 `:86-104` | replace "first registered wins ... loser recorded in notes" for a lib/bin pair by the `[bin]` rule; keep first-wins for the remaining collisions |
| same | `Crate::key` doc `:136-137` | mention the `[bin]` form |
| `plugins/rust/src/extractor/keys.rs` | `ModuleCtx::key` doc `:89-91`, `crate_root_of` doc `:164-165` | doc only: a crate-root key may be `<name>[bin]`, never matched by a bare segment |
| `plugins/rust/src/project/facts.rs` | none | the crate set simply includes `tool[bin]`; confirm `delta`'s `Unknown` on lib.rs create/delete is accepted (section 8, Q2) |

Read for context, not to change: `module_tree::scan_crate`
(`project/module_tree.rs:168-190`, builds child keys from `crate_key`),
`ProjectContext::rescan` (`mod.rs:414-438`, reuses `krate.key`, so a re-scan
keeps the `[bin]` key with no change), `reload_for` (`mod.rs:371-385`, a
reload is a fresh `load`), `resolve_module_path` (`keys.rs:247-316`),
`Declarer::use_leaf` / `import_edge` (`decls.rs:709-840`).

Existing tests whose expectations change (update, do not delete):

- `project/mod.rs` `a_lib_and_bin_defaulting_to_the_same_name_do_not_merge_their_containers`
  (`:829-848`): now `crate_keys == ["tool", "tool[bin]"]`, `main.rs` is
  `Member { key: "tool[bin]", parent: None }`; `src/shared.rs` stays the lib's
  (`tool::shared`) and the bin's `mod shared;` yields the existing
  "two mod items claim one file" note instead of the collision note. Keeps the
  "do not merge" point.
- `project/mod.rs` `deleting_the_winning_lib_rs_promotes_the_bin_under_the_same_key`
  (`:1154-1185`): with the lib, `src/cli.rs` is `tool[bin]::cli`; after
  deleting `lib.rs` it is `tool::cli`; both deltas are now `Unknown` (rename
  the test: the bin takes the plain key back).
- `project/mod.rs` `saving_a_root_that_lost_the_key_collision_does_not_reload`
  (`:1243-1258`): `main.rs` is no longer a loser. Rewrite on a real remaining
  collision (two workspace members whose libs share a name) so the "saving a
  losing root reads no disk" behaviour stays pinned.
- `core/tests/it/module_tree_refresh_rust.rs`
  `creating_and_deleting_src_lib_rs_moves_the_crate_without_a_cargo_toml_save`
  (`:298-325`): after creating `lib.rs`, `cli_fn` is `alpha[bin]::cli`, not
  `orphan:src/cli.rs`; doc comment `:298-302` updated.

## 5. Behaviours the tests must pin, with controls

Control revert for all of B1-B5 is the same production change (the new
collision branch in `load`); they sit in three test binaries
(`g-mesh-plugin-rust` lib tests, core `it`, core `it` cli_deps), which the
verify slice can build as one group.

| # | behaviour (one line) | where | control |
|---|---|---|---|
| B1 | lib+bin package: `main.rs` is `Member{tool[bin], None}`, `config.rs` is `tool[bin]::config` with parent `tool[bin]`, lib keys unchanged | `project/mod.rs` unit test | make the `Bin` collision branch `continue` as today: `main.rs` is an orphan |
| B2 | extractor on that fixture: `main.rs` `use crate::config::Settings` targets container `tool[bin]::config`; `use tool::util::u` still targets `tool::util` | `extractor/tests.rs` | same revert: target is `orphan:src/main.rs::config` |
| B3 | `inner.rs` `use super::Settings` emits one `IMPORTS` onto `tool[bin]::config` | `extractor/tests.rs` | same revert: no `IMPORTS` from `inner.rs` |
| B4 | `deps --json` and `get_dependencies` on a bin+lib project show `src/main.rs -> src/config.rs` (module `app[bin]::config`) and `src/config/inner.rs -> src/config.rs` | `core/tests/it/cli_deps.rs`: add `src/lib.rs` and `src/config/inner.rs` (`use super::X;`) to `FILES` (avoids a second cold-daemon test; the test already compares the two answers file by file) | same revert: the edge is unresolved, the existing `contains(edge(...))` assertion fails |
| B5 | creating `lib.rs` re-keys the bin to `alpha[bin]` without a `Cargo.toml` save, deleting it gives the plain key back, each state equal to a cold index | `module_tree_refresh_rust.rs` (updated test) | same revert: `cli_fn` is `orphan:src/cli.rs` |
| B6 | a bin whose `[bin]` key is also taken keeps today's note-and-skip (no panic, no merge) | `project/mod.rs` unit test: workspace member A has bin `x`; member B has lib `x` and bin `x`. A's bin is `x` (registered first), B's lib loses (note, as today), B's bin becomes `x[bin]`. Add member C with bin `x`: it finds `x` and `x[bin]` taken and is noted and skipped | drop the "`x[bin]` taken" check (register unconditionally): C's tree overwrites B's files' keys, and the assertion on C's note fails |
| B7 | an orphan file's `use super::x` still emits no `IMPORTS` | existing `keys.rs` / extractor coverage; add one extractor case if none pins it | emit an edge for `Unresolved` in `import_edge`: a row appears |

Controls B1-B5 share one revert, which is allowed by the rule (one control
per behaviour/fix, not per test): the fix is one branch. B6 and B7 have their
own. Total: 3 control builds.

## 6. Cross-file answers relied on, and how they were found

| question | answer | source |
|---|---|---|
| Who builds module containers / crate keys | `ProjectContext::load` -> `resolve_targets` -> `module_tree::scan_crate`; `container_for` answers per file | grep `orphan:` over `*.rs` (non-code-symbol string), then reading `project/mod.rs` by function |
| Who resolves `crate::`/`super::`/bare paths | `extractor::keys::resolve_module_path`; callers `Declarer::use_leaf`, `Declarer::extern_crate`, `Bodies::resolve_written`, `struct_address`, `resolve_path`, `names_module`, `resolve_type_qualified` (complete, `hasMore:false`) | g-mesh `find_callers(resolve_module_path)` on project `g-mesh` |
| Who emits `IMPORTS` | `Declarer::import_edge` (`plugins/rust/src/extractor/decls.rs:827`) -> `Emitter::import_edge` (`emit.rs:300`) | g-mesh `find_callers(import_edge)`: returned 6 same-named candidates (`ambiguous`); I took the Rust plugin's two from the candidate list and read them, no re-query |
| Does core parse container keys | no: `own_module_files` matches `qualifiedName = key`; `graph::imports` and `cli::deps::fallback_targets` look keys up | g-mesh `find_definition(own_module_files)`; grep `containers::` in `cli/deps.rs` and `graph/imports.rs` (one known file each) |
| What `deps --json` reads | the index DB (edges + `containers`), after a daemon readiness wait; the `module` field is the container key | `core/src/cli/deps.rs` module doc and `fallback_targets` |
| Does the semantic tier read keys | no: `semantic.rs` only finds rust-analyzer for the SDK's LSP bridge; no `ProjectContext`/`container` use | grep in that one file |
| Facts / incremental deltas | `facts::delta`: crate-set change -> `Unknown`; else SDK `container_delta` with separator `::` | reading `project/facts.rs` |

g-mesh note: the first two calls without `project` were refused ("more than
one project was selected on this connection (g-mesh, then g-mesh-wt-gm453)");
passing `project: "g-mesh"` fixed it.

## 7. Must confirm (implement / verify slices)

1. `module_tree::scan_crate` and its notes treat `tool[bin]` as an opaque
   `crate_key` (it joins segments with `::`; nothing splits it). Read
   `module_tree.rs:168-260` before the edit.
2. No other reader of `Crate::key` assumes identifier shape: `census.rs`
   (test-only) and `RustFacts::of` are the ones grep shows; recheck with
   g-mesh `find_references(Crate)` / `find_references(crates)` in the
   worktree.
3. `cli_deps.rs`'s `FILES` count, coverage numbers (`rust.external == 1`,
   `unresolved` totals) still hold after adding `src/lib.rs` and
   `src/config/inner.rs`; adjust the counts, not the meaning.
4. The visibility walk: a `pub(crate)` item in `config.rs` referenced from
   `main.rs` links (parent chain `tool[bin]::config` -> `tool[bin]`); worth one
   extractor-level assertion in B2's test.
5. The old collision note text is not asserted anywhere else
   (`grep -rn "collides with an earlier crate"`).

## 8. Decisions for the owner

- **Q1, key spelling**: `tool[bin]` (recommended: not an identifier, no `::`,
  sorts beside the lib, legible) or `bin:tool` (mirrors `orphan:`; reads like
  "not modelled").
- **Q2, `lib.rs` create/delete cost**: accept `Unknown` (full re-extract) on
  the bin's re-key (recommended: rare, already the case for creation), or keep
  `Affected` by filtering non-identifier keys out of `RustFacts.crates` (they
  can never match a bare segment, so the set is the same and the moved files
  are selected by `container_delta`). The filter is cheap but is a second,
  subtler change to pin with its own test and control.
- **Q3, scope of the `[bin]` rule**: any bin whose key is taken (recommended:
  also fixes a workspace where member A's lib and member B's bin share a name),
  or only a bin colliding with its own package's lib.
