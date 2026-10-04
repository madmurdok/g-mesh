# GM-323: what the TypeScript port must not break

Inventory taken before the Node plugin (`plugins/typescript`) is ported to Rust
(GM-324 structural tier, GM-325 semantic tier). Inputs: every test in
`plugins/typescript/test`, every `core/tests/*` file that touches TypeScript,
both TypeScript corpora of `../g-mesh-bench`, and the decisions already taken in
`docs/architecture/plugin-distribution.md` (option C + S2),
`docs/adr/0023-project-model-tracks-file-presence.md` and
`docs/architecture/gm-350-ts-resolution-placement.md` (section 8 classified 90
of these tests already; this note adopts it and maps it onto the four buckets).

## Summary

### Counts

`plugins/typescript/test`: **312 `test(...)` declarations in 15 files** (the
"~298" in plugin-distribution.md is stale). One declaration in
`extract.test.ts` (line 1344) is a loop over 8 shadowing forms, so a run reports
319 cases; two are skipped under root / on Windows (`chmod 000`).

| Bucket | Meaning | Tests |
|---|---|---|
| **A** | bench-load-bearing: a g-mesh-bench question on excalidraw or task-tracker-mcp depends on it | **47** |
| **B** | language behaviour that must survive | **199** |
| **C** | Node / SEA / tsserver-plumbing specific, moot after the port | **47** |
| **D** | a capability the other plugins lack and should gain (already actioned or new backlog candidate) | **19** |

B splits by where the behaviour lives after the port (column "Lives on as" in
the per-file tables): the TypeScript crate's own Rust tests (`ts-unit`, the
`plugins/python/src/extractor/tests.rs` precedent), the semantic tier's Rust
tests (`sem-unit`), the SDK which already owns it (`sdk ...`: walk, diff,
framing, semantic plumbing - GM-324 checks the SDK covers it, it does not port
it), the conformance kit's built-in session checks (`kit-check`), an existing
`expect.toml` entry (`kit: ...`) or a new one (`E1`..`E15`, below).

`core/tests`: **47 files touch TypeScript; 44 spawn the real plugin.** 16 of
them assert TypeScript-specific graph behaviour, 28 use TypeScript only as "the
bundled plugin" for lifecycle/MCP tests. **17 drive the semantic tier**: 8
assert results only tsserver produces, 9 only need its long-lived process or
its pass to complete. 5 files depend on Node specifics. The 26/13 from
2026-09-20 counted a narrower set; the table in "core/tests inventory" is the
full list.

### What cannot be carried across

| # | What is lost | Who loses it |
|---|---|---|
| 1 | **The free semantic tier.** After C + S2 every `tier = "semantic"` result needs an external TypeScript language server. Without one: namespace-import callers (`m.double(4)`; bench `ex-namespace-import-laserpointer-plerp`), renamed default imports (bench `ex-default-export-dropdownmenu-group`), the `export *` branch choice, overload bindings (the `format` `files` tally) all disappear. | TypeScript users with no server installed; CI, which must install one or run with `--skip-semantic-expectations` and lose the signal. The 8 semantic-result core tests (table below) need the server on every CI runner. |
| 2 | **Preferring the project's own TypeScript, with a bundled fallback** (`semantic.test.ts` 109, 121). After the port the server picks its TypeScript. | Projects pinned to an old/new TypeScript whose resolution differs from whatever the server bundles. |
| 3 | **"No checker child unless there is a question"** (`semanticPass.test.ts` 539, 564, 591, 813). The Node plugin asks tsserver only about namespace imports, barrels and overloads; an LSP route is started per pass whatever the file contains. | Small projects with no such shapes: a language-server start (memory, latency) they do not pay today. |
| 4 | **Runtime network instrumentation** (`security.test.ts` 339 patches `fetch`/`http` during a real index) has no direct Rust equivalent; 247/312 (source and `package.json` scans) become a dependency audit. | Nobody functionally; the guarantee becomes weaker evidence (a dependency list rather than an observed run). |
| 5 | **Resolution freshness on the fast path** (ADR 0023's named residual window): an importer routed before its new target in one burst stays unresolved until its next edit; `.gitignore` edits are not re-evaluated mid-session. Today's per-reparse disk probe has neither gap. | Agents that create a file and its importer in one batch. |
| 6 | **The SEA's "no install at all"**, and with it the tsserver-protocol quirks it needed (Windows Content-Length off-by-one, forward-slash paths: `jsonrpc` 102/119, `semanticPass` 66, `semantic` 90). | Nobody, unless a later design drives raw tsserver again. |

One change is a gain and must be planned as such: TypeScript is the only plugin
with `receiver_calls = "unresolved"` in both tiers. An LSP-backed semantic tier
can resolve `g.greet()` the way go/rust/python do; when it does, these flip by
design: `expect.toml`'s `[[callers]] symbol = "Greetable#greet"` (`expect = []`),
`core/tests/plugin_check.rs`'s structural-arm list in
`a_namespace_import_caller_needs_the_semantic_pass_to_resolve`,
`core/src/mcp/instructions/tests.rs` (the "in typescript, a method call through
a variable receiver" sentence), and `plugin.toml`'s two `receiver_calls*` keys.
In the Node suite the gap is encoded by `extract.test.ts` 1381/1599 and
`semanticPass.test.ts` 813 (no question asked for an ordinary call).

### Resolved at review (2026-10-04)

1. CI installs a TypeScript language server (via `scripts/test-deps.sh`, as for
   pyright and rust-analyzer) rather than running with
   `--skip-semantic-expectations`; the 8 semantic-result core tests run against
   it. Owned by GM-325.
2. If the ported semantic tier resolves receiver calls, `receiver_calls*` flip
   to `"resolved"` together with the sites listed above. Owned by GM-325.
3. D-1..D-4 are backlog tasks GM-510..GM-513.
4. `core/tests/semantic_pass_trigger.rs`'s Node fake plugin is rewritten in
   Rust. Owned by GM-351.

### Expectations for S2 (written against today's Node plugin)

New files only under `conformance/project`; none of the seven existing files is
edited, and no root `tsconfig.json` is added (the inferred-project case,
`semantic.test.ts` 323, is covered by the fixture having none). Every new
symbol name is unique in the fixture so existing `[[definition]]` entries stay
unambiguous. Where a field below says "S2 pins", S2 records today's exact
answer (set, `files` tally, `excluded_references`, `tier`) rather than this note
guessing it; `tier = "semantic"` goes on every entry whose structural-off arm in
`core/tests/plugin_check.rs` fails, and that test's list of entries a missing
tier must not disturb is extended accordingly.

New fixture files: `package.json` (`"workspaces": ["packages/*"]`,
`"imports": {"#int/*": "./src/int/*.ts"}`);
`packages/geom/package.json` (`"name": "@fx/geom"`, `main` and an `exports`
map with `"."` and `"./*"` both pointing at `./dist/prod/index.js`, which does
not exist - the excalidraw shape); `packages/geom/src/index.ts`
(`export * from "./point"`); `packages/geom/src/point.ts`
(`export const pointOf = (x, y) => ...`); `packages/tsconfig.base.json`
(`paths: {"~geom/*": ["./geom/src/*"]}`, no `baseUrl`);
`packages/app/tsconfig.json` (`"extends": "../tsconfig.base.json"`);
`packages/app/src/{root,subpath,alias}.ts`; `src/esm/{lib,use}.ts`,
`src/esm/dir/index.ts`; `src/members.ts`; `src/heritage/{container,box}.ts`;
`src/defaults/{menuGroup,use}.ts`; `src/amb/{a,b,index,use}.ts`;
`src/callbacks.ts`; `src/shadow.ts`; `src/dyn/load.ts`,
`src/dyn/plugins/alpha.ts`; `src/int/secret.ts`, `src/privateUse.ts`;
`src/merge.ts`; `src/ambient.ts`; `src/nsref/{lib,use}.ts`.

| Id | Entry | Expect | Bench / origin | Control (the change that must fail it) |
|---|---|---|---|---|
| E1 | `[[callers]] symbol = "pointOf"`, `file = "packages/geom/src/point.ts"` | `packages/app/src/root.ts:viaRoot` (`from "@fx/geom"`), `packages/app/src/subpath.ts:viaSubpath` (`from "@fx/geom/point"`), `packages/app/src/alias.ts:viaAlias` (`from "~geom/point"`) | `ex-find-callers-mutateelement`, `ex-references-pointfrom-highfanout`, `ex-ambiguous-clamp-math-utils`; core `reexport_linking.rs` | Ignore the root `workspaces` field: `viaRoot`, `viaSubpath` drop. Let a declared entry under `dist/` shadow source: `viaRoot` drops. Drop the subpath-into-`src` fallback: `viaSubpath` drops. Ignore `extends`, or anchor a baseUrl-less target at the project root: `viaAlias` drops. Stop extracting `export const f = () =>` as a function: anchor refuses. |
| E2 | `[[importers]] file = "packages/geom/src/index.ts"` | `packages/app/src/root.ts` | `ex-deps-package-math-incoming` | Let `main`/`exports` under `dist/` win: set empty. |
| E3 | `[[imports]] file = "packages/app/src/alias.ts"` | `packages/geom/src/point.ts` | tsconfig `extends` + config-relative targets (excalidraw `packages/tsconfig.base.json`) | Ignore `extends`: set empty. |
| E4 | `[[imports]] file = "src/esm/use.ts"` plus `[[callers]] symbol = "fromLib"` | imports `src/esm/lib.ts` (`from "./lib.js"`), `src/esm/dir/index.ts` (`from "./dir"`); callers `src/esm/use.ts:viaJsSpecifier` | `tt-deps-incoming-db-connection`, `tt-references-requiretask` (task-tracker is `NodeNext`, every import is `./x.js`); excalidraw `viewport.ts` imports `./scene` | Remove the `.js`->`.ts` substitution: `lib.ts` and the caller drop. Remove directory->`index` resolution: `dir/index.ts` drops. |
| E5 | `[[callers]] symbol = "pick"`; `[[callers]] symbol = "Store.drop"`; `[[definition]] symbol = "Store#pick"` | `src/members.ts:Store#pick` (its body calls the module function `pick()`); `src/members.ts:clear` (`Store.drop()`); `src/members.ts:Store#pick` | `ex-references-getnondeletedelements-medfanout` (standalone function vs the same-named `Scene` method); `ex-find-callees-mutateelement` (`ShapeCache.delete` is in its pool) | Let a bare name bind a class member: the first set loses its row. Stop resolving `Owner.member()`: the second is empty. Join instance members with `.`: the definition refuses. |
| E6 | `[[implementations]] symbol = "Container"`; `[[references]] symbol = "Item"` | `src/heritage/box.ts:Box` (`implements Container<Item>`, cross-file; S2 pins whether `SpecialBox extends Box` is listed); references include `src/heritage/box.ts:Box` (S2 pins the rest) | `ex-find-impl-deltacontainer-generic`, `ex-find-impl-trail-crossfile`, `tt-find-impl-completionverifier`, `ex-multihop-trail-impl-pointfrom`; core `generic_reference_resolution.rs` | Treat a generic heritage head as a reference rather than a supertype: implementations empty. Drop heritage type arguments as references: `Box` leaves the references set. |
| E7 | `[[callers]] symbol = "MenuGroup"` (S2 pins `tier`) | `src/defaults/use.ts:renderGroup` (`import DropdownMenuGroup from "./menuGroup"`) | `ex-default-export-dropdownmenu-group`; core `default_export_linking.rs` | Disable the default-import upgrade in `semanticPass.ts`: set empty. |
| E8 | `[[callers]] symbol = "mutate"`, `file = "src/amb/a.ts"` and the same for `src/amb/b.ts`, `tier = "semantic"` | the branch TypeScript picks gets `src/amb/use.ts:useMutate`, the other `[]` (S2 pins which) | core `ambiguous_reexport_linking.rs`, `plugin_bridge.rs` | Swap the two `export *` lines in `src/amb/index.ts`: the sets swap. |
| E9 | `[[callers]] symbol = "leaf"` | `src/callbacks.ts:arrowCaller` (const arrow), `:viaCallback` (`xs.map(() => leaf())`), `:Holder` (class field arrow); S2 pins `files` and the `excluded_references` block from the top-level `leaf();` | `ex-find-callees-mutateelement` (`mutateElement` is a const arrow), `ex-find-callers-mutateelement` | Drop calls inside function-valued initializers: `arrowCaller` drops. Stop attributing callback calls to the enclosing function: `viaCallback` drops. Class-field case: `Holder` drops. |
| E10 | `[[callers]] symbol = "helper"` | `src/shadow.ts:inner` only (`outer(helper: () => void)` calls its parameter) | precision: `ex-scenario-deletesafe-getrectangleboxabsolutecoords`, `tt-scenario-deletesafe-taskcode` (zero false usages) | Disable scope shadowing: `src/shadow.ts:outer` appears. |
| E11 | `[[imports]] file = "src/dyn/load.ts"` | `src/dyn/plugins/alpha.ts` (``import(`${DIR}/alpha`)`` with `const DIR = "./plugins"`), nothing for ``import(`./locales/${code}.json`)`` | `ex-control-i18n-dynamic-locale-import`; core `dynamic_import_resolution.rs` | Stop folding same-file constants into templates: set empty. (Recording an edge for the unfoldable one is caught only if `get_dependencies` lists placeholders; S2 says which; if not, that half stays a `ts-unit`.) |
| E12 | `[[imports]] file = "src/privateUse.ts"` | `src/int/secret.ts` (`from "#int/secret"`) | `bulkIndex.test.ts` 764 | Ignore package.json `imports`: set empty. |
| E13 | `[[definition]] symbol = "Settings"` | `src/merge.ts:Settings` (two `interface Settings` statements) | declaration merging | Emit one node per statement: two rows, the kit's duplicate check fails. |
| E14 | `[[references]] symbol = "target"`, `tier = "semantic"` | `src/nsref/use.ts:keep` (`export const keep = lib.target` via `import * as lib`) | `semanticPass.test.ts` 186 | Stop recording non-call namespace member sites: set empty. |
| E15 | `[[definition]] symbol = "ambient"`; `[[definition]]` of a function inside `export namespace Outer.Inner` (S2 pins the spelling) | `src/ambient.ts:ambient` (two bodiless `export declare function ambient(...)` signatures, one node) | ambient declarations; `qualifiedPath.test.ts` 129, `extract.test.ts` 448 | Split a bodiless overload set into nodes: two rows. Split a dotted namespace name into nested segments: the definition refuses or changes. |

As landed (S2/S3): E5's first entry is `[[callers]] symbol = "Store#pick"`
with `expect = []`, because the kit cannot narrow a bare `pick` that shares a
file with `Store#pick`. It catches a bare call binding the class member, but
not a bare call binding nothing; that positive half (`pick()` inside a method
binds the module function) stays a `ts-unit` test the port must carry. E9's
class-field caller is `Holder#fire`. Under its control E1f yields an empty set
rather than a refused anchor, and E3 degrades to a `container:` row rather than
an empty set.

Not expressible in the kit, so not in this list (they stay `ts-unit`): outline
`exported` flags (`extract` 140, the outline bench questions) - the kit has no
`[[outline]]`; node kinds and signatures; id stability; specifier folding
details (enum members, `path.join(__dirname)`, conditionals); NUL handling;
gitignore/hard-excluded behaviour (SDK walk).

## Bucket A evidence: g-mesh-bench

Two TypeScript corpora (`../g-mesh-bench/corpora/registry.json`):
**excalidraw** at `1acf66ed` (pnpm-style monorepo, root `package.json`
`workspaces: ["excalidraw-app", "packages/*", "examples/*"]`; each
`packages/*/package.json` declares `main`/`module` and an `exports` map
pointing into an unbuilt `dist/`; `packages/element/src/index.ts` has 48
`export *`; 915 root and 446 subpath `@excalidraw/*` imports) and
**task-tracker-mcp** at `35237c8b` (`module`/`moduleResolution: NodeNext`, so
every relative import is spelled `./x.js`). Questions:
`corpora/excalidraw/tasks.json` (34), `corpora/task-tracker-mcp/tasks.json`
(18). The harness (`harness/token-economy.ts`, `session-economy.ts`,
`search-latency.ts`, `cold-start.ts`) is plugin-agnostic: it drives the MCP
tools, so the evidence is in the questions and oracles, not the harness.

| Bench question(s) | Behaviour it needs | Node tests | Core test | Kit |
|---|---|---|---|---|
| `ex-find-callers-mutateelement`, `ex-references-pointfrom-highfanout`, `ex-references-getnondeletedelements-medfanout`, `ex-ambiguous-clamp-math-utils`, `ex-scenario-impact-getelementabsolutecoords`, `ex-multihop-*` | `@excalidraw/x` resolves to the workspace package's source though `main`/`exports` name a missing `dist/`; subpaths fall back into `src`; barrels of `export *` | bulkIndex 676, 717; resolve 219, 229, 235, 266, 332; workspace 57, 85, 97, 117, 125, 134, 172, 212; extract 1216, 1243 | `reexport_linking.rs` (built on exactly this shape) | E1, E2; `add` via `export *` |
| `ex-deps-package-math-incoming` | importers of `packages/math/src/index.ts` through the package name | as above | `import_resolution.rs` | E2 |
| `ex-find-callees-mutateelement`, `ex-find-callees-updateelbowarrowpoints`, `ex-multihop-elbowarrow-routing-callees` | calls inside `export const f = (...) => {...}` and callbacks are attributed to `f`; `ShapeCache.delete()` reaches the static member | extract 163, 1083, 1104, 1121, 1403 | - | E5, E9 |
| `ex-references-getnondeletedelements-medfanout` | the standalone `getNonDeletedElements` is not the `Scene` method of the same name; qualified names tell them apart | extract 1381; qualifiedPath 66, 90 | - | E5 |
| `ex-find-impl-trail-crossfile`, `ex-multihop-trail-impl-pointfrom`, `tt-find-impl-completionverifier` | imported interface as supertype, cross-file | extract 100, 957 | - | `Greetable`; E6 |
| `ex-find-impl-deltacontainer-generic` | `implements DeltaContainer<T>`: generic head is a supertype, arguments are references | extract 1641, 1673, 974 | `generic_reference_resolution.rs` | E6 |
| `ex-default-export-dropdownmenu-group` | `const MenuGroup ...; export default MenuGroup` imported as `DropdownMenuGroup` | extract 942; semanticPass 339 | `default_export_linking.rs` | E7 |
| `ex-namespace-import-laserpointer-plerp` | `import * as m from "./math"; m.plerp(...)` (`packages/laser-pointer/src/state.ts`) | extract 1497; semanticPass 120 | `namespace_import_resolution.rs`, `namespace_import_after_init.rs` | `double` (`useNamespaceImport`) |
| `ex-control-i18n-dynamic-locale-import` | ``import(`./locales/${currentLang.code}.json`)`` must produce no edge | extract 771 | `dynamic_import_resolution.rs` | E11 |
| `ex-outline-types-vs-i18n`, `tt-outline-lifecycle-vs-small` | outline lists exported interfaces/functions | extract 75, 140 | `daemon_core.rs` (outline) | none (no `[[outline]]`) |
| `tt-deps-incoming-db-connection`, `tt-references-requiretask`, `tt-scenario-impact-requireproject`, `tt-scenario-deletesafe-taskcode` | `./db/connection.js` resolves to `connection.ts`; `tests/` is walked | resolve 39, 56, 88; bulkIndex 591 | `import_resolution.rs`, `cli_init.rs` fixtures use `./db/connection.js` | E4 |
| all excalidraw questions (indexing at all) | `node_modules`/`dist` skipped; a package-named `extends` (`dev-docs/tsconfig.json` extends `@tsconfig/docusaurus/...`) is not fatal; directory imports (`viewport.ts` imports `./scene`) | bulkIndex 259; ignorePolicy 47; tsconfigPaths 200; resolve 73 | - | E4 (directory) |
| 13 semantic-search questions (9 excalidraw, 4 task-tracker-mcp) | embedding text from a node's doc comment and signature | (not a dedicated Node test) | `embedding_generation_pipeline.rs` | none |

Not counted as A, with the reason: tsconfig `paths` (tsconfigPaths 55-319
except 200) - excalidraw's aliases map the same `@excalidraw/*` names the
workspace already resolves, and the workspace answer wins (resolve 332), so no
bench question depends on an alias; ambiguous `export *` (semanticPass 252) -
no bench question has one; `#private` imports - neither corpus uses them.

## core/tests inventory

Found by: `find_references` on `bundled_manifest` / `BUNDLED_LANGUAGE`
(in-process spawns) and a grep for TypeScript fixture files plus the daemon's
default discovery (every cold-start walk of a project containing `.ts`
spawns the bundled plugin). There is no shared "spawn the TS plugin" helper in
`core/tests/common/mod.rs` (its outline has none).

Columns: **Role** - `TS` asserts TypeScript-specific graph behaviour,
`default` uses TypeScript only as the bundled plugin; **Semantic** - `results`
asserts something only tsserver produces, `lifecycle` needs the pass or its
long-lived process but no semantic result; **Survives** - `wire` survives a
change of implementation language untouched, `bundled_manifest()` survives
once that core helper points at the Rust binary, `server` additionally needs a
TypeScript language server on the runner, `Node` names the Node dependency.

| File | Tests | Role | Semantic | Survives |
|---|---|---|---|---|
| `ambiguous_reexport_linking.rs` | 1 | TS | results | wire, server |
| `cli_clean.rs` | 6 | default | - | wire |
| `cli_clean_sweeping.rs` | 6 | default | - | wire |
| `cli_init.rs` | 4 | default | - | wire |
| `cli_reindex.rs` | 2 | default | - | wire |
| `cli_status.rs` | 7 | default (+ `broken.ts` syntax-error count) | lifecycle | wire |
| `cli_stop.rs` | 5 | default | lifecycle | wire |
| `daemon_build_staleness.rs` | 5 | default | - | wire; the build stamp it compares includes `bundled_fingerprint()` (hash of the JS entry), which core must re-point |
| `daemon_core.rs` | 5 | default (outline) | - | wire |
| `daemon_sigterm.rs` | 1 | default | lifecycle | wire |
| `default_export_linking.rs` | 1 | TS | results | wire, server |
| `dynamic_import_resolution.rs` | 1 | TS | - | wire |
| `embedding_generation_pipeline.rs` | 8 | TS (doc comment, signature) | - | `bundled_manifest()` |
| `first_query_after_walk.rs` | 3 | default | lifecycle | wire |
| `generic_reference_resolution.rs` | 1 | TS | - | wire |
| `handshake_independent_of_indexing.rs` | 2 | default | - | wire |
| `idle_lifecycle.rs` | 2 | default | lifecycle | wire |
| `import_resolution.rs` | 1 | TS | - | wire |
| `incremental_embed_outside_lock.rs` | 1 | default | lifecycle (`init`'s pass) | wire |
| `incremental_matches_full_reindex.rs` | 8 (3 TypeScript) | TS | - (only Go runs the pass here) | `bundled_manifest()` |
| `index_wait_progress.rs` | 4 | default | - | wire |
| `last_used.rs` | 3 | default | - | wire |
| `lazy_activation.rs` | 5 (1 uses a fake Python plugin) | default | - | wire |
| `multi_project_front.rs` | 11 | default | - | wire |
| `namespace_import_after_init.rs` | 1 | TS | results | wire, server |
| `namespace_import_resolution.rs` | 1 | TS | results | wire, server |
| `orphaned_daemon.rs` | 5 | default | - | wire |
| `overload_call_binding.rs` | 1 | TS | results | wire, server |
| `overload_call_resolution.rs` | 1 | TS | results | wire, server |
| `overload_declaration_storage.rs` | 3 | TS | - | 2 via `bundled_manifest()`; **Node**: `the_plugins_own_ndjson_mentions_declarations_only_for_the_overloaded_node` runs `node ../plugins/typescript/dist/src/index.js --bulk-index` |
| `plugin_bridge.rs` | 3 | TS | results (1: `an_ambiguous_reexport_is_resolved_by_the_plugin_semantic_pass`) | wire; server for that one |
| `plugin_build_staleness.rs` | 2 | default | - | **Node**: copies `dist/src/*.js` into a scratch plugin, writes a manifest with `command = "node"`, sets `G_MESH_JS_TS_PLUGIN_PATH`; the "re-emitted JS changes the fingerprint" premise becomes "rebuilt binary" |
| `plugin_check.rs` | 32 (2 run the TS plugin, + the semantic-off arm) | TS (conformance) | results | wire for `the_typescript_plugin_passes_on_a_small_typescript_fixture` and `the_typescript_plugin_satisfies_its_own_expectations_file` (manifest-driven; `G_MESH_PLUGIN_CHECK_MARKER_DIR` is in `plugins/sdk/src/semantic.rs` too); **Node**: `a_namespace_import_caller_needs_the_semantic_pass_to_resolve` symlinks `dist` and `node_modules` into a scratch plugin dir |
| `plugin_crash_recovery.rs` | 1 | default | - | `bundled_manifest()` |
| `plugins_die_with_daemon.rs` | 9 (2 TypeScript: `bulk_` and `long_lived_typescript_plugin_dies_with_a_killed_daemon`) | default | lifecycle | wire (`G_MESH_PLUGIN_HOLD_DIR` is in `plugins/sdk/src/hold.rs`; Node's copy is `src/testHold.ts`) |
| `protocol_conformance.rs` | 10 | not spawned (records labelled `typescript`) | - | wire |
| `query_time_staleness.rs` | 1 | default | - | wire |
| `reexport_linking.rs` | 1 | TS (excalidraw-shaped workspace) | - | wire |
| `repeated_edits_through_a_warm_plugin.rs` | 3 | TS | lifecycle | `bundled_manifest()` + wire |
| `replay_progress.rs` | 2 | default | lifecycle | wire |
| `semantic_pass_trigger.rs` | 2 | not the TS plugin: a fake plugin written in JavaScript | - | **Node**: the fake runs under `node`, located via `G_MESH_JS_TS_PLUGIN_PATH`; after the port it is the last thing in core's suite that needs Node - rewrite the fake in Rust or shell |
| `serving_while_indexing.rs` | 3 | default | - | wire (`G_MESH_BULK_INDEX_DELAY_MS` is core's) |
| `shim_handle_inheritance.rs` | 1 | default | - | wire |
| `stale_index_invalidation.rs` | 2 | default | - | wire |
| `structural_does_not_wait_for_embeddings.rs` | 2 | default | - | wire |
| `ts_build_stamp.rs` | 8 | Node build (`core/build.rs`'s npm build stamp) | - | **Node**: moot after the port |
| `wedged_daemon.rs` | 5 | default | - | wire |

Shared Node-flavoured indirections the port changes in core, not in tests:
`daemon::plugin::bundled_manifest()`, `plugin_entry_path()`,
`launch_command_for()` and `PLUGIN_PATH_ENV` (`G_MESH_JS_TS_PLUGIN_PATH`);
`bundled_fingerprint()`; `plugin_pid_path()` keyed by `BUNDLED_LANGUAGE`
(survives as long as the language id stays `typescript`). Fifteen files carry a
"Requires `plugins/typescript/dist/` to be up to date; `core/build.rs` runs
npm" module comment - doc-only. Outside `core/tests`, in-crate tests that name
the JS build: `core/src/daemon/plugin/tests.rs`
(`a_checkout_still_resolves_to_the_compiled_javascript_entry_point`,
`the_bundled_plugins_build_is_fingerprintable_from_the_test_binary`) and the
`package.json`/`plugin.toml` version-agreement tests in
`core/src/daemon/manifest.rs`.

## Bucket D: backlog candidates

Already actioned from this suite: overload binding for the other languages
(GM-348; `semanticPass` 670, 729) and the symlink walk (GM-349; `bulkIndex`
467-558, `workspace` 291-372). New candidates:

| Id | Capability (TypeScript tests) | Gap found | Confidence |
|---|---|---|---|
| D-1 | Fold statically known dynamic import specifiers (`extract` 656, 836) | `plugins/python` has no `importlib.import_module("...")` / `__import__` handling (no match for `importlib` in `plugins/python/src`). Go/Rust have no dynamic import. | grep-level |
| D-2 | Getter and setter of one name stay two nodes (`extract` 217) | `plugins/python/src` has no `setter` handling; `@property` + `@x.setter` likely collide on one name. | grep-level; verify with a fixture |
| D-3 | Indexing never writes under the project tree and never reaches the network (`security` 339, 420) | No other plugin asserts either. `plugins/python/src/semantic.rs` documents that its server probe can reach the network (`npx`); rust-analyzer can write `target/`. | documented gap |
| D-4 | The semantic server never executes project-configured code (`security` 184) | rust-analyzer runs build scripts and proc macros by default; `plugins/rust/src/semantic.rs` pins only `checkOnSave = false`. | to verify |

Checked and not a gap: scope shadowing (all three have scope handling and
tests), type-parameter shadowing (Rust
`locals_parameters_and_generics_never_become_placeholders`). Unverified, worth
one look before filing: type arguments as references (`extract` 1641-1698) in
Go (`F[T]`) and Python (`list[Foo]`, `Generic[T]`); a top-level call
degrading to a File-level usage (`extract` 1042, 1163, 1551) in Python and Go.

## Per-file classification

Column "Lives on as": `ts-unit` - Rust test in the TypeScript crate;
`sem-unit` - Rust test of its semantic tier; `sdk ...` - owned by
`plugins/sdk` (GM-324 checks coverage; "add test" marks the three gaps
gm-350 section 8 found); `kit-check` - a built-in `g-mesh plugins check`
session check; `kit: ...` - an existing `expect.toml` entry; `E<n>` - new
entry above; `core ...` - an existing core integration test. For the 90 tests
gm-350 section 8 classified, its buckets map as S-port -> B (A where bench
evidence exists), S-sdk -> B, G349 -> D, Moot -> C.

| File | A | B | C | D | Total |
|---|---|---|---|---|---|
| `bulkIndex.test.ts` | 4 | 13 | 5 | 6 | 28 |
| `e2e.test.ts` | 0 | 8 | 0 | 0 | 8 |
| `extract.test.ts` | 20 | 57 | 3 | 3 | 83 |
| `ignorePolicy.test.ts` | 1 | 5 | 3 | 0 | 9 |
| `incremental.test.ts` | 0 | 19 | 7 | 0 | 26 |
| `jsonrpc.test.ts` | 0 | 0 | 10 | 0 | 10 |
| `protocol.test.ts` | 0 | 6 | 0 | 0 | 6 |
| `qualifiedPath.test.ts` | 2 | 3 | 1 | 0 | 6 |
| `resolve.test.ts` | 9 | 23 | 1 | 0 | 33 |
| `runtime.test.ts` | 0 | 0 | 3 | 0 | 3 |
| `security.test.ts` | 0 | 4 | 2 | 3 | 9 |
| `semantic.test.ts` | 0 | 6 | 4 | 0 | 10 |
| `semanticPass.test.ts` | 2 | 22 | 7 | 2 | 33 |
| `tsconfigPaths.test.ts` | 1 | 13 | 1 | 0 | 15 |
| `workspace.test.ts` | 8 | 20 | 0 | 5 | 33 |
| **Total** | **47** | **199** | **47** | **19** | **312** |

### `bulkIndex.test.ts` (28)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 130 | streams valid NDJSON matching the wire contract for a small project | B | kit-check (shape, stream order) |
| 161 | toWireNode nests flat range fields | C | - |
| 205 | toWireNode carries a multi-declaration node's list across unchanged | C | - (SDK wire types; core overload_declaration_storage) |
| 234 | toWireNode leaves an ordinary node's wire line exactly as it was | C | - |
| 259 | always skips node_modules, dist, .git, and .claude regardless of .gitignore | A | sdk walk + manifest exclude_dirs |
| 286 | respects a root .gitignore excluding a specific file | B | sdk walk |
| 301 | respects a nested .gitignore scoped to its own subdirectory | B | sdk walk (add test) |
| 317 | a negation pattern re-includes a path excluded by an earlier rule | B | sdk walk |
| 332 | a file with syntax errors still contributes its recoverable nodes | B | ts-unit; core cli_status |
| 351 | an empty project directory produces zero output lines | B | sdk |
| 363 | a project with only unsupported file types produces zero output lines | B | sdk |
| 378 | a project where every file is gitignored produces zero output lines | B | sdk |
| 393 | does not buffer: the sink is invoked once per emitted node/edge, grouped per file in walk order | C | - |
| 426 | accepts a real NodeJS.WritableStream sink, one write() call per line | C | - |
| 467 | a symlinked package directory is walked, and indexed under its own apparent path | D | sdk walk (GM-349) |
| 493 | two paths onto the same real directory index it exactly once, sorted-first path winning even when that is the symlink | D | sdk walk (GM-349) |
| 505 | a symlinked file aliasing an already-walked real file is skipped, not indexed twice | D | sdk walk (GM-349) |
| 525 | a symlink pointing back at its own containing directory does not loop forever | D | sdk walk (GM-349) |
| 541 | a symlink resolving outside the project root is refused | D | sdk walk (GM-349) |
| 558 | a dangling symlink is skipped rather than throwing | D | sdk walk (GM-349) |
| 591 | relative imports are resolved against the real tree, off-workspace packages are not | A | ts-unit; E1, E4 |
| 634 | an import of a gitignored file stays an unresolved placeholder, matching the walk's own exclusion policy | B | ts-unit (existence set, ADR 0023) |
| 654 | a relative import into a hard-excluded directory is also treated as unresolved | B | ts-unit (existence set) |
| 676 | a workspace package import resolves to the package's source, symbols included | A | E1, E2 |
| 717 | a workspace package's declared entry under dist/ does not shadow its source, even though dist physically exists | A | E1, E2 |
| 739 | a workspace package's declared entry under a gitignored (not hard-excluded-named) directory does not shadow its source | B | ts-unit |
| 764 | a `#private` import resolves to the real file its package.json `imports` map names | B | E12 |
| 781 | a `#private` import with no matching `imports` key stays an unresolved placeholder, not a crash | B | ts-unit |

### `e2e.test.ts` (8)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 54 | plugin completes handshake with the expected protocol version and language | B | kit-check (session) |
| 69 | plugin parses a framed reindex request end to end and responds | B | kit-check |
| 96 | plugin handles a fileChanged notification without crashing and without responding | B | sdk |
| 135 | --bulk-index streams the project as NDJSON on stdout and then exits | B | kit-check |
| 180 | malformed JSON body does not crash the plugin | B | sdk framing |
| 209 | plugin answers a semanticPass request with a diff-shaped result | B | kit-check |
| 246 | a whole-project semanticPass (empty filePaths) is answered the same way | B | kit-check |
| 281 | a semanticPass that could not cover a file answers incomplete, with a reason, beside the diff | B | sdk semantic |

### `extract.test.ts` (83)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 75 | extracts the documented node kinds from a TypeScript file | A | ts-unit |
| 100 | extracts SUPERTYPE_OF, CALLS and IMPORTS edges | A | kit: add/Greetable entries |
| 120 | an edge onto a declaration of this file is resolved; one onto a placeholder is not | B | kit-check (same-file rule) |
| 140 | File node DEFINES every symbol and EXPORTS only the exported ones | A | ts-unit (no outline in kit) |
| 163 | module-level Variables and namespace Modules are extracted, locals are not | A | ts-unit; E1, E9 |
| 203 | `export { name }` marks an already-declared symbol as exported | B | ts-unit |
| 217 | a getter and a setter sharing a name stay distinct nodes | D | ts-unit; D-2 (Python @property setter) |
| 254 | two overload signatures and their implementation are one node with a declaration each | B | kit: definition format |
| 287 | an overloaded function reports the first call signature, not the implementation's | B | ts-unit |
| 316 | an edge that binds no declaration keeps the id it has always had | B | ts-unit |
| 325 | a call bound to one overload gets an id of its own | B | kit: callers format files |
| 339 | the structural pass never binds a declaration on its own | B | ts-unit |
| 354 | a symbol declared once carries no declaration list at all | B | core overload_declaration_storage |
| 365 | an overloaded method and its implementation stay one node, as tsserver's outline has it | B | ts-unit |
| 385 | an interface method is a method too, so it cannot collide with an implementing class's | B | ts-unit |
| 400 | a merged interface stays one node whose range is its first declaration | B | E13 |
| 428 | a namespace merged across statements keeps one node and both statements' members | B | ts-unit |
| 448 | an overload set with no implementation takes its range from the first signature | B | E15 |
| 462 | flags syntax errors on a partially broken file without dropping what parsed | B | ts-unit; core cli_status |
| 485 | a raw NUL inside a string or template literal is not a syntax error | B | ts-unit |
| 499 | a raw NUL outside any literal is still a syntax error | B | ts-unit |
| 504 | text read back from a literal keeps its raw NUL | B | ts-unit |
| 510 | node ids survive edits elsewhere in the file | B | kit-check (bulk repeat, incremental matches bulk) |
| 530 | handles .jsx/.js with the JavaScript grammar, including require() | B | ts-unit; kit: imports util.js |
| 557 | rejects files this plugin does not own | C | - (SDK routes by manifest extensions) |
| 582 | without a resolver every import target stays a raw-specifier placeholder | C | - (Node API shape) |
| 593 | a resolved specifier becomes a placeholder addressed by the path it names | B | ts-unit |
| 613 | a bare specifier and a dangling relative one keep the old placeholder behaviour | B | ts-unit |
| 626 | specifier resolution never claims an IMPORTS edge is resolved - that is core's call | B | ts-unit |
| 656 | a template specifier folds through the same-file constants it interpolates | D | E11; D-1 (Python importlib) |
| 674 | a named string enum member folds; nothing else about an enum does | B | ts-unit |
| 696 | a conditional specifier records one import per branch | B | ts-unit |
| 710 | a conditional with one dynamic branch records neither branch | B | ts-unit |
| 724 | path.join(__dirname, ...) is a relative specifier spelled the long way | B | ts-unit |
| 741 | the path module is recognised however it was bound, and only it | B | ts-unit |
| 771 | a computed specifier that is not statically known records no edge at all | A | E11 |
| 800 | a literal made of parts is never truncated into a specifier | B | ts-unit |
| 820 | a local binding shadows the constant a specifier would otherwise fold to | B | ts-unit |
| 836 | a folded specifier is an ordinary import edge, resolved by the same handshake | D | E11; D-1 |
| 858 | a require() that is no import is still a call of a name this file may declare | B | ts-unit |
| 871 | two specifiers naming the same file collapse into one placeholder | B | ts-unit |
| 903 | a call to an imported function becomes a CALLS edge onto a pending symbol | A | kit: callers add |
| 926 | an aliased import is addressed by the name the target file exports | A | kit: callers add (addViaBarrel) |
| 942 | a default import is addressed as `default` | A | E7 |
| 957 | an imported type used as a supertype becomes a SUPERTYPE_OF edge | A | E6 |
| 974 | imported names in type positions and JSX become REFERENCES edges | A | E6 (references Item) |
| 991 | a local declaration shadows an import of the same name | B | ts-unit |
| 1013 | an unused import and an unresolvable specifier produce no pending symbol | B | ts-unit |
| 1027 | every usage of one imported symbol shares a single placeholder | B | kit: callers add files |
| 1042 | a call to an import at module top level degrades to a usage edge | B | ts-unit |
| 1083 | a call inside a callback is attributed to the symbol the callback was written into | A | E9 |
| 1104 | a callback inside a function still attributes its calls to that function | A | E9 |
| 1121 | every way of writing a function as a value carries its calls, not just arrows | A | E9 |
| 1145 | a callback in a class field attributes its calls to the class | B | E9 |
| 1163 | a callback at module top level still degrades to a usage edge | B | E9 (excluded_references) |
| 1179 | locals declared inside a callback stay out of the graph | B | ts-unit |
| 1216 | a barrel records what it publishes and where each name really lives | A | kit: callers add (export *); E1 |
| 1243 | a whole-module re-export is addressed by the name no symbol can have | A | ts-unit |
| 1251 | `export * as NS from` binds a namespace, so it is not a re-export of names | B | ts-unit |
| 1260 | a re-export of a specifier that resolves to nothing records no placeholder | B | ts-unit |
| 1271 | a local `export { name }` still marks the declaration, not a re-export | B | ts-unit |
| 1282 | a re-export binds nothing locally, so it cannot be mistaken for a declaration | B | ts-unit |
| 1301 | parses files larger than the native parser's default read buffer | C | - (Node binding read buffer; keep one large file in a ts-unit fixture) |
| 1344 | `${label}` shadowing a file-level function suppresses the call edge (x8: parameter, local const, nested function, hoisted var, destructured local, catch param, for...of binding, callback param) | B | E10; ts-unit (8 cases) |
| 1350 | shadowing confined to one block leaves a call outside it resolved | B | ts-unit |
| 1368 | a function's own name is not shadowed by itself, so recursion resolves | B | ts-unit |
| 1381 | a bare name never resolves to a class member, which only a receiver can address | A | E5 |
| 1403 | a member is still reached through a receiver that names its owner | A | E5 (Store.drop) |
| 1422 | a local shadowing an import claims neither the import nor a same-named declaration | B | ts-unit |
| 1439 | a method whose name matches an import calls the import, not itself | B | E5 |
| 1459 | an edge onto a symbol this file declares is resolved, one onto an imported symbol is not | B | ts-unit |
| 1497 | a namespace import emits no edge of its own, but records the member sites | A | kit: callers double |
| 1535 | a namespace member read outside a call is recorded as a reference | B | E14 |
| 1551 | a namespace call at module top level degrades to a reference from the file | B | ts-unit |
| 1567 | a namespace import of a specifier outside this project records nothing | B | ts-unit |
| 1584 | a local binding shadowing a namespace import is not a namespace member access | B | ts-unit |
| 1599 | an ordinary property access on a value is not mistaken for a namespace member | B | ts-unit; kit: callers Greetable#greet |
| 1614 | recording namespace sites leaves the edges a named import already resolved alone | B | ts-unit |
| 1641 | a generic type's head is a reference, exactly as the same name written bare is | A | E6; core generic_reference_resolution |
| 1673 | type arguments in a heritage clause are references, and the head stays only a supertype | A | E6 |
| 1698 | type arguments at a call and a `new` site are references | B | ts-unit |
| 1728 | a type parameter shadows a file-level type of the same name | B | ts-unit |
| 1761 | a type parameter shadows a type of that name and nothing else | B | ts-unit |

### `ignorePolicy.test.ts` (9)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 29 | hasHardExcludedSegment sees a hard-excluded name at any depth | C | - |
| 37 | hasHardExcludedSegment only matches whole segments | C | - |
| 47 | a hard-excluded directory is non-indexable regardless of depth or .gitignore | A | sdk walk |
| 68 | a root .gitignore excludes the paths it names, and only those | B | sdk walk |
| 88 | a nested .gitignore applies to its own subtree only | B | sdk walk (add test) |
| 105 | a negation re-includes a path an earlier rule excluded | B | sdk walk |
| 120 | a deeper .gitignore's negation overrides a broader rule from the root | B | sdk walk (add test) |
| 136 | a project with no .gitignore anywhere calls everything outside a hard-excluded dir indexable | B | sdk walk |
| 172 | plugin.toml's exclude_dirs equals HARD_EXCLUDED_DIRS minus the baseline | C | - |

### `incremental.test.ts` (26)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 108 | a first sighting of a file reports the whole extraction as added | B | sdk diff |
| 140 | forgetting a file makes the next reparse a full extraction again | B | sdk |
| 151 | an unsupported extension is rejected rather than silently cached | C | - |
| 159 | swapping a callee inside one body changes only that function's edges | B | sdk diff |
| 175 | widening one function's body emits only that function, as remove-old + add-new | B | sdk diff |
| 210 | a whitespace-only edit that shifts nothing produces an empty diff | B | kit-check (whitespace edit) |
| 231 | whitespace appended past the last line moves only the File node's range | B | sdk diff |
| 243 | a notification for text identical to the cached copy is an empty diff | B | sdk diff |
| 265 | adding a function reports it as purely added | B | sdk diff |
| 289 | deleting a function reports it as purely removed | B | core incremental_matches_full_reindex |
| 311 | renaming a function is a removal of the old id plus an addition of the new | B | core incremental_matches_full_reindex |
| 325 | an incremental reparse yields exactly what a full parse of the new text would | B | kit-check (incremental matches bulk) |
| 374 | a file edited into a syntax error keeps reparsing and flags it | B | ts-unit |
| 416 | editing one overload signature reports the symbol as changed | B | kit-check (declaration edit applies) |
| 436 | an edit elsewhere leaves an overloaded symbol out of the diff entirely | B | sdk diff |
| 456 | reparsing identical text with an overloaded symbol is an empty diff | B | sdk diff |
| 464 | deleting an overload leaves a shorter list, not a stale one | B | kit-check (declaration edit applies) |
| 479 | reparseChangedFile reads the project-relative path and keys state by it | C | - |
| 502 | a deleted file removes what this process had and forgets it, so a re-creation is a full extraction | B | core incremental_matches_full_reindex |
| 532 | a deleted file with nothing cached answers an empty full extraction | B | sdk |
| 544 | reparseChangedFile resolves relative imports against the project on disk | B | ts-unit (file_presence_changed, ADR 0023) |
| 584 | computeSourceEdit returns null for identical text | C | - |
| 589 | computeSourceEdit spans exactly the replaced region, on both strings | C | - |
| 607 | computeSourceEdit handles a multi-line insertion | C | - |
| 619 | computeSourceEdit keeps prefix and suffix from overlapping | C | - |
| 644 | computeSourceEdit does not cut a surrogate pair in half | C | - |

### `jsonrpc.test.ts` (10)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 18 | encodeFrame produces the exact LSP wire format | C | - |
| 23 | FrameReader parses a single frame delivered whole | C | - |
| 36 | FrameReader reassembles a frame split across many small chunks | C | - |
| 56 | FrameReader reads consecutive frames in order | C | - |
| 72 | headers other than Content-Length are ignored | C | - |
| 82 | a header block without Content-Length throws instead of hanging | C | - |
| 102 | consecutive tsserver frames parse despite Windows' off-by-one length | C | - |
| 119 | padding between frames is tolerated even when split across chunks | C | - |
| 133 | a malformed header line (no colon) throws | C | - |
| 138 | an unparsable Content-Length value throws | C | - |

### `protocol.test.ts` (6)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 15 | a semanticPass naming files parses into a string list | B | sdk |
| 26 | an empty filePaths list is valid - it means the whole project | B | sdk |
| 36 | semanticPass without filePaths is rejected | B | sdk |
| 44 | semanticPass with a non-string entry is rejected | B | sdk |
| 52 | semanticPass with filePath (singular) is rejected | B | sdk |
| 60 | adding semanticPass did not loosen the other methods | B | sdk |

### `qualifiedPath.test.ts` (6)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 66 | every declared node carries a qualifiedPath that joins back and ends in its name | A | E5 (definition Store#pick) |
| 90 | instance members are joined by `#`, static ones by `.` | A | E5; kit: refusal Greetable.greet |
| 110 | a #private member keeps its `#` in the name, not the separator | B | ts-unit |
| 129 | namespaces nest by `.`; a dotted or quoted module name is one segment | B | E15 |
| 146 | the wire node carries qualifiedPath only when the node has one | C | - |
| 159 | a reparse that changes only a node's path re-sends that node | B | sdk diff |

### `resolve.test.ts` (33)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 34 | a specifier that already names a source file resolves to itself | B | ts-unit |
| 39 | an extensionless specifier picks up the source extension | A | ts-unit; E4 |
| 46 | TypeScript's own extensions win over the JS ones for the same stem | B | ts-unit |
| 56 | an ESM `.js` specifier resolves to the `.ts` source it is compiled from | A | ts-unit; E4 |
| 62 | a real `.js` file next to TS sources still resolves to itself | B | ts-unit |
| 67 | the other emitted-extension pairs substitute the same way | B | ts-unit |
| 73 | a directory specifier resolves to its index file | A | ts-unit; E4 |
| 83 | a file wins over a same-named directory's index | B | ts-unit |
| 88 | `..` segments are resolved against the importing file's directory | A | ts-unit |
| 94 | relative resolution never claims a bare or package specifier | B | ts-unit |
| 103 | a dangling relative import resolves to nothing rather than throwing | B | ts-unit |
| 111 | a specifier climbing out of the project root resolves to nothing | B | ts-unit |
| 122 | a target this plugin does not parse is not claimed as resolved | B | ts-unit |
| 131 | an importer at the project root resolves against the root | B | ts-unit |
| 139 | the fs-backed predicate answers about real files under the project root | C | - |
| 162 | the fs-backed predicate treats a hard-excluded directory as non-existent even when the file is there | B | sdk walk / existence set |
| 178 | the fs-backed predicate treats a gitignored file as non-existent | B | sdk walk / existence set |
| 219 | a workspace package resolves to the entry file it actually has | A | ts-unit; E2 |
| 224 | a declared entry wins when the build output is really there | B | ts-unit |
| 229 | an exports map decides the entry, for the package root and its subpaths | A | ts-unit |
| 235 | a subpath with no exports entry falls back onto the package's source tree | A | ts-unit; E1 |
| 240 | a package outside the workspace stays unresolved | B | ts-unit |
| 255 | a workspace package whose entry is missing is not invented | B | ts-unit |
| 259 | a workspace with no packages resolves nothing | B | ts-unit |
| 266 | the project resolver resolves workspace imports and leaves real packages alone | A | ts-unit (fixture tree); E1 |
| 316 | a paths alias resolves through the same extension guessing as a relative import | B | ts-unit |
| 332 | the workspace answer wins over an alias for the same specifier | A | ts-unit |
| 348 | the project resolver resolves aliases and workspace packages side by side | B | ts-unit (fixture tree) |
| 381 | an alias is refused when its target is outside the project or unparseable | B | ts-unit |
| 425 | a `#` specifier resolves through the importing file's own package imports map | B | ts-unit |
| 436 | a `#` specifier is refused when the importing file has no enclosing imports map | B | ts-unit |
| 442 | the project resolver resolves a `#private` import to the real file it names | B | ts-unit; E12 |
| 460 | the project resolver leaves an unmatched `#private` import unresolved | B | ts-unit |

### `runtime.test.ts` (3)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 19 | a tsc build is not the self-contained one | C | - |
| 24 | a dev build spawns a script the way `node <script>` always did | C | - |
| 32 | the dev build passes no interpreter flag of its own | C | - |

### `security.test.ts` (9)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 89 | isSupportedFile rejects tsconfig.json outright | C | - |
| 94 | a malicious tsconfig.json `plugins` entry is never loaded/executed during indexing | B | ts-unit (structural never runs config code) |
| 184 | the tsserver child does not execute a malicious tsconfig.json `plugins` entry either | D | sem-unit (server launch flags); D-4 |
| 247 | no source file imports or invokes a networking API | B | dependency audit test |
| 261 | semantic.ts spawns only a tsserver, never a shell | B | sem-unit |
| 281 | tsserver is never spawned with plugin loading or typings acquisition enabled | B | sem-unit (server launch flags) |
| 312 | package.json declares no networking-capable runtime dependency | C | - |
| 339 | indexing a real project never calls fetch or node:http(s) request APIs | D | core test, all plugins; D-3 |
| 420 | indexing never creates, deletes, or modifies anything under the project tree | D | core test, all plugins; D-3 |

### `semantic.test.ts` (10)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 90 | a tsserver-style forward-slash path and a path.join-style native path name the same file | C | - |
| 109 | tsserver resolution prefers the project's own TypeScript over the bundled copy | C | - (lost: no bundled TypeScript) |
| 121 | resolveTsserverPath picks the project's own install when it exists, else the bundled one | C | - (lost: no bundled TypeScript) |
| 144 | the tsserver child is not started until a semantic question is actually asked | B | kit-check (lazy engine) |
| 175 | the conformance kit's semantic-engine marker is written when the child starts, not before | B | sdk semantic (marker) |
| 200 | a tsserver that dies fails only the work in flight - the plugin survives and the next query respawns it | B | sdk lsp |
| 237 | resolves a real cross-file declaration in this plugin's own source tree | C | - |
| 276 | respects the project's tsconfig, including a paths alias, when resolving an import | B | server behaviour; E3 structural |
| 323 | a project with no tsconfig.json is still answerable, through an inferred project | B | kit: fixture has no root tsconfig |
| 344 | a missing tsserver is a clean failure, not a crash | B | new core/kit test (no server) |

### `semanticPass.test.ts` (33)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 66 | path.win32.relative tolerates a tsserver-style forward-slash answer against a native-separator project root | C | - |
| 113 | state is per-process, so each test starts from nothing | C | - |
| 120 | `import * as ns` then `ns.someExport()` resolves to the declaration tree-sitter could not see | A | kit: callers double |
| 161 | the address comes from the checker, so an aliased re-export lands on the real declaration | B | kit: callers double (twice) |
| 186 | a namespace member read outside a call becomes a REFERENCES edge | B | E14 |
| 252 | two `export *` branches offering one name resolve to the branch TypeScript picks | B | E8 |
| 291 | swapping the two `export *` statements swaps the declaration the pass lands on | B | E8 control |
| 310 | a whole-project pass finds the same edge without being told which file to look at | B | kit-check (whole-project pass) |
| 339 | a default export imported under another name is upgraded onto the class it really is | A | E7 |
| 384 | an indirect default, re-exported through a barrel, is followed to the declaration | B | sem-unit |
| 413 | an anonymous default is left to the structural layer, which already matches it | B | ts-unit |
| 446 | one pass answers a namespace use and an ambiguous re-export together | C | - |
| 490 | a member the module does not export is left unresolved, not guessed at | B | sem-unit |
| 515 | a barrel whose branches all end outside the index leaves the edge alone | B | sem-unit |
| 539 | a namespace import of a package is never asked about, so no child is started | C | - |
| 564 | a name the target file declares itself is left to the structural layer | C | - |
| 591 | a project with neither a namespace import nor a barrel costs no tsserver child | C | - |
| 670 | a call of an overloaded function binds the overload TypeScript itself picks | D | kit: callers format (GM-348 done) |
| 729 | one caller calling two overloads keeps both bindings instead of collapsing them | D | kit: callers format files (GM-348 done) |
| 766 | an overloaded method called through `this` binds the matching signature | B | sem-unit |
| 813 | a call of an ordinary function is never asked about, so no child is started | C | - |
| 837 | deleting one of two overloaded calls retracts the binding it left behind | B | core overload_call_binding |
| 890 | a symbol imported both by name and through a namespace shares one placeholder | B | ts-unit |
| 933 | a checker that cannot start leaves the pass empty instead of failing it | B | sdk semantic |
| 966 | a checker that dies costs the pass its answers, not the plugin | B | sdk lsp |
| 1008 | a pass the checker fails on one file for is incomplete, and keeps what it did resolve | B | sdk semantic |
| 1037 | a pass that covers its whole scope is complete | B | sdk semantic |
| 1056 | a file in scope that no longer exists leaves nothing uncovered | B | sdk semantic |
| 1067 | a file in scope that cannot be read makes the pass incomplete | B | sdk semantic |
| 1094 | a file in scope whose extraction throws makes the pass incomplete | B | sdk semantic |
| 1123 | questions left unasked after the checker keeps failing are reported, not dropped | B | sdk semantic |
| 1158 | deleting the call retracts the edge the previous pass wrote | B | sdk semantic |
| 1189 | running the same pass twice is idempotent | B | sdk semantic |

### `tsconfigPaths.test.ts` (15)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 55 | a baseUrl-anchored wildcard alias resolves to the directory it names | B | ts-unit |
| 74 | an exact alias key matches only that literal specifier | B | ts-unit |
| 90 | a key's targets are offered in declaration order | B | ts-unit |
| 105 | every matching key contributes, in the order the config declares them | B | ts-unit |
| 124 | without a baseUrl, targets resolve against the config file's own directory | B | ts-unit; E3 |
| 142 | a config with no paths of its own inherits the ones it extends | B | ts-unit; E1/E3 (extends) |
| 159 | a config's own paths replace an inherited map whole, key by key included | B | ts-unit |
| 181 | a baseUrl without paths of its own leaves an inherited map's directory alone | B | ts-unit |
| 200 | an unreadable or package-named extends entry is skipped, not fatal | A | ts-unit |
| 224 | the nearest config wins over a more distant one | B | ts-unit |
| 245 | comments and trailing commas are read the same as strict JSON | B | ts-unit |
| 279 | a baseUrl climbing out of the project voids the aliases it anchors | B | ts-unit |
| 293 | an alias target climbing out of the project is not offered | B | ts-unit |
| 319 | a jsconfig.json is read the same way, when there is no tsconfig.json | B | ts-unit |
| 352 | the resolved config is shared by every file it governs | C | - |

### `workspace.test.ts` (33)

| Line | Test | Bucket | Lives on as |
|---|---|---|---|
| 57 | a package specifier splits into its name and the subpath it addresses | A | ts-unit; E1 |
| 73 | what is not a package specifier at all is refused | B | ts-unit |
| 85 | the declared entry fields are all offered, most authoritative first | A | ts-unit; E2 |
| 97 | an exports map is read for the subpath asked about | A | ts-unit |
| 109 | a condition map without subpath keys describes the package root | B | ts-unit |
| 117 | a wildcard exports subpath substitutes the matched part | A | ts-unit; E1 (viaSubpath) |
| 125 | the source-tree conventions are offered after whatever the manifest declares | A | ts-unit; E2 |
| 134 | a subpath falls back to the same path inside the package and its src | A | ts-unit; E1 (viaSubpath) |
| 141 | an entry pointing outside the project is not offered | B | ts-unit |
| 151 | pnpm workspace globs are expanded to the packages they name | B | ts-unit |
| 172 | the root package.json `workspaces` field is read the same way | A | ts-unit; E1 |
| 187 | yarn's object form of `workspaces` is read too | B | ts-unit |
| 199 | a `!` pattern removes a package the globs had picked up | B | ts-unit |
| 212 | node_modules is never walked into, so a vendored copy is not a workspace package | A | ts-unit |
| 226 | a project with no workspace manifest has no workspace packages | B | ts-unit |
| 238 | a malformed or nameless manifest is skipped rather than thrown on | B | ts-unit |
| 252 | pnpm-workspace.yaml keys other than `packages` are ignored | B | ts-unit |
| 274 | a flow-sequence `packages` list is read as well | B | ts-unit |
| 291 | a workspace glob matches a symlinked package directory, under the path the glob matched | D | sdk walk (GM-349) |
| 310 | a symlink cycle under a `**` pattern neither hangs nor invents packages | D | sdk walk (GM-349) |
| 335 | a real package directory and a symlink alias of it collapse to one entry, sorted-first winning | D | sdk walk (GM-349) |
| 350 | a workspace-glob symlink resolving outside the project root is refused | D | sdk walk (GM-349) |
| 372 | a dangling symlink under a workspace glob is skipped, the real package still resolving | D | sdk walk (GM-349) |
| 391 | an exact imports-map key resolves to its declared target | B | ts-unit |
| 396 | a wildcard imports-map key substitutes the captured part | B | ts-unit |
| 401 | a specifier with no matching imports-map key resolves to nothing | B | ts-unit |
| 406 | a condition-object imports value is ranked the same way exportsTargets ranks one | B | ts-unit |
| 411 | all matching imports keys contribute, not just the first | B | ts-unit |
| 423 | an imports target escaping the package directory is refused | B | ts-unit |
| 428 | nearest package.json wins: a sub-package's own imports map shadows the root's | B | ts-unit |
| 448 | a plain, non-monorepo project's own root package.json is still read for `imports` | B | ts-unit |
| 469 | a package.json with no `imports` field leaves a `#specifier` unresolved, not inherited from a grandparent | B | ts-unit |
| 492 | an exports map with both an import and a require target, both real files, always picks import | B | ts-unit |

## Appendix: how the answers were found

| Question | Call | Answer |
|---|---|---|
| Is there a shared helper that spawns the TS plugin in core tests? | g-mesh `get_file_outline core/tests/common/mod.rs` | No; only timeouts, activation, kill and Rust/Python plugin-root helpers. |
| Which core tests construct the bundled plugin in-process? | g-mesh `find_references bundled_manifest` (complete, `hasMore: false`) | `embedding_generation_pipeline.rs`, `incremental_matches_full_reindex.rs`, `overload_declaration_storage.rs`, `plugin_crash_recovery.rs`, `repeated_edits_through_a_warm_plugin.rs` (+ in-crate tests in `core/src/daemon/plugin/tests.rs`, `core/src/mcp/*`). |
| Same, via the language constant | g-mesh `find_references BUNDLED_LANGUAGE` | the same three `core/tests` files that build a `DiscoveredPlugins` by hand, plus `plugin_pid_path_in`. |
| What `bundled_manifest` runs | g-mesh `find_definition bundled_manifest`, `installed_plugin_executable` | `plugin_entry_path()` + `launch_command_for()`; installed layout looks for the bundled executable under the installed plugin root. |
| Where `receiver_calls` is asserted | g-mesh `find_references ReceiverCallResolution` | in `core/tests` only `plugin_check.rs`; in-crate `mcp/{provenance,instructions,semantic_pending_tests,untyped_tests}.rs`. |
| Which core tests run the TS plugin through the daemon | grep (fixture strings, not symbols): `\.(ts\|tsx\|js\|mts)"` per file, then `semantic`, `node`, `dist`, `G_MESH_*` per file | the 47-file table above. |
| Env hooks inside the TS plugin and their SDK twins | grep `G_MESH_[A-Z_]+` in `plugins/typescript/src`, `plugins/sdk/src` | `G_MESH_PLUGIN_HOLD_DIR`, `G_MESH_PLUGIN_CHECK_MARKER_DIR`, `G_MESH_BULK_STDIN_LIFELINE` all exist in the SDK; `G_MESH_SELF_CONTAINED__` is SEA-only. |
| Kit format | `core/src/cli/plugin_check/expectations.rs` (`ExpectFile`) | entry kinds: callers, references, implementations, imports, importers, definition, refusal; no callees or outline. |
| Bench evidence | `g-mesh-bench/corpora/{registry.json,excalidraw/tasks.json,task-tracker-mcp/tasks.json}` (JSON, read with python); the corpus checkouts at the registry revisions for `tsconfig*.json`, `package.json`, `export *` counts and import spellings | the bucket A table. |
| Other plugins' capabilities (bucket D) | grep in `plugins/{python,rust,go}` for `importlib`, `setter`, shadowing/generic test names, `network`/`checkOnSave` | the D table; grep-level, flagged as such. |
