# GM-487: why a cold first pass left `shapes.refused_by_all` without an edge

Decision D5 of `gm-485-local-receiver-types.md` asked why the Rust bridge's
first pass after a wake-up answered `semantic.shapes()` in
`by_semantic_neighbours` (`core/src/mcp/find_definition.rs`) but not
`shapes.refused_by_all` / `shapes.refuses`, and whether those sites are ever
re-asked once rust-analyzer is warm.

**Short answer.** rust-analyzer answered all three. The bridge threw away
the two answers that pointed into *another file*. A plugin process that has
just been started (after a wake-up, an idle exit or a memory suspension) has
only the files core has sent it a `fileChanged` for in its `SdkIndex`.
`semanticPass` hydrates only the pass's own files (`plugins/sdk/src/run.rs:415`,
`Session::hydrate` at :478). `file_at` (`plugins/sdk/src/lsp/bridge.rs:1262-1267`)
drops every location whose file is not in the index: `index.entry(&path).is_some()`.
So the first per-file pass in a fresh plugin process records **same-file
targets only**. `query_shapes.rs`, where `refused_by_all`/`refuses` live, was
not hydrated. `SemanticRung::shapes` is declared in `find_definition.rs` itself,
so its answer was kept. A warm rust-analyzer changes nothing here. The same
pass repeated on a warm server gives the same same-file-only result. The sites
get their cross-file edges only after their *target* files enter the plugin's
index, and their *own* file is passed again after that.

The doc comment on `hydrate` states the wrong assumption outright:
"That is enough for a *per-file* pass" (`run.rs:466`).

## 1. Reproduction

Setup: corpus = `git archive 7468d9b` of g-mesh. Binaries were the branch's
release build (`cargo build --release -p g-mesh -p g-mesh-plugin-rust`):

- `g-mesh-plugin-rust` sha256 `d61a7f193298db9e813412881f026cc316eb2451dd2a2c1a9cb4df1c477e003c`
- rust-analyzer 1.97.1 (8bab26f4 2026-07-14) behind a transparent proxy that
  logs every LSP frame with a timestamp.

I drove the plugin directly over its control protocol: handshake, then
`semanticPass {filePaths: [core/src/mcp/find_definition.rs]}`. This is exactly
what core sends for one replayed file. Each run used a fresh plugin and a fresh
rust-analyzer process. The plugin's idle timeout was not involved (no daemon).
Scripts and logs:
`/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/c24bc942-f4b3-4241-b5c3-4090b6707dbd/scratchpad/gm487/{run.sh,drive.py,summarize.py,summarize2.py,out/r1..r4}`.

| run | target/ | pass | secs | load (1m) before→after | what happened |
|---|---|---|---|---|---|
| r1 | cold (no `target/`) | 1 | 91.0 | 88.4 → 50.6 | "still indexing after 89.5s - this pass asks nothing". `Building compile-time-deps` ran 7.6→90.4s. 0 questions asked, 0 edges |
| r1 | | 2 (+20s) | 11.3 | | all asked, 19 edges (10 semantic + 9 re-sent structural) |
| r1 | | 3 (+20s) | 0.4 | | identical to pass 2 |
| r2 | warm | 1 | 39.2 | 36.1 → 25.3 | ready at ~38s. 488 definition requests, 0 empty answers. 19 edges |
| r2 | | 2 (+5s) | 0.2 | | identical |
| r3 | warm | 1 / 2 | 36.0 / 0.4 | 25.5 → 42.3 | same as r2 |
| r4 | warm | see below | | 14.4 → 13.9 | |

`real` is the driver's own wall time (`user` ≤ 3.3s). The waiting is
rust-analyzer's startup: `Building compile-time-deps`, `Loading proc-macros`
and `Indexing` in the LSP log.

In every pass that asked anything, rust-analyzer answered all three sites with
a location, at the right target:

```
def semantic.shapes()     {line 879, char 26} -> find_definition.rs (SemanticRung::shapes)
def shapes.refused_by_all {line 880, char 14} -> query_shapes.rs
def shapes.refuses        {line 907, char 58} -> query_shapes.rs
```

**So the partial answer is not rust-analyzer's.** On 7468d9b the three calls
now also carry structural edges. GM-485's typed locals (3162cbd, merged after
the original observation) make the semantic answer an agreement (GM-489 R2).
That is why r1-r3 show no difference at these exact sites. r4 shows the
mechanism on the file's remaining untyped sites:

| r4 op | semantic edges recorded | LSP answers: same-file / other project file / external (std, deps) |
|---|---|---|
| pass 1, cold plugin, `find_definition.rs` | **10** | 17 / 49 / 178 |
| pass 2, same, warm r-a | **10** | 17 / 49 / 178 |
| `fileChanged` on all 274 `.rs` files (2.45s) | | |
| pass 3, same file | **51** | 17 / 49 / 178 |

The server's answers are identical in all three passes. Passes 1 and 2 record
edges only to declarations in `find_definition.rs` itself (`DefinitionNode::with_source`,
`Lookup.key/languages/admits`, `Resolved.by/node/queried_as`). Pass 3 adds 41
edges into other files (`EmbeddingPipeline::embed_query`, `Page.results`, ...).

**Matches the original incident.** The live daemon log
(`~/.g-mesh/projects/959ade85d9a343b1/daemon.log:2384-2390`) reads: "waking
the rust plugin to replay 45 queued file change(s):
core/src/mcp/find_definition.rs, ...", then "1 file(s), 5 node(s)/7 edge(s)
upserted ... in 56.733562143s". `find_definition.rs` was the **first** replayed
file, so the fresh plugin's index held only that file. The live index's
semantic `CALLS` edges from `find_definition.rs` are all same-file
(`with_source`, `reached`, `shapes`, `admits`), the same set as r4 pass 1.
That run used binaries built before 3162cbd (exe mtime 2026-10-02 22:31), so
`refused_by_all`/`refuses` had no structural edge to fall back on.

## 2. The cause, by file:line

1. `plugins/sdk/src/run.rs:406-419`: `semanticPass` calls
   `self.hydrate(&files)`. For a per-file pass `files` is that one file.
2. `run.rs:478-501` `Session::hydrate` extracts only `files` (the whole
   project only when `files` is empty). Its doc at `run.rs:461-477` says
   per-file is enough.
3. `plugins/sdk/src/lsp/bridge.rs:1262-1267` `file_at` returns `None` for a
   project file that is not in `SdkIndex`. `node_at` (:1282) then returns
   `None`, and `record_answer` (:1564-1571) treats it as "Empty, ambiguous, or
   nothing this index holds". There is no edge, and the pass still counts as
   complete. `tests/lsp_bridge.rs:1327`
   `an_answer_outside_the_index_produces_no_edge_and_no_incompleteness` pins
   that behaviour. The behaviour is right for std/dependency locations and
   wrong for an un-hydrated project file.
4. A side effect with the same cause: `untyped_call_answered` (`bridge.rs:1675`,
   GM-486) counts a call as answered "outside the index" when every location's
   `file_at` is `None`. A project method in an un-hydrated file therefore
   *drops* its name from `untypedCalls`, and the GM-486 marker says the call
   was answered when it got no edge. I did not observe it in these runs,
   because every site in this file is now typed structurally. It follows from
   the code.

The rust-analyzer coldness only explains the 56.7s. The readiness wait
(`LspBridge::wait_ready`, `bridge.rs:600-631`) and the deferral of empty
answers (`run_pass`, :1313-1530) worked as designed: 0 empty answers out of
488 per pass.

## 3. Are unanswered sites ever re-asked once warm? Today: no

- **Dropped cross-file answers (the cause above).** These are re-asked only
  when core sends another `semanticPass` for the *same* file, after the target
  files have entered the plugin's index. Core sends that only when the file is
  edited again (`fileChanged` → per-file pass). Re-asking alone does not help
  (r4 pass 2) unless the target file was hydrated in between. During a replay,
  the target is hydrated only when it is itself one of the queued files and is
  replayed *later*, which by then is too late for the earlier file. Core does
  not repeat the whole-project pass: `language_state.rust.semanticPassError =
  "not run - its plugin was asleep ... the next daemon start or g-mesh reindex
  asks again"`, and `daemon::activation`'s `needs_semantic_pass_retry` is
  checked only at startup. In practice the answer comes at the next daemon
  start or `g-mesh reindex`.
- **A pass that ran out of readiness time** (r1 pass 1; live log line 2473:
  "still indexing after 89.34836036s - this pass asks nothing"). A per-file
  pass reports incomplete to the plugin, but `pass_response` (`run.rs:623-640`)
  sends `incomplete` only for a whole-project pass. Core treats the per-file
  pass as done and never re-asks. The file's sites stay unanswered until it is
  edited again. This is a second, real way for "a cold first pass leaves sites
  unanswered". It is the one the task title describes, and it is truly about a
  cold rust-analyzer.

## 4. Proposed fix

### Fix 1: a per-file pass sees the whole project (the cause of the GM-485 miss)

In `Session` (`plugins/sdk/src/run.rs`), the first `semanticPass` of a plugin
process hydrates the whole project, once, whatever its scope:

- `run.rs:406-419` (`"semanticPass"` arm): call `self.hydrate(&[])` once per
  process. A `project_hydrated: bool` on `Session` gates it. Reset it on
  `workspaceChanged` (`run.rs:420-440`), which clears the index. After that,
  hydrate `files` as today: those are already present, so it is a no-op.
- `run.rs:461-477`: rewrite the doc. A per-file pass needs every file its
  answers can land in.

The baseline hazard has to be fixed in the same change. A hydrated entry
becomes `file_changed`'s diff baseline (`run.rs:523` identical-text
short-cut, `:542` `diff_file(self.index.graph(path), ..)`). During a replay,
`semanticPass(f1)` would hydrate f2 from disk, already at its new text. The
following `fileChanged(f2)` would then answer an **empty diff**, and core
would keep f2's stale nodes. Fix: `FileEntry` (`plugins/sdk/src/index.rs:41-50`)
gets `reported: bool`. Entries inserted by `file_changed` are `true`, and
entries inserted by `hydrate` (:498) are `false`. `file_changed` treats an
unreported entry as no baseline: it skips the identical-text short-cut and
calls `diff_file(None, ..)`, which gives a complete diff, then marks the entry
reported. The whole-project pass after the cold walk has the same hazard in a
narrower window, so the flag covers it too.

Benefits: every per-file pass resolves cross-file targets, with no protocol
or core change. Answers in an un-hydrated file no longer feed the GM-486
marker's "outside the index" rule.

Risks and trade-offs:
- A one-time extraction of the project per plugin process. Measured: 274
  files in 2.45s through `fileChanged`, including framing and diffs, at
  load 14. That is small next to rust-analyzer's own 33-90s start.
- Memory for the whole project's sources and graphs. The process already
  holds these after a whole-project pass.
- The first `fileChanged` for each hydrated file now sends a complete diff
  instead of an incremental one: more bytes, once per file.

Alternative, not proposed: hydrate lazily only the files the answers point
into. That needs a second recording phase in `run_pass`/`record_answer`
(answers kept until the Session can extract the files), and it needs the same
`reported` flag. It costs less memory but adds more moving parts.

### Fix 2: re-ask what a pass did not finish, on the next pass

`LspBridge` (`bridge.rs:377-434`) gets `owed: BTreeMap<RelPath, u8>`: the
files a per-file pass asked about and did not finish, with the number of
attempts so far.

- `LspBridge::answer` (`bridge.rs:1828-1977`), scope at :1829-1834: for a
  per-file pass, scope = `files` ∪ `owed` keys, keeping only those still in
  the index.
- The `Err(reason)` arm at :1917 (readiness timed out, or no server): add
  every `asked_about` file to `owed`.
- After `finished` (:1928): remove the finished files from `owed`, and
  increment `asked_about − finished`. Drop a file after 3 attempts (with a
  log line), so that a site that is always refused cannot ride along forever.
- A complete whole-project pass clears `owed`.

Benefits: a file whose cold pass asked nothing gets answered on the next
pass, against a server that is warm by then. This stays inside the plugin,
with no core change.

Risks and trade-offs:
- The re-ask happens only when the *next* `semanticPass` arrives, that is, on
  the next Rust edit. With no further edits the sites wait for the next daemon
  start, as today.
- Owed files share the fixed per-file budget (`single_file`, 90s). A long
  owed list can run out of budget, and the unfinished files simply stay owed.

Functions to read for context: `run_pass` (:1313-1530, `covered` =
touched − failed), `retract_stale` (:1980), `trim_untyped_calls` (:1697),
`LspClient::settle`/`quiet_for`/`mark_edited` (`client.rs:316-420`).

## 5. Test plan, each test with its control

The tests do not need rust-analyzer: they use the fake LSP in
`plugins/sdk/tests/lsp_bridge.rs` and the toy extractor/engine in
`run.rs` tests.

1. **Fix 1, Session hydrates the project for a per-file pass**
   (`run.rs` tests; a recording engine like
   `a_workspace_changed_frame_reaches_the_started_engine`'s). Files `a.toy` and
   `b.toy` are on disk. Send `semanticPass {filePaths:[a.toy]}` to a fresh
   session, and assert that the engine's `index` holds `b.toy`.
   *Control:* revert the `hydrate(&[])` call. The engine sees only `a.toy`.
2. **Fix 1, end to end through the bridge** (`lsp_bridge.rs`). The fake server
   answers a site in `b.toy` with a location in `a.toy`. Build the index the
   way a fresh Session would after Fix 1 (both files), and also through the
   Session path if the toy plugin binary allows it. Assert a semantic edge to
   `a.toy`'s declaration.
   *Control:* with the Session change reverted (index holds `b.toy` only), no
   edge. This is the r4 pass 1/pass 3 difference in miniature.
3. **Fix 1, no stale baseline** (`run.rs` tests). A fresh session, with
   `b.toy` on disk at text v2. Send `semanticPass {filePaths:[a.toy]}` (which
   hydrates `b.toy` v2), then `fileChanged b.toy`. Assert a non-empty,
   `complete` diff carrying `b.toy`'s nodes.
   *Control:* revert the `reported` flag. The diff is empty.
4. **Fix 2, a pass that asked nothing is re-asked once the server is warm**
   (`lsp_bridge.rs`, from `a_server_that_never_becomes_ready_reports_an_incomplete_pass`).
   The server reports progress past a small `readiness` budget on pass 1 (file
   `b.toy`), then goes quiet. Pass 2 is for `a.toy` only. Assert that pass 2
   asks the `b.toy` site (`asked(log, "textDocument/definition")`) and emits
   its edge.
   *Control:* revert the `owed` merge. Pass 2 asks only `a.toy`, and `b.toy`
   has no edge.
5. **Fix 2 is bounded.** A server that refuses every question about `b.toy`:
   after 3 passes `b.toy` is no longer asked.
   *Control:* revert the attempt cap. A 4th pass still asks it.
6. Existing pins stay green unchanged:
   `an_answer_outside_the_index_produces_no_edge_and_no_incompleteness`
   (std/dependency locations), `a_per_file_pass_asks_only_about_that_file`.
   The latter needs one look: with an empty `owed` it still holds.
   Re-measure: rerun r4's op list with the fixed plugin. Pass 1 should record
   the 51 edges of pass 3.

## 6. Acceptance criteria

- *Reproduce the partial answer, or explain why it cannot be reproduced.*
  Done. Reproduced on 7468d9b as same-file-only edges from a fresh plugin's
  first per-file pass (r4: 10 vs 51 semantic edges from identical LSP
  answers). At the three exact GM-485 sites it no longer shows, because GM-485's
  structural typing (3162cbd) now covers them.
- *Name the cause.* Done: §2. It is the plugin's partial `SdkIndex`, not
  rust-analyzer.
- *Unanswered on a cold pass, answered once warm, with a control.* Fix 2
  together with tests 4-5. Fix 1 makes the GM-485 class answered on the cold
  pass itself, with tests 1-3.

Both fixes stay within the task's acceptance criteria. **Decision needed:**
whether Fix 1 (hydration plus the `reported` baseline flag, which touches
`run.rs`/`index.rs`) ships under GM-487, or Fix 2 alone ships under GM-487 and
Fix 1 gets its own task. Fix 1 is the one that fixes the observed miss.

## g-mesh calls this note relied on

- `find_callers(SemanticEngine::answer)`: returned only `LazyEngine::answer`.
  The trait-object call in `run.rs:417` is a receiver call that g-mesh does not
  link, so I found it with grep.
- `search_code("decide when to run a whole-project semantic pass again after an
  incomplete pass")` → `ActivationCtx.needs_semantic_pass_retry`
  (`core/src/daemon/activation.rs:76`). Its uses
  (`core/src/daemon/mod.rs:289,389`) were found with grep.
- Everything else is single-file reading of `bridge.rs`, `run.rs`, `index.rs`
  and `client.rs`, by function.
