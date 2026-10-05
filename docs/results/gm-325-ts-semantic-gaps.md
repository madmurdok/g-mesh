# GM-325: TypeScript semantic-tier gaps, measured

Measure slice GM-325/S33, run 2026-10-05 against `abe9d1d` (integration tip of
`feat/GM-324-325-ts-port`). Spec: `docs/architecture/gm-325-typescript-lsp-semantics.md`
§9 steps 1-2, plus the owner's question of 2026-10-05 about cold-start latency
under `typescript.tsserver.useSyntaxServer = "never"`. No code or setting was
changed.

## Headline

- **The 10 s per-request budget is exceeded on every cold start on excalidraw.**
  In 7 of 7 cold passes the first window of 8 in-flight questions timed out
  (8 timeouts without `node_modules`, 16-24 with them, 32 on a loaded
  machine). vtsls answers no `definition` at all until tsserver has loaded the
  project of every opened file: 10.5-14.3 s without dependencies installed,
  25-34 s with them. Each timeout makes the whole-project pass `incomplete`, so
  `semanticPassAt` is never recorded and **every daemon start re-runs the whole
  pass and times out again** (run 8). The answered edges are kept.
- **With the warm-up (S41, `b299235`): 0 timeouts in 6 of 6 cold passes, every
  pass recorded complete, no re-run at daemon start** (before: 8-32 timeouts,
  0 of 7 complete, re-run at every start). The first answer waited 13.2-18.9 s
  without `node_modules`, 29.1-38.0 s with them, all inside the 120 s budget
  (§5).
- Per site kind, the semantic tier answers `Reference` and `OverloadCall`
  questions almost entirely into the index (97-100%); `ReceiverCall` is where
  the gaps are: 37% empty without dependencies installed (3% with them), 30%
  (no deps) to 61% (deps) answered outside the index.
- **False-empty answers (§3): 3** of 28 273 positions, all caused by tsserver's
  automatic type acquisition (ATA) cache being cold on the very first run, all
  three landing outside the index when answered. **0** false-empty answers
  that change an edge. The §3 deferral re-asked 8-121 empties per pass; every
  re-ask came back empty again and was confirmed empty by the second pass.
- Fixture: the three arms behave as specified (vtsls 36/36 pass; structural
  exactly the 7 `tier = "semantic"` entries fail; no server: one report line,
  `session` fails, expectations not reached). 3 of the 16 traced rows of §1.3's
  kind did not land in the index: the uninstalled workspace package, a call
  into `lib.es5.d.ts`, and an untyped-JS receiver.

## Machine and method

- MacBook, Intel i7-1068NG7 (8 logical CPUs), 32 GiB, macOS 26.6.2, Node
  v20.6.1, vtsls 0.3.0 (`scripts/test-deps.sh typescript`, bundled TypeScript
  5.9.3). `cargo build --workspace --release`; every number below is from the
  **release** build.
- `uptime` load averages are recorded per run in the timing table. The machine
  was idle apart from these runs for runs 2-6 (1-minute load 3.9-5.0); run 1
  started while the load average was still decaying from the build (24, `ps`
  showed nothing busy), runs 7-8 ran while macOS XProtect scanned the freshly
  installed `node_modules` (load 10-12) and are reported separately.
- **excalidraw**: the corpus registered in `g-mesh-bench/corpora/registry.json`
  (`/Users/Valentin_Taiurskii/Projects/excalidraw` at `1acf66ed`), exported
  read-only with `git archive` into a scratch copy: 658 TS/JS files. It has no
  `node_modules`, which is how the bench indexes it. A second copy had
  `yarn install --frozen-lockfile --ignore-scripts` run in it ("deps" below),
  which is how a developer's checkout looks.
- Each pass is `g-mesh init` (or `g-mesh reindex`) in the corpus, so one cold
  vtsls per pass, with `G_MESH_HOME` in scratch, `G_MESH_MODEL_DIR` pointed at
  an empty dir (no embedding work in the timings), `CLAUDE_PROJECT_DIR` unset,
  wrapped in `/usr/bin/time -p` with every stderr line timestamped.
- **Per-question counts.** The bridge's pass log line carries totals only
  (files, nodes/edges upserted, retracted, duration), not per-kind counts. To
  get them, `vtsls` on `PATH` was a transparent proxy (`node`, ~50 lines) that
  forwards stdio unchanged and logs every LSP message with a millisecond
  timestamp. Each `textDocument/definition` (the only request the bridge sends
  for these kinds, `bridge.rs` `Ask::*`) was joined on `(file, line, col)` with
  the open sites the TS extractor records, dumped by a scratch binary linking
  `g_mesh_plugin_typescript` (`load_project` + `extract` over the same 658
  files: 40 944 sites). 28 271 of 28 273 asked positions matched; 2 228
  positions carry two site kinds and are counted once, under the first of
  `OverloadCall`, `Reference`+replaces, `ReceiverCall`, `Reference`. Answer
  classes are the *final* answer per position: `in index` = one location in an
  indexed project file (the bridge may still reject it as a target, e.g. an
  import binding, which is why the edge counts below are the measure of what
  landed); `lib.d.ts` = TypeScript's bundled `lib.*.d.ts`; `node_modules` =
  any other location outside the indexed files (the project's `node_modules`,
  or ATA's `~/Library/Caches/typescript/5.9`); `ambiguous` = more than one
  location; `timeout` = cancelled by the bridge at 10 s.
- Proxy overhead: run 2 (proxy) pass 48.3 s, run 3 (plain vtsls, same
  everything else) 41.0 s, same 8 timeouts and same upserted/retracted counts.

## 1. Fixture (`plugins/typescript/conformance`)

`g-mesh plugins check` on `plugins/typescript/conformance/project` with
`--expect conformance/expect.toml`, three arms run from the CLI the way
`tests/conformance.rs` configures them: the shipped manifest with vtsls on
`PATH`; the shipped manifest with `PATH=/usr/bin:/bin` (no vtsls, no `npx`);
and the manifest with `semantic_pass = false`, `receiver_calls = "unresolved"`
and no `[plugin.semantic]`, run without `--skip-semantic-expectations`.

| Arm | checks | expectations (file + 35) | result |
|---|---|---|---|
| vtsls | 14 pass, 1 skip (`semantic-pass-undeclared`, n/a) | 36 pass | PASS |
| no server | `session` FAIL, 13 pass, 2 skip | not reached (incomplete pass fails `session`, by design) | FAIL, 1 report line `the semantic engine could not be started (no usable vtsls ...)` |
| structural | 14 pass, 1 skip (`semantic-engine-lazy`, n/a) | 29 pass, **7 fail** | FAIL |

The 7 structural failures are exactly the `tier = "semantic"` entries:
`callers[1]` double (namespace call), `callers[2]` format (overload binding),
`callers[3]` Greetable#greet (interface receiver), `callers[8]` MenuGroup
(default import), `callers[9]` mutate in `amb/a.ts` (ambiguous barrel),
`callers[13]` Base#hello (`this`/`super`), `references[2]` target (namespace
member read). Every structural entry passes in all arms that reach them.

**What vtsls answered, per traced question** (`g-mesh init` on a copy of the
fixture through the logging proxy: 13 distinct questions, 11 semantic edges):

| Question | vtsls answer | Semantic edge in the index |
|---|---|---|
| `m.double(4)` namespace call (main.ts:32) | math.ts:6 | yes, `run`/`useNamespaceImport` -> `double` |
| `lib.target` namespace read (nsref/use.ts:4) | nsref/lib.ts:1 | yes, REFERENCES |
| `g.greet()`, `g: Greetable` (shapes.ts:25) | shapes.ts:7 (interface method) | yes |
| `this.hello()` / `super.hello()` (inherit/derived.ts) | inherit/base.ts:8 | yes, both |
| overload `format("a")` / `format(1)` (main.ts:38) | overload.ts:10 / :11 | yes, `toDeclaration` 0 and 1 |
| `export *` call `add` (main.ts:21) | math.ts:2 | yes |
| named re-export `twice` (main.ts:13) | math.ts:6 | yes |
| ambiguous `export *` `mutate()` (amb/use.ts:4) | amb/a.ts:2 | yes |
| default import `DropdownMenuGroup()` (defaults/use.ts:4) | defaults/menuGroup.ts:2 | yes |
| **workspace package `pointOf` via `@fx/geom`** (packages/app/src/root.ts:5) | root.ts:2, its own import binding | **no** (structural edge via `workspaces` stands) |
| **`xs.map(...)`** (callbacks.ts:10) | `lib.es5.d.ts:1470` | **no** (outside the index) |
| **`word.toUpperCase()`, untyped JS param** (util.js:9) | `[]`, re-asked once, `[]` again | **no** (stays an untyped call) |

Candidate gaps from the fixture: 3 (uninstalled workspace package; definition
outside the index; untyped-JS receiver).

## 2. excalidraw, per open-site kind

Open sites the extractor records (all 658 files): `ReceiverCall` 22 997,
`Reference` with `replaces` 11 616, `Reference` without 160, `OverloadCall`
6 171. The bridge asks only the hop sites it cannot settle structurally and
only the overload calls whose target is really overloaded, so 28 273 distinct
positions are asked (28 281-28 394 requests including re-asks).

### Without `node_modules` (the corpus as the bench indexes it)

Run 2 (quiet machine); run 1 in parentheses where it differs.

| Kind | asked | in index | lib.d.ts | node_modules / ATA | empty (final) | of which re-asked | ambiguous | timeout |
|---|---|---|---|---|---|---|---|---|
| ReceiverCall | 22 995 | 6 964 (30%) | 6 723 | 94 (91) | 8 577 (8 580) (37%) | 8 (121), all still empty | 629 | 8 |
| Reference + replaces | 2 298 | 2 278 | 0 | 0 | 0 | 0 | 20 | 0 |
| Reference, no replaces | 160 | 160 | 0 | 0 | 0 | 0 | 0 | 0 |
| OverloadCall | 2 818 | 2 785 | 0 | 0 | 0 | 0 | 33 | 0 |
| unmatched position | 2 | 0 | 0 | 0 | 2 | 0 | 0 | 0 |

### With `node_modules` installed

Run 5; run 4 in parentheses where it differs.

| Kind | asked | in index | lib.d.ts | node_modules | empty (final) | of which re-asked | ambiguous | timeout |
|---|---|---|---|---|---|---|---|---|
| ReceiverCall | 22 995 | 7 064 (31%) | 6 864 (6 861) | 7 160 (7 158) | 655 (653) (3%) | 13, all still empty | 1 237 (1 236) | 15 (23) |
| Reference + replaces | 2 298 | 2 093 | 0 | 159 | 0 | 0 | 45 | 1 |
| Reference, no replaces | 160 | 160 | 0 | 0 | 0 | 0 | 0 | 0 |
| OverloadCall | 2 818 | 2 719 | 0 | 16 | 0 | 0 | 83 | 0 |
| unmatched position | 2 | 0 | 0 | 2 | 0 | 0 | 0 | 0 |

### What landed: the bridge's pass line and semantic edges by kind

| | no server | vtsls, no deps | vtsls, deps |
|---|---|---|---|
| pass log line | `the semantic engine could not be started` (1 line) | `658 file(s), 5579 node(s)/13234 edge(s) upserted, 4226 edge(s) retracted ... (incomplete)` | `658 file(s), 5784 node(s)/13309 edge(s) upserted, 4225 edge(s) retracted ... (incomplete)` |
| semantic `CALLS` (of which overload-bound, `toDeclaration`) | 0 | 3 880 (219) | 3 974 (219) |
| semantic `REFERENCES` | 0 | 2 067 | 2 066 |
| semantic `SUPERTYPE_OF` | 0 | 1 | 1 |
| `untyped_calls` rows (receiver gap still listed) | 6 914 | 1 462 | 303 |
| `language_state.semanticPassAt` | null, error recorded | null, error recorded | null, error recorded |

The counts were identical across runs 1-3 and across runs 4, 5 and 7, so the
timeouts cost only the 8-32 sites they hit, not the rest of the pass.

### Second pass, diffed (false-empty exposure, §3)

Per position, the final answer of one cold pass against the next:

| Pair | positions that differ | detail |
|---|---|---|
| run 1 vs run 2 (no deps) | 3 | empty in run 1, answered in run 2 into `~/Library/Caches/typescript/5.9/node_modules/@types/...` (ATA): `scripts/build-node.js:14`, `scripts/woff2/woff2-esbuild-plugins.js:49`, `dev-docs/src/theme/ReactLiveScope/index.js:12` |
| run 4 vs run 5 (deps) | 8 | all 8 are timeouts in run 4 (third timeout window) that run 5 answered; **0** empty-vs-answered |

The 3 false empties are not tsserver's project load (§3's exposure): run 1 was
the first vtsls run on this corpus and tsserver's automatic type acquisition
downloaded `@types/*` into the machine-wide cache during it (directories
timestamped 21:02:01-21:02:08, inside run 1); run 2 answered from that cache.
All three answers land outside the index, so no edge differs. Every empty
answer the bridge re-asked after its settle (8-121 per pass) was empty again
and stayed empty in the other run.

### Timing

`real`/`user`/`sys` are `/usr/bin/time -p` around the whole `g-mesh` command.
"pass" is the bridge's own duration from its log line.

| Run | corpus | command | load avg before (1/5/15 min) | bulk walk done | pass | real | user | sys | 10 s timeouts |
|---|---|---|---|---|---|---|---|---|---|
| 1 | no deps | init, proxy | 24.2 / 23.1 / 16.6 (decaying) | 8.2 s | 54.5 s | 70.5 | 26.7 | 8.0 | 8 |
| 2 | no deps | reindex, proxy | 3.9 / 12.4 / 13.4 | 5.8 s | 48.3 s | 60.8 | 22.4 | 6.8 | 8 |
| 3 | no deps | reindex, plain vtsls | 4.2 / 10.8 / 12.8 | 5.9 s | 41.0 s | 53.3 | 16.7 | 4.1 | 8 |
| 4 | deps | init, proxy | 4.0 / 9.2 / 12.0 | 6.2 s | 82.6 s | 96.4 | 22.9 | 7.3 | 24 |
| 5 | deps | reindex, proxy | 5.0 / 8.1 / 11.3 | 7.2 s | 66.5 s | 80.8 | 21.7 | 7.5 | 16 |
| 6 | no deps | init, **no server** | 8.7 / 6.9 / 9.9 | 9.0 s | - | 15.3 | 10.5 | 2.9 | - |
| 7 | deps | init, proxy | **12.4** / 8.2 / 10.1 (XProtect) | 12.1 s | 135.4 s | 160.4 | 37.3 | 11.8 | 32 |
| 8 | deps | daemon start retrying run 7's pass | 10.2 / 11.9 / 11.6 | - | 73.3 s | - | - | - | 16 |

`real` is 2.0-3.3x `user + sys` with a server and 1.15x without one. The wait
is on tsserver, which is a grandchild (`g-mesh` -> plugin -> vtsls -> tsserver)
whose CPU `time -p` does not see: the proxy's `ps` at `shutdown` read
tsserver's CPU time as 2:11 (run 4), 1:43 (run 5), 1:50 (run 8) min - more CPU
than the whole pass's wall time - at 2.70-2.84 GB RSS, under vtsls's
`--max-old-space-size=3072`. So the seconds of `real` are tsserver loading
projects and answering, not idle waiting.

## 3. Cold start against the 10 s request budget (owner's question)

With `useSyntaxServer = "never"` the one tsserver handles the 555 `didOpen`s
the bridge sends in the first 0.26 s, loading each opened file's project in
turn (11-15 `Initializing '<tsconfig>'` progress spans: root, `dev-docs`,
`examples/*`, `packages/*`), and answers no `definition` until it is through.
The bridge has 8 questions in flight from 0.26 s.

| Run | projects loaded (last progress `end`) | first real answer | timeouts (windows of 8) | answers 5-10 s | answers 2-5 s |
|---|---|---|---|---|---|
| 1, no deps | 24.0 s | 10.5 s | 8 (1) | 0 | 8 |
| 2, no deps | 12.8 s | 14.3 s (asked at 10.3, 4.0 s) | 8 (1) | 0 | 8 |
| 4, deps | 33.1 s | 34.5 s | 24 (3) | 0 | 16 |
| 5, deps | 24.0 s | 25.2 s | 16 (2) | 2 | 22 |
| 7, deps, load 12 | - | - | 32 (4) | - | - |
| 8, deps, daemon retry | - | - | 16 (2) | - | - |

- **The budget is exceeded on every cold start**: never fewer than 8 timeouts,
  all in the first 10-30 s, then essentially every answer arrives in under
  1 s (28 212-28 349 of ~28 300 requests).
- Loading takes 12.8-24 s without `node_modules` and 24-33 s with them, i.e.
  1.3-3.3x the per-request budget. The number of timed-out windows is about
  `floor(load time / 10 s)`.
- The timed-out questions are the first ones asked, in path order
  (`.lintstagedrc.js`, `dev-docs/*`, `examples/*`): 8-24 sites, all
  `ReceiverCall` except one `Reference`+replaces. Their files are owed to the
  next pass.
- Consequence: the whole-project pass is reported incomplete, so the index
  never records `semanticPassAt`; `g-mesh status` shows `semantic pass:
  typescript failed`, and each daemon start logs `the project was walked but its
  semantic pass never completed - retrying it` and runs the whole 70-80 s pass
  again against a cold vtsls, which times out again (run 8: 16 timeouts). The
  edges it did get are kept, and tool answers carry the `provenance` hint.

## 4. Without a server (excalidraw, `PATH=/usr/bin:/bin`)

- **One log line** from the plugin: `[typescript] the semantic engine could not
  be started (no usable vtsls: vtsls (PATH): ... node_modules/.bin/vtsls ...`
  (once per process; core then logs that the pass failed, quoting it). The
  plugin's exit line still reads `semantic engine started: true`.
- **Provenance block**: `find_callers` on `Scene#getNonDeletedElements`
  through `g-mesh mcp-shim` answered 0 rows with
  `"provenance": {"language": "typescript", "semanticTier": "absent"}` and the
  hint `this language's semantic pass has not finished, so method calls through
  a variable receiver may be missing here`.
- **The receiver gap stays listed**: the same answer carries
  `untypedReceiverCalls: {count: 44, files: [12 files]}`; the index holds 6 914
  `untyped_calls` rows. With vtsls the same symbol has 47 (no deps) / 49 (deps)
  semantic `CALLS` callers and 2 / 0 untyped calls left.
- Bulk walk to done: 15.3 s `real`, 10.5 s `user`.

## 5. Cold start with the warm-up (S41)

Measure slice GM-325/S41, run 2026-10-05 against `fc89083` (`b299235`: the
bridge keeps one question in flight under a warm-up budget until the server's
first answer, refusal or timeout; the TypeScript plugin sets 120 s). Same
corpus (`1acf66ed`, fresh `git archive` copies, 655 TS/JS files on disk, 658
in the pass), same `deps` copy (`yarn install --frozen-lockfile
--ignore-scripts`), same procedure as §2-3: `g-mesh init` then `g-mesh
reindex` (one cold vtsls each), `G_MESH_HOME` in scratch, `G_MESH_MODEL_DIR`
empty, release build of this worktree, vtsls 0.3.0 from
`scripts/test-deps.sh typescript` behind a transparent stdio proxy that logs
one compact line per LSP message (method, id, timestamp; no payloads).

**Machine state differs from §2-3.** Other sessions were building on the same
machine: 1-minute load 8-118 during these runs (§2-3: 3.9-5.0), and the bulk
walk took 10-29 s instead of 6-9 s. Pass durations are therefore not
comparable with §2's; timeouts, completion and the first answer's wait are the
quantities this section answers.

| Run | corpus | command | load avg before (1/5/15) | bulk walk done | warm-up line | first question asked | first answer (wait) | projects loaded (last progress `end`) | pass | real | user | sys | timeouts | answers 2-5 s / 5-10 s | recorded |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| S1 | no deps | init | 21.2 / 46.2 / 40.8 (build ending) | 10.6 s | yes | 0.28 s | 15.03 s (**14.7 s**) | 14.0 s | 81.1 s | 106.1 | 32.5 | 9.2 | **0** | 8 / 0 | complete |
| S2 | no deps | reindex | **103.0** / 65.1 / 48.6 | 10.0 s | yes | 0.41 s | 19.27 s (**18.9 s**) | 17.5 s | 64.2 s | 91.0 | 30.1 | 8.9 | **0** | 8 / 0 | complete |
| S3 | no deps | reindex | 34.3 / 52.9 / 45.6 | 29.3 s | yes | 0.34 s | 13.55 s (**13.2 s**) | 12.4 s | 48.0 s | 87.8 | 23.4 | 7.0 | **0** | 0 / 0 | complete |
| S4 | deps | init | 10.6 / 39.2 / 41.1 | 24.4 s | yes | 0.30 s | 29.39 s (**29.1 s**) | 27.9 s | 80.0 s | 115.4 | 23.5 | 7.7 | **0** | 16 / 0 | complete |
| S5 | deps | reindex | 8.1 / 29.0 / 36.8 | 23.7 s | yes | 0.39 s | 36.99 s (**36.6 s**) | 35.0 s | 110.2 s | 144.2 | 28.6 | 9.0 | **0** | 23 / 0 | complete |
| S6 | deps | reindex | 19.2 / 23.9 / 33.4 | 15.9 s | yes | 0.35 s | 38.40 s (**38.0 s**) | 37.1 s | 95.6 s | 121.8 | 30.9 | 9.9 | **0** | 16 / 0 | complete |

Times in the "asked / answer / loaded" columns are from the vtsls process's
start (proxy clock). "answers 2-5 s" excludes the warm-up question.

- **Every cold pass is recorded complete.** 0 `did not answer` lines in 6 of 6
  passes (§3: 8-32 in 7 of 7); the pass line carries no `(incomplete)`;
  `init`/`reindex` print `semantic: pass complete`, and `g-mesh status` shows
  `semantic pass: complete` for both corpora.
- **The warm-up line appeared once per pass**, as the bridge's first question
  went out: `[typescript] the server has not answered yet - its first question
  may take up to 120s (warm-up), the rest 10s`.
- **The first answer waited 13.2-18.9 s without `node_modules` and 29.1-38.0 s
  with them** - the project load, which ends 1.1-2.0 s before it (last
  `$/progress end`, 11 `Initializing` spans as in §3). The slowest is 32% of the
  120 s budget, at a load average up to 103. The first question is
  `.lintstagedrc.js` and its answer is empty, as before; the second is sent
  within 1 ms of it and answered in 43-202 ms, so the pipeline fills to 8 at
  once. No answer after the warm-up took 5 s or more.
- **No re-run at daemon start.** After S3 (no deps) and S6 (deps), a daemon
  bootstrapped by `g-mesh mcp-shim` and given a `get_file_outline` call
  (answered in 0.2 s) logged no `semantic pass never completed - retrying`
  line and spawned no vtsls (0 proxy logs); `g-mesh stop` then stopped it.
  §3's run 8 re-ran the whole pass and timed out 16 more questions.
- **Waiting, not computing.** `real` is 2.3-3.8x `user + sys`, the same shape
  as §2: the wait is on tsserver (a grandchild whose CPU `time -p` does not
  see) loading projects and answering, here also slowed by the machine's load.
  The warm-up itself spends 13-38 s of wall time in which the bridge has one
  question outstanding; before, the same seconds went to 10 s timeouts.
- Upserted counts are identical across runs of a corpus (no deps: 5 582 nodes
  / 13 234 edges, deps: 5 790 / 13 329; 4 226 retracted each).

## README gap entries (for the docs slice)

| Category | Fixture | excalidraw, no deps | excalidraw, deps |
|---|---|---|---|
| No server installed: structural answers only, receiver calls listed in `untypedReceiverCalls`, `provenance.semanticTier = "absent"` | arm passes 29/36, fails the 7 semantic entries | 6 914 untyped-call rows, 0 semantic edges | - |
| Uninstalled workspace package (answer is the import binding) | 1 (`pointOf` via `@fx/geom`; structural edge stands) | not separable here | not separable here |
| Definition lands outside the index (`lib.d.ts`, `node_modules`) | 1 (`xs.map`) | 6 817 ReceiverCall | 14 024 ReceiverCall + 159 Reference + 16 OverloadCall |
| Receiver typed `any` / untyped JS (empty answer) | 1 (`word.toUpperCase()` in util.js) | 8 577 ReceiverCall (37%) | 655 ReceiverCall (3%) |
| Ambiguous answer (more than one location) | 0 | 682 (629 / 20 / 33) | 1 365 (1 237 / 45 / 83) |
| Computed members (`obj[k]()`) | not measured: the extractor records no open site for them, so they never reach the bridge | - | - |
| **Cold-start timeouts** (10 s budget, pass never recorded complete, retried at every daemon start) | 0 | 8 per cold pass before the warm-up; **0 with it** (6/6 passes complete, §5) | 16-24 per cold pass (32 under load) before; **0 with it** (first answer 29-38 s of a 120 s warm-up) |
| False-empty answers (§3) | 0 | 3, ATA cache cold, 0 affecting an edge | 0 |

Two observations for the docs slice beyond §9's list: the semantic tier needs
Node and pays a 13-33 s project load per cold vtsls on a project of this size;
and on a project without `node_modules`, tsserver's automatic type acquisition
downloads `@types/*` into `~/Library/Caches/typescript/<ver>` (network access,
machine-wide cache) during the pass - these runs did so on this machine.

## Raw data

Not committed (scratch): per-run stderr logs with timestamps, the proxy's LSP
logs (ndjson, ~20 MB each), the per-site joins, the open-site dump, the
analysis script and the proxy. The proxy and dumper are under 60 lines each and
are reproducible from the method above.
