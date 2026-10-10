# GM-523: `get_file_outline` response sizes, before and after

Measured 2026-10-09 by the GM-523 measure slice (S9).

- Before: release-4.3.0 at `31da724`.
- After: `feat/GM-523-outline-size-budget` at `3328ea7`.

Both arms ran real release binaries over MCP stdio.

## Results

"Bytes" is the serialized JSON body of one response: the `{results, ...}`
text block that the 8,000-byte budget bounds. "Calls" is the number of
calls needed to read the whole outline by following `nextCursor`. The
`hasMore`/`total` column describes the first response. The before arm has no
`detail` parameter: its rows are always the old full 10-field rows, and the
server ignored `detail: "full"`.

### `plugins/sdk/src/lsp/bridge.rs` (g-mesh, 193 symbols)

| call | before: bytes (first / max) | rows (first) | hasMore / total | calls | after: bytes (first / max) | rows (first) | hasMore / total | calls |
|---|---:|---:|---|---:|---:|---:|---|---:|
| defaults | 4,985 / 7,254 | 20 | true / - | 10 | 7,907 / 7,907 | 59 | true / 193 | 4 |
| `limit: 200` | **54,975** / 54,975 | 193 | false / - | 1 | **7,907** / 7,907 | 59 | true / 193 | 4 |
| `detail: "full"` | 4,985 / 7,254 (ignored) | 20 | true / - | 10 | 7,755 / 7,976 | 31 | true / 193 | 8 |
| `detail: "full", limit: 200` | 54,975 / 54,975 (ignored) | 193 | false / - | 1 | 7,755 / 7,976 | 31 | true / 193 | 8 |

### excalidraw@1acf66ed `packages/excalidraw/components/App.tsx` (228 symbols)

| call | before: bytes (first / max) | rows (first) | hasMore / total | calls | after: bytes (first / max) | rows (first) | hasMore / total | calls |
|---|---:|---:|---|---:|---:|---:|---|---:|
| defaults | 4,812 / 7,360 | 20 | true / - | 12 | 7,921 / 7,938 | 55 | true / 228 | 5 |
| `limit: 200` | **59,764** / 59,764 | 200 | true / - | 2 | **7,921** / 7,938 | 55 | true / 228 | 5 |
| `detail: "full"` | 4,812 / 7,360 (ignored) | 20 | true / - | 12 | 7,917 / 7,934 | 34 | true / 228 | 9 |
| `detail: "full", limit: 200` | 59,764 / 59,764 (ignored) | 200 | true / - | 2 | 7,917 / 7,934 | 34 | true / 228 | 9 |

### Per-page bytes and rows (bytes, rows)

- After, bridge.rs, compact: (7907, 59) (7843, 58) (7832, 52) (3850, 24).
- After, bridge.rs, full: (7755, 31) (7762, 28) (7870, 30) (7919, 30) (7976, 26) (7901, 22) (7756, 23) (1124, 3).
- After, App.tsx, compact: (7921, 55) (7938, 54) (7853, 53) (7868, 51) (2187, 15).
- After, App.tsx, full: (7917, 34) (7775, 27) (7702, 27) (7775, 27) (7882, 24) (7697, 21) (7697, 23) (7934, 22) (6753, 23).

In the after arm, every page reports `total`, which matches the row count: 193 and 228. Following the cursor returned every row once: the per-page rows add up to 193 and 228. Compact rows have 6 keys and full rows have 10.

### Other observations

- `tools/list` from the project daemon is 10,974 B before and 11,099 B after (+125 B). That is under the 11,800 B ceiling. The ceiling is defined on the front's list, which was not measured here.
- No guidance prefix or "answered from project" text block came back. Each result had exactly one text block, the JSON body.
- The whole MCP `result` object is larger than the body, because the body is escaped inside a JSON string. After the change, the largest `result` is 9,034 B (bridge.rs, compact). Before, it was 60,826 B (bridge.rs, `limit: 200`). AC1 is about the body, which is what the budget bounds. A client that counts the wrapped `result` sees about 13% more bytes.
- Cross-check: the design note's appendix script, run on each arm's fresh index DB, reproduced the note's "today" numbers exactly. bridge.rs has 193 rows, 54,975 B at `limit: 200`, and 59 compact rows fit in 8k. App.tsx has 228 rows, 59,754 B at `limit: 200`, and 55 compact rows fit in 8k. So both arms indexed the same rows as the note's measurement. The live before-arm App.tsx response is 59,764 B, 10 B over the estimate: this is the real cursor length the note warned about.

## Verdict

- **AC1 (every response <= 8,000 B, any `limit`/`detail`): met.** Across 4 call shapes on 2 files, the after arm made 52 calls. Its largest response was 7,976 B (bridge.rs, `detail: "full"`). The before arm reached 54,975 B and 59,764 B at `limit: 200`.
- **AC4 (before/after sizes on bridge.rs and App.tsx): recorded above.** The control observation separates the arms clearly. The `limit: 200` response drops from 54,975 B to 7,907 B on bridge.rs and from 59,764 B to 7,921 B on App.tsx.
- **Trade-off:** reading a whole outline now takes more calls than a `limit: 200` call did before. It takes 4 instead of 1 on bridge.rs and 5 instead of 2 on App.tsx; with `detail: "full"`, it takes 8 and 9. It takes fewer calls than the old default: 4 instead of 10 and 5 instead of 12.

## Method

1. **Builds.** The after arm was built in the task worktree. The before arm was built in a throwaway `git worktree` of `31da724`, whose `target/release` was an APFS clone (`cp -c -R`) of the task worktree's. The release-4.3.0 checkout was not touched. Both arms used `cargo build --release -p g-mesh -p g-mesh-plugin-{rust,typescript,python}`. The script checked each arm's HEAD and clean tree before building. Both binaries report `g-mesh 4.3.0`. The pre-existing binaries were stale (`4.2.0`), so neither was reused.
2. **Corpora.** g-mesh came from `git archive 31da724`. bridge.rs is the same in both arms. excalidraw came from `git archive 1acf66edabc2ac5bbd4aed0714aed7dca7cc2aab` of `~/Projects/excalidraw`, the registry's local corpus. `g-mesh-bench/corpora/excalidraw` holds only fixtures and `tasks.json`, not a checkout, so it was not used. Nothing was cloned from the network. Each arm indexed its own APFS clone of each export.
3. **Isolation.** Each arm had its own `G_MESH_HOME` under `$TMPDIR` (`.../T/gma`, `.../T/gmb`). A scratchpad path is longer than the 103-byte AF_UNIX socket limit, so it could not be used. The binaries were copied into a per-arm scratch `bin/` so that every daemon and plugin process could be found by path. The run used `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, so rust-analyzer and vtsls were not found and no semantic tier ran. Outline rows (DEFINES) come from the structural pass, and the appendix cross-check above confirms the rows are the same as the note's. Other settings: `G_MESH_INDEX_WAIT_CAP_MS=0`, so the first call waited for indexing, and `G_MESH_PROGRESS_INTERVAL_MS=0`.
4. **Calls.** A Python MCP client spawned `g-mesh mcp-shim` with `CLAUDE_PROJECT_DIR` set to the corpus copy. It sent `initialize` and then `tools/list`. Then it ran each call shape, following `nextCursor` until `hasMore` was false, and recorded the bytes of each response's JSON text block. The client was `client.py` in the session scratchpad. It was driven by one fail-fast script per arm, and the after arm ran first as the dry run.
5. **Cleanup.** After each arm, the script ran `g-mesh stop` and then `pgrep -f` on the arm's scratch path, killing any match with `kill -9`. Nothing was left either time. The throwaway worktree and the temp homes were removed afterwards.

## Machine state

Another agent was running a full `cargo nextest` suite at the same time, so the machine was heavily loaded. Bytes and rows do not depend on load. The times below are given for the record only.

| step | `uptime` load (1/5/15 min) at start | `/usr/bin/time -p` real / user / sys |
|---|---|---|
| after build (`3328ea7`) | 17.25 / 65.49 / 139.78 | 202.52 / 33.46 / 4.65 s |
| after: index g-mesh + 24 calls | 194.27 / 203.54 / 185.19 | 24.34 / 0.12 / 0.10 s (client only) |
| after: index excalidraw + 28 calls | 138.71 / 189.77 / 180.76 | 20.04 / 0.12 / 0.09 s (client only) |
| before build (`31da724`) | 140.01 / 184.24 / 179.18 | 236.29 / 35.69 / 5.57 s |
| before: index g-mesh + 22 calls | 330.61 / 262.44 / 214.18 | 25.60 / 0.12 / 0.10 s (client only) |
| before: index excalidraw + 28 calls | 264.02 / 252.38 / 211.96 | 16.68 / 0.12 / 0.09 s (client only) |

In the build rows, `real` is far above `user`: about 200 s against 34 s. Most of that is waiting, either on the shared target lock or on CPU contention from the concurrent suite. In the client rows, `user` is 0.12 s, so `real` is the client waiting on the daemon: indexing time plus call time. The daemon's own CPU time is not included in these numbers.
