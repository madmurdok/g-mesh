# g-mesh's Go plugin

A single static Go binary that indexes Go for g-mesh, in two tiers:

| Tier | Engine | Needs a toolchain? | Answers |
|---|---|---|---|
| structural | `go/parser` (`extract.go`, `uses.go`, `scope.go`) | no | declarations, containers (import paths), visibility, `DEFINES`/`EXPORTS`/`IMPORTS`, bare and package-qualified calls and references |
| semantic | `go/types` via `golang.org/x/tools/go/packages` (`semantic.go`) | **yes** — `go` on `PATH` | receiver calls (`x.M()`), method promotion through embedding, interface dispatch, dot-imported names, implicit interface satisfaction (`SUPERTYPE_OF`) |

The design, and every decision either tier had to settle, is in
[`docs/architecture/multi-language-plugins.md`](../../docs/architecture/multi-language-plugins.md)
("Go plugin", plus the GM-279/GM-280/GM-281 implementation notes).

## Running it

```bash
go build -o g-mesh-plugin-go .        # core/build.rs does this for you
./g-mesh-plugin-go --bulk-index <project-root>   # NDJSON on stdout
./g-mesh-plugin-go <project-root>                # framed JSON-RPC control loop
```

Conformance, the way CI runs it:

```bash
go vet ./... && gofmt -l . && go test ./...
g-mesh plugins check plugins/go \
  --fixture plugins/go/conformance/project \
  --expect  plugins/go/conformance/expect.toml
```

A second CI job runs the same fixture and the same `expect.toml` with `go`
removed from `PATH` and `--skip-semantic-expectations` added:

```bash
g-mesh plugins check plugins/go \
  --fixture plugins/go/conformance/project \
  --expect  plugins/go/conformance/expect.toml \
  --skip-semantic-expectations
```

That flag answers every entry `expect.toml` tags `tier = "semantic"` with
`Skip` instead of running it - the four that need the `go/types` pass (three
`[[callers]]`, one `[[implementations]]`) - while the rest of the same file
still runs and still has to pass. One file, not two: see `expectations.rs`'s
module doc, decision 6, for why a reduced set is read out of the full file
rather than kept as a second one that could drift out of sync by hand.

## The semantic pass

Core sends `semanticPass` twice over: once per language after the cold-start
walk (whole project, `filePaths: []`) and once per file after each reparse.
The engine is **lazy** — it is not started by a bulk index, a `fileChanged`
or a `workspaceChanged`, only by a `semanticPass`. That is what
`g-mesh plugins check`'s `capabilities.semantic-engine-lazy` verifies, through
a marker the pass writes at the moment it first calls `packages.Load`.

Every answer is a `qualifiedName`-keyed placeholder addressed at the
*declaring* package's container, plus one `semantic` / `go-types` edge onto
it. `Close` as a bare name is worthless — it exists on dozens of types — so
the address is `Server.Close` inside `github.com/you/app/server`, which names
one declaration and nothing else.

## Out of scope, deliberately

These are gaps this plugin has and will report honestly rather than guess at.

- **Files excluded by the host's build constraints get no semantic upgrade.**
  `go/packages` type-checks for one `GOOS`/`GOARCH` pair — the host's — so on
  a Linux machine `sys_windows.go` and anything behind a `//go:build windows`
  line is never loaded. Such a file keeps its full *structural* graph (every
  alternative of a build-tagged file is parsed and indexed, and node ids carry
  the file path, so `sys_windows.go`'s and `sys_linux.go`'s `Platform` are two
  distinct nodes that never collapse). What it does not get is the semantic
  layer: the receiver calls inside it stay open, and a type declared only
  there gets no `SUPERTYPE_OF` edge. Cross-compiling the pass per target was
  rejected: it multiplies the cost of the pass by the number of targets to
  answer questions about code the machine asking is not building.
- **Dependents are refreshed on their next check.** A per-file `semanticPass`
  re-checks that file's own package and no other. An edge from a *different*
  package into a symbol the edit moved is therefore stale until that package
  is itself checked — the same staleness the TypeScript plugin accepts today,
  and the reason the whole-project pass exists. The same bound applies to
  `SUPERTYPE_OF`: a per-file pass only compares the types declared in that
  file against the interfaces its own load saw, so a type that starts
  satisfying an interface in another package is edged on the next
  whole-project pass.
- **Struct fields have no node.** `x.field` resolves exactly, and is then
  dropped: this plugin models declarations, not type members, so there is
  nothing for the edge to land on.
- **Generic types get one node, so no implements edge.** `Stack[T]` is one
  declaration here, while whether it satisfies an interface can depend on
  `T`. Rather than pick one instantiation, generic named types are skipped by
  the `types.Implements` pass.
- **Only this project's interfaces.** `SUPERTYPE_OF` edges are computed over
  interfaces declared in the project's own packages. `Server` implementing
  `io.Closer` produces no edge, because `io.Closer` is not a node in this
  index and an edge onto an address nothing declares could only go
  unresolved.
- **No toolchain, no semantic tier.** With no `go` on `PATH` (or a
  `packages.Load` that fails outright) the plugin logs one line and answers
  every `semanticPass` with an empty diff. The structural graph is complete
  and unaffected, Go's `language_state.semanticPassAt` is never set, and
  core's MCP instructions go on listing the receiver-call gap for Go — a
  partial index that says so, rather than a broken one.

## Dependencies

`golang.org/x/tools` (and its own `golang.org/x/mod` and `golang.org/x/sync`)
— required by the semantic tier and nothing else. The structural tier has no
dependency of its own: `.gitignore` matching, the `go.mod`/`go.work` reader
and the wire protocol are all hand-written (see `ignore.go` and
`workspace.go` for the trade-off each of those made), and they keep working
with the module cache cold and the network unreachable once `go mod download`
has run once.
