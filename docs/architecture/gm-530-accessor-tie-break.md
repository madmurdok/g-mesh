# GM-530: a qualifiedName key prefers a property's getter over its setter/deleter

Status: design note (GM-530/S1). Follow-up to
[GM-511](gm-511-python-property-accessors.md) section 6 item 3.

## 1. Today

GM-511 made a Python property with `@x.setter` / `@x.deleter` three nodes that
share one `qualifiedName` `C.x` and differ only in `nativeKind`
(`method`, `setter`, `deleter`; strings from
`plugins/python/src/extractor/syntax.rs` `Accessor::native_kind`, getter left
as `method`). TypeScript has done the same since before: `get x()` / `set x(v)`
are `getter` / `setter` (`plugins/typescript/src/extractor/syntax.rs`
`method_native_kind`).

A cross-file class-qualified use - `from .shapes import C` then `C.x` in
another file - is a placeholder keyed `QualifiedName("C.x")`. The linker
(`core/src/graph/symbol_links.rs` `link`, the `several =>` arm at l.1306)
finds 2-3 candidates. The only tie-break there, `Resolver::sole_non_member`
(l.1670), returns `None` for any `qualifiedName` key, so the edge stays on its
placeholder: **unlinked**, though every candidate is a function of the same
property.

## 2. Change

A second tie-break, tried in the same `several =>` arm when
`sole_non_member` gives nothing:

> For a `qualifiedName` key, if **all** candidates are in **one file**, and
> **exactly one** has `nativeKind` in `{method, getter}` and **every other** has
> `nativeKind` in `{setter, deleter}`, link to that one.

"One declaration group" in the data is therefore *same `filePath` + same
`qualifiedName`* (the key already guarantees the second). Not needed on top:

- container: within one file a `qualifiedName` names one place;
- kind: a non-function `C.x` (a `Variable`, a TS field) has a `nativeKind`
  outside both sets, so it already breaks the rule;
- "nativeKinds pairwise distinct": a node id includes the `nativeKind`, so two
  `setter` nodes of one `C.x` in one file are one node (first-wins merge).

The sets are language-agnostic constants: Rust/Go never emit `setter`/
`deleter`, and TS never emits `deleter`, so one rule serves both languages
without a per-language table.

The rule runs only after the edge-kind filter (`required_target_kind`), so a
`CALLS` and a `REFERENCES` edge see the same accessor functions (all three
are `Function`).

## 3. Example

```python
# shapes.py                      # use.py
class C:                         from .shapes import C
    @property                    def show(c): return C.x   # REFERENCES, key QualifiedName("C.x")
    def x(self): ...
    @x.setter
    def x(self, v): ...
    @x.deleter
    def x(self): ...
```

Candidates: `C.x[method]`, `C.x[setter]`, `C.x[deleter]`, all in
`shapes.py` -> linked to `C.x[method]`. The TS pair `get x()` / `set x(v)`
links to `C.x[getter]`.

Stays unlinked:

| Case | Why |
|---|---|
| Two unrelated `C.x` (e.g. one per file in a shared container, or method + setter from different files) | not one file |
| TS `static x()` + `get x()` + `set x(v)` on one class | two getter-set kinds (`method`, `getter`) |
| Python `x = property(f)` then `@x.setter def x` | `Variable` `C.x` has a `nativeKind` outside the sets (today: `CALLS` already links the setter by the kind filter; `REFERENCES` stays ambiguous) |
| Setter + deleter with no getter (`@x.setter` whose `x` is not a `def`, only possible as in the row above) | no getter-set candidate; nothing obvious to prefer |
| A `name` key (`Key::Name`) | unchanged: members alone stay ambiguous (`sole_non_member` doc); see must-confirm 2 |

A setter-only TS property (`set x(v)` alone) is a single candidate and links
today; unchanged.

## 4. Consequence

- Restores GM-511's pre-split behaviour for class-qualified cross-file `C.x`
  (it linked to the getter then) and fixes the same gap for TS.
- A write through the class (`C.x = v`) also lands on the getter - correct for
  Python (it replaces the descriptor, GM-511 table row "class-qualified") and
  the same choice GM-511 made in-file.
- No schema, plugin or wire change; no re-index needed beyond the next link
  pass. Cost: one more column in an existing query, no extra SQL per
  ambiguity.

## 5. Edit map (all in `core/src/graph/symbol_links.rs`)

| Site | Line | Edit |
|---|---|---|
| `struct Candidate` | 1349 | add `native_kind: Option<String>` |
| `Resolver::new`, `const CANDIDATE` | 1419 | append `nativeKind` to the column list (index 9) |
| `Resolver::declared`, the `map` closure | 1759-1770 | `native_kind: row.get(9)?` |
| new `fn sole_accessor_getter(key, several) -> Option<&Candidate>` | after `sole_non_member` (~1690) | the rule of section 2; pure, no `&mut self`/SQL; constants `GETTER_NATIVE_KINDS = ["method", "getter"]`, `ACCESSOR_NATIVE_KINDS = ["setter", "deleter"]` beside it |
| `link`, `several =>` arm | 1306-1308 | `sole_non_member(..)?.or_else(\|\| sole_accessor_getter(&placeholder.key, several))` |
| `sole_non_member` doc | 1665 | "A `qualifiedName` key ... gets no tie-break" -> point to `sole_accessor_getter` |
| `docs/architecture/gm-511-python-property-accessors.md` | 119, 230 | mark the limitation fixed by GM-530 |

Tests: `core/src/graph/symbol_links/tests.rs`, beside
`a_qualified_name_key_disambiguates_two_same_named_methods` (l.1260). Note the
`member` helper's id is `<kind>:<file>:<qualifiedName>`, which collides for
getter/setter: the new tests need an id that includes the `nativeKind` (a
small helper like `gm472_member`, l.1900, setting `native_kind`).

**Overlap with GM-531** (`through_head` / `walk`): none of those bodies are
touched. Shared surface is `Candidate` (one new field) and `Resolver::new` /
`declared`; if GM-531 constructs a `Candidate` anywhere new, that site needs
the field too (compile error, not a silent merge). `through_head`'s members go
through the same `link` arm, so its results get the tie-break for free.

Code facts relied on:

- `find_callers(sole_non_member)` (g-mesh): one caller, `link`
  (symbol_links.rs:1238) - so the tie-break belongs in that one arm.
- `native_kind` reaches a `Candidate`: nowhere today (read of `Candidate`,
  `CANDIDATE` SQL and `declared`'s `map` - the only constructor; the column
  is already on `nodes`, read as `n.nativeKind` by the re-export queries).
- Accessor strings: grep of the two plugin `syntax.rs` files (non-code
  question for g-mesh: TS is not indexed in this project, plugin failed).

## 6. Semantic bridge (LSP)

No change. The SDK bridge (`plugins/sdk/src/lsp/bridge.rs` `node_at`,
`declaration_at`) binds a server's `definition` answer by **location**, not by
`qualifiedName`, so it never sees this ambiguity; and any placeholder a
semantic pass does emit by `qualifiedName` goes through the same `link` arm
and gets the tie-break. One open question is the bridge's own `agree` rule: if
pyright answers `C.x` with all three accessor locations, they land on three
nodes and the bridge refuses. That is GM-511 must-confirm 5 (measure), not
this task.

## 7. Must confirm (owner)

1. The getter-set includes Python's plain `method` (GM-511 kept the getter as
   `method`), so the rule keys on "one `method` plus only setter/deleter".
2. Scope: `qualifiedName` keys only, per the task. A `name` key reaching only
   a getter/setter pair stays ambiguous (consistent with "members alone are
   ambiguous" in `sole_non_member`). Extending it is one guard removed.
3. Setter/deleter without a getter stays unlinked rather than preferring the
   setter.

## 8. Controls (5)

Each reverts the fix (code, not test); the named test must fail.

| # | Test (new) | Revert | Expected |
|---|---|---|---|
| C1 | Python `method`+`setter`+`deleter`, `REFERENCES` by `QualifiedName("C.x")` links to the getter (acceptance 1) | `sole_accessor_getter` returns `None` | fails: unlinked |
| C2 | TS `getter`+`setter`, `CALLS` links to the getter (acceptance 2) | drop `"getter"` from `GETTER_NATIVE_KINDS` | fails: unlinked |
| C3 | `method` and `setter` `C.x` in two files of one container stay unlinked (acceptance 3) | drop the same-file check | fails: linked |
| C4 | `method` + `getter` + `setter` (TS static + accessor pair) stay unlinked | "exactly one getter" -> "first getter" | fails: linked |
| C5 | getter+setter reached by a `Key::Name` placeholder stay unlinked | drop the `Key::QualifiedName` guard | fails: linked |

Five, not 6-8: the rule has five independent pieces (kind sets x2, file,
exactly-one, key guard); kind/container/distinctness checks were dropped as
redundant (section 2), so they have nothing to revert. Acceptance 3's
"two unrelated declarations" of different kinds is already pinned by
`an_ambiguous_reference_is_left_unresolved` (l.490), which stays green. All
five are one test binary (core lib tests): one control worktree, sequential
reverts. No processes/threads/timers: no 5x repeats.
