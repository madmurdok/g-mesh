# GM-470: a type member and a same-named free fn in one module

Diagnosis for GM-470/S1, on `release-3.19.0` at `981a9ec` (after GM-469,
GM-472, GM-474 and GM-477). Structural tier only: the bulk walk runs each real
plugin binary, as `core/src/mcp/unlinked_tests.rs` does. The semantic tiers
(rust-analyzer, gopls, pyright) were not exercised.

## Verdict

**Reproduced, but on the other side.** The member's `find_references` is
complete. The **free fn** loses its cross-module uses: a name-keyed
placeholder (`use m::y`, `m::y()`, Go `m.Y()`, Python `from a import y` / `y()`)
finds two candidates, the free fn and the member, and the linker drops the
edge. It is no longer silent: since GM-477 the free fn's page carries
`unlinkedUsages`.

## Fixtures

The diagnosis used an uncommitted probe that printed every
`find_references` (by `symbol_id`) and every `pending_symbol` that still had
incoming edges. Its fixtures became the assertions in
`core/src/mcp/member_name_collision_tests.rs` (real plugins) and the
"type member and a free declaration" section of
`core/src/graph/symbol_links/tests.rs` (the linker alone).

- **Rust** (`src/m.rs`, `src/user.rs`): (a) `S.x` field + `fn x`; (b)
  `T::y` inherent method + `fn y`; (c) `U.z` field + `U::z` method; (d)
  `user.rs` has `use crate::m::{x, y, S, T, U}; use crate::m;` and calls
  `x() y() m::x() m::y() T::y(t) U::z(u) m::T::y(t)` and `s.x`.
- **Python** (`pkg/a.py`, `pkg/b.py`): class var `S.x` + `def x`; method
  `T.y` + `def y`; `b.py` has `from pkg.a import x, y, T, S`, `from pkg import a`
  and calls `x() y() a.x() a.y() T.y(t) a.T.y(t)`, and reads `S.x`.
- **Go** (`m/m.go`, `user/user.go`): field `S.X` + `func X`; method `T.Y` +
  `func Y`; `user.go` calls `m.X() m.Y() t.Y() m.T.Y(t)` and reads `s.X`.
- **TypeScript** (`src/a.ts`, `src/b.ts`): class field `S.x` + `function x`;
  method `T#y` + `function y`; named and namespace imports in `b.ts`.
- Case (c) applies only to Rust. TS, Python and Go have no field and method
  of the same name.
- **Control** (`gm470_control_no_collision`): the same fixtures with the
  method renamed (`y` to `w`, `Y` to `W`).

## Results (structural tier)

| Plugin | Declaration | Rows | `unlinkedUsages` | Unlinked placeholders |
|---|---|---|---|---|
| rust | field `m::S.x` (a) | 2 (`S::get_x`, `local`), all same-file | - | - |
| rust | method `m::T::y` (b) | 2 incl. `user::use_members` | - | - |
| rust | field `m::U.z` (c) | 2 (`U::z`, `U::both`) | - | - |
| rust | method `m::U::z` (c) | 2 incl. `user::use_members` | - | - |
| rust | fn `m::x` (a,d) | 3 CALLS incl. `user::use_free` | count 1 | `use crate::m::x` REFERENCES (name `x` in `krate::m`) |
| rust | fn `m::y` (b,d) | **0** | count 2 | `use_free` CALLS and the `use` REFERENCES (name `y` in `krate::m`) |
| python | method `T.y` | 2 incl. `use_members` | - | `a.T.y(t)`: name `y` in container `pkg.a.T` (*) |
| python | fn `x` | 3 CALLS + import REFERENCES | - | - |
| python | fn `y` | 2, same-file only | count 3 | `use_free` CALLS, import REFERENCES |
| go | method `T.Y` | 0 (receiver calls have no structural edge) | - | - |
| go | func `X` | 3 incl. `UseFree` | - | - |
| go | func `Y` | 2, same-file only | count 1 | `UseFree` CALLS (name `Y` in `example.com/probe/m`) |
| ts | `x`, `y`, `T#y` | all uses linked | - | none |

In every row, `resolved` is true and no response carried `allUnresolved` or
`hasMore`.

**Control:** with the method renamed, `m::y` (Rust), `Y` (Go) and `y` (Python)
gain the missing `use_free`/`UseFree` CALLS row and the import REFERENCES row,
and their `unlinkedUsages` disappears. The Rust control still leaves
`use crate::m::x` unlinked, because field `S.x` remains in the fixture. That
is case (a) on its own: a field collides only on REFERENCES, since CALLS
requires a `Function`.

Where each plugin stands:

- **Not affected:** TS. Its members are `file`-visible and `#`-qualified, so a
  file-scoped name key never sees them.
- **No case (a):** Go emits no field nodes, and Python emits no class-var
  nodes. The Python `S.x` placeholder (`qualifiedName` key) stays unlinked for
  that reason, which is a separate gap.

(*) This is a separate Python issue, not this collision. The member's
container is `pkg.a`, but the `a.T.y` placeholder addresses container
`pkg.a.T`. It stays unlinked in the control too.

## Cause

Members are stored in the **module's** container (`container = krate::m`,
`example.com/probe/m`, `pkg.a`), next to the free fns. The name lookups
match on `name` alone:

- `core/src/graph/symbol_links.rs:1258` `in_file_by_name` and `:1262-1264`
  `in_container_by_name` (`... AND name = ?3`) return the free fn and every
  same-named member.
- `core/src/graph/symbol_links.rs:1175-1182` (`link`) then takes several
  fitting candidates as ambiguity and `continue`s: "a missing edge beats a
  wrong one". CALLS is lost when the member is a method (a `Function`).
  REFERENCES is lost for any member, because its required kind is `None`.

The edges that address members are fine. Rust member uses are emitted with
`qualifiedName` keys (`plugins/rust/src/extractor/bodies.rs:944-955`
`tail_in`), and the rest are bound in the same file.

## Recommendation: fix it, in core, as a tie-break

"qualifiedName-first" as the task phrases it does not apply. The dropped
placeholders are name-keyed because the plugin knows only a module and a name.
Re-keying them by `qualifiedName` in each plugin would also stop the
re-export hops, since `walk` follows only `Key::Name`
(`symbol_links.rs:1340-1342`). That would break `pub use` chains, a much
larger risk.

Proposed fix, in `link()`, at the ambiguity branch only:

- When several candidates fit a `Key::Name` placeholder and **exactly one is
  not a type member**, link to that one.
- "Type member" means the candidate's `qualifiedPath` parent (its
  qualifiedName minus the last segment, available as segments since GM-474)
  is the qualifiedName of a `Type` node in the same file and container.
- Rationale: a name lookup in a module scope can never denote an associated
  item in Rust, Go or Python.

- **Benefit:** fixes cases (a) and (b) for Rust, Go and Python. It needs no
  plugin or wire change.
- **Cost:** one small indexed lookup per ambiguous placeholder, and nothing
  on the unambiguous path.
- **Risk:** low.
  - Only edges that are dropped today can change. An edge that links now
    already had one candidate and never reaches this code.
  - The residual risk is a language where a module-level name really does
    reach a member (none among the shipped plugins), or a plugin that emits a
    free fn whose qualifiedName parent is a type.
- **Tests:** this probe's fixtures, turned into assertions. Control: revert
  the tie-break, and the free fn's cross-module rows disappear (as measured
  above). The `in_file_by_name` path (TS) needs a no-change check.

**As implemented** (`Resolver::sole_non_member`, `Resolver::is_type_member`):
the enclosing `Type` is looked up in the candidate's **container** (same
language and container key), and in its file only when it has no container.
A Go method may be declared in another file of its receiver type's package;
a same-file lookup would leave those collisions dropped. For Rust and Python
a container is one file, so the two lookups agree there.

A member is also recognised through an **alias path**. A Rust trait-impl
method is `m::<S as Tr>::y`; its parent is no type, but the plugin already
sends the alias `m::S::y`, stored as a `qualified_suffixes` row. Core takes
that row minus the candidate's own last separator and name and checks it
for a `Type`, so it never parses `<S as Tr>`. Without this, an inherent `S::y`
and a trait-impl `y` beside a re-exported `y` sent `m::y()` to the trait-impl
method.

Not covered, and out of scope here: the Rust plugin binds a **same-file**
bare call (`y()` inside `m.rs`) itself (`extractor::model::lookup_name`),
with the same "several fit, drop it" rule. With a method `T::y` beside
`fn y`, that edge never reaches core.

The alternative is to document it as a known limitation. That costs nothing,
and `unlinkedUsages` already flags the page as not exact. But the free fn,
the more common target, keeps losing every cross-module use.

## g-mesh calls used

- `find_callers("graph::symbol_links::link_all")`: one caller,
  `storage::index_store::IndexStore::link_all`. `find_callers(... link_diff)`:
  one caller, `storage::index_store::apply_and_link`. These are the two entry
  points. Both go through `link` and `Resolver`.
- `find_callers("Resolver::declared")`: `Resolver::walk` and
  `Resolver::through_head`. This is where `Key::Name` and `Key::QualifiedName`
  split into the four lookups.
- `get_file_outline("core/src/graph/symbol_links.rs")`: located `link`,
  `Resolver::new` and `declared`.
- `find_references("TargetKey::QualifiedName")` failed with "no symbol named":
  enum variants are not nodes. `find_references("TargetKey")` then gave the
  emitters' files: `plugins/sdk/src/graph.rs` and `plugins/sdk/src/lsp/bridge.rs`
  (the semantic tiers), plus core. The Rust, Python, Go and TS plugins don't
  import it by that name, so grep found them:
  - Rust: `bodies.rs:952`, structural, qualifiedName keys.
  - Python: `bodies.rs:719`, structural.
  - Go: `semantic.go`, semantic tier only.
  - TS: never; `extract.ts:61-65` says it uses name keys only.
