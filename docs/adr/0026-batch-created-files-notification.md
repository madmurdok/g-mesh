# 0026. Created files of one batch reach the plugin as a `filesCreated` notification

## Status
Accepted 2026-10-05 (owner review of the design note
[`gm-515-batch-presence.md`](../architecture/gm-515-batch-presence.md); this
text describes what was built). Builds on
[ADR 0023](0023-project-model-tracks-file-presence.md).

## Context
ADR 0023 keeps `Extractor::extract` pure and updates the project model's file
set through `file_presence_changed`, which the SDK calls on each `fileChanged`.
Core routes a drained debounce batch deletions, creations, modifications, which
closes the window for modified importers. It leaves one: two files created in
the same batch, the importer routed first. When the importer is extracted the
model has not heard of its target, so the import stays unresolved until the
importer's next edit. Routing the creations twice does not help, because the
SDK answers an unchanged re-sent file with an empty diff before extracting it.

## Decision
Core sends one `filesCreated { filePaths }` **notification** (no `id`, no
response) per language, then routes every created path through the ordinary
per-file `fileChanged`, as before. The plugin applies presence for all listed
paths on the notification; each following `fileChanged` extracts its file once,
against a model that already holds all of them.

**When core sends it** (`PluginRegistry::announce_created`, called from
`daemon::watch_and_route_once` after the deletions and before the first
creation). Created paths are grouped by the language that would receive their
`fileChanged`: workspace files (routed as a reindex), paths under the
language's `exclude_dirs`, unclaimed paths and failed languages are left out.
A language is told only when:
- it has **at least two** such paths (one path cannot be both importer and
  target; a modified importer of it is covered by its own `fileChanged`);
- its manifest declares `[plugin.capabilities] files_created = true`
  (default `false`; checked in `PluginProcess::notify_files_created`);
- its supervisor is running and awake. A sleeping or unspawned plugin is
  neither woken nor queued: its wake or spawn builds the model with
  `load_project`, which reads the disk after the batch and already sees every
  created file, and the `DirtyQueue` replays each file per path.

A failed send is logged and dropped; the per-file routing meets the same dead
process on the existing crash path. No `CURRENT_PROTOCOL_VERSION` bump: the
change is additive and gated, like `prepareSemanticPass`.

**SDK hook contract** (`Extractor::file_presence_changed`, no-op default):
- idempotent: a created file is reported by `filesCreated` and again by its own
  `fileChanged`, and a hydrated file is reported although `load_project`
  already saw it;
- called only for paths the plugin claims, never from the bulk walk, and never
  with a path outside the root (root-guarded by the SDK);
- a panic is caught and costs that one path's presence.

The SDK calls it from:
- `filesCreated`: per listed path, with `present` read from the disk now
  (readable UTF-8, the test `read_source` applies), since the file may be gone
  again by arrival. Nothing is extracted. A non-string entry, an unclaimed path
  or a panicking hook costs only itself. A request with an `id` is
  acknowledged, a notification writes nothing. With no project model nothing
  is applied: the next load reads the disk;
- `fileChanged`: after reading the source, **before** the unchanged-text
  short-circuit and before extraction, so it runs even when extraction fails;
  `present` is whether the source was readable;
- `hydrate`: per hydrated file, before its extraction.

A link spelling in `filesCreated` is remapped to its real spelling when that is
indexed **or listed in the same notification** (the real file may be created in
this batch), so the existence set holds only the spelling the walk lists
([ADR 0025](0025-project-walk-follows-symlinks.md)).

**Go** handles `filesCreated` without declaring the capability: its structural
tier never asks the disk whether a file exists, so it logs and acknowledges a
request and ignores a notification. The explicit case keeps a manifest edit
from leaving a request unanswered.

Deletions and modifications do not ride in the message: deletions are routed
first and applied on their own `fileChanged`, and a modified file's presence
does not change.

## Consequences
- The same-batch created-importer window of ADR 0023 (W1) is closed for
  plugins that declare `files_created` and override the hook. Still open: a
  target and importer settling in different batches (a burst longer than the
  debounce window).
- No acknowledgement: core cannot tell a declaring plugin that ignores the
  method. Such a plugin keeps the per-file behaviour. `g-mesh plugins check`
  does not exercise the method yet.
- Rust uses the default hook and does not declare the capability. The
  end-to-end case through a shipped plugin arrives when Python declares
  `files_created` together with its hook (GM-506); until then the behaviour is
  proven by tests with a fake plugin and a test extractor.
- A batch costs one extra frame per declaring language with two or more
  created files; an ordinary single-file save adds none.
- Adding `Capabilities.files_created` touched every test literal that builds
  `Capabilities` without `..Default::default()`.

## Rejected alternatives
- **Route the creations a second time.** A no-op: the SDK short-circuits an
  unchanged re-sent text before extracting it.
- **A batched `filesChanged` request answered with per-file diffs.** Saves round
  trips on large bursts, but needs a new response type (the completeness flag
  and core's file scope are per file), a batched apply, a size-scaled timeout,
  a batch-aware pending queue and a per-file error channel. That is risk that
  buys latency, not correctness; a separate task if a measurement asks for it.
- **Extend `fileChanged` with `alsoCreated`.** One request would mean two things
  (extract this file; record presence of others), crash replay would re-send
  the extra list with an unrelated path, and the method name would no longer
  tell a plugin author which contract applies.
