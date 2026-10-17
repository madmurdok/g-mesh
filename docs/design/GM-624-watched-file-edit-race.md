# GM-624/S1 diagnosis: source_freshness::an_edit_to_a_file_no_question_touched_reaches_the_answer_about_its_caller

Worktree g-mesh-wt-gm624 @ c7ec432 (release-4.11.0 bump), no code changes. Diagnosis only.

## Repro under load: NOT reproduced, 0/21
- Script scratchpad/gm624s1/run.sh, logs run0..run20.log, summary.txt in the same dir.
- Test binary run directly (`<bin> <test> --exact --nocapture`, cwd plugins/rust), judged by rc=0 + "test ... ok" + "1 passed".
- run 0 (no burners, judge dry run): pass, real 17.2 user 8.4 sys 2.9, load 42.
- runs 1-20 with 8 `yes` burners (ncpu=8) on top of the machine's own load: 20/20 pass,
  1-min load 42-72 (5-min 43-56), real 10.5-18.9 s, user 8.2-8.8, sys 2.6-3.0 (CPU-bound, not waiting).
- Combined with GM-602/S3 (33 passes, load 155-656) and S2 (13 passes): 1 failure in 67 runs, ever.
  The rate is too low to reproduce on demand. Load alone does not trigger it.

## What the test waits on after the edit: nothing of its own
Child `..._in_its_env` (plugins/rust/tests/semantic_runs_no_project_code.rs, mod source_freshness,
lines ~1276-1310): first `engine.answer(&[], &before)`, then rewrites util.rs on disk, `index_of`,
then calls `engine.answer(&[util.rs, main.rs], &after)` synchronously and asserts the result. No
sleep, no retry, no notification wait. So freshness is entirely the plugin's job: the test does
not ask too early, because the API it calls claims to return a fresh answer.

## The racing wait (plugin, sdk/src/lsp)
Found with g-mesh (project g-mesh-wt-gm624): find_definition `LspBridge::answer`,
`LspBridge::forward_watched`, `LspBridge::wait_ready`, `LspBridge::sync_documents`,
`LspClient::settle`, `LspClient::mark_edited`, `LspClient::quiet_for`.

Second-pass sequence in `LspBridge::answer` (plugins/sdk/src/lsp/bridge.rs 3051-3238; the sync block is 3129-3150):
1. util.rs is not open on the server (it has no call site), so `forward_watched` (bridge.rs 841-866)
   sends `workspace/didChangeWatchedFiles {util.rs: Changed}` + `mark_edited()`.
2. `sync_documents` re-syncs main.rs only (already open, same hash -> nothing sent).
3. `wait_ready` (bridge.rs 873-900) -> `LspClient::settle` (client.rs 453-463): the latch is already
   set from pass 1, so it returns `quiet_for(Duration::ZERO)`, which is "nothing in flight right
   now" (client.rs 369-371). `mark_edited` (client.rs 406-410) only resets `idle_since`, and that
   affects nothing at ZERO. It matters only for GM-309's re-ask of *empty* answers.
4. `run_pass` asks textDocument/definition right away.

rust-analyzer side (checked upstream source, crates/rust-analyzer/src/handlers/notification.rs and
main_loop.rs; no local RA source): `handle_did_change_watched_files` only calls
`state.loader.handle.invalidate(path)`. The VFS loader thread re-reads the file and later sends
`vfs::loader::Message::Changed` to the main loop, which then applies `set_file_contents`. This
sends no `$/progress` and no `experimental/serverStatus quiescent:false`. A request dispatched
before that message arrives gets a snapshot with the OLD util.rs. The answer then has the old
position (line of `Thing::method` in v1), which in the edited index is `Other::method`. That is
exactly S2's failure message.
In contrast, `didOpen`/`didChange` set contents synchronously in the notification handler, so they
are applied before any later request (LSP messages are handled in order).

## Verdict: plugin bug (latent race), not a test bug
The bridge treats a sent `didChangeWatchedFiles` as applied, and there is no acknowledgement or
signal that it was. The answer comes back *complete* and non-empty, so neither GM-309's deferral
(empty answers only) nor readiness catches it. The window is RA's loader-thread round trip, usually
sub-millisecond and lost by the request only rarely, which matches 1/67.
Evidence level: from code reading on both sides plus S2's exact symptom. No run reproduced it here,
so it is not proven by repro. This applies to every LSP bridge that forwards watched files (SDK
code), not only Rust.
Second, deterministic defect on the same path: RA reads util.rs from DISK, while every question
position comes from the INDEX text. `sync_documents`' doc names this mismatch as the thing it rules
out ("a definition mapped onto whatever declaration has since drifted into that line"), but the
watched path reintroduces it whenever disk moves on past the index.

## Proposed acknowledgement-based fix (for the implement slice to decide)
A (recommended): make the edit synchronous. In `LspBridge::answer` (bridge.rs ~3133-3135), route
every pending forwarded file that IS in the index through `sync_documents` (didOpen with the index
text) instead of `forward_watched`. RA applies a didOpen before the next request, so the answer
reflects the edit when the pass sends its request. No wait, no new timeout.
  Edit map: `LspBridge::forward_watched` (bridge.rs 841-866): skip paths with `index.entry(path)`
  (needs `index` param) and return them in the resync set. Keep `didChangeWatchedFiles` for
  deleted files and non-indexed files (Cargo.toml etc., which go through workspace reload and are
  already covered by GM-433's unsettle). The call site in `answer` (3133) already extends `synced`
  with the returned set.
  Risk: every edited file stays open forever ("Nothing is ever closed again", sync_documents doc),
  charged to `[plugin] memoryLimitMb`. That is bounded by the files edited during the session, but it
  is a change to the documented policy. Don't use didOpen+didClose: didClose goes back through the
  async loader invalidate.
B (RA-only): after forwarding, poll `rust-analyzer/viewFileText` for each forwarded file until it
  equals the index text (deadline-bounded). That is a true ack, but RA-specific: it needs a manifest
  knob, a request per file per pass, and a new timeout path.
Not recommended: a sleep or a re-ask, which only narrows the window.

## Control for A
The existing control ("forward nothing") keeps working. Plus one new deterministic variant: in the
child, after `index_of(&root)` for `after`, overwrite util.rs on disk a third time (Thing moved
further down, a different decl on the index's `Thing::method` line), then ask the second pass.
With A, RA holds the index text and the answer is `util::Thing::method`. With the fix reverted
(watched forwarding), RA reads either the v3 disk text or still v1, and the answer is wrong either
way. That fails deterministically with no timing involved, so the control tells the arms apart.
