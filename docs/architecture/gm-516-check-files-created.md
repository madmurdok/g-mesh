# GM-516: `g-mesh plugins check` exercises `filesCreated`

Status: design (GM-516/S1), for owner review. No production code yet.

## Problem

ADR 0026 added the `filesCreated { filePaths }` notification, sent only to a
plugin whose manifest declares `[plugin.capabilities] files_created = true`.
Its Consequences say "`g-mesh plugins check` does not exercise the method
yet". Two bundled plugins declare it today (Python, TypeScript), and a
third-party plugin can declare it with no check that it actually closes the
window the capability exists for: an importer and its target created in the
same batch, the importer routed first.

Wanted (task ACs): the kit sends `filesCreated` to a declaring plugin and
asserts the same-batch new-importer case resolves; a non-declaring plugin is
not sent it; tests with controls.

## Facts the design rests on

g-mesh calls (`select_project g-mesh`; the branch had no changes yet):

- `find_callers notify_files_created` -> one caller,
  `daemon::lifecycle::PluginSupervisor::files_created`
  (core/src/daemon/lifecycle.rs:414). The function
  (`find_definition`, core/src/daemon/plugin.rs:1272-1285) builds
  `ControlEnvelope { id: None, message: ControlMessage::FilesCreated { file_paths } }`
  and `write_message`s it, gated on `manifest.capabilities.files_created`.
  The kit sends the same envelope; it cannot call this function (no
  `PluginProcess` in the kit).
- `find_references Capabilities.files_created` -> production reader is only
  `PluginProcess::notify_files_created`; the other three are test literals
  (`mcp::semantic_pending_tests`, `mcp::response_bound_tests`,
  `mcp::untyped_tests`). The kit reads no capability but `semantic_pass` /
  `semantic_sweep` today (grep of plugin_check/, one module).
- Where the kit's `Driver` issues `fileChanged`: `get_file_outline
  core/src/cli/plugin_check/session.rs` -> `Driver::step` (session.rs:1108-1147)
  is the only sender; it calls `watcher::apply::apply_file_change` (the
  daemon's own apply path, so linking runs per diff). `Driver::record`
  (1149-1213) reads `TeeWriter.sent` but skips every envelope without an `id`
  (line ~1174), so a notification is invisible to `Session` today.

Read directly (one known file each):

- Manifests declaring it: `plugins/python/plugin.toml:89` and
  `plugins/typescript/plugin.toml:42` (the Rust TS port). Go, Rust and the
  SDK's toy do not.
- How a missing target looks: both plugins emit an `external_module`
  placeholder (never linked by core) when the target is not in their file
  set, and a `resolved_module` placeholder with `target {file: <path>}` when
  it is (plugins/typescript/src/extractor/imports.rs:200,
  plugins/python/src/extractor/decls.rs:565). `graph::imports::link_diff`
  (core/src/graph/imports.rs:203) repoints a `resolved_module` edge onto the
  target's `File` node when the target's own `File` node arrives in a later
  diff, so importer-first routing still links once the target's
  `fileChanged` lands.
- The SDK builds the project model eagerly at session start
  (plugins/sdk/src/run.rs:260 `session.load_project()`), and
  `Session::files_created` applies nothing when there is no model
  (run.rs:651-654). So files written before the model is built make the
  window disappear (the check would pass with the notification ignored).
- The toy plugin the kit's own tests use (`plugins/sdk/fake/toy.rs`) is
  hand-written JSON-RPC, not SDK-based; `run` (toy.rs:264-306) drops every
  message without an `id` and has no import statement or presence set.
- `count_expectations` in each plugin's conformance test counts only array
  entries of expect.toml (`plugins/typescript/tests/conformance.rs:164-178`),
  so a new plain table does not change `EXPECTATIONS`.

## Decisions

### D1. Where the new file and importer text come from

**Recommended: a `[files_created]` table in the `--expect` file.**

```toml
[files_created]
target = "src/gmCheckTarget.ts"
target_text = "export function created(): number { return 1; }\n"
importer = "src/gmCheckImporter.ts"
importer_text = "import { created } from \"./gmCheckTarget\";\nexport const n = created();\n"
```

All four fields required (`deny_unknown_fields`, as every other table).
Both paths are workspace-relative, must not exist in the fixture, must not be
equal, and must carry one of the manifest's extensions; a violation is a
`Fail` of the check (D4), not a parse error, so the rest of the file still
runs.

- Benefit: the expect file is already the kit's per-language, author-written
  input, and every bundled conformance run (CI's per-plugin step and
  `plugins/*/tests/conformance.rs`) already passes it. No new CLI flag. The
  text is two tiny files, readable next to the expectations that describe the
  same fixture.
- Risk: the check runs only with `--expect`; a declaring plugin checked
  without it reports `Skip` (D3), so "declared but never exercised" is
  visible but not a failure. Source text inside TOML strings is less pleasant
  to edit than real files (multi-line `"""` strings help).

Alternatives:

- **A fixture-side directory** (`--files-created <dir>`, or a convention like
  `conformance/files_created/` named from expect.toml) holding real source
  files to copy in mid-session. Benefit: real files, linted by the language's
  own tools. Risk: a new flag or convention, must live outside the fixture
  tree (the bulk walk would otherwise see them, which removes the window),
  and still needs a way to say which file is the importer.
- **Derived from the fixture**: pick an `IMPORTS` edge bulk run 1 linked to a
  `File` node, copy importer and target to fresh paths. Rejected: rewriting
  the specifier is a language rule (core is language-agnostic, see
  `graph::imports`' module doc). Copying both into a new subdirectory keeps a
  relative TS specifier working, but a Python absolute import
  (`from pkg import x`) in the copy still lands on the *original* target,
  so the check passes without the notification (no control) or fails a
  conformant plugin, depending on what it asserts.

### D2. What "resolves" means

**Recommended: index-level, after both `fileChanged`s are applied and
linked.** In the files-created index (D5), at least one `IMPORTS` edge whose
`fromId` is a node with `filePath = importer` has a `toId` node with
`filePath = target`. One SQL read, in the style of `session::file_node_ids`:

```sql
SELECT t.filePath, t.kind, t.nativeKind, e.resolved
FROM edges e JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId
WHERE e.kind = 'IMPORTS' AND f.filePath = ?1
```

Pass iff some row's `t.filePath` is the target, **or** the row's `t` is a
container node (`nativeKind = 'container'`, no `filePath` of its own) whose
members include a node with `filePath = target`. The Python plugin links a module
import to a core container (e.g. `pkg.gm_check_target`), not to a file node,
so file-only matching could never pass it; the owner approved the widening.
On failure the finding lists
every row (e.g. "lands on `Module` `external_module` `./gmCheckTarget`"),
which is exactly what a plugin that ignored the notification produces.

- Benefit: language-agnostic, judges what a user's index ends up holding
  (the plugin's address *and* core's link), and has a sharp control: an
  ignored notification yields an `external_module` row.
- Risk: the container branch is looser than the file branch: an edge onto a
  container passes if the target file is one of its members, whichever
  member the plugin meant. Accepted: the importer is new, so no other edge
  of it can land there by accident (must-confirm M4).

Alternatives:

- **Wire-level**: the importer's `fileChanged` diff holds a `resolved_module`
  placeholder whose `target` is `{file, target}`. Benefit: no index read.
  Risk: judges the plugin's address only, hard-codes the TS/Python
  placeholder convention into the kit, and misses a core-side link failure.
- **Through `get_dependencies`** (the `[[imports]]` expectation's handler)
  on the importer. Benefit: MCP semantics, covers container targets. Risk:
  needs a second `EvalContext` over the files-created index and the
  handler's fallbacks (`entry_points`) can answer from outside the edge.

### D3. A plugin that does not declare it

Not sent anything: the files-created session (D5) is not run at all, so no
plugin process sees a `filesCreated` frame, and no extra spawn happens.

**Recommended: the check is shown, as `Skip("not applicable: the manifest
declares files_created = false")`.** Same convention as
`capabilities.semantic-engine-lazy`, and it keeps every report the same
shape, so the `ALL_CHECKS` lists in the tests stay one constant per file.

Alternatives:

- **Not shown.** Benefit: a shorter report for Go/Rust. Risk: the check list
  then depends on the manifest, and every `ALL_CHECKS`-style assertion
  (five test files) must branch on it.
- **A paired `capabilities.files-created-undeclared` check** (fail if a
  `filesCreated` frame was written to a non-declarer). Benefit: mirrors
  `semantic-pass-undeclared`. Risk: unlike that check there is no
  plugin-side evidence to judge (no marker); it could only ever catch a bug
  in the kit itself, which a test catches more cheaply (behaviour 3). Two more ids in
  five test lists.

### D4. Check id, outcomes, position

**Id `capabilities.files-created-resolves`**, in the `checks` section,
appended **after** `capabilities.semantic-engine-lazy` (last), so every
existing id keeps its position. Outcomes, in evaluation order:

| Condition | Outcome |
|---|---|
| manifest `files_created = false` | `Skip` "not applicable: the manifest declares files_created = false" |
| declared, no `--expect` given, or the file has no `[files_created]` | `Skip` "not configured: the manifest declares files_created but the expectations file names no [files_created] pair - see ..." |
| declared, the expect file did not parse | `Skip` "not configured: the expectations file did not parse (see `expectations.file`)" |
| bulk run 1 incomplete or the main session failed | `Skip` via `not_reached(...)` |
| pair invalid (a path exists in the fixture, the paths are equal, an extension the manifest does not claim) | `Fail` naming the field |
| files-created session failed (spawn, handshake, timeout, apply error, process exit) | `Skip` "not reached" here; the failure itself goes to `failures`, so `session` fails (one defect, one failing check) |
| importer has an `IMPORTS` edge landing in the target file | `Pass` |
| otherwise | `Fail` listing the importer's `IMPORTS` rows (D2) |

Notes section gains one line per frame of the files-created session (as for
the main session's exchanges), plus `files_created: <importer> -> <target>`.

### D5. How and when the kit drives it

**Recommended: a separate, short session after bulk run 3, on its own fresh
index.** `session::run_files_created_session(manifest, scratch, pair,
warm_file, timeouts) -> FilesCreatedRun`:

1. Spawn and handshake (the code `run_session` has today, factored into a
   shared `Driver::spawn`).
2. **Warm-up round trip**: `fileChanged` on `warm_file` (the `EditTarget`'s
   file, already in the fixture), structural gate closed. Its answer proves
   the plugin processed a request after building its model; without it the
   kit could write the files while an SDK plugin is still in `load_project`
   (run.rs:260) and the model would see them - the window gone, a pass with
   the notification ignored.
3. Write `target` and `importer` (creating parent dirs) into the workspace.
4. Send `filesCreated { filePaths: [importer, target] }` with **no `id`**,
   exactly the envelope `notify_files_created` builds (new
   `Driver::notify`, recorded as a frame in the run).
5. `fileChanged importer`, then `fileChanged target`, through the same
   `Driver::step` (importer first: that order is the window; target first
   lets the target's own `fileChanged` apply presence and the check passes
   with the notification ignored).
6. Read D2's rows from the files-created index, then remove both files from
   the workspace and finish the process.

The index is a fresh `open_index(manifest)` (empty, in-memory). The warm-up
upserts one file; the pair's two diffs link against each other, which is all
D2 reads.

- Benefit: the main session, its checks and the `expectations` section never
  see the two extra files (no entry's result set can grow by a new importer),
  and a failure here never skips `expectations`. Not run at all for a
  non-declarer (D3).
- Risk: one more plugin spawn per declaring run (structural only; the
  semantic gate stays closed, so Python never starts pyright and TS never
  starts vtsls). The expect file is parsed before the session, earlier than
  `expectations_section` parses it today (parse once, pass the result to
  both).

Alternatives:

- **Append to `run_session` as steps 8-10**, then delete both files and send
  their `fileChanged`s before `expectations` run. Benefit: no extra spawn.
  Risk: the deletion diffs become part of every check's evidence, a failure
  there fails the whole main session and skips `expectations`, and the
  index is "restored" only as well as the plugin's deletion path works.
- **Run it after `expectations_section`, on the main index.** Benefit: no
  fresh index. Risk: still a second spawn (the main process is finished by
  then), and couples the check to the expectations' lifetime for nothing.

### D6. Docs

- ADR 0026, Consequences: replace "`g-mesh plugins check` does not exercise
  the method yet" with a pointer to this check.
- `docs/architecture/multi-language-plugins.md` "Conformance kit" (line ~706):
  one paragraph on the check and the `[files_created]` table.
- `README.md` ("Checking a plugin": the check table row and the
  `[files_created]` table in the `--expect` section) and
  `plugins/python/README.md` (the passing-check count).
- `checks.rs` module doc: the new bullet; `expectations.rs` module doc: the
  table (it is the expect-file format reference).

## Edit map

Change (line ranges on 39542c0):

- `core/src/cli/plugin_check/session.rs`
  - `Driver` 881-894 / `run_session` 914-1083: factor spawn + handshake
    (~925-980) into `Driver::spawn(manifest, scratch, conn, timeouts) ->
    Result<Driver, Session>`; `run_session` keeps its steps unchanged.
  - New `Driver::notify(label, ControlMessage)` next to `Driver::step`
    1108-1147: `write_message` an id-less envelope through `self.writer`.
  - `Driver::record` 1149-1213: today skips id-less envelopes; collect
    `ControlMessage::FilesCreated` ones into a new `Session.notifications:
    Vec<(String /*step*/, Vec<String> /*paths*/)>` (or the
    `FilesCreatedRun`'s own list).
  - New `FilesCreatedPair`, `FilesCreatedRun { failure, notifications,
    exchanges, import_rows }`, `run_files_created_session`, and an
    `import_rows(store, importer)` reader beside `file_node_ids` 545-550.
- `core/src/cli/plugin_check/expectations.rs`
  - `ExpectFile` 605-623: `#[serde(default)] files_created:
    Option<FilesCreatedPair>` (or the struct defined here and re-used by
    session). Accessor for `mod.rs`. Module doc: the table.
- `core/src/cli/plugin_check/mod.rs`
  - `check` 113-304: parse `--expect` once up front (keep the error for
    `expectations.file`); after the bulk-run-3 block (ends ~230), run
    `run_files_created_session` when declared + configured + main session
    ok; push its failure into `failures`; notes lines (~255-277); pass the
    run into `RunData` (279-288).
  - `expectations_section` 315-378: take the parsed file instead of
    re-parsing at ~345.
- `core/src/cli/plugin_check/checks.rs`
  - `RunData` 134-149: `files_created: FilesCreatedEvidence` (declared,
    configured/parse state, pair validity findings, `Option<&FilesCreatedRun>`).
  - New `files_created_resolves(run)`; call it at the end of `evaluate`
    191-245 (after `capabilities`). Module doc bullet (list ends ~91).
- `plugins/sdk/fake/toy.rs` (the kit's test fake)
  - Module doc 1-41: new statement `import FILE` and defect.
  - `run` 264-306: build a presence set from `walk_all(root)` **before** the
    handshake; handle `filesCreated` with or without `id` (insert listed paths
    unless defect `files-created-ignored`; ack only when an `id` is present);
    append each notification to a log file the tests read (e.g.
    `$G_MESH_PLUGIN_CHECK_MARKER_DIR/notifications`, the marker dir is
    already isolated per run).
  - `file_changed` 347-400: own path's presence = readable on disk, before
    extraction.
  - `extract` 137-222: `import FILE` -> `resolved_module` placeholder with
    `target {scopeKind: "file", scope: FILE}` when FILE is in the set, else
    `external_module`; `IMPORTS` edge from the `File` node, `resolved: false`.
    `bulk`/`extract` stay pure: the set is a parameter.
- Fixtures/tests data:
  - `plugins/typescript/conformance/expect.toml`,
    `plugins/python/conformance/expect.toml`: add `[files_created]`
    (TS: relative `./` import under `src/`; Python: `pkg/gm_check_target.py`
    + `pkg/gm_check_importer.py` with `from pkg.gm_check_target import ...`,
    `pkg/__init__.py` exists).
  - Add the id to `ALL_CHECKS` in `core/tests/plugin_check.rs:84-100`,
    `plugins/{typescript,python,rust}/tests/conformance.rs`,
    `plugins/sdk/tests/toy_conformance.rs` (~50-64), with the expected
    outcome per arm (PASS for TS/Python with `--expect`, SKIP elsewhere).
  - `core/tests/plugin_check.rs` `install_fake` 155-184: a `files_created`
    switch in the generated manifest.

Untouched: `core/src/cli/plugins.rs` (its `Capabilities` literal at ~613
already has `files_created`; no new manifest field), `daemon/*`, wire.

Read for context: `daemon::plugin::PluginProcess::notify_files_created`
(plugin.rs:1272-1285), `watcher::apply::apply_file_change`,
`graph::imports::link_diff` (imports.rs:203), SDK `Session::files_created`
(run.rs:645-700), `daemon/test_plugin.rs::declare_files_created` (445; the
daemon-level fake, not the kit's).

## Behaviour list (tests slice)

Each with its control (the code revert that must make it fail).

1. **A conformant declaring plugin passes.** Toy with `files_created = true`,
   fixture `a.fk`/`b.fk`, expect file with a `.fk` pair: the check is `PASS`
   and every other check keeps its baseline outcome. *Control:* drop the
   `Driver::notify` call (step 4).
2. **A declaring plugin that ignores the notification fails only this
   check.** Toy defect `files-created-ignored`: `FAIL`, the finding names an
   `external_module` landing; every other check as baseline. *Control:* send
   `fileChanged target` before `importer` (step 5) - the test must then fail
   because the check passes.
3. **A non-declaring plugin is sent nothing and reports `SKIP` (not
   applicable).** Toy with `files_created = false` plus a pair: the toy's
   notification log is empty and the process spawn count is the
   non-declaring baseline. *Control:* remove the `files_created` gate in
   `check`.
4. **The warm-up orders the model before the files.** A toy variant that
   builds its presence set lazily on the first request (or a unit test on the
   step sequence of `run_files_created_session` over recorded frames: the
   first frame after the handshake is a `fileChanged` of `warm_file`, and
   the `filesCreated` frame precedes both pair `fileChanged`s, importer
   first). *Control:* remove the warm-up step.
5. **The notification has no `id` and lists both paths.** From the recorded
   frames. *Control:* send it with an id.
6. **Declared but not configured is `SKIP` with the "not configured"
   reason**, both without `--expect` and with an expect file lacking the
   table; no `filesCreated` frame sent. *Control:* treat missing as `Pass`.
7. **An invalid pair fails the check, not the parse**: a path that exists in
   the fixture; an extension the manifest does not claim. Other
   expectations still run. *Control:* skip the validation.
8. **A failed files-created session fails `session`, skips this check**
   (toy `hang` restricted to the pair, or a pair the toy cannot read):
   *Control:* drop the push into `failures`.
9. **`expectations` are unaffected by the pair**: the pair's importer also
   imports a fixture file F; an `[[importers]]` expectation on F keeps its
   set (the extra importer never reaches the main index). *Control:* run
   the steps on the main index.
10. **Unknown keys in `[files_created]` are a parse error** (as every table).
    *Control:* drop `deny_unknown_fields`.
11. **Real plugins:** TS and Python conformance runs with `--expect` report
    `PASS`; Go, Rust and the SDK toy report `SKIP`. *Control (verify):*
    against a build where the TS (or Python) extractor's
    `file_presence_changed` is a no-op, the check `FAIL`s.

Unit tests (checks.rs `tests`) for the outcome table of D4 over hand-built
evidence; integration (core/tests/plugin_check.rs) for 1-3, 6-9.
None of these involve timers beyond the existing round-trip timeouts; 8 uses
the kit's timeout path, so run it 5 times.

## Must confirm

- **M1 (D1)**: the pair lives in the `--expect` file as `[files_created]`,
  rather than a new CLI flag/directory.
- **M2 (D3/D4)**: declared but not configured (no `--expect`, or no table) is
  `SKIP`, not `FAIL`. `FAIL` would force every third-party declaring plugin
  to ship a pair, and break any declaring plugin's existing CI that runs the
  kit without `--expect`.
- **M3 (D5)**: a separate short session (one extra spawn, structural only) on
  a fresh index, rather than extra steps in the main session.
- **M4 (D2)**: "resolves" = an `IMPORTS` edge from the importer lands on a
  node of the target file, or on a container whose members include the target
  file (the Python plugin links module imports to a core container such as
  `pkg.gm_check_target`; owner-approved).
- **M5**: one new check id, `capabilities.files-created-resolves`, appended
  last; no `files-created-undeclared` companion (AC 2 is pinned by behaviour 3 instead).
