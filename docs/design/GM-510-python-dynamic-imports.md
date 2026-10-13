# GM-510: Python, fold dynamic imports and emit function-local imports

Design slice S1 of release 4.7.0, revised after the owner widened the task
("Расширить GM-510"). Verdict: **a real gap.** No form of
`importlib.import_module(...)` or `__import__(...)` produces an `IMPORTS`
edge today, not even a plain string literal, and no `import` written inside a
function body produces one either.

Scope after the revision:

1. Static `import` / `from ... import` inside a function body (and a class
   body, section 4 D7) produces the same `IMPORTS` edge, from the file node,
   that a module-level one does. The names it binds stay function locals.
2. Dynamic imports with a statically known name fold into `IMPORTS` edges,
   at module level and inside functions, including when `importlib` /
   `import_module` is bound by a function-local import.
3. Out of scope, follow-up task: resolving **uses** of names a
   function-local import binds (e.g. a `CALLS` edge from `f` onto
   `pkg.mod.helper` for `def f(): from pkg.mod import helper; helper()`).
   Those uses stay `Bound::Nothing`, as today.

## 1. Gap evidence

### Fixture

A scratch project (outside the repo, in the session scratchpad
`gm510-s1/fx/`) with `pkg/__init__.py`, `pkg/mod.py`, `pkg/other.py`,
`pkg/consts.py` (`OTHER = "pkg.mod"`), `pkg/sub/__init__.py`,
`pkg/sub/leaf.py`, and one file per case:

| file | source |
|---|---|
| c01_attr | `import importlib` / `def f(): importlib.import_module("pkg.mod")` |
| c02_alias | `import importlib as il` / `def f(): il.import_module("pkg.other")` |
| c03_from | `from importlib import import_module` / `import_module("pkg.sub.leaf")` |
| c04_from_alias | `from importlib import import_module as im` / `im("pkg.sub")` |
| c05_dunder | `__import__("pkg.sub.leaf")` |
| c06_rel_name | `import_module(".mod", package=__name__)` |
| c07_rel_kw | `import_module(".leaf", package="pkg.sub")` |
| c08_rel_pos | `importlib.import_module("..mod", "pkg.sub")` |
| c09_const | `NAME = "pkg.mod"` / `def f(): return import_module(NAME)` |
| c10_fstring | `import_module(f"pkg.{x}")` (unfoldable) |
| c11_concat | `import_module("pkg." + x)` (unfoldable) |
| c12_cross_file | `from pkg.consts import OTHER` / `import_module(OTHER)` (unfoldable) |
| c13_external | `importlib.import_module("json.decoder")` |
| c14_static_control | `import pkg.mod` (control: the harness does see imports) |

A scratch cargo binary (path dependency on `plugins/python`, its own target
dir) ran `PythonExtractor.extract` over each file with a real
`ProjectContext::load` of the fixture and printed every edge.

### Observed output (`IMPORTS` edges only; every file also has its usual `DEFINES`/`EXPORTS`)

```
c01..c04, c06..c11, c13   Imports file -> external_module importlib  spec="importlib"
c05_dunder                (no IMPORTS edge at all)
c12_cross_file            Imports file -> external_module importlib
                          Imports file -> resolved_module pkg.consts::*  spec="pkg.consts"
c14_static_control        Imports file -> resolved_module pkg.mod::*     spec="pkg.mod"
```

The only `IMPORTS` edges come from the static imports: `import importlib`,
the control, and `pkg.consts`. No dynamic import reaches `pkg.mod`,
`pkg.other`, `pkg.sub`, `pkg.sub.leaf` or `json.decoder`. (`c09` keeps its
existing `References f -> NAME` edge, which must survive the change.)

The function-local static gap was not run through the fixture. The code
shows it directly (next subsection), and the owner's acceptance criterion
states it.

### Why: today's code

- `Declarer::statement` (`decls.rs:234-237`) handles `import_statement` /
  `import_from_statement` **only when `self.scopes.kind() ==
  FrameKind::Module`**. An import in a function or class body reaches
  `statement` (because `Declarer::function`/`class` walk bodies to find nested
  `def`s) and falls through to `_ => {}`.
- `Bodies::visit` (`bodies.rs:263`) has `"import_statement" |
  "import_from_statement" => {}` for **every** frame. It was written so that
  module-level imports are not walked twice, and it also skips function-body
  imports.
- `Scopes::bind_import` (`scope.rs:305-333`, reached from the pre-scan at
  `scope.rs:289`) binds a function-local import's names as plain locals.
  That is the only trace a local import leaves.
- For dynamic imports, `Bodies::call` (`bodies.rs:608`) resolves the callee.
  `importlib.import_module` resolves through `qualifier_of` to
  `Qualifier::External`, and `import_module` resolves through `resolve_bare`
  to `Bound::Nothing`, because both names are bound by an import of an
  external module (`Import::External`). Nothing is emitted, and the string
  argument is walked as an ordinary expression, which also emits nothing.
- `Import::External` does not record which external module the name came
  from, so nothing downstream can tell `importlib` apart from any other
  library.

## 2. How TypeScript does it (the shape to mirror)

`plugins/typescript/src/extractor/imports.rs` (the TS plugin is Rust now; the
`extract.test.ts` lines named in the task predate that):

- `Declarer::record_call_import` (line 302) defers a `require(...)`/`import(...)`
  call's **first argument** when its shape could fold
  (`is_foldable_specifier_shape`, line 495).
- `Declarer::resolve_call_imports` (line 324) runs after all declarations are
  known. `resolve_call_import` (339) calls `fold_specifiers` (355) and then,
  for each folded string, `record_specifier` (195). That produces the **same**
  placeholder and `IMPORTS` edge as an ordinary `import` statement: from the
  **file** node, spanning the argument node, with the folded string as
  `specifier`.
- `fold_static` (371) folds a closed set of shapes: literal/template,
  same-file `const` (`fold_constant`, 402, with a `folding` set against
  cycles, a local of the same name shadows it, another file's name does not
  fold), string enum members, `path.join(__dirname, ...)`. Conditional
  `a ? "x" : "y"` yields one edge per branch. **All or nothing**: any part not
  known makes the whole argument emit nothing.
- **No special edge kind or flag.** A dynamic import is a plain `IMPORTS`
  edge; `WireEdge` (`wire/src/lib.rs:499`) has no "dynamic" field.

g-mesh: `find_callers` found no resolved callers for `record_call_import` or
`resolve_call_imports` (the Rust semantic tier is absent). Their
`unlinkedUsages` point to `plugins/typescript/src/extractor/bodies.rs` and
`decls.rs` respectively.

## 3. Proposed behaviour

### 3a. Function-local static imports

An `import` / `from ... import` anywhere below a function or class body
(including inside `if`/`try`/`with`/`for` there) emits the same `IMPORTS`
edges the identical module-level statement would emit, and nothing else:

- `import a.b` / `import a.b as c` -> edge onto `a.b`;
- `from a.b import x` -> edge onto `a.b`, plus onto `a.b.x` when `a.b.x` is a
  real submodule (`has_container`, the module-level GM-358 rule);
- relative forms resolve against this file's own package exactly as at module
  level; dots above the top-level package emit nothing;
- `from a import *` inside a function is a `SyntaxError` in Python 3. It
  emits the container edge and **no** re-export. The re-export belongs to the
  module namespace, and this statement cannot populate it.

What it does **not** emit: no `pending_symbol` placeholder or `REFERENCES`
edge for a from-imported name, no `Import` binding in `FileModel`, no
re-export. The bound names stay plain locals (`scope.rs:289` unchanged). A
use of them still resolves to nothing (follow-up, scope item 3).

Example:

```python
# pkg/service.py
def handler():
    from pkg.models import User      # lazy, to break a cycle
    return User()
```

Today: no `IMPORTS` edge to `pkg.models`. After: `IMPORTS file ->
resolved_module pkg.models::*` with `specifier = "pkg.models"`.
`get_dependencies` shows `pkg/models.py`. `User()` still produces no edge
(follow-up).

### 3b. Dynamic imports

A call whose callee is statically `importlib.import_module` or the builtin
`__import__`, and whose module-name argument folds to a string from this
file alone, emits exactly what the equivalent static `import <name>` emits: a
`resolved_module` placeholder (project container) or an `external_module`
node, and an `IMPORTS` edge from the file node onto it. Nothing else changes:
the existing `REFERENCES` edges of the call's arguments (e.g. onto `NAME`)
stay.

Example, `pkg/c09_const.py`:

```python
from importlib import import_module
NAME = "pkg.mod"
def f():
    return import_module(NAME)
```

Today: `IMPORTS file -> external_module importlib`. After: also `IMPORTS
file -> resolved_module pkg.mod::*` with `specifier = "pkg.mod"`.

Example with a function-local binding (the lazy-loader shape):

```python
def load_backend():
    import importlib
    return importlib.import_module("pkg.backends.fast")
```

After: `IMPORTS file -> external_module importlib` (3a) **and** `IMPORTS
file -> resolved_module pkg.backends.fast::*` (3b).

What does **not** fold emits nothing. The worst outcome is a missing edge,
never a wrong one: f-strings, `+`/`%`/`.format`, a name from another file, a
parameter, a function-local variable, a reassigned module name, a call
result, a loop variable. The real-world case in psf/requests
`compat._resolve_char_detection` does `for lib in ("chardet",
"charset_normalizer"): importlib.import_module(lib)`, which uses a loop
variable and does not fold.

### 3c. Consequences

- **Lazy imports used to break import cycles now show those cycles.** A
  function-local `from pkg.models import User` in `pkg/service.py`, where
  `pkg/models.py` imports `pkg.service` at module level, used to look acyclic
  in `get_dependencies`. Now both directions are edges. This is correct
  (both files depend on each other), but a reader who relied on the missing
  edge will see new cycles.
- **Lazy and eager are indistinguishable.** The edge starts at the file node
  and carries no flag, so a function-local or dynamic import looks exactly
  like a module-level one. Answering "is this dependency loaded at import
  time?" is not possible from the graph. TS has the same property for
  `import()`.
- **Dependency answers grow.** `get_dependencies` both ways and the
  `IMPORTS` count per file can only grow. No existing edge changes target.
- **Dead or conditional branches count.** As at module level, every branch
  is indexed: `try: from fast import x / except ImportError: from slow import
  x` inside a function yields both edges.
- **No existing test pins the old behaviour.** No unit test in
  `extractor/tests.rs` asserts the absence of a local-import edge, and the
  conformance fixture (`plugins/python/conformance/project`) has no
  function-local imports and no `importlib`. Must-confirm M5.

## 4. Decisions

Each one covers today, the change, an example, and the consequence. D1a, D2,
D5a and M3 are still open for the owner. D3, D4, D6 and D7 are stated as
decided and can be reopened.

### D1. Which callees count (alias tracking)

**Today:** `import importlib`, `import importlib as il`, `from importlib
import import_module [as im]` are all recorded as `Import::External` with no
module name, so the callee cannot be recognised.

**Change:** the callee counts when, after the usual scope walk, it is:

- `<L>.import_module`, where `<L>` is bound by `import importlib`,
  `import importlib.<anything>` (which binds `importlib`), or `import
  importlib as <L>` (exactly `importlib`; `import importlib.util as u` binds
  `util`, not `importlib`);
- `<L>`, where `<L>` is bound by `from importlib import import_module [as
  <L>]` (absolute `importlib` only; `from . import importlib` is a project
  item);
- bare `__import__`, when no binding of this file shadows the name: no module
  declaration, no import, no local in the frame chain.

The binding can sit at module level (D1a) or in an enclosing function frame
(D7b). A name rebound later is not the callee any more: a later
unconditional import of another module to the same local, a module-level
`def` or assignment after the import, or a non-import local of the same name
in the deciding frame.

**Decision D1a: where to keep the module-level importlib binding.**
(Open for the owner.)

- **A (recommended): a side map in `FileModel`**, `importlib: HashMap<String,
  ImportlibName>` (`Module` | `ImportModule`), written next to every
  `model.import(.., Import::External, ..)` call in `Declarer`. A use counts
  only if `lookup_import(L) == Some(Import::External)` **and** the map has
  `L`. A later unconditional external import of another module to `L` removes
  it. Benefit: `Import` and its equality are untouched, and
  `FileModel::import`'s replace-or-keep rule depends on that equality. Three
  write sites. The `ImportlibName` classifier is shared with the
  function-local frames (D7b), so module level and function level use one
  vocabulary. Risk: two maps kept in step by hand. A missed write site leaves
  a stale alias (B8 pins the rebinding case).
- **B: add the dotted module to `Import::External { module }`.** Benefit: one
  source of truth at module level. Risk: it changes `Import` equality, so in
  `try: import a as x / except: import b as x` the two externals now compare
  unequal, and the conditional-replace rule in `FileModel::import` behaves
  differently. Every `Import::External` match changes (bodies.rs
  `resolve_bare`, `qualifier_of`, census).

**Does function-local import tracking change this?** It strengthens A.
Function frames never hold `Import` values (they live in `Scopes`, not
`FileModel`), so under B the function level would still need its own
`ImportlibName` map. B would then mean two mechanisms with different shapes.
Under A, both levels store the same `ImportlibName`, produced by one
classifier.

### D2. What folds (folding scope)

(Open for the owner.) The module-name argument folds when it is:

1. a **plain string literal**: node kind `string`, no `interpolation` child
   (rejects f-strings), no `escape_sequence` child, prefix (the
   `string_start` text minus quotes) without `f`/`b` (any case); `r`/`u` are
   fine. Implicit concatenation (`concatenated_string`) does not fold.
   The existing `syntax::string_literal` returns `pkg.{x}` for `f"pkg.{x}"`,
   so it **must not** be reused as is (B7 pins this);
2. an **identifier** naming a module-level constant of this file (D3),
   folded recursively with a cycle guard (`A = B`, `B = "pkg.mod"` folds;
   `A = B`, `B = A` does not).

An empty folded string emits nothing (`import_module("")` raises).

- **A (recommended): literal and same-file constant only.** Benefit: exactly
  the acceptance criteria, with the smallest surface. Risk: it misses
  `import_module(f"pkg.{KIND}")` where `KIND` is a constant, which TS would
  fold (a template over consts).
- **B: also f-strings and `+` whose every part folds, and conditional
  expressions (one edge per branch), mirroring TS's templates and
  ternaries.** Benefit: parity with TS. Risk: about 40 more lines plus tests.
  f-string format specs (`{x!r}`, `{x:>3}`) must be rejected explicitly or
  they fold wrong. B can be a follow-up task built on A without rework.

### D3. Constant folding scope

(Decided.) **Today:** `Declarer::assignment` declares a `Variable` for each
module-level name and keeps no value.

**Change:** a module-level name is a constant only if, across the whole file:

- it has exactly **one** binding statement: a module-level `assignment` whose
  `left` is that single identifier (an annotated `NAME: Final = "..."` counts)
  and whose `right` is a plain literal or an identifier (D2);
- there is no second module-level assignment of it (including inside
  `if`/`try` at module level, which `Declarer::statement` already walks with
  `conditional > 0`), no module-level `augmented_assignment` of it, and no
  `global NAME` in any function body.

Any of these **poisons** the name (no fold). Constants are keyed by the
variable's **node id**: two assignments of one name share one id, so the
second one finds the first and poisons it. At the use site, the identifier
must resolve through the normal frame chain to that module-level
declaration. A parameter or local named `NAME` shadows it (no fold), and so
does a later unconditional import of `NAME` (`module_binding` returns
`Import`). Only module-level constants fold. A function-local `NAME =
"pkg.mod"` does not, because it is a local (`frame.binds`).

Example: `NAME = "pkg.a"` followed by `NAME = "pkg.b"` gives no edge.
Neither value is used, because the value at call time is not statically
known.

Accepted gap: a module-level `for NAME in ...`, `with ... as NAME` or walrus
also rebinds `NAME` but does not poison it. See M3.

### D4. `__import__("a.b")` semantics

(Decided.) `__import__("a.b")` imports `a`, then `a.b`, and returns `a` (or
`a.b` with a non-empty `fromlist`). The static `import a.b` already emits
**one** edge, onto `a.b` (`import_statement`), not onto `a`. **Decision: mirror
that; the edge goes onto `a.b`.** Consequence: `__import__("pkg.sub.leaf")`
and `import pkg.sub.leaf` produce the same edge. `__import__` binds nothing
(its return value is an expression), so no binding is recorded.

`level`: Python 3 defaults to `0` (absolute). Fold only when there is no
5th positional argument and no `level=` keyword, or when it is the integer
literal `0`. Anything else (a relative `__import__`) emits nothing. Only the
first argument is the name; `globals`/`locals`/`fromlist` are ignored.

### D5. Relative names and `package=`

`importlib.import_module(name, package)`: a leading-dot `name` is resolved
against `package`, following CPython `_resolve_name`:

- level = the number of leading dots;
- base = `package.rsplit('.', level - 1)[0]`;
- an error is raised when there are fewer segments than `level - 1`.

`package` is the second positional argument or the `package=` keyword. A
relative name whose `package` is missing or unfoldable raises or is unknown,
so it emits nothing.

`package` folds when it is:

- `__package__`: the module's own package (`ModuleCtx::package`; for an
  `__init__.py`, the package itself). This is exactly
  `ModuleCtx::relative(level, tail)`, the rule `from .x import y` uses.
- `__name__`: the module's own dotted key (`ModuleCtx::key`), which equals
  `__package__` only in an `__init__.py`. In a plain module,
  `import_module(".mod", package=__name__)` really means `<this module>.mod`,
  which Python can only import if this module were a package. The edge goes
  there anyway, as a dangling `resolved_module`. A static `from .missing
  import x` has the same outcome.
- a plain literal or a constant (D2/D3).

For an orphan file (`ModuleCtx::package == None`, key `orphan:<path>`),
`__name__`/`__package__` do not fold.

**Decision D5a: how the relative result is emitted.** (Open for the owner.)

- **A (recommended):** `__package__`/`__name__` forms go through the
  relative path (`import_edge(.., relative = true)`, always a
  `resolved_module`), with `specifier` = the name as written (`.leaf`), as
  static relative imports do (GM-544). A **literal/constant** `package` is
  first resolved to an absolute name and emitted as an absolute import
  (`relative = false`, `has_container` decides internal vs external), with
  `specifier` = the resolved absolute name (`pkg.sub.leaf`). Benefit: a
  literal package naming a third-party distribution (`package="requests"`)
  becomes an `external_module`, not a fake project placeholder. The absolute
  `specifier` is also what the resolution delta's `specifier_under` selectors
  (`project/facts.rs`) match. Risk: two code paths for "relative" in one
  feature.
- **B: always absolute** (resolve, then `relative = false`). Benefit: one
  path. Risk: a `__package__`-relative import of a file not yet in the
  project becomes an `external_module` named `pkg.x`. That differs from the
  static relative rule, and a resolution delta does not repair it.

### D6. Edge shape (mirrors TS)

(Decided.)

- Kind: plain `IMPORTS`, no flag. TS has none, and `WireEdge` has no field
  for one.
- From: the **file** node, including when the import or call sits inside a
  function. Module-level imports and TS both do this.
- To: the same placeholder the static module-level import uses. A static
  module-level import, a function-local import and a dynamic import of one
  module therefore share **one node and one edge**. `Emitter::placeholder`
  dedupes nodes on `(kind, target)`. `Emitter::import_edge`
  (`emit.rs:377`) dedupes edges on `edge_id(file, IMPORTS, to)`, which does
  not include the specifier, and keeps the **first** specifier. `Declarer`
  runs to completion before `Bodies`, so when a module-level import exists,
  its specifier wins. Among function-local and dynamic imports alone, the
  first one in source order wins.
- Placeholder range: the statement range for a static local import (as at
  module level); the module-name **argument** node for a dynamic import (TS
  uses the argument node). Only the first writer's range survives the dedup.
- The call is still walked exactly as today (callee and arguments), so no
  existing edge disappears.

### D7. Function-local imports

(Decided by the owner's expansion.) Two parts: edges for static local
imports (D7a), and importlib bindings made by local imports (D7b).

#### D7a. Emitting static local imports from `Bodies`

**Where:** the arm `"import_statement" | "import_from_statement" => {}` at
`bodies.rs:263`. It becomes: when `self.scopes.kind() != FrameKind::Module`,
emit the statement's `IMPORTS` edges and record its importlib bindings
(D7b). At module level it stays a no-op, because `Declarer` already emitted
those edges.

**How `Bodies` reaches the placeholder logic.** `Declarer::import_edge`
(`decls.rs:616-631`) needs `project` (for `has_container`) and the
emitter; `Bodies` has the emitter and `module` but not `project`. The
container, the `written` specifier and the GM-358 submodule edge are
computed inline in `import_statement`, `import_from_statement` and
`imported_name`, interleaved with the `FileModel` binding writes that
`Bodies` must not do (it holds `&FileModel`). Options:

- **Chosen: one pure function that lists a statement's edges.**
  `pub(crate) fn import_edges(item: Node, source: &str, project:
  &ProjectContext, module: &ModuleCtx) -> Vec<ImportEdge>`, where `ImportEdge
  { container, specifier, relative, range }` covers every edge the statement
  emits, the GM-358 submodule edge included. `Declarer::import_statement` /
  `import_from_statement` call it for their edges and keep their binding
  code. `Bodies` calls it and emits each edge through the free function
  `emit_import_edge(project, emitter, ...)` (edit map step 1). The rules
  therefore live in one place and cannot drift. The existing import tests
  (`every_imports_edge_carries_the_import_as_written`, the GM-358 pair, the
  relative ones) pin the refactor of `Declarer`.
- Rejected: copy the container/specifier logic into `Bodies`. It is cheaper
  now, but the two copies drift (GM-544 and GM-358 each changed this logic
  once).

`Declarer`'s binding code still needs each leaf's `external` flag. That flag
is `!(relative || project.has_container(container))`, which is cheap to
recompute or to carry on `ImportEdge`.

**Cases:**

- *Relative local imports*: `import_edges` uses `ModuleCtx::relative`, the
  same as at module level. Above the top-level package, or in an orphan:
  nothing.
- *`try/except ImportError`, `if TYPE_CHECKING`, `with`, loops inside a
  function*: `Bodies::visit` recurses into every compound statement through
  `_ => visit_children`, so imports at any depth below a function body are
  reached. Every branch emits (module-level rule 6, "every branch is indexed,
  no predicate is evaluated").
- *Nested functions and methods*: each `def` pushes a `Function` frame
  (`Bodies::function`), and `scopes.kind()` is `Function` there. The edge
  still starts at the file node, whatever the nesting.
- *Lambdas*: a lambda body is an expression and cannot contain an `import`
  statement. Nothing to do.
- *Class bodies*: a class body is a **`Class` frame, not a function frame**
  (`FrameKind::Class`, `Bodies::class`). `Declarer::statement`'s module-only
  guard skips class-body imports today as well. Decision: a class-body
  `import` emits its `IMPORTS` edge like a function-body one (the
  `!= Module` test covers it). The import statement runs when the class is
  defined, so it is an eager dependency of the module. The names it binds
  become class attributes, which this tier does not model (unchanged).
- *Comprehensions*: no frame and no statements. Nothing to do.

#### D7b. Per-frame importlib bindings

**Where they live:** in the `Scopes` frame (`scope.rs` `Frame`, 104-112), as
`importlib: HashMap<String, ImportlibName>`, next to `locals`. Lookup goes
through `Scopes::chain()`, the same walk `resolve_bare`/`qualifier_of` use.

**When they are filled: the function pre-scan, not the source-order walk.**
Python makes a name local to the whole function, and `bind_function_locals`
(`scope.rs:258`) already scans the whole body before `Bodies` walks it. The
rule, applied per function frame at pre-scan time:

- a name is an importlib binding of this frame if **every** binding of it in
  the function is an import classified as the same `ImportlibName` (by the
  shared classifier `importlib_name(statement, leaf, source)`, the one D1a
  uses);
- any other binding of the same name in the function poisons it: an
  assignment, a parameter, a `for`/`with`/`except` target, a walrus, or an
  import of something else. Implementation: `bind_import` records the
  classification, every other `bind_unless`/`bind_parameter` site records the
  name as "otherwise bound", and at the end of `bind_function_locals` the
  frame keeps only the names that are never otherwise bound.

Consequences, with examples:

- `def f(): import importlib; importlib.import_module("pkg.x")` folds.
- `def f(): import importlib as il; ...il.import_module("pkg.x")` folds.
- `try: from importlib import import_module / except ImportError: from
  compat import import_module` inside a function is **poisoned**. Two
  different bindings mean the callee is not statically `importlib`'s. The
  static edges onto `importlib` and `compat` are still emitted (D7a). This is
  stricter than module level, where a conditional import does not displace
  an earlier one (D1a), because the pre-scan has no source order. The result
  is a missing edge, never a wrong one.
- `if TYPE_CHECKING: from importlib import import_module` with no other
  binding: the binding counts and calls fold. The edge is not wrong, since the
  name only resolves when that branch ran.
- A use above the import in source order (`import_module(..)` before `from
  importlib import import_module`) would raise `UnboundLocalError`. It folds
  anyway: harmless, and order-free by design.
- **Nested functions**: the chain walk lets a nested `def` (or a lambda)
  without its own binding of the name see an enclosing function's importlib
  binding. That is Python's closure rule. A nested function that binds the
  name itself decides for itself. `global import_module` in the nested
  function makes the pre-scan skip the name (`collect_outer_declarations`),
  so the lookup falls through to module level.
- A **sibling** function does not see another function's local import
  (different frames).
- **Class bodies**: no pre-scan exists for a `Class` frame. Decision: a
  class-body `import importlib` is **not** tracked as an importlib binding
  (documented gap: rare, and `chain()` skips class frames from methods, so
  only the class body itself could ever use it). The static edge is still
  emitted (D7a).

**Lookup** (`binding_is_importlib(name, want)` in `bodies.rs`): walk
`self.scopes.chain()`. In the first non-module frame where `frame.binds(name)`
holds, answer `frame.importlib(name) == Some(want)`. If no function frame
binds it, use the module-level rule (D1a). A non-module frame that
**declares** `name` (`model.lookup(frame.path(), name, ..)`, i.e. a nested
`def import_module`) answers `false`.

## 5. Edit map

Line numbers are 1-based, on branch `feat/GM-510-python-dynamic-imports` at
its fork from `release-4.7.0`. All paths are under
`plugins/python/src/extractor/`.

### Read for context (do not change)

- `mod.rs` `PythonExtractor::extract` (206-249): pass order, `Declarer`
  then `Bodies`. `Bodies::new` is called at line 240, its only caller (g-mesh
  `find_callers` on `extractor::bodies::Bodies::new`).
- `bodies.rs` `Bodies::visit` (249-299), `function` (302-360), `class`
  (362-380), `lambda` (554-567), `resolve_bare` (752-805), `qualifier_of`
  (888-933): the frame-chain walk to reuse.
- `scope.rs` `Frame` (104-126), `Scopes::chain` (196-205),
  `bind_function_locals` (258-265), `bind_locals_in` (267-299),
  `bind_import` (305-333), `bind_unless` (341-358).
- `model.rs` `Import` (84-109), `FileModel::import` (248-271),
  `module_binding` (287-303), `lookup_import` (323-325).
- `keys.rs` `ModuleCtx` (164-277), `relative` (266-276), `parent_key` (280).
- `syntax.rs` `string_literal` (214-216), `string_content` (245-251),
  `dotted_segments` (61).
- `emit.rs` `Emitter::placeholder` (269), `external_module` (344),
  `import_edge` (377-383).
- TS reference: `plugins/typescript/src/extractor/imports.rs`
  `resolve_call_import` (339-352), `fold_static` (371-379), `fold_constant`
  (402-416).

### Change

1. **`decls.rs` `Declarer::import_edge` (616-631):** move the body into a
   free function `pub(crate) fn emit_import_edge(project: &ProjectContext,
   emitter: &mut Emitter, container: &str, specifier: &str, range: Range,
   relative: bool) -> bool`. `Declarer::import_edge` becomes a one-line
   delegate. Its callers (g-mesh `find_callers`, symbol id
   `15acf0360433c0dd45a8923553e8cded`) are `import_statement`,
   `import_from_statement` and `imported_name`, all in decls.rs.
2. **`decls.rs`, new pure function `import_edges(item, source, project,
   module) -> Vec<ImportEdge>` (D7a):** extract it from `import_statement`
   (413-453), `import_from_statement` (456-524), `relative_container`
   (533-544, which becomes a free function over `&ModuleCtx`) and the GM-358
   submodule block of `imported_name` (586-602). Rewire `import_statement` /
   `import_from_statement` so they take their edges from it and keep only the
   binding writes (`model.import`, `star`, `reexport`, `imported_name`'s
   placeholder and `REFERENCES` edge). `imported_name` loses its edge block.
   The submodule edge now comes from `import_edges`, which needs
   `has_container` and the `written` text, both available there.
3. **`decls.rs`, new classifier `importlib_name(statement, leaf, source) ->
   Option<ImportlibName>`:** `Module` for an `import_statement` leaf whose
   dotted name starts with `importlib` (plain leaf) or is exactly `importlib`
   (aliased leaf). `ImportModule` for an `import_from_statement` whose
   `module_name` is the `dotted_name` `importlib` and whose leaf name is
   `import_module`. `None` otherwise. Used by step 5 and by `scope.rs`
   (step 8).
4. **`model.rs` `FileModel` (124-150):** add `importlib: HashMap<String,
   ImportlibName>`, `constants: HashMap<String /* node id */,
   Option<ConstValue>>`, `poisoned_names: HashSet<String>`, with
   `enum ImportlibName { Module, ImportModule }` (shared with `scope.rs`, so
   put it in `model.rs` or `syntax.rs`) and `enum ConstValue {
   Literal(String), Name(String) }`. Methods:
   - `note_importlib(local, Option<ImportlibName>, unconditional)`: `Some`
     inserts; `None && unconditional` removes;
   - `importlib(local)`;
   - `note_constant(id, Option<ConstValue>)`: absent inserts; present sets
     `None`;
   - `poison_name(name)`;
   - `constant(id, name) -> Option<&ConstValue>`: `None` if the name is
     poisoned.
5. **`decls.rs` `import_statement` / `imported_name`:** next to each
   `model.import` on an external binding (lines 434, 447, 568), call
   `note_importlib(local, importlib_name(..), unconditional)`.
6. **`decls.rs` `assignment` (362-387):** for each name: if `left` is a
   single `identifier` (or the annotated-assignment form of one) and `right`
   is a plain literal (`syntax::plain_string`), record `Literal`; if `right`
   is an `identifier`, record `Name`; otherwise record `None`. Call
   `note_constant(id, ..)`.
7. **`decls.rs` `statement` (212-267):** in the `expression_statement` arm,
   a module-level `augmented_assignment` poisons its target name. Add a
   `"global_statement"` arm (any frame) that poisons each named identifier.
   (`Declarer::function` already walks function bodies through `collect`, so
   a `global` inside a function reaches `statement`.)
8. **`scope.rs` (D7b):**
   - `Frame` gets `importlib: HashMap<String, ImportlibName>` and
     `Frame::importlib(name)`.
   - `bind_import` (305-333) computes `importlib_name` per leaf and records
     the classification in a scan-local map. `bind_unless` /
     `bind_parameter`, when not called through `bind_import`, record the
     name as otherwise bound. Thread a small `PreScan` accumulator through
     `bind_locals_in`, or add two scan-local fields on `Scopes` cleared by
     `bind_function_locals`.
   - At the end of `bind_function_locals`, keep in the innermost frame only
     the names whose every binding is one consistent `ImportlibName`.
   - `bind_parameters` runs **before** `bind_function_locals`
     (`bodies.rs:352` then `:355`), so parameter names must be read from the frame's
     `locals` at the start of the pre-scan and treated as otherwise bound.
9. **`syntax.rs`:** add `pub(crate) fn plain_string<'s>(node, source) ->
   Option<&'s str>`. It accepts only a `string` node with no
   `interpolation`/`escape_sequence` child and a prefix without `f`/`b`, and
   returns `string_content`. Leave `string_literal` (used by `__all__`) as
   is.
10. **`bodies.rs` `Bodies` (220-237):** add a `project: &'a ProjectContext`
    field and constructor parameter, and update the one caller, `mod.rs:240`.
11. **`bodies.rs` `visit` arm at 263 (D7a):** `"import_statement" |
    "import_from_statement"`. If `self.scopes.kind() != FrameKind::Module`,
    call `emit_import_edge` for each `import_edges(node, ..)`. Otherwise do
    nothing, as today.
12. **`bodies.rs` `Bodies::call` (608-635):** after the existing walk, call
    a new `self.dynamic_import(node)`. New functions in `bodies.rs`:
    - `dynamic_import_callee(&self, function) -> Option<Callee>`
      (`ImportModule | DunderImport`): an identifier gives
      `binding_is_importlib(name, ImportModule)`, or `name == "__import__"`
      when nothing binds it (no frame `binds`, no declaration, no
      `lookup_import`). An attribute with segments `[head, "import_module"]`
      gives `binding_is_importlib(head, Module)`.
    - `binding_is_importlib(&self, name, want) -> bool`, as described in D7b
      "Lookup". The module-level branch requires `module_binding(name, None)`
      to be `None` or `Some(Import(_))`, then `lookup_import(name) ==
      Some(&Import::External)` and `model.importlib(name) == Some(want)`.
    - `fold(&self, node, guard) -> Option<String>`: either `plain_string`, or
      an identifier that resolves (frame chain, as `resolve_bare`: any
      non-module frame binding it means `None`) to a module-level `Variable`
      declaration whose `constant(id, name)` is `Some`. `Name(n)` recurses at
      module scope.
    - `fold_package(&self, node) -> Option<(String, bool /* own package */)>`:
      `__package__` gives `(module.package?, true)`, `__name__` gives
      `(module.key, true)` unless the file is an orphan, otherwise `fold`
      gives `(s, false)`.
    - `dynamic_import(&mut self, call)`:
      - Pick the name argument: first positional, or the `name=` keyword
        (M4). Pick the package argument: second positional or `package=`.
        For `__import__`, apply the `level` check (D4).
      - Fold the name. Empty: return. Absolute: `emit_import_edge(project,
        emitter, name, name, range(arg), false)`.
      - Relative: count the dots; the tail is the rest split on `.` (an empty
        tail is allowed). Then:
        - own package (`__package__`): `module.relative(level, &tail)`;
        - own package (`__name__`): `_resolve_name` on `module.key`;
        - either own-package form: emit with `relative = true` and specifier
          = the name as written;
        - literal package: `_resolve_name` to an absolute name, emit with
          `relative = false` and specifier = that absolute name.
      - Fewer segments than `level - 1`: emit nothing.

Docs: in the `mod.rs` module doc, "# 4. Imports" gains the function-local and
dynamic forms, and "# 1." loses its implication that function-local imports
are only locals. Update the `scope.rs:302-304` doc comment ("this plugin
records imports at module level only") to say the edges are now emitted by
`Bodies` while the names stay locals. Also update
`docs/architecture/multi-language-plugins.md`'s Python section if it lists
import shapes.

## 6. Behaviours the tests must pin (with controls)

Tests go in `extractor/tests.rs`, reusing `tree(...)` and
`import_specifiers(...)`. There are 9 behaviours with one control each (a
code revert after which the named test must fail). That is one more than the
6-8 guideline, for the reason given on B6.

| # | Behaviour | Implementing function | Control (revert) |
|---|---|---|---|
| L1 | Function-local static imports emit file-level `IMPORTS` edges: `import pkg.helpers` in a `def`, a relative `from ..base import Base` in a method, `from pkg import sub` (submodule edge), an import inside `try/except ImportError` in a nested `def`, and one in a class body. A from-imported local name gets **no** `pending_symbol`/`REFERENCES` row. A module-level and a local import of one module give exactly one edge with the module-level specifier | `Bodies::visit` arm (263) + `import_edges` | Restore the arm at `bodies.rs:263` to `{}` |
| B1 | `importlib.import_module("pkg.helpers")` inside a `def` gives `IMPORTS file -> resolved_module pkg.helpers::*` with spec `pkg.helpers`, and `"json.decoder"` gives `external_module json.decoder`. `__import__("pkg.sub.leaf")` gives an edge onto `pkg.sub.leaf`, not `pkg`. `__import__("pkg.mod", g, l, [], 1)` gives nothing | `Bodies::dynamic_import` (D4) | For `DunderImport`, emit `name.split('.').next()` instead of `name` |
| B2 | Module-level aliases fold: `import importlib as il; il.import_module(..)`, `from importlib import import_module as im; im(..)`, `import importlib.util; importlib.import_module(..)` | `import_statement` / `imported_name` -> `note_importlib`; `importlib_name` | In `importlib_name`'s aliased branch, return `Module` only when the alias text is `importlib` |
| L2 | Function-local importlib bindings fold through the frame chain. Folds: `def f(): import importlib as il; il.import_module(..)`, and a nested `def` inside it. Does not fold: a sibling function using the same name, and a `try: from importlib import import_module / except ImportError: from compat import import_module` inside a function | `scope.rs` pre-scan + `binding_is_importlib` | Make the pre-scan keep a name whose bindings disagree (drop the "otherwise bound / mixed" poison), so the try/except case folds |
| B4 | Relative names: `(".leaf", package="pkg.sub")` gives `pkg.sub.leaf` with spec `pkg.sub.leaf`; `("..mod", "pkg.sub")` gives `pkg.mod`; `(".helpers", package=__package__)` in `pkg/x.py` gives `pkg.helpers` with spec `.helpers`; `(".mod")` with no package gives nothing | `dynamic_import`, `fold_package`, `_resolve_name` | In `_resolve_name`, skip the `level - 1` segment stripping (the `..` case then lands on `pkg.sub.mod`) |
| B5 | A same-file constant folds (`NAME = "pkg.mod"`, also through `A = NAME`). A reassigned, `+=`-ed or `global`-written `NAME` gives nothing. The existing `References f -> NAME` edge stays | `Declarer::assignment` / `statement` -> `note_constant`/poison; `Bodies::fold` | Make `note_constant` keep the first value instead of poisoning on a second assignment |
| B6 | A parameter or local named `NAME` shadows the constant: no edge | `Bodies::fold` (frame chain) | Drop the non-module `frame.binds(name)` check in `fold`'s identifier branch. *Reason for the 9th control:* this check alone separates the constant from a runtime value in the common `def f(NAME): import_module(NAME)` shape. A silent revert would emit **wrong** edges, the only wrong-edge failure in this design that B5 does not catch |
| B7 | Unfoldable arguments give nothing: `f"pkg.{x}"`, `f"pkg.mod"`, `"pkg." + x`, `"pkg" ".mod"`, `"pkg\x2emod"`, a cross-file `OTHER`, a call result | `syntax::plain_string`, `Bodies::fold` | Make `plain_string` delegate to `string_literal` (the f-strings then fold to `pkg.{x}` / `pkg.mod`) |
| B8 | An `import_module` that is not importlib's gives nothing: `from mylib import import_module`, and `from importlib import import_module` followed by an unconditional `from mylib import import_module` at module level | `FileModel::note_importlib` (`None` removes), `binding_is_importlib` | Make `note_importlib(local, None, true)` a no-op, so the stale alias folds |

L1 and B1 to B8 land in one test binary (the crate's lib tests), so the
verify slice can build all 9 controls in one throwaway worktree in sequence.

## 7. Must-confirm items (for the implementer)

- **M1.** Confirm the tree-sitter-python node kinds used above against the
  grammar version in `Cargo.lock`, with a one-line parse in a unit test,
  before relying on them: `keyword_argument` (`name`/`value` fields),
  `argument_list`, `integer`, `string` with
  `string_start`/`string_content`/`escape_sequence`/`interpolation`
  children, `concatenated_string`, `augmented_assignment`, `global_statement`,
  `relative_import`.
- **M2.** Owner decisions still open: D1a, D2, D5a (recommended options are
  marked) and M3. D3, D4, D6 and D7 are stated as decided.
- **M3.** (Open for the owner.) Accepted gap in D3: a module-level
  `for`/`with`/walrus rebinding of a constant's name does not poison it, so
  the stale literal could fold. That is a wrong edge, the one place this
  design can produce one. Alternative: poison any name that is bound at
  module level by anything other than a plain assignment. Benefit: no wrong
  edge. Risk: it needs a module-level pre-scan (about 20 lines, reusing
  `bind_locals_in`'s shape).
- **M4.** `import_module(name=...)` as a keyword: CPython accepts it.
  Include it (one extra branch) unless the owner prefers positional only.
- **M5.** No existing test asserts that a function-local import has no edge,
  and the conformance fixture (`plugins/python/conformance/project`) has no
  function-local imports and no `importlib`. The `get_dependencies`
  expectations in `conformance/expect.toml` should therefore be unchanged.
  The verify slice confirms this by running the python conformance section,
  not by assumption.
- **M6.** No core change is needed: core consumes these as ordinary
  `IMPORTS` edges onto ordinary placeholders. g-mesh `find_callers` on
  `Emitter::import_edge` (id `6ce5268510897b0a2e23a4d1563acddb`) returns only
  `Declarer::import_edge`. Its `untypedReceiverCalls` hint points at
  `plugins/sdk/src/graph.rs`, which is the SDK's own `FileGraph::import_edge`,
  a different function.
- **M7.** Step 2's refactor of `Declarer` must keep the current output byte
  for byte at module level. Edge order may change, but the emitted *set* must
  not. The existing import tests pin that set.

## 8. Follow-up (separate task)

Resolve uses of names that a function-local import binds. Example: `def f():
from pkg.mod import helper; helper()` should give `CALLS f -> pending_symbol
pkg.mod::helper`, and `def g(): import pkg.mod as m; m.run()` should give
`CALLS g -> pkg.mod::run`. This needs per-frame `Import` values in `Scopes`
(today a frame holds only names) and a frame-aware `resolve_bare` /
`qualifier_of`. If that task adopts per-frame `Import`, D1a option B should
be reconsidered there.

## Owner decisions (2026-10-12)

- Scope: "Расширить GM-510" — function-local static and dynamic imports emit IMPORTS edges (D7); resolving uses of names they bind is a follow-up.
- D1a: "Отдельная таблица (Recommended)" — side map in `FileModel`.
- D2: "Литерал + константа (Recommended)" — plain string literal and same-file constant only.
- D5a: "Два пути (Recommended)" — `__package__`/`__name__` through the relative path; literal/constant `package` resolved to an absolute import.
- M3: "Закрыть пробел (Recommended)" — any module-level binding of a name other than its single plain assignment (for/with/walrus included) poisons it.
- Controls: 9 accepted (the 9th pins the parameter-shadows-constant wrong-edge case).
