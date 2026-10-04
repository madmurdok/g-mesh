# GM-493: embedding lifecycle of nodes re-sent by the semantic pass

Status: research note. Nothing is implemented; the options are for the owner to decide.

## Summary

A semantic pass re-sends structural nodes it did not parse (GM-486: to shorten
`untypedCalls`; TS: re-export targets and overload bindings). Core upserts them,
and then `round_trip` drops from the embedding step every re-sent node that
already has a vector (`core/src/watcher/apply.rs:446-448`, `retain`).

Two findings:

1. **Cost.** On the pass path core embeds 4,225 re-sent nodes with no vector.
   Compared with leaving all of them to the backfill, that is **+364 embeddings
   (+4.5%)** and about **+19 s real (+5%)**, which is roughly one run-to-run spread.
   The count is deterministic. The time is not distinguishable from noise.
2. **Correctness.** If a re-sent node carries text newer than core's (TS plugin
   reads the declaring file from disk), the row gets the new text and keeps the old
   vector. Reproduced for TS: the vector stays stale until the declaring file is
   reparsed, and without a watcher event that is unbounded. The Rust/Python SDK
   path has the same shape in code but is **not reproduced**.

The retain is what makes the stale vector permanent. Dropping pass-time embedding
(question 1) saves 364 embeddings but does not touch it.

## 1. Measurement: pass-time embedding, A vs B

Commit `35758d7` (release-3.21.0 tip), one machine, one script, order A B A B.
Corpus: `git archive 35758d7` (274 `.rs`), identical for all runs.

- **A (current):** the pass embeds re-sent nodes that have no vector.
- **B (measurement only):** after the retain, `diff.upsert_nodes.clear()` inside the
  `ControlMessage::SemanticPass` branch of `round_trip`, so the pass embeds nothing
  and the backfill gets every re-sent node. Edges untouched; the diff is already committed.

Procedure (as GM-486 S17): `/usr/bin/time -p g-mesh reindex` in the corpus, fresh
`G_MESH_HOME` per run, only the Rust plugin discovered
(`G_MESH_PLUGIN_ROOTS_OVERRIDE`, `semantic_pass = true`), rust-analyzer 1.97.1.
`reindex` runs in-process and returns after the semantic pass and the embedding
backfill. Builds: `cargo build --release -p g-mesh -p g-mesh-plugin-rust`.

Binaries (sha256):

| Binary | sha256 |
|---|---|
| A `g-mesh` | `c6eaa9327f042d60ba955b3fbaeb24f37f59753bc1b574c21c36cb8f0881d563` |
| B `g-mesh` | `1ab90512307e439c90e78edfed70141f23cf7ef56b5551c2588861e610ea8dd7` |
| A and B `g-mesh-plugin-rust` (identical) | `5bf2b887c97114699614a38391929b248d7bcbf785b82bf149e51b2adfb896e9` |

Results:

| run | real s | user s | sys s | pass s | pass node upserts | pass embed texts/embedded/s | backfill texts/hits/embedded/s | embedded total | vectors | load 1m before/after |
|---|---|---|---|---|---|---|---|---|---|---|
| 1A | 366.53 | 1138.82 | 7.45 | 64 | 7529 | 4225/4225/148 | 5055/801/4254/140 | 8479 | 9280 | 6.22/8.01 |
| 2B | 349.48 | 1090.13 | 7.27 | 64 | 7529 | 0/0/0 | 9280/1165/8115/275 | 8115 | 9280 | 8.01/6.20 |
| 3A | 384.56 | 1217.31 | 8.68 | 63 | 7529 | 4225/4225/158 | 5055/801/4254/152 | 8479 | 9280 | 6.20/7.86 |
| 4B | 363.87 | 1108.16 | 8.15 | 70 | 7529 | 0/0/0 | 9280/1165/8115/281 | 8115 | 9280 | 7.86/6.67 |

Machine state: `uptime` at start 6.22/41.67/65.44 (tail of the build), at end
6.67/7.03/17.17. `user` is about 3x `real` in every run, so the runs are CPU-bound,
not waiting.

Means: A real 375.5 s, user 1178 s, embedding 299 s. B real 356.7 s, user 1099 s,
embedding 278 s.

**A - B: +364 embeddings (8,479 vs 8,115, +4.5%), +18.9 s real (+5.3%), +79 s user (+7%).**

- The 4,225 re-sent nodes embedded on the pass path in A had 0 embedding-cache hits
  there. In B the backfill embeds them and its cache hits rise 801 -> 1,165 (+364),
  exactly the A-B difference. The pass path misses a cache the backfill would hit.
- Spread inside an arm: real 18 s (A), 14 s (B). The time delta is about one spread;
  the count delta is deterministic.
- An earlier run on `84a82eb` gave +358 embeddings and +69 s at load ~16. The count
  reproduces (+364); at load 6-8 the time cost is ~19 s, so most of the +69 s was
  probably load.
- Same final state in every run: 20,309 nodes, 9,280 vectors, 11,029 nodes without a
  vector (non-embeddable kinds). Dropping pass-time embedding changes cost, not the result.

Caveats: one corpus, one language plugin (Rust), two runs per arm.

## 2. The stale-vector path

### Code path (TS plugin, `plugins/typescript/src`)

- `index.ts:117` `handleSemanticPass` -> `runSemanticPass` (`semanticPass.ts:375`), a
  fresh `ProjectIndex` per pass (`:882`).
- `extractionOf` (`:921`) -> `extract` (`:929`): tries `cachedExtraction`
  (`incremental.ts:385`, the text core was last told about). On a miss it reads the
  file from disk (`fs.readFile` `:937`) and parses it (`extractFile` `:950`). The
  result lives for that pass and is never written to `fileStates`.
- `fileStates` only holds files this long-lived control process got a `fileChanged`
  for. The bulk walk is a separate process, so after a daemon start every file not
  edited since is a cache miss.
- Two places re-send a target node built this way: re-export upgrade
  (`askUpgrade` `:667` -> `declarationAt` `:689` -> `out.upgradedEdge` `:699`/`:1175`)
  and overload binding (`askBinding` -> `declarationBindingAt` `:738` ->
  `out.boundEdges` `:765`). `PassOutput.finish` (`:1245`) puts those nodes, with disk text,
  into `upsertNodes`.

The disk text is newer than core's when the declaring file changed and core has not
reparsed it yet:

- its watcher event is queued behind the importer's round trips (300 ms debounce,
  `daemon/mod.rs:73`; events are processed one at a time);
- it was edited while no daemon ran (nothing re-walks on restart);
- a whole-project pass after the cold walk finds files edited after the walk read them.

### Core (`core/src/watcher/apply.rs`)

- `apply_file_change_in` (`:120`) runs `round_trip(FileChanged)`, then
  `apply_semantic_pass_in` (`:226`) runs `round_trip(SemanticPass)`.
- `round_trip` (`:400`): `:435` `apply_diff_linked` upserts the node row with the new
  signature/docComment and keeps the existing `vectors` row. `:446-448`
  `nodes_with_vectors` (`:469`) + `retain` drops the node because it has a vector.
  `compute` (`:454`) and `store_vectors` (`:459`) never see it.
- Result: new text, old vector. The GM-396 re-check in `EmbeddingPipeline::store`
  (`pipeline.rs:646`) does not help, since nothing is stored for this node.
- The `vectors` table (`schema.rs:508`) holds only `nodeId`, `embedding`,
  `embeddingVersion`. Nothing identifies the source text, so staleness was proven
  against a fresh index of the final text.

### Reproduction (TS, worktree at `35758d7`, S1's release binaries)

Setup: `G_MESH_HOME` under a short path (a longer one made the socket path 177 bytes,
over the 103 limit); `G_MESH_PLUGIN_ROOTS_OVERRIDE` -> a dir holding only a
`typescript` symlink; `G_MESH_MODEL_DIR=~/.g-mesh/models/jina-embeddings-v2-base-code`;
`G_MESH_EMBEDDING_CACHE=off`.

Project:

- `decl.ts`: `/** Adds two numbers together and returns the sum. */ export function foo(a: number, b: number): number`
- `barrel.ts`: `export { foo } from "./decl";`
- `a.ts`: `import { foo } from "./barrel"; export function useIt() { return foo(1, 2); }`
- `tsconfig.json`

Edit to `decl.ts` (same line layout): `/** Parses an ISO date string into epoch milliseconds, throwing on bad input. */ export function foo(input: string, strict: boolean): number`.

Query:

```sql
SELECT n.id, n.signature, n.docComment, v.embedding FROM nodes n
LEFT JOIN vectors v ON v.nodeId = n.id
WHERE n.name='foo' AND n.filePath='decl.ts' AND n.kind='Function';
```

(sha256 of `v.embedding` and cosine similarity compared across snapshots.)

**Run A, deterministic** (`repro.sh`, existing test holds): `g-mesh init`; start the
daemon with `G_MESH_PLUGIN_HOLD_DIR` and `G_MESH_ROUND_TRIP_HOLD_COMPUTE_FILE`; create
`semantic-typescript.hold`; edit `a.ts` (plugin parks at the start of the `a.ts` pass);
edit `decl.ts`, touch `compute.hold`, release the plugin hold; query at compute hold #2
(`a.ts` semantic round trip, after `apply_diff`) and #3 (`decl.ts` `fileChanged`, after
the semantic round trip finished); release; query after the `decl.ts` reparse.
Reference = `g-mesh init` on a copy of the final files in a separate home.

```
V0    sig='foo(a: number, b: number): number'           sha=e9f72279668fe97b
Q1    sig='foo(input: string, strict: boolean): number' sha=e9f72279668fe97b  (after semantic apply_diff)
Q2    sig='foo(input: string, strict: boolean): number' sha=e9f72279668fe97b  (semantic round trip done)
Q3    sig='foo(input: string, strict: boolean): number' sha=ff9255169621dfb8  (after decl.ts reparse)
Vref  fresh index of the final text                     sha=ff9255169621dfb8
cos(v0,v2)=1.000000  cos(v2,vref)=0.157674  cos(v3,vref)=1.000000
```

Daemon log: no `embeddings [semanticPass]` line for the `a.ts` pass (the retain emptied
it); the next line is `embeddings [fileChanged]: 1 texts, 1 embedded` for `decl.ts`,
which is the heal. Order: `semantic pass requested for: a.ts` -> `1 edge(s) answered`
-> `file changed: decl.ts`.

**Run B, no test knobs** (`repro2.sh`): `g-mesh init`, no daemon; edit `decl.ts`; start
the daemon, edit only `a.ts`.

```
Q-daemon-up    old sig, sha=e9f7...
Q1 after pass  NEW sig, sha=e9f7...
Q2 60 s later  NEW sig, sha=e9f7...  (still stale)
```

Control for a future fix: drop the retain at `apply.rs:446-449`; Q2's sha must equal
Vref's.

Scripts and outputs: `repro.sh`, `repro2.sh`, `vec.py`, `repro.out`, `repro2.out`,
`run/daemon.log`, `run2/daemon.log` in the S2 scratch dir (`gm493/s2/`).

### When it heals, and the window

Heals when the declaring file goes through `apply_file_change_in` ->
`round_trip(FileChanged)` (no retain). In TS that reparse is always a cache miss, so
`reparseFile` returns the full extraction and the node is re-embedded (Run A Q3 = Vref).
Triggers:

- the file's watcher event;
- `staleness::ensure_fresh` (`staleness.rs:155`), only on a file-anchored tool call on
  that file (`find_definition`, `get_file_outline`, `get_dependencies`);
  `search_code`, which uses the vector, never triggers it;
- `g-mesh reindex`.

Does not heal: the backfill (`backfill.rs:222` picks only nodes with no vector or a
different `embeddingVersion`), and a later semantic pass (retained again).

Window: with normal editing, the rest of the semantic round trip + 300 ms debounce +
one reparse and embedding, typically under 1 s to a few s, bounded by the semantic
pass timeout. With the daemon down during the edit, or a missed watcher event:
unbounded (Run B: at least 60 s).

### Rust/Python SDK bridge: code reading only, NOT reproduced

- The bridge re-sends structural nodes only in `trim_untyped_calls` (`bridge.rs:1499`,
  called at `:1675`, `:1735`), cloned from `SdkIndex`, not read from disk during the
  pass. The placeholders carry no text to embed.
- The same stale path appears to exist: `Session::hydrate` (`plugins/sdk/src/run.rs:478`)
  fills `SdkIndex` from disk for in-scope files the process has not seen; in the
  whole-project pass after a cold walk, that is every file. A file edited between the
  walk and `hydrate` gets a trimmed `untypedCalls` node re-sent with newer text, and
  the retain keeps the old vector.
- It may be worse than TS: `hydrate` writes that disk text into the cache, and
  `file_changed` returns an empty diff for identical text (`run.rs:523`), so the later
  reparse sends nothing. Stale until the file changes again, the plugin restarts, or a
  reindex. That file's other structural rows would stay at the walk's text too.
- Per-file passes look safe (scope = the just-reparsed file, so the cache matches core).
- Reproducing needs rust-analyzer and an edit timed between the walk and `hydrate`
  (the `bulk` hold point would do it).

## 3. Options

The three research questions: (1) should the pass embed at all; (2) pass/backfill
order; (3) whose text wins for a re-sent node, core's or the disk read.
Options are not exclusive; the table at the end shows what each fixes.

### Option 1: leave every re-sent node to the backfill (arm B, made permanent)

Pass-time embedding removed; the retain and `nodes_with_vectors` call go away
(or the diff is cleared as in arm B).

- Benefit: -364 embeddings (-4.5%) on this corpus, ~19 s (about one spread) less;
  one embedding path for these nodes; the backfill's cache-hit rate applies.
- Risk/cost: re-sent nodes with no vector are searchable only after the backfill runs,
  not at the end of the pass; the gap is not measured here (in `reindex` the backfill
  runs right after, so the end state is identical, 9,280 vectors). If the backfill is
  deferred or cut short, pass-created nodes lack vectors longer.
- Fixes: cost only. Leaves open: the stale vector. A re-sent node that has a vector
  is still not re-embedded, because the backfill only picks nodes with no vector or
  a different `embeddingVersion` (`backfill.rs:222`).

### Option 2: keep the pass embedding, re-embed a re-sent node when its text changed

Compare the re-sent node's embedding text with the stored one (needs a text hash
or the source text stored with the vector; `vectors` has neither, `schema.rs:508`)
and re-embed on mismatch instead of dropping every node that has a vector.

- Benefit: fixes the stale vector for TS and SDK in all three trigger cases
  (queued event, daemon down, cold-walk race), whichever side's text is newer.
- Risk/cost: schema change (a text/hash column, a migration, backfill of existing
  rows) or recomputing the embedding input for each re-sent node and comparing it
  to something. Without a stored hash the only check is re-embedding, which is the
  cost the retain exists to avoid ("recompute every such caller's unchanged vector",
  the comment at `apply.rs:438-443`). Adds a per-pass comparison cost, roughly the
  size of the 7,529 re-sent upserts in S1's run.
- Fixes: the stale vector. Does not reduce embeddings; may add some. The SDK case
  where core's text is behind the disk text stays inconsistent in other columns.

### Option 3: fix at the source, core ignores text fields on semantic re-sends

For a node already in the index, a `SemanticPass` upsert keeps core's stored text
fields (signature, docComment, etc.) and applies only the fields the pass owns
(`untypedCalls`, edges). New ids in the diff are still inserted whole.

- Benefit: row and vector can never diverge from a semantic re-send; core's text
  stays the single truth (answers question 3 with "core"); keeps the retain valid
  as written (its premise, "a semantic answer carries no new text", becomes true by
  construction). Covers TS and SDK in one place, with no plugin change.
- Risk/cost: touches `apply_diff` semantics for one message type; needs a field list
  (new text fields added later must be classed as pass-owned or not); a node core
  has no row for needs the disk text, so the new-id path must stay. If the declaring
  file is genuinely newer and core never reparses it, core keeps the older text until
  its own reparse (same as before, but consistently).
- Fixes: the stale vector, with no embedding-count change.

### Option 4: fix at the source, plugin does not send disk text

TS: re-sent nodes use `cachedExtraction` text only, or are sent as id-only
placeholders (the SDK bridge's placeholders already carry no text).
SDK: `hydrate` must not feed re-sent nodes from disk text core has not seen.

- Benefit: no core change; the retain's premise holds per plugin.
- Risk/cost: every plugin needs its own change and must be kept honest (new
  plugins can reintroduce it). For TS, a cache miss means the node has no text to
  send, so the pass either skips the upgrade/binding edge or reads text and
  discards it; the edge target may then lack a row. Hardest for SDK `hydrate`,
  which is the cache's design.
- Fixes: the stale vector for the plugin changed. Leaves open: any other plugin
  and any later re-sender.

### Order of pass and backfill (question 2)

Today the pass runs first and embeds what it re-sends; the backfill runs afterwards
for nodes still without a vector. Backfill-first, then pass: the backfill sees only
nodes the walk wrote, so re-sent nodes already have vectors and the retain drops them
as now; it would not reduce the count (the re-sent nodes with no vector in A are
those the pass itself creates) and does not touch the stale path. Measured here only
as pass-first (A) and pass-without-embedding (B); backfill-first was **not measured**.

### What each option fixes

| Option | Saves embeddings | Fixes stale vector (TS) | Fixes SDK path | Schema/protocol change |
|---|---|---|---|---|
| 1 drop pass-time embedding | yes (-364, -4.5%) | no | no | none |
| 2 re-embed on text change | no (may add) | yes | yes | likely (hash) |
| 3 core ignores text on re-sends | no | yes | yes | none (apply semantics) |
| 4 plugin sends no disk text | no | yes (per plugin) | only if SDK changed | plugin contract |
| 1 + 3 | yes | yes | yes | none |

## 4. Recommendation (not a decision)

The evidence favours Option 3 for correctness: it is the only one that fixes both
plugins without a schema change and makes the retain's comment true. Option 1 is a
separable cost saving of about 4.5% embeddings and about one spread of time; on its
own it leaves the stale vector, so it should not be justified as a fix. Option 1 and
Option 3 combine. Option 2 is the heavier route and fits only if core must stay
tolerant of plugin text. Open before choosing: reproduce the SDK path (the possibly
permanent variant), and measure how much later pass-created nodes become searchable
under Option 1 outside `reindex`.
