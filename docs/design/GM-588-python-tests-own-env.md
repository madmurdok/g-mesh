# GM-588: Python `semantic_runs_no_project_code` tests build their own environment

Status: diagnosis (S1). Nothing in the code has changed yet.

## Finding: the file depends on test order

`plugins/python/tests/semantic_runs_no_project_code.rs` has four tests. Each
one calls `set_environment` (l.114-119), which changes this process's `PATH`,
`G_MESH_PLUGIN_MANIFEST` and `VIRTUAL_ENV` and its working directory. The file
never restores them. Every test starts by calling `absolute("node")`
(l.58-63). That helper looks `node` up on the *current* process `PATH`.

- `the_projects_own_pyright_is_never_probed_or_run` and
  `a_real_npx_in_the_project_never_runs_the_projects_pyright` narrow `PATH` to
  `<scratch>/shims:/usr/bin:/bin`. The shims directory belongs to a `Scratch`,
  and the `Scratch` deletes it when the test ends. That leaves a `PATH` with
  no `node` on it. On this machine `node` is `/usr/local/bin/node`.
- Any test that runs after one of those two in the same process panics with
  `this test needs \`node\` on PATH` (l.61) before it reaches the code under
  test.
- The two `pyright_runs_no_interpreter` tests put the real node directory on
  `PATH`, so a test that runs after them passes.

The `ENVIRONMENT` mutex makes the tests run one at a time. It does not restore
anything, so the order still decides the result.

### Evidence

Run on 2026-10-14, worktree `g-mesh-wt-gm588` at `1ab6896`, pyright 1.1.414
(`scripts/test-deps.sh pyright`). Load average was 69 at the start and 28 at
the end. Logs are in the slice scratchpad (`gm588/`).

| Run | Result |
|---|---|
| each test alone, `cargo nextest run -E 'test(=…)'` (4 runs) | all pass |
| each test alone, `cargo test -- --exact …` (4 runs) | all pass |
| `cargo nextest run --test semantic_runs_no_project_code` | pass (one process per test) |
| forward: `cargo test -- --test-threads=1` (alphabetical: a_pyrightconfig, a_real_npx, pyright_runs_neither, the_projects_own) | **2 fail**: `pyright_runs_neither…` and `the_projects_own…`, both with `this test needs \`node\` on PATH` |
| reverse: scratch copy with the tests renamed `t1_the_projects_own` … `t4_a_pyrightconfig`, `--test-threads=1` | **3 fail**: every test after `t1`, with the same panic |
| default `cargo test` (threads plus the mutex, any order), 3 runs | **fail 3/3**: 2, 3 and 2 tests, with the same panic |

How the reverse run worked: libtest and nextest both order tests by name, and
neither has a reverse option. A one-process run in reverse order therefore
needs the names to sort in reverse. The script copied the file to
`tests/gm588_reverse_scratch.rs`, renamed the four `fn`s with `t1_`…`t4_`
prefixes using `sed`, ran the copy and then deleted it. The test bodies in the
copy are byte-identical to the original.

### Why CI is green

CI runs the suite through `scripts/test-sections.sh run --profile ci
plugin-python`, which calls `cargo nextest run` (ci.yml l.802, test-sections.sh
l.167). nextest runs each test in its own process, so CI never sees the
problem. It shows up locally under plain `cargo test`, including the
`cargo test --workspace` that the `scripts/test-deps.sh` header names.

### Where the process environment is read

The fix has to cover each of these reads.

- `absolute()` (test helper): uses the process `PATH`. This is the failure we
  observed.
- `SemanticConfig::from_manifest` → `manifest::manifest_path`
  (`plugins/sdk/src/manifest.rs:165`): uses the process `G_MESH_PLUGIN_MANIFEST`.
- `lsp::resolve::probe` (`plugins/sdk/src/lsp/resolve.rs:148`): spawns the bare
  `pyright` name, so it uses the process `PATH`. It sets no `current_dir`, so
  it also runs in the process's working directory. Control F1 (the npx
  candidate) depends on that working directory being the project.
- `LspClient::start` (`plugins/sdk/src/lsp/client.rs:228`): inherits the
  process `PATH`. It sets `current_dir(root)` itself.

## Decision 1: how each test gets its own environment

**Today:** each test rewrites this process's environment and hopes that the
tests before it left the environment in a usable state.

### A. Each test runs in a child process with its own environment (recommended)

Each `#[test]` keeps the arrange step: the scratch project, shims and logs. It
then runs the act and assert steps in a child process: `std::env::current_exe()`
started with `--exact <name>_in_its_env --ignored --nocapture`, with
`Command::env("PATH", …)`, `.env(MANIFEST_PATH_ENV, …)`,
`.env_remove("VIRTUAL_ENV")`, `.env(SCRATCH_ENV, scratch)` and
`.current_dir(root)`. The child is an `#[ignore]`d test. It reads its fixture
paths from `SCRATCH_ENV`, runs the sanity checks, the engine and the
assertions, and exits. The parent asserts that the child exited 0 and prints
the child's output. The parent never calls `set_var`. `absolute()` then always
reads the `PATH` the process started with.

- *Benefit:* no production code changes. The engine reads `PATH`, the
  manifest variable and the working directory the way production does, so the
  tests still cover the real code path, including F1's dependence on the
  working directory. The `ENVIRONMENT` mutex and `set_environment` are
  removed. Tests can run in parallel under `cargo test`.
- *Risk:* the inner `#[ignore]` tests show up in listings, and
  `--include-ignored` / `--run-ignored all` would run them in the parent
  environment. Each inner test therefore checks for `SCRATCH_ENV` and returns
  early with an `eprintln!` when it is missing (see decision 2).
  `the_real_sections_partition_the_real_suite` counts ignored tests. They live
  in the same binary, so the `plugin-python` section should already cover
  them, but verify must check. Each test also costs one more process start
  (about 0.1 s).
- *Variant:* use `harness = false` with a custom `main` that dispatches on an
  environment variable. This avoids the ignored tests, but it changes
  `Cargo.toml`, loses the libtest filters, and nextest needs a custom-harness
  list format. Not recommended.

### B. An SDK entry point that takes the environment

Add `semantic::engine_with(root, &ServerEnv { path, manifest, cwd })`. Thread it
through `lsp::resolve` (look up bare names on `env.path`, then
`probe(...).env("PATH", …).current_dir(cwd)`) and through `LspClient::start`
(`.env("PATH", …)`). Add `SemanticConfig::from_manifest_at` for the manifest.
`from_manifest_at` already exists for the same reason.

- *Benefit:* no process environment is involved anywhere. The tests become
  ordinary in-process tests that run in parallel.
- *Risk:* this is a production change to code that all three plugins share
  (`resolve.rs`, `client.rs`, and every plugin's `engine`). The tests would
  then cover the injected path, not the process-environment path that
  production uses. A future production read of `std::env` or of the working
  directory (the probe's working directory already is one) would go untested.
  It is also a much larger change than a test-hygiene task needs.

### C. Minimal: capture the original `PATH` once

Copy the TypeScript suite's `ORIGINAL_PATH: OnceLock` (`plugins/typescript/tests/no_project_code/mod.rs:73-90`)
into `absolute()`.

- *Benefit:* a 10-line change that fixes the observed failure.
- *Risk:* the file still changes the process environment, so it fails
  acceptance criterion 2. The working directory and `VIRTUAL_ENV` changes stay
  hidden coupling between tests.

**Recommendation: A.** It is the only option that meets criteria 1-2 without
changing production code.

## Decision 2: what an inner test does when started without its parent

- **Return early with `eprintln!` (recommended).** *Benefit:* `--include-ignored`
  and `--run-ignored all` stay green. *Risk:* a mistaken direct run reports
  "ok" while testing nothing. The parent's assertion on the child's exit code
  is what actually checks the behaviour.
- **Panic.** *Benefit:* a mistaken direct run is loud. *Risk:* any
  `--run-ignored all` run fails, and the sections `check` lists with
  `--run-ignored all`. It only lists and does not run, but local habits differ.

## Decision 3: scope

Other files follow the same pattern: `plugins/rust/tests/semantic_runs_no_project_code.rs`
(`set_var("PATH")` l.163, `set_current_dir` l.436/629), and the
`indexing_writes_nothing_and_stays_offline.rs` files in the Python and Rust
plugins. The TypeScript suite already has the `ORIGINAL_PATH` mitigation, but
it still calls `set_var`. This diagnosis did not measure any of them.

- **GM-588 = the Python file only, and one new task per sibling (recommended).**
  *Benefit:* a small diff that matches the acceptance criteria. *Risk:* the
  siblings keep the same latent order dependence until their tasks are done.
- **Fix all of them in GM-588.** *Benefit:* one pattern, applied once. *Risk:*
  four files with different fixtures, and no diagnosis evidence for three of
  them.

## Edit map (option A, Python file only)

`plugins/python/tests/semantic_runs_no_project_code.rs`:

1. Module doc (l.21-23): replace "rewrite PATH … hold one lock" with the
   parent/child split.
2. Remove `static ENVIRONMENT` (l.38) and `set_environment` (l.114-119). Remove
   the `Mutex` import.
3. Add `const SCRATCH_ENV`, `fn in_its_env(test: &str, path: &[PathBuf], cwd: &Path, scratch: &Path)`
   (spawn `current_exe()` as above and assert success, printing stdout and
   stderr), and `fn scratch_from_env() -> Option<PathBuf>`.
4. `pyright_runs_no_interpreter`: keep everything up to the `set_environment`
   call in the parent, then call `in_its_env`. Move the sanity checks, the
   engine and the assertions (l.162-215) into
   `fn act(config_names_the_venv, scratch)`, called by two `#[ignore]` inner
   tests.
5. `the_projects_own…` and `a_real_npx…`: split them the same way. The `npx`
   sanity step (`run(Path::new("npx"), …)`) and the `Command::new("pyright")`
   absence checks move into the child, because they depend on the child's
   `PATH`.
6. `Scratch::new` uses `std::process::id()`. Only the parent creates it, so
   names stay unique per test. The child must not create a `Scratch`.

## Control (acceptance criterion 3)

In the parent of `a_pyrightconfig_naming_the_venv_runs_no_interpreter_either`
(first by name), add `std::env::set_var("PATH", "/usr/bin:/bin")`. Then run
`cargo test -p g-mesh-plugin-python --test semantic_runs_no_project_code --
--test-threads=1`. `a_real_npx…` must fail with `this test needs \`node\` on
PATH`. Revert the edit and the same run must pass. Verify with a forward run, a
reverse run (the renamed scratch copy above) and 5 default `cargo test` runs.
