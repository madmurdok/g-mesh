# GM-496: in Python, the later import binds the name

Status: design, for review. Follow-up to GM-490 (ADR 0020) and GM-491
(`gm-491-link-diff-star-provider.md`, "Why reopen everything").

## Problem

`pkg/__init__.py`:

```python
from .a import f
from .b import *
__all__ = ["f"]
```

Python binds `pkg.f` to `pkg.b.f`: the star import runs later and rebinds
`f`. GM-490 gave Python "no winner" (named and `*` rows side by side at one
depth), so `link_all` and, since GM-491, `link_diff` in every arrival order
leave `user.py`'s call unresolved. Swap the two imports and Python binds
`pkg.a.f`; we still say nothing.

## What the real plugin emits today (read before deciding)

- A `*` row comes from `from .b import *`
  (`Declarer::import_from_statement`, `plugins/python/src/extractor/decls.rs`
  426-480). Its range is the import statement's.
- A **named** row comes only from `__all__`
  (`Declarer::reexport_dunder_all`, decls.rs 578-598). Its range is the
  **`__all__` statement's**, for every name in it, not the import's. So the
  node's start position does *not* give statement order today: `__all__` is
  usually written after both imports (or before both).
- `FileModel::import` (`model.rs` 172-176) keeps the first binding of a local
  name.
- `Emitter::reexport` (`emit.rs` 306-330) dedups by node id and keeps the
  first range: `from .b import *` written twice is one row at the first line.
- Without `__all__` there is no named row at all: the GM-490 shape needs
  `__all__ = ["f"]` (or the test fixture's hand-built rows).

## Decisions

### D1. Statement order: the re-export node's start position, made truthful by the plugin

**Recommendation.** Order rows by the node's own `(startLine, startCol)`,
compared only between rows of the **same file**. No schema, wire or store
change: `startLine`/`startCol` are already `NOT NULL` columns of `nodes`. The
linker's `REEXPORT` select (`Resolver::new`) adds `n.filePath, n.startLine,
n.startCol`; `reexports_in_file` and `reexports_in_container` keep their
`WHERE` clauses. Store/query module code does not change.

The Python plugin must make the position mean "where the name was bound":

1. A named `__all__` row takes the range of the **import statement that bound
   the name**, not `__all__`'s. `FileModel` keeps a side map
   `import_ranges: HashMap<String, Range>` beside `imports` (not a field on
   `Import::Item`, which `model.rs` tests compare by value).
2. A repeat of the *same* binding (`from .a import f` twice, same container
   and name) moves that range to the later statement. A repeat with a
   *different* target keeps the first, as today (`model.rs` doc: "first
   binding wins"; out of scope, see must-confirm 4).
3. A repeated `*` row (same node id) keeps the **latest** range:
   `Emitter::reexport` buffers re-export specs by id and flushes them in
   `Emitter::finish` before `graph.finish()`. Safe because a re-export node is
   the source or target of no edge (`reexport` adds none), so emitting it late
   cannot break "an edge may only name a node already emitted".

**Alternatives.**

- *New wire field / column (`ordinal`).* Explicit, but a wire, schema and
  store change for a fact the range already carries once the plugin fills it
  honestly. Rejected.
- *Order by the `__all__` range as is.* Wrong on the common layout above (the
  named row would always be "latest"). Rejected.

Side effect: a named Python re-export node now points at its import line,
not at `__all__`. Nothing reads re-export node ranges for answers
(`find_definition` never lands on a placeholder); see must-confirm 1.

### D2. How a language declares it: a second key in `[plugin.reexports]`

**Recommendation.** `ReexportRules` (`core/src/daemon/manifest.rs` 254-268)
gets `#[serde(default)] pub later_import_binds: bool`.
`plugins/python/plugin.toml` adds

```toml
[plugin.reexports]
# Each import statement rebinds the name: in one module, the later of a named
# import and a star import that provides the name binds it
# (docs/architecture/gm-496-python-later-import-binds.md).
later_import_binds = true
```

`read_manifest` refuses a manifest setting both keys true (they are opposite
answers to the same question). `link_rules` (manifest.rs 271-279) builds
`LinkRules` with both sets. `LinkRules` (`symbol_links.rs` 578-601) gets a
`later_import_binds: HashSet<String>`, a chainable builder
`with_later_import_binds(self, languages)` and the query
`later_import_binds(&self, language) -> bool`.
`with_named_shadows_glob` and every existing caller stay as they are, so the
Rust and TypeScript manifests and rules are untouched.

**Alternatives.** An enum `rule = "named_shadows_glob" | "later_binds"`
(cleaner, but changes the shipped Rust/TS manifests and ADR 0020's key);
hard-coding `"python"` in core (rejected: ADR 0020 keeps language rules in
manifests).

### D3. The rule

In `Resolver::walk`, per frontier step `(scope, name)`, after `hops(scope,
name)` and the existing `named_shadows_glob` block, when every hop of that
scope is of a `later_import_binds` language:

1. Drop the hops this requester may not follow (`restricted_to`), as the loop
   below already does: a row nobody here may follow neither wins nor hides.
2. If two of the remaining hops come from different files, or two share one
   position, keep them all (today's "no winner"). One Python module is one
   file, so this is only a guard.
3. Otherwise visit hops latest first:
   - a **named** row binds the name: it wins, even if it leads nowhere (an
     unresolvable target means "missing", never a wrong earlier answer; same
     principle as ADR 0020);
   - a `*` row wins only if it **provides** the name: a sub-walk from its
     `to` step, with the depth left (`MAX_REEXPORT_DEPTH - depth - 1`), finds
     at least one visible candidate. Two candidates still count as providing
     (the outer walk then reports the ambiguity itself);
   - a `*` row that does not provide is skipped, so it never hides an
     earlier named row or an earlier providing `*`.
4. Keep only the winner (or nothing). The frontier continues as today.

Implementation: `walk` becomes `walk_capped(scope, key, requester, cap)`
with `walk = walk_capped(.., MAX_REEXPORT_DEPTH)`; the probe is a new
`Resolver::provides(step, requester, cap) -> Result<bool>` calling
`walk_capped`, memoized in `HashMap<(Step, usize, Requester), bool>`
(`Requester` derives `Hash, Eq`). The probe applies the same rule
recursively. `through_head` calls `walk`, so a head reached through a Python
barrel gets the same rule.

Consequence beyond the AC (must-confirm 2): two star imports that both
provide `f` (`from .a import *; from .b import *`) now link to the later one
(`pkg.b.f`), where today they are ambiguous. That is Python's own answer and
the same rule; restricting it to named-vs-star would need an extra special
case.

Depth 0 is unchanged: a scope that declares the name itself still shadows
every re-export, whatever the order (must-confirm 3).

### D4. Edge cases

| Case | Answer |
|---|---|
| `from .a import f` twice around a `*` | D1.2 moves the range to the second: `a.f` wins if it is last. |
| `from .b import *` twice around a named import | D1.3 keeps the later range. |
| `if`/`try` branches (`try: from .fast import f` / `except: from .slow import *`) | The plugin indexes every branch (decls.rs `statement`); textual order decides. A documented approximation, like today's "conditional import is a documented gap". |
| `try: from .fast import f / except ImportError: from .slow import f` | Model keeps the first binding (one named row): unchanged. |
| `def f` and a later `from .b import *` in one module | Depth 0 wins: `pkg.f` stays the local `def` (Python: `b.f`). Out of scope, must-confirm 3. |
| `__all__` naming a local `def` | No row (`reexport_dunder_all` skips it): unchanged. |
| A later `from numpy import *` (external) | The plugin emits no `*` row for an external module: it cannot hide an earlier named row. Python would rebind if numpy has `f`; we cannot know. |
| Imports inside a function | Not module-level, no rows: unchanged. |
| Rust / TypeScript / Go scopes | Not `later_import_binds`: the new block never runs. |

### D5. `link_diff`

No new trigger is needed. GM-491 already re-decides every woken placeholder,
linked or not, and the answer above is a pure function of the stored rows,
their positions and the declarations they reach. Each of those reaches a
trigger:

- `__init__.py` re-sent (including a swap that only changes ranges): its `*`
  row seeds `(pkg, "*")`, which `waiting_placeholders` treats as the whole
  scope, so every placeholder addressed at `pkg` is re-decided.
- `a.py` or `b.py` arriving: a declaration seed, walked back up through the
  `*` and named rows by `republished_addresses` to `(pkg, f)` (GM-491's
  24-order test already exercises this path).

Deleting `f` from `b.py` (the star stops providing, `a.f` should win) is the
existing "declaration deleted" gap; `link_all` heals it. Must-confirm 5 asks
whether an edit that changes only node ranges reaches `link_diff` as an
upsert in the daemon path.

### D6. Proving Rust and TypeScript unchanged

A measure slice indexes the g-mesh repo twice, base `a525fde` (release-4.1.0
tip this branch was cut from) and the branch tip, and diffs the `edges` table
(`fromId, toId, kind, linkedFrom`) joined to the source node's language,
**split by language**: Rust and TypeScript must be byte-identical; Python's
delta is listed (expected: only edges that were unresolved or pointed at an
earlier import and now land on the later binding). Run the TS plugin build
first (`npm ci` in `plugins/typescript`): the main index shows TS failing to
spawn, and an empty TS arm proves nothing; the measure must show non-zero TS
edges in both arms as its control.

The base cache `SCRATCH/index-cache/dc7d160/` is **not** reusable: GM-497
(Rust field reads) and GM-511 (Python accessors) changed extractor output
since. Build a new base at `a525fde` into `SCRATCH/index-cache/a525fde/`.

## Edit map

Change (line numbers at `a525fde`):

| Function / item | File:lines | Change |
|---|---|---|
| module doc, re-export paragraph | `core/src/graph/symbol_links.rs` 120-131 | Replace "no winner" with D3's rule. |
| `LinkRules`, `with_named_shadows_glob`, `named_shadows_glob` | symbol_links.rs 575-601 | Add `later_import_binds` set, builder, query (D2). |
| `Resolver::new`, `REEXPORT` const | symbol_links.rs ~1370-1426 | Select `n.filePath, n.startLine, n.startCol`. |
| `Hop` | symbol_links.rs 1328-1342 | Add `later_import_binds: bool`, `position: (String, i64, i64)`. |
| `Requester` | symbol_links.rs 483-487 | Derive `Hash, PartialEq, Eq` for the memo. |
| `Resolver::walk` | symbol_links.rs 1447-1512 | Becomes `walk_capped`; insert D3 after the shadow block. |
| new `Resolver::later_binding`, `Resolver::provides` | symbol_links.rs, after `walk` | D3 steps 1-4; memo field on `Resolver`. |
| `Resolver::hops` | symbol_links.rs 1688-1767 | Read the three new columns; fill the new `Hop` fields. |
| `ReexportRules` | `core/src/daemon/manifest.rs` 254-268 | Add `later_import_binds`. |
| `link_rules` | manifest.rs 271-279 | Also `with_later_import_binds`. |
| `read_manifest` (builds `reexports`) | manifest.rs ~414 | Refuse both keys true. |
| `[plugin.reexports]` | `plugins/python/plugin.toml` | `later_import_binds = true`. |
| `FileModel::import`, new `import_range` | `plugins/python/src/extractor/model.rs` 166-176 | Side map; same-target repeat moves the range (D1.2). |
| `Declarer::imported_name` | `plugins/python/src/extractor/decls.rs` 516-545 | Pass `range` to `model.import`. |
| `Declarer::reexport_dunder_all` | decls.rs 578-598 | Use `model.import_range(published)`, not `__all__`'s range. |
| `Emitter::reexport`, `Emitter::finish` | `plugins/python/src/extractor/emit.rs` 306-330, 377-398 | Buffer re-exports by id, latest range wins, flush in `finish`. |
| decls.rs module doc `# __all__` | decls.rs 100-115 | State that the row carries the import's range and why. |
| ADR 0020 | `docs/adr/0020-named-reexport-shadows-glob.md` | Addendum: the second rule. |

Read for context: `Resolver::through_head` (1515), `Resolver::visible`
(1771), `seeds` (746), `waiting_placeholders` (893), `republished_addresses`
(954), `link_diff` (685).

Tests that change expectation (tests slice, not the code slice):
`bundled_rules` (tests.rs 9) adds Python's rule;
`gm490_a_python_explicit_import_never_shadows_a_later_star_import` (2780),
`gm491_a_late_star_import_provider_unlinks_the_named_answer` (2964) and
`gm491_link_diff_agrees_with_link_all_in_every_arrival_order` (2930) now
expect `pkg.b.f`. `gm490_python_diffs` (2688) must give the rows distinct
`start_line`s (`NodeRecord::new` sets 0) and take an order parameter.
`gm490_each_row_follows_its_own_languages_rule` (2818) runs with Rust-only
rules and still expects Python unresolved.

## Cross-file facts relied on (g-mesh, project `g-mesh`, branch unchanged)

- `find_callers Resolver::hops` -> only `Resolver::walk`.
- `find_callers Resolver::walk` -> `Resolver::resolve`, `Resolver::through_head`.
- `find_references LinkRules::named_shadows_glob` -> only `Resolver::hops`.
- `find_references LinkRules::with_named_shadows_glob` -> `daemon::manifest::link_rules`, tests `bundled_rules`, `gm490_an_index_store_links_under_its_own_rules`, `gm490_each_row_follows_its_own_languages_rule`.
- `find_callers daemon::manifest::link_rules` -> `cli::reindex::reindex`, `cli::init::init`, `daemon::run`, `cli::plugin_check::session::open_index`, `mcp::unlinked` test fixture, manifest test.
- `find_references ReexportRules` -> `RawPlugin`, `PluginManifest` only.

So `LinkRules` is built in exactly one production place (`link_rules`);
adding a field reaches every linker entry point.

## Behaviour list (tests slice) with controls

Linker (`core/src/graph/symbol_links/tests.rs`, rules = bundled + Python):

1. GM-490 fixture (named `f` at line 0, `*` at line 1): `link_all` links the
   call to `pkg.b.f`. Control: drop the D3 block in `walk` -> unresolved.
2. Same fixture, all 24 arrival orders of `link_diff`, compared to `link_all`
   after every step: equal, and the end state is `pkg.b.f`. Control: as 1
   (end state unresolved).
3. Swapped positions (`*` at 0, named at 1): `link_all` and every `link_diff`
   order give `pkg.a.f`. Control: compare positions ascending in
   `later_binding` -> `pkg.b.f`.
4. A later `*` that does not provide (`b` declares no `f`): `pkg.a.f`.
   Control: make `*` rows win without `provides` -> unresolved.
5. A later named row whose target has no declaration beats an earlier
   providing `*`: unresolved, not `pkg.b.f`. Control: make named rows probe
   like `*` rows -> `pkg.b.f`.
6. Two `*` rows both providing: the later wins. Control: as 1 -> unresolved.
7. A later `*` that provides only through a second `*` hop (depth 2): it
   wins. Control: run `provides` with cap 0 (declared-only) -> earlier named
   row wins.
8. Python rows with no `later_import_binds` rule (`LinkRules::default()`):
   unresolved, as GM-490. Control: make `later_import_binds()` return true
   for every language -> links.
9. `incremental` swap: link, then re-send `__init__`'s rows with swapped
   positions through `link_diff`: the edge moves `pkg.b.f` -> `pkg.a.f`.
   Control: remove the `REEXPORT_ALL_NAME` whole-scope branch in
   `waiting_placeholders` -> stays on `pkg.b.f`.
10. Existing Rust/TS GM-490/GM-491 tests pass unchanged (no edit).

Manifest (`core/src/daemon/manifest/tests.rs`):

11. `later_import_binds = true` parses; absent defaults false; `link_rules`
    reports it for that language only. Control: drop the field from
    `link_rules` -> false.
12. Both keys true is refused with an error naming the file. Control: remove
    the check -> accepted.

Python plugin (`plugins/python/src/extractor/tests.rs`):

13. `from .a import f` / `from .b import *` / `__all__ = ["f"]`: the named
    row's start line is the import's (0), the `*` row's is 1. Control: pass
    `all.range` again in `reexport_dunder_all` -> line 2.
14. `from .a import f`, `from .b import *`, `from .a import f`: the named
    row's start is the third line. Control: keep the first range in
    `FileModel::import` -> line 0.
15. `from .b import *`, `from .a import f`, `from .b import *`, `__all__`:
    one `*` row, start at the third line. Control: keep the first range in
    `Emitter::reexport` -> line 0.
16. The bundled `plugins/python/plugin.toml` parses with
    `later_import_binds = true` (manifest test reading the real file).
    Control: delete the key -> false.

No test involves threads, processes or timers.

## Must confirm (owner)

1. Moving a named Python re-export node's range from `__all__` to its import
   line is acceptable (D1). Alternative: a new wire/column ordinal.
2. Star-vs-star also resolves to the later provider (D3), producing new
   Python links that were "ambiguous" before.
3. A module-level `def f` stays ahead of any re-export regardless of order
   (Python would let a later `*` rebind it). Follow-up task, or accept.
4. Two different named imports of one name keep "first wins" in the plugin
   (Python: last wins). Follow-up task, or fix here with D1.2 extended.
5. The daemon sends a node whose only change is its range as an upsert (else
   AC2 in the watcher path needs a seed): verify in the verify slice with a
   real watcher edit, or accept the unit-level `link_diff` test.
6. New base index cache at `a525fde` for the measure slice (D6).
