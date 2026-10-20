# GM-205: verify the Windows MSVC archive and install.ps1 on a real runner

Slice S1 (design/investigation). Read-only: no workflow was edited, nothing
was pushed or dispatched.

## Finding: most of the task has already happened

The task assumes nobody has seen the Windows archive build and install on
real Windows. The run history says otherwise.

`gh run list --workflow release.yml` shows 23 successful runs, including tag
pushes for v4.0.0 to v4.12.0 and two `workflow_dispatch` rehearsals. In every
one of them the `x86_64-pc-windows-msvc` job on `windows-2022` ran all of
these and passed:

- `Build and package x86_64-pc-windows-msvc` (`bash scripts/build-targets.sh`),
  which produces `g-mesh-v<ver>-x86_64-pc-windows-msvc.zip` and four plugin
  `.tar.gz` assets.
- `Smoke test the staged artifact (GM-333)` (`scripts/release-smoke.sh`).
- `Install from the staged archive with install.ps1 (GM-333)`: the step whose
  comment says "Unverified: this has never run on a real Windows runner".

Evidence, all read from logs:

| Run | Event | What it shows |
|---|---|---|
| [39272924435](https://github.com/madmurdok/g-mesh/actions/runs/39272924435/job/118129800581) | tag v4.12.0 | Windows job 6 min. release-smoke: `13 nodes, 20 edges`, PASS for bundled and plugin-asset legs. install.ps1: all five `PASS:` lines (installed + `--version` + `plugins list`; Path added once; refusal without `-Force`; `-Force` replace; `-NoModifyPath`; `G_MESH_NO_MODIFY_PATH=1`). |
| [38962958123](https://github.com/madmurdok/g-mesh/actions/runs/38962958123) | workflow_dispatch, branch `rehearsal/gm613-untested` | Same two steps green; publish job skipped. |
| [35865211933](https://github.com/madmurdok/g-mesh/actions/runs/35865211933) | workflow_dispatch, branch `experiment/GM-347-control` | Windows job failed in the install.ps1 step on purpose (GM-347 control). So the step can fail; it is not a vacuous green. |

The Windows zips are also published: v4.11.0 (Latest), v4.10.0 and v4.9.0
releases carry `g-mesh-v<ver>-x86_64-pc-windows-msvc.zip` + `.sha256`.

**The "Unverified" comment (release.yml lines 486-490) is stale.** The step
has run and passed on real Windows since at least v4.0.0.

## What is still unproven (the real gap)

1. **The *installed* binary never indexes anything or answers MCP.** The
   install step checks only `g-mesh.exe --version` and `g-mesh.exe plugins
   list`. Indexing is checked by release-smoke.sh, but against the *staged*
   binary in `dist/`, not the installed one. No release step spawns
   `g-mesh.exe mcp-shim`. (ci.yml's `mcp_e2e` integration test does run the
   shim and daemon on Windows, but that is a debug build from the checkout,
   not the shipped archive.)
2. **The `plugins list` check in the install step cannot fail on a missing
   plugin.** `daemon::manifest::bundled_roots` (core/src/daemon/manifest.rs
   lines 82-89) adds a second root after `plugins\` next to the exe:
   `env!("CARGO_MANIFEST_DIR")/../plugins`, compiled in as
   `D:\a\g-mesh\g-mesh\core\../plugins`. On the runner, that path is the live
   checkout, and each `plugins/<lang>/plugin.toml` there is a manifest. The
   release-smoke log shows it: `plugin "go" at D:\a\g-mesh\g-mesh\core\../plugins\go
   shadows the same language already found at ...dist\...\plugins\go - the
   earlier one wins`. If install.ps1 dropped a plugin directory, `plugins
   list` would still name that language because it would find the checkout's
   copy. release-smoke.sh's own header says the same about `plugins list`.
3. **The per-language index check is skipped on Windows.** The log says
   `per-language check skipped (sqlite3 not on PATH ...)`. On Windows only
   the aggregate count (`13 nodes, 20 edges`) is checked, so a language that
   extracts nothing on Windows would go unnoticed.
4. Not exercised anywhere, and out of this task's acceptance criteria:
   - the `irm ... | iex; Install-GMesh` path (CI runs only `& install.ps1`);
   - Windows PowerShell 5.1 running the install (CI's install run uses pwsh 7;
     5.1 only parses and lints it in ci.yml);
   - a download from real GitHub Releases, including `releases/latest`
     resolution (CI always passes `-Version` and serves from a local
     `python -m http.server`).
   These are listed under decision D4.

The vtsls / pyright / rust-analyzer semantic tiers fail to start on the
runner (the log shows "no usable vtsls ... answering structurally only").
That is expected: they need external tools on every platform. The structural
tier is what this smoke asserts.

## Route

Add one block to the existing install step (installed binary: index, then
one MCP call), then run release.yml once by `workflow_dispatch` on the task
branch.

Why this route and not a dedicated workflow:

- `workflow_dispatch` only works for a workflow file that already exists on
  the default branch. release.yml exists on `main`; a new
  `windows-smoke.yml` could not be dispatched until it was merged.
- The step already has the installed tree, the local server and the Path
  bookkeeping. Adding to it costs no second install or server.
- Every later release then repeats the check, as the task's "every failure
  fixed" intent wants.

**Nothing gets published by a dispatch:**

- `publish` (release.yml line 727) runs only when `startsWith(github.ref,
  'refs/tags/v')`. A dispatch on a branch has `refs/heads/...`, so it is
  skipped. Run 38962958123 shows it: `Publish draft Release: skipped`.
- The `Check the tag matches` step is also tag-only. The `verify` job runs a
  dry run without `gh`, under `contents: read`.
- The only job with write access is `publish`. The install step edits the
  runner's user Path and restores it in `finally`, and the runner is
  ephemeral anyway.

**What runs, and how long it takes:**

- `ci-gate` looks for a green full ci.yml run on the same commit or tree.
  The branch changes the tree, so it answers `green=false`.
- `tests` (all of ci.yml through `workflow_call`) therefore runs: about 34
  min wall, set by x86_64 macOS (run 38962958123: 20:32 to 21:06).
- Then the four `build` legs run (5-7 min each; Windows 6-7 min), plus
  `verify` (about 1 min).
- Total: about 45 min wall and about 120 runner-minutes. The repo is public,
  so standard runners cost nothing in billed minutes.

**The push and the dispatch** (the owner approves them; not done here):

```sh
# After the implement slice commits on this branch:
git -C /Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh-wt-gm205 \
  push -u origin ci/GM-205-windows-archive-install
gh workflow run release.yml --repo madmurdok/g-mesh \
  --ref ci/GM-205-windows-archive-install
gh run list --repo madmurdok/g-mesh --workflow release.yml \
  --branch ci/GM-205-windows-archive-install --limit 1 \
  --json databaseId,url
```

Pushing `ci/GM-205-...` starts no workflow: ci.yml's `push` filter is
`main`, `release-*` and `experiment/*`, and release.yml's is `v*` tags. Only
the dispatch starts work. An `experiment/*` branch would also start ci.yml's
own push run, so the tests would run twice; see D1. Do not open a PR before
the dispatch run finishes. A PR touching release.yml starts release.yml and
ci.yml again.

## What the new smoke asserts

It runs inside the existing install step, against the binary install.ps1
just put in `$installDir`. Before anything is installed, the checkout's
`plugins\` is hidden so that the compiled-in fallback root (gap 2) finds
nothing:

1. **Hide the fallback.** `Rename-Item plugins plugins.gm205-hidden` at the
   start of `try`, restored in `finally`. Discovery is then exactly what a
   user's machine has: `~\.g-mesh\plugins` (absent on the runner) plus
   `plugins\` next to `g-mesh.exe`. The existing `plugins list` check thus
   starts to mean something (it fixes gap 2 for free). See D2 for the
   alternative.
2. **Index with the installed binary.** Copy `scripts/release-smoke-fixture`
   (without README.md) to `$env:RUNNER_TEMP\gm205-proj`. Use a short
   `G_MESH_HOME` of `$env:RUNNER_TEMP\gmh`. Run `& $exe reindex` with that
   directory as cwd. Assertions:
   - exit 0;
   - output has no `has no plugin installed` and no `shadows`;
   - the `index:` line reports more than 0 nodes and more than 0 edges.

   These are the same rules release-smoke.sh applies.
3. **Make one MCP tool call through the shim.** Run
   `python -I scripts/mcp-smoke.py <exe> <project>`, with the same
   `G_MESH_HOME` and `CLAUDE_PROJECT_DIR` removed from the environment. The
   new script uses only the Python standard library (python is already
   required by this step). It spawns `<exe> mcp-shim` with the project as cwd
   and talks newline-delimited JSON-RPC over stdio:
   - `initialize` (`protocolVersion` `2025-03-26`), then
     `notifications/initialized`;
   - `tools/list`, which must include `find_callers` and `find_definition`;
   - `tools/call find_callers {"symbol_name": "add"}`. It must return a
     result with no `isError`, and the text must name `double` in each of
     `hello.go`, `hello.py`, `lib.rs` and `hello.ts`. All four fixture files
     define `add`, and `double` calls it there. A per-language edge through
     MCP therefore also closes gap 3 on Windows.

     If the server answers an unfiltered ambiguous name with a choice list
     instead of callers, the script makes one call per file with
     `file_paths`. The implement slice decides which by running the script
     once locally against a macOS release build on the fixture before the
     push.
   - It closes stdin, waits for the shim to exit, and runs `<exe> stop` in
     the project. The daemon must be stopped: on Windows it holds files under
     `$installDir` and `G_MESH_HOME`, and `finally`'s `Remove-Item
     -ErrorAction SilentlyContinue` would hide a failed cleanup.
   - Each read has a deadline: 180 s total, because the first call
     bootstraps the daemon and waits for the walk. On timeout the script
     prints what it got, kills the shim and exits non-zero.
4. On success, print `PASS: the installed g-mesh.exe indexed the fixture and
   answered find_callers over MCP (go, python, rust, typescript)`.

## Risks seen by reading

| Risk | Where | Handling |
|---|---|---|
| Compiled-in checkout plugin root masks a missing installed plugin | manifest.rs 82-89; the log's "shadows" lines | Hide checkout `plugins\` during the step (D2) and fail on `shadows`. |
| AF_UNIX socket path length (about 108 chars) on Windows under a long `G_MESH_HOME` | shim/daemon socket under `G_MESH_HOME` | Short home: `$env:RUNNER_TEMP\gmh` (`D:\a\_temp\gmh`). |
| Detached daemon outlives the step and locks `$installDir` | daemon spawned by mcp-shim | `g-mesh.exe stop` in the script and again in `finally`. The runner reaps orphans at job end in any case. |
| Native exit codes don't throw in pwsh | `& $exe reindex` | Check `$LASTEXITCODE` explicitly after every native call (the step already does this for `--version`). |
| stdout encoding/CRLF on Windows pipes | mcp-smoke.py reading the shim | Read binary, decode UTF-8, `strip()` each line; write `\n`-terminated UTF-8 bytes. |
| `.exe` suffix | install step uses `g-mesh.exe` explicitly; release-smoke.sh maps `*-windows-*` | No change. The script takes the full exe path. |
| `python` vs `python3` | windows-2022 has `python` (step already requires it) | Use `python`. |
| Renamed checkout `plugins\` not restored | later step `upload-artifact` reads only `dist/` | Restore in `finally`. No later step reads `plugins/`. |
| `ort` / model | `search_code` needs the model; not fetched | Only structural tools are called. |
| Stale numbers in the step comment ("616 lines") | release.yml 469 | Rewrite with the comment (E1). |

## Edit map for the implement slice

Line numbers are on `release-4.15.0` as of this note.

| # | File | Lines | Change |
|---|---|---|---|
| E1 | `.github/workflows/release.yml` | 469-490 | Rewrite the comment per the project comment rule (present tense, no ticket ids, no change history). Delete the "616 lines ... never been *executed*" history and the whole "Unverified:" paragraph. Say what the step checks, and the invariant that the checkout `plugins\` is hidden so that discovery matches a user's machine. Keep the python-on-PATH sentence. The run links (39272924435, 38962958123, control 35865211933) and the first GM-205 dispatch run go in the commit message and the task, not in the comment. |
| E2 | `.github/workflows/release.yml` | 536 (start of `try {`) | Rename checkout `plugins` to `plugins.gm205-hidden` (`Join-Path $PWD`), and record that it was renamed. |
| E3 | `.github/workflows/release.yml` | after 579 (first `PASS:`), before the Path-count check at 581 | Insert the index + MCP block from "What the new smoke asserts", steps 2-4. |
| E4 | `.github/workflows/release.yml` | 639-660 (`finally`) | First: `& $exe stop` in the fixture dir (errors ignored). Then rename `plugins.gm205-hidden` back if it was renamed. Then remove `gm205-proj` and `gmh`. |
| E5 | `scripts/mcp-smoke.py` | new | The stdlib MCP client described above. Usage: `mcp-smoke.py <g-mesh-exe> <project-dir>`. Exit 0 on pass, 1 on failure with the received frames printed. |
| E6 | `.github/workflows/release.yml` | 189-207 (`pull_request.paths`) | Add `"scripts/mcp-smoke.py"`. |
| E7 (if D3 = yes) | `scripts/release-smoke.sh` | 175-189 | When `sqlite3` is absent, run the same per-language `COUNT(*)` through `python -c` with `sqlite3` (or `python3` on Unix). Windows then stops skipping the per-language check. Keep the skip message for when neither exists. |

Before the push, the implement slice must run E5 locally on macOS against a
`cargo build --release` binary and the fixture, to settle the
`find_callers` answer shape. It also runs `shellcheck` on E7 if E7 is made.
No test suite is needed: workflow and scripts only.

After the green run: link the run in GM-205. Remove or rewrite the
"Unverified" comment (E1 does it). Re-assess priority (D5).

## Decisions for the owner

### D1. Which branch the dispatch runs on

- **Today:** the task branch is `ci/GM-205-windows-archive-install`, and
  nothing is pushed. The brief suggested an `experiment/*` branch.
- **Change:** push one branch and run `gh workflow run release.yml --ref
  <branch>`.
- **Example:** with `experiment/GM-205`, the push itself starts ci.yml (its
  filter includes `experiment/*`), about 34 min on four platforms. The
  dispatch then sees no finished full ci.yml run (ci-gate is false) and
  runs the same tests again inside release.yml. With
  `ci/GM-205-windows-archive-install`, the push starts nothing, and the
  dispatch runs the tests once.
- **Consequence:** about 120 runner-minutes and one parallel queue saved.
  The minutes are free on a public repo, but the queue is shared with other
  work.
- **Options:**
  - (a) Push the task branch as is and dispatch on it. One test run. The
    branch name does not follow the `experiment/*` convention.
  - (b) Push `experiment/GM-205` and dispatch on it. Follows the convention,
    but the tests run twice.
  - (c) Push `experiment/GM-205`, wait for its ci.yml run, then dispatch.
    That run may be partial (a scripts-only change), and ci-gate wants
    "Full test suite ran", so this likely still runs the tests twice and
    takes longer.
- **Recommendation:** (a).

### D2. How the smoke keeps the checkout's plugins out of discovery

- **Today:** the shipped binary also looks in its compiled-in
  `CARGO_MANIFEST_DIR/../plugins`, which on the runner is the live checkout.
  The install step's `plugins list` check passes even if install.ps1 dropped
  a plugin.
- **Change:** make the installed binary see only what a user's machine has.
- **Example:** install.ps1 forgets `plugins\rust\`. Today `plugins list`
  still prints `rust` from `D:\a\g-mesh\g-mesh\plugins\rust`. After the
  change, it does not print `rust`, and the reindex reports "has no plugin
  installed".
- **Options:**
  - (a) Rename the checkout `plugins\` for the step and restore it in
    `finally`. This exercises the real lookup (`current_exe()` + `plugins\`).
    Risk: a failed restore would affect later steps, but there are none
    that read it.
  - (b) Set `G_MESH_PLUGIN_ROOTS_OVERRIDE=$installDir\plugins`. No
    filesystem change, but it bypasses the real `current_exe()` lookup, and
    `plugins list` ignores it, so the existing check stays non-discriminating.
  - (c) Leave it and rely on the reindex. The "shadows" message names the
    checkout copy, and the earlier root wins, so a missing installed plugin
    would silently index from the checkout's manifest.
- **Recommendation:** (a).

### D3. Fix the skipped per-language check on Windows (E7)?

- **Today:** release-smoke.sh checks that each of go/rs/py/ts contributed
  nodes only when the `sqlite3` CLI exists. windows-2022 lacks it, so
  Windows checks only the aggregate.
- **Change:** fall back to Python's built-in `sqlite3` module.
- **Example:** if the Rust plugin extracted nothing on Windows, the
  aggregate would still be above 0 and pass today. With E7 it fails with
  `.rs contributed 0 nodes`.
- **Consequence:** about 15 lines in release-smoke.sh, which already runs
  on all four legs. If D2(a) and the MCP per-file assertion land, the
  installed binary already gets a per-language check on Windows. E7 adds it
  for the staged binary and the plugin-asset leg.
- **Options:**
  - (a) Yes, in this task. Small, and the gap is real.
  - (b) File it as a separate task. Keeps GM-205 to its acceptance
    criteria.
- **Recommendation:** (a).

### D4. The Windows install paths still not exercised

- **Today:** CI never runs `irm ... | iex; Install-GMesh`, never runs the
  install under Windows PowerShell 5.1, and never downloads from real GitHub
  Releases or resolves `releases/latest`.
- **Change:** possible follow-up: a small job, no build, that installs the
  latest published release with the real one-liner under both `pwsh` and
  `powershell`, and then runs `--version`.
- **Example:** a TLS 1.2 default or `Invoke-RestMethod` difference in 5.1
  would break the README's one-liner for users on stock Windows. Nothing
  today would catch it.
- **Consequence:** about 2 runner-minutes per run. It could run on a
  schedule or after publishing. It can only be dispatched once its workflow
  file is on `main`.
- **Options:**
  - (a) File as a new task, outside GM-205. GM-205 keeps to its acceptance
    criteria.
  - (b) Add it to GM-205 now. That adds scope and needs a merge to `main`
    before it can be dispatched.
  - (c) Not needed.
- **Recommendation:** (a).

### D5. GM-205's priority

- **Today:** GM-205 is framed as "nobody has seen it work on Windows".
- **Change:** the build, the install.ps1 run, and the staged-binary index
  have been green on real Windows for 23 runs. What remains is the
  installed-binary index + MCP call (E2-E5) and a stale comment (E1).
- **Consequence:** this is now a small hardening task. It is about one
  implement slice plus one dispatch run, not a release blocker.
- **Options:**
  - (a) Lower priority now, and keep it in 4.15.0 because it is small.
  - (b) Keep the priority until the first green run, as the acceptance
    criteria say.
- **Recommendation:** (a). The acceptance criteria's re-assessment is still
  recorded after the run.

## Owner decisions (2026-10-21)

- D1: "Пушить task-ветку (Recommended)" — dispatch release.yml on
  `ci/GM-205-windows-archive-install`.
- D2: "Переименовать plugins\ на время шага (Recommended)" — option (a).
- D3: "Да, в этой задаче (Recommended)" — E7 lands in GM-205.
- D4: "Добавить в GM-205" — option (b): the one-liner / Windows PowerShell
  5.1 / real-download install check joins this task (acceptance criteria
  amended).
- D5: "Понизить сейчас (Recommended)" — priority 6 -> 2, still in 4.15.0.
