# GM-586: self-versioned Rust-workspace plugins

Owner decision: «Свои версии, общий релиз (Recommended)». One release, one
archive, one tag; `plugins/rust`, `plugins/python`, `plugins/typescript` each
carry their own version, bumped only when what they ship changed.

## Today

- `scripts/cut-release.sh` `check_workspace_versions` (l.264) requires every
  manifest in `OTHER_WORKSPACE_MANIFESTS` (l.237: wire, walk, sdk, rust,
  python, typescript) to equal `core/Cargo.toml`'s version.
- `check_crate_backed_plugin_versions` (l.314) requires each plugin.toml
  `plugin_version` to equal the release version.
- So every release bumps 7 `Cargo.toml` + 3 `plugin.toml` (commit 3b6665b),
  and `plugin_version` says "4.16.0" for a plugin that did not change.
- `check_self_versioned_plugin_versions` (l.342) checks only that
  plugins/go's two copies of its version agree, never a change rule.

## Facts this note relies on

| Fact | Source |
|---|---|
| g-mesh had no answers here: `select_project g-mesh` says "No language plugin is installed: g-mesh has no answers about this project's files"; `find_references plugin_version` returned "no symbol named 'plugin_version' found". Everything below comes from grep. | g-mesh calls, then `grep -rn` |
| Plugins depend on `plugins/sdk` and `wire` (path deps); sdk depends on `wire` and `walk`; no plugin depends on `core` | `grep "path =" plugins/*/Cargo.toml` |
| Core reads `plugin_version` only to show it: `plugins list` (`cli/plugins.rs:243`), `status` (`cli/status.rs:282,393`, `status/json.rs:156`), the protocol-mismatch message (`protocol/handshake.rs:16`), the `plugins install` "replacing X" line (`cli/plugin_install.rs:439-472`). It compares only `protocol_version`. | grep `plugin_version` in `core/src` |
| The index fingerprint (`registry::indexer_version`, `daemon/plugin.rs::fingerprint`) hashes the plugin build's content, not its version | `core/src/daemon/plugin.rs:345` |
| Plugin release assets are named after **core's** version: `g-mesh-plugin-<lang>-v<core>-<target>.tar.gz`, fetched from tag `v<core>` (`cli/plugin_install.rs:91,144-158`). This does not change: the asset name says which release it came from, and the `plugin_version` inside it says which plugin build. | grep `CARGO_PKG_VERSION` |
| Each plugin reports its own `CARGO_PKG_VERSION` in the handshake (`plugins/*/src/main.rs` `PluginSpec::new`), and `the_manifest_version_matches_the_crates` (rust `semantic.rs:1071`, python `semantic.rs:901`, typescript `tests/units.rs:110`) pins plugin.toml == crate version. Both stay valid. | grep `CARGO_PKG_VERSION` |
| Tests that assume plugin == release: only `core/tests/scripts/cut_release_plugin_versions.rs` (`RELEASE = env!("CARGO_PKG_VERSION")`). `symlink_table_routing.rs:64` writes core's version into the **toy** plugin (sdk crate), so it is fine while sdk keeps the release version. `plugin_install.rs:26` uses core's version for asset URLs, which do not change. | grep `CARGO_PKG_VERSION` |
| `test_local_script.rs:833` stubs `check_crate_backed_plugin_versions` **by name** | grep |
| What a plugin ships: `src/`, `Cargo.toml`, `plugin.toml` (bundlers copy the binary plus a rewritten plugin.toml). There is no `build.rs`. `package.json` in python/typescript only pins test deps (pyright), `conformance/` and `tests/` are fixtures. `include_str!` reaches only `src/` and `plugin.toml`. | `ls`, grep `include_str!`, `plugins/python/package.json` |

## The change

1. **The release bump touches core, wire, walk, sdk only** (4 `Cargo.toml` +
   their `Cargo.lock` lines). `check_workspace_versions` keeps its equality
   rule over wire/walk/sdk; the three plugin `Cargo.toml` leave
   `OTHER_WORKSPACE_MANIFESTS`.
2. **Each plugin's crate version = its `plugin_version`**, starting from
   today's 4.16.0, bumped only when the plugin changed (rule below).
3. `check_crate_backed_plugin_versions` (keeps its name and its
   `"$crate_version"` argument, so `main()`'s call site and the
   test_local_script stub stay as they are) checks per plugin:
   - **(a) internal agreement:** plugin.toml `plugin_version` == the plugin's
     `Cargo.toml` `[package] version`. This repeats the unit test, so it also
     holds under `--skip-tests`, the same as the go check.
   - **(b) change rule** against the previous release tag (below).
4. Release archive, asset names, release.yml, install.sh, `plugins install`:
   unchanged.

## Detection rule

**Previous tag** `P` = `git tag --merged HEAD -l 'v[0-9]*' --sort=-v:refname`,
first entry that is not `v$crate_version`. With none, log and skip (b); keep (a).

**Plugin X changed since P** if any of these differ between `P` and the
working tree (the same view the other checks read):

- `plugins/X/src/`, `plugins/sdk/src/`, `wire/src/`, `walk/src/`:
  `git diff --quiet P -- <dirs>`;
- `plugins/X/Cargo.toml`, `plugins/X/plugin.toml`, `plugins/sdk/Cargo.toml`,
  `wire/Cargo.toml`, `walk/Cargo.toml`, each compared **with its own
  `[package] version` / `[plugin] plugin_version` line removed**
  (`git show P:<f>` vs the file, both through one `strip_version` awk). So the
  release bump of sdk/wire/walk, and X's own bump, never count as a change;
- `Cargo.lock`, **restricted to X's external dependency closure**: an awk
  `lock_closure <lockfile> <crate>` walks `[[package]]` `dependencies` from
  `g-mesh-plugin-X` and prints `name version source` for every reached
  package **that has a `source`** (registry/git). Path members (sdk, wire,
  walk, the plugin itself) have no `source`, so their release bumps drop out.
  Changed if `lock_closure(git show P:Cargo.lock)` != `lock_closure(Cargo.lock)`.
  This uses no cargo, so it works in a scratch repo. The lock does not tell
  dev-deps from normal deps, so a dev-dep upgrade also counts (it errs toward
  a bump).

Not counted: `tests/`, `conformance/`, `README.md`, `package*.json`,
`node_modules/`, sdk's `toy/`, `fake/` and `fake-lsp/`, docs, and every
other path.

**Verdict per plugin** (`v_P` = version at P from `git show P:plugins/X/Cargo.toml`, `v` = now):

| changed | `v` vs `v_P` | result |
|---|---|---|
| no | equal | accept: `X unchanged since P, stays v` |
| no | greater | accept with a log line (a deliberate bump) |
| yes | equal | **refuse**: `plugins/X changed since P (<first changed paths>) but its version is still v - bump plugins/X/Cargo.toml and plugin.toml` |
| any | lower | **refuse**: version went backwards |
| X absent at P | any | accept (new plugin) |

Every refusal is collected before dying, the same as today. "Greater" uses
`sort -V`.

**Example (cut 4.17.0, P = v4.16.0):** the bump commit moved core, sdk, wire
and walk to `4.17.0`, which counts as a change for no plugin. Since v4.16.0,
only `plugins/python/src/project/mod.rs` changed. rust and typescript stay at
`4.16.0` and are accepted. python at `4.16.0` is refused until both of its
files say e.g. `4.16.1`. If `Cargo.lock` had also moved `anyhow` (reached by
all three plugins through sdk), all three would count as changed and be refused
until bumped.

## Options (real choices)

1. **sdk/wire/walk versioning.** **A (Recommended):** they keep the release
   version. Benefit: a single, unchanged equality check; sdk/wire are the
   protocol surface and a release names them. Risk: their `Cargo.toml` changes
   every release, so detection must strip the version line, and the lock
   closure must skip path packages (both covered above). **B:** self-version
   them too. Benefit: the release bump touches only core. Risk: two more change
   rules; `walk` is also linked by core, so it would carry two meanings.
2. **What counts as a change.** **A (Recommended):** the shipped inputs listed
   above. Benefit: a fixture or test edit does not force a bump. Risk: a future
   file outside `src/` that the build embeds (`build.rs`, an `include_str!`
   reaching `../assets`) is invisible until it is added to the list. **B:**
   any file under the six directories. Benefit: nothing slips. Risk: almost
   every release touches tests, so in practice it brings back today's
   bump-everything.
3. **Cargo.lock.** **A (Recommended):** the filtered closure. **B:** ignore
   the lock. Risk: a dependency upgrade that changes the shipped binary keeps
   the old number. **C:** any lock change counts. Risk: every core dependency
   bump forces three plugin bumps.
4. **Bump without change.** **Accept, logged (Recommended):** this lets the
   first release (4.16.0, already bumped) pass without reverting the bumps.
   Refusing it would keep the number exact, but 4.16.0 would then need its
   plugin bumps reverted for any plugin unchanged since v4.15.0.

## Edit map

| File | Where | Change |
|---|---|---|
| `scripts/cut-release.sh` | `OTHER_WORKSPACE_MANIFESTS` l.237-245 | drop the 3 plugin `Cargo.toml` |
| same | `CRATE_BACKED_PLUGIN_MANIFESTS` l.~292-299 | keep; turn into plugin dirs (`rust python typescript`) or derive paths from it |
| same | new helpers next to `plugin_toml_version` l.301-311: `previous_release_tag`, `strip_version`, `lock_closure`, `plugin_changed_since <dir> <tag>` (prints the changed paths) | pure git/awk, no cargo |
| same | `check_crate_backed_plugin_versions` l.310-340 (body + its doc comment) | rules (a)+(b), the verdict table, collect then die; same name and argument |
| same | header GM-303 paragraph l.44-79 and refusal list l.189-192 | state the new rule, present tense, no history (CLAUDE.md comment rule). **Touch nothing else in the header:** GM-643 edits this file first (publish step), and its edits are outside the version-check block |
| same | `main()` l.620-625 | **no change** |
| `docs/architecture/plugin-modularity.md` | "`plugin_version`: two rules" l.277-323 | rewrite: workspace plugins carry their own version, the change rule, what counts, that the asset name carries core's version |
| `plugins/{rust,python,typescript}/plugin.toml` | comment above `plugin_version` (rust ~l.25-44, python ~l.25-40, typescript ~l.20-23) | "equals this crate's version; bumped when the plugin changes, cut-release enforces"; drop the "tracks the release" sentence and the history |
| `scripts/bundle-plugin.sh` | comment l.50-52 | "tracks the release" -> "equals this crate's version" (comment only) |
| `README.md` | "Cutting a release" step 2, l.393-397 | which manifests the bump touches (core, wire, walk, sdk), plus "bump a plugin only if it changed" |
| `core/tests/scripts/cut_release_plugin_versions.rs` | whole file | rewrite around a scratch **git** repo (see tests) |

Read for context: `package_version` (l.249), `check_self_versioned_plugin_versions`
(l.342, message and collect pattern), `check_no_untagged_prior_releases` (l.420,
git use under a stubbed `REPO_ROOT`), `test_local_script.rs:815-845` (the stub
list must keep working).

Not edited: core sources (display only), `plugin_install.rs` and its tests,
`symlink_table_routing.rs`, the three `the_manifest_version_matches_the_crates`
tests, release.yml, install.sh, build-targets.sh.

## Behaviours for the tests slice

Scratch repo: `git init`, minimal `plugins/{rust,python,typescript,sdk}`,
`wire`, `walk` (each with `Cargo.toml` and `src/lib.rs`), plugin.toml files, a
hand-written `Cargo.lock` with path members plus one registry package each
plugin reaches. Commit and tag `v1.0.0`, edit, then run
`source cut-release.sh; REPO_ROOT=…; check_crate_backed_plugin_versions 1.1.0`.

1. Only the core/sdk/wire/walk version lines and their path lock entries moved: accepted, all plugins unchanged.
2. `plugins/python/src/x.rs` edited, python version unchanged: refused, names python and the path.
3. Same edit plus python bumped in both files: accepted.
4. `plugins/sdk/src/x.rs` edited, no plugin bumped: refused for all three.
5. `wire/src` and `walk/src` edits: same as 4 (one case each).
6. A registry package in rust's lock closure changed: refused for rust only (python and typescript do not reach it).
7. A registry package that only core reaches changed: accepted.
8. Only `plugins/rust/tests/`, `conformance/` or `README.md` edited: accepted.
9. plugin.toml `plugin_version` != its `Cargo.toml` version: refused (rule a), even with no change.
10. Version lower than at the tag: refused.
11. Bumped without change: accepted.
12. No `v*` tag reachable: rule (b) skipped with a log line, (a) still enforced.
13. Two plugins wrong: one run lists both.

Controls (revert the code, the test must fail): strip_version removed (1 fails),
lock closure without the `source` filter (1 fails), and src dirs left out of the
diff (2 fails).

## Must confirm

1. Option 1A: sdk/wire/walk keep the release version.
2. Option 2A: the shipped-input path list, rather than every file.
3. Option 3A: the Cargo.lock closure, including the over-count from dev-deps.
4. Option 4: accept bumps without changes, so 4.16.0 keeps its plugins at 4.16.0 as the starting point.
5. The bump size (patch or minor) is left to the author; the script checks only "greater". Is that enough?
6. release.yml's "HOW TO CUT A RELEASE" comment (l.49-56) is now stale. The brief says CI is unchanged, so leave it stale, or allow a comment-only edit?
