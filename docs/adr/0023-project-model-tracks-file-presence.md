# 0023. The project model tracks file presence on `fileChanged`; resolution configs are watch files

## Status
Accepted 2026-10-04 (GM-350/S1, owner review). Reasoning, measurements,
the per-plugin check and the test classification:
[`gm-350-ts-resolution-placement.md`](../architecture/gm-350-ts-resolution-placement.md).

## Context
GM-324 ports the TypeScript structural tier to Rust on `plugins/sdk`. The
TypeScript plugin resolves import specifiers against the disk: it stats
candidate files and re-reads tsconfig/package.json/pnpm-workspace.yaml on
every reparse, so a file created mid-session resolves from the next edit of
its importer. The SDK's `Extractor` contract forbids disk access in `extract`,
and its project model (`load_project`) is rebuilt only on `workspaceChanged`.
A load-once file set would make every file created after the plugin started
unresolvable until a restart. On the excalidraw corpus 16% of the last 1,000
commits add a TS file. Python and Rust already have that gap (Python documents
it), so copying them would level TypeScript down.

## Decision
We will add one method to `Extractor`, with a no-op default:
`file_presence_changed(&self, project: &mut Self::Project, path: &RelPath, present: bool)`.
The SDK's control plane calls it on every `fileChanged` (and per hydrated
file), before the file is extracted. The bulk walk never calls it. `extract`
stays pure.

The TypeScript project model holds the existence set (the SDK walk's file
list), workspace packages, tsconfig/jsconfig `paths`, and package.json
`exports`/`imports` maps, all read in `load_project`. TypeScript implements
the hook to keep the existence set current.

Resolution configs become TypeScript watch files:
`watch_files = ["package.json", "tsconfig*.json", "jsconfig*.json", "pnpm-workspace.yaml"]`.
Editing one runs the whole-language reindex (ADR 0008).

Rejected: a load-once model with only `watch_files` (silent, session-long
missing edges after every file creation), and relaxing `extract`'s purity
(breaks the id-stability guarantees and the bulk walk's parallel extraction,
and hides a cache-invalidation rule instead of stating one).

## Consequences
- Rust and Python are unchanged (they use the default). Python can opt in to
  close its own "module created since load" gap. Go and today's Node TS plugin
  do not use the trait.
- A config edit now re-resolves every importer, which today's plugin never
  does, at the cost of a TS reindex per save (~20 s CPU for the Node plugin on
  excalidraw, measured under heavy load).
- Named residual windows: W1, an importer routed before its new target in one
  burst stays unresolved until its next edit (a regression against today;
  closed for modified importers, see below); a config file whose name the
  globs miss is read only at reload; `.gitignore` edits are not re-evaluated
  mid-session.
- W1 closed (GM-505): core routes each drained debounce batch in the order
  deletions, creations, modifications (`watcher::batch`, applied in
  `daemon::watch_and_route_once`). A path is classified by its state at drain
  time, not by event kind (the debouncer coalesces kinds, and FSEvents merges
  create/modify flags): absent on disk is a deletion, present without an
  `indexed_files` baseline a creation, otherwise a modification. Deletions go
  first so a modified importer of a deleted file resolves against a model that
  has already dropped it instead of linking to a file about to disappear.
  Ordering is per batch only, with no added latency and no re-extraction.
  A *created* importer of another file created in the same batch (both are
  creations, so their relative order is the batch's) is closed by GM-515: core
  sends a batch's creations together and the plugin applies every presence
  change before extracting any of them. Re-routing the creations a second time
  was rejected: a re-sent unchanged file is a no-op in the SDK. Still open: a target
  and importer whose last events settle in different batches (a burst longer
  than the debounce window).
- GM-324 implements the trait change, the TS model and the manifest change.
  It does not port `ignorePolicy.ts` (the SDK walk owns it) and inherits
  symlinks from GM-349.

## Resolved at review (owner, 2026-10-04)
- The burst-ordering window (an importer routed before its new target in one
  batch) is fixed in this release: core routes a drained batch's creations
  before its modifications (GM-505).
- Python implements `file_presence_changed` in this release (GM-506).
- Rust's mid-session orphan module and `.gitignore` re-evaluation are backlog
  tasks.
- A watch-file save keeps the whole-language reindex in this release; GM-324
  measures its cost for the Rust port. Re-extracting only the importers a
  config change affects is a backlog task.
