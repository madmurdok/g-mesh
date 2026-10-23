# GM-645: the installed MCP copy of g-mesh discovers no language plugins

Status: design/diagnosis (GM-645/S1). Nothing in the repo or in the install
was changed while writing this.

## 1. Cause

### How discovery works today (unchanged since at least v4.15.0)

- `core/src/daemon/manifest.rs:58` `default_roots()` -> `roots_with_override`
  (l.65-76) returns, in precedence order:
  1. `~/.g-mesh/plugins/` (user root),
  2. `bundled_roots()` (l.82-89): `installed_bundled_root()` (l.93-96) =
     `<dir of current_exe>/plugins/`, then
  3. `PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins")` (l.87): the
     **compile-time** path of the checkout the binary was built in.
  `G_MESH_PLUGIN_ROOTS_OVERRIDE` replaces all three (tests only).
- `discover(roots)` (l.681-742) scans `<root>/<dir>/plugin.toml` only. A
  directory without `plugin.toml` is skipped; loose binaries are never looked
  at. An earlier root shadows a later one per language.
- `${G_MESH_BIN_DIR}` (l.27) is expanded only inside a manifest's `command`
  (`resolve_command`, l.762-781) to `current_bin_dir()` (l.31). It locates a
  plugin **binary** once a manifest has been found; it never locates manifests.
- Callers of `default_roots`/`discover` (grep; see "g-mesh" below): daemon
  start `daemon/mod.rs:334`, `daemon/plugin.rs:373`, `cli/init.rs:184`,
  `cli/reindex.rs:101`, `cli/status.rs:383`, `cli/plugin_install.rs:482`
  (shadow warning). `plugins list` has its own copy of the root list,
  `cli/plugins.rs:160` `default_roots()` (user root + `manifest::bundled_roots`,
  ignores the override env).
- Who writes plugins: only `g-mesh plugins install` (`cli/plugin_install.rs`),
  into `install_root_from` (l.109-115) = `installed_bundled_root()`, i.e.
  `<exe dir>/plugins/<lang>/`. **Nothing writes `~/.g-mesh/plugins/`**; it is
  a user override root only. `scripts/install.sh` installs the whole release
  archive (`g-mesh` + `plugins/<lang>/{plugin.toml, g-mesh-plugin-<lang>}`).
- Release manifests are not the checkout ones: `scripts/bundle-plugin.sh`
  (l.140-164, and the rust/python/go bundlers) rewrites `command` from
  `${G_MESH_BIN_DIR}/g-mesh-plugin-<lang>` to `./g-mesh-plugin-<lang>`, the
  binary sitting inside the plugin's own directory.

### Why the installed copy finds nothing

The memory-note procedure copies four bare binaries flat into
`~/.local/share/g-mesh/bin/`. That layout has no `plugin.toml` anywhere it
looks:

| root | for the installed 4.15.0 binary |
|---|---|
| `~/.g-mesh/plugins/` | does not exist |
| `~/.local/share/g-mesh/bin/plugins/` | does not exist (binaries are flat beside `g-mesh`) |
| `CARGO_MANIFEST_DIR/../plugins` | the **CI runner's** checkout (the 4.15.0 copy came from the published release archive, built on `/Users/runner/...`), absent here |

The note's claim "manifests still come from the main checkout's `plugins/`"
was only ever true for a **locally built** core, whose root 3 is baked as this
checkout. The first refresh (2026-10-11, local build) therefore worked by
accident; the 2026-10-21 refresh from the release archive (memory note's
"alternative") lost root 3 and with it every plugin. GM-586 (self-versioned
plugins) is not involved: it changed version bookkeeping only, not discovery.

### Evidence (commands run, output abridged)

```
$ ls ~/.local/share/g-mesh/bin
g-mesh g-mesh-plugin-python g-mesh-plugin-rust g-mesh-plugin-typescript   # no plugins/
$ ~/.local/share/g-mesh/bin/g-mesh --version          -> g-mesh 4.15.0
$ ~/.local/share/g-mesh/bin/g-mesh plugins list       -> (empty, rc 0; from repo cwd and from scratchpad cwd)
$ ls ~/.g-mesh                                         -> config.toml embedding-cache models orphan-sweep.stamp projects  (no plugins/)
$ strings g-mesh | grep runner                         -> /Users/runner/.cargo/registry/... (CI build)
$ <repo>/target/release/g-mesh plugins list   (local 4.12.0 build, cwd=scratchpad)
go 0.5.0 bundled ... / python 4.17.0 bundled ... / typescript 4.17.0 bundled ... / rust 4.17.0 bundled ...
   # same code, found via its baked checkout root; note the 4.12.0 core reading 4.17.0 manifests
~/.claude.json mcpServers.g-mesh = {"command": "/Users/Valentin_Taiurskii/.local/share/g-mesh/bin/g-mesh", "args": ["mcp-shim"], "env": {}}
select_project g-mesh -> "Nothing is indexed here ... Not indexed, no plugin installed: go (63 files; `g-mesh plugins install go`), python (56), rust (406), typescript (67)"
```

g-mesh MCP (as the brief asked, one try): `select_project g-mesh` -> the
message above; `find_callers default_roots` -> "no symbol named
'default_roots' found". All cross-file answers above therefore come from grep.

Side findings:
- About 30 `g-mesh daemon` processes of the installed 4.15.0 binary are
  running (since Oct 21-22), most for scratchpad roots. Each discovered its
  plugin set at start (`daemon/mod.rs:334`), so they keep "no plugins" after a
  fixed install until stopped.
- The help text `PluginsCommand::List` (`cli/mod.rs:227`) says "installed under
  `~/.g-mesh/plugins/`", which is wrong for every install path that exists.
- `install.sh`'s pre-install check (l.~702) greps `plugins list` for
  `typescript` only, and on a dev machine a locally built core passes it via
  the baked checkout root even if the archive's plugins were broken.
- Machine: Intel Mac (`file` -> Mach-O x86_64), target `x86_64-apple-darwin`.

## 2. Correct install steps (working MCP install)

Required layout, the release-archive layout:

```
~/.local/share/g-mesh/bin/
  g-mesh
  plugins/<lang>/plugin.toml            # command = "./g-mesh-plugin-<lang>"
  plugins/<lang>/g-mesh-plugin-<lang>   # for lang in typescript go rust python
```

LSP deps: **none needed to index**. `node_modules` under `plugins/typescript`
and `plugins/python` only pin test deps (`@vtsls/language-server`, `pyright`;
GM-586 note confirms). The semantic tiers look up `vtsls`,
`pyright-langserver`, `rust-analyzer` on `PATH` only (ADR 0032; never a
plugin's or project's `node_modules`). Here only `rust-analyzer` is on PATH;
vtsls/pyright would need `npm i -g` if the TS/Python semantic tier is wanted.
The Go plugin is a static binary needing no Go toolchain.

A. From a published release (network):
```
sh scripts/install.sh --install-dir ~/.local/share/g-mesh/bin --no-modify-path --version X.Y.Z
```
install.sh stages, checks the sha256, runs `--version` and `plugins list`,
then atomically swaps the directory (`mv`), never touching `~/.g-mesh`.
`--install-dir` and `--no-modify-path` are mandatory here (default dir is
`~/.g-mesh/bin`, next to `~/.g-mesh/projects`; default edits `~/.zshrc`).

B. From a local build (no publish needed):
```
scripts/build-targets.sh x86_64-apple-darwin      # builds core + 4 plugins, dist/g-mesh-vX-<t>.tar.gz(+.sha256)
serve=<scratch>/serve; mkdir -p $serve/vX; cp dist/g-mesh-vX-<t>.tar.gz{,.sha256} $serve/vX/
G_MESH_DOWNLOAD_BASE=file://$serve sh scripts/install.sh --install-dir ~/.local/share/g-mesh/bin \
    --no-modify-path --version X --target x86_64-apple-darwin
```
(`file://` serving is what `scripts/test-install.sh` l.85-115 already does.)

C. Repair of the current copy without re-downloading: `plugins install
<lang>` for each language (writes `<exe dir>/plugins/<lang>` from the v4.15.0
GitHub assets), or `plugins install --from <dir-or-archive>`.

After any of them: stop the running daemons of that binary (`g-mesh stop` in
each project, or end them), then `/mcp` reconnect. Then
`~/.local/share/g-mesh/bin/g-mesh plugins list` must print 4 lines and
`select_project g-mesh` must report the project indexed.

## 3. Options for a refresh that cannot produce an undiscoverable install

1. **`scripts/refresh-mcp-install.sh` (new), delegating to install.sh.**
   `--install-dir DIR` required; `--version X` (published) or `--local`
   (build-targets.sh host target + `file://` serve as in 2B). Always passes
   `--no-modify-path`; refuses a DIR equal to or under `~/.g-mesh`. After the
   swap it verifies the effect, not the return code: for each language from
   `build-targets.sh --plugins`, `DIR/plugins/<lang>/plugin.toml` exists, its
   `command` resolves to an executable inside that directory, and
   `DIR/g-mesh plugins list` lists it **from DIR** (needs option 2). It then
   lists running daemons of `DIR/g-mesh` and stops them (or prints the
   command, see decision below).
   Benefit: one command, one install code path (install.sh), checksum + atomic
   swap reused, works for both sources. Risk: `--local` costs a full
   `--target` release build (separate `target/<triple>/`, minutes, disk);
   another script to keep in sync with build-targets/install.sh.
2. **`plugins list` shows where each plugin was found** (manifest dir), and
   the `List` help text is corrected.
   Benefit: makes the false positive visible (a local core finding manifests
   in the checkout instead of beside itself), lets scripts assert the root.
   Risk: output format change; tests and any parser of `plugins list`
   (install.sh greps the language name only; `release-smoke.sh`) must be
   checked.
3. **Strengthen install.sh's pre-install check**: every `plugins/<lang>/` in
   the staged archive must be listed by `plugins list`, and (with option 2)
   from the staged directory. Benefit: closes the same hole for every user,
   not just the MCP copy. Risk: POSIX-sh only; `test-install.sh` fixtures fake
   a `g-mesh` that prints a `plugins list` - they need updating.
4. **Discovery fallback in code**: when no manifest is found for a language
   but `g-mesh-plugin-<lang>` sits beside the exe, synthesize a manifest from
   one embedded at build time (`include_str!` of `plugins/<lang>/plugin.toml`).
   Benefit: the flat copy would just work. Risk: a second manifest source
   that can drift from the shipped one, contradicts "manifest is the
   contract" (`plugin.toml` is the only discovery input today), larger change
   to discovery and to `plugins list`/`status`. Not recommended.
5. **Diagnostic only**: when discovery finds nothing but bare
   `g-mesh-plugin-*` binaries sit beside the exe, the "no plugin installed"
   instruction/CLI hint says "found g-mesh-plugin-rust beside the binary but
   no plugins/rust/plugin.toml - install with `plugins install` or install.sh".
   Benefit: cheap, self-explaining failure. Risk: small; one more branch in
   `mcp/instructions.rs` coverage text (ADR 0022 wording tests).
6. **Doc/memory only**: replace the memory note with the steps of section 2.
   Benefit: zero code. Risk: relies on being followed; that is what failed.

**Recommendation: 1 + 2 (+3 if cheap)**, and replace the memory note's
procedure with "run `scripts/refresh-mcp-install.sh`". Option 2 is what makes
the script's verification able to tell a correct install from the
accident-of-a-local-build that hid this problem the first time. Option 5 is a
reasonable follow-up, not required for the acceptance criteria.

## 4. Edit map (recommended option)

| file | change |
|---|---|
| `scripts/refresh-mcp-install.sh` (new, bash, `set -euo pipefail`) | args `--install-dir`, `--version X` \| `--local`, `--stop-daemons`; refuse DIR under `$HOME/.g-mesh`; `--local`: `build-targets.sh <host>` (host via `build-targets.sh`'s `host_triple`, or `rustc -vV`), stage `<scratch>/serve/v<ver>/`; call `sh scripts/install.sh --install-dir DIR --no-modify-path --version V [--target T]` with `G_MESH_DOWNLOAD_BASE=file://...` for local; post-verify per language (section 3.1); daemon handling. |
| `core/src/cli/plugins.rs` | `PluginInfo` (l.118-124): add `dir: PathBuf`; `scan_root` (l.220-262): fill it for Loaded and Error; `render` (l.264-298): append the directory (e.g. trailing `dir=<path>`); unit tests l.384-680 that compare rendered strings. |
| `core/src/cli/mod.rs:227` | `PluginsCommand::List` doc: "List the language plugins g-mesh discovers: `~/.g-mesh/plugins/`, `plugins/` beside this binary, then the checkout it was built from". |
| `scripts/install.sh` l.~686-705 (optional, option 3) | loop over `"$_stage"/plugins/*/plugin.toml`, require each language in `"$_stage/g-mesh" plugins list` output, and (with option 2) its `dir=` under `$_stage/plugins`. |
| `scripts/test-install.sh` | fake `plugins list` output to match (only if option 3). |
| `README.md` (Install section ~l.47-140) | a short "Pointing an MCP client at a fixed install directory" paragraph: use the script; never copy bare binaries. |
| memory note `gmesh-mcp-binary-installed-copy.md` (outside repo, orchestrator) | replace procedure. |

Read for context: `manifest.rs` l.27-96 and `discover` l.681-742;
`cli/plugins.rs` `default_roots` l.160-166, `list_from_roots` l.196-218;
`cli/plugin_install.rs` `install_root_from` l.109-115, `place` l.436-479;
`scripts/install.sh` `main` l.584-794; `scripts/build-targets.sh` `build_one`
l.265-386 and its query flags; `scripts/test-install.sh` l.80-120;
`scripts/release-smoke.sh` l.~250-260 (parses `plugins list`?).

## 5. Behaviours the tests slice must pin

- `plugins list` prints each plugin's manifest directory; a plugin under `<exe>/plugins/<lang>` shows that path, a user-root plugin shows `~/.g-mesh/plugins/<lang>`.
- A flat layout (bare `g-mesh-plugin-*` beside the exe, no `plugins/`) with an unreachable checkout root lists nothing (documents the failure mode; existing `a_root_that_does_not_exist_contributes_nothing` covers half).
- Script: refuses without `--install-dir`, refuses a dir under `$HOME/.g-mesh`, never writes rc files (HOME sandbox, as test-install.sh does).
- Script `--local` against a fixture archive (file:// serve) produces `DIR/plugins/<lang>/{plugin.toml,binary}` for all of `build-targets.sh --plugins` and exits 0.
- Script fails (non-zero, names the language) when the installed result lacks one language's `plugin.toml` (control: remove one from the fixture archive).
- (option 3) install.sh refuses an archive whose staged `plugins list` misses any staged language, not only typescript.

Controls: revert the `dir` field in `render` -> the path test fails; drop the
per-language check in the script -> the missing-language test passes wrongly.

### Must confirm (implementation)

- Does `mcp-shim` reuse an already running daemon of an older/same-path binary after the swap (likely yes -> daemons must be stopped; decide auto-stop vs print)? `g-mesh stop` is per current project; ~30 daemons run for scratchpad roots.
- After restart with plugins, does the existing `~/.g-mesh/projects/<hash>` state of g-mesh (recorded "absent" languages) re-index automatically, or does it need `g-mesh reindex`? Never `g-mesh clean` / delete `~/.g-mesh/projects`.
- `install.sh` moves the whole `--install-dir` aside and back (`mv`); confirm it is fine while `mcp-shim`/daemons run from it (macOS keeps the open inode) and that nothing else lives in `~/.local/share/g-mesh/bin`.
- `build-targets.sh x86_64-apple-darwin` on this machine: cost, and that `bundle-go-plugin.sh` works with the local Go toolchain.
- Whether `release-smoke.sh` or any test parses `plugins list` lines positionally (format change in option 2).
- Acceptance re-check after implementation: installed `plugins list` lists rust, python, typescript, go; `select_project g-mesh` indexed; `find_references` returns rows.

### Decision needed from the owner

- Daemon handling in the script: auto-stop daemons of the installed binary (recommended; risk: interrupts other sessions' MCP until reconnect) vs print the list and the command.
- Default source for the refresh: published release (`--version`, recommended for released versions; needs network, only after CI publishes) vs local build (`--local`, works before publishing; slow full target build).
