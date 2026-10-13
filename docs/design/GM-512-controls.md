# GM-512 controls

Each control reverts one behaviour in **production code only** (never a
test) in a throwaway worktree; the named test must then fail. The tests are:

- rust: `plugins/rust/tests/indexing_writes_nothing_and_stays_offline.rs`,
  test `indexing_reaches_no_network_and_writes_nothing_under_the_root`
  (binary `g-mesh-plugin-rust::indexing_writes_nothing_and_stays_offline`);
- python: `plugins/python/tests/indexing_writes_nothing_and_stays_offline.rs`,
  test `indexing_reaches_no_network_and_writes_nothing_under_the_root`
  (binary `g-mesh-plugin-python::indexing_writes_nothing_and_stays_offline`).
  A pin: nothing of ours prevents pyright from writing or connecting, so it
  has no control of its own (see C3);
- go: `plugins/go/semantic_offline_test.go`, tests
  `TestSemanticPassFetchesNoModule`,
  `TestSemanticPassRewritesNoGoModUnderUserModMod`,
  `TestSemanticPassLoadsAVendoredDependencyFromVendor` (subtests `module`,
  `workspace`), `TestModFlagFollowsTheGoverningFile`.

A failure is judged from stdout+stderr: a non-zero exit **and** the named
test reported as failed (cargo prints `test <name> ... FAILED` and
`test result: FAILED`; `go test` prints `--- FAIL: <Test>`). A failure on a
`sanity:` assertion means the revert broke loading rather than letting the
pass reach the network or write; the column below says where that is the
intended failure.

## Controls

| Ctl | File | Revert (code only) | Must fail | Failing assertion expected |
|---|---|---|---|---|
| C1 | `plugins/rust/src/semantic.rs`, `pinned_env` | delete `env.insert("CARGO_NET_OFFLINE".to_string(), "true".to_string());` | rust integration test | "network fixture: the semantic pass made network connections" (dry-run: `CONNECT github.com:443` and `CONNECT api.github.com:443`, 3 each) |
| C2 | `plugins/rust/plugin.toml` | under `[plugin.semantic.initialization_options.cargo.buildScripts]` set `enable = true` | rust integration test | "writes fixture: the semantic pass wrote under the project root" (`added target`, ...) |
| C3 | `plugins/sdk/src/lsp/resolve.rs`, `npm_candidates_in` | delete the `if !npx { return candidates; }` early return (python gets the `npx` candidate back) | python lib `semantic::tests::a_bare_name_is_path_only_never_a_project_install_or_npx` and `a_real_npx_in_the_project_never_runs_the_projects_pyright` | Same as GM-513 F1 (`docs/design/GM-513-controls.md`); listed so the GM-512 python criterion has its control. The new python integration test is not expected to fail under it: pyright is on `PATH`, and that candidate comes first |
| C4 | `plugins/go/semantic.go`, `loadEnv` | remove `, "GOPROXY=off"` from the appended list | `TestSemanticPassFetchesNoModule` | "the semantic pass reached the module proxy: [GET /example.com/gm512missing/@v/v1.0.0.zip ..., GET .../v1.0.0.mod ...]" |
| C5 | `plugins/go/semantic.go`, `semanticEngine.load` | delete the `if flag := modFlag(dir, os.Getenv); flag != "" { cfg.BuildFlags = ... }` block | `TestSemanticPassRewritesNoGoModUnderUserModMod` | "the semantic pass wrote under the root: [changed dep/go.mod changed go.mod]". `TestSemanticPassLoadsAVendoredDependencyFromVendor` also fails (sanity: `useDep` unresolved, the user's GOFLAGS wins) |
| C6 | `plugins/go/semantic.go`, `modFlag` | `return "-mod=vendor"` becomes `return "-mod=readonly"` | `TestSemanticPassLoadsAVendoredDependencyFromVendor` (both subtests) | "sanity: the semantic pass did not resolve [app/]a.go:useDep CALLS example.com/gv#Plain.Seven" - the dependency is unloadable without vendor/, which is the intended failure. `TestModFlagFollowsTheGoverningFile` also fails on the vendor cases |
| C7 | `plugins/go/semantic.go`, `modFlag` | both `return ""` (after the `GO111MODULE == "off"` check and after `governing == ""`) become `return "-mod=readonly"` | `TestModFlagFollowsTheGoverningFile` | `modFlag(mod) = "-mod=readonly", want ""` (GO111MODULE=off), same for `vmod`, `none` |

`Cargo.lock` absence is pinned by the rust test's snapshot and has no
control: no code of ours prevents it (design 1.rust.2).

## Grouping into builds

One throwaway worktree, target seeded with `cp -c -R <task-wt>/target
<ctl-wt>/target`; python needs `plugins/python/node_modules` copied too.
Reverts in different test binaries share a build; each group is applied,
built and run, then `git checkout -- .`. The go controls are one test binary,
so each is its own (cheap, seconds) `go test` run; C2 changes only the
runtime manifest, so its rust run reuses C1's build after the checkout.

| Group | Reverts | Run |
|---|---|---|
| G1 | C1 + C4 | `cargo test -p g-mesh-plugin-rust --test indexing_writes_nothing_and_stays_offline` ; `(cd plugins/go && go test -count=1 -run 'TestSemanticPassFetchesNoModule' .)` |
| G2 | C2 + C5 | rust command as G1 ; `(cd plugins/go && go test -count=1 -run 'TestSemanticPassRewritesNoGoModUnderUserModMod' .)` |
| G3 | C3 + C6 | `cargo test -p g-mesh-plugin-python --lib a_bare_name_is_path_only` and `--test semantic_runs_no_project_code` ; `(cd plugins/go && go test -count=1 -run 'TestSemanticPassLoadsAVendoredDependencyFromVendor' .)` |
| G4 | C7 | `(cd plugins/go && go test -count=1 -run 'TestModFlagFollowsTheGoverningFile' .)` |

## Results in the tests slice

C1 was dry-run in a throwaway worktree (target cloned): it failed as
intended, exit 101, on "network fixture: the semantic pass made network
connections". C4-C7 were each run there too and failed with the assertions
in the table. C2 and C3 are left to verify.
