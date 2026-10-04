# GM-515: a batch's created files reach the project model together

Status: design (GM-515/S1), for owner review. No production code yet.

## Problem

ADR 0023 keeps `Extractor::extract` pure and keeps the project model's
file-existence set current through `Extractor::file_presence_changed`, called
by the SDK on every `fileChanged` before extraction. GM-505 routes a drained
debounce batch deletions -> creations -> modifications
(`core/src/watcher/batch.rs`, `daemon::watch_and_route_once`), which closes W1
for *modified* importers. It leaves one window: two files created in the same
batch, the importer routed first. When the importer is extracted the model has
not yet heard of its target, so the import stays unresolved until the next edit
of the importer. Routing the creations twice does not help: the SDK's
`Session::file_changed` answers an unchanged re-sent file with an empty diff
before extracting it.

Wanted: core announces a batch's created paths together; the plugin applies
every presence change before extracting any of them; each created file is
extracted once.

## Decisions

### D1. A new `filesCreated` notification, followed by the ordinary per-file `fileChanged`s

Core sends one `filesCreated { filePaths }` **notification** (no `id`, no
response) per language, carrying that language's created paths of the batch,
and then routes every created path through the existing per-file `fileChanged`
round trip, exactly as GM-505 does today. The plugin applies presence for every
listed path when the notification arrives; the `fileChanged`s that follow each
extract their file once, against a model that already holds all of them.

Why a notification and not a batched request:

- Nothing on core's apply side changes. `watcher::apply::round_trip`, the
  per-file `FileScope` (`complete`/`Gone`/`Partial`, `file_scope`), the
  per-file `store.unit(Unit::WatcherApply, ..)`, the per-file semantic pass,
  `PluginProcess`'s pending queue and crash replay, and the sleeping
  supervisor's `DirtyQueue` all keep working one path at a time.
- Failure isolation is the one we have: one bad file costs its own round trip,
  never its siblings' diffs. A combined response would need a per-file error
  channel to say the same thing.
- It has a precedent with the same shape: `prepareSemanticPass` is a
  capability-gated, best-effort notification sent with `write_message` and no
  read (`PluginProcess::notify_prepare_semantic_pass`).
- Presence is a fact about the disk, not about the index, so it does not need
  an acknowledgement. If it is lost (dead process), the relaunched process
  builds its model with `load_project`, which reads the disk after the batch:
  every created file is already present there.

Rejected:

- **`filesChanged { filePaths }` request answered with per-file diffs.** It
  would save N-1 round trips on a large burst, but needs a new response type
  (`Vec<{filePath, diff}>`, since `FileChangeDiff.complete` and core's
  `FileScope` are per file and one combined diff cannot carry them), a batched
  `apply_file_diff_linked`, a timeout that scales with batch size, a pending
  queue that knows batches, and a per-file error channel. All of that is risk
  that buys latency, not correctness. Can be revisited as its own task if a
  measurement shows per-file round trips matter on bursts.
- **Extend `fileChanged` with `alsoCreated: [..]` on the first creation.** Old
  plugins would ignore the field for free, but one request would then mean two
  things (extract this file; record presence of others), core's crash replay
  would re-send the extra list with an unrelated path, and plugin authors could
  not tell from the method which contract they are on.
- **Route creations twice.** Rejected in ADR 0023 already: the second send is
  a no-op in the SDK.

### D2. Capability in the manifest: `capabilities.files_created`

`[plugin.capabilities] files_created = true` declares that the plugin
understands `filesCreated`. Default `false`: core never sends it, and the
plugin gets exactly today's per-file `fileChanged` sequence (the degradation
path). Manifest rather than handshake, for the reason `Capabilities` already
states: core needs it without a plugin process, and every other "may core send
X" bit lives there. A handshake field would change the `Handshake` type and be
needed by every third-party plugin.

No `CURRENT_PROTOCOL_VERSION` bump: the change is additive and gated, as
`prepareSemanticPass` was.

Who declares it:

- SDK-based plugins handle `filesCreated` for every `Extractor` (S3), but
  declaring it is only worth a frame when the extractor overrides
  `file_presence_changed`. Rust uses the default: does not declare. Python
  declares it with GM-506 (its hook implementation). The TypeScript Rust port
  declares it with GM-324.
- The Node TypeScript plugin does not declare it and is not changed.
- Go does not declare it (D7) but handles the method, so a manifest edit alone
  cannot wedge it.

### D3. When core sends it

Per drained batch, after every deletion has been routed and before the first
creation's `fileChanged`:

1. Group the batch's `Created` paths by the language that would receive their
   `fileChanged`, with the same filters `PluginRegistry::file_changed` applies:
   not a workspace file (`workspace_language_matches` non-empty means it is
   routed as a reindex, not a reparse), claimed (`language_for`), not under
   that language's `exclude_dirs` (`discovered.indexing_language`), not a
   failed language.
2. For each language with **at least two** such paths, whose manifest declares
   `files_created`, and whose supervisor is **running and awake**: send
   `filesCreated` with those paths in batch order.
3. Route every path of the batch with `route_settled_path`, unchanged.

Skipped, deliberately:

- A language with one created path: the window needs two new files; the hook
  on that file's own `fileChanged` already covers a modified importer of it.
- A language whose plugin is not spawned: spawning it reads the disk in
  `load_project`, after the batch, so it already sees every created file.
  `filesCreated` never spawns a plugin.
- A sleeping plugin: its `fileChanged`s go to the `DirtyQueue`; the wake spawns
  a fresh process whose `load_project` reads the disk. The replay is per file
  and needs no presence message. Same for `PluginProcess`'s crash replay.

A failed send (a dead process) is logged and dropped; the per-file routing
that follows meets the same dead process and takes the existing crash path.

### D4. Deletions and modifications do not ride in the message

- **Deletions:** every deletion in the batch is already routed before any
  creation or modification, and the SDK applies `present = false` on that
  deletion's own `fileChanged` (D5) before any extraction in the batch. A
  batched deletion message would change nothing observable.
- **Modifications:** a modified file's presence does not change.

So the message names only created paths, and its name says so. If a later
window needs batched deletions, a `filesDeleted` notification is the additive
extension; a generic `presenceChanged { created, deleted }` is not worth the
second list today.

### D5. The SDK hook, and where the SDK calls it

ADR 0023 says the hook runs on every `fileChanged` (and per hydrated file),
before extraction. That stays true, and `filesCreated` is one more caller:

- `Extractor::file_presence_changed(&self, project: &mut Self::Project, path: &RelPath, present: bool)`,
  default no-op. Contract: **idempotent** (a created file is announced by
  `filesCreated` and again by its own `fileChanged`), called only for paths
  the plugin claims, never from the bulk walk, and never sees a path outside
  the root.
- `Session::file_changed`: after the GM-349 remap and the `claims` check, and
  after reading the source, call the hook with `present = source.is_some()`
  (an unreadable file is treated as gone, as the diff already treats it). It
  runs **before** the unchanged-text short-circuit, so a short-circuited
  `fileChanged` still records presence (harmless: it can only re-assert
  `present = true` for a file the index holds). Only when `self.project` is
  `Some`: a missing model is rebuilt from disk on the next load, which already
  has the right presence.
- `Session::files_created(paths)` (new, for the notification): for each path,
  remap (D6), skip unclaimed, then call the hook with `present` = the path is a
  readable file **now** (it may have been deleted since core drained the
  batch; its own `fileChanged` follows and agrees). Extracts nothing. Each
  hook call is wrapped in `catch_unwind` like `extract_caught`: a panicking
  hook costs that path's presence, never the batch or the process.
  Acknowledged if it arrives with an `id`; silent otherwise.
- `Session::hydrate`: hook with `present = true` per hydrated file before its
  extraction (ADR 0023's "per hydrated file"). Idempotent, since `load_project`
  already saw these files.

Interaction with the unchanged-text short-circuit: none that loses work. A
`Created` path has no `indexed_files` baseline, so the SDK holds at most an
*unreported* (hydrated) entry for it, which the short-circuit does not
match. Presence is applied outside the extraction path, so the short-circuit
cannot swallow it.

### D6. GM-349 link spellings in `filesCreated`

`Session::indexed_spelling` remaps a link spelling to its real spelling only
when the real one is indexed. For a batch, the real spelling may be created in
the same batch and not yet indexed, so the link spelling would enter the
existence set although the walk (ADR 0025) only ever lists the real one.
`files_created` remaps a path to its real in-root spelling when that real
spelling is **indexed or in the same notification**; otherwise it handles the
path as spelled (the walk's own rule for files reachable only through links).
`file_changed` keeps today's remap unchanged.

### D7. Go plugin

Go's structural tier never asks the disk whether a file exists (imports
resolve to packages through the module layout, `workspace.go`), so it has no
model to update. `handleEnvelope` gains a `filesCreated` case that logs and
acknowledges when the frame has an `id`, and does nothing for a notification.
Today an unknown method with an `id` gets no answer from Go at all, which would
wedge core's stream until its timeout; the explicit case closes that for this
method. Go's manifest does not declare `files_created`.

### D8. ADR record

New **ADR 0026** "Created files of one batch reach the plugin as a
`filesCreated` notification" (0024 and 0025 are taken). It records D1-D4 and
the capability. ADR 0023's W1 bullet is amended to "closed for created
importers by ADR 0026"; the remaining open window (a target and importer
settling in different batches) stays named there. Reason for a new ADR rather
than only an amendment: it adds a wire method and a manifest key, which are
their own decisions and the place a plugin author looks.

## Wire shape

```rust
// wire/src/lib.rs, enum ControlMessage
/// Tells a plugin that every listed file was created in one watcher batch,
/// before any of them is sent as `fileChanged`, so its project model knows
/// all of them before it extracts the first (ADR 0026). A notification:
/// nothing is answered, and each file still gets its own `fileChanged`.
/// Sent only to a plugin whose manifest declares `capabilities.files_created`.
#[serde(rename_all = "camelCase")]
FilesCreated {
    file_paths: Vec<String>,
},
```

On the wire: `{"jsonrpc":"2.0","method":"filesCreated","params":{"filePaths":["src/a.ts","src/b.ts"]}}`.

```rust
// core/src/daemon/manifest.rs, struct Capabilities
/// Whether core may send this plugin a `filesCreated` notification before the
/// `fileChanged`s of a batch's created files (ADR 0026). `false` (the
/// default): never sent; the plugin sees only per-file `fileChanged`.
pub files_created: bool,
```

```rust
// plugins/sdk/src/lib.rs, trait Extractor
/// Records that `path` now exists (`present`) or no longer does, so `extract`
/// can resolve against the project's current file set without reading the
/// disk (ADR 0023). Called before any extraction it affects; never from the
/// bulk walk. Must be idempotent: one creation may be reported more than once.
fn file_presence_changed(&self, _project: &mut Self::Project, _path: &RelPath, _present: bool) {}
```

## Edit map

Line numbers are at `6776fd2`.

### S2: wire + core (opus)

Change:

- `wire/src/lib.rs:538-592` `ControlMessage`: add `FilesCreated`. Add a
  round-trip test beside the `PrepareSemanticPass` one (~`:1060-1075`).
- `core/src/watcher/apply.rs:533-542` `method_name`: add the arm (exhaustive
  match).
- `core/src/daemon/manifest.rs:122-158` `Capabilities`: add `files_created`.
  Then every `Capabilities { .. }` literal without `..Default::default()`
  fails to compile; fix each: `core/src/mcp/semantic_pending_tests.rs:26`,
  `core/src/mcp/provenance.rs:378`, `core/src/mcp/instructions/tests.rs:45,65,75,93,1073,1091,1205`,
  `core/src/mcp/untyped_tests.rs:322`, `core/src/mcp/find_callers_callees.rs:711`,
  `core/src/cli/plugins.rs:609`. Manifest parse test in
  `core/src/daemon/manifest/tests.rs`.
- `core/src/daemon/plugin.rs:1439-1453`: add
  `PluginProcess::notify_files_created(&self, paths: &[String]) -> Result<bool>`,
  modelled on `notify_prepare_semantic_pass` (gate on
  `manifest.capabilities.files_created`, `write_message` a no-id envelope).
- `core/src/daemon/lifecycle.rs:397-405`: add
  `PluginSupervisor::files_created(&self, paths: &[String]) -> Result<bool>`,
  modelled on `prepare_semantic_pass`: asleep -> `Ok(false)`, else `touch()`
  and notify. Never spawns, never queues.
- `core/src/daemon/registry.rs` near `route_settled_path` (`:924-937`) and
  `file_changed` (`:965-995`): add
  `PluginRegistry::announce_created(&self, created: &[String])` per D3. Use
  `active_supervisors` (`:1083`) or the `supervisors` map directly (running
  slots only); never `get_or_spawn`. Reuse the filters of `file_changed`
  (`language_for`, `discovered.indexing_language`, `is_failed_language`) and
  `workspace_language_matches` (`:687`).
- `core/src/watcher/batch.rs:50-53` `order_for_routing`: keep kinds available
  to the caller (e.g. return `Vec<(SettledKind, T)>` sorted, or add a helper
  that splits off the `Created` run). Update `core/src/watcher/batch/tests.rs`
  callers.
- `core/src/daemon/mod.rs:470-497` `watch_and_route_once`: route deletions,
  then `registry.announce_created(&created)`, then creations and
  modifications, each via `route_settled_path` as today. Update the function
  doc and `batch.rs`'s module doc (invariant: created paths are announced,
  per language, before the first of them is routed).
- `core/src/daemon/test_plugin.rs:438-446`: add
  `declare_files_created(plugin_dir)` like `declare_semantic_prepare`;
  `notifications()` (`:424`) and `frames()` (`:431`) already record what the
  tests need, but the fake's entry point (`:730-733`) logs only
  `params.filePath`: extend it to join `params.filePaths` so a `filesCreated`
  line names its paths.

Read only: `core/src/watcher/apply.rs:87-196,400-466` (`apply_file_change`,
`round_trip`), `core/src/daemon/plugin.rs:1093-1220` (pending queue),
`core/src/daemon/lifecycle.rs:123-160,294-375` (`DirtyQueue`, `file_changed`,
`replay_pending`), `core/src/daemon/tests.rs:244,414-520`
(`registry_over`, `route_one_batch`, GM-505 tests).

Docs in this slice: none (S5).

### S3: SDK hook + batch (opus)

Change:

- `plugins/sdk/src/lib.rs:137-175` `Extractor`: add `file_presence_changed`
  with the default no-op and the contract of D5.
- `plugins/sdk/src/run.rs:394-505` `Session::handle`: add the `"filesCreated"`
  arm (parse `params.filePaths`, skipping non-string entries), call
  `files_created`, then `acknowledge(out, id)`.
- `plugins/sdk/src/run.rs`: new `Session::files_created` (D5, D6) and a
  `presence_caught` helper beside `extract_caught` (`:730-750`).
- `plugins/sdk/src/run.rs:534-595` `Session::file_changed`: the hook call per
  D5, after `read_source`, before the short-circuit.
- `plugins/sdk/src/run.rs:507-532` `Session::hydrate`: the hook per hydrated
  file, before extraction.
- `plugins/sdk/src/run.rs:597-602` `indexed_spelling`: factor the
  canonicalize step so `files_created` can apply D6 ("indexed or in this
  notification") without a second canonicalize per path.

Read only: `plugins/sdk/src/index.rs` (`SdkIndex::entry`, `baseline`,
`insert_unreported`), the test helpers in `run.rs:1003-1135`
(`Declares`, `request`, `fresh_session`, `toy_spec`) and the GM-349 tests
`run.rs:1247-1343`.

### S4: Go (opus: small, but it has a `go test` loop)

Change:

- `plugins/go/control.go:257-340` `handleEnvelope`: `case "filesCreated":`
  log the count, fall through to the shared ack (`writeAck` when `hasID`).
- `plugins/go/wire.go:290-296`: reuse `filePathsParams` for the log line; no
  new type needed.

Read only: `plugins/go/control_test.go:325-406` (`frameOf`, `readFrames`,
`TestRunControlLoopEndToEnd`).

### S5: docs (sonnet)

- `docs/adr/0026-batch-created-files-notification.md` (D8).
- `docs/adr/0023-project-model-tracks-file-presence.md`: the W1 bullet in
  Consequences.
- `docs/architecture/multi-language-plugins.md`: the Wire v2 method list
  (`:594`, beside `prepareSemanticPass`) and the manifest schema (`:401`).
- `docs/architecture/plugin-modularity.md:248` capability list.

Comment rule for S2-S4: comments state invariants only, link ADR 0023/0026,
never ticket ids.

## Behaviours for the tests slice

Each with the control that must make it fail (built in a throwaway worktree).

Core (`core/src/daemon/tests.rs`, `test_plugin`):

1. **A declaring plugin gets one `filesCreated` before the batch's created
   files.** Batch fed modified -> created x2+ -> deleted; expected frames:
   deleted `fileChanged`s, then one `filesCreated` naming every created path,
   then the created `fileChanged`s, then the modified ones; each path in
   exactly one `fileChanged`. *Control:* remove the `announce_created` call in
   `watch_and_route_once`.
2. **A plugin that does not declare it sees only per-file `fileChanged`, in
   GM-505's order.** Same batch, no `files_created`; no `filesCreated` in
   `notifications()`, request order as today. *Control:* drop the capability
   gate in `notify_files_created`.
3. **One created file sends no notification.** *Control:* lower the threshold
   to one.
4. **A sleeping plugin is neither woken nor told; its queue replays every
   created file.** Put the supervisor to sleep (`sleep_now`), route the batch,
   assert no new spawn and no `filesCreated`, then `replay_pending` sends each
   created path once. *Control:* make `PluginSupervisor::files_created` call
   `get_or_spawn`/wake (spawn count rises).
5. **Workspace files, excluded-dir files and a failed language's files are
   not announced.** *Control:* remove the filter in `announce_created`.
6. **Created files of two languages are announced per language.** *Control:*
   send the whole created list to every language.

Wire and manifest:

7. `FilesCreated` serializes to `method: "filesCreated"`,
   `params.filePaths`, no `id`, and round-trips. *Control:* rename the field.
8. `files_created` parses from `[plugin.capabilities]` and defaults to
   `false`. *Control:* change the default.

SDK (`plugins/sdk/src/run.rs` tests; a test extractor whose `Project` is the
existence set, filled by `load_project` from the walk and kept by the hook,
whose `extract` emits a resolved import edge only when the target is in the
set and counts its calls):

9. **A same-batch new importer of a new file resolves in that batch.**
   Session started on an empty root; then write importer `a` and target `b`;
   frames: `filesCreated [a, b]`, `fileChanged a`, `fileChanged b`. `a`'s diff
   holds the resolved edge; `extract` ran exactly once per file. *Control:*
   empty the `filesCreated` arm (acknowledge only).
10. **Without the notification the importer stays unresolved** (the
    degradation, and the arm 9 is measured against): same files,
    `fileChanged a`, `fileChanged b` only.
11. **`fileChanged` applies presence before extracting.** A modified importer
    after `fileChanged` of a newly created target resolves; a deleted target's
    `fileChanged` makes the next importer extraction unresolved. *Control:*
    remove the hook call in `file_changed`.
12. **A listed path no longer on disk is applied as absent.** *Control:*
    hardcode `present = true` in `files_created`.
13. **One bad entry does not cost the others.** A list with a non-string, an
    unclaimed extension and a path whose hook panics, plus two good paths: the
    good ones are present, the process answers the next request. *Control:*
    return on the first bad entry.
14. **A link spelling whose real spelling is in the same notification records
    only the real spelling** (D6). *Control:* skip the remap in
    `files_created`.
15. **`filesCreated` with an `id` is acknowledged; without one nothing is
    written.** *Control:* remove the `acknowledge` call.

Go (`plugins/go/control_test.go`):

16. **`filesCreated` with an `id` is acknowledged, as a notification it
    writes nothing, and the next `fileChanged` answers with its own id.**
    *Control:* remove the case (the request gets no answer).

## Risks and trade-offs

- **Two kinds of presence report for one file.** A created file is announced
  and then reported again by its `fileChanged`; correctness rests on the
  hook being idempotent. Stated in the trait doc; test 9 counts extractions,
  not hook calls.
- **The notification can be stale by the time it arrives.** The plugin checks
  the disk (D5), and the file's own `fileChanged` follows and agrees.
- **No acknowledgement means core cannot tell a plugin that dropped it.** A
  declaring plugin that ignores the method silently keeps today's behaviour;
  `g-mesh plugins check` does not cover it yet (Q3).
- **Still open:** a target and its importer whose last events settle in
  different batches (a burst longer than the debounce window); a tool call's
  `ensure_fresh` reparsing a created importer before the watcher routes its
  batch. Both stay named in ADR 0023.
- **`Capabilities` literals.** Adding a field touches ~11 test literals in
  core; mechanical, but the reason S2's diff is wider than the feature.
- **Nothing in production declares the capability until GM-506 / GM-324.**
  The behaviour is proven by tests only in this task (Q1).

## Owner questions

1. **Who flips Python's manifest to `files_created = true`?** Recommend: GM-506,
   which implements Python's hook; if GM-506 is already merged into
   release-4.0.0 when S2 runs, S2 flips it and adds Python to test 1's
   languages.
2. **Threshold: announce only when a language has two or more created paths,
   or on every creation?** Recommend two or more: one path cannot form the
   window, and it keeps the frame count of an ordinary new-file save
   unchanged.
3. **Should `g-mesh plugins check` exercise `filesCreated` for a declaring
   plugin** (stream stays in sync, next request answered)? Recommend a
   backlog task, not this one.
4. **ADR: new 0026 plus an 0023 amendment (D8), written in a docs slice after
   the code?** Recommend yes.

## Appendix: g-mesh calls this note relied on

- `find_callers route_settled_path` -> only production caller
  `daemon::watch_and_route_once` (`core/src/daemon/mod.rs`); the rest are
  tests in `daemon/workspace_reindex.rs` and `daemon/registry/tests.rs`.
  Complete (`hasMore: false`).
- `find_callers apply_file_change` -> ambiguous; re-queried by id:
  - `daemon::plugin::PluginProcess::apply_file_change`: production callers
    `PluginSupervisor::file_changed` and `PluginSupervisor::replay_pending`
    (`core/src/daemon/lifecycle.rs`); tests in `daemon/plugin/tests.rs`,
    `core/tests/{incremental_matches_full_reindex,plugin_crash_recovery,repeated_edits_through_a_warm_plugin}.rs`.
  - `watcher::apply::apply_file_change`: production callers
    `PluginProcess::send_one` and `cli::plugin_check::session::Driver::step`;
    the rest are `watcher/apply/tests.rs`.
- `find_callers apply_file_change_diff` -> no such symbol (it is the import
  alias of `watcher::apply::apply_file_change` in `daemon/plugin.rs`); the
  previous call answered it.
- grep (one known symbol or non-code): `ControlMessage::PrepareSemanticPass`
  (the exhaustive `method_name` match), `Capabilities {` literals,
  `semantic_prepare` (the precedent), the `fileChanged` dispatch in
  `plugins/go/control.go` and `plugins/sdk/src/run.rs`, and the
  `plugin.toml` capability keys.
