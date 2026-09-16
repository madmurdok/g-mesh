# g-mesh's Rust plugin

A structural Rust plugin built on `plugins/sdk`: a `Cargo.toml`/module-tree
project model (GM-285) and a tree-sitter-rust extractor (GM-286). The design
is `docs/architecture/multi-language-plugins.md` ("Rust plugin"); the
reasoning behind each decision is in the module docs named below, and this
file is the summary plus the one thing a *user* of the index has to know:
what it does not see.

```
src/project/     the workspace model - crates, module tree, container keys
src/extractor/   one file's bytes -> nodes, edges, open sites
  keys.rs        container keys, qualifiedName, visibility
  scope.rs       the lexical binding stack
  syntax.rs      tree-sitter helpers: paths, doc comments, signatures
  decls.rs       pass 1 - declarations and `use`
  bodies.rs      pass 2 - calls, references, implementations, open sites
  emit.rs        the graph, de-duplicated, in the wire's own units
```

## What it indexes

| Rust | node `kind` | `nativeKind` | name within its module |
|---|---|---|---|
| `fn f` | `Function` | `function` | `f` |
| `struct` / `enum` / `union` | `Type` | `struct` / `enum` / `union` | `T` |
| `type A = …` | `Type` | `type_alias` | `A` |
| `trait Tr` | `Type` | `trait` | `Tr` |
| `const` / `static` | `Variable` | `const` / `static` | `C` |
| `macro_rules! m` | `Function` | `macro` | `m` |
| `mod m` | `Module` | `module` | `m` |
| `impl T { fn m }` | `Function` | `method` | `T::m` |
| `impl Tr for T { fn m }` | `Function` | `trait_impl_method` | `<T as Tr>::m` |
| `trait Tr { fn m }` | `Function` | `trait_method` | `Tr::m` |

A node's `qualifiedName` is its path from its crate root **without** the
crate name (`parse::Lexer::next`), and its container is
`<crate>::<module path>`. Two details of that are worth stating because they
depart from the design doc's sketch, both for the same reason - an id is
`(filePath, kind, qualifiedName, nativeKind)` and has to be injective:

- The module path is in the name, so two inline modules of one file can each
  have a `helper`.
- A trait impl's method carries the trait, in Rust's own disambiguation
  syntax, so `impl Display for P` and `impl Debug for P` can each have a
  `fmt`.

Edges: `DEFINES`/`EXPORTS` from the file; `IMPORTS` from the file onto the
container each `use` reads from; `CALLS` and `REFERENCES` onto a declaration
of the same file or onto a placeholder core links; `SUPERTYPE_OF` from a type
to each trait it implements and from a trait to each of its supertraits.

## What it does not see

These are structural gaps, not bugs. Each is a question only name resolution
can answer, and each is answered by the rust-analyzer tier (GM-290) once that
lands. Until then nothing in the index claims otherwise: a missing edge is
missing, never guessed. `conformance/project/crates/alpha/src/gaps.rs`
has all three written beside code that has them.

1. **Macro-generated items.** Nothing inside a `macro_rules!` body is parsed,
   and nothing inside a macro invocation's token tree is either - the grammar
   hands back tokens rather than expressions there, so a call written inside
   `assert_eq!(…)` has no call node to find. An item a macro expands to is
   not in the index, and a call to it does not resolve.
2. **`cfg` alternatives.** No predicate is evaluated and every branch is
   indexed, so a caller may see a callee defined under an inactive `cfg`. Two
   branches that declare the same path in one file are the same id, and are
   merged into one node (the first in source order) rather than emitted
   twice.
3. **Trait dispatch through generics.** `fn f<S: Shape>(s: &S) { s.area() }`
   reaches whichever `Shape::area` the type argument selects at each call
   site. That is a receiver call, so it produces no edge at all - see below.

Two smaller ones, for completeness:

- **Receiver calls (`x.m()`) produce no edge.** This is the design's own
  documented gap for Rust, declared in `plugin.toml`
  (`receiver_calls = "unresolved"`) so the MCP instructions say so, and it is
  what open sites exist for. `self.m()` inside an `impl` *is* resolved, to
  the impl type's own method.
- **A path call through a `pub use` chain** (`a::b::f()` where `a::b`
  re-exports `f`) does not resolve: a type-qualified path is addressed by
  `qualifiedName`, and core walks re-export chains for `name` keys only. A
  `use a::b::f;` followed by `f()` does resolve, through the chain, because
  that is a `name` key.
- **Two `pub use` items publishing one declaration under two names** keep the
  first name only: both re-export nodes derive one id from one address.
- **A module that declares nothing but `pub use` gets no container node.**
  Core materializes a container from its *members*, and a re-export is
  deliberately not one (`graph::containers` excludes every placeholder kind
  from `memberCount`). So `use some::prelude::*;` keeps an unresolved
  `IMPORTS` edge and `get_dependencies` under-reports such a module. The
  re-export *walk* is unaffected - it reads a re-export node's own
  `container` column rather than the `containers` table - so names imported
  through a prelude still resolve, which is the case that matters. Nothing
  else is: a module with a submodule has that `mod` item as a member, so a
  `pub(crate)` parent chain can never gap here.

## Running it

```bash
cargo test -p g-mesh-plugin-rust      # unit tests, plus the conformance kit
cargo build -p g-mesh --bin g-mesh    # the kit needs core's binary
g-mesh plugins check plugins/rust \
  --fixture plugins/rust/conformance/project \
  --expect  plugins/rust/conformance/expect.toml
```

`tests/conformance.rs` runs exactly that from `cargo test`, and asserts each
check's verdict by name so that a check which starts *skipping* fails the
suite instead of quietly shrinking it. `conformance/project` is the same
fixture `tests/workspace_changed.rs` copies to a scratch dir and mutates -
GM-287 moved it here from `tests/fixtures/workspace` so CI's `plugins/*/
conformance` loop and `cargo test` share one copy rather than a second,
divergence-prone one (`plugins/typescript/conformance/`'s own layout,
GM-277's precedent).

The `--expect` file (GM-287) covers every category `g-mesh plugins check
--expect` can assert at least once: callers (a free function, a
module-qualified path call, a type-qualified path call paired with the
equivalent `Self::` call from inside its own impl, a `pub use` re-export
chain, `pub(crate)` visibility, and the receiver-call gap itself made
airtight - the same declaration called both a resolving and a non-resolving
way, so the non-resolving call site's absence from the expected set is a
real assertion rather than an omission nobody would notice), references,
`impl Trait for T` via `find_implementations`, imports (a glob and a
container-scoped named re-export), and definition. GM-286's original five
acceptance-criteria entries are folded in rather than duplicated.
