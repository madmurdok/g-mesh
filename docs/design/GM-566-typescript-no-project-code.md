# GM-566: the TypeScript semantic tier runs no project code

Status: design (slice S1), awaiting owner decisions D1-D4.
Branch: `feat/GM-566-typescript-no-project-code` (base `release-4.8.0`).
Precedents: [GM-513](GM-513-semantic-servers-no-project-code.md) /
[ADR 0032](../adr/0032-semantic-tiers-execute-no-project-code.md) (no project
code), [GM-512](GM-512-plugins-no-writes-no-network.md) /
[ADR 0033](../adr/0033-semantic-tiers-reach-no-network-and-write-nothing.md)
(no network, no writes). Both ADRs name this task as the TypeScript follow-up.

Every claim is marked **[run]** (observed by running something, section
"How this was established") or **[read]** (read from source).

## Summary

| Channel | Today | Proposed | Evidence |
|---|---|---|---|
| Server binary from `<root>/node_modules/.bin/vtsls` | probed and run when `vtsls` is not on `PATH` | never | [read] |
| `npx --yes --package @vtsls/language-server vtsls` | last candidate; run with the project as cwd it executes the project's own `node_modules/@vtsls/language-server`, and contacts the registry the project's `.npmrc` names | never | [read] here; measured for pyright in ADR 0032 D4 / GM-512 |
| tsconfig `compilerOptions.plugins` | **not loaded** with vtsls's bundled TypeScript | unchanged, now pinned by a test | [run] |
| The project's own `node_modules/typescript` (`tsserver.js`) | **not loaded**: vtsls uses its bundled TS unless `vtsls.autoUseWorkspaceTsdk` is true; then both it *and* the tsconfig plugin run | pin `autoUseWorkspaceTsdk = false` explicitly (D2) | [run] |
| Automatic Type Acquisition (ATA) | **on**: tsserver's typings installer runs `npm install types-registry@latest` and `npm install @types/<dep>` on a JS project | `typescript.disableAutomaticTypeAcquisition = true` | [run] |
| Windows: `vtsls.cmd` resolves `node` through `cmd.exe`, which searches the cwd (= project root) first | a `node.exe` at the project root would run | `NoDefaultCurrentDirectoryInExePath=1` for server and probe (D3) | [read], not observed (no Windows host) |

The user-visible cost: vtsls must be installed globally (`npm install -g
@vtsls/language-server`); JavaScript projects lose types that only ATA would
have downloaded.

## How this was established

Scratch dir `/private/tmp/claude-502/.../scratchpad/gm566` (`run.sh`,
`lsp_drive.py`, `summary.txt`). `lsp_drive.py` is GM-513's minimal LSP
client (answers `workspace/configuration` from the arm's settings; the server
runs with `cwd = <fixture>` as `LspClient::start` does,
`plugins/sdk/src/lsp/client.rs:222-235`), extended to also open a `.js` file.
Server: vtsls 0.3.0 from `plugins/typescript/node_modules/.bin/vtsls` (the
version CI pins, `plugins/typescript/package.json`), bundled TypeScript 5.9.3,
node v20.6.1. `uptime` at start: load averages 118.75 113.61 69.47, so the
timings are not measurements.

Fixture `fx` (fresh copy per arm):

- `tsconfig.json`: `baseUrl`/`paths` `@lib/*` → `src/lib/*`, and
  `compilerOptions.plugins: [{ "name": "evil-ts-plugin" }]`.
- `src/a.ts` imports `double` from `@lib/b`: the definition resolves to
  `src/lib/b.ts` **only if this tsconfig is the one in force** (sanity).
- `node_modules/evil-ts-plugin/index.js` writes `MARK_plugin` at `require()`
  - the package form tsserver can load (the deleted `security.test.ts`
  measured that the relative-path form never loads either way).
- `node_modules/typescript`: a clone of the real 5.9.3 whose `lib/tsserver.js`,
  `lib/_tsserver.js` and `lib/typescript.js` each write `MARK_ts_<file>` when
  loaded.
- `loose.js` (outside `include`, so an inferred JS project) does
  `require("lodash")`; `package.json` depends on `lodash` - the ATA trigger.
- `typescript.npm` points at a logging shim that exits 1, so ATA is observed
  without reaching the registry.

Base settings = today's manifest, `{"typescript": {"tsserver":
{"useSyntaxServer": "never"}}}`, plus the `npm` shim.

| Arm | Extra settings | Markers | ATA `npm` calls | `definition` of `double` |
|---|---|---|---|---|
| t_today | none | none | 2: `install --ignore-scripts types-registry@latest`, `install --ignore-scripts @types/lodash@ts5.9 --save-dev` | `src/lib/b.ts` |
| t_prop | `disableAutomaticTypeAcquisition: true`, `vtsls.autoUseWorkspaceTsdk: false` | none | 0 | `src/lib/b.ts` |
| t_autows | `vtsls.autoUseWorkspaceTsdk: true` | `MARK_plugin`, `MARK_ts_tsserver.js`, `MARK_ts__tsserver.js`, `MARK_ts_typescript.js` | 2 | `src/lib/b.ts` |
| t_tsdk | `typescript.tsdk: "node_modules/typescript/lib"` | none | 2 | `src/lib/b.ts` |
| t_tsdk_prop | t_prop + `tsdk` | none | 0 | `src/lib/b.ts` |

Read: with the bundled TypeScript neither the project's tsserver nor its
tsconfig plugin runs (t_today); the hostile fixture is real, because the
moment the project's TypeScript is in force both run (t_autows). A
client-sent `typescript.tsdk` alone does not switch (vtsls reads only its
*workspace* value, `readLocalTsdk`, `@vtsls/language-service/dist/index.js:4813`).
ATA runs npm twice on a JS file and nothing turns it off today.

Why the tsconfig plugin is inert [read]: tsserver resolves a
`compilerOptions.plugins` name from `--pluginProbeLocations`, then the
`node_modules` beside its own `tsserver.js` (`<tsserver.js>/../../..`); the
project's directory is added only with `--allowLocalPluginLoads`. vtsls 0.3.0
never passes that flag (0 occurrences in `index.js`), and its
`--pluginProbeLocations`/`--globalPlugins` come only from client settings
`typescript.tsserver.pluginPaths` / `vtsls.tsserver.globalPlugins`
(`index.js:12784-12796`, `:15169`), which the manifest does not set. The
bundled TypeScript is `require.resolve("typescript")` from vtsls's own
install directory (`index.js:15540`), never from the project - **as long as
vtsls itself is not installed in the project**, which is exactly the origin
this task removes.

## 1. How the server is located today [read]

- `plugins/typescript/plugin.toml:65-75`: `command = "vtsls"` (bare),
  `args = ["--stdio"]`; the comment documents `PATH` → `node_modules/.bin` →
  `npx`.
- `plugins/typescript/src/semantic.rs:105-118` `prepare` → `:124-133`
  `resolve` → `:136-138` `candidates` → `lsp::npm_candidates(command, root,
  &VTSLS, …)`.
- `plugins/sdk/src/lsp/resolve.rs:127-129` `npm_candidates` →
  `:143-190` `npm_candidates_in(command, Some(root), npx = true, …)`: `PATH`
  spellings, then `<root>/node_modules/.bin/vtsls` (+ `.cmd` on Windows),
  then `npx --yes --package @vtsls/language-server vtsls`.
- `:246-262` `resolve` probes each candidate with `<candidate> --version`
  (`probe`, `:195-229`; inherits the plugin's cwd, normally the session's
  project) and returns the first that answers. So a project-shipped
  `node_modules/.bin/vtsls` is **executed twice** (probe, then server)
  whenever vtsls is not on `PATH`.
- The server starts with `current_dir(root)` and `envs(config.env)`
  (`client.rs:229-235`).

g-mesh (`find_references npm_candidates`, this worktree): the only caller is
`plugins/typescript/src/semantic.rs::candidates`, plus the re-export in
`plugins/sdk/src/lsp/mod.rs:91`. `find_references npm_candidates_on_path`:
`plugins/python/src/semantic.rs::candidates` (GM-513) and the same re-export.
After this task `npm_candidates` has no caller.

## 2. Locating the server without project binaries or npx

| Option | Benefit | Risk |
|---|---|---|
| **(a) `PATH` only: `npm_candidates_on_path`, as python (Recommended)** | One line in the plugin; same rule and same SDK function as pyright; the manifest's explicit-path escape hatch stays. | Users with vtsls only in the project, or relying on `npx`, lose the semantic tier until `npm install -g @vtsls/language-server`. |
| (b) `PATH`, then a g-mesh-owned install dir (e.g. `~/.g-mesh/tools/node_modules/.bin`) | A home for a future `g-mesh install-server`; never the project. | No installer exists; a second directory to document and test, for no user today. |
| (c) Keep `npx` with `npm_config_offline=true`, the registry pinned in env, and a neutral cwd | Users with a warm npx cache keep working. | Still executes a package npm picks; offline only helps a warm cache; GM-512 rejected the same for pyright (ADR 0033). |
| (d) Ship vtsls in the release archive | Zero setup. | Node.js still required, archive size, a second release artefact to keep pinned; outside this task. |

Recommendation: (a). The remedy message names the global install only, as
python's does (`plugins/python/src/semantic.rs:317-326`).

## 3. tsconfig plugins and the project's TypeScript

Nothing in our code loads them today; the guarantee comes from vtsls's
defaults (section "How this was established"). Proposed: keep it, make it
explicit, and pin it with a test:

- `vtsls.autoUseWorkspaceTsdk = false` in `[plugin.semantic.settings.""]`
  (D2). It is vtsls's default; writing it makes the invariant visible and
  survives a vtsls default change.
- Never set `typescript.tsdk`, `typescript.tsserver.pluginPaths`,
  `vtsls.tsserver.globalPlugins` or `typescript.tsserver.nodePath`; the
  manifest unit test asserts the exact settings object, so adding one fails
  it.
- The settings vtsls sees come only from our manifest: the TS plugin adds no
  project-derived setting (unlike python's `add_project_settings`), and vtsls
  reads no `.vscode/settings.json` (it is an LSP server; config is pulled
  over `workspace/configuration`) [read; t_tsdk is consistent with it].

Other project-code paths checked [read]: the structural tier spawns nothing
(no `Command::new` under `plugins/typescript/src`); tsconfig `extends` and
`.d.ts` files are read, not executed; vtsls's shebang `#!/usr/bin/env node`
takes `node` from `PATH` (only a relative `PATH` entry could make that the
project's, which is the user's configuration); on Windows see D3.

## 4. Network stance

- No `npx` (section 2): removes the registry contact on every start
  (GM-512 measured it for pyright: 1-2 requests even with a warm cache, to
  the registry the project's `.npmrc` names).
- **ATA off** (`typescript.disableAutomaticTypeAcquisition = true`): today
  every JS file in an inferred or `jsconfig` project makes tsserver's typings
  installer run `npm install` against the registry [run, t_today]. It runs
  with `cwd` = the global typings cache (outside the root, `--ignore-scripts`;
  `_typingsInstaller.js:126,163` [read]), so it is a
  network path, not project-code execution - but ADR 0033 forbids it.
- vtsls passes `--enableTelemetry` to tsserver (`index.js:12759`); those
  events go to the LSP client, not the network [read]. The tests slice's
  network sink is the proof for the whole server.

## 5. What users lose

1. vtsls installed only inside the indexed project, or never installed (the
   `npx` path): no TypeScript semantic tier until `npm install -g
   @vtsls/language-server`; the structural tier is unchanged; the failure is
   logged once with the remedy (existing `LazyEngine` degradation).
2. ATA: a JavaScript project whose dependencies have no types in its own
   `node_modules` loses the `@types/*` ATA would have fetched. Those types
   describe code outside the index (`node_modules` is excluded), so the loss
   is receiver calls into third-party JS libraries staying unresolved; TS
   projects (`tsconfig.json`) do not use ATA by default.
3. Nothing from the tsconfig/TS-version side: today's behaviour already
   ignores both, and stays so.

## 6. Owner decisions

**D1. Where vtsls may come from.**
Today: `PATH`, then the indexed project's `node_modules/.bin/vtsls`, then
`npx --yes --package @vtsls/language-server vtsls` (run with the project as
cwd, which executes the project's own copy of that package if it has one,
and otherwise downloads one from the registry the project's `.npmrc` names).
Change: `PATH` only, like pyright since GM-513. Example: a repository that
ships `node_modules/.bin/vtsls` as a script that does anything - today it
runs as soon as the user has no global vtsls; after, it never runs, and the
log says "no usable vtsls … npm install -g @vtsls/language-server".
- (a) **`PATH` only, no project origin, no `npx` (Recommended).** Benefit:
  closes both channels with the SDK function python already uses; also
  removes the per-start registry contact. Risk: users without a global vtsls
  lose the TS semantic tier until they install it.
- (b) `PATH` only, but keep `npx` offline-pinned (env registry + offline +
  neutral cwd). Benefit: warm-cache `npx` users keep working. Risk: still
  runs a package npm selects; more env plumbing and tests; ADR 0033 rejected
  it for pyright.
- (c) Keep today's order. Risk: the task's goal is not met.

**D2. Pin "never the project's TypeScript" explicitly.**
Today: vtsls uses its bundled TypeScript because its own default
`vtsls.autoUseWorkspaceTsdk` is false; nothing in our manifest says so.
Observed: with that setting true, the project's `node_modules/typescript`
tsserver **and** its tsconfig `plugins` package both execute (t_autows).
Change: write `vtsls.autoUseWorkspaceTsdk = false` into the manifest's
settings. Example: a future vtsls that flips the default (or reads it from a
new place) would start running every project's TypeScript; with the pin it
does not.
- (a) **Write it explicitly and assert the exact settings object in the
  manifest unit test (Recommended).** Benefit: the invariant is visible and
  version-proof. Risk: none practical; it cannot be "reverted" into a
  failing control (removing it equals the default), so its control is a
  flip to `true` (see section 7).
- (b) Rely on the default, pin only by the end-to-end test. Benefit: one
  setting fewer. Risk: a vtsls upgrade that changes the default is caught
  only if the test runs on that version.

**D3. Windows: `cmd.exe` finds `node.exe` in the project root.**
Today: on Windows the candidate is `vtsls.cmd`; npm's `.cmd` shim runs
`node` through `cmd.exe`, which searches the current directory before
`PATH`, and both the probe (inherited cwd) and the server (`cwd = root`) run
in the project. A repository with `node.exe` at its root would get that
binary run [read from npm's shim and Windows documentation; not observed -
no Windows host]. The same holds for python's `pyright-langserver.cmd`.
Change: set `NoDefaultCurrentDirectoryInExePath=1` in the environment of the
probe and of the server (it makes `cmd.exe` skip the cwd). Example: a hostile
repo's `node.exe` stops running on Windows; nothing changes on macOS/Linux.
- (a) **Set it in the SDK for every LSP server and probe (`LspClient::start`,
  `probe`), unit-pinned; Windows end-to-end test as a follow-up task
  (Recommended).** Benefit: closes the hole for TS and python at once, two
  lines. Risk: unverified until the follow-up's Windows test runs; touches
  the SDK shared by all plugins (harmless env var elsewhere).
- (b) Only the TS plugin (`config.env` + its probe). Benefit: smallest
  blast radius. Risk: python keeps the hole; the probe needs an env
  parameter anyway, so the SDK changes regardless.
- (c) Out of scope: open a follow-up task. Benefit: this task stays
  cross-platform-verifiable. Risk: Windows keeps the hole meanwhile.

**D4. ATA (automatic type acquisition).**
Today: on by vtsls/VS Code default. Opening a JavaScript file makes
tsserver's typings installer run `npm install types-registry@latest` and
`npm install @types/<each dependency>` against the registry (observed).
Change: `typescript.disableAutomaticTypeAcquisition = true`. Example: a JS
project using `lodash` without `@types/lodash` installed: today ATA
downloads the types into `~/Library/Caches/typescript`; after, nothing is
downloaded and calls into lodash stay unresolved (lodash is outside the
index either way).
- (a) **Disable ATA (Recommended).** Benefit: ADR 0033's no-network rule
  holds for TypeScript; no npm runs at all. Risk: JS projects lose
  ATA-only types (edge-level, third-party targets).
- (b) Keep ATA, route it offline (`typescript.npm` → a refusing shim or
  offline env). Benefit: a warm typings cache still helps. Risk: npm still
  runs; extra plumbing; little gain.

## 7. Tests (for S3) and candidate controls

New `plugins/typescript/tests/semantic_runs_no_project_code.rs`, modelled on
`plugins/python/tests/semantic_runs_no_project_code.rs` (one `Mutex` for
`PATH`/cwd, logging shell shims, `#![cfg(unix)]`), driving
`g_mesh_plugin_typescript::semantic::engine` with the shipped manifest
(`MANIFEST_PATH_ENV`) and bare `vtsls`; the crate's own
`plugins/typescript/node_modules/.bin` (test dependency) is the `PATH` vtsls
where one is wanted. Fixture = `fx` above. Real-server tests: 5x repeats in
S3 and in verify.

| # | Behaviour | Test shape | Sanity | Candidate control (revert in a throwaway worktree; the test must fail) |
|---|---|---|---|---|
| B1 | A project `node_modules/.bin/vtsls` is never probed or run | `fx` + logging `node_modules/.bin/vtsls`; `PATH` without vtsls, with a logging `npx` | the tier fails with the remedy (no server resolves) | C1: `candidates` back to `npm_candidates(command, root, …)` → project shim log non-empty |
| B2 | `npx` is never run, so the project's `node_modules/@vtsls/language-server` never runs | as python's `a_real_npx_in_the_project_never_runs_the_projects_pyright`: real `npx`+`node` on `PATH`, no vtsls, a marker package in the project | tier off with remedy | C2: restore the `npx` origin (e.g. `npm_candidates_in(command, None, true, …)`) → marker / npx log |
| B3 | tsconfig `plugins` and the project's `typescript` never load with the shipped settings | `fx` (plugin package + marked `node_modules/typescript`), vtsls on `PATH` | `double` in `src/a.ts` resolves to `src/lib/b.ts` through `paths` (the hostile tsconfig is in force) | C3 (a flip, not a revert - D2): `vtsls.autoUseWorkspaceTsdk = true` in the manifest → `MARK_plugin` + `MARK_ts_*` (observed t_autows) |
| B4 | ATA runs no npm | `fx` with `loose.js` + `package.json` dep; `npm_config_registry` (and the proxy variables) pointing at GM-512's counting sink. A `PATH` npm shim does **not** work: with `typescript.npm` unset the typings installer runs the `npm` beside the `node` executable, falling back to `PATH` only when none is there (`getDefaultNPMLocation`, `typescript/lib/_typingsInstaller.js:68-79` [read]) | as B3 | C4: drop `disableAutomaticTypeAcquisition` from the manifest → the sink sees a connection (`npm install @types/lodash`; ATA observed in t_today) |
| B5 | No network and no write under the root for a whole pass | `plugins/typescript/tests/indexing_writes_nothing_and_stays_offline.rs`, a copy of python's (snapshot + sink) | as B3 | covered by C4 (sink connection count) |
| B6 | Manifest settings are exactly the pinned object | extend `the_shipped_manifest_configures_vtsls` (`semantic.rs:211-238`) | - | C4 also fails it |
| B7 | Candidates are `PATH` only (unit) | rewrite `a_bare_vtsls_is_path_then_the_projects_node_modules_then_npx` (`semantic.rs:162-193`) to "PATH only, every spelling" | - | C1/C2 fail it |
| B8 | (D3 a) probe and server env carry `NoDefaultCurrentDirectoryInExePath=1` | SDK unit tests: a probe/server shim that prints its env | - | C5: drop the env line in `probe` / `LspClient::start` |

Control budget: C1, C2, C3, C4 (+ C5 if D3 a) - five, within 6-8; C1/C2
share the `--test semantic_runs_no_project_code` binary with B1/B2; C4 needs
both test binaries plus `--lib`.

CI: `plugins/typescript` tests already run on all runners with vtsls
installed (`.github/workflows/ci.yml:594-603`, section `plugin-typescript`
`:786-790`); the new files are unix-only like python's.

## 8. Edit map (S2)

Change:

- `plugins/typescript/src/semantic.rs`
  - module doc `:18-25` ("Resolution: `PATH`, then …") → `PATH` only, link
    ADR 0032; add a line that the server runs vtsls's bundled TypeScript,
    never the project's, with no ATA.
  - `:77-79` `NPM_PACKAGE` doc (mentions `npx --package`); keep the const
    (`NpmServer.package` is a required field) or drop if S2 removes the
    field (see SDK below).
  - `prepare` `:105-118` (stops passing `root`), `resolve` `:124-133`
    (drop `root`; remedy text as python's, `plugins/python/src/semantic.rs:317-326`),
    `candidates` `:136-138` → `npm_candidates_on_path(command, &VTSLS, …)`.
  - tests: `a_bare_vtsls_is_path_then_the_projects_node_modules_then_npx`
    `:162-193`, `nothing_usable_names_vtsls_and_the_remedy` `:198-206`,
    `the_shipped_manifest_configures_vtsls` `:211-238` (exact settings).
- `plugins/typescript/plugin.toml:65-92` - the `[plugin.semantic]` comment
  on resolution; settings `typescript.disableAutomaticTypeAcquisition = true`
  (D4) and `vtsls.autoUseWorkspaceTsdk = false` (D2) under
  `[plugin.semantic.settings.""]`, with the invariant stated once and the
  ADR linked.
- `plugins/sdk/src/lsp/resolve.rs`
  - `npm_candidates` `:117-129` has no caller left: delete it and the
    `root`/`npx` parameters of `npm_candidates_in` `:143-190` (the `npx`
    branch becomes dead), or keep `npm_candidates_in` private and simplified.
    If `npx` goes, `NpmServer.package`/`probe_bin` are read by nothing:
    S2 decides whether to drop them (touches python's `PYRIGHT` const) or
    leave them documented as unused - prefer dropping, `find_references`
    on each field first.
  - `PROBE_BUDGET` doc `:40-44` (mentions an `npx` probe).
  - tests `a_bare_name_is_path_then_node_modules_then_npx` `:421-454`,
    `installed_candidates_are_probed_through_the_twin` `:456-470`,
    `the_npx_candidate_names_the_package_and_probes_the_probe_bin` `:472-490`
    → rewrite/delete with the code they cover.
  - (D3 a) `probe` `:195-229`: `.env("NoDefaultCurrentDirectoryInExePath", "1")`.
- `plugins/sdk/src/lsp/mod.rs:91` - re-export list.
- (D3 a) `plugins/sdk/src/lsp/client.rs:222-235` `LspClient::start`: same env.
- Docs: `README.md:371-377` (vtsls "found on `PATH`" only);
  `docs/architecture/gm-325-typescript-lsp-semantics.md:20, 220-240` (the
  resolution order; mark superseded by ADR 0032 rather than rewriting
  history); ADR 0032 - add a TypeScript decision (D7: `PATH` only, bundled
  TS pinned, D3's env) and replace the "TypeScript keeps … until its own
  task" sentence; ADR 0033 - ATA off and the `npx` follow-up closed.
- New tests per section 7.

Read for context: `plugins/python/src/semantic.rs:317-333` (`resolve`,
`candidates`), `plugins/python/tests/semantic_runs_no_project_code.rs`
(helpers `shim`, `set_environment`, the npx test `:283-330`),
`plugins/python/tests/indexing_writes_nothing_and_stays_offline.rs`
(`snapshot`, sink), `plugins/typescript/tests/conformance.rs:122-176`
(`semantic`, `vtsls()`), `plugins/sdk/src/lsp/resolve.rs:246-262`
(`resolve`), `plugins/sdk/src/lsp/client.rs:222-240`.

Callers/references relied on (g-mesh, project `g-mesh-wt-gm566`):
`find_references npm_candidates` → `plugins/typescript/src/semantic.rs::candidates`
+ `plugins/sdk/src/lsp/mod.rs`; `find_references npm_candidates_on_path` →
`plugins/python/src/semantic.rs::candidates` + `mod.rs`;
`get_file_outline plugins/typescript/src/semantic.rs`. Single known files
(`resolve.rs`, `client.rs`, vtsls's `index.js`) were read with `grep`/`sed`.
