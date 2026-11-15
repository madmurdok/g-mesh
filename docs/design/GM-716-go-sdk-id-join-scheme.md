# GM-716: Go plugin and SDK id join schemes

Source: docs/review/4.30/README.md, side discovery 8.2 and R-21.

## Today

- `plugins/go/ids.go` `nodeIDFor`/`edgeIDFor`: sha256 of
  `"node <path> <kind> <qn> <nativeKind>"` / `"edge <from> <kind> <to>[ <ord>]"`,
  **space**-joined, first 32 hex chars. This is the scheme of the deleted Node
  TS plugin (`git show 874cc752^:plugins/typescript/src/extract.ts`, lines 517-540).
- `plugins/sdk/src/ids.rs` `digest`: the same fields joined by **NUL**. Used by
  the TS, Rust and Python plugins (g-mesh `find_callers ids::node_id` /
  `ids::edge_id`: callers only in `plugins/{sdk,typescript,rust,python}`, none in core).
- Recomputed here (python `hashlib`), tuple `(main.go, File, main.go, "")`:
  space `1453099e1c11aef90ca118eb6d140fe1` (the value pinned in `ids_test.go`),
  NUL `1c06532b9ba18d30da653a043b3ed0e1`. Edge `(n1, CALLS, n2, 2)`: space
  `1deb655be169c9fcfe9e081b775c7117` (pinned), NUL `c25c95159195db66a611a5b42f125c09`.
- Core hashes two id families of its own: `graph::containers::container_id`
  (`"container <lang>\0<key>"`) and `defines_edge_id`
  (`"edge <container> DEFINES <member>"`, space-joined).

False or stale comments (more than the three the review lists):

| Where | Claim | Fact |
|---|---|---|
| `plugins/sdk/src/ids.rs:19` | "plugins/go must match this" | Go is space-joined |
| `plugins/go/ids.go:9` | "mirrors node_id/edge_id in plugins/sdk/src/ids.rs" | it mirrors the old Node TS plugin |
| `plugins/go/ids_test.go:30-31` | "plugins/sdk/src/ids.rs pins the same scheme" | it pins NUL |
| `plugins/sdk/tests/id_scheme.rs:1-28` | values "recorded" from the Node TS plugin | `5ff9a337...` is the NUL form; Node TS gave the space form `30feda4f...` for the same tuple |
| `core/src/graph/containers.rs:243-244` | DEFINES id "exactly as edgeIdFor in extract.ts" | true of the deleted TS plugin, not of today's SDK |

## Finding: where plugin ids meet

Traced with g-mesh (`find_callers nodeIDFor`, `find_callers edgeIDFor`,
`find_callers ids::node_id`, `find_callers ids::edge_id`,
`find_callers container_id`, `search_code` for id recomputation and symbol links):

- **Go id producers** are all inside `plugins/go` (extract.go, uses.go,
  semantic.go, open_sites.go). The plugin looks its own ids up again
  (`knownID(nodeIDFor(...))` in uses.go) - always the same function, so consistent.
- **Core never recomputes a plugin id.** Ids are opaque row keys for upsert,
  `deleteNodeIds` and edge endpoints. `graph::symbol_links` and
  `graph::imports` match placeholders by `placeholder_targets` rows
  (scope + name/qualifiedName), not by hashing. Containers use core's own
  `container_id`, disjoint from both plugin families by its `container ` marker.
- **No cross-plugin comparison.** One file is owned by one plugin, so a Go id
  and an SDK id are never compared or deduplicated against each other.
- **Conformance** (`cli/plugin_check/checks.rs`, `id-stability.*`) checks a
  plugin against itself (bulk vs bulk, bulk vs incremental), never against a
  reference scheme.
- **Persisted state:** `index.db` (wiped on generation change, below); the
  embedding cache is keyed by `(model_id, text_hash)`, not node id;
  `embed_eval` caches vectors under `node_ids_sha256` (a changed id only
  forces a rebuild of that cache); eval queries name symbols by qualifiedName.

**Verdict: cosmetic today.** No wrong edge, duplicate node or failed lookup
follows from the difference. The only behavioural property in play is
injectivity: space-joining is ambiguous in principle (`("a b","c")` and
`("a","b c")` hash the same), but in Go `kind` and `nativeKind` come from
closed vocabularies with no spaces and only `path` and a semantic placeholder's
`qn` (`container.name@<scope file>`) can carry a space, so a collision needs
file names that embed `" <Kind> "` plus a matching `nativeKind`. None was
constructible from what the plugin emits. What would prove otherwise: a
property test over a fixture with space-bearing paths asserting that distinct
`(path, kind, qn, nativeKind)` tuples emitted by one bulk run give distinct ids.

The real cost is the contract: the SDK spec says every plugin must match it,
three tests say they pin it, and none does for Go.

## Options

### (a) Migrate Go to NUL

- **Change:** `hashID` over NUL-joined fields in `ids.go` (about 10 lines);
  the three literal ids in `ids_test.go`; the comment fixes in the table above.
  No other Go or core test pins a literal id (grep for 32-hex literals:
  only `ids_test.go` and a v1 legacy fixture that is not Go).
- **Reindex trigger already exists:** `daemon::registry::indexer_version`
  digests every discovered plugin's fingerprint (the Go binary's bytes,
  `core/tests/it/go_plugin_fingerprint.rs`), and `schema::ensure_current`
  wipes and rebuilds on a generation change. A rebuilt Go plugin rebuilds every
  index on the next daemon start; a running daemon with the old binary is
  reported not current (`daemon::build_stamp`). No schema bump needed. The TS
  move to the SDK (874cc752) changed every TS id the same way.
- **Benefit:** one scheme, spec and tests become true; R-21's corpus pins Go
  directly; space ambiguity gone.
- **Risk:** one full reindex of each Go project (a cost every Go plugin
  change already pays); symbol ids held by an MCP client across the restart
  miss once; a Go `embed_eval` vector cache rebuilds. If the fingerprint path
  were ever bypassed (stale generation), incremental deletes by new ids would
  leave old rows behind as duplicates - the verify slice pins that it is not.
- **Size:** S.

### (b) Keep Go space-joined, document it

- **Change:** rewrite the comments to say Go uses the legacy Node TS scheme;
  add a "Go exception" paragraph to the `ids.rs` spec.
- **Benefit:** no id changes, no reindex.
- **Risk:** two specs forever; every future non-Rust plugin must pick one;
  R-21's golden corpus needs a second, space-form column generated by a Rust
  helper that exists only for Go, so the corpus pins Go to a scheme nothing
  else uses.
- **Size:** XS.

### How R-21's golden corpus pins each

R-21 adds Rust-generated `ids.json` (tuples -> ids from `sdk::ids`) read by a
Go test. Under (a) the Go test asserts `nodeIDFor`/`edgeIDFor` equal the file's
ids: any drift on either side fails at the same commit. Under (b) it can only
assert the tuples against a separately generated space column, which pins Go to
itself, not to the SDK.

## Recommendation

**(a) Migrate Go to NUL**, inside R-21 (GM-711) or right before it, so the
corpus lands with one scheme. The reindex is automatic and the change is small.
Leave core's `defines_edge_id` as is (core-owned, disjoint `from`), fix its comment.

## Draft slicing of the follow-up

1. **Code** (sonnet, edit map above): `ids.go` NUL join + comments in
   `ids.go`, `ids.rs`, `id_scheme.rs`, `containers.rs`.
2. **Tests:** replace `ids_test.go` literals with the NUL values
   (`1c06532b9ba18d30da653a043b3ed0e1`, `c25c9515...` for the bound edge,
   recomputed by the test author from `sdk::ids`, not copied from here),
   or read R-21's `ids.json`; add the distinct-tuple property test over a
   space-bearing fixture. Pins: Go equals SDK byte-for-byte.
3. **Verify:** `go test ./...` in `plugins/go`; `g-mesh plugin-check` on the Go
   plugin (`id-stability.*` still pass after the scheme change); nextest
   `go_plugin_fingerprint` and `registry` generation tests. Controls: revert
   the join to space -> `ids_test.go` fails; drop the separator between two
   fields -> the corpus test fails.
