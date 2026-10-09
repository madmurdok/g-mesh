# GM-526: should an ambiguous page inline source for 4 candidates?

GM-352 inlines every candidate's source on an ambiguous `find_definition`
page when the whole set has at most `SOURCED_CANDIDATES = 3` candidates
(`core/src/mcp/find_definition.rs`). Its probe (gm-352-response-probe.md,
Findings 1) counted the 4-candidate names but did not measure whether a
4-source page costs less than the second call it saves. This measures it.

## Arms

- **A:** release-4.3.0 `404ed48` as is (`SOURCED_CANDIDATES = 3`).
- **B:** A with `SOURCED_CANDIDATES = 4`, nothing else. Built in a throwaway
  detached worktree (removed afterwards); the diff was the one line.
- **Caps kept as they are.** `CANDIDATE_SOURCE_LINES = 20` and
  `CANDIDATE_SOURCE_CHARS = 1_500` are literals, not computed from
  `SOURCED_CANDIDATES` (the doc comment says "a quarter" of
  `source::MAX_LINES = 80` / `MAX_CHARS = 6_000`). At 4 candidates the four
  caps sum to exactly one resolved answer's caps (80 lines, 6,000 chars), so
  B does not rescale them. No B source reached the char cap (largest 1,096
  chars).

## Method

As in gm-352-response-probe.md "Method": a ~100-line Python JSON-RPC driver
(not in the repo) speaks to `g-mesh mcp-shim` (debug build) with cwd = a fresh
`git clone` of the corpus per arm and a separate `G_MESH_HOME` per arm
(`~/.gm526/<arm>-<corpus>`; the scratchpad path exceeded the 103-byte socket
limit), `G_MESH_MODEL_DIR` pointed at a nonexistent directory (no embedding
backfill). Responses that say "still being built" are retried, never
recorded. Each run waits for `index.phase` = `ready` (all ten did) before
calling. Indexing was strictly sequential, one corpus and arm at a time; each
arm's daemon and its descendants were killed afterwards (0 left each time).

Corpora at the bench's pinned revs: gin `73726dc`, ripgrep `e89fff8`,
requests `6e83187` (cloned from GitHub into the scratchpad: no local copy
existed), task-tracker-mcp `35237c8` and excalidraw `1acf66e` (cloned from the
local repos at those revs).

- **A:** `find_definition(symbol_name)` for every distinct non-File/Module
  name. For each page that is ambiguous with exactly 4 candidates and no
  `hasMore`, also `find_definition(symbol_id)` for each of the 4 candidates.
  The second call is costed as the **mean** of those 4 follow-ups (the caller
  picks one; which one is unknown).
- **B:** the same names with exactly 4 candidates, plus up to 20 names each of
  2-3 and 5 candidates from A as a control.
- **Delta** per name = B page − (A page + second call + turn overhead).
- **Turn overhead:** the main table counts **0 B** for the turn (bytes of tool
  results only). The second call also costs its own `tool_use` request (~100-
  200 B) and a model turn that re-reads the whole context; the sensitivity rows
  charge 150 B and 300 B per turn. A context re-read is not modelled.

## Result

Counts match GM-352's (gin 16, ripgrep 44, requests 8, excalidraw 8;
task-tracker-mcp has none).

| corpus | 4-cand names | all 4 sourced on B | A page (med) | 2nd call (med) | B page (med) | **Δ med** | **Δ p90** | Δ < 0 | Δ med @150 B/turn | Δ med @300 B/turn |
|---|---|---|---|---|---|---|---|---|---|---|
| gin (Go) | 16 | 16/16 | 1,101 | 474 | 1,742 | **+61** | +292 | 7/16 | −89 | −239 |
| ripgrep (Rust) | 44 | 44/44 | 1,184 | 566 | 1,871 | **+134** | +572 | 19/44 | −16 | −166 |
| requests (Python) | 8 | 8/8 | 1,300 | 604 | 2,218 | **+321** | +710 | 1/8 | +171 | +21 |
| task-tracker-mcp (TS) | 0 | - | - | - | - | - | - | - | - | - |
| excalidraw (TS) | 8 | 8/8 | 1,198 | 608 | 2,328 | **+404** | +562 | 0/8 | +254 | +104 |

Bytes. Δ min/max: gin −87/+315, ripgrep −252/+1,770, requests −35/+952,
excalidraw +57/+563. ripgrep's tail is names whose four bodies are all long
(`various` B 4,159 B, `search` 3,684 B, `search_reader` 4,108 B).

**Control (arms differ only where intended).** In A no 4-candidate page carries
source (0 of 76); in B all 76 carry all 4. Every 2-, 3- and 5-candidate
control page is byte-identical between the arms (gin 23/23, ripgrep 40/40,
requests 24/24, task-tracker-mcp 7/7, excalidraw 24/24). Every A follow-up
carried `source` (76/76).

**Compared with the accepted ≤3 trade.** GM-352 measured a sourced ≤3 page at
+329 to +933 B (median, per corpus) over the unsourced one, against a
653-752 B single answer: net −400 to +290 B per saved call. Raising to 4 adds
+641 to +1,130 B per page (B − A medians) against a 474-608 B follow-up by
`symbol_id`: net +61 to +404 B. So 4 is a worse trade than 3 was, and in bytes
alone it is not free on any corpus's median.

Not measured: whether a caller needs any candidate's source at all (when the
file path alone picks the reading, B's extra +641 to +1,130 B buys nothing),
and the token cost of a turn in a real session.

## Machine state

Load was very high throughout (another agent's suite): `uptime` load averages
44 → 716 (1-min) during the run, 63 at the end. Builds (`/usr/bin/time -p`,
sequential): A `real 148 / user 194 / sys 33` (seeded from the main checkout's
`target/debug`, same commit), B `real 132 / user 191 / sys 34` (seeded from
A's). Probe runs (driver process only; the daemon is a separate process, so
`user` is the driver's 0.4-2.8 s and the rest of `real` is waiting on
indexing and the daemon): task-tracker-mcp 39/44 s, gin 86/71 s, requests
191/165 s, ripgrep 226/159 s, excalidraw 908/644 s (A/B). Byte counts do not
depend on load. Not a performance claim.

## Recommendation

**Lean keep at 3 for bytes; raise to 4 only if a turn is valued at ≥ ~150-300
B.** In bytes alone, raising costs more than the call it saves on every
corpus's median (+61 Go, +134 Rust, +321 Python, +404 TS) and p90 (+292 to
+710). Charging a turn 150 B makes Go and Rust break even or better (−89,
−16; 9/16 and 23/44 names cheaper), where 60 of the 76 names sit; Python and
TS stay positive until a turn costs more than ~320-400 B. If a saved turn
(its re-read of the whole context) is valued above ~400 B, which holds in any
session past a few thousand tokens, raising is a net win everywhere: 76 second
calls removed across the four corpora (ripgrep 171 → 127). The owner decides.
