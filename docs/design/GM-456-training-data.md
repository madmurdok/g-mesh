# GM-456: training-data pipeline (design)

Slice GM-456/S1. This is a read-only analysis of `feat/GM-456-training-data-pipeline` at
54250276 (the 4.28.0 version bump). I ran no builds and no generation. Repo
pins were read with `gh api` and `git ls-remote` on 2026-11-10. Prices come
from the claude-api skill's model table (cached 2026-10-06).

The output is (natural-language query, code, hard negatives) rows for GM-436
(query adapter, asymmetric LoRA, distillation into gte-small) and GM-452
(fine-tuning). The rows come from pinned public repos that no eval set uses.
Two pieces of work came after the task text, and the note covers both:

- **GM-460 v2 eval.** `eval/embedding/v2/queries/<corpus>.jsonl` holds the
  GM-398 bytes, then the GM-422 confirm bytes, then `v2/new/queries` (270
  new positives and 68 new absent queries). New targets are in
  `v2/new/targets/`. The v2 eval uses the same six corpora at the same pins
  as GM-398 and GM-422 (`v2/corpora.toml`), so repo disjointness is
  unchanged. The leak guard reads all three studies.
- **GM-698 CE text.** The product now builds two different texts:
  - Embedder: `text_to_embed(doc, sig)`, the structured trim (ADR 0012,
    `core/src/embedding/text.rs:99`). There is no context header, because
    GM-455 failed Q4.
  - Cross-encoder: E + P + B48 (ADR 0042, `core/src/embedding/ce_text.rs:54`).
    It also needs `nativeKind`, the line span and the source lines.

  The pipeline stores the raw fields and renders both forms (section 5).

## Decisions for the owner (summary)

| # | decision | recommendation |
|---|---|---|
| D1 | Repo set and per-language counts (section 1) | 24 repos. 4,000 targets: Rust 1,600, Go 800, Python 800, TS 800 |
| D2 | Generator: API batch or Claude Code agents (sections 2, 2b) | **Changed by the owner to "Агенты Claude Code"**: headless `claude -p` per pack on the subscription, no tools, Opus 5.5 generates, Sonnet 5.5 judges (section 2b). Cost is measured by `/usage` around the 100-target pilot. The API route (section 2) is superseded |
| D3 | Code near-duplicates: shingle Jaccard instead of embeddings (section 4) | Shingles for code; embeddings and word Jaccard for queries |
| D4 | Teacher scores (section 6) | **Defer to GM-436.** Store the candidate lists and mining scores here |
| D5 | Data location (section 5) | Outside git, at `~/Projects/ClaudeProjects/g-mesh-data/gm456/<version>/`. Commit the card, the manifest and a sample |

## 1. Source repos

**Rule.** Every repo meets four conditions:

- It is not one of the six eval corpora: task-tracker-mcp, excalidraw,
  ripgrep, g-mesh, gin, requests. These are the same in GM-398, GM-422 and
  GM-460 v2, and g-mesh-bench's `corpora/registry.json` holds the same set.
- It is not a fork of an eval corpus. The guard compares root commits,
  section 4.
- It is not a near-clone of one. httpx is excluded because its API mirrors
  requests. chi and echo are excluded as gin-like routers, to keep the Go
  eval about generalisation.
- Its licence is permissive: MIT, Apache-2.0 or BSD. mdBook (MPL-2.0) was
  dropped for that reason.

Pins are release tags where a release exists, read as the peeled commit.
Otherwise the pin is HEAD on 2026-11-10.

| lang | repo | ref | commit | licence | split |
|---|---|---|---|---|---|
| rust | serde-rs/json | v1.0.154 | 20572a6d066958542d79cdd6cf1a7cd9f0c8e1f8 | Apache-2.0 (dual MIT) | train |
| rust | tokio-rs/axum | axum-v0.8.9 | c59208c86fded335cd85e388030ad59347b0e5ae | MIT | train |
| rust | rayon-rs/rayon | v1.12.0 | c9ced185ae3508246a9eb70c8407a1199bb1b77f | Apache-2.0 (dual MIT) | train |
| rust | sharkdp/bat | v0.26.1 | 979ba22628bc9d8171f2cffca2bd5c90c9fc0a9e | Apache-2.0 (dual MIT) | train |
| rust | tokio-rs/tracing | tracing-0.1.44 | 2d55f6faf9be83e7e4634129fb96813241aac2b8 | MIT | train |
| rust | tokio-rs/bytes | v1.12.1 | 76c0fbb54ed4336caf9d2311658a2f4a5627c21d | MIT | train |
| rust | hyperium/http | v1.5.0 | 16fc9a7b840c2181e7f8b37397c107b0ffcd050d | Apache-2.0 (dual MIT) | **dev** |
| rust | tokio-rs/mini-redis | HEAD | 3d93b42bc363220f85af4fc9e1bebd35b588a4a3 | MIT | **dev** (S2 pilot repo) |
| go | spf13/cobra | v1.10.2 | 88b30ab89da2d0d0abb153818746c5a2d30eccec | Apache-2.0 | train |
| go | urfave/cli | v3.14.0 | 2f64589904992914a49b1c1869eba269e580bc5d | MIT | train |
| go | spf13/viper | v1.21.0 | 394040caccbdf5821fa6839386a35f0fb1b1ee9e | MIT | train |
| go | sirupsen/logrus | v1.10.2 | 6d6a132bc03324d4ceb78e1b927f995d014cda20 | MIT | train |
| go | BurntSushi/toml | v1.6.0 | 52534926c55b4cd85b05aee90569dd0668b8cf30 | MIT | **dev** |
| go | golang/groupcache | HEAD | 2c02b8208cf8c02a3e358cb1d9b60950647543fc | Apache-2.0 | **dev** |
| python | pallets/click | 8.5.1 | cf19179e8e2fd54c601378cec9e0d21bb12b6fb1 | BSD-3-Clause | train |
| python | python-attrs/attrs | 26.1.0 | 7bfc49e9b22d5ba25b6e429524c3d49fee27cb36 | MIT | train |
| python | Textualize/rich | v15.0.0 | 6ac483cbea39cab124dfd3483bba70ffafb71050 | MIT | train |
| python | psf/black | 26.10.1 | 258e7dbf348bc23cb70b8cdf693481c04634ebcc | MIT | train |
| python | more-itertools/more-itertools | v11.2.1 | 4aeb4338ae1407d8da9723e86da99efeb446f719 | MIT | **dev** |
| ts | colinhacks/zod | v4.6.7 | b500a2ee95134f81d74da7911d7378dd2853d042 | MIT | train |
| ts | immerjs/immer | v11.2.9 | 75a290e44ba14bd723c8ef8546b6559c8ef3ae88 | MIT | train |
| ts | honojs/hono | v4.13.17 | 61f7c59e50244ab4d2dd83c151928cf7ea448510 | MIT | train |
| ts | pmndrs/zustand | v5.1.0 | 9fed2eb2f3af672da9f7bf694b996df3831fc207 | MIT | train |
| ts | sindresorhus/ky | v2.2.0 | 3541888878c4d303a8bc2ad61815f4e048f55a5d | MIT | **dev** |

"Dual MIT" is my reading of the repos' licence files: GitHub reports only one
SPDX id. S2 records the licence file path and its sha256 at the pin.

**Counts.** Targets come first and are stratified 6 fn / 3 type / 1 other,
using `sample_targets.STRATA_PATTERN` (`eval/embedding/sample_targets.py:41`)
with seed 456.

- Per language: Rust 1,600 (40%, which is jina's weak language, r@10 0.34),
  Go 800, Python 800, TypeScript 800, so 4,000 targets.
- Per repo: the quota is proportional to the repo's frame (embeddable,
  non-test nodes), capped at 30% of its language.
- Dev repos hold about 15-20% of each language.

The frames are not yet measured. S6 step 0 indexes every repo with the model
off, which is cheap, and fixes the quotas. If Rust's frame is below about
3,000 nodes, swap in another permissive Rust repo rather than raising the
quotas.

| option | benefit | risk |
|---|---|---|
| **a. 24 repos, 4,000 targets, Rust 40% (recommended)** | Many repos per language, so the model learns no single repo's style. The dev split is by repo | 24 clones and indexes. A few repos (rich, bat) are large |
| b. 2 repos per language, 2,000 targets | Half the cost and setup | The model can memorise repo idiom, and the dev split has one repo per language |

## 2. Generator

> **Superseded in part.** The API route below (Message Batches, batch ids, API cost model, option table) is replaced by section 2b. The per-target content, the prompt rules and the `overlap`/`docEcho` checks still hold.

**Per target.** The model reads the full code span (capped at 150 lines),
the untrimmed doc, the file path and the parent. It never sees a trimmed
form, so a trim cannot leak into what the queries describe. It writes:

- 2 phrases (3-7 words) and 2 sentences (10-25 words), in D3's shapes
  (`docs/architecture/embedding-eval.md` D3 step 2): behaviour, not
  identifiers, written as an agent that does not know the name would type
  them;
- 1 paraphrase of each sentence;
- `answersAlso`, other symbols it believes answer equally. These are a hint
  only. The judge decides.

That gives 6 texts per target, about 24,000 query rows. The prompt says:

- do not copy doc-comment phrasing;
- do not use identifier sub-tokens.

After generation, the D3 rule 6 `overlap` flag is computed per query with
`check_queries.overlap` (`eval/embedding/check_queries.py:68`). A language
below 50% `overlap = false` gets its offending queries regenerated once, and
the result is recorded.

`docEcho` is also recorded: the word Jaccard between the query and the doc's
first paragraph. The card reports it next to the same number for the v2 eval
queries. This measures the task's "LLM style bias" risk.

**Prompt.** The prompt is `eval/training/prompts/generate.md` and is
committed, with its sha256 in the manifest. Its few-shot examples are
hand-written over a toy snippet, never taken from an eval file. The guard
checks the prompt too (section 4).

**Sampling parameters.** On API version 2026-10-21, `temperature`, `top_p`
and `top_k` return a 400 on every model, so there is no temperature or seed
to record. The manifest records instead:

- model id and `effort`;
- API version;
- prompt sha256;
- batch id and per-request `custom_id`;
- the raw response of every request, kept on disk as the cache of record.

Seed 456 fixes target order and batch composition. The LLM output is not
reproducible; the stored responses are the reproducible artefact.

**Cost model.**

- Generation, per target: about 1,050 input tokens (span, doc, header, and
  the system prompt amortised over 10 targets per request) and about 320
  output tokens (6 texts, JSON, low-effort thinking).
- Judging, per target: about 3,800 input tokens (10 candidates at about
  250 tokens each, the positive, the 6 queries, the system prompt) and about
  550 output tokens (a candidate x query verdict matrix, thinking).
- Totals for 4,000 targets:

  | step | input | output |
  |---|---|---|
  | generation | 4.2M | 1.3M |
  | judging | 15.2M | 2.2M |

Prices per million tokens (input/output): Opus 5.5 $4/$20, Sonnet 5.5
$2/$10, Haiku 5.5 $0.10/$0.50. Message Batches cost 50%.

| full run, batch prices | Haiku 5.5 | Sonnet 5.5 | Opus 5.5 |
|---|---|---|---|
| generation | $0.5 | $10.7 | $21.4 |
| judging | $1.3 | $26.2 | $52.4 |

The recommended mix is generation on Opus 5.5 and judging on Sonnet 5.5:
about **$48**, or **about $62 with 30% margin** for regeneration and
estimation error. The pilot is 100 targets (2.5%) and costs **about $1.5**.
The per-target token counts are estimates (+-2x). The pilot reads the real
counts from `usage` and re-prices the full run before it starts. That is the
gate the task asks for.

**Subscription route (Claude Code agents).** An agent does not pay per
request; it re-reads its whole context on every turn:

- One agent per 25-target pack: about 6-10 turns at about 30-40k context,
  so about 0.3M re-read tokens and about 10k output per agent.
- Generation: 160 agents, about 50M re-read tokens.
- Judging: about 160 more agents, about 50M.
- In total roughly 100M tokens, mostly cache reads, against the subscription
  limit.

I cannot convert this to a share of the limit without a measurement. If this
route is chosen, the pilot runs 4 agents (100 targets), the owner reads the
`/usage` delta, and that delta x 40 is the full-run cost. Agents also run in
a checkout that contains `eval/embedding`, so "the generator never sees the
eval queries" becomes a brief rule instead of a structural fact.

| option | benefit | risk |
|---|---|---|
| **a. API Message Batches, Opus 5.5 generates, Sonnet 5.5 judges (recommended)** | Exact recorded cost. Prompt and responses stored byte for byte. The generator structurally cannot read eval files. No agent re-read overhead | Needs an API key and about $60 of spend. The batch can take up to 24 h |
| b. Claude Code agents on the subscription | No API dollars | About 100M tokens of limit (to be measured). Not blind to eval files by construction. Agent output is less uniform |
| c. API, Haiku 5.5 for both | About $2 | Lower query quality and a likely higher false-negative rate. S7 would measure it, but after the money and time are spent |

## 2b. Generation and judging on Claude Code (replaces D2's API route)

The owner chose "Агенты Claude Code": generation and judging run on the
Claude Code subscription, with no API key. Flags below come from
`claude --help` of Claude Code 2.1.319 (`claude --version`, 2026-11-10). One
probe was run (a schema-constrained "ok=true" on Sonnet, no tools, in an
empty scratch dir): exit 0, 4.8 s real, and the result JSON shape quoted in
2b.4.

**Recommendations:** a Python driver that runs one headless `claude -p` per
pack with **no tools at all**; generation packs of 10 targets on Opus 5.5,
judging packs of 5 targets on Sonnet 5.5, both `--effort low`; 3 calls at
once; the CLI's `structured_output` is wrapped into a message dict that the
existing `parse_message` functions read unchanged.

### 2b.1 Driver

| option | benefit | risk |
|---|---|---|
| **a. Script: one `claude -p` per pack (recommended)** | Runs in `run_pipeline.sh` in the background with no orchestrator. The prompt is the whole input, so no tools are needed and the call is one model turn plus the structured-output turn (the probe reported `num_turns: 2`). Per-pack JSON with usage. Resumable per file | Each call pays CLI start-up (about 2 s) and a small fixed context. Subscription rate limits can stop a run midway (handled by 2b.4) |
| b. Packs dispatched as subagents by the orchestrating session | No new driver code | About 1,200 dispatches (400 + 800) or 300+ with large packs: every result lands in the orchestrator's context and is re-read on each later turn. Subagents run in a checkout containing `eval/embedding` and have tools, so blindness is a brief rule only. Output shape is not schema-enforced |

### 2b.2 Pack size and models

| step | pack | calls (4,000 targets) | model | effort | why |
|---|---|---|---|---|---|
| generate | 10 targets (`PER_REQUEST`, unchanged) | 400 | `claude-opus-5-5` (`--model`) | `--effort low` | `build_requests` already groups 10; about 10k input and 3-4k output per call; a bad pack costs 10 targets |
| judge | 5 targets (today 1) | 800 | `claude-sonnet-5-5` | `--effort low` | 1 target per call would be 4,000 calls whose fixed context is a large share of 3.8k input. The schema gains `t` (target index) per verdict |

Thinking stays the CLI default (adaptive); only effort is set. `--fallback-model`
is **not** used: a silent model switch would mix generators. An option for
the owner: judging on Opus too (higher quality, more limit), decided after
the pilot from the S7 false-negative check.

### 2b.3 Eval blindness

Structural, not a brief rule. Each call runs with:

- `--tools ""`: no built-in tools, so nothing on disk is readable;
- `--strict-mcp-config` and no `--mcp-config`: no MCP servers (the g-mesh
  MCP would otherwise expose indexed code);
- `--setting-sources ""`, `--disable-slash-commands`: no user/project
  settings, hooks or skills;
- `--system-prompt <prompts/generate.md text>`: replaces the default system
  prompt;
- cwd = a fresh empty dir under `$TMPDIR/gm456-<version>/<pack>/`, outside any
  git repo, so no project `CLAUDE.md` is discovered;
- `--no-session-persistence`, `--permission-prompts none`, and the input
  piped on stdin.

`--bare` would also drop auto-memory and the user `CLAUDE.md`, but it needs
`ANTHROPIC_API_KEY`, so it is not usable here.

Check, once per run, recorded in the manifest: a probe call with
`--output-format stream-json --verbose` and the same flags. Its `init`
event must list `tools: []` and `mcp_servers: []`. If not, the run stops.
Per pack: `permission_denials == []` and `num_turns <= 2`. **To confirm in
the code slice:** whether the user `~/.claude/CLAUDE.md` and auto-memory still
reach the context (the probe's 3.4k input tokens suggest not). They contain no
eval text, but they cost tokens per call. The leak guard (section 4) still
runs on every output.

### 2b.4 Output contract, failures, concurrency

Call: `claude -p --model M --effort low --output-format json --json-schema
<SCHEMA as JSON> --system-prompt <prompt> <flags of 2b.3>`, user message
(today's `params.messages[0].content`) on stdin. The CLI returns one JSON
object. The fields used are `subtype`, `is_error`, `api_error_status`,
`terminal_reason`, `structured_output`, `usage`, `modelUsage` (keyed by model
id), `total_cost_usd`, `num_turns`, `session_id` and `duration_ms`. The driver
turns it into the dict `parse_message` already reads:

```
{"model": <only key of modelUsage>, "stop_reason": "end_turn" | "refusal",
 "content": [{"type": "text", "text": json.dumps(structured_output)}],
 "usage": <usage>}
```

So `generate.parse_message` (:162) and `judge.parse_message` (:130) stay as
they are. The judge's version maps `t` to the pack's target.

| outcome | detected by | action |
|---|---|---|
| success | `subtype == "success"`, `structured_output` present | write `responses/gen/<custom_id>.json` (tmp file + rename), parse |
| refusal | `stop_reason`/`terminal_reason` says refusal | record in `*-skipped.jsonl`, never retried |
| malformed / missing `structured_output`, wrong counts | parse problems | recorded; packs are retried only by an explicit `--retry <custom_id>...` after a person reads the reason |
| rate or usage limit (`api_error_status` 429, limit text) | `is_error` | **stop the whole run** (no sleep loop); resume later |
| transient (5xx, overloaded, CLI crash) | non-zero exit or 5xx | one retry, logged with the reason; a second failure is recorded and the run continues |

Resumability: a pack whose `responses/<step>/<custom_id>.json` exists is
never rerun; failures go to `<custom_id>.err.json` and do not block a resume.
Concurrency: 3 calls at once (`--jobs 3`, a thread pool of subprocesses),
each under `taskpolicy -b`. The limit is the subscription, not the CPU, so
low priority mainly means few calls at a time and stopping at the first
limit error.

### 2b.5 Cost measurement

- Pilot (100 targets: 10 gen calls, about 20 judge calls): the owner reads
  `/usage` before and after. Full-run estimate = delta x 40, plus 30%
  margin. If the delta shows the run would not fit in the owner's window,
  the owner splits the run over several days (resume handles that).
- Per pack, `responses/<step>-usage.jsonl` logs `custom_id`, `usage`
  (input, cache creation, cache read, output, `thinking_tokens`),
  `num_turns`, `duration_ms` and `total_cost_usd`. The probe shows
  `costBasis: "list"`: it is a list-price equivalent, not money spent, and
  it gives a second extrapolation to check the `/usage` one against.
- The summary prints totals per step, so the pilot gives tokens per target to
  compare with section 2's estimates (+-2x).

### 2b.6 Reproducibility record

The manifest's `generation`/`judging` blocks lose `apiVersion` and batch ids
and store: `driver: "claude-cli"`, `claudeVersion` (`claude --version`), the
model id from `modelUsage`, effort, prompt sha256, schema sha256, the exact
argv template, pack size, the blindness probe's `init` tools/MCP lists, and
the sha256 of every `responses/<step>/<custom_id>.json` (the raw CLI output
with `session_id` is the artefact of record). The output is not reproducible;
the stored outputs are.

### 2b.7 Edit map for the code slice

| where | change |
|---|---|
| new `eval/training/claude_cli.py` | `run_pack(request, schema, prompt, cwd_root, model, effort)` builds argv (2b.3), runs `claude -p`, returns the CLI JSON; `to_message(cli_json)` (2b.4); `classify(cli_json, rc)`; `run_packs(requests, step_dir, jobs)` with skip-if-done, the retry rule and stop-on-limit; `probe_blindness()` |
| `generate.py` docstring (:1-28), `API_VERSION` (:43), `client` (:207) | Replace `--submit/--collect` with `--run [--jobs N] [--retry ID...]`. Drop `API_VERSION` and `client`. `build_requests` (:90) is unchanged |
| `generate.py main` (:215, submit/collect branch :246-281) | `--run`: `claude_cli.run_packs`, then the existing parse loop over the stored files via `to_message` |
| `judge.py` `SCHEMA` (:41), `build_requests` (:76) | Group 5 targets per request (`targets`, per-target `queries`/`candidates`), add `t` to the verdict items |
| `judge.py parse_message` (:130), `main` (:147, :183-214), import (:32) | Index by `t`; `--run` as in generate; import `custom_id` only |
| `export.py` manifest (:193-207) | Fields of 2b.6 in place of `apiVersion` (:203) |
| `run_pipeline.sh` (:9-14, :75-90) | A `--live` mode that runs `generate.py --run` and `judge.py --run` in-line (no hand-off stop); `--stub` unchanged |
| tests | `to_message`/`classify` on recorded CLI JSON fixtures (success, refusal, 429, missing output); a pack already done is not rerun; argv contains `--tools ""` and `--strict-mcp-config` |

**Code-slice end to end:** `run_pipeline.sh --version v0.1-cli --live --count
5 mini-redis`. That is 1 generate call on Opus (5 targets) and 1-2 judge calls
on Sonnet through real `claude -p`, plus the blindness probe, then export and
`leak_guard --check`. The run is re-launched once to show that no pack reruns.
Cost: a few cents of limit; `total_cost_usd` is logged.

## 3. Hard negatives

Each target gets one candidate list, shared by its 6 queries and capped at
12. Every candidate is labelled with **all** the sources that found it:

| source label | how | from |
|---|---|---|
| `same-parent` | Same owner: the qualifiedName minus its name, `context::owner_of` rules (`core/src/cli/embed_eval/context.rs:46`) | snapshot `nodes` |
| `same-file` | Same `filePath` (a boolean `sameFile` on every candidate, whatever its source) | snapshot `nodes` |
| `trait-impl` | The same method name under another `<X as Trait>` segment (`context.rs:53`), or another implementer of a shared supertype (`edges.kind = 'SupertypeOf'`, `wire/src/lib.rs:88`) | snapshot `nodes`, `edges` |
| `name-sibling` | A name sharing at least half its sub-tokens (`check_queries.sub_tokens`, `check_queries.py:48`) | snapshot `nodes` |
| `bm25` | Top 20 for the query, BM25 over the structured text | `debug-embed-eval run`, a `bm25` arm |
| `embed` | Top 20 for the query, the shipped jina int8 on the structured text | `debug-embed-eval run`, variant `jina-v2-base-code-int8-structured` |

**The judge.** Every candidate is judged, graph-sourced ones included,
because a sibling impl can answer equally. The judge (Sonnet 5.5) sees each
query, the positive and the candidate's code head, and returns one of three
verdicts:

- `answers`: the candidate moves to that query's `positives`;
- `partial`: dropped as ambiguous;
- `non-answer`: kept as a hard negative.

The verdict, the judge model and the prompt sha256 are recorded per
(query, candidate). Unjudged rows never reach the export.

S7 estimates the false-negative rate on 100 or more rows. The card reports
the hard-negative share per source and the `sameFile` share separately, so
GM-436 can ablate them.

**Mining with no new Rust.** `eval/training/` is laid out as an eval dir
(`corpora.toml`, `variants.toml`, `queries/<repo>.jsonl` in the D3 record
format, with `expected` = the positive). Then the existing commands do the
mining:

- `debug-embed-eval snapshot` and `run` (`core/src/cli/embed_eval.rs:394`,
  `:854`) write `rankings.jsonl` with the top `KEPT_HITS = 100` per query
  (`embed_eval/metrics.rs:19`).
- `query_vectors.bin` is written too.
- `run` fails hard on an `expected` entry that does not resolve, which
  checks every positive against the snapshot for free.

The embedding pass runs under `taskpolicy -b` with 1-2 threads (owner
handoff). It is resumable, because `run` reuses `vectors.bin` when the
manifest matches. It is cached under the data dir.

## 4. Leak guard (`eval/training/leak_guard.py`)

**Inputs.**

- Eval query texts (positive and absent) from all three studies:
  `eval/embedding/queries/*.jsonl` and `queries/mechanical/*`,
  `eval/embedding/confirm/queries/*`, and `eval/embedding/v2/new/queries/*`.
  The guard reads `v2/queries/*` as well and checks that it equals the
  concatenation of the three. Each file's sha256 is pinned in the script
  (read at S2 time from the files and the freeze records) and checked before
  use.
- Eval symbol keys per study and corpus: the consumed targets plus every
  expected symbol of the authored and the mechanical queries. That is
  `sample_targets.exclusion(study_dir, corpus)` (`sample_targets.py:94`,
  authored queries only) applied to `eval/embedding`,
  `eval/embedding/confirm` and `eval/embedding/v2/new`, plus the `expected`
  keys of each study's `queries/mechanical/<c>.jsonl` (GM-456/S9 M1). Their
  code comes from the eval snapshots (`--eval-work`, default the main
  checkout's `eval/embedding/work`); each snapshot sqlite's sha256 is pinned
  in the script and must equal its `snapshot.json` record.
- The dataset (`queries`, `positives`, `negatives`) and
  `prompts/generate.md`.

**Checks.** Each check fails the run and names the row:

1. **Repo identity.** No training repo's normalised remote URL or root
   commit (`git rev-list --max-parents=0`) matches an eval corpus checkout.
   This catches forks.
2. **Exact code.** No positive or negative has a whitespace-normalised span
   sha256 equal to an eval key's. This catches vendored copies.
3. **Code near-duplicate.** Shingle Jaccard, or containment of the eval
   span's shingles in the training span (an eval function copied inside a
   larger node, GM-456/S9 L2), (5-grams of `bm25::terms`-style
   tokens, spans of at least 20 tokens) >= 0.7 against any eval key.
4. **Query word Jaccard.** Jaccard >= 0.4 against any eval query text, with
   the tokens of `v2/neardup.py:7` (`[A-Za-z0-9]+`, lowercased). The same
   check runs on the prompt's few-shot texts; a prompt with no few-shot
   block is an error.
5. **Query embedding near-duplicate.** Cosine >= 0.90 with jina int8 (Python
   onnxruntime, mean pooling, the local `work/models/jina-v2-base-code-int8`)
   between any training query and any eval query. Vectors are cached by the
   text's sha256 in chunks, under `taskpolicy -b`.

The 0.90 threshold is provisional. S2 prints the nearest-eval cosine
distribution on the pilot. If more than 2% of rows would drop, the owner
picks the threshold at S6.

`--filter` writes the dataset minus the hits and a `leaks.jsonl`. `--check`
exits non-zero on any hit. The exporter runs `--check` last, on a staging
file: it first deletes the previous export and `guard.json`, and writes
`queries.jsonl`, `sample.md`, `card.md` and `manifest.json` only on a clean
guard whose `guard.json` names the staging file's sha256.

**D3 decision: code near-duplicates by shingles, not embeddings.**

| option | benefit | risk |
|---|---|---|
| **a. Exact hash plus shingle Jaccard for code; word Jaccard plus embeddings for queries (recommended)** | Code leaks are copies, and shingles catch copies without dropping all same-topic code. Cheap. No Rust vector-order coupling | Deviates from the task's "embedding near-duplicates of code"; a heavily rewritten port is missed |
| b. Embedding cosine for code too | As the task says | Cosine on code mostly measures topic: it would flag every JSON parser against g-mesh's serde use. It also needs the eval runs' `vectors.bin` order to be reproduced in Python |

**Control (S4/S5).** A copy of the pilot dataset gets 5 plants. The guard
must exit non-zero and name all five:

1. A GM-398 query text verbatim as a training query.
2. A GM-422 confirm query with two words changed (Jaccard still >= 0.4).
3. A GM-460 `v2/new` query paraphrased below 0.4 Jaccard. This tests check 5
   alone.
4. A training positive whose span is replaced by an eval target's span
   (check 2).
5. A row whose repo has its URL set to `https://github.com/gin-gonic/gin.git`
   (check 1).

The reverse control: on the unplanted copy the guard exits 0. Reverting
each check's code makes its plant pass.

## 5. Schema, rendering, split, storage

**Data dir.** `$GM456_DATA`, default
`~/Projects/ClaudeProjects/g-mesh-data/gm456/` (no repo). Each version is in
its own folder, `v<major>.<minor>/`:

- major changes when targets or generation change;
- minor changes when mining, judging, rendering or the guard change.

| file | content |
|---|---|
| `nodes/<repo>.jsonl` | One row per embeddable, non-test node, raw fields only: `key`, `repo`, `commit`, `language`, `kind`, `nativeKind`, `name`, `qualifiedName`, `parent` (owner), `filePath`, `startLine`, `endLine`, `signature`, `docComment`, `srcLines` (span, rebased to 0), `spanSha256` |
| `texts/<renderId>/<repo>.jsonl` | `{key, text}`, one folder per rendered form |
| `queries.jsonl` | One row per query (layout below) |
| `responses/`, `judge/` | Raw API requests and responses (generation and judging) |
| `manifest.json` | Pins, seeds, models, efforts, prompt sha256s, API version, `renderIds`, guard result, per-language counts, sha256 of every file |
| `card.md` | Source, size, language mix, known biases, the S7 rates |

`key` is `<repo>:<filePath>#<qualifiedName>@<startLine>`. It does not use
`nodes.id`, which an indexer version can change.

A `queries.jsonl` row:

```json
{"id": "mr-0001-p1", "split": "dev", "repo": "mini-redis", "commit": "3d93b42b...", "language": "rust",
 "shape": "phrase", "paraphraseOf": null, "text": "...", "overlap": false, "docEcho": 0.08,
 "target": "<key>", "positives": ["<key>"],
 "negatives": [{"key": "<key>", "sources": ["same-parent", "embed"], "sameFile": true,
                "mining": {"bm25Rank": null, "embedRank": 7, "embedCos": 0.71},
                "judge": {"verdict": "non-answer", "model": "claude-sonnet-5-5"}}],
 "gen": {"model": "claude-opus-5-5", "effort": "low", "promptSha256": "...", "customId": "..."}}
```

**Render forms.** Each `renderId` is a name plus the sha256 of the renderer
file, recorded in the manifest. A consumer refuses a texts folder whose id
does not match. A form change is a re-render in seconds, never a
regeneration.

- `embed-structured`: `gm423_token_lengths.text_to_embed(doc, sig)`
  (`eval/embedding/gm423_token_lengths.py:181`). Parity with Rust is pinned
  by `core/src/embedding/testdata/structured_text.json`.
- `ce-epb48`: `gm698_ce_text_fixture.build(row, src, tok)`
  (`eval/embedding/gm698_ce_text_fixture.py:54`), the E P B48 builder over
  `gm462_ce_text.build`. Parity with Rust `ce_text` is pinned by
  `core/src/embedding/testdata/ce_text.json`. `tok` is the MiniLM tokenizer
  (`work/models/ms-marco-MiniLM-L6-v2/tokenizer.json`). The rebased
  `srcLines` reproduce the whole-file text, as the fixture already asserts.
- Optional `embed-structured-ctx`: the GM-455 header, built from `filePath`
  and `parent`. It is not rendered by default, because GM-455 failed.

**Split.** By repo: the table's dev column. There is no row-level split.

**Committed to git:** scripts, `corpora.toml`, the prompts, `card.md`, a
copy of `manifest.json`, and a sample of 100 rows rendered human-readable
(`eval/training/sample/<version>.md`).

**D5 decision: where the data lives.**

| option | benefit | risk |
|---|---|---|
| **a. Outside git, in a sibling `g-mesh-data/` dir; manifest and card in git (recommended)** | No repo bloat (nodes plus responses are on the order of 100-300 MB). The manifest sha256s make it verifiable | The laptop is the only copy. Back it up as a tarball, at least as a GitHub release asset |
| b. `eval/training/work/`, gitignored | One path convention with eval | A worktree cleanup (`wt-cleanup.sh`) deletes it |
| c. Git LFS | Versioned with the code | LFS setup and quota for a research artefact |

## 6. Teacher scores

| option | benefit | risk |
|---|---|---|
| **a. Defer to GM-436 (recommended)** | Teacher scores depend on the form (`renderId`) and the teacher, and both are GM-436's choices. The candidate lists with stable keys make adding them a column job. Laptop CPU time is spent once, for the arm GM-436 actually picks | GM-436 must also build the chunked, cached runner |
| b. Score here: jina fp32 and int8 plus the CE over every candidate list | GM-436 starts with the scores in hand | About 24k queries x 13 candidates x 2-3 models on CPU is hours of low-priority runtime, for forms that may change |

Recorded here anyway, for free: `mining.embedCos` and the BM25 rank. These
are labelled mining data, not teacher scores.

## 7. Edit map for S2 (code; no LLM generation)

New directory `eval/training/` (an eval-dir layout):

| path | what | reuses |
|---|---|---|
| `corpora.toml` | The 23 pins of section 1, plus `licence`, `split`, `url` keys | `CorporaFile` (`embed_eval/config.rs:11`) has no `deny_unknown_fields`, so S2 confirms the extra keys parse |
| `variants.toml` | `[settings]` plus two rows: a copy of `jina-v2-base-code-int8-structured` (`eval/embedding/variants.toml:145`) and `name="bm25", arm="bm25", role="informational"` | the existing variant parser |
| `setup_work.sh` | Clone each repo at its pin, check `HEAD == revision`, record the licence file sha256, index with the model off, snapshot | `eval/embedding/make_snapshot.sh`. Parametrise its hard-coded `eval_dir="eval/embedding"` with `EVAL_DIR` (a one-line edit) |
| `nodes.py` | Snapshot plus checkout to `nodes/<repo>.jsonl` (raw fields, `srcLines`), dropping test and fixture paths | `sample_targets.embeddable` (:59), `TEST_PATH`/`TEST_QNAME` (:43, :48), `line_at_pin` (:63) |
| `sample.py` | Seed-456 stratified target order with per-repo quotas | `sample_targets.stratum` (:51), `STRATA_PATTERN` (:41) |
| `render.py` | `texts/<renderId>/` for both forms | `gm423_token_lengths.text_to_embed` (:181), `gm698_ce_text_fixture.build` (:54), `minilm_counter` (:58) |
| `generate.py` | Build the batch request JSONL (`--dry-run` in S2), submit, collect responses, parse to queries. `--stub` writes clearly-labelled stub queries (the doc's first sentence, else the name words) for pipeline tests only | `check_queries.overlap` (:68) |
| `prompts/generate.md`, `prompts/judge.md` | Prompts | none |
| `mine.py` | Write `queries/<repo>.jsonl` in D3 format, call `debug-embed-eval run` for both variants, read `rankings.jsonl`, add the graph sources, write candidate lists | `context.rs` owner and trait-segment rules (port the two functions, 20 lines) |
| `judge.py` | Batch judge requests (`--dry-run` in S2) | none |
| `leak_guard.py` | Section 4 | `sample_targets.exclusion` (:94), the `neardup.py` tokens (:7) |
| `export.py` | Split by repo, `manifest.json`, sample md; runs `leak_guard --check` last | none |
| `run_pipeline.sh` | One background script, fail-fast, under `taskpolicy -b`, writing `summary.txt` | none |
| `README.md` | Commands and layout | none |

**S2 end-to-end** runs on `mini-redis` (dev, about 240 KB). The steps are
setup, nodes, sample (50 targets), render, `generate --stub`, mine (bm25 and
int8, 2 threads), `judge --dry-run` (stub verdict `non-answer`, marked as
such), `leak_guard --check` (clean), and export to `v0.0-stub`. S2 also
prints:

- the frame size;
- the nearest-eval-query cosine and Jaccard distributions;
- the candidate counts per source.

S2 makes no API calls.

**Behaviours for S4 tests:**

- The leak guard fires on each of the 5 plants and passes clean.
- Render parity: `render.py` output equals the `structured_text.json` and
  `ce_text.json` fixture texts.
- The split never puts one repo on both sides.
- Unjudged negatives never reach the export.
- A `renderId` mismatch is refused.
- The guard refuses an eval file whose sha256 differs from its pin.

## Must confirm (later slices)

- **S2:**
  - The extra `corpora.toml` keys parse.
  - `run` accepts a corpus whose eval dir has no `queries/mechanical`. The
    code says it is optional (`queries.rs:78`).
  - `onnxruntime` 1.19.2 and `tokenizers` 0.22.2 are present (both checked
    today). The jina int8 model is in `work/models`.
  - Python mean pooling is sane: on 20 eval queries, cosine >= 0.994 to
    the Rust `query_vectors.bin` (`POOLING_BAR`; bar lowered from 0.999 at
    GM-456/S9 L1, accepted). Measured over 2279 queries of the six GM-465
    int8-structured runs: vectors exact (cos 1.000000) up to 15 tokens,
    drift from 16 tokens (min 0.99478), likely the int8 GEMM kernels of the
    two ONNX Runtime builds (Rust ort-sys 2.0.0-rc.9 vs Python 1.19.2); the
    guard compares Python with Python, and 0 verdicts flip at 0.90 over
    2.6M pairs.
- **S6 step 0:**
  - The frame of every repo, and the quotas; replace a repo that is too
    small.
  - The licence file is present at each pin.
  - The pilot's real tokens and dollars, and the full-run estimate shown to
    the owner before the full run.
  - The cosine threshold.
- **S6:** the `overlap = false` share per language is >= 50%. Report
  `docEcho` against the eval queries.
- **S7:** a fresh agent checks 100 or more rows. It reports query
  correctness and the false-negative rate, with Wilson CIs, by source and by
  `sameFile`.

## Risks

- **Style gap.** LLM queries are cleaner than agent queries, and GM-436 may
  learn the style. `docEcho` and the overlap share make the gap visible. The
  dev split measures it.
- **Topic proximity without leak.** axum and hono are web code, and gin is
  in the eval. That is legitimate domain transfer, not a copy. The card lists
  the domain overlap per eval corpus, so a gin-only gain can be questioned.
- **Judge errors.** A false `non-answer` teaches the wrong ranking. S7
  measures the rate, and the `partial` drop absorbs doubt.
- **Batch latency or refusals.** Batches can take up to 24 h. A refusal
  (`stop_reason: "refusal"`) is logged and its target skipped, never retried
  blind.

## Code facts relied on (and how they were found)

| fact | source |
|---|---|
| `text_to_embed` at `core/src/embedding/text.rs:99` (0-based outline 98); `node_text` applies noise and generated filters | g-mesh `get_file_outline` on text.rs, then `sed` of those lines |
| `EdgeKind` has `SupertypeOf` (`wire/src/lib.rs:88`) | g-mesh `find_definition EdgeKind` |
| `ce_text`, `BODY_BUDGET=48`, `source_lines` in `core/src/embedding/ce_text.rs` | grep of one known file |
| `debug-embed-eval` snapshot/run, rankings top 100, bm25 arm, `text_for` | grep and sed of `core/src/cli/embed_eval.rs` (one known file) |
| v2 layout, sha256 pins, same six pins | `eval/embedding/v2/README.md`, `v2/corpora.toml`, `docs/results/gm-460-enlarged-eval.md` |

I asked g-mesh no caller or reference questions: the note changes no Rust
symbol. The only planned Rust-adjacent edit is the `EVAL_DIR` parameter in
a shell script.
