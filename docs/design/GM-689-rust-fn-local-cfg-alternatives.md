# GM-689: Rust cfg alternatives of one impl block become two nodes

Status: design (GM-689/S1). Follows GM-682 (`GM-682-rust-missing-callers-impls.md`
§3.2, Decision B, §8 F2).

## 1. Background in two paragraphs

The Rust plugin indexes every `cfg` alternative of an item, because the
structural tier cannot know which `cfg` is active. Two alternatives with the
same Rust path are **one node**: the node id is
`node_id(file, kind, qualifiedName, nativeKind)`, and `Emitter::declare`
(`plugins/rust/src/extractor/emit.rs:180-187`) keeps the first and returns its
id for every later one ("Decision 6", module doc `emit.rs:5-33`). Edges written
from the second alternative land on the first node. `cfg` attributes are
never read; the merge falls out of "same path, same id".

An `impl Tr for T` block whose `T` names nothing this file can resolve (a
local struct inside a fn body, a tuple, a reference type) is itself declared
as a node ("Decision 8", `Bodies::declare_impl_block`,
`plugins/rust/src/extractor/bodies.rs:1662-1688`), named after the block
prefix `<T as Tr>`, qualified by the module and the enclosing fns
(`m::f::<T as Tr>`). GM-682/F2 added a collision loop there: when the name is
already declared in the file, the block is renamed `#2`, `#3`, ... so that two
real blocks in different anonymous scopes (two `const _: () = { .. };` items,
two `{ .. }` blocks in one fn) stop being merged. The loop cannot tell those
apart from two `cfg` alternatives of one block, so it splits those too.

## 2. Today (measured)

Run on the branch base (`release-4.23.0`, commit `db1074a`) with a temporary
probe test in `plugins/rust/src/extractor/tests/` (not committed; source kept
at the scratchpad as `gm689_probe.rs`), `cargo test -p g-mesh-plugin-rust --lib
gm689_probe -- --nocapture`.

### 2.1 Top-level items: one node per path

```rust
pub trait Tr { fn x(&self); }
#[cfg(a)] pub fn f() {}
#[cfg(not(a))] pub fn f() {}
#[cfg(a)] pub struct S;
#[cfg(not(a))] pub struct S;
#[cfg(a)] impl S { pub fn m() {} }
#[cfg(not(a))] impl S { pub fn m() {} }
#[cfg(a)] impl Tr for S { fn x(&self) {} }
#[cfg(not(a))] impl Tr for S { fn x(&self) {} }
#[cfg(a)] impl Tr for (u8, u8) { fn x(&self) {} }
#[cfg(not(a))] impl Tr for (u8, u8) { fn x(&self) {} }
```

Nodes (qualifiedName, nativeKind, 0-based line):

| node | kind | line |
|---|---|---|
| `f` | function | 1 |
| `S` | struct | 3 |
| `S::m` | method | 5 |
| `<S as Tr>::x` | trait_impl_method | 7 |
| `<(u8, u8) as Tr>::x` | trait_impl_method | 9 |
| `<(u8, u8) as Tr>` | impl | 9 |
| `<(u8, u8) as Tr>#2` | impl | 10 |

Every declaration-pass item merges (one `f`, one `S`, one `S::m`, one
`<(u8, u8) as Tr>::x`; the existing test
`two_cfg_alternatives_of_one_item_are_one_node_and_do_not_collide`,
`tests.rs:1841`, pins this for `fn` and `mod`). The one exception is the
Decision-8 block node: `<(u8, u8) as Tr>` and `<(u8, u8) as Tr>#2`, each with
its own `SUPERTYPE_OF -> Tr`. So the inconsistency is **not fn-local only**: it
hits every Decision-8 block, at module level as well, while the block's own
method `<(u8, u8) as Tr>::x` stays one node.

### 2.2 Fn-local

```rust
pub fn g() {
    #[cfg(a)] fn inner() {}
    #[cfg(not(a))] fn inner() {}
    struct L;
    #[cfg(a)] impl Tr for L { fn x(&self) { helper(); } }
    #[cfg(not(a))] impl Tr for L { fn x(&self) { helper(); } }
}
pub fn h() {
    struct L;
    impl Tr<u8> for L { fn x(&self) {} }
    impl Tr<u16> for L { fn x(&self) {} }
}
pub fn k() {
    { struct K; impl Tr for K { fn x(&self) { helper(); } } }
    { struct K; impl Tr for K { fn x(&self) {} } }
}
```

| node | line |
|---|---|
| `g::<L as Tr>` | 6 |
| `g::<L as Tr>#2` | 7 |
| `h::<L as Tr>` | 11 |
| `h::<L as Tr>#2` | 12 |
| `k::<K as Tr>` | 15 |
| `k::<K as Tr>#2` | 16 |

Each has `SUPERTYPE_OF -> Tr`. Other facts the design depends on:

- Fn-local `fn`s, structs and **methods of fn-local impl blocks are not
  nodes** (`decls::collect` walks only module and impl/trait item lists).
  `inner` has no node; the `helper()` calls inside `x` are `CALLS g -> helper`
  and `CALLS k -> helper`, from the enclosing module-level fn.
- `g` (cfg alternatives, one block in rustc's eyes) and `k` (two real blocks)
  produce the same shape. `h` (two real, legal impls whose prefixes match
  because `type_name` drops generic arguments) does too.

## 3. What decides "same block"

At top level the rule is "same path in the same file is one node". For a
Decision-8 block the path is module + enclosing fns + **the anonymous scope**
the block sits in; the `#N` ordinal exists only to stand in for that
anonymous scope (rustc's `{const#0}`, `{const#1}`), which has no name. Two
alternatives of one item are siblings: tree-sitter-rust puts `#[cfg(..)]` in
an `attribute_item` *beside* the `impl_item`, so both alternatives have the
same parent node (`source_file`, a `mod`'s `declaration_list`, or a fn's
`block`). Two blocks in different `const _` bodies or different `{ }` blocks
have different parents.

So the top-level rule, applied faithfully, is: **within one scope, a repeated
block name is the same block (merge); a name already taken by another scope
gets the next ordinal**.

## 4. Options

### Option A (recommended): ordinal per scope, merge within a scope

`declare_impl_block` remembers, per file, which name it gave each
`(base qualified name, scope)` pair, scope = the impl item's parent node
(its `start_byte`, unique in the file). On a hit it declares under the
remembered name, which `Emitter::declare` resolves to the first node; on a
miss it runs today's `#N` loop and records the result.

- Example: §2.1's tuple gives one `<(u8, u8) as Tr>` (line 9). §2.2 gives
  `g::<L as Tr>` (one node), `h::<L as Tr>` (one node), `k::<K as Tr>` and
  `k::<K as Tr>#2` (unchanged). A fn with two `{ }` blocks, each holding two
  cfg alternatives, gives exactly `f::<K as Tr>` and `f::<K as Tr>#2`.
- Benefit: one rule for module-level and fn-local blocks, the same rule every
  other item already follows; the F2 behaviour (different scopes stay
  apart) is kept. No `cfg` parsing. Small: one map, one lookup.
- Risk: `h` (`impl Tr<u8> for L` + `impl Tr<u16> for L`, two real impls in one
  scope) goes from two block nodes to one. That matches top level, where
  their methods are already merged into one `<L as Tr>::x` and the two
  `SUPERTYPE_OF` edges have the same target `Tr` anyway, so
  `find_implementations(Tr)` loses a duplicate row, not an implementor
  trait. Cfg alternatives in *different* scopes (`#[cfg(a)] const _ = {..}` /
  `#[cfg(not(a))] const _ = {..}`, or two cfg-gated `{ }` blocks) stay two
  nodes: they are two items in Rust too, and rustc numbers them apart.

### Option B: merge only siblings that both carry `#[cfg]`/`#[cfg_attr]`

A, plus a check that both alternatives have a `cfg` attribute; otherwise
take the ordinal.

- Example: `g` merges; `h` stays `h::<L as Tr>` and `h::<L as Tr>#2`.
- Benefit: keeps `h`'s two real impls as two rows.
- Risk: a second rule that nothing else in the plugin follows (top level
  never reads `cfg`), so the next "why are these two/one" question has two
  answers; `h`'s second row carries an artificial `#2` name with no way to
  tell which generic argument it is.

### Option C: leave the split, document it

- Example: unchanged (§2 tables).
- Benefit: no code change.
- Risk: one logical block is two implementor rows (`find_implementations`
  shows `g::<L as Tr>` and `g::<L as Tr>#2`), the `#2` swaps on reordering,
  and module-level Decision-8 blocks keep disagreeing with their own methods
  (one method node, two block nodes).

## 5. Recommendation

**Option A.** It is the top-level rule ("one path, one node") with the scope
made part of the path, it also fixes the module-level case §2.1 found, and its
one behaviour change beyond cfg (`h`) is the same merge the plugin already
performs for those impls' methods.

### What A implies for GM-688 (methods of a second impl block)

- Within one scope, a repeated block is the same block, so its members must
  take the **same** prefix as the first (today's behaviour for module-level
  methods, e.g. one `<(u8, u8) as Tr>::x`). GM-688 must not give members of a
  merged alternative a `#2` prefix.
- A block that got an ordinal (`#2`) is always in a different anonymous
  scope. Its members' prefix should be the block's **assigned** name
  (`<K as Tr>#2::x`), which only matters where those members show up: today
  fn-local methods are not nodes, but `Bodies::function` pushes
  `member_tail_path(block, name)` = `<K as Tr>::x` onto `enclosing_fns`
  (`bodies.rs:460-469`), so a block nested in that method is qualified as
  `f::<K as Tr>::x::<M as U>` for both scopes and collides into `#2` again.
  GM-688 should carry the assigned name (from A's map) into the `BlockCtx`
  or into that push, so nested names read `f::<K as Tr>#2::x::<M as U>`.
- Module-level `impl Tr<u8> for X` / `impl Tr<u16> for X` share member
  names because `type_name` drops generic arguments; A keeps that. Splitting
  them needs generic arguments in the prefix, a wider change than GM-688.

## 6. Edit map

Change:

| function | file:lines | change |
|---|---|---|
| `struct Bodies` | `plugins/rust/src/extractor/bodies.rs:152-162` | add `impl_block_names: HashMap<(String, usize), String>`: `(base qualified name, scope start byte) -> assigned name` |
| `Bodies::declare_impl_block` | `plugins/rust/src/extractor/bodies.rs:1662-1688` | scope = `item.parent().map(start_byte)`; key = base spec's qualified name; on a hit build the spec with the remembered name; on a miss run the existing `is_declared` loop, then insert |
| doc comment of `declare_impl_block` | `plugins/rust/src/extractor/bodies.rs:1636-1661` | replace the F2 paragraph: the ordinal names an anonymous scope; siblings in one scope are one block (cfg alternatives), as Decision 6 |
| `Bodies` construction in `RustExtractor::extract` | `plugins/rust/src/extractor/mod.rs:210-217` | initialise the new field |
| module doc | `plugins/rust/src/extractor/tests/fn_local_impls.rs:1-8` | add the merge rule |

Read (no change): `Emitter::declare`, `Emitter::is_declared`
(`emit.rs:180-198`), `Bodies::fn_local_path` (`bodies.rs:1692-1705`),
`Bodies::supertype_edge` (`bodies.rs:1563-1638`), `decls::impl_block`
(`decls.rs:257-285`), `Bodies::function` (`bodies.rs:450-477`).

Downstream identity: the node id stays `node_id(file, kind, qualifiedName,
nativeKind)` (`plugins/sdk/src/ids.rs:144`); a merged alternative returns the
existing id, exactly as every top-level cfg alternative already does, and
core upserts by id, so no edge or identity consumer changes. The block nodes
are never looked up by name from other files (Decision 8 records no open
site). No `cfg`-reading code exists in the extractor; `cfg` is only a census
context (`bodies.rs:258-288`). Changelog line: "Rust: `cfg` alternatives of
one impl block that names no declared type are one node again".

g-mesh calls behind this map (project `g-mesh-wt-gm689`):
`find_callers(declare_impl_block)` -> only `Bodies::supertype_edge`;
`find_callers(is_declared)` -> only `declare_impl_block`;
`find_callers(fn_local_path)` -> only `declare_impl_block`;
`find_references(ids::node_id, answer=files)` -> `plugins/rust/src/extractor/emit.rs`
(5) is the Rust plugin's only user; the rest are other plugins, the SDK and
tests. `Bodies {` construction (one site, `mod.rs:210`) and the `cfg`
attribute sites were found by grep in the one plugin.

## 7. Behaviours the tests must pin

In `plugins/rust/src/extractor/tests/fn_local_impls.rs`:

1. **Fn-local cfg alternatives are one node.** `g` of §2.2: exactly one
   `g::<L as Tr>` impl node (none with `#`), at the first alternative's line,
   one `SUPERTYPE_OF -> Tr`. Control: in `declare_impl_block`, skip the map
   lookup (always run the `#N` loop) - `g::<L as Tr>#2` reappears.
2. **Module-level Decision-8 cfg alternatives are one node**, agreeing with
   their method. §2.1's tuple: one `<(u8, u8) as Tr>`, one
   `<(u8, u8) as Tr>::x`. Same control as 1 (one revert, both tests fail).
3. **Different scopes still get ordinals, and cfg alternatives inside each
   scope merge.** `f() { { #[cfg(a)] impl Tr for K {} #[cfg(not(a))] impl Tr for K {} }
   { #[cfg(a)] impl Tr for K {} #[cfg(not(a))] impl Tr for K {} } }` (each block
   with its `struct K;`): exactly `f::<K as Tr>` and `f::<K as Tr>#2`. Control:
   key the map by the base name alone (drop the scope) - the second scope
   merges into the first and `#2` is missing. The existing F2 test
   `a_colliding_impl_block_is_suffixed_and_keeps_its_edge` must keep passing
   unchanged and fails under this control too.
4. **Same-prefix impls in one scope are one block** (Option A's chosen
   consequence): `h` of §2.2 gives one `h::<L as Tr>`. Covered by the control
   of 1.

Controls: two reverts (1 and 3), both in `bodies.rs`, both run by
`cargo test -p g-mesh-plugin-rust --lib fn_local_impls`.
