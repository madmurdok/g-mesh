# GM-511: a Python property's getter, setter and deleter as three nodes

Design note for GM-511, slice S1. Base: `fix/GM-511-python-property-accessors`
at `9e3097b`. Status: proposed, awaiting owner review before code.

## 1. Observed behaviour (it is a gap)

Fixture (`pkg/mod.py`, run through `extractor::tests`' `tree()` harness as an
uncommitted probe test):

```python
def audit(v): return v
def compute(): return 1
def wipe(): pass

class C:
    @property
    def x(self):            # line 10
        return compute()

    @x.setter
    def x(self, value):     # line 14
        audit(value)

    @x.deleter
    def x(self):            # line 18
        wipe()

    def reader(self): return self.x
    def writer(self, v): self.x = v
    def eraser(self): del self.x

def outside(obj):
    y = obj.x
    obj.x = 2
    del obj.x
    return C.x
```

Output (relevant rows):

| What | Observed |
|---|---|
| Nodes named `C.x` | **one**: `Function`, `nativeKind = "method"`, range lines 10-12 (the getter only), signature `@property def x(self)`, no `declarations` |
| `CALLS` out of `C.x` | `compute`, **`audit`, `wipe`** - the setter's and deleter's bodies are attributed to the getter node |
| `@x.setter` / `@x.deleter` | no edge (resolves to `C.x.setter`, which `lookup_qualified` does not find: `Bound::Nothing`) |
| `@property` | no edge (builtin) |
| `self.x` read / `self.x = v` / `del self.x` | `REFERENCES reader/writer/eraser -> C.x` (all onto the getter) |
| `obj.x`, `obj.x = 2`, `del obj.x` | nothing (unknown receiver, not a call: no edge, no open site) |
| `C.x` (class-qualified, same file) | `REFERENCES outside -> C.x` |

Cause: `Declarer::function` (decls.rs:242) gives every `def` in a class
`nativeKind = "method"`, so the three defs share one id
`(path, Function, "C.x", "method")`; `Emitter::declare` (emit.rs:226) keeps the
first (emit.rs module doc, "first-wins"). The body pass `Bodies::function`
(bodies.rs:277) finds a def's id through `FileModel::lookup_qualified("C.x")`,
which also returns the first, so the setter's and deleter's edges land on the
getter. `find_callers(audit)` therefore names the getter, and the setter's
signature, range and docstring are not in the index at all.

## 2. Id scheme

**Chosen: the accessor kind is the `nativeKind`; the getter keeps today's id.**

| def | `nativeKind` | id |
|---|---|---|
| `@property def x` (and any undecorated / other method) | `method` (unchanged) | unchanged |
| `@x.setter def x` | `setter` | `(path, Function, "C.x", "setter")` |
| `@x.deleter def x` | `deleter` | `(path, Function, "C.x", "deleter")` |

All three keep `name = "x"`, `qualifiedName = "C.x"`, the same
`qualifiedPath` and container; each gets its own range, signature and
docstring, and its own `DEFINES`/`EXPORTS` (through `Emitter::declare`
unchanged). Same pattern as TypeScript (`plugins/typescript/src/extractor/syntax.rs`
`method_native_kind`: `getter`/`setter` in the id).

Recognition (one shared helper in `syntax.rs`, used by both passes): in a
**class frame**, a `def NAME` carrying a decorator whose expression is exactly
the two-segment dotted name `NAME.setter` or `NAME.deleter`
(`dotted_segments == [NAME, "setter"|"deleter"]`). Not recognised, on purpose:
`@x.getter def x` (stays `method`, merges into the getter by first-wins - a
getter replacement *is* the getter), `@Base.x.setter def x` (3 segments; one
def of that name, no collision), a setter whose name differs from its
decorator's head (no collision either).

Stability: deterministic from the source; the getter's id, range and
signature do not move, so an existing index and anything keyed on getter ids
(embeddings, cached answers) is untouched; a property with no setter/deleter
produces exactly today's output. A repeated `@x.setter def x` (conditional
definition) merges into one `setter` node, first-wins, as every other repeat.

**Rejected: TypeScript-exact `getter`/`setter` (the `@property` def becomes
`getter`).** More uniform across languages and marks a lone `@property`, but it
changes the id of every `@property` in every indexed project (a churn of
nodes, edges and embeddings on upgrade) for information the signature
(`@property def x(self)`) already carries. Also rejected: distinct
`qualifiedName`s (`C.x.setter`) - that invents a path segment Python does not
have and breaks `find_definition("C.x")` returning all three.

## 3. How usages resolve

Setter and deleter are registered in the model **only** in a new accessor
table keyed by `(getter's qualified name, accessor)`, not in `by_scope` and not
in `by_qualified`. So every existing lookup (`lookup`, `lookup_qualified`,
`qualifier_of`) sees exactly what it sees today - the getter - and a bare `x`
in the class body does not become ambiguous.

| Usage | Today | After |
|---|---|---|
| Edges written from inside a setter/deleter body | on the getter | on the setter/deleter node (body pass looks the def's own id up through the shared helper + accessor table) |
| `@x.setter` / `@x.deleter` decorator | no edge | no edge (unchanged; pinned by a test so it is not left to an accident of resolution) |
| `self.x` read (instance parameter) | `REFERENCES -> getter` | unchanged |
| `self.x = v` (instance parameter, assignment target) | `REFERENCES -> getter` | `REFERENCES -> setter` when the class has one, else getter as today |
| `del self.x` | `REFERENCES -> getter` (via generic walk) | `REFERENCES -> deleter` when there is one, else getter |
| `self.x += v` | `REFERENCES -> getter` | `REFERENCES -> getter` **and** `-> setter` (read then write) |
| `self.x.y = v` | getter (receiver walk) | unchanged: only the outermost target attribute is a store; its object is a load |
| `C.x = v` / `del C.x` (class-qualified) | getter | unchanged: getter. Assigning on the class replaces the descriptor, it does not call the setter |
| `obj.x` / `obj.x = v` / `del obj.x` (unknown receiver) | nothing | nothing (documented gap, as today) |
| Cross-file `C.x` (`from .mod import C`, placeholder keyed `QualifiedName("C.x")`) | linked to the getter | **unlinked**: the linker now sees 2-3 candidates with one `qualifiedName`, and a `qualifiedName` key gets no tie-break (`symbol_links.rs` ~1257-1270, `sole_non_member`). Accepted and documented; TypeScript getter/setter pairs behave the same today. Only properties that do have a setter/deleter are affected, and only class-qualified access (`C.x`), which is rare. **Fixed by GM-530**: the linker prefers the getter (`sole_accessor_getter`, [design](gm-530-accessor-tie-break.md)) |

Routing only happens on `Bound::Here` through the instance parameter
(`resolve_path`'s `self.member` branch); everything else keeps today's target.

### Semantic tier (pyright, ADR 0024)

- No property access creates an open site (a load/store is not a call), so the
  bridge never asks pyright about one. Unchanged.
- No accessor gets a `declarations` list (only `@overload` sets do,
  `Emitter::finish`), so ADR 0024's ordinals are unaffected.
- `obj.x(...)` (calling what a property returns) is a `ReceiverCall` open site;
  pyright's `definition` location is mapped with `node_at` (tightest node
  containing it). With distinct ranges a location in the setter now lands on
  the setter node instead of the class `C` - an improvement, not a regression.
  If pyright answers with several accessor locations, the bridge's "agree"
  rule leaves the site unanswered, as it does today (today they would land on
  `C.x` and `C`).
- `core/src/mcp/untyped.rs` `is_method`: a `setter`/`deleter` node is not in
  `METHOD_NATIVE_KINDS` but is a type member (`unlinked::is_type_member`), so
  it is still treated as a method. No change needed.

### Conformance

`plugins/python/conformance/project` has no `@property`/`.setter`/`.deleter`
(grep). No expectation changes. `README.md` gap 7 / "Decorators that replace a
function" (README ~202, ~264) gains one sentence on accessors.

## 4. Edit map

Change:

- `plugins/python/src/extractor/syntax.rs` - new
  `property_accessor(item, name, source) -> Option<Accessor>` (`Setter |
  Deleter`) beside `has_decorator` (158-168), built on `decorators` (113-119)
  and `dotted_segments` (61).
- `plugins/python/src/extractor/decls.rs` - `Declarer::function` (242-269):
  `native_kind` = `"setter"`/`"deleter"` in a class frame when the helper
  says so; `Declarer::declare` (290-307): for an accessor, record it in the new
  accessor table instead of `model.declare`.
- `plugins/python/src/extractor/model.rs` - `FileModel` (103-109): new
  `accessors: HashMap<(String, Accessor), DeclRef>` with
  `declare_accessor` (first-wins) and `accessor(qualified, Accessor)`;
  `declare` (118-125) unchanged.
- `plugins/python/src/extractor/bodies.rs`:
  - `Bodies::function` (277-327): the def's own id = accessor table when the
    helper recognises it, else `lookup_qualified` as today.
  - `visit` (231-275): a `delete_statement` arm walking its targets in a
    delete context.
  - `assignment` / `visit_target` (458-491): pass a store context (augmented
    assignment: load + store) to the outermost attribute target only.
  - `attribute` (587-607) / `resolve_path` (678-746, instance branch ~686-697)
    / `emit` (796-829): carry the access context; on an instance-parameter
    `Bound::Here` to a getter with a matching accessor, retarget (or add, for
    augmented) the `REFERENCES` edge.
- `plugins/python/src/extractor/emit.rs` - module doc (54-66): a paragraph
  "Property accessors" next to "Overload sets". No code change.
- `plugins/python/README.md` - gap 7 / known-gaps section.

Read for context (no change): `Emitter::declare` (emit.rs:226),
`Emitter::finish` (emit.rs:364), `FileModel::lookup` (model.rs:129),
`Bodies::walk_receiver` (bodies.rs:612), `qualifier_of` (bodies.rs:758),
TS precedent `method_native_kind` and `plugins/typescript/tests/declarations.rs`
`getter_and_setter_of_one_name_are_two_nodes`.

Call sites relied on (g-mesh, project `g-mesh`; branch has not changed these):
`find_references(has_decorator)` -> `Declarer::function`, `Bodies::function`
only; `find_references(STATIC_METHOD)` -> `Bodies::function` only (the const
OVERLOAD is used once, decls.rs:256, grep in one known file);
`find_callers(FileModel::lookup_qualified, python)` -> `Bodies::class`,
`Bodies::function`, `Bodies::resolve_path`;
`find_callers(Emitter::declare, python)` -> `Declarer::declare`, `announce`,
`assignment`.

## 5. Behaviour list (for the tests slice)

1. The fixture yields three `Function` nodes `C.x` with nativeKinds
   `method`, `setter`, `deleter` and three distinct ids; each has its own range
   (its own decorator through its body) and signature.
2. The getter's id equals `node_id(path, Function, "C.x", "method")`, i.e. the
   id a property without accessors has today (stability).
3. A property with no setter/deleter extracts byte-identically to today
   (one `method` node).
4. `CALLS C.x[method] -> compute`, `C.x[setter] -> audit`,
   `C.x[deleter] -> wipe`, and no cross-attribution.
5. `@x.setter` / `@x.deleter` produce no edge.
6. `self.x` -> getter; `self.x = v` -> setter; `del self.x` -> deleter;
   `self.x += v` -> getter and setter.
7. With no setter declared, `self.x = v` still references the getter (today's
   behaviour).
8. `C.x = v` and `C.x` (class-qualified) reference the getter.
9. `obj.x` / `obj.x = v` / `del obj.x` produce no edge and no open site.
10. Two `@x.setter def x` in one class (conditional) are one `setter` node,
    range of the first; the second body's edges land on it.
11. `@x.getter def x` merges into the getter (no `getter` node).
12. A `def x` decorated `@x.setter` outside a class frame stays a plain
    `function`.
13. A bare `x` in the class body after the accessors still resolves to the
    getter (not made ambiguous).

Controls: 1/4/6 fail with the `native_kind` choice in `Declarer::function`
reverted to always `method`; 4 alone fails with `Bodies::function` reverted to
`lookup_qualified`; 6 fails with the context retargeting in `emit` removed;
13 fails if accessors are registered through `model.declare`.

## 6. Must confirm (owner)

1. Getter keeps `method` (stable id) rather than TS-style `getter`.
2. Usage routing by context (store -> setter, del -> deleter, augmented ->
   both) in this task, rather than the cheaper "all usages stay on the getter,
   documented". Routing is ~one context parameter through the body pass.
3. Accepting that a cross-file class-qualified `C.x` on a property **with**
   accessors becomes unlinked (ambiguous `qualifiedName`), like TS
   getter/setter today. The alternative is a core linker tie-break preferring
   `nativeKind = method` for a qualifiedName key - out of scope; would be a
   follow-up task. **Fixed by GM-530** ([design](gm-530-accessor-tie-break.md)):
   the linker now links such a `C.x` to the getter.
4. `deleter` as the new nativeKind spelling (TS has no counterpart).
5. To verify in the code slice: the tree-sitter-python shape of
   `delete_statement` with several targets (`del a.x, b.y`), and (optional,
   measure) what pyright's `definition` returns for a property access.
