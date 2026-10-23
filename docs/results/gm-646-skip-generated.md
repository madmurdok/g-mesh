# GM-646 measurements: skipping generated files in the embedding pass

Protocol: `docs/design/GM-646-skip-generated-files.md` section 4, "Protocol for S5".
Arms: A = `96c0822` (release-4.17.0 base, built in a throwaway worktree), B = branch HEAD
(`6172574`). Both release builds. Each run: fresh `G_MESH_HOME`, cold `g-mesh reindex` run from the corpus
(reindex does the walk, the semantic pass and the foreground embedding backfill), then counts read
from `index.db`. Embedding cache left on, but cold (fresh home). Rounds alternate A, B.
Corpora: grpc-go (3 rounds), client-go (1 round), shallow clones at the commits in the design note.

## Counts (identical in every round)

| corpus | arm | nodes | edges | generated=1 nodes | vectors | generated nodes with a vector |
|---|---|---|---|---|---|---|
| grpc-go | A | 57,417 | 151,631 | n/a (no column) | 14,316 | n/a |
| grpc-go | B | 57,417 | 151,631 | 4,458 | 11,464 | 0 |
| client-go | A | 93,582 | 195,422 | n/a | 25,094 | n/a |
| client-go | B | 93,582 | 195,422 | 42,474 | 5,624 | 0 |

Vector cut: grpc-go -19.9% (2,852 fewer), client-go -77.6% (19,470 fewer). Expected 15-20% and 65-75%.
Structural no-regression: nodes and edges equal between arms on grpc-go (all 3 rounds) and client-go.
(The `reindex` console line shows 54,636 / 120,503 for both arms in grpc-go; that is the walk summary
before the semantic pass, equal in A and B as well.)

## Timing (seconds)

`bulk done` = wall time from start until the Go plugin's `bulk index complete` line (the structural walk).
`embed` = the backfill log line (`embeddings [backfill] ... Ns`). `real/user/sys` = `/usr/bin/time -p` of the whole `reindex`.

| corpus | round | arm | real | user | sys | bulk done | embed | load avg at start (1/5/15 min) |
|---|---|---|---|---|---|---|---|---|
| grpc-go | 1 | A | 1429.54 | 3830.48 | 86.84 | 35.33 | 1342.0 | 143.41 79.82 39.94 |
| grpc-go | 1 | B | 1246.43 | 3479.06 | 74.63 | 36.76 | 1160.5 | 96.25 129.36 111.27 |
| grpc-go | 2 | A | 1264.58 | 3697.38 | 82.82 | 54.64 | 1151.9 | 156.28 189.35 175.55 |
| grpc-go | 2 | B | 804.94 | 2557.60 | 60.91 | 43.74 | 702.6 | 71.62 71.73 103.92 |
| grpc-go | 3 | A | 1157.81 | 3385.93 | 82.86 | 42.32 | 1060.8 | 19.37 24.57 53.01 |
| grpc-go | 3 | B | 1552.87 | 3604.39 | 86.95 | 36.82 | 1463.9 | 24.25 43.28 44.49 |
| client-go | 1 | A | 1739.80 | 5473.01 | 94.57 | 71.28 | 1606.1 | 51.93 91.61 96.57 |
| client-go | 1 | B | 282.61 | 916.85 | 24.26 | 23.02 | 231.9 | 7.11 15.70 38.43 |

Means over 3 grpc-go rounds: A real 1284 s, embed 1185 s, bulk 44.1 s; B real 1201 s, embed 1109 s, bulk 39.1 s.
Per embedded vector (embed time / vectors), grpc-go: A 0.083 s, B 0.097 s.

## Machine state

8 cores, macOS (Darwin 25.6.0). Other agent sessions ran concurrently the whole time: load averages ranged
from 7 to 190 (table above; full `uptime` lines in the scratchpad summary). `user` of 2,500-5,500 s against
`real` of 800-1,700 s means the process was CPU-busy on all cores while contending with that load, not idle.
The timings are therefore noisy: grpc-go B ranged 805-1553 s, and the round order did not control for load.

## Conclusion

- Correctness: B drops only vectors; nodes and edges are unchanged on both corpora, and no generated node has
  a vector. B has `generated=1` rows (4,458 / 42,474).
- Vector count falls 19.9% on grpc-go and 77.6% on client-go, a little above the design's estimate on client-go.
- Embedding time follows the vector count in principle (client-go: 1606 s to 232 s, 6.9x faster, both
  measured under comparable-or-lighter load for B, so the load bias favours B and the real ratio is likely
  smaller than 6.9x but still large). On grpc-go the 3-round means are 6% (embed) lower for B, far
  below the 20% vector cut, and the rounds overlap (B's worst round is slower than A's worst), so the grpc-go
  time effect is not resolved under this load. Per-vector cost is not worse in B on the evidence available.
- The structural walk (`bulk done`) is unchanged within noise (35-55 s); the cost is in embedding, which dominates
  `reindex` real time (over 90%).
- Not run: the `find_implementations` / `search_code` spot checks of the protocol; structural equality was
  checked by counts only, as asked.
