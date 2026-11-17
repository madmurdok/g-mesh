# GM-711: plugin contracts - Go golden corpora, wire control types, one `plugin.toml` schema, SDK emit layer (R-21, R-14, R-24, R-26)

Source findings: `docs/review/4.30/README.md` R-14, R-21, R-24, R-26. Prior decision:
`docs/design/GM-716-go-sdk-id-join-scheme.md`.
Read against `release-4.33.0` (= `main` + version bump). Line numbers are this branch at the
time of writing; each code slice re-reads by symbol, not by line.

## 0. Where we start

### R-21 (Go contract fixtures)

- **The id scheme question is closed.** GM-716 recommended option (a), migrate Go to NUL,
  and it shipped: `8dfb726c fix(go): hash ids over NUL-joined fields like the SDK` (on `main`)
  rewrote `plugins/go/ids.go` (`hashID` over NUL-separated fields), the literals in
  `ids_test.go`, and the comments in `ids.rs`, `id_scheme.rs`, `core/src/graph/containers.rs`.
  The three comments the review called false are now true: `ids.rs:19` ("`plugins/go`
  included (its `ids.go` hashes the same NUL-joined bytes)"), `ids.go:9-10`
  (`idFieldSeparator` "as in plugins/sdk/src/ids.rs: a NUL"), `ids_test.go:4-14` (values
  computed from the SDK byte string). The Go reindex is automatic through the plugin
  fingerprint (GM-716 "Reindex trigger"). Nothing to decide here.
- **The id corpus also exists already:** `e931f487 test: pin Go ids to an SDK-generated corpus`
  added `plugins/sdk/tests/go_id_corpus.rs` (writes `plugins/go/testdata/ids.json` under
  `G_MESH_BLESS=1`, otherwise asserts the file is current) and
  `plugins/go/ids_corpus_test.go` (`TestNodeIDForMatchesTheSDKCorpus`,
  `TestEdgeIDForMatchesTheSDKCorpus`, `TestIDsOfSpaceJoinCollidingTuplesDiffer`). This is the
  pattern every new corpus below copies.
- **Shared fixtures today:** `plugins/sdk/testdata/generated_headers.json` (read by
  `plugins/sdk/src/generated.rs:197` and `plugins/go/generated_test.go:13`), `ids.json`,
  and four hand-written framing files `core/tests/fixtures/*.rpc` read by
  `plugins/go/jsonrpc_test.go` (`coreFixture`, :20-27). Those `.rpc` files are not
  generated; nothing ties them to `wire` types.
- **Still unpinned:**
  - *Wire lines.* Go `wireNode` (`plugins/go/wire.go`) has 17 JSON fields; `wire::WireNode`
    has 20 (`wire/src/lib.rs:383`). Go lacks `declarations`, `aliasPaths`, `untypedCalls`.
    Go `wireEdge` has 8 fields; `WireEdge` (`:515`) has 9 (Go lacks `toDeclaration`). Each gap
    is deliberate (Go never emits them), but nothing records that, so a field
    renamed on either side goes unnoticed.
  - *Control frames.* Go reads `controlEnvelope` generically and decodes `params` per method
    in `handleEnvelope` (`control.go:280-394`, 7 `case` arms: `reindex`, `fileChanged`,
    `semanticPass`, `workspaceChanged`, `resolutionChanged`, `filesCreated`, `status`; no
    `prepareSemanticPass`, Go does not declare `semantic_prepare`). Go `filePathsParams` has
    only `filePaths` (no `linkedEdges`, `budgetMs`); Go `resolutionChangedResult` has no
    `watchPaths`. Responses: `fileChangeResponse`, `resolutionChangedResponse`, `ackResponse`
    (`wire.go:330-379`), mirrored by hand.
  - *Walk/gitignore.* Go hand-rolls gitignore (`ignore.go`, a documented subset: no character
    classes, no backslash escapes, no `**` fused with text) and the link policy
    (`walk.go`, `symlinks.go`); Rust uses `g_mesh_walk` (`walk/src/lib.rs`, `ignore` crate
    with `hidden(false) parents(false) ignore(false) git_ignore(true) git_global(false)
    git_exclude(false) require_git(false) follow_links(true)`). Go has 17 walk/ignore tests
    with hand-built trees and expectations (`walk_test.go`, `ignore_test.go`); none compares
    against the Rust walker.
- **Stale comments:** `plugins/go/ignore.go:8-10` says "`go.mod` for this plugin has zero
  requirements today"; `go.mod` has `require golang.org/x/tools v0.50.0` (read from
  `plugins/go/go.mod`). The same header (`:3-5`) and `walk.go:5-6` point at
  `plugins/sdk/src/walk.rs` as the walker; since GM-514 the walker is `walk/src/lib.rs`
  (`sdk/src/walk.rs` wraps it). `ignore.go:14` cites "the TS plugin's own fixtures" (that
  plugin is gone).

### R-14 (SDK control plane)

- `plugins/sdk/src/run.rs` `Session::handle` (:418-591) parses the body as
  `serde_json::Value`, then `match method` on 6 string literals (`"fileChanged"` :433,
  `"filesCreated"` :445, `"semanticPass"` :459, `"workspaceChanged"` :511,
  `"resolutionChanged"` :533, `"prepareSemanticPass"` :550) plus `"reindex"`/`"status"` in the
  catch-all (:562). Every param is read by `params.get("…").and_then(as_*)`.
- Production `serde_json::json!` literals in `run.rs`: 6. Responses at :546
  (`resolutionChanged`), :885 (`respond`, a diff), :896 (`acknowledge`), :915
  (`pass_response`); the bulk-stream trailer at :216 and :218 (`{"resolutionFacts"…}`), which
  is not a response but the same pattern. Core's reader of that trailer is a private
  `ResolutionFactsLine` (`core/src/protocol/ndjson.rs:26-31`, `deny_unknown_fields`).
- Wire types the SDK does not use: `ControlMessage`, `ControlEnvelope`, `FileChangeResponse`,
  `ResolutionChangedResponse` (only `LinkedEdge`, `FileChangeDiff`,
  `ResolutionChangedResult` are used). `wire` has no `serde_json` dependency (dev only).
  No wire type for the `{acknowledged: true}` result (Go has `ackResult`, `wire.go:377`).
- **Today's fallback when a frame does not decode** (all in `Session::handle`):

  | Input | Today |
  |---|---|
  | body not JSON | logged, no answer |
  | `id` | any non-null JSON value is echoed verbatim |
  | `method` missing / not a string | treated as `""`: acknowledged silently |
  | unknown method | logged, acknowledged |
  | `fileChanged.filePath` missing / not a string | `""`: `file_changed(RelPath::new(""))` runs and its diff is sent |
  | `fileChanged.reextract` not a bool | `false` |
  | `filesCreated.filePaths` missing | empty list; a non-string entry is dropped (test `one_bad_entry_does_not_cost_the_others`, :2135) |
  | `semanticPass.filePaths` missing / not an array / all entries non-string | empty list = **whole-project pass** |
  | `semanticPass.budgetMs` not a u64 | no deadline (engine's own) |
  | `semanticPass.linkedEdges` entry that does not parse | dropped |
  | `workspaceChanged.filePath` bad | `""`, only used in a log line |
  | `resolutionChanged.filePath` bad | `""`; `previousFacts` not a string -> `None` -> `Unknown` delta |

  The Go plugin's rule differs: malformed `params` -> logged, **no answer even with an id**
  (`control.go:294-297` and siblings), so core waits for its timeout.
- Core reads answers typed: `read_message` decodes the whole body after consuming it
  (`core/src/protocol/jsonrpc.rs:51`), so a body that is not a `FileChangeResponse` fails that
  one request ("failed to deserialize frame body") without desynchronising the stream; only
  `RoundTripTimedOut` kills the plugin.
- `pass_response` (:909-933) always writes `"incomplete": <bool>`; `FileChangeResponse`
  skips it when false (`skip_serializing_if = "is_false"`), as Go does. Core reads absent as
  false. Tests at :1087-1170 index `response["incomplete"]` and expect `false`.

### R-24 (`plugin.toml` schema)

Five raw readers of one file (the review counted three):

| Site | Shape | Strictness |
|---|---|---|
| `core/src/plugin_manifest.rs:845-910` `RawManifest`/`RawPlugin`/`RawSpawn`/`RawLanguages`/`RawFingerprint`/`RawWorkspace` | the whole file; typed core sections `Capabilities` (:162), `NonSymbolShapes` (:244), `SymbolQueryPrefixes` (:286), `ReexportRules` (:312), the last three `deny_unknown_fields` | required: `language`, `protocol_version`, `plugin_version`, `spawn`, `languages`; validation in `read_manifest` (:401-490) |
| `core/src/plugin_manifest.rs:564-585` `query_tables_of` (test-only `Outer`/`Inner`) | the two query tables | validated |
| `plugins/sdk/src/manifest.rs:182-207` | `language`, `plugin_version`, `languages.extensions`, `workspace.exclude_dirs` | any failure -> manifest ignored, code values kept (`ResolvedSpec::resolve_from`, :109-170) |
| `plugins/sdk/src/lsp/config.rs:461-489` `RawManifest`/`RawPlugin`/`RawSemantic` | `[plugin.semantic]` only (core does not type it) | `[plugin]` itself optional |
| `toml::Value` indexing in tests | `exclude_dirs`/`watch_files` | see below |

Tests and code that restate what `plugin.toml` says:
- `PluginCheck` builders: `plugins/python/tests/conformance.rs:219-229` (`.extensions(&[".py", ".pyi"])`,
  `EXCLUDE_DIRS`, `watch_files`, `entry_points`, `files_created`), `plugins/rust/tests/conformance.rs:248-256`,
  `plugins/typescript/tests/conformance.rs:108-118`, `plugins/sdk/tests/toy_conformance.rs:69-76`.
  The three plugin conformance files also re-parse `plugin.toml` as `toml::Value` to splice
  `[plugin.semantic]` (python :206-215, rust :235-244, typescript :92-104).
- `PluginSpec::new(…, &[".py", ".pyi"]).exclude_dirs(&project::EXCLUDE_DIRS)` in
  `plugins/{python,rust,typescript}/src/main.rs` and `plugins/sdk/toy/main.rs:67`.
- Sync tests by `toml::Value`: `plugins/python/src/project/mod.rs:1123-1133`,
  `plugins/rust/src/project/mod.rs:669-679`, `plugins/typescript/tests/units.rs:16-95`.
- `EXCLUDE_DIRS` consts stay: project-model code uses them in const/`contains` contexts
  (e.g. `python/src/project/mod.rs:652`, `typescript/src/project/tsconfig.rs:672`).

### R-26 (emit layer)

- Two near-identical emit layers: `plugins/python/src/extractor/emit.rs` (549 lines) and
  `plugins/rust/src/extractor/emit.rs` (455). Both define `Positions` (python :91-140,
  rust :60-105), `placeholder_key`, `Emitter` (dedup by id: `nodes`/`edges: HashSet`,
  `placeholders: HashMap<PlaceholderKey, String>`) with `declare`, `placeholder`, `reexport`,
  `external_module`, `resolved_edge`, `placeholder_edge`, `import_edge`, `open_site`,
  `mark_syntax_errors`, `finish`, and the same four unit tests. `Positions` is
  `sdk::CharColumns` (`plugins/sdk/src/columns.rs:17`) plus a `tree_sitter::Point`/`Node`
  adapter; `file_range` already delegates to `CharColumns`.
- **TypeScript does not duplicate the emit layer.** `plugins/typescript/src/extractor/emit.rs`
  is a 63-line `flush` onto `FileGraphBuilder`; its only copy is a 3-line node->`Range`
  adapter over `CharColumns` (`decls.rs:129-133`). The task's acceptance line "python/typescript
  emit via the SDK emit layer" should read **python/rust** (D5).
- Python's `render_target` (`emit.rs:427-447`) says the SDK keeps its renderer private; the SDK
  exports it (`plugins/sdk/src/lib.rs:103`, `graph.rs:698`), and the Rust plugin already
  imports it (`rust/src/extractor/emit.rs:53`, used :261). The copy differs: the SDK prefixes
  a `QualifiedName` key with `=`, Python's copy does not. Its only production use is
  `Emitter::reexport` (:319), whose two callers (`decls.rs` `import_from_statement`,
  `reexport_dunder_all`) both pass `TargetKey::Name` (`decls.rs:508,622`), so the switch is
  **id-neutral** in production; only the unit test at `emit.rs:527` builds a `QualifiedName`
  target.
- `tree-sitter` in `Cargo.lock`: exactly one version, `0.25.10`; grammars
  `tree-sitter-javascript 0.23.1`, `-python 0.25.0`, `-rust 0.24.2`, `-typescript 0.23.2`, all
  on `tree-sitter-language 0.1.8`. Declared three times as `tree-sitter = "0.25"`
  (`plugins/{python,rust,typescript}/Cargo.toml`); the root `Cargo.toml` has no
  `[workspace.dependencies]`. The SDK has no `[features]` and no tree-sitter dependency; core
  has none. So "single version in the lockfile" already holds; the work is to make it
  structural.

## 1. Decisions

### D1. Id scheme: closed by GM-716, nothing to decide

GM-716 decided (a) NUL for Go; `8dfb726c` implemented it and `e931f487` added the corpus.
The three comments are true (section 0). GM-711 only adds the remaining corpora.

### D2. Where the `plugin.toml` schema lives

- **Today:** five readers (section 0); core owns the only validation.
- **Change (recommended, a): a new crate `g-mesh-manifest` at `manifest/`** (sibling of
  `wire/`, `walk/`), deps `serde` + `toml`. Types: `Manifest { plugin: Option<PluginSection> }`,
  every field `Option<_>` or `#[serde(default)]`, no `deny_unknown_fields`; `capabilities` as
  `Option<bool>`/`Option<String>` fields; core-only tables (`non_symbol_queries`,
  `symbol_query_prefixes`, `reexports`) as `Option<toml::Value>`, which core decodes with its
  own strict types (`T::deserialize(value)` keeps `deny_unknown_fields`); `semantic:
  Option<SemanticSection>` with `RawSemantic`'s fields. `pub fn parse(&str) -> Result<Manifest,
  toml::de::Error>`.
  - Alternative (b): a `manifest` module in `wire` behind a `manifest` feature. No new crate to
    register in `scripts/cut-release.sh:248`, `.github/workflows/release.yml:54` and
    `scripts/test-sections.sh`, but `wire` (the protocol crate, `json-schema` feature for
    core) gains `toml` and a non-protocol concern.
  - Alternative (c): SDK owns it. Rejected: core does not depend on the SDK by design
    (root `Cargo.toml` header).
- **Example:** core `read_manifest` = `g_mesh_manifest::parse(&contents)?` then
  `plugin.language.context("… has no language")?` and so on; SDK `ResolvedSpec::resolve_from`
  uses `manifest.plugin?.languages?.extensions`; `SemanticConfig::from_manifest_at` uses
  `manifest.plugin.and_then(|p| p.semantic)`.
- **Consequence:** one struct set. Core's missing-field errors become core's own messages
  (no test asserts serde's "missing field" text for manifests; `absent_language_field.rs`
  must still pass). The SDK keeps today's rule "a manifest without `language`,
  `plugin_version` and `extensions` is ignored whole". A wrong-typed key anywhere (e.g.
  `capabilities.semantic_pass = "yes"`) now fails the SDK and LSP readers too, where they
  ignored it before; core already rejects such a manifest, so no plugin runs with it.
  Cost of (a): one crate to register in the three release scripts.

### D3. The typed-decode fallback rule (R-14)

- **Today:** the table in section 0: per-field defaults, never an error answer, and a
  `semanticPass` whose `filePaths` is malformed silently becomes a whole-project pass.
- **Change (recommended):** `wire` gains `ControlRequest::parse(body: &[u8]) ->
  Result<ControlRequest, NotJson>` with `ControlRequest { id: Option<RequestId>, method: String,
  message: Result<ControlMessage, ControlDecodeError>, dropped: Vec<String> }`
  (`ControlDecodeError::{UnknownMethod, BadParams(String), BadId}`). Rules:
  1. **List entries** (`filesCreated.filePaths`, `semanticPass.filePaths`,
     `semanticPass.linkedEdges`) are decoded element-wise; a bad entry is dropped and named in
     `dropped` (kept: today's "one bad entry costs itself"). New: a **non-empty list whose
     every entry was dropped** is `BadParams`, not an empty list, so it can no longer turn
     into a whole-project pass.
  2. **Optional scalars with a safe default** (`reextract`, `budgetMs`, `previousFacts`) that
     have the wrong type decode as absent and are named in `dropped` (today's behaviour,
     now logged).
  3. **Required scalars** (`filePath` of `fileChanged`, `workspaceChanged`,
     `resolutionChanged`) missing or mistyped -> `BadParams`.
  4. **Unknown method** -> `UnknownMethod`: acknowledged, as today. `reindex`/`status` are
     known variants and acknowledged.
  5. **`id`** is `RequestId` (number or string, JSON-RPC 2.0); another JSON type (or a
     fractional number) is `BadId`: logged, unanswered (core only sends integers).
  6. **`BadParams` with an id** -> a JSON-RPC error answer (new `wire::ControlErrorResponse`,
     code `-32602`, message = the decode error). Core's typed read fails that one request and
     the stream stays in step. Without an id -> logged only.
  The tolerance lives in `ControlRequest::parse` (pre-pass on the `params` object for rules
  1-2, then `serde_json::from_value::<ControlMessage>`), not in `#[serde]` attributes on
  `ControlMessage`, so core's own decodes of `ControlEnvelope`
  (`cli/plugin_check/session.rs:1398`) are unchanged. `wire` gains `serde_json` as a normal
  dependency (already in every consumer's tree).
  - Alternative (b): keep every per-field default exactly as today, typed only on success.
    Zero behaviour change, but `fileChanged` with no path still extracts `""` and the
    malformed-`filePaths` whole-project pass stays.
  - Alternative (c): strict: any decode failure is an error answer. Breaks "one bad entry
    costs itself" (`run.rs:2135`), a deliberate property.
- **Example:** `{"id":3,"method":"semanticPass","params":{"filePaths":[5]}}` -> today a
  whole-project pass; under (a) `{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"filePaths: every entry is malformed"}}`.
  `{"id":4,"method":"filesCreated","params":{"filePaths":[5,"b.toy"]}}` -> `b.toy` applied,
  `[0]` logged, acknowledged (as today).
- **Consequence:** `Session::handle` matches on `ControlMessage` variants; responses are
  `FileChangeResponse`, `ResolutionChangedResponse`, new `AckResponse { result: AckResult {
  acknowledged } }` and `ControlErrorResponse` in `wire`. **Wire-visible change:**
  `"incomplete": false` is no longer written (aligned with `FileChangeResponse` and Go; core
  reads absent as false). The Go plugin's rule (no answer on bad params) is not changed here;
  suggested follow-up: answer `-32602` there too.

### D4. Bulk trailer type moves to `wire` (in scope, small)

- **Today:** SDK writes `json!({"resolutionFacts", "watchPaths"})` (`run.rs:216,218`); core
  reads a private `ResolutionFactsLine` (`ndjson.rs:26-31`); Go has `bulkFactsLine`.
- **Change (recommended):** `wire::ResolutionFactsTrailer` (the core struct moved, same serde
  attributes incl. `deny_unknown_fields`, `watchPaths` skipped when empty); both sides use it,
  and it joins the Go wire corpus. Alternative: leave it (the acceptance names response
  literals only).
- **Consequence:** `run.rs` has no production `json!` left; byte output unchanged.

### D5. Emit layer: shape, feature name, acceptance scope

- **Today:** section 0.
- **Change (recommended, a):** a new SDK module `emit` with `GraphEmitter` (the shared
  `Emitter` core: dedup-by-id nodes/edges/placeholders over `FileGraphBuilder`, `declare`,
  `placeholder`, `external_module`, `resolved_edge`, `placeholder_edge`, `import_edge`,
  `open_site`, `mark_syntax_errors`, `finish`), **always compiled** (it needs no tree-sitter),
  and a feature **`tree-sitter`** (`tree-sitter = ["dep:tree-sitter"]`) that adds
  `CharColumns::point(tree_sitter::Point) -> Position` and `CharColumns::node_range(Node) ->
  Range` (= today's `Positions::at`/`range`). Plugin-specific parts stay in each plugin as a
  thin wrapper (Python `Definition`/`overloaded`/`reexport` label "`… as published`"; Rust
  `module_import`, `is_declared`, its `reexport` label "`container: … as published`").
  Python and Rust adopt both; TypeScript adopts `node_range` only. Root `Cargo.toml` gains
  `[workspace.dependencies] tree-sitter = "0.25"`, and the SDK and three plugins use
  `tree-sitter = { workspace = true }`.
  - Alternative (b): the whole `emit` module behind the feature. Simpler gating, but the
    dedup builder then needs tree-sitter for no reason (the toy plugin cannot use it).
  - Alternative (c): a separate crate `g-mesh-plugin-emit`. One more crate to register;
    no consumer needs it apart from the SDK.
- **Example:** `let mut emit = GraphEmitter::new("python", engine, path, columns.file_range());
  let id = emit.declare(spec, exported);` and `columns.node_range(node)`.
- **Consequence:** ~350 duplicated lines go; the four emitter unit tests move to the SDK once.
  Owner: **acceptance should say python and rust**, not python and typescript.

### D6. Split the task

See section 3. Recommended: yes.

## 2. Edit maps

### R-21

Write (generators follow `plugins/sdk/tests/go_id_corpus.rs`: build the corpus, write it under
`G_MESH_BLESS=1`, else assert the file on disk equals it):
- `wire/tests/go_corpus.rs` (new; `wire` dev-dep `serde_json` exists) ->
  `plugins/go/testdata/wire.json`: node lines (minimal + every optional field set), edge lines
  (with/without `toDeclaration`, `specifier`), one bulk trailer (D4), request frames
  (one `ControlEnvelope` per `ControlMessage` variant, with and without optional fields),
  response frames (`FileChangeResponse` with/without `incomplete*`/`unfinishedFiles`,
  `ResolutionChangedResponse` per `ResolutionDelta` kind, `AckResponse`). Built from wire
  values with `serde_json::to_value`, never hand JSON.
- `plugins/sdk/tests/go_walk_corpus.rs` (new, `#[cfg(unix)]`) ->
  `plugins/go/testdata/walk.json`: each case = `{name, files, gitignores: {dir: text},
  links: [{link, target}], expect}`; `expect` from `sdk::walk_project(root, [".go"],
  ["vendor","testdata"])` over a materialised tempdir. Exclude dirs read from
  `plugins/go/plugin.toml` (the `exclude_dirs` key; after R-24 via `g_mesh_manifest`).
  Patterns limited to Go's documented subset (`ignore.go:11-20`); no names differing only in
  case (APFS).
- `plugins/go/wire_corpus_test.go` (new): unmarshal each line/frame into the Go type
  (`wireNode`, `wireEdge`, `bulkFactsLine`, `controlEnvelope` + per-method params,
  `fileChangeResponse`, `resolutionChangedResponse`, `ackResponse`), marshal back, compare as
  JSON values after removing an explicit `goIgnores` list in the test (`declarations`,
  `aliasPaths`, `untypedCalls`, `toDeclaration`, `linkedEdges`, `budgetMs`, `watchPaths`,
  method `prepareSemanticPass`). A Rust field outside that list fails the Go test.
- `plugins/go/walk_corpus_test.go` (new): materialise each case in `t.TempDir()`, call
  `walkProjectFiles`, compare to `expect`; skip link cases on Windows.
- Comments: `plugins/go/ignore.go:3-20` (go.mod requires `golang.org/x/tools`; walker is
  `walk/src/lib.rs`; drop the TS-plugin reference), `walk.go:1-6`.
- Read for context: `walk/src/lib.rs` `walker` (:246-300), `sdk/src/walk.rs`
  `walk_project` (:56), Go `walk.go` `walkProjectFiles` (:27-62), `ignore.go`
  `compilePattern` (:112), `wire.go` (:255-379).
- Optional, not required by acceptance: regenerate `core/tests/fixtures/*.rpc` from wire types.

### R-14

- `wire/src/lib.rs`: add `ControlRequest`, `ControlDecodeError`, `AckResponse`/`AckResult`,
  `ControlErrorResponse`, `ResolutionFactsTrailer` beside `ControlEnvelope` (:825-832) and
  `FileChangeResponse` (:900-944); unit tests in `mod tests` (:953-) for each D3 rule.
  `wire/Cargo.toml`: `serde_json` to `[dependencies]`.
- `plugins/sdk/src/run.rs`: `Session::handle` (:418-591) -> `match request.message`;
  `respond` (:878-887), `acknowledge` (:889-899), `respond_to_pass` (:857-876), `pass_response`
  (:909-933) -> build wire types (`pass_response` returns `FileChangeResponse`); the
  `"resolutionChanged"` answer (:546); `bulk_index` trailer (:209-224). `id` becomes
  `Option<RequestId>`. Tests: `pass_response` tests (:1087-1170) read the typed struct;
  `files_created_with_an_id_is_acknowledged` (:2220) unchanged in meaning.
- `core/src/protocol/ndjson.rs:26-50`: use `wire::ResolutionFactsTrailer`.
- Read for context: `semantic.rs` `SemanticAnswer` (:93), `core/src/daemon/plugin.rs`
  `send_one` (:1393), `send_resolution_changed` (:1465), `core/src/protocol/jsonrpc.rs`
  `read_message` (:45-60).

### R-24 (after GM-713 is merged)

- `manifest/` crate (D2): `Cargo.toml` (version from the workspace's current version, read it,
  do not type it), `src/lib.rs`; root `Cargo.toml` `members`; register in
  `scripts/cut-release.sh` (list at :248), `.github/workflows/release.yml` (:54 check),
  `scripts/test-sections.sh`, and a path rule in `scripts/test-select.sh` (a path without a
  rule selects `full`).
- `core/src/plugin_manifest.rs`: `read_manifest` (:401-490) over the schema; delete
  `RawManifest`..`RawWorkspace` (:845-910); `query_tables_of` (:564-585) over the schema.
- `plugins/sdk/src/manifest.rs`: `read_manifest` (:172-175), delete `RawManifest`.. (:182-207);
  `ResolvedSpec::resolve_from` (:113-170); add `PluginSpec::from_manifest_str(&'static str)`
  (needs owned fields: `PluginSpec` holds `&'static` slices today, :57-62 -> `Cow<'static, …>`
  or `Vec<String>`).
- `plugins/sdk/src/testing.rs`: add `PluginCheck::from_manifest(path, plugin_binary, fixture)`
  next to `PluginCheck::new` (:129-154), reading `language`, `plugin_version`, `extensions`,
  `exclude_dirs`, `entry_points`, `watch_files`, `capabilities.{semantic_pass,files_created,
  receiver_calls,receiver_calls_structural}`, and the `[plugin.semantic]` table (so
  `manifest_extra` only overrides `command`).
- **LSP config:** `SemanticConfig::from_manifest_at` (`lsp/config.rs:346-420`), delete
  `RawManifest`/`RawPlugin`/`RawSemantic` (:461-489); keep `to_json`. **GM-713 contact:** this
  map names symbols, not paths. GM-713 splits `lsp/bridge.rs` (no R-24 edit) and moves
  `plugins/sdk/src/lsp/` to a crate `plugins/sdk-lsp`; the plan merges GM-713 first, so the
  R-24 slice rebases on it, locates `struct RawSemantic` by symbol (likely
  `plugins/sdk-lsp/src/config.rs`), and adds `g-mesh-manifest` to that crate's
  `Cargo.toml`. No R-24 edit touches a file GM-713 splits.
- Callers: `plugins/{python,rust,typescript}/src/main.rs`, `plugins/sdk/toy/main.rs:67` ->
  `PluginSpec::from_manifest_str(include_str!("../plugin.toml"))`; conformance tests
  (python :219-253, rust :248-280, typescript :108-144, toy :69-76) -> `from_manifest`;
  sync tests (python `project/mod.rs:1123`, rust `project/mod.rs:669`, typescript
  `tests/units.rs:16`) -> schema type instead of `toml::Value`.

### R-26

- `plugins/sdk/src/emit.rs` (new) + `columns.rs` (`point`/`node_range` under
  `#[cfg(feature = "tree-sitter")]`); `lib.rs` exports (:95-118); `plugins/sdk/Cargo.toml`
  `[features]` + optional dep.
- `plugins/python/src/extractor/emit.rs`: delete `Positions` (:88-140), `placeholder_key`
  (:142-153), the shared `Emitter` body, `render_target` (:427-447, use `g_mesh_plugin_sdk::render_target`);
  keep `container_target`, `Definition`, `reexport` label; rewrite the module doc where it
  claims the SDK renderer is private.
- `plugins/rust/src/extractor/emit.rs`: delete `Positions` (:57-105), `placeholder_key`
  (:105-119), shared `Emitter` body; keep `module_import`, `is_declared`, `reexport`.
- `plugins/typescript/src/extractor/decls.rs:129-133` -> `columns.node_range(node)`.
- Root `Cargo.toml` `[workspace.dependencies]`; `plugins/{python,rust,typescript}/Cargo.toml`.
- Read for context: `sdk/src/graph.rs` `FileGraphBuilder` (:428-660), `render_target` (:698).

## 3. Split

**Yes: GM-711 keeps R-21 + R-14; a new task takes R-24 + R-26.**
- R-24 is gated on GM-713's merge (LSP config move); R-21/R-14 are not and can start now.
- The halves share no source file (R-14: `wire/src/lib.rs`, `run.rs`, Go tests; R-24/26:
  manifest crate, `plugin_manifest.rs`, `sdk/manifest.rs`, `testing.rs`, LSP config, emit
  files), only `Cargo.lock` and the root `Cargo.toml`.
- R-21's control-frame corpus needs R-14's new wire types (ack, error), so those two belong
  together; R-24 and R-26 both touch every plugin's crate and `Cargo.toml`.
- Each half gets a smaller verify (R-21/R-14: `wire`, SDK, Go; R-24/R-26: core manifest,
  three plugins' conformance), instead of one verify over everything.

## 4. Behaviours the tests slices must pin

GM-711 (R-21 + R-14):
1. Each new corpus: the Rust generator test fails when the file is stale; the Go test fails
   when Go's decode/encode drifts; a Rust field outside `goIgnores` fails the Go test.
2. Walk corpus covers: root and nested `.gitignore`, deeper layer overrides, negation, `**`,
   trailing `/`, anchored `/`, hard-excluded `vendor`/`testdata`/`.git`/`.claude`,
   `.GO` extension, followed dir link once, cycle, dangling link, link outside root, link into
   excluded dir.
3. Each D3 rule: dropped list entry costs itself; all-dropped non-empty `filePaths` is an
   error answer, not a whole-project pass; bad optional scalar defaults; missing required
   `filePath` -> `-32602` with id, nothing without id; unknown method acknowledged; non-JSON
   unanswered; non-integer/string id unanswered.
4. Responses serialize to the wire types (a `fileChanged` answer decodes as
   `FileChangeResponse`; `incomplete` absent when false; ack decodes as `AckResponse`).
5. Bulk trailer bytes unchanged (`watchPaths` absent when empty).

New task (R-24 + R-26):
6. Lenient schema: unknown keys/tables accepted, missing optional sections default; core still
   rejects a missing `language`/`spawn` with the manifest path in the message; core still
   rejects an unknown key inside `non_symbol_queries` (`deny_unknown_fields` reached through
   `toml::Value`).
7. SDK ignores a manifest of another language or without extensions (as today);
   `[plugin.semantic]` read as before; `PluginCheck::from_manifest` equals today's explicit
   builder for python/rust/typescript/toy.
8. `node_range` converts byte columns to characters; `GraphEmitter` dedups nodes, edges,
   placeholders; Python reexport ids unchanged on a fixture (`plugin-check` id-stability plus
   one pinned id); one `tree-sitter` package in `Cargo.lock`.

### Candidate controls (one per behaviour, revert = code, never the test)

| # | Revert / inject | Must fail | Binary |
|---|---|---|---|
| C1 | Go `wire.go`: `wireEdge` tag `fromId` -> `from_id` | `wire_corpus_test.go` | Go (`go test ./...` in `plugins/go`) |
| C2 | Go `ignore.go`: ignore `!` negation in `compilePattern` | `walk_corpus_test.go` | Go |
| C3 | `wire`: drop `rename_all = "camelCase"` on `SemanticPass` (field `budget_ms`) | `wire/tests/go_corpus.rs` stale check | Rust `-p g-mesh-wire --test go_corpus` |
| C4 | `ControlRequest::parse`: decode lists strictly (no element drop) | SDK `one_bad_entry_does_not_cost_the_others` | Rust `-p g-mesh-plugin-sdk --lib` |
| C5 | remove the all-dropped -> `BadParams` rule | new SDK test "malformed filePaths is not a whole-project pass" | Rust SDK `--lib` |
| C6 | answer `BadParams` with an empty diff instead of `-32602` | new SDK/wire test on the error answer | Rust SDK `--lib` |
| C7 | `g-mesh-manifest` consumer in core: skip the `language` presence check | core `plugin_manifest` tests / `absent_language_field` | Rust `-p g-mesh --lib` (new task) |
| C8 | SDK `GraphEmitter`: drop the `edges` dedup set | moved `one_edge_written_twice_is_one_edge` | Rust SDK `--lib --features tree-sitter` (new task) |

C1-C2 are Go-only (no cargo build; group them in one `go test` run). C3-C6 are GM-711's Rust
controls; C4-C6 share one SDK lib build. C7-C8 belong to the new task.

## 5. Proposed re-slicing

GM-711 (R-21 + R-14): **S2 code R-14 (opus)** -> **S3 code R-21 corpora + Go tests + comments
(sonnet, this edit map)** -> S4 review -> S5 tests -> S6 verify.

New task (R-24 + R-26), after GM-713 merges: S1 code R-24 (opus) -> S2 code R-26 (sonnet) ->
S3 review -> S4 tests -> S5 verify.

Unsplit fallback: S2 R-14 opus; S3 R-21 sonnet; S4 R-24 opus (after GM-713); S5 R-26 sonnet;
S6 review; S7 tests; S8 verify.
