# GM-435: how long one edit takes to reach each tool tier (results)

Method: docs/design/GM-435-edit-to-tier-latency-method.md. Slice GM-435/S10.
Binary: g-mesh 4.22.0 **release** build of commit 428100d (the instrumented
release-4.22.0 tip), `G_MESH_EDIT_TIMING=1`, isolated `G_MESH_HOME=/tmp/gm435h`
(models linked, embedding cache copied), daemon started directly, one warm
daemon per language with its semantic engine loaded and idle (rust-analyzer,
vtsls, go/types) before rep 1. Projects were detached throwaway worktrees
outside the user's checkouts. Edit: in-place `open(O_TRUNC)/write/close`,
~42-45 lines removed (two functions with docs) and ~60 lines appended (three
functions, one of which calls a method through a local variable, so the call
edge exists only after the semantic pass). Each rep's new doc comments carry a
salt, so each rep embeds 3 new texts. Between reps the original text is written
back (restore) and observed too. One discarded warm-up rep per arm, then 5 reps
(Rust A/B, interleaved) or 3 reps (TS, Go). Times are seconds after save.

## Arms

| arm | file | removed | why |
|---|---|---|---|
| Rust A | core/src/daemon/identity.rs (201 lines incl. tests) | `project_hash`, `record_project_root` | public; imported **by name** in 6 other files (`use ...identity::{project_hash, record_project_root}`: storage/connection.rs, cli/clean.rs, 4 integration tests). Picked with g-mesh `find_references` (project_hash: 15 refs in 7 files; record_project_root: 8 refs in 4 files). file_rows.rs, ndjson_frame.rs, answer.rs, resume_token.rs were rejected: their removable items are methods, tiny, or reached by module path. |
| Rust B | core/src/cli/status/json.rs (186) | `languages`, `outcome` | private, used only in the same file |
| TS | task-tracker-mcp src/domain/taskStatus.ts (182) | `findUnblockedDependents`, `displayStatus` | exported, imported by other files; worktree without node_modules |
| Go | torpeek internal/swarm/files.go (158) | `SelectVideos`, `looksLikeSample` | exported; separate worktree |

## Headline: medians [min-max] against the 2026-09-28 estimate

| tier | estimate | Rust A (public, by-name) | Rust B (private) | TS | Go |
|---|---|---|---|---|---|
| (a) structural, tool | ~0.5 s | 0.69 [0.59-0.85] | 0.54 [0.41-0.72] | 0.44 [0.37-0.45] | 0.41 [0.41-0.50] (rerun) |
| (b) search_code, tool | ~1 s | 0.86 [0.69-1.35] | 0.99 [0.59-1.45] (4 of 5; one rep not in top 10) | 0.94 [0.57-0.97] | not measurable, see anomalies; vectors stored (db) at 0.68 [0.62-0.94] |
| (c) semantic edge, tool | ~1.5-3.5 s | 2.75 [2.65-3.11] | 1.13 [0.81-2.31] | 2.80 [2.66-2.90] | 5.19 [4.12-6.60] (load 44-72, see anomalies) |
| semantic done (log) | - | 2.73 | 1.12 | 2.79 | 5.18 |

Verdict per tier:
- **(a) held, slightly under-estimated**: 0.4-0.7 s. 0.30 s is the debounce; the rest is FS event (0.01-0.04 s), parse (<10 ms) and store write (10-50 ms), plus the tool's poll period and a find_definition round trip (30-180 ms under the Rust index). The db sees the change at 0.33-0.53 s; the tool adds 0.05-0.3 s.
- **(b) held**: about 1 s for Rust and TS. Embedding of the 3 new texts took 0.14 s (A), 0.15 s (B), 0.37 s (TS), 0.32-1.0 s (Go); 8 of 11 texts were cache hits (A, TS), 1 of 4 (B). The vectors are stored at 0.6-0.8 s; search_code adds a poll and a ~110-180 ms round trip.
- **(c) held for Rust A and TS (2.7-2.8 s), better than estimated for Rust B (1.1 s), worse for Go (4-7 s)**. The phase that decides it is the semantic question phase: Rust A asks 101 questions (2.04 s round trip), Rust B 23 (0.48 s), TS 24 (2.1 s round trip, ~2 s of it waiting on vtsls). `wait_ready` was 0 ms in every rep (latched), sync <6 ms, answer build 0.2 ms, commit/embed/settle <10 ms, `ContentModified` re-asks 0. Rust-analyzer's re-analysis is therefore not a separate wait: it is inside the question latency, and it is larger when the file has more cross references (A: 101 questions).
- **No re-extract in any arm**: for Rust A the plugin answered `affected=none` although six files import the removed functions by name, so `reextract_start/done` never fired (TS warm-up answered `affected=2` once but no reextract line was logged for it either; later reps said none). The re-extract path was not exercised; this is a finding for the Q3 premise.
- The estimate's 20 s without a running rust-analyzer was not measured (warm daemon only). The first edit after warm-up (discarded) was slower in A (a=1.29, c=3.35 s, route 0.6 s), i.e. first-touch effects are real but one edit deep.

## Anomalies and caveats

1. **The machine was not quiet.** Load average at start was 6-14, with no `rustc` running; per-rep load is in the table below (Rust 4.5-18.6, TS 7.9-9.9). A Godot app and other agents' processes ran meanwhile. After the TS arm, load rose to 45-225 (not from this run: no build of ours was running) so **Go numbers were measured under load 13-72 (rerun) and 46-225 (first run)** and are an upper bound. Go (c) in the first run was 6.0-8.4 s, in the rerun 4.1-6.6 s.
2. **Go: search_code took ~20 s per call** (RTT 20.06 s median in the rerun and 20.09 s in the first run, 3 calls each, versus 0.1-0.2 s for Rust and TS), so the 5-second search budget expired after the first call and `b_tool` is recorded as unmeasured. Not investigated at the time; the vectors for the three new nodes were stored at 0.68 s. **Cause found in GM-686: it was not Go.** Every rep ran inside a cold embedding backfill, and `search_code` waits up to 20 s for the embedding pass to reach Ready. See [docs/design/GM-686-go-search-code-latency.md](../design/GM-686-go-search-code-latency.md). Rule: search latency measurements must wait for the phase to be ready before the first timed call.
3. Rust B rep 1: the new function was not in search_code's top 10 within 5 s (ranking, not latency). Excluded from the (b) tool median.
4. Rust B reps 2 and 4 started with rust-analyzer not idle (idle gate hit its 60 s cap), which shows in their slower (c) (1.53 s and 2.29 s) and longer route.
5. The db observer needed one fix mid-run: a `pending_symbol` placeholder named like a removed function lingers in the same file (tests still call it), so the "removed fn is gone" db check ignores placeholders. The first full run (kept in the scratchpad) aborted at Rust A rep 1 because of it; this report uses the second run.
6. All measured reps reached every tier except Go (b) tool and one Rust B (b) tool; no whole-language reindex line appeared; no daemon outlived the run (`left_processes` empty for each).
7. Observer load: tool pollers add work (find_definition every 25 ms); search/callees pollers start only after the db flip.
8. `time -p` for the full run: real 1147.01 s, user 545.38 s, sys 137.67 s (includes warm-ups, restores, idle gates). Rerun of Go: real 184.67 s, user 448.57 s, sys 11.91 s.

## Full tables (main run: Rust A/B, TS, Go first run)

Seconds after save, median [min-max] over measured reps (warm-up excluded).

| phase | rust/A (n=5) | rust/B (n=5) | ts/T (n=3) | go/G (n=3) |
|---|---|---|---|---|
| fs_event (save->FSEvent) | 0.03 [0.02-0.04] | 0.03 [0.02-0.21] | 0.01 [0.00-0.02] | 0.01 [0.01-0.01] |
| debounce | 0.30 [0.30-0.30] | 0.30 [0.30-0.33] | 0.30 [0.30-0.31] | 0.30 [0.30-0.30] |
| route/queue (settled->rt_sent) | 0.03 [0.00-0.05] | 0.13 [0.00-0.29] | 0.00 [0.00-0.01] | 0.01 [0.00-0.03] |
| plugin parse | 0.00 [0.00-0.01] | 0.00 [0.00-0.01] | 0.00 [0.00-0.01] | 0.00 [0.00-0.01] |
| store write | 0.02 [0.01-0.07] | 0.01 [0.00-0.02] | 0.02 [0.01-0.03] | 0.05 [0.04-0.05] |
| (a) db | 0.41 [0.37-0.42] | 0.53 [0.38-0.66] | 0.33 [0.33-0.38] | 0.39 [0.37-0.40] |
| (a) tool | 0.69 [0.59-0.85] | 0.54 [0.41-0.72] | 0.44 [0.37-0.45] | 0.54 [0.48-0.93] |
| embedding | 0.14 [0.09-0.18] | 0.15 [0.10-0.55] | 0.37 [0.17-0.45] | 1.02 [0.37-1.49] |
| vectors store | 0.15 [0.09-0.28] | 0.00 [0.00-0.04] | 0.01 [0.01-0.01] | 0.00 [0.00-0.00] |
| (b) db | 0.70 [0.59-0.86] | 0.63 [0.48-1.24] | 0.75 [0.51-0.78] | 1.40 [0.78-1.86] |
| (b) tool | 0.86 [0.69-1.35] | 0.99 [0.59-1.45] | 0.94 [0.57-0.97] | n/a |
| re-extract | n/a | n/a | n/a | n/a |
| sem sync | 0.00 [0.00-0.01] | 0.00 [0.00-0.01] | 0.00 [0.00-0.00] | n/a |
| sem wait_ready | 0.00 [0.00-0.00] | 0.00 [0.00-0.00] | 0.00 [0.00-0.00] | n/a |
| sem questions | 101.00 [101.00-101.00] | 23.00 [23.00-23.00] | 24.00 [24.00-24.00] | n/a |
| sem answer build | 0.00 [0.00-0.00] | 0.00 [0.00-0.00] | 0.00 [0.00-0.00] | n/a |
| sem round trip (core) | 2.04 [2.03-2.20] | 0.48 [0.31-1.03] | 2.10 [2.03-2.14] | 5.24 [4.81-6.52] |
| sem commit | 0.00 [0.00-0.03] | 0.00 [0.00-0.00] | 0.01 [0.00-0.01] | 0.00 [0.00-0.00] |
| (c) db | 2.74 [2.64-3.08] | 1.12 [0.80-2.29] | 2.79 [2.65-2.89] | 6.21 [6.03-8.39] |
| (c) tool | 2.75 [2.65-3.11] | 1.13 [0.81-2.31] | 2.80 [2.66-2.90] | 6.21 [6.03-8.40] |
| semantic_done (log) | 2.73 [2.63-3.08] | 1.12 [0.79-2.28] | 2.79 [2.65-2.89] | 6.21 [6.02-8.38] |
| restore: (a) log | 0.34 [0.34-0.39] | 0.33 [0.32-0.34] | 0.35 [0.33-0.37] | 0.44 [0.37-0.50] |
| restore: semantic_done | 0.79 [0.73-0.88] | 0.74 [0.55-1.08] | 2.62 [2.60-2.68] | 4.71 [3.98-5.08] |

Embedding texts / hits / newly embedded, extra counts, per rep:

| arm | rep | texts | hits | embedded | affected | reextract n | sem questions | content_modified | max_request_ms | load1 before | rustc | daemon+plugin CPU% before |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| rust/A | w | 11 | 8 | 3 | none | None | 101 | 0 | 381 | 6.5 | 0 | 0.0 |
| rust/B | w | 8 | 5 | 3 | none | None | 23 | 0 | 213 | 4.92 | 0 | 0.1 |
| rust/A | 1 | 11 | 8 | 3 | none | None | 101 | 0 | 175 | 4.82 | 0 | 0.0 |
| rust/B | 1 | 4 | 1 | 3 | none | None | 23 | 0 | 223 | 4.48 | 0 | 0.0 |
| rust/A | 2 | 11 | 8 | 3 | none | None | 101 | 0 | 592 | 6.01 | 0 | 0.0 |
| rust/B | 2 | 4 | 1 | 3 | none | None | 23 | 0 | 357 | 6.01 | 0 | 1.4 |
| rust/A | 3 | 11 | 8 | 3 | none | None | 101 | 0 | 211 | 9.23 | 0 | 0.0 |
| rust/B | 3 | 4 | 1 | 3 | none | None | 23 | 0 | 316 | 18.6 | 0 | 0.0 |
| rust/A | 4 | 11 | 8 | 3 | none | None | 101 | 0 | 267 | 11.67 | 0 | 0.0 |
| rust/B | 4 | 4 | 1 | 3 | none | None | 23 | 0 | 854 | 8.56 | 0 | 89.7 |
| rust/A | 5 | 11 | 8 | 3 | none | None | 101 | 0 | 1096 | 8.43 | 0 | 0.0 |
| rust/B | 5 | 4 | 1 | 3 | none | None | 23 | 0 | 201 | 6.23 | 0 | 0.6 |
| ts/T | w | 11 | 8 | 3 | 2 | None | 24 | 0 | 65 | 8.36 | 0 | 0.0 |
| ts/T | 1 | 11 | 8 | 3 | none | None | 24 | 0 | 116 | 7.91 | 0 | 0.0 |
| ts/T | 2 | 11 | 8 | 3 | none | None | 24 | 0 | 185 | 8.06 | 0 | 34.8 |
| ts/T | 3 | 11 | 8 | 3 | none | None | 24 | 0 | 135 | 9.86 | 0 | 0.0 |
| go/G | w | 11 | 0 | 11 | none | None | None | None | None | 46.79 | 0 | 0.0 |
| go/G | 1 | 7 | 4 | 3 | none | None | None | None | None | 145.11 | 0 | 0.0 |
| go/G | 2 | 7 | 4 | 3 | none | None | None | None | None | 219.55 | 0 | 0.0 |
| go/G | 3 | 7 | 4 | 3 | none | None | None | None | None | 225.84 | 0 | 0.0 |

Per-rep tier times (s): a_tool / b_tool / c_tool (+ route, reextract):

- rust/A warm-up: a_db=1.0304 a_tool=1.2856 b_db=1.2981 b_tool=1.427 c_db=3.3324 c_tool=3.3474 route=0.6007 debounce=0.3366 missing=[] idle_before=True pre_c=False
- rust/B warm-up: a_db=0.5428 a_tool=0.5489 b_db=0.6468 b_tool=0.7591 c_db=0.992 c_tool=1.0179 route=0.1012 debounce=0.3045 missing=[] idle_before=True pre_c=False
- rust/A 1: a_db=0.4111 a_tool=0.5871 b_db=0.5946 b_tool=0.6899 c_db=2.641 c_tool=2.6539 route=0.0013 debounce=0.303 missing=[] idle_before=True pre_c=False
- rust/B 1: a_db=0.5293 a_tool=0.5354 b_db=0.6251 b_tool=None c_db=0.9622 c_tool=0.9805 route=0.0006 debounce=0.3029 missing=[] idle_before=True pre_c=False
- rust/A 2: a_db=0.3657 a_tool=0.6893 b_db=0.7019 b_tool=0.8575 c_db=2.7381 c_tool=2.7529 route=0.0005 debounce=0.3046 missing=[] idle_before=True pre_c=False
- rust/B 2: a_db=0.6413 a_tool=0.6832 b_db=0.92 b_tool=1.0992 c_db=1.5281 c_tool=1.5516 route=0.2196 debounce=0.3312 missing=[] idle_before=False pre_c=False
- rust/A 3: a_db=0.3665 a_tool=0.6094 b_db=0.6225 b_tool=0.7365 c_db=2.6507 c_tool=2.6628 route=0.0254 debounce=0.3028 missing=[] idle_before=True pre_c=False
- rust/B 3: a_db=0.4739 a_tool=0.5031 b_db=0.6346 b_tool=0.8861 c_db=1.1167 c_tool=1.1303 route=0.1313 debounce=0.3038 missing=[] idle_before=True pre_c=False
- rust/A 4: a_db=0.4192 a_tool=0.6995 b_db=0.7095 b_tool=0.8799 c_db=2.748 c_tool=2.7764 route=0.0537 debounce=0.3037 missing=[] idle_before=True pre_c=False
- rust/B 4: a_db=0.6577 a_tool=0.722 b_db=1.2443 b_tool=1.4475 c_db=2.2855 c_tool=2.3054 route=0.2852 debounce=0.3054 missing=[] idle_before=False pre_c=False
- rust/A 5: a_db=0.4173 a_tool=0.8544 b_db=0.8637 b_tool=1.3517 c_db=3.0845 c_tool=3.1102 route=0.0511 debounce=0.3036 missing=[] idle_before=True pre_c=False
- rust/B 5: a_db=0.3801 a_tool=0.4108 b_db=0.4765 b_tool=0.5926 c_db=0.7952 c_tool=0.8117 route=0.0283 debounce=0.3048 missing=[] idle_before=True pre_c=False
- ts/T warm-up: a_db=0.3602 a_tool=0.4063 b_db=0.4615 b_tool=0.4885 c_db=2.4887 c_tool=2.494 route=0.0007 debounce=0.3038 missing=[] idle_before=True pre_c=False
- ts/T 1: a_db=0.3326 a_tool=0.4378 b_db=0.7835 b_tool=0.972 c_db=2.8948 c_tool=2.9044 route=0.0008 debounce=0.3012 missing=[] idle_before=True pre_c=False
- ts/T 2: a_db=0.3766 a_tool=0.4533 b_db=0.7471 b_tool=0.9435 c_db=2.7926 c_tool=2.8025 route=0.0064 debounce=0.3055 missing=[] idle_before=True pre_c=False
- ts/T 3: a_db=0.3296 a_tool=0.3677 b_db=0.5122 b_tool=0.5724 c_db=2.6519 c_tool=2.6559 route=0.0009 debounce=0.3023 missing=[] idle_before=True pre_c=False
- go/G warm-up: a_db=0.6476 a_tool=1.0384 b_db=5.0355 b_tool=None c_db=11.0792 c_tool=11.0876 route=0.1776 debounce=0.3025 missing=[] idle_before=True pre_c=False
- go/G 1: a_db=0.3671 a_tool=0.9274 b_db=1.8634 b_tool=None c_db=8.3857 c_tool=8.3981 route=0.0019 debounce=0.3022 missing=[] idle_before=True pre_c=False
- go/G 2: a_db=0.3852 a_tool=0.5446 b_db=1.4043 b_tool=None c_db=6.2059 c_tool=6.2134 route=0.0094 debounce=0.3008 missing=[] idle_before=True pre_c=False
- go/G 3: a_db=0.399 a_tool=0.4792 b_db=0.7811 b_tool=None c_db=6.0251 c_tool=6.0311 route=0.0328 debounce=0.3008 missing=[] idle_before=True pre_c=False

Tool RTT (ms, median over all polls):
- rust/A struct: median 117.8, max 456.9 (n=36)
- rust/A search: median 150.6, max 482.0 (n=5)
- rust/A callees: median 10.1, max 25.6 (n=5)
- rust/B struct: median 176.7, max 578.1 (n=24)
- rust/B search: median 112.0, max 294.8 (n=27)
- rust/B callees: median 13.2, max 19.7 (n=5)
- ts/T struct: median 28.9, max 139.4 (n=46)
- ts/T search: median 183.6, max 191.7 (n=3)
- ts/T callees: median 4.6, max 5.0 (n=3)
- go/G struct: median 135.4, max 486.1 (n=20)
- go/G search: median 20091.0, max 20116.6 (n=3)
- go/G callees: median 6.6, max 10.0 (n=3)

Run events:
- {"kind": "start", "uptime": "21:58  up 11 days, 10 hrs, 8 users, load averages: 6.09 12.85 13.94", "ncpu": 8}
- {"kind": "first_call", "lang": "rust", "secs": 18.1, "err": false, "text": "{\"results\":[{\"symbolId\":\"459c03599e3bd89aecc14c950cbfe8a4\",\"name\":\"project_hash\",\"kind\":\"Function\",\"startLine\":9,\"endLine\":17,\"exported\":true},{\"symbolId\":\"2ec5300552553ae8874b0315399274a4\",\"name\":\"RO"}
- {"kind": "warm", "lang": "rust", "semantic_complete": true, "secs": 196.4}
- {"kind": "stopped", "lang": "rust", "status": "ok", "left_processes": []}
- {"kind": "first_call", "lang": "ts", "secs": 2.4, "err": false, "text": "{\"results\":[{\"symbolId\":\"d7d224a0c18d3f2f24fbb25e894a87d7\",\"name\":\"LOCK_LEASE_MS\",\"kind\":\"Variable\",\"startLine\":15,\"endLine\":15,\"exported\":true},{\"symbolId\":\"61a8b07423fc76551f82ab0a44bd03f6\",\"name\":\""}
- {"kind": "warm", "lang": "ts", "semantic_complete": true, "secs": 12.4}
- {"kind": "stopped", "lang": "ts", "status": "ok", "left_processes": []}
- {"kind": "first_call", "lang": "go", "secs": 7.2, "err": false, "text": "{\"results\":[{\"symbolId\":\"dbeda23cbb1dc35f732ed2b807e3178a\",\"name\":\"FileInfo\",\"kind\":\"Type\",\"startLine\":11,\"endLine\":16,\"exported\":true},{\"symbolId\":\"0716a3f36fae47a7018797329e71737b\",\"name\":\"Name\",\"ki"}
- {"kind": "warm", "lang": "go", "semantic_complete": true, "secs": 35.2}
- {"kind": "stopped", "lang": "go", "status": "ok", "left_processes": []}
- {"kind": "end", "uptime": "22:17  up 11 days, 10:19, 8 users, load averages: 186.16 98.17 47.28"}


## Go rerun (separate daemon, same method)

Seconds after save, median [min-max] over measured reps (warm-up excluded).

| phase | go/G (n=3) |
|---|---|
| fs_event (save->FSEvent) | 0.01 [0.01-0.02] |
| debounce | 0.30 [0.30-0.30] |
| route/queue (settled->rt_sent) | 0.00 [0.00-0.04] |
| plugin parse | 0.00 [0.00-0.00] |
| store write | 0.02 [0.01-0.03] |
| (a) db | 0.35 [0.33-0.38] |
| (a) tool | 0.41 [0.41-0.50] |
| embedding | 0.32 [0.29-0.55] |
| vectors store | 0.00 [0.00-0.00] |
| (b) db | 0.68 [0.62-0.94] |
| (b) tool | n/a |
| re-extract | n/a |
| sem sync | n/a |
| sem wait_ready | n/a |
| sem questions | n/a |
| sem answer build | n/a |
| sem round trip (core) | 4.56 [3.43-5.66] |
| sem commit | 0.00 [0.00-0.00] |
| (c) db | 5.18 [4.11-6.59] |
| (c) tool | 5.19 [4.12-6.60] |
| semantic_done (log) | 5.18 [4.10-6.58] |
| restore: (a) log | 0.38 [0.34-0.39] |
| restore: semantic_done | 4.96 [4.31-5.37] |

Embedding texts / hits / newly embedded, extra counts, per rep:

| arm | rep | texts | hits | embedded | affected | reextract n | sem questions | content_modified | max_request_ms | load1 before | rustc | daemon+plugin CPU% before |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| go/G | w | 11 | 0 | 11 | none | None | None | None | None | 13.24 | 0 | 0.0 |
| go/G | 1 | 7 | 4 | 3 | none | None | None | None | None | 44.62 | 0 | 0.0 |
| go/G | 2 | 7 | 4 | 3 | none | None | None | None | None | 62.34 | 0 | 0.0 |
| go/G | 3 | 7 | 4 | 3 | none | None | None | None | None | 72.46 | 0 | 0.0 |

Per-rep tier times (s): a_tool / b_tool / c_tool (+ route, reextract):

- go/G warm-up: a_db=0.4008 a_tool=0.5101 b_db=2.2951 b_tool=None c_db=6.5132 c_tool=6.5203 route=0.0039 debounce=0.3007 missing=[] idle_before=True pre_c=False
- go/G 1: a_db=0.3813 a_tool=0.5049 b_db=0.9359 b_tool=None c_db=6.5894 c_tool=6.5993 route=0.0359 debounce=0.3006 missing=[] idle_before=True pre_c=False
- go/G 2: a_db=0.3257 a_tool=0.4132 b_db=0.6216 b_tool=None c_db=5.1803 c_tool=5.192 route=0.0008 debounce=0.3023 missing=[] idle_before=True pre_c=False
- go/G 3: a_db=0.3527 a_tool=0.4092 b_db=0.6774 b_tool=None c_db=4.1093 c_tool=4.1195 route=0.001 debounce=0.304 missing=[] idle_before=True pre_c=False

Tool RTT (ms, median over all polls):
- go/G struct: median 27.4, max 145.4 (n=38)
- go/G search: median 20060.0, max 20154.7 (n=3)
- go/G callees: median 5.2, max 6.3 (n=3)

Run events:
- {"kind": "start", "uptime": "22:28  up 11 days, 10:30, 8 users, load averages: 11.56 51.00 52.66", "ncpu": 8}
- {"kind": "first_call", "lang": "go", "secs": 5.7, "err": false, "text": "{\"results\":[{\"symbolId\":\"dbeda23cbb1dc35f732ed2b807e3178a\",\"name\":\"FileInfo\",\"kind\":\"Type\",\"startLine\":11,\"endLine\":16,\"exported\":true},{\"symbolId\":\"0716a3f36fae47a7018797329e71737b\",\"name\":\"Name\",\"ki"}
- {"kind": "warm", "lang": "go", "semantic_complete": true, "secs": 29.8}
- {"kind": "stopped", "lang": "go", "status": "ok", "left_processes": []}
- {"kind": "end", "uptime": "22:31  up 11 days, 10:33, 8 users, load averages: 121.46 69.22 58.86"}
