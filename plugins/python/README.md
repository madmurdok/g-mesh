# g-mesh's Python plugin

A Python plugin built on `plugins/sdk`: a
`pyproject.toml`/src-layout/namespace-package project model (GM-295), a
tree-sitter-python extractor (GM-296), and a pyright tier over the SDK's
generic `LspBridge` (GM-299). The design is
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
src/semantic.rs  finding pyright, configuring it, handing it to LspBridge
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

## The semantic tier (pyright)

The plugin runs `pyright-langserver` behind the SDK's `LspBridge` and asks it
one question per **open site** - which for Python means one question per
receiver call, `obj.method()`, and nothing else. An answer becomes a `CALLS`
edge with `source: semantic` and `engine: pyright`; no answer becomes no edge.

It is found in three places, in order, and each is *probed* before it is used:
`PATH`, then the indexed project's own `node_modules/.bin`, then
`npx --yes --package pyright pyright-langserver`. The probe runs
`pyright --version` - the CLI twin from the same npm package - because
`pyright-langserver --version` has no such flag and exits 1.

The `npx` branch is last, and there are two reasons rather than one. It is the
only one that can **reach the network**: on a machine with no pyright anywhere
it fetches the package into npm's `_npx` cache, which cost 4.94s measured here
and costs more on a slow link. It is bounded rather than trusted (60s, then the
child is killed). And it is measurably more expensive even once cached: on the
conformance fixture the whole-project pass goes from ~3.4s to 4.13s, and peak
process-tree RSS from 131 MiB to **204 MiB**, because `npm` stays resident
beside the server it launched - 74 MiB of launcher, charged to this plugin by
`[plugin] memoryLimitMb` like everything else in its tree. Install pyright in
the project or globally and the branch is never reached; the log line always
names which of the three answered.

**No pyright at all is not an error**: the plugin logs one line, answers every
pass with an empty *incomplete* diff, and the structural index is untouched.
`semanticPassAt` stays unset, so the receiver gap below keeps being listed
until a pyright is actually there.

If the project has a virtual environment (`<root>/.venv` or `<root>/venv`),
its interpreter is passed to pyright as `python.pythonPath`, so third-party
imports resolve against the environment the code actually runs in. `$VIRTUAL_ENV`
is deliberately **not** read: it describes whatever shell started the daemon,
which is as likely to be another project's environment as this one's.

### What a resolved receiver call resolves *to*

`plugin.toml` says `receiver_calls = "resolved"`, and that is a statement
about the *tier*, not a promise about which code runs. pyright resolves
`x.m()` against the **static type of `x`** — the annotation, or what it can
infer — so a call through a parameter annotated with a base class lands on
the base's declaration, never on the override that executes:

```python
def through_a_base_annotation(obj: Base) -> str:
    return obj.describe()          # -> Base.describe, whatever obj really is
```

Measured on `conformance/project`, with pyright resolved and the
whole-project pass complete:

| query | answer |
| --- | --- |
| `find_callers("Base.describe")` | `{call_through_a_class, module_alias_is_bound, through_a_base_annotation}` |
| `find_callers("Deep.describe")` | `{through_a_subclass}` — no `through_a_base_annotation` |
| `find_callers("Greeter.describe")` | `{}` — empty |
| `find_implementations("Base")` | `{Greeter, Deep}` |

The third row is the one to read twice. `Greeter` overrides `describe`, so
`through_a_base_annotation` runs `Greeter.describe` for every `Greeter` it is
handed — and the page that asks who calls it comes back empty, with
`hasMore: false`, `allUnresolved: false` and no `provenance` block, because
nothing was absent: the tier ran and filed that call under the annotation.
Every signal says complete, and it is complete for "who *names* this
declaration", which is a different question from "what runs this code".

**Where the missing calls went, and why this plugin will not count them.**
They are on the base's page, exactly — so `find_implementations` is the
crossing, and `conformance/expect.toml` asserts all four rows above as exact
sets. What this plugin never reports is a *number*: it knows `Base` has two
subclasses, it does not know how many of `through_a_base_annotation`'s
callers pass a `Greeter`, and that is unknowable by construction. A count an
agent acts on is worse than silence, which is the rule
`core/src/mcp/provenance.rs` already states for an absent tier.

This is not Python-specific. `plugins/go` and `plugins/rust` have the same
section, saying the same thing in their own syntax, because it is what static
resolution means rather than what one engine does — Go's interface method and
Rust's trait declaration are this base class, spelled differently.
`plugins/typescript` is the odd one out and says so: it declares
`receiver_calls = "unresolved"` for both tiers and emits no edge for `x.m()`
at all. A session is told the consequence once, by
`core/src/mcp/instructions.rs`'s `P4_STATIC_RECEIVER`.

What follows is the part that *is* Python-specific, and it is the larger one.

### What `resolved: true` covers for Python, and what it does not

This is the part to read before trusting a Python caller list, and it is
deliberately not written to sound like Go's. Everything in the section above
applies here word for word; what does not carry over is how much pyright can
infer in the first place. **Python is the language where `resolved: true`
covers the least of any plugin here**, and the reason is not pyright's: it is
that Python decides at run time what Go and Rust decide at compile time.

pyright answers a receiver call when it can infer the receiver's type:

- a local whose type comes from its initializer (`g = Greeter(); g.render()`);
- a parameter with an annotation (`def f(obj: Base): obj.describe()`);
- a receiver that is a call result (`base_module.Base().describe()`);
- and it answers with the **override**, not the base's declaration, when the
  receiver is of a subclass type.

It answers *nothing at all* for every one of these, with pyright running:

1. **An unannotated parameter.** `def f(obj): obj.describe()` is the most
   common shape in Python that carries no type hints, and its type is
   `Unknown`. `conformance/project/pkg/mod.py`'s `on_an_unknown_receiver` is
   exactly this, and its **absence** from `Base.describe`'s expected caller set
   in `conformance/expect.toml` is asserted rather than merely described.
2. **Dynamic dispatch in general.** Which `describe` runs is a fact about the
   object at run time; the best a checker can do is name the statically visible
   declaration. This one is not really a case of "answers nothing at all" —
   it answers, and answers the base — so it is the section above rather than
   an item in this list, and is kept here only so a reader working down the
   list is not left thinking it was forgotten.
3. **Monkey patching.** `Klass.method = something_else` after import. The index
   shows the declaration as written and every caller still appears to call it.
4. **Attributes created at run time.** `setattr`, `__getattr__`,
   `__getattribute__`, a class built by `type(...)` or a metaclass, anything
   populated from a registry or a config file. There is no declaration for an
   edge to land on, so nothing points at it and nothing ever will.
5. **Conditional imports.** Every branch is indexed and no predicate is
   evaluated - pyright does not change this, because the plugin, not pyright,
   decides what is indexed.
6. **Star-import name sets computed at run time.** `from mod import *` where
   the other module's `__all__` is built at import time.
7. **Decorators that replace a function.** `@functools.wraps`, `@property`,
   `@singledispatch`: callers reach the wrapper, the index shows the decorated
   declaration.

There is one more, and it is about this plugin's *architecture* rather than
about Python: **pyright resolves things the bridge never asks it about.** The
bridge asks one question per open site, and the structural tier deliberately
records an open site only for a receiver call - a bare name that resolves to
nothing (a star-imported name, a builtin) is not one, because recording those
would make the open-site set mostly builtins. So a name that arrived through
`from mod import *` stays unresolved *even though* pyright resolves it happily
when asked directly. `conformance/project/pkg/dynamic.py` is that case written
out: `class Megaphone(Speaker)` where `Speaker` came from a star import.
pyright answers `textDocument/definition` there with `pkg/base.py`'s `Speaker`;
nothing in this plugin asks.

And one that is pyright's own: **`find_implementations` is not helped at all.**
pyright implements no `textDocument/implementation` - it advertises no
`implementationProvider` and answers the request with JSON-RPC error -32601 -
so the manifest's `implementation_kinds` is empty and the implementation sweep
that gives the Rust plugin its cross-crate answers does not run for Python.
`find_implementations` on a Python class returns exactly what the structural
tier's `SUPERTYPE_OF` edges say: subclasses whose base was imported *by item*,
in any module. A subclass whose base arrived through a star import is invisible
to it, with or without pyright.

## What it does not see

These are gaps, not bugs. Each is a question only name resolution or a running
interpreter can answer, and the five below are the ones the *structural* tier
leaves open; the section above says which of them pyright closes (one: part of
the receiver-call gap) and which it does not (the rest).
`conformance/project/pkg/gaps.py` has all five written beside code that has
them, `pkg/mod.py` carries the receiver-call one, and `pkg/callers.py` carries
the three shapes pyright does answer.

Only the receiver-call cases are named in `conformance/expect.toml`, and only
since GM-299 made them answerable: an expectation over a gap would be an
assertion that it stays a gap forever.

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

- **Receiver calls (`obj.method()`) produce no *structural* edge.** They are
  what open sites exist for, and since GM-299 the pyright tier answers the ones
  whose receiver it can type - see "What `resolved: true` covers" above for the
  ones it cannot, which is why `plugin.toml` keeps
  `receiver_calls_structural = "unresolved"` beside
  `receiver_calls = "resolved"`. The one exception the *structural* tier
  resolves by itself is a call through a method's **first parameter**
  (`self.render()`, `cls.build()`), which resolves to that class's own
  declaration - read structurally, from the parameter's position, never from
  the name `self`. A `@staticmethod` has no instance parameter and is excluded;
  a member the class inherits rather than declares here is not guessed at the
  base.
- **A bare unresolved call is not an open site.** `print`, `len`, `open`,
  `isinstance` are the most common calls in any Python file and no engine's
  answer for them is a node this index holds, so recording them would make the
  open-site set mostly builtins. The cost is that a star-imported name shares
  their fate (gap 4 above).

  That used to be a prediction. GM-314 measured it, and the exclusion stands
  because the numbers are not close. On django/django (2,932 files) recording
  every unresolved bare name adds **23,320** questions to the 87,832 this
  plugin already asks - and 23,022 of them, 98.7%, are builtins and dunders
  (`str` 2,247, `len` 2,137, `super` 1,829). Of that whole added set, **one**
  names something this index actually holds. Narrowing to positions where a
  declaration is expected - base class, decorator, annotation - drops 94% of
  the volume and keeps none of the value: Django's 1,215 decorator sites are
  100% builtins, and pallets/flask's 684 annotation sites are 100% builtins.
  So gap 4's `Speaker` is real, stays unanswered, and paying 23,320 questions
  to recover it is not a trade worth making. The shape that *might* be worth
  it is Rust's glob-scope names, and structurally rather than as a semantic
  question - the design doc's GM-314 notes carry that argument.
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
npm install pyright --prefix plugins/python   # a test dependency, gitignored
cargo build -p g-mesh --bin g-mesh            # the kit needs core's binary
cargo test -p g-mesh-plugin-python            # unit tests, plus the conformance kit
g-mesh plugins check plugins/python \
  --fixture plugins/python/conformance/project \
  --expect  plugins/python/conformance/expect.toml
```

pyright is a **test dependency of this crate**, the way rust-analyzer is one of
`plugins/rust`: without it `cargo test -p g-mesh-plugin-python` fails naming
the install command rather than skipping, because a conformance check that
passes by not running is the failure the kit exists to remove. It goes in
`plugins/python/node_modules` and not in the fixture: the kit runs a plugin
against a scratch *copy* of the fixture and that copy skips symlinks, which is
all an npm `.bin` directory is.

`tests/conformance.rs` runs the kit in three configurations and asserts each
check's verdict by name, so that a check which starts *skipping* fails the
suite instead of quietly shrinking it:

1. the shipped manifest with a real pyright - 14 checks pass, one skips
   (`capabilities.semantic-pass-undeclared`, because `semantic_pass = true`),
   and all ten expectations pass;
2. the 3.4.0 manifest (`semantic_pass = false`) with
   `--skip-semantic-expectations` - the three semantic expectations report
   `Skip` and every structural one still passes, which is what "without
   pyright, the results are the 3.4.0 results" means as something a machine
   checks;
3. the same, *without* the skip flag - where those three must **fail**, which
   is what keeps the expectation file measuring the pyright tier rather than
   describing it.

`conformance/project` is a small package tree carrying one of each edge shape:
a package whose `__init__` re-exports through `__all__`, a module with every
import form, a subpackage whose module reaches two levels up with `..`, a PEP
420 namespace package, a `.pyi` stub beside its module, a `gaps.py` holding the
five structural gaps above, a `callers.py` holding the three receiver shapes
pyright answers, a receiver call whose *absence* from the expected caller set
is itself an assertion, and a `dynamic.py` whose star-imported base class no
tier here resolves.
