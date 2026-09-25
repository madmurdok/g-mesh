# 0006. Language server scan scope: pyright gets the walker's include and exclude

## Status
Proposed (2026-09-25, GM-416). Measured in GM-416/S1 and GM-416/S5;
implementation is GM-416/S6 in `plugins/sdk/src/walk.rs` (the scope) and
`plugins/python/src/semantic.rs` (the settings).

## Context
GM-415/S5 recorded the Python semantic pass failing on a cold index of
g-mesh itself: "the language server did not answer a question about
plugins/python/conformance/project/pkg/mod.py within 10s" (`Budgets::request`,
counted from when each question is sent).

The cause, measured with pyright 1.1.414 via `npx --yes --package pyright`
on an 8-core macOS machine:

- **pyright blocks once at start-up, walking the whole root.** After
  `initialize` and `workspace/configuration` it sends nothing until its
  source enumeration finishes, then releases every queued answer in the same
  instant. `sample` shows the main thread in synchronous `readdirSync`. Later
  answers take 0.07-0.23s. There is no `$/progress` and no
  `workDoneProgress/create`, so there is no readiness signal to wait for.
- **What it walks is `target/`.** pyright's default excludes are only
  `**/node_modules`, `**/__pycache__`, `**/.*` and `**/__editable__.*`. We send
  it `rootUri`/`workspaceFolders` and nothing else about scope, so it walks
  the gitignored 377k-file Cargo cache that holds zero `.py` files. S1: first
  answer 0.85-1.89s without `target/` (7 runs), 1.22-8.60s with it (27 runs,
  10 over 5s). The walker we already own (`walk_project`: `.gitignore`,
  `.git`/`.claude`, the manifest's `exclude_dirs`) never enters `target/`.

S5 measured the fix directly, with `target/` in place and a probe module
`target/gm416probe/gm416_probe_mod.py` as the control. First answer to the
same `definition` question, interleaved runs, load average 2.9-4.8 (a
separate reading from S1's load, which went up to 318):

| `python.analysis` sent | Runs | First answer (s) | "Found N source files" | Probe in `workspace/symbol` | `time -p` real / user (s) |
|---|---|---|---|---|---|
| nothing (today) | 6 | 4.83-5.22 (median 5.08) | 15, at 8.4-9.0s | yes, 6 of 6 | 8.6-9.2 / 9.0-9.9 |
| `exclude` only | 6 | 0.70-0.82 (median 0.77) | 14, at 1.6-1.7s | no, 0 of 6 | 1.8-1.9 / 1.9-2.2 |
| `include` + `exclude` | 6 | 0.70-0.80 (median 0.72) | 14, at 1.5-1.7s | no, 0 of 6 | 1.7-1.9 / 1.8-2.1 |
| `include` only | 3 | 0.72-0.85 | 14, at 1.7-1.8s | no, 0 of 3 | 1.9-2.0 / 2.0-2.1 |

`user` close to `real` in every row: the process is computing, not waiting.
The 14 files are exactly what `walk_project` finds.

Other facts S5 established:

- **Only `workspace/configuration` delivers them.** pyright reads
  `python.analysis.include`, `.exclude` and `.ignore` from the `python`
  section it pulls after `initialized` (the same path that already carries
  `typeCheckingMode` and `pythonPath`). Relative entries resolve against the
  workspace root; an entry may be a file. The same `exclude` sent in
  `initializationOptions`, or pushed as `workspace/didChangeConfiguration`
  settings, changed nothing (4.78s and 5.03s, probe found). A
  `didChangeConfiguration` does make pyright pull `workspace/configuration`
  again, which is how a later change would reach it.
- **pyright's own defaults stay on.** With our `exclude` set, its log still
  shows "Auto-excluding **/node_modules" and the rest (`useDefaultExcludes`
  defaults to true).
- **Imports are not limited by `include`.** Fixture: `app/main.py`
  imports `lib.helper` (not in `include`) and `genpkg.gen` (in `exclude`,
  and gitignored). With `include = ["app"]`, `exclude = ["genpkg"]`, both
  `definition` answers land in `lib/helper.py` and `genpkg/gen.py`, the same
  as without settings. A file *inside* an excluded directory, once opened
  with `didOpen`, is answered normally too (`genpkg/user.py` resolved
  `helper_fn` to `lib/helper.py`). Scope decides what pyright *tracks*,
  not what it can resolve or answer.
- **A long exclude list has a cost.** Every directory pyright visits is
  matched against every entry: 2,000 entries added about 0.3s, 10,000
  added about 1.2s, on this small tree.

## Decision
We will hand pyright the walker's view of the project as
`python.analysis.include` and `python.analysis.exclude`, answered through
`workspace/configuration`. Nothing is written into the user's project.

1. **Computed in the SDK, from the same walk.** A new
   `walk_scope(root, extensions, exclude_dirs) -> WalkScope` in
   `plugins/sdk/src/walk.rs` shares `walk_project`'s builder (same
   `.gitignore` layering, same `BASELINE_EXCLUDED_DIRS`, same manifest
   `exclude_dirs`), so the scope cannot drift from what we index. It
   returns:
   - `include`: the top-most directories that contain a claimed file (a
     directory is dropped when an ancestor is already listed, since pyright
     recurses), plus each claimed file directly in the root as a file entry.
     Paths are relative, `/`-separated.
   - `pruned`: every directory the walk declined to enter because of
     `.gitignore`, relative to the root, top-most only (nothing under an
     already pruned directory is listed). The walk yields each directory it
     enters; a child of an entered directory that is a real directory (not a
     symlink), was not itself entered, and is not one of the named excludes
     is pruned.
   - `exclude_dirs`: the named excludes (baseline plus manifest), which
     are sent as `**/<name>` patterns, so they cost one entry each however
     often they occur.
2. **Mapped to pyright's keys in the Python plugin.** In
   `plugins/python/src/semantic.rs`, at the same place that adds
   `pythonPath`, and by the same merge into the `python` section:
   `analysis.exclude = pruned ++ ["**/<name>" for exclude_dirs]` and
   `analysis.include = include`. When the walk finds no Python file,
   `include` is omitted (an empty `include` makes pyright assume the root
   anyway). The manifest's own `[plugin.semantic.settings.python.analysis]`
   keys are kept; if the manifest ever sets `include`/`exclude` itself, the
   manifest wins and the plugin logs that it did not overwrite them.
3. **Bounded.** `exclude` carries at most 1,000 entries, the shallowest
   first (the big ones, such as `target/` and `dist/`, are near the root).
   The remainder is dropped with one log line naming the count. The worst
   case is today's behaviour for the dropped directories.
4. **Computed once per server.** The scope is taken when the engine is
   built, and the server keeps it for its lifetime. A directory that appears
   later only costs speed: pyright may scan it, and a new Python directory
   outside `include` is not tracked. Neither affects answers, because the
   bridge opens every file it asks about and imports resolve outside the
   scope. Refreshing it through `didChangeConfiguration` is possible (pyright
   re-pulls) but is not part of this decision.
5. **`include` is sent, although it measured no gain here.** On g-mesh,
   `exclude` alone does all the work (medians 0.77s against 0.72s, within
   noise), because what pyright walks outside the Python directories is
   small once `target/` is gone. `include` pays off in a project with large
   non-ignored, non-Python trees (vendored assets, data), costs nothing to
   compute because the walk already happened, and the import check above
   shows it does not narrow resolution.

**No first-answer budget.** The residual warm-up is 0.70-0.85s, more than
10 times inside `Budgets::request` (10s), and S1's no-`target/` runs never
exceeded 1.89s. So `Budgets::first_answer` from the earlier
draft of this ADR is dropped; the per-request 10s stays the only budget.
Reopen that question only if a measured first answer on a scoped server
comes within half of `request`.

**A server that ignores the settings** (an older pyright, a fork, a manifest
pointing at basedpyright with different keys) behaves exactly as today: it
walks the root, the first answer may be late, and a question that misses 10s
is recorded as a failed pass by the existing path. The settings cannot make
an answer wrong, only a scan shorter.

## Rejected options
- **Writing `pyrightconfig.json` (or a `[tool.pyright]` table) into the
  project.** It is the channel pyright documents for `exclude`, and it would
  work, but it modifies the user's tree, collides with a config they
  already have (the file's values take precedence over client settings), and
  is left behind if we die. Settings through `workspace/configuration` do the
  same thing with no file.
- **`initializationOptions` or `didChangeConfiguration` payloads.** Measured
  above: pyright ignores both for scope.
- **A one-time first-answer budget (this ADR's first draft).** It would
  have made the 5-9s warm-up survivable instead of removing it, and every
  pass would still pay it. With the scope, the warm-up is under 1s.
- **Waiting for an analysis-complete signal.** pyright sends none, and its
  "Found N source files" log text arrives after the first answers.
- **A longer `request` budget, or retrying the pass.** Both pay for the
  scan instead of removing it; a longer `request` also makes every hung
  question cost more. (Details in S1's result on GM-416.)
- **Advertising `didChangeWatchedFiles.dynamicRegistration`.** S1 measured
  no change (8.60s and 7.93s with it).
- **Exclude from the manifest only** (`target` in `exclude_dirs`). It fixes
  g-mesh, and nothing else: a Python project's own build or data directory
  is gitignored, not named in our manifest.

## Other servers
- **rust-analyzer** scopes itself from `cargo metadata` and never walks
  `target/` as source; its start-up is covered by the default readiness
  (`$/progress` then a quiet `settle`). Nothing needed.
- **gopls** loads packages from `go.mod`; it has `build.directoryFilters`
  (default `-**/node_modules`) that the same `WalkScope` could feed later.
  Not measured, and no failure recorded, so nothing now.
- **TypeScript** hands its compiler the file list its own semantic pass
  walks; there is no server-side scan to scope.

## Consequences
- The cold-start failure is removed at its cause: on g-mesh the first
  answer falls from about 5s at load 3-4 (up to 8.6s under load in S1) to
  about 0.75s.
- `WalkScope` is generic; another plugin maps it to its own server's keys.
- Task acceptance criterion 3 ("a fake LSP that answers late on the first
  request") belonged to the dropped budget. It needs to be restated as the
  scope tests below; that is the owner's call before S6.
- Tests S6 must add, each with its control (the code is reverted, never the
  test):
  1. **`walk_scope` matches the walk** (`plugins/sdk/src/walk.rs` unit
     tests, with the existing `Tree` helper): a tree with a gitignored
     `build/` holding claimed files, a nested gitignored `src/gen/`, a
     manifest-excluded `vendor/` at depth 2, a root-level claimed file and
     two claimed directories under one parent. Assert `pruned == ["build",
     "src/gen"]` (no `vendor`, nothing under `build`), `include` is the
     root file plus the top-most directories, and the union of files under
     `include` equals `walk_project`'s output. Control: have `walk_scope`
     skip the `.gitignore` layering; `pruned` comes back empty and the
     assertion fails.
  2. **The cap** keeps the shallowest 1,000 entries: a tree with 1,005
     gitignored leaf directories at depth 2 and one at depth 1; the depth-1
     entry survives. Control: remove the depth sort; it is dropped.
  3. **The Python settings merge** (`plugins/python/src/semantic.rs` unit
     test, next to the `set_python_path` test): after the scope is added,
     the `python` section still holds the manifest's `typeCheckingMode` and
     `pythonPath`, `analysis.exclude` has the pruned paths and
     `**/<name>` for each manifest excluded directory, and a manifest-set
     `exclude` is not overwritten. Control: insert instead of merge; the
     manifest key is lost.
  4. **The engine is built with the scope** (`plugins/python/src/semantic.rs`
     test on the function that builds the engine's `SemanticConfig` for a
     root): a temporary project with a gitignored `junk/` and a claimed
     `app/`; the `python` section carries `analysis.exclude` containing
     `junk` and `analysis.include == ["app"]`. The client already answers
     `workspace/configuration` with this section verbatim (existing
     `client.rs` tests). Control: skip the scope step; neither key is
     present.
  5. **Real pyright ignores a pruned tree** (an ignored-by-default
     integration test, run where pyright is available): a temporary project
     with a gitignored `junk/` holding a `.py` module and a claimed
     `app/main.py`. `workspace/symbol` for the junk module's function
     returns nothing, and `definition` from `app/main.py` into an import of
     a module outside `include` still lands there. Control: send no scope;
     the junk symbol is found.
- S8 measures 3 cold daemon starts of g-mesh with `target/` present and
  records the first-answer latency against S1's 6.57-7.33s.
