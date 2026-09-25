# 0005. Plugin manifest: read once, fail hard, resolve paths at read time

## Status
Accepted (2026-09-25). Records decisions already embodied in
`core/src/daemon/manifest.rs`; the schema itself is in
[`plugin-modularity.md`](../architecture/plugin-modularity.md) (Data Model,
Interfaces) and
[`multi-language-plugins.md`](../architecture/multi-language-plugins.md)
("`plugin.toml` additions").

## Context
Core learns about each language plugin from its `plugin.toml`, read once at
daemon startup, before any plugin process exists. The same manifest is
consumed in two layouts (an installed release archive, and a cargo checkout
where binaries live in `target/<profile>/` and manifests in
`plugins/<language>/`) and on three platforms. A manifest that is wrong
cannot produce a correctly spawned plugin, and several failure modes are
silent rather than loud: a plugin that reads a different manifest than core
did degrades to structural without saying so, and a path spawned in the
wrong spelling fails before the plugin writes a byte.

## Decision
1. **Validation is a hard failure.** Every check bails with an error naming
   the manifest path and the problem, in the spirit of
   `protocol::handshake::verify` ("a protocol is code, not data"). No
   partial or best-guess manifest is returned. TOML, not JSON, to match
   `core/src/config`.
2. **Paths are resolved at read time, not by the spawner.** A value with no
   path separator is left for `$PATH` lookup at spawn time, as a shell
   would; anything else is joined against the manifest's own directory
   (`Path::join` already handles an absolute value), so a plugin can ship a
   relative entry point without knowing where it was installed.
3. **`${G_MESH_BIN_DIR}` in `command`.** A leading placeholder expands to
   the running g-mesh executable's directory, so a plugin built by cargo
   follows the core binary's profile (`target/release` spawns release
   plugins) instead of a hard-coded `../../target/debug/...`. Only
   `command`, only as a prefix; anywhere else is an error.
4. **Platform suffix fallback for `command`.** Manifests stay
   platform-neutral (suffix-less), while cargo emits `<name>.exe` on
   Windows. The suffixed spelling is used only once confirmed to exist;
   otherwise the path is left as written, so a missing binary is reported
   under the path the manifest names rather than a guess. The Go plugin
   needs no fallback: its build names the binary explicitly.
5. **Core tells the plugin which manifest it read**
   (`G_MESH_PLUGIN_MANIFEST`). Left alone, an SDK plugin looks for its
   manifest beside its executable, which is false in a checkout; core would
   then send a `semanticPass` to a plugin with no `[plugin.semantic]`, which
   silently degrades to structural. Passing the path removes the question,
   as `g_mesh_plugin_sdk::testing::PluginCheck` already does.
6. **Capabilities come from the manifest, not the handshake,** because
   routing and MCP instruction assembly need them before any plugin process
   exists. An absent section or field takes the conservative default ("says
   nothing" means "can do the least"), so an old manifest never silently
   claims a semantic pass or resolved receiver calls. Receiver-call
   resolution is an enum (`resolved`/`unresolved`) rather than a `bool`, so
   call sites read as the manifest's words.
7. **`watch_files` are globs** (`globset`), compiled at read time: C#'s
   project files (`*.csproj`, `*.sln`) have no fixed name, and an exact name
   is already a glob matching only itself, so there is one code path.
8. **Bundled roots: installed first, then checkout.** Both are listed
   unconditionally because at most one exists on a machine. Installed first
   so a release archive unpacked inside a checkout runs the plugins it
   shipped with; `~/.g-mesh/plugins/` outranks both.
9. **Windows extended-length paths are written back in ordinary form.**
   `fs::canonicalize` returns `\\?\...` on Windows, under which `/` is not a
   separator and `.`/`..` are not resolved; manifests spell entry points
   with forward slashes, so `node` could not open the joined path and every
   node-spawned conformance test failed on Windows CI while native plugins
   passed. `plain_win32_path` rewrites the prefix away when the ordinary
   spelling fits in `MAX_PATH`.

## Consequences
- A bad manifest stops the daemon from loading that plugin with an
  actionable message instead of a later, unexplained spawn failure.
- The spawner receives final paths and needs no knowledge of layouts.
- `MANIFEST_PATH_ENV` is duplicated between core and the SDK (core does not
  depend on the SDK); the two constants must be kept equal by hand.
- Adding a platform-specific spelling or a new placeholder is a change here,
  not in manifests.
