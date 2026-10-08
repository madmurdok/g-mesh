# GM-533: Python, a later import rebinds a def and an earlier named import

Follow-up to GM-496 (`gm-496-python-later-import-binds.md`, must-confirm 3
and 4). Line numbers are at `4b6bd27` (release-4.2.0 base); GM-531 and GM-530
land first, so re-anchor by function name.

## Today

In one Python module, each `def`, `class`, assignment and import statement
rebinds a module-level name; the last one executed wins. After GM-496 g-mesh
follows that between a named import and a star import of `pkg/__init__.py`
(re-export rows ordered by start position, `Resolver::later_binding`). Two
cases still differ:

```python
# pkg/__init__.py                        Python        g-mesh today
def f(): ...                         #   ->  b.f     ->  pkg.f (the def)
from .b import *    # b defines f

from .a import g                     #   ->  c.g     ->  a.g (first binding)
from .c import g
__all__ = ["g"]
```

1. **Def, then star.** `Resolver::walk_capped` returns as soon as a frontier
   step has visible declarations (depth 0), before it looks at any hop. "A
   declaration beats any re-export" is shared by all languages.
2. **Named, then named.** `FileModel::import` (Python plugin) keeps the first
   binding of a name; the `__all__` row and every bare use then point at
   `a.g`.

## Change

### R1. Linker: a later binding hop beats an earlier Python declaration

In `walk_capped`'s candidate loop, for a frontier step `(scope, Name(n))`
whose visible candidates include one of a `later_import_binds` language (gate
on `candidate.language`, so Rust/TS steps never even query hops), and
`depth < cap`:

1. Compute the step's binding hops exactly as the expansion half of the loop
   does today (named-shadows-glob filter, `restricted_to` filter, then
   `later_binding` with `cap - depth - 1`). Extract that block into
   `Resolver::binding_hops(scope, name, requester, cap_left) -> Vec<Hop>` and
   call it from both places, so the two halves cannot drift.
2. If it returns exactly one hop (an ordered winner), and that hop's file is
   the file of the step's latest same-file candidate, and its position is
   **after** that candidate's start: the step is **rebound**. Its candidates
   are dropped and the hop's `to` goes into `next` like any hop, so the
   reported depth is the hop's (through_head's `depth == 0` check stays
   right).
3. Otherwise the declaration wins, as today. That covers: a def **after** the
   star (def wins again), a star that does not provide `n` (`later_binding`
   already skips it), rows of other files, and declarations of another file
   in the same container (a submodule node `pkg.a` is a member of `pkg` but
   lives in `pkg/a.py`).

Candidate position: a new `Resolver::position_of(id) -> (i64, i64)` using
`conn.prepare_cached("SELECT startLine, startCol FROM nodes WHERE id = ?1")`,
not new `Candidate` fields: GM-530 already edits `Candidate` and the
CANDIDATE SQL in `Resolver::new`, and this path runs only for Python steps
that have both a declaration and hops.

**Switch: the existing `LinkRules::later_import_binds`, no new signal.**
The rule is the same Python fact GM-496 declared in `plugin.toml`
(`[plugin.reexports] later_import_binds = true`): "each binding statement
rebinds the name". A plugin-side signal is impossible here: the plugin
cannot know whether `b` provides `f` (that is a cross-file fact the linker
alone has). No manifest, `LinkRules` or `link_rules` change.

`link_diff`: no new trigger. Moving or adding the def is a declaration
upsert at `(pkg, f)`; the star row and providers are GM-491/GM-496 triggers
already.

### R2. Plugin: of two named imports, the later binds (unconditional only)

`FileModel::import` replaces an existing different binding, and its range,
when the new statement is **unconditional** (directly at module level). A
binding made inside a compound statement (`if`/`try`/`with`/`for`/...) never
replaces an existing one, as today. `Declarer` tracks a `conditional` depth
(incremented around `collect` for the compound kinds in `Declarer::statement`)
and passes it to `import` from `import_statement` and `imported_name`.

Why the guard: `try: from ._speedups import f / except ImportError: from ._pure
import f` is a common idiom; Python normally binds the first, and plain
"last wins" would move every such link to the fallback. With the guard it
stays as today (GM-496 D4 row unchanged).

### R3. Plugin: a later top-level named import displaces an earlier def

Today `def f` followed by `from .a import f` keeps the def everywhere:
`Bodies::resolve_bare` and `qualifier_of` ask `FileModel::lookup` before
`lookup_import`, and `Declarer::reexport_dunder_all` skips any name the file
declares. Python binds `a.f`.

Change: `FileModel` records, beside each module-level declaration, the start
of its statement (side map `decl_starts: HashMap<String, Position>`, latest
per name; `DeclRef` stays a value) and, beside each import binding, whether
its statement is unconditional (from R2's `conditional` depth). A new
`FileModel::module_binding(name) -> ModuleBinding` answers the module frame's
final binding:

- `Import(&Import)` when an unconditional named import of `name` starts after
  the latest module-level declaration (the import displaced it);
- `Decl(&DeclRef)` otherwise (a def after the import wins again; a
  conditional import never displaces, the same guard as R2);
- `DeclBeforeStar(&DeclRef)` for R4.

Users of it:

- `resolve_bare` and `qualifier_of`, for the **module** frame only (a
  function-local `f` is untouched): on `Import` they fall through to the
  existing import branch, so the use addresses `a.f`.
- `reexport_dunder_all`: skips a name only when the module binding is a
  `Decl`. On `Import` it emits the named row at the import's range.
- **Without `__all__`** (new, must-confirm 3): a displaced name still gets
  that one named row. Today a name without `__all__` gets no row, and
  `from pkg import f` from another module then lands on the def through
  `declared` (a wrong answer). With the row, R1 gives `a.f`, because a named
  row placed after the def binds even if it leads nowhere. No row is added
  for any name that was not displaced.

### R4. Plugin: an in-file use of a def that precedes a star goes to the linker

The plugin cannot know whether `b` provides `f`; only the linker can. So
when the module binding is `DeclBeforeStar` (a module-level declaration and,
after it, a non-external `*` import, with no displacing named import),
`resolve_bare` returns `Bound::There` addressed at the module's **own
container**: `container_target(module.key, Name(f), module.key)`, with
`looks_class` taken from the def's kind. That is the same address
`from pkg import f` uses elsewhere, so R1 decides: `b.f` if the star
provides `f`, otherwise the def (today's answer, now reached through the link
pass instead of a direct edge).

Star statements keep GM-496's textual order, including those inside a
branch: the linker has no conditionality for a row. Qualified uses
(`f.attr`) keep the local def: a `qualifiedName` key never follows re-export
rows (must-confirm 4).

Cost: every in-file use of a module-level def written before some star
import becomes a placeholder. Where the star provides nothing, the answer is
the same def. Where two branches provide `f`, the answer is now "no edge"
(ambiguous) instead of the def: a missing edge, never a wrong one.

## Position: file-level only (approved)

g-mesh does not model where a use sits relative to the bindings. Every use in
the module goes to the module's final binding (R2-R4), and the linker answers
per module. Python agrees for every use inside a function, which runs after
the module has loaded. It disagrees only for module-level code written
between two bindings (`x = g()` between the two imports calls `a.g`).

## Edit map

| Function | File:line (`4b6bd27`) | Change |
|---|---|---|
| `Resolver::walk_capped` | `core/src/graph/symbol_links.rs` 1504-1577 | R1 rebinding check in the candidate loop; hop block replaced by `binding_hops` |
| `Resolver::binding_hops` (new) | same file, after `walk_capped` | extracted hop filter + `later_binding` |
| `Resolver::position_of` (new) | same file, after `provides` (1608) | `prepare_cached` position lookup |
| `Resolver::later_binding`, `provides`, `hops` | 1586, 1608, 1797 | unchanged (reused) |
| `FileModel` struct, `declare`, `import` | `plugins/python/src/extractor/model.rs` 107, 132, 180-191 | `decl_starts`, unconditional flag, star positions; R2 replace rule |
| `FileModel::module_binding` + `ModuleBinding` (new) | same file | R3/R4 final binding |
| `Declarer` struct, `statement`, `import_statement`, `import_from_statement`, `imported_name` | `plugins/python/src/extractor/decls.rs` 148, 197, 391, 434, 524 | `conditional` depth; record star positions |
| `Declarer::reexport_dunder_all` | decls.rs 590 | skip only on `Decl`; one row for a displaced name without `__all__` |
| `Bodies::resolve_bare`, `qualifier_of` | `plugins/python/src/extractor/bodies.rs` 745, 859 | module frame asks `module_binding` |
| module docs | `decls.rs` `__all__` section, `model.rs` `import` doc, `bodies.rs` `resolve_bare` doc, `symbol_links.rs` `walk` doc | "later binding" wording |

Not touched: `Candidate`, `Resolver::new`, `declared`, `link`,
`sole_accessor_getter` (GM-530), anything GM-531 touches.

## Must confirm (owner)

Approved: R1, R2 with the top-level-only guard, file-level resolution, and
in-scope AC3/AC4.

1. R3's guard: a conditional import (`if TYPE_CHECKING: from .a import f`)
   does not displace an earlier def, the same rule as R2.
2. R4 converts in-file uses of every module-level def written before a star
   import into placeholders (cost above). Alternative: only for names the
   plugin cannot rule out, which is every name, so no narrower form exists.
3. R3 emits a named re-export row for a displaced name even without
   `__all__`. Without it, other modules keep the wrong def.
4. Gaps kept: a qualified use `f.attr` after def + later star stays on the
   def; star order ignores branches (GM-496 D4).
5. Pre-existing, unchanged: a later `from numpy import f` (external) after
   `from .b import *` leaves no named row, so the star still wins (Python:
   numpy's).

## Tests and controls

Linker unit tests (`core/src/graph/symbol_links/tests.rs`, positioned
fixtures like `gm496_python_diffs`):

- T1 Python: `def f` line 1, `*` line 2 providing `f` -> `from pkg import f`
  links to `b.f`.
- T2 Python: `*` line 1, `def f` line 2 -> the def.
- T3 Python: `def f`, then a `*` that does not provide `f` -> the def.
- T4 Rust and TypeScript: declaration line 1, glob/`export *` line 2
  providing the name -> the declaration.

Plugin tests (`model.rs`, `decls.rs`, `bodies.rs`, plus one end-to-end
Python fixture through the linker):

- T5: `from .a import g` then `from .c import g`, `__all__ = ["g"]` -> row to
  `c.g` at line 2; a bare `g()` in a function addresses `c.g`.
- T6: `try: from .a import g / except ImportError: from .c import g` -> `a.g`.
- T7: `def f` then `from .a import f`: a bare `f()` addresses `a.f`; with and
  without `__all__` a named row exists; `import` before `def` -> the def, no
  row.
- T8: `def f` then `from .b import *`: a bare `f()` is a placeholder at
  `pkg`/`f`; end to end it links to `b.f`, and to the def when `b` lacks `f`.

Controls (revert the code, the named test must fail; 8):

| # | Revert | Must fail |
|---|---|---|
| C1 | drop the R1 rebinding check in `walk_capped` | T1 |
| C2 | rebind without the position comparison | T2 |
| C3 | rebind without the language gate | T4 (Rust and TS: one build) |
| C4 | rebind on any later `*` row, skipping `provides` | T3 |
| C5 | `FileModel::import` back to first-wins | T5 |
| C6 | drop the `conditional` guard | T6 |
| C7 | `module_binding` ignores `decl_starts` (def always wins) | T7 |
| C8 | `resolve_bare` returns `Here` for `DeclBeforeStar` | T8 |

Rust/TS answers unchanged: besides T4, the verify slice runs the full
`-p g-mesh-core` and `-p g-mesh-python` suites. No measure slice is needed:
no Rust/TS code path is entered, since the rule is gated on the candidate's
language.
