# GM-351: core without the Node plugin

Design note, slice GM-351/S1. Branch `feat/GM-324-325-ts-port` at `ca38bb7`.
GM-324 made `plugins/typescript/plugin.toml` spawn
`${G_MESH_BIN_DIR}/g-mesh-plugin-typescript` (`semantic_pass = false`); core's
production code is unchanged and still carries a second, hand-written view of
the Node plugin. This note decides how that view goes.

**Scope.** 4.0.0 removes Node from *core*: no production path, extension,
launcher or build command that assumes a Node plugin. It does not remove Node
from the *repository* or from CI: pyright and (GM-325) the TypeScript language
server are npm installs (`scripts/test-deps.sh`), and g-mesh-bench's harness is
TypeScript. Test fixtures may keep using Node where that is the cheaper tool;
production code may not.

## Evidence (g-mesh calls on the main checkout's index)

| Question | Call | Answer |
|---|---|---|
| callers of `bundled_manifest` | `find_callers` | prod: `bundled_fingerprint` only. Tests: `plugin/tests.rs` (5), `mcp/query_shapes/tests.rs` (2), `mcp/semantic_rung_worker_tests.rs` (1), and `core/tests/` `plugin_crash_recovery`, `overload_declaration_storage`, `embedding_generation_pipeline`, `incremental_matches_full_reindex`, `repeated_edits_through_a_warm_plugin` |
| callers of `plugin_entry_path` | `find_callers` | `bundled_manifest` only |
| callers of `launch_command_for` | `find_callers` | `bundled_manifest` + 3 tests in `plugin/tests.rs` (369, 382, 394) |
| refs to `BUNDLED_PLUGIN_EXE` | `find_references` | `installed_plugin_executable` only |
| refs to `PLUGIN_PATH_ENV` | `find_references` | `plugin_entry_path`, `core/tests/plugin_build_staleness.rs` (3) |
| callers of `bundled_fingerprint` | `find_callers` | `build_stamp::of_running_process` + one test |
| who reads the build stamp | `find_references` on `of_running_process`, `build_stamp::read` | `shim::incumbent`, `cli::status::build_state`, `daemon::run`, `core/tests/daemon_build_staleness.rs` |
| callers of `missing_node_entry_hint` | `find_callers` | `missing_plugin_binary_hint` (called by `PluginState::spawn` and `bulk_index::walk_one_language`), `manifest/tests.rs::the_bundled_plugins_handshake_reports_the_version_its_manifest_declares` |
| refs to `test_plugin` helpers | `find_references` on `test_plugin::install` | 18 call sites in `bulk_index.rs`, `lifecycle/tests.rs`, `registry/tests.rs`, `mcp/instructions/tests.rs`, `semantic.rs`, `workspace_reindex.rs` - all in-crate `#[cfg(test)]` |

Grep was used for non-code and for one known file each: `G_MESH_JS_TS_PLUGIN_PATH`
(also documented in `README.md` 313-320 and `plugin-modularity.md`; g-mesh-bench
mentions it only in a comment), `ts_build_stamp` (`core/build.rs`,
`core/Cargo.toml` build-dep, `core/tests/ts_build_stamp.rs`), and the 19
`core/tests/*.rs` whose module comment says "`core/build.rs` runs npm".

## Decisions

### 1. `test_plugin.rs` stays a Node fake, with a corrected justification

It is `#[cfg(test)]`, writes its own self-contained JS (no `dist/`, no npm),
and spawns `command = "node"` from a temp `plugin.toml`. It is a fixture, not a
Node *plugin* path in core, so it does not violate the 4.0.0 criterion.

What changes: only its module doc, whose "Node, rather than a shell script"
paragraph (lines 32-37) argues from `core/build.rs`'s npm build and the real
plugin being Node - both false after this task. The new reason: Node is on
every CI runner for pyright and the TS language server, and the fake is ~760
lines of behaviour (handshake gate, stall, incomplete-once, gated semantic pass,
frames log, bulk stream, sig/doc files) whose `install_memory_hungry`
calibration is measured against Node's idle footprint.

What does not: the JS body, `command = "node"`, every helper. Porting it is a
backlog task (Q1), cheap once item 6's Rust fake exists.

### 2. `daemon/plugin.rs`

| Item | Fate |
|---|---|
| `BUNDLED_PLUGIN_EXE` | **Deleted.** The plugin is found by discovery like every other plugin; GM-335's `resolve_exe_suffix` adds `.exe`. |
| `plugin_entry_path` | **Deleted.** Its only caller is `bundled_manifest`. |
| `installed_plugin_executable` | **Deleted.** |
| `launch_command_for` | **Deleted**, with its 3 tests (`plugin/tests.rs` 369-405). |
| `PLUGIN_PATH_ENV` (`G_MESH_JS_TS_PLUGIN_PATH`) | **Deleted.** `G_MESH_PLUGIN_ROOTS_OVERRIDE` is the general override and already what discovery honours. README 313-320 and `plugin-modularity.md` lose the variable (user-visible; release note). |
| `bundled_manifest()` | **Deleted from production.** Tests get two helpers (item 5). |
| `bundled_fingerprint()` | **Replaced** by a digest over the *discovered* plugins (below). |
| `missing_node_entry_hint` | **Deleted.** It names `npm ci && npm run build`. `missing_plugin_binary_hint` collapses to `missing_workspace_binary_hint` (keep the wrapper name so both spawn sites stay untouched). A third-party `command = "node"` plugin with a missing script now gets the generic handshake error - accepted. |
| entry-exists check | Goes with the hint above; the workspace-binary check stays. |
| `BUNDLED_LANGUAGE` | **Stays** (pid-file name); doc comment drops `index.ts`. |
| `fingerprint`, `digest_of_plugin_build`, `BASELINE_FINGERPRINT_IGNORE` | Stay; `fingerprint`'s doc drops the `npm run build`/`dist/` example. `node_modules` stays in the baseline ignore (pyright installs into `plugins/python/node_modules`). |

**What the staleness fingerprint covers now.** Today it hashes
`entry.parent()` = `plugins/typescript/dist/src`, which disappears with the npm
package. With no `dist/` the hash becomes `FINGERPRINT_UNAVAILABLE` on both
sides and the check goes silently dead.

Proposed: `build_stamp`'s `plugin` field = `registry::plugins_digest` over
`manifest::discover(&manifest::default_roots())`, memoized per process, and
`FINGERPRINT_UNAVAILABLE` if discovery fails. This is the same digest that
`indexer_version` already computes. It covers each plugin's `manifest_dir`: in
a checkout that is the plugin's sources (`plugins/typescript/{Cargo.toml,src,...}`);
in an archive it is the staged `plugin.toml` plus binary
(`bundle-rust-plugin.sh` layout).

Why all plugins and not just TypeScript: TypeScript was singled out only
because it was the one plugin built outside cargo. Now python/rust/typescript
are all workspace binaries that change without the core executable's mtime
changing, so a TS-only digest would be an arbitrary survivor of the Node era.
Alternative A (narrower): fingerprint only the discovered `typescript`
manifest. See Q2.

### 3. `build_stamp.rs` and `core/build.rs`

- `build_stamp.rs`: module doc lines 41-56 ("built by `npm`",
  `npm run build`) are rewritten to talk about workspace plugin binaries;
  `of_running_process` (145-158) takes the discovered-plugins digest. The mtime
  comparison is on the core exe and was never against an npm file. Only the doc
  says otherwise.
- `core/build.rs`: delete `NPM` (45-48), `include!("ts_build_stamp.rs")` (73),
  the `build_ts_plugin` call and `build_ts_plugin` itself (82-153); rewrite the
  header (1-33) for Go only. Delete `core/ts_build_stamp.rs`,
  `core/tests/ts_build_stamp.rs` and `[build-dependencies] sha2` in
  `core/Cargo.toml` (its comment says it exists only for the TS stamp). Nothing
  replaces the build: `g-mesh-plugin-typescript` is a workspace member, and a
  missing binary already gets `missing_workspace_binary_hint`'s
  `cargo build --workspace`.
- The 19 `core/tests` module comments that say "`core/build.rs` runs npm" become
  "needs `cargo build --workspace`". These are doc comments only. Each site is
  edited by hand, not by a sweep.

### 4. Retiring the package.json gates (GM-303)

GM-303's rule (`docs/architecture/plugin-modularity.md`, "`plugin_version`:
two rules, not one"): a plugin that is a cargo-workspace member has
`plugin_version` equal to its crate version, which equals the release version.
TypeScript now falls under it (`plugins/typescript/Cargo.toml` = `4.0.0`).

- `scripts/cut-release.sh` `check_self_versioned_plugin_versions` (338-368): drop
  the TypeScript half (package.json read); keep Go. Add
  `plugins/typescript/plugin.toml` to `CRATE_BACKED_PLUGIN_MANIFESTS` (282-290)
  and `plugins/typescript/Cargo.toml` to `OTHER_WORKSPACE_MANIFESTS` (234). Comments
  at 43-78 and 190-192 cite GM-303 and say TypeScript moved rules in 4.0.0.
  This overlaps GM-326's file (Q4).
- `core/src/daemon/manifest/tests.rs`: delete
  `every_declaration_of_the_bundled_plugins_version_agrees` (38-65) and leave a
  comment pointing at GM-303's rule and at the new
  `plugins/typescript` test `the_manifest_version_matches_the_crates` (the
  python/rust precedent, `plugins/python/src/semantic.rs:1291`).
  `the_bundled_plugins_handshake_reports_the_version_its_manifest_declares`
  (86-120) stays. It swaps `missing_node_entry_hint` for
  `missing_workspace_binary_hint`, and its message drops
  `scripts/generate-version.js`.

### 5. Re-pointing the `bundled_manifest()` tests

Two test helpers replace `bundled_manifest()`, each by role:

- `typescript_manifest()`: `read_manifest(CARGO_MANIFEST_DIR/../plugins/typescript)`
  with `capabilities` reset to `Capabilities::default()`, so a test keeps the
  "bare" capabilities it had. This matters because GM-325 turns `semantic_pass` on.
  `bin_dir_of` already steps over `deps/`, so `${G_MESH_BIN_DIR}` resolves to
  `target/<profile>/`. The precedent is `plugin_manifest()` in
  `incremental_matches_full_reindex.rs`. The helper lives in
  `core/tests/common/mod.rs` and, `#[cfg(test)]`, in `daemon/plugin/tests.rs`.
- `bare_manifest(language)` (`#[cfg(test)]`, `daemon/manifest.rs`): a
  field-defaulted `PluginManifest` for tests that only want a struct to spread
  (`..`). These are `query_shapes/tests.rs` 42, 137 and
  `semantic_rung_worker_tests.rs` 298, which must *not* inherit the real
  manifest's `[plugin.non_symbol_queries]` table.

| Test | Re-pointed to |
|---|---|
| `plugin_crash_recovery`, `repeated_edits_through_a_warm_plugin`, `embedding_generation_pipeline`, `overload_declaration_storage` (`only_the_bundled_plugin`), `incremental_matches_full_reindex` (`Language::TypeScript`) | `typescript_manifest()` |
| `plugin/tests.rs` 428, 487, 537 | `typescript_manifest()` |
| `plugin/tests.rs` 343 `..._fingerprintable_from_the_test_binary` | rewritten: the discovered-plugins digest is not `FINGERPRINT_UNAVAILABLE` from the test binary |
| `plugin/tests.rs` 408 `a_checkout_still_resolves_to_the_compiled_javascript_entry_point` | deleted |
| `query_shapes/tests.rs`, `semantic_rung_worker_tests.rs` | `bare_manifest(..)` |

The 5 Node-specific files from the GM-323 inventory:

| File | Resolution |
|---|---|
| `overload_declaration_storage.rs::the_plugins_own_ndjson_mentions_declarations_only_for_the_overloaded_node` (201) | runs `<bin dir>/g-mesh-plugin-typescript[.exe] --bulk-index` (sibling of `CARGO_BIN_EXE_g-mesh`) instead of `node dist/src/index.js` |
| `plugin_build_staleness.rs` | the copy becomes `plugin.toml` only (its command already names the binary by `${G_MESH_BIN_DIR}`). The "rebuild" is a byte change to a file in the copied plugin dir, which `fingerprint` hashes. `PLUGIN_PATH_ENV` goes and the roots override alone drives both halves, which is the point of decision 2 |
| `plugin_check.rs::a_namespace_import_caller_needs_the_semantic_pass_to_resolve` | **already resolved by GM-324**: it copies only `plugin.toml` (lines 1375-1381) |
| `semantic_pass_trigger.rs` | item 6 |
| `ts_build_stamp.rs` | deleted with `core/ts_build_stamp.rs` |

### 6. `semantic_pass_trigger.rs`'s fake in Rust

A new `[[bin]] g-mesh-fake-plugin` in `plugins/sdk` (`fake/main.rs`), beside
the existing test-only `g-mesh-plugin-toy` and `g-mesh-fake-lsp`, built on
`g_mesh_plugin_sdk::framing`. Behaviour is exactly the JS stub's: it appends
each method to `$G_MESH_FAKE_PLUGIN_LOG`, returns an empty diff to
`fileChanged`/`semanticPass` and `{acknowledged:true}` to any other request, and
answers `--bulk-index` with one canned `File` node. Language and version come
from argv/env, so later fakes can reuse it.

How the tests find it: the test's `plugin.toml` says
`command = "${G_MESH_BIN_DIR}/g-mesh-fake-plugin"`, suffix-less. The daemon
under test is `CARGO_BIN_EXE_g-mesh` in `target/<profile>/`, so the
placeholder resolves there and GM-335's fallback adds `.exe` on Windows. That
gives the test a second Windows witness for item 8. Not in `core`, because
`cargo install` would ship a core `[[bin]]`. Not a core dev-dependency on the
SDK, because a library dependency does not build its bins. The cost is that
`cargo nextest run -p g-mesh` alone does not build it. That is already true of
every TS-spawning core test now that the TS plugin is a workspace binary, and CI
builds the workspace. The test asserts the binary exists, with the
`cargo build --workspace` hint, rather than skipping.

### 7. Lifecycle tests and `plugin_pid_path` (interface with GM-325)

`cli_stop` (4), `cli_status` (2), `daemon_sigterm`, `idle_lifecycle` (2),
`plugins_die_with_daemon`, `orphaned_daemon` (4) and `replay_progress` (2) need a
*live* long-lived TS plugin. After a cold start only the post-walk
`semanticPass` spawns it, and `semantic_pass = false` removes that spawn.

From core's side **the waits should hold**. The subject of these tests is "a
live plugin process dies with the daemon", and a wait that passes with no
plugin proves nothing. Core does not need to change. GM-325 owns one
**interface**:

> After a cold-start walk, the TypeScript plugin is spawned for the post-walk
> semantic pass whenever `plugin.toml` declares `semantic_pass = true`, and
> that declaration is static: it does not depend on whether a language server
> resolves. With no server, the plugin still starts and answers with an
> incomplete diff (existing degradation).

If GM-325 keeps that, the waits go green with or without a server on the
runner. If GM-325 makes the capability conditional, or ships without a tier,
then GM-351's tests slice re-points these tests so the plugin is brought up by
a structural action (an edit to a `.ts` file, which spawns the supervisor
through `fileChanged`) instead of the pass. See Q3.

### 8. Windows

The criterion needs a CI run on `x86_64-pc-windows-msvc` at the exact branch
tip, with a push that the owner approves. The run must show:

1. The `tests (x86_64-pc-windows-msvc)` job is green. The JUnit summary shows
   these TS-spawning tests passed, not skipped: `plugin_crash_recovery`,
   `overload_declaration_storage` (including the `--bulk-index` one, which runs
   the `.exe`), `plugin_build_staleness`, `cli_init`, and
   `semantic_pass_trigger` (the fake, via the suffix-less placeholder).
2. Nothing in the job runs `npm` for `plugins/typescript`: no `npm ci`/`npm test`
   step for it, and no `build.rs` npm warning in the cargo log.
3. `grep -rn "BUNDLED_PLUGIN_EXE\|G_MESH_JS_TS_PLUGIN_PATH\|dist/src/index.js" core`
   is empty at that commit. Run this locally. It is the static half of the same
   claim.

### 9. Order on the shared branch

The npm package deletion and `git mv plugins/typescript/rust plugins/typescript/src`
are **one mechanical commit owned by GM-351** (its own code slice). It runs
**after GM-351's core slice** (so core no longer builds or reads `dist/`) and
**before GM-325's TS-tier code slice** (GM-325/S3), which edits files under the
moved directory. The same commit drops `plugins/typescript/{src(TS),dist,test,sea,scripts,package.json,package-lock.json,tsconfig.json}`,
fixes `plugins/typescript/Cargo.toml` paths and `plugin.toml`'s comment, and
removes ci.yml's `plugins/typescript` `npm ci`/`npm test` steps. Without that
last change CI breaks, so the ci.yml edit has to be in the same commit even
though ci.yml is in GM-325's file list. GM-325/S2 (SDK lift: `plugins/sdk`,
`python`, `rust`) shares no files with GM-351's core slice, so the two can run
in parallel, each in its own worktree. GM-351's verify runs after GM-325/S3,
because the 17 semantic files can only go green then.

`scripts/bundle-plugin.sh` builds the SEA from `sea/`. Deleting `sea/` breaks it
unless GM-326 has switched it to the cargo binary (Q4).

```
GM-351/S2 core ──┐
GM-325/S2 SDK  ──┴─> GM-351/S3 delete npm + git mv ─> GM-325/S3 TS tier ─> GM-351/S4 tests ─> GM-325 S4/S5 ─> GM-351/S5 verify
```

## Edit map (worktree line numbers, 1-based)

| File | Lines | Symbol | Change |
|---|---|---|---|
| `core/src/daemon/plugin.rs` | 61-65 | `PLUGIN_PATH_ENV` | delete |
| | 304-310 | `BUNDLED_PLUGIN_EXE` | delete |
| | 312-337 | `plugin_entry_path` | delete |
| | 338-346 | `installed_plugin_executable` | delete |
| | 347-372 | `launch_command_for` | delete |
| | 374-392 | `BUNDLED_LANGUAGE` doc | trim |
| | 393-432 | `bundled_manifest` | delete |
| | 434-486 | `fingerprint` doc | drop npm/dist example |
| | 487-513 | `bundled_fingerprint` | replace (discovered digest) or move to `build_stamp` |
| | 627-674 | `missing_node_entry_hint` | delete |
| | 770-783 | `missing_plugin_binary_hint` | body = workspace hint |
| `core/src/daemon/plugin/tests.rs` | 343, 369-427, 428, 487, 537 | see item 5 | rewrite / delete / re-point |
| `core/src/daemon/registry.rs` | 303 | `plugins_digest` | `pub(crate)` |
| `core/src/daemon/build_stamp.rs` | 41-56, 145-158 | module doc, `of_running_process` | rewrite |
| `core/src/daemon/manifest.rs` | new | `#[cfg(test)] bare_manifest` | add |
| `core/src/daemon/manifest/tests.rs` | 13-65, 86-120 | version-agreement test, handshake test | delete + GM-303 note; swap hint |
| `core/src/mcp/query_shapes/tests.rs` | 42-50, 137-149 | | `bare_manifest` |
| `core/src/mcp/semantic_rung_worker_tests.rs` | 298-308 | | `bare_manifest` |
| `core/src/daemon/test_plugin.rs` | 32-37 | module doc | new justification |
| `core/build.rs` | 1-33, 45-48, 73, 78, 82-153 | header, `NPM`, include, `build_ts_plugin` | delete / rewrite |
| `core/ts_build_stamp.rs`, `core/tests/ts_build_stamp.rs` | all | | delete |
| `core/Cargo.toml` | 183-187 | `[build-dependencies] sha2` | delete |
| `core/tests/common/mod.rs` | new | `typescript_manifest()` | add |
| `core/tests/{plugin_crash_recovery,repeated_edits_through_a_warm_plugin,embedding_generation_pipeline,overload_declaration_storage,incremental_matches_full_reindex}.rs` | callers of `bundled_manifest` | | re-point |
| `core/tests/overload_declaration_storage.rs` | 201-230 | ndjson test | binary, not node |
| `core/tests/plugin_build_staleness.rs` | 1-60, 98-210, 287, 344 | staging, env | re-stage |
| `core/tests/semantic_pass_trigger.rs` | 1-12, 30-106, 111-147 | stub, harness | Rust fake |
| `core/tests/*.rs` (19) | module comments | | npm -> `cargo build --workspace` |
| `plugins/sdk/Cargo.toml`, `plugins/sdk/fake/main.rs` | new | `g-mesh-fake-plugin` | add |
| `plugins/typescript/src/...` | new test | `the_manifest_version_matches_the_crates` | add |
| `scripts/cut-release.sh` | 43-78, 190-192, 234, 282-290, 338-368 | gates | retire TS half; add TS to crate-backed lists |
| `README.md` | 305-325 | plugin resolution order | drop the env var |
| `docs/architecture/plugin-modularity.md` | `G_MESH_JS_TS_PLUGIN_PATH` mentions | | drop |

## Slices (revise S2-S4 of GM-351)

| Slice | Kind | Model | Content | Exit |
|---|---|---|---|---|
| S2 | code | opus | Items 2, 3, 4 and the compile-forced re-pointing in item 5 (helpers + call sites; delete the 4 obsolete tests). README/doc edits. | `cargo build --workspace`; only the touched tests run |
| S3 | code | opus | Item 9: delete the npm package, `git mv rust src`, Cargo paths, ci.yml npm steps. Before GM-325/S3. | workspace builds; `plugins/typescript` crate tests |
| S4 | tests | opus | Item 6 (fake bin + `semantic_pass_trigger`); `overload_declaration_storage` ndjson; `plugin_build_staleness` re-staging; new tests below, each with a control | each control fails with the fix reverted |
| S5 | verify | opus | Controls in their own worktree; nextest `-p g-mesh -p g-mesh-plugin-sdk -p g-mesh-plugin-typescript` once; 17 semantic files; lifecycle tests 5x (once under load); the Windows run (item 8, push asked first) | all green, Windows green |

New tests and their controls (S4):
- The build stamp changes when a discovered plugin's file changes. Control:
  revert `of_running_process` to `bundled_fingerprint`.
- `cut-release.sh` refuses when `plugins/typescript/plugin.toml` drifts from the
  crate version. Control: remove TS from `CRATE_BACKED_PLUGIN_MANIFESTS`.
- `the_manifest_version_matches_the_crates` (typescript). Control: edit
  `plugin_version`.
- `semantic_pass_trigger` passes against the Rust fake. Control: the fake skips
  logging `semanticPass`.

## Risks and trade-offs

- **The discovered-plugins stamp is broader.** Any plugin source edit in a
  checkout now retires a running daemon, not just a TS one. That is correct but
  noisier for someone editing plugins. It is memoized per process and costs the
  same as `indexer_version`, which already runs at daemon start.
- **`G_MESH_JS_TS_PLUGIN_PATH` is a public variable** (README). Removing it
  breaks anyone who points at a private build. `G_MESH_PLUGIN_ROOTS_OVERRIDE`
  replaces it, and this needs a release-note line.
- **Third-party Node plugins lose the "unbuilt entry" hint.** They fall back to
  the generic handshake error.
- **`cargo test -p g-mesh` without a workspace build** now fails more tests
  (TS plugin, fake plugin). It fails with an actionable hint and is unchanged
  in CI.
- **Lifecycle tests depend on GM-325 keeping the static `semantic_pass = true`.**
  If that changes late, GM-351/S4 grows by about 15 test edits.
- **`test_plugin.rs` keeps Node in core's unit tests.** "Core is Node-free" is
  true of production only. Saying so is the honest version of the criterion.
- **`sea/` deletion vs `bundle-plugin.sh`.** The release can break if GM-326
  lags.

## Owner questions

1. **`test_plugin.rs` stays a Node fixture in 4.0.0 (doc-only change), with a
   backlog task to port it onto `g-mesh-fake-plugin`.** Recommend: yes.
2. **The staleness fingerprint covers all discovered plugins (B), or TypeScript
   only (A)?** Recommend B, because nothing left makes TypeScript special.
3. **Lifecycle waits hold, on the interface in item 7 (GM-325 keeps
   `semantic_pass = true` static, and the plugin spawns post-walk with no
   server).** Recommend: yes. GM-351 re-points the tests only if GM-325 cannot
   keep it.
4. **GM-351 edits `scripts/cut-release.sh` (TS into the crate-backed lists) and
   ci.yml's npm steps, though they are in GM-326's and GM-325's file lists; and
   GM-326 must switch `bundle-plugin.sh` off `sea/` before S3 lands.**
   Recommend: yes, and record a dependency GM-351/S3 -> GM-326's bundle change
   (or GM-351/S3 keeps `sea/` until GM-326 lands).
5. **The npm deletion and `git mv` are GM-351/S3, run after GM-351/S2 and before
   GM-325/S3.** Recommend: yes.
6. **Drop `G_MESH_JS_TS_PLUGIN_PATH` outright, with a release note, and no
   deprecation alias.** Recommend: yes. An alias would need a Node-shaped
   meaning that no longer exists.
