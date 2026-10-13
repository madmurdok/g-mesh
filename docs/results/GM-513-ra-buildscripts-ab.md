# GM-513 S7: rust-analyzer build scripts / proc macros A/B (null result)

Design note section 2, "S2 A/B". Verdict: **the A/B measured nothing**. The arms
are identical and the control observation does not differ.

## Setup

- Arm A = g-mesh `90a2d87` (base), arm B = `d869cf1`. Debug builds of `g-mesh` and
  `g-mesh-plugin-rust`; each arm ran its own binaries with its own `G_MESH_HOME`.
- rust-analyzer 1.97.1 (8bab26f4 2026-07-14), same binary in both arms.
- Corpora (copied out of any task worktree): (1) g-mesh source at `90a2d87`;
  (2) the `rfx` build-script/proc-macro fixture (`app` + `mac`, see design note).
- Run: `g-mesh reindex` in the corpus copy (walk + semantic pass, synchronous).
  Metrics read from `index.db` (`edges`, `untyped_calls`).
- Only the rust and go plugins were present beside the binaries; python/typescript
  failed to spawn in both arms (same in both, irrelevant to rust counts).

## g-mesh corpus, rust-analyzer semantic edges

| metric | A (90a2d87) | B (d869cf1) |
|---|---|---|
| CALLS resolved | 2926 | 2926 |
| CALLS unresolved | 30 | 30 |
| REFERENCES resolved | 2920 | 2920 |
| SUPERTYPE_OF resolved | 29 | 29 |
| untyped_calls rows / nodes | 315 / 252 | 315 / 252 |
| rust pass upserts | 9304 nodes / 10010 edges, 135 retracted | identical |
| rust pass wall time | 153.6 s | 142.0 s |
| `reindex` real / user / sys | 891.03 / 2541.73 / 28.72 s | 828.76 / 2404.52 / 26.91 s |
| `target/` under indexed root | absent | absent |

rfx corpus: both arms 3 files, 1 node, 0 edges upserted; pass 15.8 s (A) vs 11.1 s (B);
reindex real/user/sys 23.13/14.49/3.82 (A), 18.24/12.81/2.77 (B); no `target/`, no
`MARK_buildrs` / `MARK_procmacro` file in either arm.

## Control observation: did not differ

- `find_implementations Serialize` on g-mesh: both arms return "Nothing is named
  'Serialize'" (semantic-neighbour fallback). Index has no resolved SUPERTYPE_OF edge
  to a node named Serialize in either arm.
- `find_definition generated` on rfx: both arms "no symbol named 'generated'".
- Arm A also ran no build script and wrote no `target/`, so base behaviour in this
  harness did not exercise build scripts either.

So the control is not an A>B difference; per the note, a null result means the A/B
measured nothing. Likely causes (not verified): the index holds only project nodes,
so edges into serde's derive output or `generated()` (macro-expanded, no source node)
cannot appear in it in either arm; and in arm A rust-analyzer apparently never
completed build-script/proc-macro loading before the pass (no MARK files, no target).
The edge-count deltas B - A are 0 for every kind. The timing gap (A slower by ~11 s on
the pass) is within load noise (see below).

## Machine state (`uptime` before each run)

- A gmesh: load 35.68 59.20 62.54 (heavily loaded; embedding backfill dominates real time)
- A rfx: 13.71 18.79 32.80
- B gmesh: 11.36 17.84 32.05
- B rfx: 14.91 16.46 21.98

Timings are not comparable between arms (load differed 3x); the real time is mostly
embedding (590 s in one A pass), user >> real.

## Reproduction notes

Harness scripts and logs: scratchpad `.../ab/` (`build.sh`, `run.sh`, `mcp.py`,
`summary.txt`). Queries used `mcp-shim` with `G_MESH_HOME` symlinked to a short path
(unix socket path limit 103 bytes).
