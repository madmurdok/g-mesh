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
  `workspace`), `TestModFlagFollowsTheGoverningFile`; and
  `plugins/go/semantic_goenv_test.go` (the go env file, GOENV), tests
  `TestSemanticPassLoadsGOPATHModeFromTheGoEnvFile`,
  `TestSemanticPassLoadsAWorkspaceMemberUnderGoWorkOffFromTheGoEnvFile`,
  `TestModFlagForResolvedGoEnv`, `TestModFlagFallsBackToReadonlyWhenGoEnvFails`;
  and `plugins/go/semantic_offline_sumdb_test.go` (direct connections that
  bypass GOPROXY=off, seen through an HTTPS_PROXY sink), tests
  `TestSemanticPassFetchesNoPrivateModuleDirectly` (subtests `GOPRIVATE`,
  `GONOPROXY`), `TestSemanticPassInAWorkspaceWritesOnlyGoWorkSum`.

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
| C5 | `plugins/go/semantic.go`, `semanticEngine.load` | delete the `if flag := modFlag(dir, env); flag != "" { cfg.BuildFlags = ... }` block | `TestSemanticPassRewritesNoGoModUnderUserModMod` | "the semantic pass wrote under the root: [changed dep/go.mod changed go.mod]". `TestSemanticPassLoadsAVendoredDependencyFromVendor` also fails (sanity: `useDep` unresolved, the user's GOFLAGS wins) |
| C6 | `plugins/go/semantic.go`, `modFlagFor` | `return "-mod=vendor"` becomes `return "-mod=readonly"` | `TestSemanticPassLoadsAVendoredDependencyFromVendor` (both subtests) | "sanity: the semantic pass did not resolve [app/]a.go:useDep CALLS example.com/gv#Plain.Seven" - the dependency is unloadable without vendor/, which is the intended failure. `TestModFlagFollowsTheGoverningFile` also fails on the vendor cases |
| C7 | `plugins/go/semantic.go`, `modFlagFor` | both `return ""` (after `gomod == ""` and after `governing == ""`) become `return "-mod=readonly"` | `TestModFlagFollowsTheGoverningFile` | `modFlag(mod) = "-mod=readonly", want ""` (GO111MODULE=off), same for `vmod`, `none` |
| C8 | `plugins/go/semantic.go`, `modFlag` | `cmd.Env = env` becomes `cmd.Env = append(env, "GOENV=off")` (the flag is chosen without the go env file, as when only the process environment was read) | `TestSemanticPassLoadsGOPATHModeFromTheGoEnvFile` and `TestSemanticPassLoadsAWorkspaceMemberUnderGoWorkOffFromTheGoEnvFile` | "the semantic pass did not load the GOPATH-mode package" (-mod=readonly in GOPATH mode, go rejects it) and "the semantic pass did not resolve app/a.go:use CALLS ... under GOWORK=off" (-mod=vendor from the workspace, inconsistent vendoring) |
| C9 | `plugins/go/semantic.go`, `modFlagFor` | delete the `if gomod == "" { return "" }` guard | `TestModFlagForResolvedGoEnv` | subtest `GOPATH mode, GOWORK still set`: `= "-mod=vendor", want ""` |
| C10 | `plugins/go/semantic.go`, `modFlag` | both `return "-mod=readonly"` in the `go env` error branches become `return ""` | `TestModFlagFallsBackToReadonlyWhenGoEnvFails` (both subtests) | "modFlag with a broken `go env` = \"\", want -mod=readonly" |
| C11 | `plugins/go/semantic.go`, `modFlag` | `cmd.Env = env` becomes `cmd.Env = os.Environ()` (`go env` no longer runs under the load's env) | `TestModFlagFollowsTheGoverningFile` | subtests `GO111MODULE=off`, `GO111MODULE=off, vendored`, `GOWORK=off ...`: the settings passed only in the env slice are lost |
| C12 | `plugins/go/semantic.go`, `loadEnv` | remove `"GONOPROXY=-",` from the appended list | `TestSemanticPassFetchesNoPrivateModuleDirectly` (both subtests) | "the semantic pass connected out with GOPRIVATE=example.com: [CONNECT example.com:443 ...]", same for GONOPROXY |
| C13 | `plugins/go/semantic.go`, `loadEnv` | remove `"GOSUMDB=off"` from the appended list (and the comma before it) | `TestSemanticPassInAWorkspaceWritesOnlyGoWorkSum` | fails on the missing `useDep` edge (sanity check near test line 193): without GOSUMDB=off the offline lookup fails before the connect assertion is reached |

The go.work.sum write in a workspace whose member go.sum lacks entries for
cached modules is go's own, the owner-accepted exception in ADR 0033: it is
pinned by `TestSemanticPassInAWorkspaceWritesOnlyGoWorkSum` (the only change
under the root) and has no control. Clearing GOPRIVATE was superseded by
GOSUMDB=off and has no control either.

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
| G5 | C8 | `(cd plugins/go && go test -count=1 -run 'FromTheGoEnvFile' .)` |
| G6 | C9 | `(cd plugins/go && go test -count=1 -run 'TestModFlagForResolvedGoEnv' .)` |
| G7 | C10 | `(cd plugins/go && go test -count=1 -run 'TestModFlagFallsBackToReadonlyWhenGoEnvFails' .)` |
| G8 | C11 | `(cd plugins/go && go test -count=1 -run 'TestModFlagFollowsTheGoverningFile' .)` |
| G9 | C12 | `(cd plugins/go && go test -count=1 -run 'TestSemanticPassFetchesNoPrivateModuleDirectly' .)` |
| G10 | C13 | `(cd plugins/go && go test -count=1 -run 'TestSemanticPassInAWorkspaceWritesOnlyGoWorkSum' .)` |

## Results in the tests slice

C1 was dry-run in a throwaway worktree (target cloned): it failed as
intended, exit 101, on "network fixture: the semantic pass made network
connections". C4-C7 were each run there too and failed with the assertions
in the table. C2 and C3 are left to verify.

C8-C11 come from the fix round on the go env file: they pin behaviours
(the go env file, the `gomod == ""` guard, the `go env` failure fallback,
`go env` under the load's env) that C1-C7 do not tell apart, hence more than
eight controls in this task. C8 was dry-run in a throwaway worktree (Go
only): exit 1, both `FromTheGoEnvFile` tests failed with "did not load the
GOPATH-mode package; edges = []" and "did not resolve app/a.go:use CALLS
example.com/gv#Plain.Seven under GOWORK=off; edges = []". C9-C11 are left
to verify.

C12-C13 come from the GOPROXY=off bypass round (GONOPROXY=-, GOSUMDB=off).
C12 was dry-run in a throwaway worktree (Go only): exit 1, both subtests of
`TestSemanticPassFetchesNoPrivateModuleDirectly` failed with "the semantic
pass connected out with GOPRIVATE=example.com: [CONNECT example.com:443
HTTP/1.1]" (same for GONOPROXY). C13 is left to verify.
