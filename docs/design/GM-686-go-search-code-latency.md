# GM-686: Go `search_code` takes ~20 s per call (diagnosis)

Slice GM-686/S1. Binary: release build of this branch (`release-4.23.0` tip,
db1074a), isolated `G_MESH_HOME=/tmp/gm686h` (models linked, embedding cache
copied from `~/.g-mesh` with `sqlite3 .backup`), daemons spawned by
`g-mesh mcp-shim` from a Python probe (the `scripts/mcp-smoke.py` `Shim`),
projects as detached worktrees / fixture copies in the scratchpad.
(`G_MESH_HOME` under the scratchpad itself was refused: the daemon socket path
was 170 bytes against macOS's 103-byte limit, so the home lives in `/tmp`.)

## Verdict

**Nothing in the query path depends on Go.** The 20 s is
`SEARCH_EMBEDDING_WAIT` (`core/src/mcp/mod.rs:134`, 20 s), the bounded wait
`search_code` makes for `Phase::Ready` while the project's **embedding
backfill pass** is still running. Every call during the pass waits the whole
bound and then answers a partial page. torpeek was in that state for the
whole GM-435 Go arm. Its backfill is cold: 1,608 texts owed a vector, 22 cache
hits, because the copied cache had almost nothing from torpeek. That backfill
took 631.8 s here. GM-435's Go warm gate passed after 29.8 s, on
`semantic_complete`, long before the backfill could finish. The Rust and TS
arms had warm caches, so their passes finished before rep 1.

Once the pass is done, torpeek answers in **0.05 s** (3 calls: 0.050, 0.048,
0.051 s), the same order as TS (task-tracker-mcp: 0.030, 0.025 s after a
1.24 s first call). So the Go gap is a measurement artifact. Behind it is a
general product problem that the artifact exposed: **during a long backfill,
every `search_code` call costs the full 20 s**, in any language.

## Evidence

The machine was not quiet at any point of this slice. Load average was
38-107 throughout (other agents' builds); no quiet window came. The ready
torpeek still answered in 0.05 s at load 44-72, so a quiet machine cannot
reverse the verdict. It can only shorten the backfill.

### Run 1: cold index per project (probe `run1.jsonl`)

| project | first `search_code` | 2nd | 3rd | `index.phase` at call | load (1/5/15) |
|---|---|---|---|---|---|
| fixture go (2 nodes embedded) | 5.28 s | 0.187 | 0.067 | ready (2.5 s after first call) | 63-65 / 101 / 87 |
| fixture rust (3) | 5.25 s | 0.085 | 0.164 | ready | 65 / 100 / 87 |
| fixture typescript (2) | 4.70 s | 0.080 | 0.160 | ready | 63 / 98 / 87 |
| **torpeek** (go 8,102 nodes, ts 622, py 71) | **20.151 s** | **20.161** | **20.061** | **embedding**, 512 then 768 of 2,405 | 49-44 / 67-64 / 78-76 |
| task-tracker-mcp (ts, 253 embedded) | 1.718 s | 0.037 | 0.036 | ready | 43-42 / 61 / 74 |

- The first call in a fresh daemon spends ~5 s loading the embedding and
  rerank models. That is not this task.
- A tiny Go fixture is as fast as Rust and TS, so the Go plugin is not
  involved.
- torpeek's three calls each returned the partial-page note from
  `search_code::Coverage::note`:
  `g-mesh: the embedding pass for …/torpeek is still running (embeddings 512 of 2405 computed, this call waited 20s)`.
  The 20.06-20.16 s RTT is 20 s of wait plus 60-160 ms of query. GM-435
  reported 20.06 s median, max 20.15 s.
- Phase timeline for torpeek: `structural` from the first call (13.7 s walk);
  `embedding` only at +143 s, after the go semantic pass (1 m 20 s) and the ts
  pass (24.7 s), which `ActivationCtx::activate` runs first; the backfill then
  ran past the probe's 600 s watch.

### Run 2: same torpeek index, daemon restarted

The daemon log shows the pass completing:
`embeddings [backfill]: 1608 texts, 22 cache hits, 1586 embedded, 0 cache errors, 631.8s`.
That is 2.5 texts/s at load 45-100; GM-425 measured ~8 texts/s cold on a
less loaded machine. `status --json` then reports `phase: ready`,
`embeddings 1642/1642`.

### Run 3: torpeek ready

`search_code` took 0.050 / 0.048 / 0.051 s (uptime `load averages: 44.45 72.48 52.62`);
task-tracker-mcp 1.238 / 0.030 / 0.025 s. Probe `time -p`: real 136.80,
user 0.45, sys 0.62. The probe was idle waiting on the daemon, which is the
expected shape.

### Why GM-435's Go arm was always mid-backfill

- The user's own torpeek index (`~/.g-mesh/projects/ef290ec70b24925c`) has
  173 nodes and 140 vectors, so the embedding cache GM-435 copied held almost
  no torpeek texts. Here it gave 22 hits out of 1,608.
- GM-435's Go warm event is `{"kind": "warm", "lang": "go", "semantic_complete": true, "secs": 29.8}`.
  A cold torpeek backfill cannot finish in 30 s, and its 3 reps then ran
  within ~3 minutes (`time -p` real 184.67 s for the rerun).
- The incremental edit's own 3 texts are embedded on the watcher path,
  outside the backfill. That is why the new nodes' vectors were stored at
  0.68 s while `search_code` still waited for `Phase::Ready`.

## Today's behaviour (code)

- `GMeshMcpServer::search_code` (`core/src/mcp/mod.rs:922-936`) runs
  `prepare(.., Need::Structural)`, then `wait_for_embeddings`, then
  `search_code::handle_off_worker`.
- `wait_for_embeddings` (`core/src/mcp/mod.rs:586-614`) waits through
  `wait_until(.., Need::Embeddings, deadline = now + bound)`. The bound is the
  test override `search_embedding_wait`, else `G_MESH_SEARCH_EMBEDDING_WAIT_MS`,
  else `SEARCH_EMBEDDING_WAIT` = 20 s. On `TimedOut` it builds
  `Coverage::partial` from `embed_progress()` and the phase.
- `Need::Embeddings` is satisfied only by `Phase::Ready`
  (`core/src/daemon/indexing_status.rs:81-86`). `Phase::Embedding` is set
  just before `embedding::backfill::run` and `Ready` right after it
  (`core/src/daemon/activation.rs:228-247`). Before that, `Structural` covers
  the whole semantic pass, during which the wait also runs to its bound
  (note: "has not started yet").
- Nothing remembers that an earlier call already waited the bound for this
  same pass. Each call starts its own 20 s deadline, so N calls during a
  10-minute backfill cost N x 20 s.
- The only language-dependent input to the query path is `QueryShapes`
  (plugin manifests' `non_symbol_queries`, read in `search_code::handle`).
  It is a pure in-memory classification with no I/O and no wait.

### g-mesh calls behind these claims (project `g-mesh-wt-gm686`)

- `find_references SEARCH_EMBEDDING_WAIT` gives one reference, in
  `GMeshMcpServer::wait_for_embeddings` (core/src/mcp/mod.rs).
- `find_references SEARCH_EMBEDDING_WAIT_ENV` gives one reference, in the same
  function.
- `find_callers GMeshMcpServer::wait_for_embeddings` gives one caller,
  `GMeshMcpServer::search_code`.
- `find_callers search_code::handle_off_worker` gives one caller,
  `GMeshMcpServer::search_code`.
- `find_callers IndexingStatus::set_phase` gives `request_activation` and
  `activation_failed` (indexing_status.rs), `ActivationCtx::activate` and
  `ActivationCtx::walk` (activation.rs), and `SeedGate::release` (seed.rs).
  The Rust semantic tier was `absent`, and 19 untyped receiver calls were
  listed, all in tests except indexing_status.rs. `Phase::Ready` itself is
  set only at activation.rs:247 (grep, one file).
- `find_references QueryShapes` (answer: files) gives 44 refs in 19 files. In
  the search path the only one is `search_code.rs`, used for classification.
- Other ~20 s / 10 s timeouts (grep over `core wire walk`):
  `LINK_EVENT_TIMEOUT` (watcher, 20 s), `SEMANTIC_PASS_PER_FILE_BUDGET`
  (10 s), `BOOTSTRAP_TIMEOUT` (10 s), `PURGE_LOCK_WAIT` (10 s). None is on
  the `search_code` path. The partial-page note names the wait explicitly.

## Fix options

| option | benefit | risk |
|---|---|---|
| **A. No product change.** Record the artifact, and require `phase: ready` in the warm gate of any search latency measurement. | Zero code risk; the GM-435 number is explained. | Users keep paying 20 s per call during every long backfill: a first session on a new large project, or after a model change. An agent that calls `search_code` 5 times waits 100 s for 5 partial pages. |
| **B. One full wait per pass (latch).** After a call has waited the whole bound and timed out, later calls answer the partial page at once, until the phase reaches `Ready` or restarts at `Walking`. | One 20 s wait per activation instead of one per call. Small, deterministic, easy to pin and control. Keeps GM-432's case: a small project's whole backfill still fits in the first call's wait. | A later call made seconds before the pass ends returns partial where waiting would have given a complete page. The note already says "call again later". The latch is daemon-wide, so a second session gets no wait either. |
| C. ETA wait: wait only if `(total - embedded) / rate` fits in the bound. | Waits exactly when it pays off. | The rate is unknown in `Structural`/`Counting` and jumps in 256-row pages. It is noisy under load (2.5 vs 8 texts/s here). More code, and timing-dependent tests that are hard to make repeatable. |
| D. Lower `SEARCH_EMBEDDING_WAIT` (e.g. 5 s). | One-line change. | Undoes GM-432's reasoning. A small project's backfill or one change batch no longer fits, so first calls return partial pages more often. Repeated calls still pay 5 s each. |
| E. Make the backfill faster or earlier (run before the semantic pass, share cache across worktrees). | Shrinks the window itself. | Wider change, competes with the semantic pass for CPU, and does not remove the per-call cost. Separate task if wanted. |

**Recommended: B, plus A's measurement note.** B removes the repeated cost
that made the anomaly, keeps the first-call wait that GM-432 sized, and is
pinned by fast, timing-tolerant tests. Option C can come later if one wait
per pass still proves too costly.

## Edit map (option B)

Functions to change:

- `core/src/daemon/indexing_status.rs`
  - `struct Inner` (138-177): add an `AtomicBool`, e.g.
    `embedding_wait_spent`.
  - `IndexingStatus::set_phase` (318-337): clear it on `Phase::Ready` and on
    `Phase::Walking` (a new activation, or a retry after `Failed`).
  - New methods `embedding_wait_spent()` / `mark_embedding_wait_spent()`,
    next to `embed_progress` (575).
- `core/src/mcp/mod.rs`
  - `GMeshMcpServer::wait_for_embeddings` (586-614): if the latch is set,
    use a zero bound (`deadline = started`, so `wait_for` still returns
    `Satisfied` if the phase is already `Ready`). On `TimedOut` after a
    non-zero bound, set the latch.
  - Doc of `SEARCH_EMBEDDING_WAIT` / `SEARCH_EMBEDDING_WAIT_ENV` (127-134):
    say "per pass, not per call".
- `core/src/mcp/search_code.rs`
  - `Coverage::partial` (153-168) / `Coverage::note` (171-200): a no-wait
    page should say it did not wait because an earlier call already waited
    for this pass. Today it would read "this call waited 0s", which is true
    but unhelpful. This is a wording choice; keep the `partial` JSON shape.
- Docs: `docs/architecture/gm-432-search-code-hang.md` (the wait rule), and
  a pointer from `docs/results/GM-435-edit-to-tier-latency.md` anomaly 2 to
  this note.

Functions to read for context:

- `GMeshMcpServer::wait_until` (`core/src/mcp/mod.rs:510-578`): progress and
  cancellation race.
- `IndexingStatus::wait_for` (`core/src/daemon/indexing_status.rs:370-393`).
- `ActivationCtx::activate` (`core/src/daemon/activation.rs:147-248`): the
  phase order.
- `embedding::backfill::run` (`core/src/embedding/backfill.rs:155-210`):
  progress counters.
- `core/src/mcp/search_code_wait_tests.rs`: the existing fixture (`fixture`,
  `search`, `with_search_embedding_wait`).

## Behaviours the tests must pin

1. **A second call during the same pass does not wait.** With the bound at
   e.g. 2 s and the phase held at `Embedding`, call 1 takes at least the
   bound and is partial; call 2 returns well under the bound (assert < bound/2)
   and is partial with the same `partial` counts. Control: remove the latch
   check in `wait_for_embeddings`, and call 2 takes the full bound.
2. **The first call still waits and can complete.** The existing
   `a_pass_that_finishes_within_the_wait_gives_a_complete_answer` must keep
   passing.
3. **The latch clears at `Ready`.** After call 1 times out, set
   `Phase::Ready`: the next call is complete (no note, no `partial`).
4. **A new activation waits again.** After the latch is set, go
   `Walking -> Structural -> Embedding`: the next call waits the bound again.
   Control: do not clear the latch on `Walking`, and it answers at once.
5. **No-wait note wording** (if changed): the partial note of call 2 names the
   earlier wait, and call 1's note keeps "this call waited".

Tests 1 and 4 use timers. Per the owner's rules they run 5 times in the
tests slice. Assert only "under bound/2" against "at least bound", never an
exact duration.

## Artifacts

Scratch (not committed):
`/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/221b5b90-07f5-4c78-b052-fb06d4b6838c/scratchpad/gm686/`
holds `probe.py`, `run1.jsonl`, `run2.out`/`run2.jsonl` and `run3.jsonl`. The
daemon log is `/tmp/gm686h/projects/f20413943b425e71/daemon.log`.
