# Area D review: plugins and edges (g-mesh 4.30)

Model: claude-fable-5-2

Scope: `plugins/*` (go, python, rust, sdk, typescript), `wire`, `walk`, `scripts`, `.github/workflows`.
Input: `docs/review/4.30/module-map.md`, then code. No builds or tests were run.

Evidence labels: **measured** = seen in code, in a command's output or in a g-mesh answer during this review; **inferred** = a conclusion drawn from measured facts, not itself observed.

g-mesh calls used (project `g-mesh`, all returned complete answers, none slow): `find_references` on `ControlMessage`, `FileChangeResponse`, `ResolutionChangedResponse`, `Handshake` (`answer: files`); `find_definition` on `protocol::handshake::verify`. One call missed: `find_references("g_mesh_plugin_sdk::ids::node_id")` returned `resolvedBy: semanticNeighbours` (no such qualified name), so id questions were answered with grep in known files.

Findings are ranked by benefit/effort, highest first.

---

## D-fable-1. The SDK does not use the wire crate's request and response types

**Location:** `plugins/sdk/src/run.rs`: `Session::handle`, `Session::respond`, `Session::acknowledge`, `pass_response`; `wire/src/lib.rs`: `ControlMessage`, `FileChangeResponse`, `ResolutionChangedResponse`.

**Problem (leaky abstraction, duplication):** `wire` exists so that "sharing the declaration is what makes drift impossible" (`wire/src/lib.rs:28`). That holds for graph payloads and the handshake only. On the control plane the SDK decodes every request from a loose `serde_json::Value` using string literals, and builds every response with `serde_json::json!` literals. A field renamed or added in `ControlMessage` compiles on both sides and is silently read as its default by every SDK plugin (`unwrap_or_default`, `unwrap_or(false)`).

**Proposed change:** put the tolerant decode beside the type, in `wire`: `ControlRequest::parse(body) -> { id: Option<RequestId>, message: Result<ControlMessage, UnknownMethod> }`. It reads `id` and `method` loosely first (keeping the reason the SDK gives for loose parsing: an unknown method must still be acknowledged), then deserializes `params` into `ControlMessage`. `Session::handle` matches on `ControlMessage` variants. Responses are built from `FileChangeResponse` / `ResolutionChangedResponse` plus a new `AckResponse` in `wire`. Callers that change: `run.rs` only.

**Benefit:** a control-plane change becomes a compile error in the SDK instead of a behaviour discovered by the conformance kit or not at all. Adding a request field touches `wire` and the one handler that uses it.

**Effort:** S-M. **Risk:** serde strictness. A request the loose reader tolerates today (a non-string entry in `filePaths` is skipped, `run.rs` "A non-string entry costs itself, not the list") would fail typed deserialization as a whole; the fallback for a failed typed decode must be defined (acknowledge, log) and pinned by a test.

**Evidence:**
- measured, g-mesh `find_references(ControlMessage, answer: files)`: total 10, files are `core/src/watcher/apply.rs`, `core/src/cli/plugin_check/session.rs`, `core/src/daemon/plugin.rs`, `core/src/protocol/jsonrpc.rs`, `core/src/watcher/staleness.rs`, `core/tests/it/protocol_conformance.rs`, `wire/src/lib.rs`. No file under `plugins/`.
- measured, g-mesh `find_references(FileChangeResponse)`: total 7, all in `core/`. `find_references(ResolutionChangedResponse)`: total 2, both in `core/`. For contrast `find_references(Handshake)` does list `plugins/sdk/src/run.rs` (2 refs).
- measured, `plugins/sdk/src/run.rs:418-560`: `match method { "fileChanged" => ... params.get("filePath") ... params.get("reextract") ...`, also `"filePaths"`, `"budgetMs"`, `"linkedEdges"`, `"previousFacts"`.
- measured, `plugins/sdk/src/run.rs:909-940` (`pass_response`): keys `"incomplete"`, `"incompleteReason"`, `"unfinishedFiles"` written by hand; the same names are `#[serde(rename = ...)]` attributes on `FileChangeResponse` in `wire/src/lib.rs:897-940`.
- inferred: a rename on one side would not be caught by the compiler. Whether an existing test would catch a specific rename was not checked.

---

## D-fable-2. Three bundle scripts are one script; the language list is hand-enumerated around it

**Location:** `scripts/bundle-plugin.sh`, `scripts/bundle-python-plugin.sh`, `scripts/bundle-rust-plugin.sh`; `scripts/build-targets.sh` (`BUNDLED_PLUGINS`, `bundle_script_for`, the staged-discovery smoke at lines 365-381); `.github/workflows/release.yml` lines 187-190, 585, 791, 822; `scripts/cut-release.sh` (`CRATE_BACKED_PLUGINS`); `scripts/test-sections.sh`, `scripts/test-select.sh`.

**Problem (duplication, config sprawl):** the three cargo-plugin bundle scripts differ only in the language name. A fix to staging, manifest rewriting or the handshake smoke must be made three times (four with the Go variant, which shares the same tail). Around them, the set of bundled languages is partly centralised (`build-targets.sh --plugins`, used by `prepare-release-assets.sh`, `release-smoke.sh`, `test-install.sh`) and partly re-typed.

**Proposed change:** one `scripts/bundle-cargo-plugin.sh <language> [target] [dest]` deriving directory, binary name, marker and messages from the language; keep `bundle-go-plugin.sh` and move the shared tail (manifest rewrite, Mach-O check, handshake smoke) into `scripts/lib/`. In `build-targets.sh`, replace the four per-language `grep` checks with a loop over `BUNDLED_PLUGINS`. In `release.yml`, replace the three `@('typescript','go','rust','python')` literals with the output of `build-targets.sh --plugins`.

**Benefit:** adding or removing a bundled language, or changing how a plugin is staged, is one edit.

**Effort:** S. **Risk:** release packaging only; `core/tests/scripts/release_packaging_scripts.rs` exercises these scripts (it references `BUNDLED_PLUGINS`), and the `release.yml` path filter (lines 187-190) names the old script files and must follow.

**Evidence:**
- measured: `diff` of the three scripts with comment lines removed shows only language-name substitutions (directory, log prefix, exe name, stage dir, marker, generated-header text and its task id, handshake pattern, messages). No logic line differs.
- measured: `scripts/build-targets.sh:130` `BUNDLED_PLUGINS=(typescript go rust python)`; lines 207-210 map each to a script; lines 372-380 repeat one `grep -Eq "^<lang> +[0-9]"` per language.
- measured: `release.yml` lines 585, 791, 822 each contain `foreach ($lang in @('typescript', 'go', 'rust', 'python'))`.
- measured: `scripts/install.sh:747` and `scripts/install.ps1:510` use `plugins/typescript/plugin.toml` as the archive-layout sentinel.

---

## D-fable-3. A plugin's static facts are written in up to five places; `plugin.toml` has three independent Rust readers

**Location:** `plugins/<lang>/plugin.toml`; `plugins/<lang>/src/main.rs` (`PluginSpec::new`); `plugins/*/src/project/mod.rs` (`EXCLUDE_DIRS`, `WATCH_FILES`); `plugins/<lang>/tests/conformance.rs` (`PluginCheck` builder); `plugins/go/ignore.go` (`manifestExcludeDirs`), `plugins/go/wire.go` (`pluginVersion`); readers: `core/src/daemon/manifest.rs` (`RawManifest`), `plugins/sdk/src/manifest.rs` (`RawManifest`), `plugins/sdk/src/lsp/config.rs` (`RawManifest`); writer: `plugins/sdk/src/testing.rs` (`PluginCheck::manifest`).

**Problem (duplication, missing seam):** `plugin.toml` is core's source of truth, but the plugin restates extensions, exclude dirs, watch files, entry points and capabilities in code, and its conformance test restates them again to synthesise a manifest instead of checking the shipped one. The schema itself is declared three times in Rust with no shared type, and the `command` path rule is implemented twice.

**Proposed change:**
1. `PluginCheck::from_manifest(path)`: start from the shipped `plugins/<lang>/plugin.toml` and rewrite only `[plugin.spawn] command` (what the bundle scripts already do), so the conformance test checks the manifest that ships.
2. `PluginSpec::from_manifest_str(include_str!("../plugin.toml"))` as the in-code fallback, so `main.rs` and the `EXCLUDE_DIRS` constants stop restating the file.
3. Move the on-disk schema (`RawManifest` and sections, serde only) into `wire` or a small `g-mesh-manifest` crate used by core's validating reader and the SDK's two permissive readers.

**Benefit:** changing an extension list, exclude dir or capability flag is one edit in `plugin.toml`; a new manifest section is declared once.

**Effort:** M (steps 1-2 are S each). **Risk:** the SDK reader is permissive on purpose ("a manifest gaining a field never stops a plugin from starting", `plugins/sdk/src/manifest.rs:176-180`); a shared type must keep unknown-field tolerance on the plugin side and strictness on core's. Go keeps its own copies regardless.

**Evidence:**
- measured: `plugins/python/plugin.toml:45` `extensions = [".py", ".pyi"]`; `plugins/python/src/main.rs` `PluginSpec::new("python", env!("CARGO_PKG_VERSION"), &[".py", ".pyi"])`; `plugins/python/tests/conformance.rs:224-229` `.extensions(&[".py", ".pyi"])`, `.watch_files(&["pyproject.toml", "setup.cfg", "setup.py"])`, `.entry_points(&["__init__"])`, `.files_created(true)` with the comment "The shipped manifest declares it". Same pattern in `plugins/rust/tests/conformance.rs:248-256` and `plugins/typescript/tests/conformance.rs:108-118`.
- measured: three `struct RawManifest` declarations at `core/src/daemon/manifest.rs:856`, `plugins/sdk/src/manifest.rs:182`, `plugins/sdk/src/lsp/config.rs:461`.
- measured: `resolve_command` exists in both `core/src/daemon/manifest.rs:762` and `plugins/sdk/src/lsp/config.rs:422`; the latter's doc says "the same rule core resolves `[plugin.spawn] command` by".
- measured: `plugins/go/ignore.go:43` `manifestExcludeDirs` "Must equal" the manifest list, pinned by a test (per its comment; the test was not read).
- inferred: the conformance test would still pass if the shipped `plugin.toml` lost a capability the builder sets. The CI step `g-mesh plugins check` (`ci.yml:911`) does run the real directories, so the gap is local-only.

---

## D-fable-4. The Go plugin re-implements the SDK runtime, and parity rests on prose; the id scheme has already diverged

**Location:** `plugins/go/wire.go`, `jsonrpc.go`, `control.go`, `main.go`, `bulkindex.go`, `walk.go`, `ignore.go`, `symlinks.go`, `generated.go`, `ids.go`, `hold.go` (about 2,100 lines by the module map's counts) against `wire/src/lib.rs`, `walk/src/lib.rs` and `plugins/sdk/src/{run,framing,walk,generated,ids,hold}.rs`.

**Problem (duplication without a shared contract artefact):** every protocol or walk change is implemented twice, and only one of the eleven mirrored concerns has shared test data. Where there is none, the two sides have drifted while both sets of comments say they match.

**Proposed change:** keep two implementations (no cross-language code sharing is possible), but make the contract data, following the one case that already does: `plugins/sdk/testdata/generated_headers.json` is read by both `plugins/sdk/src/generated.rs` and `plugins/go/generated_test.go`. Add sibling corpora read by both test suites:
- `ids.json` (field tuples and expected ids);
- `gitignore_cases.json` (tree, `.gitignore` contents, expected walked set), including the syntax `ignore.go` declares unsupported, as expected differences;
- `wire_lines.ndjson` and `control_frames.json` emitted by `wire`'s tests from the real types, parsed and re-emitted by Go tests.

Then decide the id question explicitly: either migrate Go to the NUL scheme (changes every Go id, so a reindex) or correct both comments to say the schemes differ and why that is safe.

**Benefit:** a wire or walk change fails the Go suite at the same commit instead of at a conformance run; a fifth non-Rust plugin gets a specification it can execute.

**Effort:** M. **Risk:** low for the corpora; the id migration itself is a data change for existing Go indexes.

**Evidence:**
- measured: `plugins/go/ids.go:31` `hashID(fmt.Sprintf("node %s %s %s %s", ...))` (space-joined); `plugins/sdk/src/ids.rs` `digest` writes the tag then `FIELD_SEPARATOR` (`0u8`) before each field (NUL-joined).
- measured, computed with `shasum -a 256` during this review: `"node main.go File main.go "` gives `1453099e1c11aef90ca118eb6d140fe1`, the value pinned in `plugins/go/ids_test.go`; the NUL-joined spelling of the same tuple gives `1c06532b9ba18d30da653a043b3ed0e1`. `"node\0p\0File\0q\0"` gives `d0b5b9f1750a7aae60a7d88b4b17bd3c`, the value pinned in `plugins/sdk/tests/id_scheme.rs`; the space-joined spelling gives `788cfed0013ba5122d8a1ac404b1690e`. The two suites pin different schemes.
- measured: `plugins/sdk/src/ids.rs:18` "**`plugins/go` must match this**"; `plugins/go/ids_test.go:30` "plugins/sdk/src/ids.rs pins the same scheme".
- measured: `plugins/go/ignore.go:3-26` describes a hand-rolled gitignore subset without character classes, escapes or fused `**`, where the Rust side uses the `ignore` crate (`walk/Cargo.toml`).
- measured: `grep` for shared fixtures in `plugins/go/{ignore,ids,walk,generated}_test.go` finds only `../sdk/testdata/generated_headers.json`.
- inferred: the id difference causes no wrong answers today, because each plugin owns disjoint files and ids looked opaque to core in what was read. Not verified against every core path that handles ids.

---

## D-fable-5. The test-section list is duplicated in `ci.yml` with no guard

**Location:** `scripts/test-sections.sh` (`SECTIONS`, `section_filter`), `scripts/test-sections-check.py`, `.github/workflows/ci.yml` lines 779-849.

**Problem (duplication, testability):** `SECTIONS` has 12 entries and `ci.yml` has 12 hand-written steps, each repeating the same `if:` expression with the section name twice. `test-sections.sh check` proves that sections partition the suite, not that CI runs each one.

**Proposed change:** either one step that loops `scripts/test-sections.sh list` with a `::group::` per section and per-section timeouts kept in the script, or keep the steps and extend `test-sections.sh check` to parse `ci.yml` and fail when a section has no step.

**Benefit:** adding a crate or splitting a section is one edit, and a missed edit fails instead of silently dropping tests from CI.

**Effort:** S. **Risk:** the loop variant loses per-step timing and status in the Actions UI; the check variant has none.

**Evidence:**
- measured: `scripts/test-sections.sh:38-51` and `ci.yml` steps "tests: core-it" ... "tests: wire" (12 each).
- measured: `grep -n "ci.yml\|workflow"` over `test-sections.sh`, `test-sections-check.py`, `test-select.sh` matches only `test-select.sh:162` (the path rule), so no script reads the workflow.
- inferred: a 13th section added only to the script would run locally and never in CI.

---

## D-fable-6. `plugins/sdk/src/lsp` is a separable crate held in by one call

**Location:** `plugins/sdk/src/lsp/*` (7,918 lines including inline tests); `plugins/sdk/src/run.rs:read_control_stream`; `scripts/test-select.sh:classify`.

**Problem (cohesion, change blast radius):** the SDK holds two things with different change rates: the protocol runtime every plugin and core's tests depend on, and the LSP bridge only the semantic tier of three plugins uses. Any edit under `plugins/sdk/` selects the full suite.

**Proposed change:** new workspace crate `plugins/sdk-lsp` (`g-mesh-plugin-lsp`) depending on `g-mesh-plugin-sdk`; move `lsp/*`, `fake-lsp/` and `tests/lsp_bridge.rs` there. Remove the one back-edge by replacing `crate::lsp::kill_live_servers()` in `run.rs` with an exit-hook registry in `semantic.rs` (`on_lifeline_exit(fn())`) that the bridge registers with. In `test-select.sh`, map `plugins/sdk-lsp/*` to that crate's section plus the three plugin sections and `core-spawns-*`, not `full`.

**Benefit:** bridge work (the most active large file in the area by size) stops forcing a full run and stops recompiling for structural-only consumers; a structural-only plugin does not compile an LSP client.

**Effort:** M. **Risk:** `SemanticConfig::from_manifest` reuses the SDK's manifest lookup (`manifest_path` is `pub(crate)`), so one item must become public; `cut-release.sh` `PLUGIN_SHARED_CRATES` and `OTHER_WORKSPACE_MANIFESTS` need the new crate.

**Evidence:**
- measured: the only non-comment reference to `lsp` from SDK files outside `lsp/` is `plugins/sdk/src/run.rs:370` `crate::lsp::kill_live_servers();`.
- measured: `lsp/*` imports from the rest of the SDK are `framing`, `graph`, `index`, `path`, `semantic` (five `use crate::` lines in total).
- measured: `scripts/test-select.sh` `wire/* | plugins/sdk/*) full=1`.
- inferred: "most active" is from size, not from change history, which was not read.

---

## D-fable-7. `lsp/bridge.rs` is one 3,394-line module holding six concerns

**Location:** `plugins/sdk/src/lsp/bridge.rs` (5,195 lines, tests from line 3394).

**Problem (god module, testability):** budgets and server lifecycle (`Budgets`, `LspBridge::ensure_client`, `sync_documents`, `wait_ready`), question planning (`Question`, `Ask`, `questions`), answer accumulation and overload settling (`Answers`, `settle`, `settle_overloads`, `bind_overload`), location mapping (`locations`, `file_at`, `node_at`, `declaration_at`), hover-text matching (`hover_text`, `normalise_hover`, `hover_matches`, `without_first_parameter`) and the pass loop (`run_pass`, `record_answer`, `answer`) share one file and one private namespace. Per-server behaviour arrives as knobs on `LspBridge` (`language_ids`, `quiescent_signal`, `client_watches_files`, `request`, `OverloadDisambiguation`), so each new server need edits this file.

**Proposed change:** split along the existing section boundaries into `bridge/{lifecycle,questions,answers,landing,hover,pass}.rs` with `pub(super)` items, no behaviour change. Second step, optional: gather the per-server knobs into one `ServerProfile` value the plugin passes, so a new server quirk is a field, not a builder method plus a branch.

**Benefit:** overload or hover changes (Python, TypeScript) and readiness changes (Rust) stop colliding in one file; the pure parts (`hover_*`, `locations`, `questions`) become unit-testable without a server.

**Effort:** M, mechanical. **Risk:** low; `run_pass` (283 lines) and `LspBridge::answer` (206 lines) share state through `&mut self` and will not split cleanly without a context struct.

**Evidence:**
- measured: item outline of `bridge.rs` by line (struct and fn starts listed above); module map 7.3 lists `answer` at `bridge.rs:3167` with 206 lines.
- measured: doc comments at `bridge.rs:578-632` tie each knob to one server (tsserver's `typescriptreact`, rust-analyzer's `experimental/serverStatus` and `rust-analyzer/reloadWorkspace`).

---

## D-fable-8. The OS sandbox is a generic mechanism living inside the Rust plugin, in a cycle with its semantic module

**Location:** `plugins/rust/src/sandbox.rs` (1,566 lines), `plugins/rust/src/sandbox/landlock_ns.rs` (1,229 lines), `plugins/rust/src/semantic.rs`.

**Problem (cohesion, dependency direction):** `SandboxPolicy` (project root, executable prefixes, writable prefixes), `confine(&mut SemanticConfig, ...)` and the Seatbelt and Landlock backends say nothing about Rust. Only `SandboxPolicy::for_server` and `toolchain_prefixes` do. The mechanism and the Rust policy are in one module that imports from `semantic` while `semantic` imports from it.

**Proposed change:** move the mechanism (policy type, `confine`, `seatbelt`, `landlock_ns`, `trampoline`) to its own crate `plugins/sandbox` (or into the lsp crate of D-fable-6); keep `for_server`, `toolchain_prefixes`, `cargo_proxy` in the Rust plugin as the policy builder. That removes the cycle.

**Benefit:** OS-backend work is testable and releasable without the Rust plugin; pyright and vtsls could be confined with a policy of their own.

**Effort:** M-L. **Risk:** the Linux backend re-executes the plugin binary as a trampoline (`plugins/rust/src/main.rs:14`), so any host binary must call `trampoline` first; the Docker-only tests (`scripts/test-linux-sandbox-docker.sh`) name current module paths.

**Evidence:**
- measured: `plugins/rust/src/sandbox.rs:29` `use crate::semantic::Toolchain;` and `plugins/rust/src/semantic.rs:83` `use crate::sandbox::{self, SandboxOutcome, SandboxPolicy};`.
- measured: `grep -i sandbox` finds no match in `plugins/python/src/semantic.rs`, `plugins/typescript/src/semantic.rs` or `plugins/sdk/src/lsp/*.rs`.
- inferred: that the other two plugins would benefit. Whether they need confinement is a product decision not visible in this code.

---

## D-fable-9. The Rust and Python extractors carry the same emit layer twice

**Location:** `plugins/rust/src/extractor/emit.rs`, `plugins/python/src/extractor/emit.rs` (`Positions`, `Emitter`, `placeholder_key`, `container_target`).

**Problem (duplication):** both files define the same tree-sitter position adapter and the same thin wrapper over the SDK's `FileGraphBuilder` with the same method set (`declare`, `placeholder`, `reexport`, `external_module`, `resolved_edge`, `placeholder_edge`, `import_edge`, `open_site`, `mark_syntax_errors`, `finish`). Python adds overload bookkeeping on top.

**Proposed change:** a `tree-sitter` cargo feature on the SDK (or a small `plugins/sdk-treesitter` crate) providing `Positions` and a base `Emitter`; Python keeps its `definitions` extension as a wrapper.

**Benefit:** a change to placeholder or re-export emission rules is made once; a new tree-sitter plugin starts from the shared layer.

**Effort:** S-M. **Risk:** the two tree-sitter grammar crates must agree on one `tree-sitter` version for a shared `Point` type.

**Evidence:**
- measured: `diff` of `Positions` plus the edge-emitting methods of the two files (comment lines removed) shows three hunks: Python's extra `definitions` field, `finish(mut self)`, and two extra fields in `container_target`.
- measured: identical `pub(crate) fn` names and order in both files (`grep -n "fn "`).
- not measured: `Emitter::placeholder` and `reexport` bodies were not diffed line by line.

---

## D-fable-10. The plugin version rule is spread over three spellings, and this branch's bump broke it

**Location:** `plugins/{rust,python,typescript}/Cargo.toml` (`version`), `plugins/*/plugin.toml` (`plugin_version`), `scripts/cut-release.sh` (`check_crate_backed_plugin_versions`), tests named `the_manifest_version_matches_the_crates`.

**Problem (config sprawl; a defect that follows from the structure):** the rule is "a plugin's crate version equals its `plugin_version` and both move only when the plugin changed", while `wire`, `walk` and `sdk` must equal core's version. A release bump therefore has to treat seven manifests in two different ways by hand.

**Proposed change:** `version.workspace = true` for the four crates that must equal the release (core, `wire`, `walk`, `sdk`); for the three plugins, have `plugin.toml` be the only place the plugin version is typed and assert it from `Cargo.toml` in `cut-release.sh` as now, or generate one from the other in the bump step. `cut-release.sh:30` records that workspace versions were considered and declined; the reason given there was not read in full.

**Benefit:** the release bump stops touching plugin crates, and "which plugin changed in this release" is readable from versions.

**Effort:** S. **Risk:** `build-targets.sh` and `cut-release.sh` parse `[package] version` with text tools and would need to read the workspace value.

**Evidence:**
- measured: in this worktree `plugins/{python,rust,typescript}/Cargo.toml` say `version = "4.30.0"` and their `plugin.toml` say `plugin_version = "4.28.0"`; `git diff --stat main` lists the three plugin `Cargo.toml` files among the eight changed by the bump.
- measured: `plugins/python/src/semantic.rs:901-910` asserts `plugin_version == env!("CARGO_PKG_VERSION")`.
- inferred: that test, its two siblings and `cut-release.sh` rule (a) fail on this branch. Not run.

---

## Considered and rejected

- `wire` as one 1,962-line file: 951 lines of types plus tests, one concern, no split needed.
- `g-mesh-walk` as a separate crate from the SDK's `walk.rs`: the crate is shared with core, the SDK file is the plugin-side use of it; the layering is right.
- Plugins depending on both `g-mesh-plugin-sdk` and `g-mesh-wire` although the SDK re-exports `wire`: one type identity either way, cosmetic.
- Exact-match `CURRENT_PROTOCOL_VERSION` in `handshake::verify`: additive change goes through manifest capabilities and serde defaults, so a hard version check is a sound choice.
- The SDK hosting `g-mesh-fake-plugin` for core's tests: Cargo has no stable binary dependency, and a separate crate would not remove the test-time coupling.
- `core/build.rs` building the Go plugin: best-effort, warns instead of failing, and the flags are shared through `core/go_plugin_build_flags.rs`.
- The SDK's permissive `plugin.toml` reader beside core's strict one: deliberate (a plugin must start on a manifest newer than itself); only the missing shared schema is a finding (D-fable-3).
- `census.rs` in both the Rust and Python plugins: same purpose, different data (347 of about 650 lines differ after normalising names), not a copy.
- `install.sh` and `install.ps1` as parallel implementations: two shells, no sharing mechanism, each has its own tests.
- `plugins/go/semantic.go` at 2,961 lines in one file: a single Go package, splitting is file layout only.
- The CI step that loops `plugins/*/conformance`: already data-driven, the pattern the other lists should follow.

## Not verified

- No build or test was run; every "would fail" statement is inferred.
- Whether the `ci.yml` `test` job installs every language toolchain regardless of the selected sections was not checked.
- Core-side handling of ids (D-fable-4) was sampled by grep, not traced.
- The brief said not to read `git log`. One `git log --oneline -3 -- plugins/python/Cargo.toml` was run to date the version bump; it returned three "chore: bump version" subjects and nothing else. `git status` also printed the file names of other reports in this directory; none was opened.
