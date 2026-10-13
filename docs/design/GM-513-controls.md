# GM-513 controls

Each control reverts one behaviour in **production code only** (never a
test) in a throwaway worktree; the named test must then fail. The tests are:

- rust: `plugins/rust/tests/semantic_runs_no_project_code.rs`, test
  `rust_analyzer_runs_no_build_script_proc_macro_toolchain_or_rustc_the_project_names`
  (binary `g-mesh-plugin-rust::semantic_runs_no_project_code`);
- python: `plugins/python/tests/semantic_runs_no_project_code.rs`, tests
  `pyright_runs_neither_the_venv_python_nor_one_from_path`,
  `a_pyrightconfig_naming_the_venv_runs_no_interpreter_either`,
  `the_projects_own_pyright_is_never_probed_or_run`
  (binary `g-mesh-plugin-python::semantic_runs_no_project_code`);
- go: `plugins/go/semantic_project_code_test.go`, tests
  `TestSemanticPassRunsNoToolchainTheProjectNames`,
  `TestSemanticPassRunsNoPackagesDriverFromPath`.

A failure is judged from stdout+stderr: a non-zero exit **and** the named
test reported as failed (nextest prints `FAIL [...] <binary> <test>` and its
summary on stderr; `go test` prints `--- FAIL: <Test>`). For the rust and
python tests, a failure on the final assertion (the marker/log list) is the
intended one; a failure on a `sanity:` assertion means the revert broke the
server rather than letting project code run, and is reported as such.

## Controls

| Ctl | File | Revert (code only) | Must fail | Failing assertion expected |
|---|---|---|---|---|
| C1 | `plugins/rust/plugin.toml` | delete the tables `[plugin.semantic.initialization_options.cargo.buildScripts]` and `[plugin.semantic.initialization_options.procMacro]` (both `enable = false`) | rust integration test; also lib unit `semantic::tests::the_shipped_manifest_configures_the_server_this_module_expects` | `MARK_buildrs` (and/or `MARK_procmacro`, `target/`) in the written list |
| C2(a) | `plugins/rust/src/semantic.rs`, `pinned_env` | delete the `if let Some(pin) = &toolchain.pin { env.insert("RUSTUP_TOOLCHAIN", ..) }` block | rust integration test | `evil-cargo` / `evil-rustc` log. **Uncertain:** `RUSTC`, `CARGO_BUILD_RUSTC` and `CARGO` stay pinned to absolute toolchain binaries, so rust-analyzer may never reach a rustup proxy; if this control does not fail, the pin is defence in depth behind those, not a test gap to fix by weakening another pin |
| C2(b) | `plugins/rust/src/semantic.rs`, `candidates` | right after `let mut candidates = Vec::new();` insert `candidates.push((command.to_path_buf(), "PATH"));` (the bare name, probed first through `PATH`, which the test starts with rustup's proxy dir) | rust integration test | `evil-rust-analyzer` log (the proxy, run with cwd = fixture, picks the `rust-toolchain.toml` `path` toolchain) |
| C3 | `plugins/rust/src/semantic.rs`, `pinned_env` | delete the `for wrapper in [..] { env.insert(wrapper, String::new()) }` loop and the two inserts `RUSTC` / `CARGO_BUILD_RUSTC` (keep `RUSTUP_TOOLCHAIN` and `CARGO`) | rust integration test | `build-rustc` and `rustc-wrapper` logs (`.cargo/config.toml` `build.rustc` / `build.rustc-wrapper`) |
| C4 | `plugins/python/src/semantic.rs`, `add_project_settings` | replace `set_python_path(config, &no_interpreter());` with `set_python_path(config, &root.join(".venv").join("bin").join("python"));` | `pyright_runs_neither_the_venv_python_nor_one_from_path` (also lib unit `the_python_path_names_no_interpreter`) | `venv-python` log. `a_pyrightconfig_naming_the_venv_...` may or may not fail: whether pyright runs a client `pythonPath` when the project config names a venv was not measured |
| C5 | `plugins/python/src/semantic.rs`, `add_project_settings` | delete the line `set_python_path(config, &no_interpreter());` (pythonPath unset) | `pyright_runs_neither_the_venv_python_nor_one_from_path` and `a_pyrightconfig_naming_the_venv_runs_no_interpreter_either` | `path-python` log (design: 6 resp. 2 PATH-python calls) |
| C6 | `plugins/python/src/semantic.rs`, `candidates` | body becomes `g_mesh_plugin_sdk::lsp::npm_candidates(command, &std::env::current_dir().unwrap(), &PYRIGHT, script_extensions)` (the project's `node_modules/.bin` origin back; the test's cwd is the project root, so no signature change and the lib unit tests still compile) | `the_projects_own_pyright_is_never_probed_or_run` | `project-pyright` log (`PATH` lacks pyright, so the local candidate is probed) |
| C7 | `plugins/go/semantic.go`, `loadEnv` | remove `"GOTOOLCHAIN=local", ` from the appended list | `TestSemanticPassRunsNoToolchainTheProjectNames` | "the semantic pass did not load the project" (go.mod `toolchain go1.99.1`, `GOPROXY=off`: toolchain not available) |
| C8 | `plugins/go/semantic.go`, `loadEnv` | remove `"GOPACKAGESDRIVER=off"` from the appended list | `TestSemanticPassRunsNoPackagesDriverFromPath` | "ran the gopackagesdriver on PATH" (the shim on `PATH` exits 1) |

## Grouping into builds

One throwaway worktree, target seeded with `cp -c -R <task-wt>/target
<ctl-wt>/target`. Reverts in different test binaries share a build; each
group is applied, built and run, then `git checkout -- .`.

| Group | Reverts | Run |
|---|---|---|
| G1 | C1 + C4 + C7 | `cargo nextest run -p g-mesh-plugin-rust --test semantic_runs_no_project_code --lib` ; `cargo nextest run -p g-mesh-plugin-python --test semantic_runs_no_project_code` ; `(cd plugins/go && go test -count=1 -run 'TestSemanticPassRunsNo' .)` |
| G2 | C2(a) + C5 + C8 | same three commands (rust: `--test` only) |
| G3 | C2(b) + C6 | rust and python commands |
| G4 | C3 | rust command |

C1 needs no rebuild (the manifest is read at run time). The Go tests build
in seconds and need no seeding.

## Dry run (tests slice)

C1 was dry-run by the tests slice in a throwaway worktree; result in the
slice report.
