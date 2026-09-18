# g-mesh's Rust plugin

A Rust plugin built on `plugins/sdk`: a `Cargo.toml`/module-tree project
model (GM-285), a tree-sitter-rust extractor (GM-286), and a rust-analyzer
semantic tier behind the SDK's LSP bridge (GM-290). The design is
`docs/architecture/multi-language-plugins.md` ("Rust plugin"); the reasoning
behind each decision is in the module docs named below, and this file is the
summary plus the one thing a *user* of the index has to know: what it does
not see.

```
src/project/     the workspace model - crates, module tree, container keys
src/extractor/   one file's bytes -> nodes, edges, open sites
  keys.rs        container keys, qualifiedName, visibility
  scope.rs       the lexical binding stack
  syntax.rs      tree-sitter helpers: paths, doc comments, signatures
  decls.rs       pass 1 - declarations and `use`
  bodies.rs      pass 2 - calls, references, implementations, open sites
  emit.rs        the graph, de-duplicated, in the wire's own units
src/semantic.rs  finding rust-analyzer, and handing it to the SDK's bridge
```

## The two tiers

The structural tier always runs and never fails. The semantic tier runs when
core asks for it and rust-analyzer is there.

```bash
rustup component add rust-analyzer    # what the semantic tier needs
```

Without it the plugin logs one line, answers empty semantic diffs, and serves
the structural graph unchanged - and because those diffs are reported
*incomplete*, `language_state.semanticPassAt` stays unset and the MCP
instructions keep listing Rust's receiver-call gap. Installing the component
and restarting the daemon is enough to get the pass.

A `rust-analyzer` on `PATH` is not proof of one: `~/.cargo/bin/rust-analyzer`
is a rustup proxy that exists for every component rustup knows, installed or
not, and exits 1 when the one behind it is missing. `src/semantic.rs` tries
the `PATH` name and then `rustup which`, and probes each with `--version`
before believing it.

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

These are what the *structural* tier does not see. Each is a question only
name resolution can answer, and the rust-analyzer tier answers 1 and 3 by
asking a compiler; 2 it does not, and nothing will. Until a semantic pass has
landed, and on any machine without rust-analyzer, nothing in the index claims
otherwise: a missing edge is missing, never guessed.
`conformance/project/crates/alpha/src/gaps.rs` has all three written beside
code that has them.

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

- **Receiver calls (`x.m()`) produce no *structural* edge.** They are what
  open sites exist for, and the semantic tier answers them: `plugin.toml`
  declares `receiver_calls_structural = "unresolved"` with
  `receiver_calls = "resolved"`, which is what makes the MCP instructions
  list the gap until a semantic pass has landed for this language and stop
  afterwards. `self.m()` inside an `impl` *is* resolved structurally, to the
  impl type's own method.
- **An `impl Trait for T` whose trait arrives through a glob import** gets no
  structural edge and not even an open site - a bare type name that is
  neither declared nor imported by item resolves to nothing, deliberately, so
  that `Vec` and `String` do not become questions. The semantic tier finds it
  from the other end, by asking rust-analyzer which types implement the
  trait; `conformance/project/crates/beta/src/main.rs` is that case.

  GM-314 put numbers on both halves of that. The exclusion is worth keeping:
  on tokio-rs/tokio, recording every unresolved bare name adds 5,858 questions
  to the 27,750 already asked, and the type histogram is `Option` 753,
  `Result` 474, `Sized` 309, `Send` 287, `Vec` 233, `Box` 216 - the list this
  bullet names, measured. But Rust is where the *loss* is real, unlike Python:
  **2,052 of g-mesh's own 5,468 excluded names (37.5%) sit under a glob**,
  almost all of it `mod tests { use super::*; }`, which is why `Diff`,
  `Connection` and `SymbolQueryParams` come back unresolved in this
  repository's own tree. The recommendation there is not a wider question list
  but a structural one: the extractor already knows the module has a glob and
  which container it names, so a `name`-keyed placeholder into that container
  costs the bridge nothing and fails to a missing edge. What blocks it is two
  globs in scope at once, which would make the placeholder ambiguous.
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
chain, `pub(crate)` visibility), references, `impl Trait for T` via
`find_implementations`, imports (a glob and a container-scoped named
re-export), and definition. GM-286's original five acceptance-criteria
entries are folded in rather than duplicated.

Five of its entries are tagged `tier = "semantic"` (GM-290) and are run three
ways by `tests/conformance.rs`, which is what makes any of the runs mean
something:

- with the manifest this plugin ships, where all fifteen entries pass;
- with 3.2.0's own manifest (`semantic_pass = false`) and
  `--skip-semantic-expectations`, where those five report `Skip` and every
  structural entry keeps passing - this plugin binary answering the release
  before it;
- and once more without the skip flag, where those five must **fail**. That
  third run is the discrimination: it is how the semantic entries are known
  to be measuring the rust-analyzer tier rather than being answered by
  something else.

A fourth run covers the degradation itself - the shipped manifest with the
server command pointing at nothing - and asserts the log, not the
expectations: with `semantic_pass = true` and no engine the whole-project pass
is reported incomplete, which the kit reads as a session failure and after
which it judges no expectations at all. That one failing check is the kit
telling the truth about the environment, and the test pins it to exactly that
one so nothing else can hide behind it.

The receiver-call gap's own assertion moved with them. Through 3.2.0 it lived
in this file as a caller set that deliberately *omitted* the non-resolving
call site; it is now the same entry listing all three callers and requiring
the semantic tier to produce the third. The structural half - that `x.m()`
emits one open site and no edge - is pinned by
`src/extractor/tests.rs::a_receiver_call_produces_no_edge_and_one_open_site`,
which needs no toolchain at all.
