# GM-713: split `lsp/bridge.rs` by concern, then `plugins/sdk/src/lsp` as its own crate (R-29, R-28)

Source findings: `docs/review/4.30/README.md` R-28, R-29 (and `D-fable.md` D-fable-6, D-fable-7).
Read against `release-4.33.0` (= `main` + version bump). Line numbers are that tree at the
time of writing; each code slice re-reads by symbol, not by line.

Goal: a server-quirk change stops touching lifecycle/budget code (R-29), and a bridge edit
stops forcing a full test run (`scripts/test-select.sh:160` maps `plugins/sdk/*` to `full=1`)
(R-28).

## 0. Where we start

### 0.1 `bridge.rs`

`plugins/sdk/src/lsp/bridge.rs` is **5194** lines: 1-3393 code, 3394-5194 `#[cfg(test)] mod
tests` (1801 lines, 52 `#[test]`s, `use super::*`). It has two section markers today:
`// --- questions` (961) and `// --- the pass` (1367). Ranges below include each item's doc
comment (start of doc, then `item line`, end).

| Lines | Items | Module (proposed) |
|---|---|---|
| 1-22 | module doc, `use` block | `bridge.rs` (trim unused imports after the move) |
| 24-172 | `Budgets` (doc 24, struct 132), `impl Default for Budgets` 158 | `bridge.rs` (lifecycle/budgets) |
| 174-187 | `MAX_SERVER_STARTS` | `bridge.rs` |
| 189-554 | `LspBridge` (doc 189-492: readiness, retraction, re-export hops; struct 493) | `bridge.rs` |
| 556-560 | `OpenDocument` | `bridge.rs` |
| 562-938 | `impl LspBridge`: `new`, `language_ids`, `warm_up`, `quiescent_signal`, `client_watches_files`, `request`, `with_budgets`, `pass_deadline`, `pass_budget`, `ensure_client`, `sync_documents`, `forward_watched`, `wait_ready` | `bridge.rs` |
| 940-959 | `missing_binary`, `hash_of` | `bridge.rs` |
| 960-1043 | `// --- questions`, `Question`, `Ask`, `Hovered`, `impl Question` (`method`, `accounted_to`, `is_empty_answer`) | `bridge/questions.rs` |
| 1045-1178 | `Questions`, `questions` | `bridge/questions.rs` |
| 1180-1187 | `language_id` | `bridge.rs` (callers: `sync_documents` 797/829, `answer` 3230/3253; move the fn up next to `hash_of`) |
| 1189-1365 | `site_key`, `unsettled_hop`, `is_type_member`, `Overloaded` + impl (`of`, `targets`), `name_position`, `declaration_name_position` | `bridge/questions.rs` |
| 1366-1582 | `// --- the pass` marker, `Answers` (struct), `PendingOverload`, `Landing`, `Recorded`, `answer_ids`, `Placeholder`, `witness_rank`, `site_range`, `written_start`, `Address`, `address_key` | `bridge/answers.rs` |
| 1584-1991 | `impl Answers`: `new`, `builder`, `record`, `settle`, `settle_overloads`, `conclude`, `bind_overload`, `hovered`, `finish` | `bridge/answers.rs` |
| 1993-2030 | `address_of`, `is_addressable` | `bridge/answers.rs` |
| 2032-2315 | `ServerLocation`, `locations`, `file_at`, `wire_position`, `node_at`, `named_landing`, `lands_on_name`, `declaration_at`, `overload_landing`, `Choice`, `choose_overload`, `refines` | `bridge/landing.rs` |
| 2317-2412 | `hover_text`, `normalise_hover`, `hover_matches`, `without_first_parameter` | `bridge/hover.rs` |
| 2414-2424 | `EXIT_GRACE`, `exited` | `bridge.rs` (used by `wait_ready` 933 and `run_pass`) |
| 2426-3137 | `run_pass`, `PassTiming`, `record_answer`, `untyped_call_answered`, `trim_untyped_calls`, `record_implementor`, `supertype_reaches` | `bridge/pass.rs` |
| 3139-3373 | `impl SemanticEngine for LspBridge` (`workspace_changed`, `prepare`, `set_pass_deadline`, `answer` 206 lines) | `bridge/pass.rs` (D3) |
| 3375-3392 | `retract_stale` | `bridge/pass.rs` |
| 3394-5194 | `mod tests` | `bridge/tests.rs` (D2) |

Sizes after the split, approximately: `bridge.rs` ~990, `questions.rs` ~400, `answers.rs` ~665,
`landing.rs` ~285, `hover.rs` ~97, `pass.rs` ~970, `tests.rs` ~1800.

Cross-module edges the split creates (from `find_callees run_pass`, `find_callers` on
`site_key`, `Overloaded::of`, `name_position`, `is_type_member`, `refines`, `language_id`, and a
grep of each item's non-test uses in this one file):
- `questions` -> `landing::locations`, `hover::hover_text` (`Question::is_empty_answer` 1039-1040),
  `answers::is_addressable` (`unsettled_hop` 1227).
- `answers` -> `questions::{Hovered, declaration_name_position}` (1867, 1877),
  `landing::{choose_overload, Choice}` (1852), `hover::hover_matches` (1922).
- `landing` -> `answers::{address_of, answer_ids, is_addressable}` (2115, 2192, 2304-2305).
- `pass` -> all four, plus `bridge::{exited, EXIT_GRACE, Budgets, LspBridge}` and private
  `LspBridge` fields (a child module sees its parent's private items).
- `bridge.rs` -> `pass::trim_untyped_calls` only in doc links (537, 1412); no code edge back.

### 0.2 `crate::lsp` references outside `plugins/sdk/src/lsp`

- **Code: 1.** `run.rs:370` `crate::lsp::kill_live_servers();` in `read_control_stream`
  (`find_callers kill_live_servers`: `run::read_control_stream` and the test
  `a_server_started_by_prepare_is_reaped_by_kill_live_servers`).
- **Declaration: 1.** `lib.rs:90` `pub mod lsp;`.
- **Doc links: 8** (grep; rustdoc cannot link to a downstream crate, so these become plain
  text): `lib.rs:21`, `semantic.rs:148,180,190,203`, `testing.rs:230`, `run.rs:1076`,
  `manifest.rs:159`.

### 0.3 Reverse edges (lsp -> rest of sdk), which a separate crate must reach through sdk's public API

| Used by lsp | Where | Public outside sdk today? |
|---|---|---|
| `graph::{EdgeSpec, FileGraphBuilder, OpenSite, OpenSiteKind, PlaceholderKind, FileGraph, NodeSpec, placeholder_id}` | bridge.rs 19, 1211, 1280, 1486, tests 3397 | yes, root re-exports (`lib.rs:102-105`) |
| `index::SdkIndex` | bridge.rs 20 | yes, root |
| `index::contains` (as `node_contains`) | bridge.rs 20 | **no**: `pub(crate)` (`index.rs:197`), module private |
| `path::RelPath` | bridge.rs 21 | yes, root |
| `semantic::{SemanticAnswer, SemanticEngine}` | bridge.rs 22 | yes, root |
| `ids::edge_id` | bridge.rs 1487, 1663 | yes (`pub mod ids`) |
| `log::timing_enabled`, `log_line!` (17 sites), `log_timing!` (4) | bridge.rs, client.rs | yes; both macros are `#[macro_export]` and expand to `$crate::log::*` (pub) |
| `framing::{read_frame, write_message}` | client.rs 71 | yes (`pub mod framing`) |
| `manifest::manifest_path` | config.rs 336 | **no**: `pub(crate)` (`manifest.rs:164`), module private |

Two items must be published (D-fable-6 named only `manifest_path`). External crates the lsp
code uses: `anyhow`, `serde`, `serde_json`, `toml`, `g_mesh_wire`; dev: `tempfile`.

### 0.4 Who depends on sdk's lsp

No Cargo features anywhere (`plugins/sdk/Cargo.toml` has no `[features]`; plugins depend on it
plainly). `get_dependencies Incoming g_mesh_plugin_sdk::lsp` (complete, not truncated): 11 files,
24 `g_mesh_plugin_sdk::lsp` lines:
- rust: `src/semantic.rs` (80, 1014), `src/sandbox.rs` (27, 376, 566),
  `src/sandbox/landlock_ns.rs` (37), `tests/semantic_pass_measurement.rs` (39),
  `tests/semantic_runs_no_project_code.rs` (852, 1262, 1323, 1826, 2223).
- python: `src/semantic.rs` (211, 454, 702), `tests/overload_binding.rs` (25).
- typescript: `src/semantic.rs` (47, 153), `tests/overload_binding.rs` (15).
- sdk's own: `tests/lsp_bridge.rs` (49, 3200, 3216), `tests/lsp_prepare_reaped.rs` (11), and the
  doctest in `lsp/mod.rs`.
- Call sites through the module name (`use ...::lsp::{self, ..}`): `lsp::resolve` (rust
  semantic.rs:910, python 308, typescript 128), `lsp::probe` (rust sandbox.rs:540,
  landlock_ns.rs:177). (`lsp::uri`/`lsp::Session` in python semantic.rs:660-708 is a local test
  module, not the sdk.)
- The Go plugin is Go; it does not link the sdk.
- `g-mesh-fake-lsp` (`[[bin]]` of sdk, `fake-lsp/main.rs`): used through `CARGO_BIN_EXE_` by
  `tests/lsp_bridge.rs` and `tests/lsp_prepare_reaped.rs`, and by file name from core's
  `mcp/unlinked_tests.rs:370` (`target/<profile>/g-mesh-fake-lsp`, needs `cargo build
  --workspace`). The name must not change.

### 0.5 Test selection, release scripts

- `scripts/test-select.sh` `classify` (160-205): `wire/* | plugins/sdk/*) full=1` (162). A
  path under `plugins/sdk-lsp/` matches no row today and falls to `*) full=1` (203). Plugin rows
  (178-185): CI adds `plugin-<p>` + every core section; `--narrow` adds `plugin-<p>` +
  `core-spawns-<p>`.
- `scripts/test-sections.sh`: `SECTIONS` (38-51) has `sdk` (filter `package(g-mesh-plugin-sdk)`,
  110); `wire` already covers two packages (`package(g-mesh-wire) | package(g-mesh-walk)`, 114).
  A section list is also hard-coded in `core/tests/scripts/test_sections_scripts.rs:20-33`
  (`[&str; 12]`), `scripts/test-local.sh:49` (`LIB_SECTIONS`) and `.github/workflows/ci.yml`
  (one step per section, 779-850).
- `scripts/test-plugin-spawns.txt:107` `lib:mcp::unlinked::tests rust`: the core test that runs
  `g-mesh-fake-lsp` is already in `core-spawns-rust`.
- `scripts/cut-release.sh`: `OTHER_WORKSPACE_MANIFESTS` (246-250: version must equal core's) and
  `PLUGIN_SHARED_CRATES` (305-309: their `src/` and `Cargo.toml` are shipped inputs of every
  crate-backed plugin). Both need `plugins/sdk-lsp`, or an sdk-lsp change would not demand a
  plugin version bump and its version could drift. Fixture:
  `core/tests/scripts/cut_release_plugin_versions.rs:165-177` writes the member manifests.

## 1. Decisions

### D1. Layout: lifecycle stays in `bridge.rs`; five sibling child modules under `bridge/`

- **Today:** one file, one private namespace; an overload fix (python/typescript) and a
  readiness fix (rust) edit the same file as `Budgets` and `ensure_client`.
- **Change:** `lsp/bridge.rs` keeps the module doc, `Budgets`, `LspBridge` and its inherent
  impl, `OpenDocument`, `missing_binary`, `hash_of`, `language_id`, `EXIT_GRACE`, `exited`, and
  declares `mod questions; mod answers; mod landing; mod hover; mod pass; #[cfg(test)] mod
  tests;`. Files: `lsp/bridge/{questions,answers,landing,hover,pass,tests}.rs` (2018-style
  `bridge.rs` + `bridge/`, so `git log --follow bridge.rs` keeps the lifecycle history).
- **Example:** a hover-normalisation fix touches `bridge/hover.rs` only; a new server knob
  touches `bridge.rs` only.
- **Consequence:** every moved item, field and inherent method gets `pub(super)` (visible in
  `bridge` and all its children, `tests` included); nothing becomes visible outside `bridge`.
- **Alternative:** `answers/{hover,landing}.rs` nested under `answers`, which the review's
  wording "answers (+ hover, landing)" allows. Not taken: `landing` is used by `questions`
  (`locations`) and `pass` (`node_at`, `file_at`, `refines`, `overload_landing`) as much as by
  `answers`, so nesting would make `pass` reach into `answers::landing`.

### D2. The tests move whole to `bridge/tests.rs`, unchanged

- **Today:** 1801 test lines at the end of `bridge.rs`, sharing fixtures (`node`,
  `untyped_index`, `landing_index`, `declaring_file`, ...) across concerns.
- **Change:** cut 3395-5193 (the body of `mod tests`) into `bridge/tests.rs`; `bridge.rs` gets
  `#[cfg(test)] mod tests;`. Inside, `use super::*;` plus explicit `use super::{questions::*,
  answers::*, landing::*, hover::*, pass::*};` (a test-only glob; the production modules import
  by name).
- **Example:** `lsp::bridge::tests::a_call_answered_inside_a_function_still_lands_on_it` keeps
  that exact name.
- **Consequence:** the test-name set is identical before and after, which is the check that the
  move dropped nothing (section 3). Splitting tests per module (D-fable-7's "pure parts
  unit-testable") is left for when someone edits them; it is not mechanical, the fixtures cross
  concerns.
- **Alternative:** per-module `#[cfg(test)] mod tests` now. Costs fixture extraction and makes
  S2 a judgement slice instead of a sonnet move.

### D3. `impl SemanticEngine for LspBridge` moves whole to `pass.rs`

- **Today:** 3139-3373; `answer` (206 lines) is the pass driver, the other three methods are
  3-12 lines each.
- **Change:** move the impl block and `retract_stale` unchanged to `bridge/pass.rs`.
- **Consequence:** a pure move (sonnet-safe). `prepare` (calls `ensure_client`) and
  `workspace_changed` are lifecycle hooks that then live in `pass.rs`; 20 lines, acceptable.
- **Alternative:** keep the trait impl in `bridge.rs` and turn `answer`'s body into
  `fn answer_pass(&mut self, ..)` in `pass.rs`. Cleaner split, but not a pure move.

### D4. `ServerProfile`: later, after GM-711, as its own task

- **Today:** per-server knobs are four builder methods on `LspBridge` (`language_ids`,
  `warm_up` used by typescript; `quiescent_signal`, `client_watches_files` used by rust;
  `semantic.rs:138`, `typescript/semantic.rs:102`) plus manifest fields on `SemanticConfig`
  (`readiness`, `overload_disambiguation`).
- **Change now:** none. After D1 those knobs live only in `bridge.rs`, which already meets
  R-29's goal (a quirk change does not touch questions/answers/pass).
- **Why later:** GM-711 replaces `config.rs`'s `RawManifest` with a shared typed `plugin.toml`
  schema. Whether a knob is a manifest field (like `readiness`) or code (like `warm_up`) is
  exactly what a profile would settle, and settling it before that schema exists means doing it
  twice. A profile also changes the public API three plugins call, and two plugin tests
  pin the builder text (`rust/semantic.rs:1202`, `typescript/semantic.rs:273`).
- **Alternative:** `ServerProfile` now, in R-28's crate move. Adds an API change to a move.

### D5. Exit-hook registry: `on_lifeline_exit(fn())` in a new sdk module, registered by the client on first spawn

- **Today:** `read_control_stream` (run.rs:343-375), after EOF and `LIFELINE_GRACE`, calls
  `crate::lsp::kill_live_servers()` then `process::exit(1)` (GM-397: `exit` skips
  `LspClient::drop`).
- **Change:** new `plugins/sdk/src/lifeline.rs`:
  ```rust
  static HOOKS: Mutex<Vec<fn()>> = Mutex::new(Vec::new());
  pub fn on_lifeline_exit(hook: fn()) { /* lock (poison-tolerant), push */ }
  #[doc(hidden)]
  pub fn run_lifeline_exit_hooks() { /* clone the Vec under the lock, release it, call each */ }
  ```
  re-exported from `lib.rs`. `run.rs:370` becomes `crate::lifeline::run_lifeline_exit_hooks();`.
  In sdk-lsp, `LspClient::start` (client.rs:268-272, right after the `LIVE_SERVERS` push) calls
  `static REGISTERED: Once` -> `g_mesh_plugin_sdk::on_lifeline_exit(kill_live_servers)`.
- **Example:** a plugin whose engine never starts a server registers nothing; the first
  `prepare` or pass that spawns rust-analyzer registers the kill once.
- **Consequence:** sdk names no lsp item; registration happens exactly when there is something
  to kill (servers started by `prepare` included). Running hooks from a clone means a hook that
  registers another cannot deadlock. A new file keeps the edit out of `run.rs` but for one line
  (GM-711 also edits `run.rs`).
- **Alternatives:** (B) a `SemanticEngine::lifeline_hook(&self) -> Option<fn()>` that
  `LazyEngine` registers after the factory runs: no global in sdk-lsp, but a trait member that
  every non-LSP engine ignores, and the registry in sdk is still a global (the reader thread
  starts before any engine exists). (C) `Box<dyn Fn() + Send + Sync>`: nothing to capture, an
  allocation for no gain. (D) `libc::atexit`: `process::exit` does run atexit, but it is FFI
  for one call. D-fable put the registry in `semantic.rs`; a separate file is cheaper to merge.

### D6. Crate: `plugins/sdk-lsp`, package `g-mesh-plugin-sdk-lsp`, lib `g_mesh_plugin_sdk_lsp`, no features

- **Change:** `plugins/sdk/src/lsp/*` -> `plugins/sdk-lsp/src/*` (`mod.rs` -> `lib.rs`),
  `plugins/sdk/fake-lsp/` -> `plugins/sdk-lsp/fake-lsp/` (same `[[bin]] name =
  "g-mesh-fake-lsp"`), `plugins/sdk/tests/lsp_{bridge,prepare_reaped}.rs` ->
  `plugins/sdk-lsp/tests/`. Version `4.33.0` (read from `plugins/sdk/Cargo.toml`), added to
  root `members`.
- **Consequence:** the name matches the directory and the grep for `sdk-lsp` finds every
  mention. Every plugin uses the bridge, so a feature gate would be on in every build.
- **Alternative:** `g-mesh-plugin-lsp` (D-fable-6). Shorter, but directory and package differ.

### D7. No compatibility re-export from sdk: it cannot exist

- **Today:** plugins write `g_mesh_plugin_sdk::lsp::X`.
- **Why not:** sdk-lsp depends on sdk (`SemanticEngine`, `SdkIndex`, graph builders), so sdk
  re-exporting sdk-lsp is a dependency cycle cargo refuses. The only re-export that compiles
  is the other way (sdk-lsp re-exporting sdk), which does not keep old paths working.
- **Change:** each plugin's `Cargo.toml` gains `g-mesh-plugin-sdk-lsp = { path = "../sdk-lsp" }`
  and the 24 lines in section 0.4 change `g_mesh_plugin_sdk::lsp` to `g_mesh_plugin_sdk_lsp`;
  where the module name is used as a path (`lsp::resolve`, `lsp::probe`), import it as
  `use g_mesh_plugin_sdk_lsp::{self as lsp, ...}` so the call sites stay as they are.
- **Consequence:** 11 files of import edits, no behaviour change.

### D8. Test selection: widen the `sdk` section to both packages; map `plugins/sdk-lsp/*` like a plugin row

- **Today:** any `plugins/sdk/*` path is `full`.
- **Change:** `test-sections.sh:110` `sdk) echo 'package(g-mesh-plugin-sdk) |
  package(g-mesh-plugin-sdk-lsp)'` (as `wire` covers walk). `test-select.sh`, a row before the
  plugin row (178):
  ```bash
  plugins/sdk-lsp/*)
  	case "$narrow:$1" in
  	1:plugins/sdk-lsp/Cargo.toml | :*) add sdk plugin-typescript plugin-rust plugin-python "${CORE[@]}" ;;
  	*) add sdk plugin-typescript plugin-rust plugin-python core-spawns-typescript core-spawns-rust core-spawns-python ;;
  	esac
  	;;
  ```
  plus a line in the header invariants (22-46) and in `docs/adr/0030-test-sections.md`.
- **Example:** `--narrow` with `plugins/sdk-lsp/src/bridge/hover.rs` prints
  `sdk plugin-typescript plugin-rust plugin-python core-spawns-typescript core-spawns-rust
  core-spawns-python`; CI prints the seven core sections + `sdk` + the three plugin sections.
  `plugins/sdk/src/run.rs` still prints `full`.
- **Consequence:** CI keeps its rule ("every core section for any plugin change"), so the CI
  saving is the `wire` section and full-mode overhead; the local (`--narrow`) saving is most of
  core. No edit to `ci.yml`, `test_sections_scripts.rs:20`, `test-local.sh:49`: fewer collisions
  with GM-712. The `sdk` section also runs sdk's own (now small) tests on an sdk-lsp change.
  Note: the acceptance says "the lsp + plugin sections"; under this option the lsp crate's
  section is `sdk`.
- **Alternative:** a new section `sdk-lsp`. Matches the acceptance wording literally; costs a
  `SECTIONS` entry and filter in `test-sections.sh`, a `ci.yml` step, the 12->13 array in
  `test_sections_scripts.rs`, `LIB_SECTIONS` in `test-local.sh`, all files GM-712 also edits.

### D9. Order: R-28 (S3) starts after GM-711 and GM-712 are merged into release-4.33.0

- **Why:** S3 moves `config.rs`, which GM-711 rewrites (its `RawManifest` becomes the shared
  schema), publishes `manifest_path` from `manifest.rs` (GM-711 likely reshapes it), and edits
  `run.rs:370` (GM-711 edits control handling in `run.rs`). It edits `test-select.sh` and
  `test-sections.sh`, which GM-712 edits. Moving a file someone else rewrites in parallel is
  the worst merge; rebasing a move onto the rewrite is mechanical.
- **S2 (R-29) has no collision:** it touches only `lsp/bridge.rs` and adds `lsp/bridge/*`;
  merge it into the release first, as the plan says.

## 2. Edit maps

### 2.1 R-29 (S2, sonnet): a mechanical move inside `plugins/sdk/src/lsp/`

Read first: this note's table 0.1; `bridge.rs` 1-22 (imports), 961 and 1367 (markers).

1. Create `plugins/sdk/src/lsp/bridge/` with the files below. Each starts with a one-line
   `//!` doc naming its concern (questions: "Which questions a pass asks"; answers: "What an
   answer becomes: the accumulated diff and overload settling"; landing: "Where a server
   location lands: a file, a node, an overload"; hover: "Hover text, normalised and compared";
   pass: "One pass: the request loop and recording each answer"; tests: none needed).
2. Cut, in this order from the bottom up so earlier line numbers stay valid:
   - 3395-5193 (inside `mod tests { ... }`, not the braces) -> `bridge/tests.rs`; replace
     3394-5194 with `#[cfg(test)]\nmod tests;`.
   - 3375-3392 and 3139-3373 -> `bridge/pass.rs` (keep source order).
   - 2426-3137 -> `bridge/pass.rs` (before the impl).
   - 2317-2412 -> `bridge/hover.rs`.
   - 2032-2315 -> `bridge/landing.rs`.
   - 1366-2030 -> `bridge/answers.rs` (drop the `// --- the pass` marker line).
   - 1189-1365 and 960-1178 -> `bridge/questions.rs` (drop the `// --- questions` marker),
     leaving `language_id` (1180-1187) in `bridge.rs`.
   - Keep 2414-2424 (`EXIT_GRACE`, `exited`) and 1180-1187 in `bridge.rs`.
3. In `bridge.rs` after the `use` block: `mod answers; mod hover; mod landing; mod pass; mod
   questions;` and the `use` lines it needs from them (none for code; doc links only).
4. Visibility: every moved top-level item, struct field, enum (variants follow) and inherent
   method gets `pub(super)`, unless already `pub`. No `pub` and no `pub(crate)` added.
5. Imports: each new file names what it uses: `use super::{..}` for parent items,
   `use super::<sibling>::{..}` for siblings (section 0.1's edge list), `use crate::..` and
   `g_mesh_wire::..` / `serde_json::..` / `std::..` as the moved code needs. No glob except in
   `tests.rs` (D2). Trim `bridge.rs`'s now-unused imports. Let the compiler drive this.
6. Doc links that now cross modules (e.g. `bridge.rs` 537 and 1412 [`trim_untyped_calls`],
   [`untyped_call_answered`]): prefix the module (`[`pass::trim_untyped_calls`]`).
7. No other edit: no rename, no reformat beyond `cargo fmt`, no comment rewrites.

Checks (targeted, not the suite): before the move, `cargo nextest list -p g-mesh-plugin-sdk
--lib > before.txt`; after, `after.txt`; `diff` must be empty. `cargo fmt --check`,
`cargo clippy -p g-mesh-plugin-sdk --all-targets -- -D warnings`, `cargo nextest run -p
g-mesh-plugin-sdk --lib -E 'test(/^lsp::/)'` and `--test lsp_bridge`. `cargo doc -p
g-mesh-plugin-sdk --document-private-items` warning count not higher than before (no CI gate,
but broken links are the move's typical leftover). `wc -l` per file in the report.

### 2.2 R-28 (S3, opus): the crate move, the hook, selection, release scripts

Read first: `run::read_control_stream` (run.rs:343-375), `client::kill_live_servers`
(client.rs:118) and the `LIVE_SERVERS` push in `LspClient::start` (client.rs:264-275),
`classify` (test-select.sh:160-205), `check_workspace_versions` and
`check_crate_backed_plugin_versions` in `cut-release.sh`. Re-read GM-711's merged `config.rs`
and `manifest.rs` first (D9).

1. **Move** (`git mv`, then edit): `plugins/sdk/src/lsp/` -> `plugins/sdk-lsp/src/`
   (`mod.rs` -> `lib.rs`; its `mod`/`pub use` lines stay); `plugins/sdk/fake-lsp/` ->
   `plugins/sdk-lsp/fake-lsp/`; `plugins/sdk/tests/lsp_bridge.rs`, `lsp_prepare_reaped.rs` ->
   `plugins/sdk-lsp/tests/`.
2. **Cargo:** new `plugins/sdk-lsp/Cargo.toml` (package `g-mesh-plugin-sdk-lsp`, version
   4.33.0, edition 2021, license as sdk; `[lib] name = "g_mesh_plugin_sdk_lsp"`;
   `[[bin]] g-mesh-fake-lsp` with sdk's comment from `plugins/sdk/Cargo.toml:29-41`; deps
   `anyhow`, `g-mesh-plugin-sdk = { path = "../sdk" }`, `g-mesh-wire = { path = "../../wire" }`,
   `serde` (derive), `serde_json`, `toml`; dev `tempfile`). Remove the fake-lsp `[[bin]]` from
   `plugins/sdk/Cargo.toml` (29-41). Root `Cargo.toml:38` members + `"plugins/sdk-lsp"`.
   `plugins/{rust,python,typescript}/Cargo.toml` + `g-mesh-plugin-sdk-lsp = { path =
   "../sdk-lsp" }`. Keep `Cargo.lock` as cargo writes it.
3. **Paths in the moved code:** every `crate::<sdk item>` becomes `g_mesh_plugin_sdk::<item>`
   via root re-exports (`graph::X` -> `X`, `index::SdkIndex`, `path::RelPath`,
   `semantic::{..}` -> root; `ids`, `log`, `framing` stay module paths; `crate::log_line!` ->
   `g_mesh_plugin_sdk::log_line!`, same for `log_timing!`). `crate::<lsp module>` stays.
   List: section 0.3 plus the same in `bridge/*.rs` after S2. Check: `grep -rn "crate::"
   plugins/sdk-lsp/src` shows only lsp-internal modules.
4. **Publish two sdk items** (section 0.3), hidden: `lib.rs` `#[doc(hidden)] pub use
   index::contains as node_contains;` and `#[doc(hidden)] pub use manifest::manifest_path;`
   (`index.rs:197`, `manifest.rs:164` become `pub`), or whatever GM-711 has made of
   `manifest_path` by then.
5. **Hook (D5):** `plugins/sdk/src/lifeline.rs`; `lib.rs` `mod lifeline; pub use
   lifeline::{on_lifeline_exit, run_lifeline_exit_hooks};`; `run.rs:370` ->
   `crate::lifeline::run_lifeline_exit_hooks();`; client.rs `LspClient::start` registers once.
   `lsp_prepare_reaped.rs` switches from `kill_live_servers()` to
   `g_mesh_plugin_sdk::run_lifeline_exit_hooks()` (it then pins the registration, section 3
   B1); `kill_live_servers` stays `#[doc(hidden)] pub` for it.
6. **sdk leftovers:** delete `lib.rs:90` `pub mod lsp;`; rewrite the 8 doc links (0.2) as plain
   text naming `g_mesh_plugin_sdk_lsp::LspBridge`. `grep -rn "lsp" plugins/sdk/src` must show
   no code reference.
7. **Consumers (D7):** the 11 files / 24 lines of section 0.4 (sdk tests already moved);
   the doctest in the new `lib.rs` (`use g_mesh_plugin_sdk_lsp::{LspBridge, SemanticConfig};`).
8. **Comments naming the old path** (text only): `plugins/rust/src/semantic.rs:5`,
   `plugins/python/src/semantic.rs:13,136`, `plugins/typescript/src/semantic.rs:5`,
   `plugins/rust/src/extractor/bodies.rs:56`, `plugins/python/tests/semantic_runs_no_project_code.rs:359`,
   `plugins/sdk-lsp/src/client.rs:34` (`plugins/sdk/fake-lsp`), `plugins/sdk-lsp/tests/lsp_bridge.rs:10`.
   Leave `plugins/{rust,python}/plugin.toml` comments (82, 94) unless the plugin version is
   bumped anyway (a `plugin.toml` edit counts as a plugin change for `cut-release.sh`). Leave
   `docs/design`, `docs/architecture`, `docs/results` (history).
9. **Selection (D8):** `test-sections.sh:110`; `test-select.sh` row + header; ADR 0030 line.
10. **Release scripts:** `cut-release.sh` `OTHER_WORKSPACE_MANIFESTS` (+ `"plugins/sdk-lsp/Cargo.toml"`),
    `PLUGIN_SHARED_CRATES` (+ `plugins/sdk-lsp`), header comment (24-25, 38, 61-62);
    `release.yml:54` and `bundle-rust-plugin.sh:89` comments; fixture
    `core/tests/scripts/cut_release_plugin_versions.rs:165-177` writes `plugins/sdk-lsp/Cargo.toml`
    too, or `check_workspace_versions` fails on the fixture.

Checks: `cargo build --workspace`; fmt; `scripts/check.sh clippy`; `scripts/test-sections.sh
check` (partition: every sdk-lsp test is in `sdk`); targeted runs: `-p g-mesh-plugin-sdk-lsp`,
`-p g-mesh-plugin-sdk`, the three plugins' `--lib`, `core` `--test scripts`
(`test_select_scripts`, `test_sections_scripts`, `cut_release_plugin_versions`).

### 2.3 Merge contacts

| File | This task | Other |
|---|---|---|
| `plugins/sdk/src/lsp/bridge.rs` | S2 split, S3 move | none known |
| `plugins/sdk/src/lsp/config.rs` | S3 move + `crate::manifest` -> sdk path | **GM-711** rewrites `RawManifest` |
| `plugins/sdk/src/manifest.rs` | S3 publishes `manifest_path` | **GM-711** (shared schema) likely |
| `plugins/sdk/src/run.rs` | S3 one line (370) | **GM-711** control handling |
| `plugins/sdk/src/lib.rs` | S3: drop `pub mod lsp`, two hidden re-exports, `lifeline` | GM-711 may add a schema module/re-export |
| `scripts/test-select.sh`, `scripts/test-sections.sh` | S3 one row + header; one filter line | **GM-712** |
| `core/tests/scripts/test_select_scripts.rs`, `test_sections_scripts.rs` | S5 new tests | **GM-712** likely |
| `scripts/cut-release.sh` | S3 two arrays + comment | none known |
| Plugin versions | R-29 and R-28 change `plugins/sdk` `src/` (a shipped input), so `cut-release.sh` demands a rust/python/typescript version bump in 4.33.0 | GM-711 touches sdk too; one bump per plugin per release covers all |

## 3. Behaviours the tests slice must pin, and controls

R-29 is a pure move: a "revert" restores the old file and every test still passes, so it has
**no control**. Its proof is the equal test-name list (2.1) and the unchanged suite.

| # | Behaviour | Test (where) | Control: revert this code, the test must fail |
|---|---|---|---|
| B1 | A server started by `prepare` (and by a pass) is killed when the lifeline hooks run | `plugins/sdk-lsp/tests/lsp_prepare_reaped.rs`, calling `g_mesh_plugin_sdk::run_lifeline_exit_hooks()` instead of `kill_live_servers()` | delete the `Once` registration in `LspClient::start` |
| B2 | Every registered hook runs once per `run_lifeline_exit_hooks`; a hook registered from inside a hook does not deadlock | sdk unit test in `lifeline.rs` (counters; no order assertion beyond what the `Vec` guarantees, and only within one thread) | make `run_lifeline_exit_hooks` a no-op; for the deadlock half, call hooks while holding the lock |
| B3 | The control-stream lifeline still runs the hooks before `exit` | source-text pin in sdk (as `rust/semantic.rs:1202` does): `read_control_stream`'s body contains `run_lifeline_exit_hooks()` before `process::exit`. An end-to-end test (a test plugin binary with the fake server, stdin closed mid-request, server pid gone after `LIFELINE_GRACE`) is stronger but needs a new `[[bin]]`; tests slice decides, recommendation: source-text pin | remove the call at `run.rs:370` |
| B4 | `plugins/sdk-lsp/*` selects `sdk` + three plugin sections (+ `core-spawns-*` with `--narrow`, + every core section without); `plugins/sdk/*` still selects `full` | `core/tests/scripts/test_select_scripts.rs`, both modes, plus `plugins/sdk-lsp/Cargo.toml` with `--narrow` | delete the `plugins/sdk-lsp/*` row (falls to `full`) |
| B5 | The `sdk` section holds the sdk-lsp package's tests | `test_sections_scripts.rs`: `filter_of("sdk")` equals the two-package filter; `scripts/test-sections.sh check` in verify | revert `test-sections.sh:110` (sdk-lsp tests in no section, `check` fails) |
| B6 | A change under `plugins/sdk-lsp/src` counts as a plugin change in `cut-release.sh` | `cut_release_plugin_versions.rs` (a fixture commit touching `plugins/sdk-lsp/src/x.rs` without a plugin bump is refused) | drop `plugins/sdk-lsp` from `PLUGIN_SHARED_CRATES` |
| B7 | An sdk-lsp version that differs from core's is refused | `cut_release_plugin_versions.rs` or the workspace-version test beside it | drop the entry from `OTHER_WORKSPACE_MANIFESTS` |

Seven controls (B2 counts once). B1 must run in its own test binary (it kills every server the
process started); it already is one. Repeats: B1 and B2 start processes/threads, so 5x in the
tests slice and 5x in verify.

Structural acceptance (verify, by grep, not tests): `grep -rn "crate::lsp\|mod lsp"
plugins/sdk/src` empty; `plugins/sdk/Cargo.toml` names no sdk-lsp; `bridge.rs` declares the
five modules and holds none of the moved items.

## 4. Re-slicing

Draft kept, with order and one model change:

| Slice | Model | Scope | When |
|---|---|---|---|
| S2 R-29 | sonnet | 2.1 | now; merge into release-4.33.0 before GM-711's `run.rs` edits |
| S3 R-28 | opus | 2.2 | after GM-711 **and** GM-712 are merged into the release (D9); rebase the task branch first |
| S4 review | opus | diff of S2+S3 against this note; D7 consumers, D5 hook, scripts | after S3 |
| S5 tests | sonnet | section 3 B1-B7 | after S4 |
| S6 verify | opus | controls in one throwaway worktree, 5x B1/B2, suite `-p` sdk, sdk-lsp, three plugins, core `--test scripts` | after S5 |

If S3 has to wait long for GM-711/GM-712, S2 can be merged and the task parked between S2 and
S3; nothing in S2 depends on S3.
