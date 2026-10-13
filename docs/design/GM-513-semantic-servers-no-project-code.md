# GM-513: semantic servers never execute project-configured code

Status: design (slice S1), awaiting owner decisions D1-D6.
Branch: `fix/GM-513-semantic-servers-run-no-project-code`.

## Summary

Every one of the three semantic tiers runs something the indexed project
chooses, today, with the shipped configuration:

| Tier | What runs from the project today (observed) | Stopped by |
|---|---|---|
| rust (rust-analyzer 1.97.1) | `build.rs`, proc macros, `.cargo/config.toml` `build.rustc` / `rustc-wrapper`, a `rust-toolchain.toml` `path =` toolchain (its `cargo`, `rustc` and even `rust-analyzer` itself) | RA options + an env pin (all observed) |
| python (pyright 1.1.414) | `<root>/.venv/bin/python` (the plugin itself hands it to pyright as `python.pythonPath`); `<root>/node_modules/.bin/pyright` (the plugin probes it) | no interpreter at all + `extraPaths` (observed); drop the project-local server origin |
| go (`go/packages`, in-process, no gopls) | nothing from the project. It does run a different toolchain named by `go.mod` (`GOTOOLCHAIN=auto` download), a `gopackagesdriver` found on `PATH`, and the system `cc`/`pkg-config` for cgo files | `GOTOOLCHAIN=local`, `GOPACKAGESDRIVER=off` (observed); cgo is a decision |

Turning rust-analyzer's build scripts and proc macros off means it builds
nothing: no `target/` appears. That closes GM-524's D3 without a
`cargo.targetDir` (section 3).

## How this was established

All experiments ran in
`/private/tmp/claude-502/.../scratchpad/gm513` (`run.sh`, `run2.sh`,
`lsp_drive.py`, outputs `summary.txt`, `summary2.txt`). Load average 15-38
during the runs; timings below are only indicative.

- `lsp_drive.py` is a minimal LSP client: `initialize` with the arm's
  `initializationOptions`, answers `workspace/configuration` from the arm's
  settings, waits for rust-analyzer's `quiescent` status (or 6 s for
  pyright), asks one `textDocument/definition`, shuts down. The server is
  started with `cwd = <fixture>`, as `LspClient::start` does
  (`plugins/sdk/src/lsp/client.rs:233`), and rust-analyzer through the
  rustup proxy `~/.cargo/bin/rust-analyzer`, as the plugin's bare `command`
  resolves on a rustup machine.
- Every hostile hook is a shell script that appends its argv to a log in the
  arm's directory, or Rust code that writes a `MARK_*` file. An arm's verdict
  is "which logs/markers exist afterwards".
- The Go arms run a 30-line loader with the plugin's exact `packages.Config`
  (`loadMode`, `Tests: true`; `plugins/go/semantic.go:128,313`), against
  x/tools v0.50.0 (the plugin's `go.mod`).

### Rust fixture (`rfx`)

Workspace `app` + `mac`. `app/build.rs` writes `MARK_buildrs`; `mac` is a
function-like proc macro whose expansion writes `MARK_procmacro` and emits
`pub fn generated()`; `app/src/main.rs` calls `generated()` - the definition
asked for. `.cargo/config.toml` sets `build.rustc-wrapper` to a logging shim.

`BASE` = today's options `{"checkOnSave": false}`.
`PROP` = `{"checkOnSave": false, "cargo": {"buildScripts": {"enable": false}}, "procMacro": {"enable": false}}`.

| Arm | Options / extra | Markers and logs | `definition` of `generated()` | `target/` |
|---|---|---|---|---|
| r_base | BASE | `MARK_buildrs`, `MARK_procmacro`, wrapper 32 calls | resolves (macro call site) | 2.6 MB created |
| r_prop | PROP | wrapper 27 calls (`rustc -vV`, `--print cfg`, `--print=file-names`) | `[]` | absent |
| r_prop_envwrap | PROP + `RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER=` | none | `[]` | absent |
| r_prop_ratoml_on | PROP + project `rust-analyzer.toml` enabling both | wrapper only; no build/proc-macro marker | `[]` | absent |
| r_prop_ratoml_override | PROP + `rust-analyzer.toml` `cargo.buildScripts.overrideCommand = [sh -c touch MARK]` | wrapper only; no marker | `[]` | absent |
| r_prop_buildrustc | PROP + `.cargo/config.toml` `build.rustc = <shim>` | shim rustc 45 calls | `[]` | absent |
| r_prop_toolchain | PROP + `rust-toolchain.toml` `[toolchain] path = <evil_tc>` | evil `rust-analyzer` 1, `cargo` 27, `rustc` 51 calls | `[]` | absent |
| r_prop_toolchain_pinned | as above + `RUSTUP_TOOLCHAIN=stable-x86_64-apple-darwin` | no evil-toolchain call | `[]` | absent |
| r_probe_version | `rust-analyzer --version` via proxy, `cwd` = that fixture | evil `rust-analyzer --version` ran | - | - |
| r_rustup_which | `rustup which rust-analyzer`, same `cwd` | prints `<evil_tc>/bin/rust-analyzer` | - | - |
| f_all_env (run2) | PROP + `build.rustc` + both wrappers + toolchain `path`, env: pin + empty wrappers + `RUSTC` + `CARGO_BUILD_RUSTC` | none | `[]` | absent |
| f_no_rustc_env | same, without `RUSTC`/`CARGO_BUILD_RUSTC` | shim rustc 27 calls | `[]` | absent |
| f_only_cargo_build_rustc / f_only_rustc | either one of the two | none | `[]` | absent |

No arm created `Cargo.lock` (rust-analyzer 1.97 passes `-Zlockfile-path` to
`cargo metadata`, seen in the evil-cargo log). Relevant to GM-512.

### Python fixture (`pfx`)

`main.py` does `from vpkg import vfunc; vfunc()`; `vpkg` lives only in
`.venv/lib/python3.9/site-packages`. `.venv/bin/python` is a logging shim
(exec `/usr/bin/python3`); a `python`/`python3` logging shim is first on
`PATH`. `json.py` and `sitecustomize.py` at the root write markers if
imported. (Each arm's `PATH` log has one extra line: the driver itself.)

| Arm | Settings / project config | Executed | `vfunc` resolves |
|---|---|---|---|
| p_plugin_today | `python.pythonPath = <root>/.venv/bin/python` (what `add_project_settings` sends) | venv python 6 calls: 2 with `-I` (cwd = pyright's dist), 4 **without** `-I`, cwd = project root | no (the shim's prefix is not the venv) |
| p_no_pythonpath | none | `PATH` python 6 calls, same shape | no |
| p_config_venv | project `pyrightconfig.json` `{"venvPath": ".", "venv": ".venv"}` | `PATH` python 2 calls; venv python **not** run | yes (directory scan) |
| p_extrapaths | `analysis.extraPaths = [<venv site-packages>]` | `PATH` python | yes |
| p_false_extrapaths | `pythonPath = /usr/bin/false` + `extraPaths` | **nothing** | yes |
| p_false_config_venv | `pythonPath = /usr/bin/false` + project venv config | nothing | yes |
| p_cfg_pythonpath (run2) | project `pyrightconfig.json` `{"pythonPath": ".venv/bin/python"}` | venv python not run: pyright has no such config key | - |
| p_pth_extrapaths (run2) | real `python3 -m venv`, editable install as `__editable__.edpkg.pth`; `pythonPath = /usr/bin/false` + `extraPaths` | nothing | yes (pyright reads `.pth` in an extra path) |
| p_pth_interp (run2) | same venv, today's `pythonPath` | venv python | yes |

`json.py`/`sitecustomize.py` markers never appeared, so the non-isolated
calls did not import project modules in this fixture; that is not a
guarantee (`sys.path[0]` is the project root for those calls).

### Go fixture (`gfx`)

`a.go` plain; `c.go` cgo with `#cgo pkg-config: gm513fake`. Shims on `PATH`:
`cc`, `pkg-config`, and (in the driver arms only) `gopackagesdriver`.
`GOPROXY=off` in every arm.

| Arm | Env | Executed | Result |
|---|---|---|---|
| g_base | inherited | `pkg-config --cflags/--libs -- gm513fake`, `cc` 29 calls (`go list -compiled -export` compiles deps and cgo) | loads, `Seven` resolved |
| g_nocgo | `CGO_ENABLED=0` | nothing | loads, `c.go` dropped, `Seven` missing |
| g_driver | `gopackagesdriver` on `PATH` | the driver, instead of `go list` | load fails (shim exits 1) |
| g_driver_off | + `GOPACKAGESDRIVER=off` | no driver | loads |
| g_newer_go | `go.mod` `go 1.99.0` | `go: downloading go1.99.0` (blocked only by `GOPROXY=off`) | fails |
| g_newer_go_local | + `GOTOOLCHAIN=local` | nothing downloaded | fails: `go.mod requires go >= 1.99.0` |
| g_toolchain_line | `go 1.27.0` + `toolchain go1.99.1` | download attempted | fails |
| g_toolchain_line_local | + `GOTOOLCHAIN=local` | nothing downloaded | loads |

Source confirmation: `findExternalDriver` (x/tools `go/packages/external.go:89`)
runs `gopackagesdriver` from `PATH` unless `GOPACKAGESDRIVER=off`;
`golistargs` (`golist.go:862`) passes `-compiled=true -export=true -deps=true`
for this mode (`usesExportData`: `NeedTypes` without `NeedDeps`).

## 1. What each server can execute from the project

### rust-analyzer (and the plugin around it)

1. **Build scripts** (`build.rs`) - RA's `cargo.buildScripts.enable`
   (default true) runs `cargo check --workspace --compile-time-deps
   --all-targets` (GM-524 trace) which compiles and runs every build script.
   Observed: r_base `MARK_buildrs`.
2. **Proc macros** - `procMacro.enable` (default true): the dylibs built in
   step 1 are loaded into `rust-analyzer-proc-macro-srv` and executed on
   every expansion. Observed: r_base `MARK_procmacro`. Without step 1 there
   is no dylib to load, so 1 off implies 2 inert; both are set off
   explicitly anyway.
3. **`.cargo/config.toml` `build.rustc-wrapper` / `rustc-workspace-wrapper`
   / `build.rustc`** - a path the project names (it may be relative to the
   project). Run by `cargo metadata` / `cargo rustc --print cfg` / RA's own
   `rustc` probes **even with 1 and 2 off**. Observed: r_prop (27 wrapper
   calls), r_prop_buildrustc (45 calls).
4. **`rust-toolchain.toml` `[toolchain] path = …`** - every rustup proxy
   (`rust-analyzer`, `cargo`, `rustc`) run with `cwd` inside the project
   executes that directory's binaries. Observed: r_prop_toolchain (the
   server itself, cargo, rustc), and the plugin's own `rust-analyzer
   --version` probe (`resolve`, `plugins/rust/src/semantic.rs:131`) and
   `rustup which` fallback (`rustup_which`, `:178`), which inherit the
   daemon's `cwd` - the session directory, usually the project. A
   `channel = …` naming a toolchain not installed makes rustup download and
   install it (network; GM-512).
5. **Not reached in 1.97.1**: a project `rust-analyzer.toml` could not turn
   build scripts back on or set `overrideCommand` (r_prop_ratoml_*). This
   is an observation about one version, so the test in section 4 pins it.
6. `checkOnSave` (`cargo check`, i.e. 1+2 on save) is already off
   (`plugins/rust/plugin.toml:159-160`).

### pyright

1. **The interpreter.** pyright runs `python.pythonPath`, or `python`/`python3`
   from `PATH` when unset, with a `-c` script to learn `sys.path`; two of
   every three calls are not isolated (`-I` absent) and run with
   `cwd = <root>`. The plugin sets `pythonPath` to the project's own
   `.venv/bin/python` (`interpreter`, `plugins/python/src/semantic.rs:377`;
   `set_python_path`, `:396`), so **the project's binary runs**, along with
   its `site` and any `.pth` `import` lines. Observed: p_plugin_today.
2. A project config cannot name the interpreter (`pythonPath` is not a
   pyright config key; p_cfg_pythonpath) and `venvPath`/`venv` is resolved
   by directory scan without running it (p_config_venv).
3. **The server binary.** `npm_candidates` (`plugins/sdk/src/lsp/resolve.rs:126`)
   tries `PATH`, then `<root>/node_modules/.bin/pyright` (probed with
   `--version`, then run as the server), then `npx --yes --package pyright`
   (download + run). A project that ships its own `node_modules/.bin/pyright`
   gets it executed whenever pyright is not on `PATH`. Source-level; no
   experiment needed. The TypeScript plugin shares this function.

### go/packages (no gopls)

1. Nothing from the project tree is executed: Go has no build-time code,
   `go list` never runs `//go:generate`, and `GOFLAGS`/`CC`/`-toolexec`
   come from the environment or the user's `go env` file, never the module.
2. **Toolchain switch.** `go.mod`/`go.work` `go` and `toolchain` lines with
   `GOTOOLCHAIN=auto` (the default) download and run another Go toolchain
   (checksummed against `sum.golang.org`, so an official release, but a
   binary chosen by the project and fetched over the network). Observed:
   g_newer_go, g_toolchain_line.
3. **`gopackagesdriver` on `PATH`** replaces `go list` entirely. Not
   project-controlled unless `PATH` holds a relative entry; observed.
4. **cgo**: `-compiled -export` runs the system `cc` and `pkg-config` with
   project-chosen package names and allow-listed `#cgo` flags (Go's
   CVE-2018-6574 allowlist rejects `-fplugin` and similar). These are the
   user's binaries, not the project's; observed.

## 2. Proposed configuration and what it costs

### rust

`plugins/rust/plugin.toml` `[plugin.semantic.initialization_options]`:

```toml
checkOnSave = false
[plugin.semantic.initialization_options.cargo.buildScripts]
enable = false
[plugin.semantic.initialization_options.procMacro]
enable = false
```

`plugins/rust/src/semantic.rs::engine` adds to `config.env` (it already
merges into the server's inherited environment, `SemanticConfig::env`):

| Variable | Value | Stops |
|---|---|---|
| `RUSTUP_TOOLCHAIN` | the toolchain from `rustup default` (or the daemon's own `RUSTUP_TOOLCHAIN` if set) | `rust-toolchain.toml` (`path` and `channel`), auto-install |
| `RUSTC`, `CARGO_BUILD_RUSTC` | `rustup which --toolchain <pin> rustc` | `build.rustc` |
| `CARGO` | `rustup which --toolchain <pin> cargo` | RA finding cargo via the proxy |
| `RUSTC_WRAPPER`, `CARGO_BUILD_RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, `CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER` | empty | wrappers |

and resolves the server itself as `rustup which --toolchain <pin>
rust-analyzer` (an absolute path, not the proxy), with every `rustup` call
run with `current_dir` set to a neutral directory (the plugin binary's
directory). A non-rustup install (no `rustup` on `PATH`) keeps today's `PATH`
lookup and still gets the wrapper/`RUSTC` overrides from `which rustc`.

Costs:

- **Proc-macro output unresolved.** Third-party derives (`serde`,
  `thiserror`, `clap`, `schemars`) lose their generated `impl`s, so
  `find_implementations` on those traits and calls through them lose edges;
  function-like macros (`sqlx::query!`) yield nothing. Built-in derives
  (`Clone`, `Debug`, `PartialEq`, …) are RA-internal and unaffected.
  Attribute macros (`#[tokio::main]`, `#[async_trait]`, `rmcp`'s
  `#[tool_router]`/`#[tool]`) are analysed unexpanded: the body still
  resolves, whatever the macro generates around it does not. **Needs the
  S2 A/B.**
- **Build-script output unresolved.** `include!(concat!(env!("OUT_DIR"), …))`
  (prost/tonic/bindgen) is empty, and `cargo:rustc-cfg` cfgs are unset, so
  code behind them is inactive. **Part of the S2 A/B.**
- **Pinned toolchain.** A project pinned to nightly or an old MSRV is
  analysed with the default toolchain's std. Small, edge-level; not
  measured (D2).
- Faster start and no `target/` (r_prop 16 s vs r_base 30 s real at load
  27-38; not a measurement).

### python

- Stop sending the venv interpreter. Instead send
  `python.analysis.extraPaths = [<venv>/lib/python3.*/site-packages]`
  (Windows: `<venv>/Lib/site-packages`), found by directory scan of the same
  `VENV_DIRS`, and set `python.pythonPath` to a sentinel that is not an
  interpreter, so pyright runs none. `/usr/bin/false` is observed to work on
  Unix; the implementation must pick a cross-platform sentinel (a path that
  does not exist, or the plugin's own binary) and **verify it with the
  `PATH`-shim test below**, since an absent path may make pyright fall back
  to `PATH`.
- Drop the `<root>/node_modules/.bin` origin from the python plugin's
  candidates (D4).

Costs: `.pth` editable installs still resolve (p_pth_extrapaths). Lost: what
only a running interpreter knows - `.pth` files with `import` lines that
compute paths, `sitecustomize` path changes, conda/pyenv layouts that are
not `lib/python3.*/site-packages`, and the interpreter's version (pyright
falls back to its default `pythonVersion`). **S2 A/B, small.**

### go

In `semanticEngine.load` (`plugins/go/semantic.go:311`):
`cfg.Env = append(os.Environ(), "GOTOOLCHAIN=local", "GOPACKAGESDRIVER=off")`,
plus `CGO_ENABLED=0` if D5 says so.

Costs: `GOPACKAGESDRIVER=off` none (Bazel users with a driver lose it, and
the plugin never supported one). `GOTOOLCHAIN=local`: a module whose `go`
line is newer than the installed Go does not load at all (g_newer_go_local)
and degrades through the existing `degrade` path; a `toolchain` line alone
is ignored. `CGO_ENABLED=0`: cgo files drop out (g_nocgo, `Seven` missing).
No A/B needed: these are binary, observed per fixture.

### S2 A/B (sonnet measurement slice), only if D1 = (a) or (b)

- Corpus: g-mesh itself at this branch's base (serde, thiserror, clap,
  schemars, tokio-macros, async-trait, tracing, rmcp macros), plus one
  build-script-codegen crate (a small prost/tonic example vendored as a
  fixture), REPS=low is fine for counts.
- Arms: A = `BASE` (today), B = `PROP` + env pin, same rust-analyzer 1.97.1.
- Metric: the rust semantic pass's resolved edges by kind (CALLS,
  REFERENCES, SUPERTYPE_OF) and the count of receiver-call sites left
  unresolved, from the index after a full semantic pass; secondary: pass
  wall time, `target/` size, `uptime` and `/usr/bin/time -p` for each.
- Cheap observation where the arms must differ (else the A/B measured
  nothing): `find_implementations serde::Serialize` count on g-mesh (A > 0
  derive impls, B none of them), and the `rfx` fixture's `generated()`
  definition (A resolves, B `[]`, already seen above).
- Python (optional, small): conformance fixture plus `pfx`, today vs
  sentinel+`extraPaths`; metric = resolved definitions; differing
  observation = none expected beyond interpreter-only paths, so a null
  result is the expected answer and the control is p_plugin_today vs
  p_no_pythonpath (`vfunc` resolves only with a venv).

## 3. D3 from GM-524: where rust-analyzer builds

With `PROP` rust-analyzer builds nothing: `target/` was absent in every PROP
arm and `cargo` was only asked for `metadata`, `locate-project`,
`config get` and `rustc --print cfg` (evil-cargo log). So D3 closes as
GM-524's option (a): **no `cargo.targetDir`**, no state-dir plumbing.

Only if the owner keeps build scripts/proc macros on (D1 (b) opted-in, or
(c)) does D3(b) come back: core passes the project's state dir to the
plugin at spawn (`PluginState::spawn`, `core/src/daemon/plugin.rs:606-624`,
env `G_MESH_STATE_DIR = storage::connection::project_dir(root)`), and
`engine` sets `initialization_options.cargo.targetDir =
<state>/rust-analyzer-target`. That build is a second full compile-time-deps
build per worktree, deleted with the state dir (GM-524 D1).

## 4. Tests

Each mirrors the deleted TS test (`plugins/typescript/test/security.test.ts`,
removed with the npm package in 874cc75): a hostile fixture, a sanity check
that the server really answered and the hostile config was in force, and an
assertion that no marker exists. Shims are shell scripts, so the shim parts
are `#[cfg(unix)]` / `runtime.GOOS != "windows"`; the build.rs/proc-macro
part is cross-platform.

| Plugin | Test | Fixture | Sanity | Control (revert the code, the test must fail) | CI |
|---|---|---|---|---|---|
| rust | `plugins/rust/tests/semantic_runs_no_project_code.rs`, driving the real engine like `conformance.rs::semantic()` | `rfx` + `.cargo/config.toml` (`build.rustc`, both wrappers → fixture shims) + `rust-toolchain.toml` `path` → fixture `evil_tc` + `rust-analyzer.toml` enabling both | a cross-file `definition` in a plain function resolves; running `cargo metadata` in the fixture without the env writes the wrapper log (the hostile config works) | C1: drop `buildScripts`/`procMacro` from plugin.toml → `MARK_buildrs`/`MARK_procmacro`. C2: drop the `RUSTUP_TOOLCHAIN` pin → evil toolchain log. C3: drop wrapper/`RUSTC` env → shim logs | yes: CI installs rust-analyzer via rustup on all 4 runners; shim parts unix only |
| rust | unit, extend `the_shipped_manifest_configures_the_server_this_module_expects` (`semantic.rs:218`) | - | - | covered by C1 | yes |
| python | `plugins/python/tests/semantic_runs_no_project_code.rs` | `pfx` with a real `python3 -m venv` whose `bin/python` is replaced by a marker shim, `PATH` shim `python`/`python3`, `node_modules/.bin/pyright` marker shim, and `pyrightconfig.json` venv config | `vfunc` resolves into the venv site-packages | C4: restore `set_python_path(interpreter(root))` → venv marker. C5: restore the sentinel to unset → `PATH` marker. C6 (if D4 a): restore the local origin with `PATH` lacking pyright → local marker | yes on linux/macOS (pyright from `plugins/python/package.json`); Windows no |
| go | `plugins/go/semantic_project_code_test.go` | `gfx` with `toolchain go1.99.1`, `gopackagesdriver` shim on `PATH` via `t.Setenv`, `GOPROXY=off` | the semantic pass resolves `Plain` | C7: drop `GOTOOLCHAIN=local` → load fails "toolchain not available". C8: drop `GOPACKAGESDRIVER=off` → driver marker. (If D5 b: C9 drop `CGO_ENABLED=0` → `cc` marker) | yes (CI's Go 1.27 job); shim parts non-Windows |

These are rust-analyzer/pyright runs: 5x repeats per the standing rule.

## 5. Edit map

Change:

- `plugins/rust/plugin.toml:129-160` - options above; rewrite the comment at
  `:154-158` that argues for keeping `procMacro`/`buildScripts` on.
- `plugins/rust/src/semantic.rs` - `engine` (`:80-101`): pin + env;
  `resolve` (`:131-157`), `candidates` (`:158-170`), `rustup_which`
  (`:178-185`): `--toolchain <pin>`, neutral `current_dir`; new `pin()`;
  test `:218-252`.
- `plugins/python/src/semantic.rs` - `add_project_settings` (`:297-317`),
  `interpreter` (`:377-388`) → `site_packages`, `set_python_path`
  (`:396-400`) → `set_extra_paths` + sentinel; `candidates` (`:335`); tests
  around `:580-640` (`an_interpreter_is_found_under_the_root_and_nowhere_else`,
  `the_interpreter_is_merged_into_the_manifests_own_python_section`);
  module doc Decision 1 (`:38-60`).
- `plugins/python/plugin.toml:106-119, 190-198` - comments on the command
  search and on `pythonPath`.
- `plugins/sdk/src/lsp/resolve.rs:117-160` `npm_candidates` - a flag (or a
  second function) to omit the project-local origin, so TS is unchanged
  until its own task (D4).
- `plugins/sdk/src/lsp/config.rs:39-56` - module doc "additions, not an
  allowlist": now also the security overrides.
- `plugins/go/semantic.go:311-330` `load` - `cfg.Env`; doc at `:37-55`.
- D3(b) only: `core/src/daemon/plugin.rs:606-624`.
- New ADR in `docs/adr/` (index in `docs/adr/README.md`): "semantic tiers
  execute no project code", carrying D1-D6 and the trade-offs. Per the
  project's comment rule the manifests and modules state only the invariant
  ("this server runs no build script, proc macro or project toolchain") and
  link the ADR once; the rejected alternatives and this task's history do
  not go into comments.

Read for context: `plugins/sdk/src/lsp/client.rs:222-240` (`LspClient::start`,
`cwd = root`, `envs(config.env)`), `plugins/sdk/src/lsp/bridge.rs:561`
(`LspBridge::new`), `plugins/sdk/src/lsp/resolve.rs:169` (`probe`),
`plugins/rust/tests/conformance.rs:238-308` (`base`, `semantic`,
`rust_analyzer`), `plugins/python/tests/conformance.rs:221-337`,
`plugins/go/semantic_test.go:192` and `:455`, `.github/workflows/ci.yml:481-560, 628-651`.

Callers relied on (g-mesh, this worktree): `find_references npm_candidates`
→ `plugins/python/src/semantic.rs::candidates`,
`plugins/typescript/src/semantic.rs::candidates` (so D4 touches TS unless
flagged); `find_references semantic::interpreter` → only
`add_project_settings`; `find_callers semanticEngine.load` → only
`loadFor`. `PluginState::spawn`'s `cwd` (inherited) and
`LspClient::start`'s `cwd(root)` were read directly (one known file each).

## 6. Owner decisions

**D1. rust-analyzer build scripts and proc macros.**
Today: on (`plugins/rust/plugin.toml:154-158` keeps them on deliberately for
edge coverage). Opening any cloned repo runs its `build.rs` and proc macros
inside the daemon, and writes `<root>/target`. Change: turn both off.
Example: a crate with `#[derive(Serialize)] struct Config` - today
`find_implementations Serialize` lists `Config`; after, it does not, while
`Config`'s own fields and methods still resolve.
- (a) **Off, no opt-in; S2 measures the loss for the record (Recommended).**
  Benefit: the guarantee holds for every project; no `target/`, D3 closed.
  Risk: derive-generated impls and `OUT_DIR` code go missing (size unknown
  until S2).
- (b) Off by default, opt-in per project from the *user's* g-mesh config
  (never a project file). Benefit: trusted repos keep full edges. Risk:
  config plumbing, D3(b) state-dir build for opted-in projects, a second
  code path to test.
- (c) Keep on, document the risk. Benefit: no edge loss. Risk: the task's
  goal is not met.

**D2. Which Rust toolchain the semantic tier uses.**
Today: whatever the rustup proxy picks from `cwd`, so a project's
`rust-toolchain.toml` can name a directory of its own binaries (observed: its
`rust-analyzer` ran as our server) or a toolchain rustup then downloads.
Change: pin to `rustup default`. Example: a nightly-pinned project is
analysed with stable std.
- (a) **Pin to the user's default toolchain, ignore `rust-toolchain.toml`
  (Recommended).** Benefit: no project binary, no install, simple. Risk:
  small edge drift on projects that need nightly/old std.
- (b) Honour `channel` only when that toolchain is already installed;
  refuse `path` and installs. Benefit: closer to the project's toolchain.
  Risk: parsing both rustup file formats, more tests, a toolchain that lacks
  the `rust-analyzer` component still needs a fallback.

**D3. Which Python interpreter pyright may run.**
Today: the plugin hands pyright `<root>/.venv/bin/python`, which runs
(observed, non-isolated, `cwd` = project). Change: run none; pass the venv's
site-packages as `extraPaths`. Example: `requests` in `.venv` still
resolves; an editable `-e .` install still resolves (`.pth` read); a conda
env outside `lib/python3.*/site-packages` does not.
- (a) **No interpreter at all; scanned `extraPaths` + a sentinel
  `pythonPath` (Recommended).** Benefit: nothing executes. Risk:
  interpreter-only paths and the exact Python version are lost.
- (b) Run only the user's `PATH` interpreter (drop `pythonPath`) plus
  `extraPaths`. Benefit: the user's global site-packages resolve. Risk: a
  user binary still runs non-isolated with `cwd` = project.
- (c) Keep today's venv interpreter. Risk: project binary runs.

**D4. Server binaries from the project's `node_modules/.bin`.**
Today: if `pyright` is not on `PATH`, the plugin runs
`<root>/node_modules/.bin/pyright --version` and then serves with it; the TS
plugin does the same for its server through the same SDK function. Change:
python stops looking there. Example: a user who installed pyright only in
the indexed project gets "pyright not found, install it with npm -g" in the
log and a structural-only Python tier.
- (a) **Drop it for python now; open a follow-up task for TypeScript
  (Recommended).** Benefit: closes the python hole without touching TS in
  this task. Risk: TS keeps the hole until its task; project-local-only
  pyright users lose the semantic tier.
- (b) Drop it for both python and TS here. Benefit: one fix. Risk: scope
  grows into a plugin this task did not analyse (vtsls tsconfig plugins are
  also unpinned since 874cc75).
- (c) Keep it. Risk: a project ships the binary that runs.

**D5. Go cgo.**
Today: cgo files make `go list` run the user's `cc` and `pkg-config` with
package names the project chooses. They are not project binaries. Change
option: `CGO_ENABLED=0`. Example: `func Seven()` in a cgo file disappears
from the semantic tier (structural extraction still sees it).
- (a) **Keep cgo on (Recommended).** Benefit: cgo files resolve. Risk:
  system tools run with project-influenced argv (flags allow-listed by Go).
- (b) `CGO_ENABLED=0`. Benefit: `go list` runs no external tool. Risk:
  semantic edges in cgo files are lost.

**D6. Go toolchain switching.**
Today: a `go.mod` asking for a newer Go makes `go list` download and run
that toolchain. Change: `GOTOOLCHAIN=local`. Example: a module with
`go 1.28` on a machine with Go 1.27 fails to load and its semantic pass
degrades (logged); a `toolchain go1.28` line alone is ignored and it loads.
- (a) **`GOTOOLCHAIN=local` (Recommended).** Benefit: no download, no
  foreign toolchain run (also GM-512's no-network rule). Risk: such modules
  lose the semantic tier until the user upgrades Go.
- (b) Leave `auto`. Benefit: they load. Risk: network + a binary chosen by
  the project's `go.mod`.

## 7. What this implies for GM-512 (not designed here)

- Rust: with D1 (a), RA writes no `target/` and (1.97.1) no `Cargo.lock`
  under the root; GM-512 should still pin both with a test. `cargo metadata`
  can still fetch uncached dependencies (network) - GM-512's to decide
  (`--offline` through RA's `cargo.extraArgs`/`CARGO_NET_OFFLINE`). The
  `RUSTUP_TOOLCHAIN` pin already removes rustup auto-installs.
- Both tasks edit `plugins/rust/src/semantic.rs::engine`; GM-512 should add
  its env (e.g. `CARGO_NET_OFFLINE=true`) to the same env map this task
  introduces, not a second mechanism.
- Python: with D3 (a) pyright runs no interpreter; the `npx --yes` fallback
  in `npm_candidates` is a network download and stays GM-512's.
  *Amended by the verify (finding 1):* run with the project as cwd, `npx`
  executes the project's own `node_modules/pyright`, so it is a D4 channel
  too; python drops `npx` here (GM-512's D2 decision, "Убрать npx для
  python, TS отдельно"), resolving `PATH` only (`npm_candidates_on_path`).
- Go: `GOTOOLCHAIN=local` removes toolchain downloads; module downloads
  (`GOPROXY`, `GOFLAGS=-mod=…`) remain GM-512's. `go list -export` writes
  `GOCACHE` (outside the root).
- Out of scope, worth a task: the TS plugin's "never runs project code"
  test no longer exists (deleted in 874cc75 with the npm package); vtsls
  and the project-local server origin are unpinned.
