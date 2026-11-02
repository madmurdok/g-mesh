# GM-691: impl blocks nested in bodies of cfg alternatives split into `#2`

Status: design (GM-691/S1). Follows GM-689 (`GM-689-rust-fn-local-cfg-alternatives.md`,
Option A) and GM-688 (members of an ordinal block are qualified by its
assigned name). Source: GM-688/S2 review, Finding 1.

## 1. Background

`Bodies::declare_impl_block` (`plugins/rust/src/extractor/bodies.rs:1678-1703`)
names a Decision-8 impl block (an `impl Tr for T` whose `T` names no
declaration) `<T as Tr>`, qualified by the module and the enclosing fns
(`f::<K as Tr>`). GM-689 made the map `impl_block_names` decide "same block":
key = (unsuffixed qualified name, **scope**), where scope is
`impl_block_scope(item)` = the start byte of the node holding the block
(`bodies.rs:1844-1848`). Same key -> the name already assigned (merge); new
key whose name is taken -> next `#N`.

The start byte works for siblings (cfg alternatives of *one* item sit in one
parent node). It fails one level down: when two cfg alternatives of an item
merge (one `f`, one `<K as Tr>`), each alternative still has its *own body*,
at its own byte offset. A block nested in that body gets the same qualified
name in both alternatives but a different scope byte, so the second becomes
`#2`. Everything nested under it then inherits the `#2` (GM-688 carries the
assigned name down), so the split cascades.

## 2. Today (measured)

Branch base `release-4.24.0` (`b4f96e1`), throwaway probe test in
`plugins/rust/src/extractor/tests/` (removed; source kept at the scratchpad as
`gm691_probe.rs`), `cargo test -p g-mesh-plugin-rust --lib gm691_probe -- --nocapture`.
Every fixture is prefixed with `pub trait Tr { fn x(&self); }\npub trait U {}\n`.
Output: impl block nodes as (qualifiedName, 0-based line).

| # | fixture (abridged) | today | wanted |
|---|---|---|---|
| P1 | `fn g() { struct L; #[cfg(a)] impl Tr for L { fn x(&self) { struct M; impl U for M {} } }` + `#[cfg(not(a))]` twin `}` | `g::<L as Tr>` 4, `g::<L as Tr>::x::<M as U>` 4, `g::<L as Tr>::x::<M as U>#2` 5 | no `#2` |
| P2 | `#[cfg(a)] pub fn f() { struct M; impl U for M {} }` + `#[cfg(not(a))]` twin | `f::<M as U>` 2, `f::<M as U>#2` 3 | no `#2` |
| P3 | `fn g() { #[cfg(a)] fn h() { struct M; impl U for M {} } #[cfg(not(a))] fn h() {..same..} }` | `g::h::<M as U>`, `g::h::<M as U>#2` | no `#2` |
| P4 | cfg twins `f() { { struct M; impl U for M {} } }` (impl in a nested block) | `f::<M as U>`, `#2` | no `#2` |
| P7 | cfg twins `f() { {impl U for M} {impl U for M} }` | `f::<M as U>`, `#2`, `#3`, `#4` | `f::<M as U>`, `#2` |
| P8 | cfg twins `f`, alt a has `let _a = 1;` before the impl | `f::<M as U>`, `#2` | no `#2` |
| P9 | cfg twins `f() { let _c = \|\| { struct M; impl U for M {} }; }` | `f::<M as U>`, `#2` | no `#2` |
| P10 | three deep: cfg twins `impl Tr for K { fn x { impl Tr for M { fn x { impl U for N {} } } } }` in `f` | `f::<K as Tr>`, `f::<K as Tr>::x::<M as Tr>`, `...::<M as Tr>#2`, `...::<M as Tr>::x::<N as U>`, `...::<M as Tr>#2::x::<N as U>` | three nodes, no `#` |
| P12 | cfg twins `f`, alt a has an extra `{ let _ = 1; }` before a body-level impl | `f::<M as U>`, `#2` | no `#2` |
| P5 | KEEP: `fn g() { { fn h() { impl U for M {} } } { fn h() { impl U for M {} } } }` | `g::h::<M as U>`, `#2` | same |
| P6 | KEEP: two `const _: () = { fn h() { impl U for M {} } };` | `h::<M as U>`, `#2` | same |
| P11 | KEEP (GM-682 F2): `fn f() { {impl U for M} {impl U for M} }` | `f::<M as U>`, `#2` | same |
| P13 | KEEP: `if c { impl U for M {} } else { impl U for M {} }` | `f::<M as U>`, `#2` | same |
| P15 | KEEP: `const C: () = { impl U for M {} }; const D: () = {..};` | `<M as U>`, `#2` | same |
| P14 | cfg twins of a named `const C: () = { impl U for M {} };` | `<M as U>`, `#2` | see §5 residuals |
| P16 | cfg twins `f`, alt a has an extra `{}` before a *nested-block* impl | `f::<M as U>`, `#2` | see §5 residuals |

P1 is the review's fixture; P2 the top-level case from the task. P5, P6, P11,
P13, P15 are genuinely different anonymous scopes (or different named items
whose names the qualified name does not carry) and must keep their `#N`.

## 3. What the key must express

The qualified name already carries every *named* level: module path,
enclosing fns (`enclosing_fns`), and enclosing impl blocks by their assigned
name (GM-688). Two blocks with equal qualified names therefore differ only in
**which anonymous scope** (`{ }`, `const _` body, closure body, branch) they
sit in at some level. The start byte answers "which node" - too strong: it
also distinguishes the bodies of two merged alternatives. The key should
answer "which anonymous scope *within its named owner*", so that alternatives
of one owner (same name, so one node) map their bodies to one scope, while
two scopes inside one owner stay apart.

## 4. Options

### Option A (recommended): block-ordinal path within each owner

Scope = for every `block` ancestor of the impl item (innermost first), its
ordinal among the `block`s owned by its nearest owner, in source order. Owner
= nearest `function_item`, `mod_item` or `source_file` ancestor; counting does
not descend into nested owners (their blocks are counted in their own frame).
A fn's body is its owner's first block, so a block directly in a body has
ordinal 0 at that level whichever alternative it is in.

Examples (key shown outermost-last, as computed):
- P1: nested `impl U for M` in `x` of either alternative -> `[0 (x body in x), 0 (g body in g)]`, name `g::<L as Tr>::x::<M as U>` -> one node.
- P2: `[0]` in both `f` -> one node. P8/P12: still `[0]` (statements and blocks *before* the impl do not change the impl's own block ordinal) -> one node.
- P7: alt a gives `[1,0]`, `[2,0]`; alt b the same -> exactly `f::<M as U>` and `#2`.
- P5: `{ fn h }` blocks are g's blocks 1 and 2 -> `[0,1,0]` vs `[0,2,0]` -> `#2` kept.
- P11/GM-682 F2: `[1,0]`, `[2,0]`, `[3,0]` -> `#2`, `#3` kept. P6/P15: module blocks 0 and 1 -> `#2` kept. P13: then/else are blocks 1 and 2 -> `#2` kept.

Consequences:
- Pure function of the tree, like today's `impl_block_scope`: no walk state,
  so `declare_impl_block` and `assigned_impl_block_name` (which must agree)
  keep calling one helper. Only the key type changes (`usize` -> `Vec<u32>`).
- Cost: one preorder count over the owner's subtree up to the block, per
  `block` ancestor, per call. Module-level impls have no `block` ancestor
  (zero cost); fn-local ones count within their (small) fn. Calls happen once
  per Decision-8 block and once per member fn of an impl in a file whose map
  is non-empty. If a profile ever shows it, memoise per owner.
- Alternatives pair by structure: block *k* of alt a merges with block *k*
  of alt b if names match. If alternatives differ in *how many blocks
  precede* a nested-block impl (P16), the ordinals differ and it still
  splits - no worse than today, documented.
- Prototype (throwaway, reverted; diff kept at the scratchpad as
  `gm691_prototype.diff`, ~50 lines in `bodies.rs`): P1-P4, P7-P10, P12 merge
  as wanted; P5, P6, P11, P13, P15 keep `#2`; all 9 `fn_local_impls` tests pass
  unchanged.

### Option B: start byte relative to the nearest enclosing fn

Scope = `parent.start_byte() - owner_fn.start_byte()`, plus the same for each
outer owner. Fixes P1-P3 only when the alternatives are byte-identical up to
the impl; P8 (`let _a = 1;` in one alternative) and P12 still split, which is
the usual case (alternatives differ, that is why they exist). Fragile; rejected.

### Option C: "fn body" sentinel, byte offset elsewhere

Scope = a constant when the impl's parent is a fn body (the fn path is
already in the name), else the start byte. Fixes P1-P3, P8, P12 with a
two-line change. But P5 regresses: both inner `h` bodies get the sentinel and
`g::h::<M as U>#2` merges into `g::h::<M as U>` (two real blocks, one node,
one edge lost). P4, P7, P9 still split. Rejected for the regression.

### Option D: document the split

Keep today's behaviour and record it as a known limitation (GM-689's
"one path, one node" holds only for the merged item itself, not for what its
body declares). Zero risk, but P10 shows the split cascades into every
nested name below, and the acceptance criterion prefers one node.

## 5. Recommendation

**Option A.** It states the rule GM-689 §3 already gave ("within one scope a
repeated block is the same block; another scope gets the next ordinal") with
"scope" defined relative to named owners instead of byte positions, so the
merge of the owner carries down to its body. It keeps every `#N` the KEEP
fixtures need, changes one helper and one field type, and was prototyped
green against the existing suite.

Residuals (documented, not fixed):
- **P14, named const/static alternatives**: owners are fns and modules only.
  A `const C`'s initializer block is a module-level block; its twin is the
  next one -> `#2`. Making named consts owners would need their name in the
  key (the qualified name does not carry `C`; P15 must keep `#2`). Rare;
  leave as is.
- **P16, differing block structure before a nested-block impl**: ordinals
  shift, split stays as today.
- **cfg on block expressions** (`#[cfg(a)] { .. } #[cfg(not(a))] { .. }`):
  two blocks, two ordinals, `#2` as today - an anonymous scope has no name
  to merge by.

## 6. Edit map

Change:

| function | file:lines | change |
|---|---|---|
| `struct Bodies`, field `impl_block_names` | `plugins/rust/src/extractor/bodies.rs:164-167` | key type `(String, usize)` -> `(String, Vec<u32>)`; doc: "scope = block-ordinal path within each owner" |
| `impl_block_scope` | `plugins/rust/src/extractor/bodies.rs:1844-1848` | return `Vec<u32>`: for each `block` ancestor, its ordinal within its owner (Option A); doc comment rewritten |
| new helpers `is_scope_owner`, `block_ordinal`, `count_blocks_before` | after `impl_block_scope` | owner = `function_item` / `mod_item` / `source_file`; preorder count of `block`s before the target, not descending into nested owners, stopping at the target |
| doc of `declare_impl_block` | `plugins/rust/src/extractor/bodies.rs:1646-1677` | "one scope" paragraph: scope is the anonymous scope within the named owner, so bodies of merged alternatives are one scope |
| module doc of the tests | `plugins/rust/src/extractor/tests/fn_local_impls.rs:1-11` | add: blocks nested in merged alternatives are one block |

No change: `declare_impl_block` body (`1678-1703`) and
`assigned_impl_block_name` (`1724-1731`) only pass the key through;
`RustExtractor::extract` (`mod.rs:217`) initialises with `Default::default()`,
which follows the new type.

Read (context): `Bodies::function` (`bodies.rs:456-487`, the
`enclosing_fns` push and the GM-688 assigned-name lookup),
`Bodies::impl_item` (`bodies.rs:371-420`, declares before walking members),
`Bodies::supertype_edge` (`bodies.rs:1573-1644`, the only caller of
`declare_impl_block`), `Bodies::fn_local_path` (`bodies.rs:1735-1746`),
`Emitter::declare` / `is_declared` (`emit.rs:181-198`).

## 7. Behaviours the tests must pin

In `plugins/rust/src/extractor/tests/fn_local_impls.rs`:

1. Block nested in a method of merged cfg alternatives is one node (P1, and P10's three-deep chain: no `#` anywhere).
2. Block nested in the body of top-level cfg alternatives of a fn is one node (P2), also when the alternatives differ before it (P8).
3. Block in a nested `{ }` / closure of merged alternatives is one node per position: P7 gives exactly `f::<M as U>` and `#2`; P9 one node.
4. Same-named fns in two different blocks of one fn keep `#2` for what they declare (P5), as do two `const _` (P6) and if/else branches (P13).
5. Existing `a_colliding_impl_block_is_suffixed_and_keeps_its_edge` and the other `fn_local_impls` tests pass unchanged.

Controls (all in `bodies.rs`, one test target `--lib fn_local_impls`):
- C1 (pins 1-3): restore `impl_block_scope` to `parent.start_byte()` (wrapped as `vec![..]`) - tests 1-3 fail.
- C2 (pins 4): ordinals collapsed to depth (`block_ordinal` returns 0) - P5/P11/P13 merge, tests 4 and 5 fail.

## Appendix: g-mesh calls behind the edit map

Project `g-mesh` (main checkout on `release-4.24.0`; `bodies.rs` and
`mod.rs` verified byte-identical to the worktree by `diff -q`). The index
reported `provenance.semanticTier: absent` (receiver calls `self.x()` may be
missing), so one grep in `plugins/rust/src` cross-checked the same symbols;
it found the same sites and nothing more.

- `find_callers(Bodies::declare_impl_block)` -> only `Bodies::supertype_edge` (`bodies.rs`), `hasMore: false`.
- `find_callers(impl_block_scope)` -> `Bodies::declare_impl_block`, `Bodies::assigned_impl_block_name`; `files`: `bodies.rs` (2).
- `find_callers(Bodies::assigned_impl_block_name)` -> only `Bodies::function`.
- `find_callers(Bodies::supertype_edge)` -> only `Bodies::impl_item`.
- `find_references(impl_block_names)` (the byte-offset key's map; `Bodies::impl_block_names` was not found by that name, the bare name resolved to `Bodies.impl_block_names`) -> `declare_impl_block`, `assigned_impl_block_name` (`bodies.rs`), `<RustExtractor as Extractor>::extract` (`mod.rs`, initialiser).

Files a key change touches: `plugins/rust/src/extractor/bodies.rs` (key,
helpers, docs); `mod.rs` needs no edit (`Default::default()`); tests in
`plugins/rust/src/extractor/tests/fn_local_impls.rs`.

Probe timing: `real 154.96 user 97.39 sys 19.34` (first build into a cloned
target); prototype run `real 28.34 user 11.45`, load average 144 at start.
