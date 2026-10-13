# GM-513 controls

Each control reverts one behaviour in **production code only** (never a
test) in a throwaway worktree; the named test must then fail. The tests are:

- rust: `plugins/rust/tests/semantic_runs_no_project_code.rs`, test
  `rust_analyzer_runs_no_build_script_proc_macro_toolchain_or_rustc_the_project_names`
  (binary `g-mesh-plugin-rust::semantic_runs_no_project_code`);
- rust lib (`plugins/rust/src/semantic.rs`, binary `g-mesh-plugin-rust`):
  `semantic::tests::the_server_env_pins_the_toolchain_by_name`,
  `semantic::tests::a_proxy_is_a_proxy_wherever_it_is_spelled`,
  `semantic::tests::candidates_never_offer_a_proxy_under_any_path_spelling`
  (re-runs this binary as a child with its own `PATH`);
- python: `plugins/python/tests/semantic_runs_no_project_code.rs`, tests
  `pyright_runs_neither_the_venv_python_nor_one_from_path`,
  `a_pyrightconfig_naming_the_venv_runs_no_interpreter_either`,
  `the_projects_own_pyright_is_never_probed_or_run`,
  `a_real_npx_in_the_project_never_runs_the_projects_pyright`
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
| C2(a) | `plugins/rust/src/semantic.rs`, `pinned_env` | delete the `if let Some(pin) = &toolchain.pin { env.insert("RUSTUP_TOOLCHAIN", ..) }` block | rust lib `semantic::tests::the_server_env_pins_the_toolchain_by_name` | `RUSTUP_TOOLCHAIN` is `None`, expected `Some("gm513-pin")`. Replaces the integration-test arm: `RUSTC`/`CARGO` name absolute binaries there, so no rustup proxy is reached and the pin had no observable effect (verify) |
| C2(b) | `plugins/rust/src/semantic.rs`, `candidates` | right after `let mut candidates = Vec::new();` insert `candidates.push((command.to_path_buf(), "PATH"));` (the bare name, probed first through `PATH`, which the test starts with rustup's proxy dir) | rust integration test | `evil-rust-analyzer` log (the proxy, run with cwd = fixture, picks the `rust-toolchain.toml` `path` toolchain) |
| C3 | `plugins/rust/src/semantic.rs`, `pinned_env` | delete the `for wrapper in [..] { env.insert(wrapper, String::new()) }` loop and the two inserts `RUSTC` / `CARGO_BUILD_RUSTC` (keep `RUSTUP_TOOLCHAIN` and `CARGO`) | rust integration test | `build-rustc` and `rustc-wrapper` logs (`.cargo/config.toml` `build.rustc` / `build.rustc-wrapper`) |
| C4 | `plugins/python/src/semantic.rs`, `add_project_settings` | replace `set_python_path(config, &no_interpreter());` with `set_python_path(config, &root.join(".venv").join("bin").join("python"));` | `pyright_runs_neither_the_venv_python_nor_one_from_path` (also lib unit `the_python_path_names_no_interpreter`) | `venv-python` log. `a_pyrightconfig_naming_the_venv_...` may or may not fail: whether pyright runs a client `pythonPath` when the project config names a venv was not measured |
| C5 | `plugins/python/src/semantic.rs`, `add_project_settings` | delete the line `set_python_path(config, &no_interpreter());` (pythonPath unset) | `pyright_runs_neither_the_venv_python_nor_one_from_path` and `a_pyrightconfig_naming_the_venv_runs_no_interpreter_either` | `path-python` log (design: 6 resp. 2 PATH-python calls) |
| C6 | `plugins/python/src/semantic.rs`, `candidates` | body becomes `g_mesh_plugin_sdk::lsp::npm_candidates(command, &std::env::current_dir().unwrap(), &PYRIGHT, script_extensions)` (the project's `node_modules/.bin` origin back; the test's cwd is the project root, so no signature change and the lib unit tests still compile) | `the_projects_own_pyright_is_never_probed_or_run` | `project-pyright` log (`PATH` lacks pyright, so the local candidate is probed) |
| F1 | `plugins/sdk/src/lsp/resolve.rs`, `npm_candidates_in` | delete the `if !npx { return candidates; }` early return (python gets the `npx` candidate back) | `a_real_npx_in_the_project_never_runs_the_projects_pyright` (also `the_projects_own_pyright_is_never_probed_or_run`, and python lib unit `semantic::tests::a_bare_name_is_path_only_never_a_project_install_or_npx`) | `project-pyright-package` marker and `npx` log in the written list (the real `npx` runs the project's `node_modules/pyright`) |
| F2 | `plugins/rust/src/semantic.rs`, `is_rustup_proxy` | body becomes `found.parent() == rustup.parent()` (the uncanonicalized directory rule) | rust lib `a_proxy_is_a_proxy_wherever_it_is_spelled` and `candidates_never_offer_a_proxy_under_any_path_spelling` | "a symlink from another dir"; the child reports "PATH=other: a proxy was offered" |
| C7 | `plugins/go/semantic.go`, `loadEnv` | remove `"GOTOOLCHAIN=local", ` from the appended list | `TestSemanticPassRunsNoToolchainTheProjectNames` | "the semantic pass did not load the project" (go.mod `toolchain go1.99.1`, `GOPROXY=off`: toolchain not available) |
| C8 | `plugins/go/semantic.go`, `loadEnv` | remove `"GOPACKAGESDRIVER=off"` from the appended list | `TestSemanticPassRunsNoPackagesDriverFromPath` | "ran the gopackagesdriver on PATH" (the shim on `PATH` exits 1) |

## Grouping into builds

One throwaway worktree, target seeded with `cp -c -R <task-wt>/target
<ctl-wt>/target`. Reverts in different test binaries share a build; each
group is applied, built and run, then `git checkout -- .`.

| Group | Reverts | Run |
|---|---|---|
| G1 | C1 + C4 + C7 | `cargo nextest run -p g-mesh-plugin-rust --test semantic_runs_no_project_code --lib` ; `cargo nextest run -p g-mesh-plugin-python --test semantic_runs_no_project_code` ; `(cd plugins/go && go test -count=1 -run 'TestSemanticPassRunsNo' .)` |
| G2 | C2(a) + C5 + C8 | `cargo nextest run -p g-mesh-plugin-rust --lib -E 'test(the_server_env_pins_the_toolchain_by_name)'` ; python and go commands as G1 |
| G3 | C2(b) + C6 | rust and python commands |
| G4 | C3 | rust command |
| G5 | F1 + F2 | `cargo nextest run -p g-mesh-plugin-python --test semantic_runs_no_project_code --lib` ; `cargo nextest run -p g-mesh-plugin-rust --lib -E 'test(/proxy/)'` |

C1 needs no rebuild (the manifest is read at run time). The Go tests build
in seconds and need no seeding.

## Dry run (tests slice)

C1 was dry-run by the tests slice in a throwaway worktree; result in the
slice report. F1 was dry-run by the second tests slice (S11) the same way;
result in that slice's report.
