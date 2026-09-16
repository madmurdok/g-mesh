# g-mesh's Python plugin

A structural Python plugin built on `plugins/sdk`: a
`pyproject.toml`/src-layout/namespace-package project model (GM-295) and a
tree-sitter-python extractor (GM-296). The design is
`docs/architecture/multi-language-plugins.md` ("Python plugin"); the reasoning
behind each decision is in the module docs named below, and this file is the
summary plus the one thing a *user* of the index has to know: what it does not
see.

```
src/project/     the package model - roots, container keys, namespace packages, stubs
src/extractor/   one file's bytes -> nodes, edges, open sites
  keys.rs        container keys, qualifiedName, visibility, relative imports
  scope.rs       the lexical binding stack, with Python's own scoping rules
  syntax.rs      tree-sitter helpers: dotted names, docstrings, signatures
  decls.rs       pass 1 - declarations, imports, __all__
  bodies.rs      pass 2 - calls, references, class bases, open sites
  emit.rs        the graph, de-duplicated, in the wire's own units
```

## What it indexes

| Python | node `kind` | `nativeKind` | name within its module |
|---|---|---|---|
| `def f` at module level | `Function` | `function` | `f` |
| `def inner` inside a `def` | `Function` | `function` | `outer.inner` |
| `def m` inside a `class` | `Function` | `method` | `C.m` |
| `class C` | `Type` | `class` | `C` |
| `class Inner` inside a `class` | `Type` | `class` | `Outer.Inner` |
| a module-level assignment | `Variable` | `variable` | `NAME` |
| the module itself | `Module` | `module` | `pkg.mod` |
| a package's `__init__.py` | `Module` | `package` | `pkg` |

A node's `qualifiedName` is its **lexical path within its own module** - which
is also CPython's `__qualname__`, minus the `<locals>` marker a nested function
gets there. Its container is the module's dotted key (`pkg.mod`), or the
package's own key (`pkg`) for anything declared in `pkg/__init__.py`.

Two details are worth stating plainly because they are decisions, not
conveniences:

- **A method of a nested class is `Outer.Inner.m`, not `Inner.m`.** An id is
  `(filePath, kind, qualifiedName, nativeKind)` and has to be injective: one
  file may hold `class Request: class Inner:` and `class Response: class
  Inner:`, and the short form collapses their methods into one node.
- **`async def` is not its own `nativeKind`.** `async` lives in the signature.
  Putting it in the id would delete a symbol and add a stranger - taking every
  inbound edge with it - on an edit that renamed nothing.

**The module announces itself.** In addition to whatever `pkg/mod.py` declares,
the plugin emits one node whose `qualifiedName` is `pkg.mod`, whose `name` is
`mod`, and whose `container` is the **parent** package `pkg`. Python has no
`mod child;` statement to hang that membership on, and without it `from pkg
import mod` has nothing in container `pkg` named `mod` to find. The same node
is what keeps `graph::containers::parent_chain` gap-free across a PEP 420
namespace package. A module or package with no parent (a script at the project
root) emits none, because there is no container above it to be a member of.

**Everything is `public`.** Python enforces no access control: `from mod import
_helper` works, and so does `mod._helper`. A leading underscore is a convention
for human readers, and modelling it as `file` visibility would make core
*refuse* links for imports that really happen. `__all__` is not visibility
either - it controls `from mod import *` and nothing else - so it is modelled
as a re-export.

Edges: `DEFINES`/`EXPORTS` from the file; `IMPORTS` from the file onto the
container each import reads from (every form, not only `*`); `CALLS` and
`REFERENCES` onto a declaration of the same file or onto a placeholder core
links; `SUPERTYPE_OF` from a class to each of its bases.

## What it does not see

These are structural gaps, not bugs. Each is a question only name resolution or
a running interpreter can answer. Until a semantic tier exists (a pyright
bridge is future work, not scheduled), nothing in the index claims otherwise: a
missing edge is missing, never guessed.
`conformance/project/pkg/gaps.py` has all five written beside code that has
them, and `pkg/mod.py` carries the receiver-call one.

None of them is named in `conformance/expect.toml`: an expectation over a gap
would be an assertion that it stays a gap forever, and these are exactly what a
semantic tier is expected to close.

1. **Dynamic attributes.** `setattr(obj, name, value)`, `__getattr__`,
   `__getattribute__`, a class built by `type(...)` or by a metaclass: an
   attribute that exists only at runtime has no declaration to point at, so
   nothing references it and nothing calls it.
2. **Monkey patching.** `mod.helper = my_helper`, or `Klass.method =
   something_else`, rebinds a name after import. The index shows the
   declaration as written; the replacement is invisible, and callers still
   appear to call the original.
3. **Conditional imports.** `if TYPE_CHECKING:`, `try: import fast / except
   ImportError: import slow`, a version check. **Every branch is indexed and no
   predicate is evaluated** - the same rule the Rust plugin applies to `cfg` -
   so `get_dependencies` reports a file as depending on *both* alternatives.
   The same applies to a conditional `def`: two branches declaring one name are
   one node (the first in source order), because they are one id.
4. **Star-import name sets that depend on runtime.** `from mod import *` is
   recorded as a container import and a `*` re-export, which is enough for core
   to walk the chain. What is *not* known is which names it actually binds:
   that depends on the other module's `__all__`, which may be built at import
   time. So a bare call to a name that arrived through a star import resolves
   to nothing. `__all__` itself is read only when it is a plain list or tuple
   of string literals; anything computed (`__all__ = [n for n in dir()]`,
   `__all__ += other.__all__`) republishes nothing here.
5. **Decorators that replace a function.** `@functools.wraps`-style decorators
   keep the name pointing at a wrapper, and a decorator may return something
   else entirely (`@property` returns a descriptor, `@singledispatch` returns a
   dispatcher). The decorator is recorded in the signature and as a
   `REFERENCES` edge, and the decorated declaration is indexed as written - but
   a call to it reaches the wrapper at runtime, which the index does not model.

Five smaller ones, for completeness:

- **Receiver calls (`obj.method()`) produce no edge.** This is the design's own
  documented gap for Python, declared in `plugin.toml`
  (`receiver_calls = "unresolved"`) so the MCP instructions say so, and it is
  what open sites exist for. The one exception is a call through a method's
  **first parameter** (`self.render()`, `cls.build()`), which resolves to that
  class's own declaration - read structurally, from the parameter's position,
  never from the name `self`. A `@staticmethod` has no instance parameter and
  is excluded; a member the class inherits rather than declares here is not
  guessed at the base.
- **A bare unresolved call is not an open site.** `print`, `len`, `open`,
  `isinstance` are the most common calls in any Python file and no engine's
  answer for them is a node this index holds, so recording them would make the
  open-site set mostly builtins. The cost is that a star-imported name shares
  their fate (gap 4 above).
- **A module's container holds its methods too, so a name shared by a
  top-level function and a method is ambiguous.** Every declaration of a file
  carries the module's container key, including `C.m` - so a lookup addressed
  at container `pkg.mod` for the bare name `m` sees both the free `def m` and
  `C.m`. Core refuses an ambiguous `name` key rather than picking, which is
  the safe direction: a missing edge, not a wrong one. (This is the same
  arrangement `plugins/rust` uses, where `Point::new` and a free `fn new`
  share their module's container.)
- **Class attributes are not nodes.** `class C: attr = 1` declares an
  attribute, and the design doc's "member-level privacy is not modelled" is the
  same boundary - nothing in the tool surface addresses one. Module-level
  assignments *are* nodes, because they are exactly what `from mod import
  CONSTANT` addresses.
- **A `.pyi` stub contributes its `File` node and nothing else.** Its
  declarations would land in the same container as its `.py` twin's, which
  without a multi-file symbol model (`DECLARATION_OF`, designed but not built)
  would make `from pkg import mod` ambiguous and `from pkg.mod import greet`
  find two candidates - so core would rightly refuse both. Skipping is the
  missing-answer side of that trade. See `src/project/mod.rs`'s Decision 6.

## Running it

```bash
cargo test -p g-mesh-plugin-python     # unit tests, plus the conformance kit
cargo build -p g-mesh --bin g-mesh     # the kit needs core's binary
g-mesh plugins check plugins/python \
  --fixture plugins/python/conformance/project \
  --expect  plugins/python/conformance/expect.toml
```

`tests/conformance.rs` runs exactly that from `cargo test`, and asserts each
check's verdict by name so that a check which starts *skipping* fails the suite
instead of quietly shrinking it. Exactly one check skips
(`capabilities.semantic-engine-lazy`, because `semantic_pass = false`); the
other fourteen pass.

`conformance/project` is a small package tree carrying one of each edge shape:
a package whose `__init__` re-exports through `__all__`, a module with every
import form, a subpackage whose module reaches two levels up with `..`, a PEP
420 namespace package, a `.pyi` stub beside its module, a `gaps.py` holding the
five structural gaps above, and a receiver call whose *absence* from the
expected caller set is itself an assertion.
