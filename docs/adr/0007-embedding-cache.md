# 0007. Embedding cache: machine-wide, keyed by the embedded text and the model's bytes

## Status
Accepted (2026-09-26, GM-424); the owner reviewed the proposal and accepted
it with the answers recorded under "Owner's answers" at the end.

## Context
Every reindex path recomputes every vector, although a node's vector is a
pure function of two things: the text `text_to_embed` builds
(`core/src/embedding/pipeline.rs:339-349`: trimmed doc comment, `"\n\n"`,
trimmed signature; either alone; neither means no vector) and the model that
embeds it (tokenizer truncation to `DEFAULT_MAX_SEQUENCE_LENGTH = 1024`
tokens, `model.rs:134`/`:213`; mean pool + L2 norm, 768 dims; session built
with `with_deterministic_compute(true)`, `model.rs:222`, and tests already
assert identical vectors across separately loaded models).

### Baseline (GM-424/S1, 3.13.0 at a40d040, 8-CPU macOS, load 5-19)
- **Cold full reindex** (`g-mesh reindex`): real 668s, user 2397s. The
  embedding backfill is **604s (90%)** for **6,536 nodes** (rust 5,676, ts
  435, go 356, python 69), ~92ms wall per node at ~4 cores. Structural walk
  ~4s, semantic passes ~63s.
- **Workspace reindex** under a live daemon, triggered by changing only the
  version string in `core/Cargo.toml`: ~578s, of which **~510s (88%)** is
  re-embedding all 5,676 rust nodes; semantic pass ~58s.
- **No lock stall.** `IndexStore` does not hold the store lock across
  inference (`index_store.rs:87` `BulkWalk => Hold::PerStep`, `:205`;
  `bulk_index.rs:398` computes before `commit_batch`). Query latency during
  the reindex: `search_code` median 67ms (baseline ~50ms, CPU contention
  only), `find_definition` 4-5ms. A control with a forced 30s hold showed
  up as a 29.5s outlier, so the probe could see a stall.
- **A correctness gap instead.** Rust rows are deleted up front
  (`workspace_reindex.rs:276`) and return batch by batch as each batch is
  embedded: from t=3s to t=140s `find_definition EmbeddingPipeline` answered
  not-found and `search_code` ranked a TypeScript node first; rust vectors
  stayed partial until t~516s.
- **Model identity today** is the config string only
  (`config.embedding.model`, default `jina-embeddings-v2-base-code`,
  `config/mod.rs:136`), written as `vectors.embeddingVersion` and
  `meta.embedding_model`. The pinned revision (`cli/model.rs:67`), the
  weights' sha256 (`cli/model.rs:170,176`), the truncation length and the
  dim are not part of it, and `G_MESH_MODEL_DIR` can put any `model.onnx`
  under the same name.

## Decision

### 1. The key: `sha256(text)` under a model fingerprint
- **Text bytes:** the UTF-8 bytes of `text_to_embed`'s output, exactly as
  passed to `EmbeddingModel::embed` - after trimming, **before**
  truncation. Truncation is a deterministic function of the text, the
  tokenizer and the max length, all of which are in the fingerprint, so
  hashing the untruncated text is exact. Keying on the truncated token ids
  would share a vector between texts that differ only past token 1024 (rare)
  at the price of tokenizing every hit. Rejected.
- **Hash:** SHA-256 (`sha2` is already a dependency, `core/Cargo.toml:112`).
  A 64-bit hash (xxhash) would need collision handling; BLAKE3 is a new
  dependency for a cost (~1.8M chars per full index) that is negligible
  next to 92ms/node of inference.
- **Model fingerprint:** `sha256` over a canonical record of
  `sha256(model.onnx)`, `sha256(tokenizer.json)`,
  `max_sequence_length`, `EMBEDDING_DIM`, a `PIPELINE_EPOCH` constant
  (pooling/normalization/`text_to_embed` format; bumped by hand when that
  code changes) and the `ort` crate version (`=2.0.0-rc.9`,
  `Cargo.toml:99`; an ONNX Runtime upgrade may change floating-point
  results). Not the g-mesh version: an upgrade's generation-mismatch wipe is
  exactly the reindex the cache should serve. Not the config name: two names
  for the same bytes share vectors, one name for different bytes does not.
- **Cost of the fingerprint:** hashing the 612 MiB `model.onnx` takes
  seconds, so it is memoized in the cache database by
  `(canonical path, size, mtime_ns)`. The first process on a machine pays
  it once, lazily, on the first `compute`; later processes read one row.
  Rejected: trusting the pinned digest when sizes match (a
  `G_MESH_MODEL_DIR` export of the same size would alias), and keying on
  the config name only (today's weakness).

### 2. Location and storage
- `$G_MESH_HOME/embedding-cache/cache.sqlite` (default `~/.g-mesh/...`).
  It follows `G_MESH_HOME`, unlike the model weights (`model.rs:359-365`),
  so an isolated test or benchmark run starts cold and never touches the
  developer's cache.
- SQLite through the existing `rusqlite` (bundled), WAL mode as the project
  index already uses (`storage/connection.rs:109`). No `sqlite-vec`: the
  cache only does point lookups.
- Schema (versioned with `PRAGMA user_version`; an unknown version is
  treated as a corrupt cache, below):
  - `models(id INTEGER PRIMARY KEY, fingerprint BLOB UNIQUE, last_used INTEGER)`
  - `model_files(path TEXT, size INTEGER, mtime_ns INTEGER, sha256 BLOB, PRIMARY KEY(path, size, mtime_ns))`
  - `entries(model_id INTEGER, text_hash BLOB, vector BLOB, last_used INTEGER, PRIMARY KEY(model_id, text_hash)) WITHOUT ROWID`
- **Vector encoding:** the same little-endian f32 blob as
  `vectors::pack` (`storage/vectors.rs:65`), 3,072 bytes; decode with
  `f32::from_le_bytes`. A bit-for-bit round trip, never text or f16. A blob
  of any other length is a miss.
- Size: ~3.1 KB per entry; one full index of this repo is ~20 MB.

### 3. Eviction
- `last_used` is a day number (UTC). A hit updates it only when it differs
  from today, so a warm reindex does not rewrite every row.
- Bound: `embeddingCache.maxSizeMb`, default 512 (~160k entries, ~25 indexes
  the size of this repo). Checked at the end of each reindex unit that
  inserted rows (bulk walk, workspace reindex, backfill), never per file
  change: if `page_count * page_size` exceeds the bound, delete oldest
  `last_used` first down to 80%, then `PRAGMA incremental_vacuum`
  (`auto_vacuum = INCREMENTAL` set at creation).
- Model change: a new fingerprint gets a new `models` row, so old entries are
  unreachable at once. They leave by LRU; additionally the GC drops a whole
  model whose `last_used` is older than 30 days.
- GC runs `BEGIN IMMEDIATE` with no wait; if another daemon holds the
  writer, it skips this round.

### 4. Concurrency and failure
- One cache connection per daemon/CLI process, behind its own `Mutex`
  inside `EmbeddingPipeline` - never the project store lock - held only for
  one batch lookup or one batch insert, never across inference.
- Several daemons: WAL gives concurrent readers; writers are serialized by
  SQLite with `busy_timeout` 250ms. Inserts are `INSERT OR IGNORE`: the same
  key always carries the same bytes (determinism), so a race is harmless.
  One transaction per batch of misses.
- A busy timeout on insert drops that batch's inserts (the vectors are
  still stored in the project index); a busy lookup is an all-miss batch.
- Crash safety is SQLite's: a batch is committed or absent.
- **The cache never fails indexing.** Open failure, `SQLITE_CORRUPT`/
  `NOTADB`, unknown schema version: log once, rename the file to
  `cache.sqlite.corrupt-<unix>` and recreate; if that fails too, disable
  the cache for the process's life and compute everything, as today.

### 5. Integration: one seam, inside `EmbeddingPipeline::compute`
Every reindex path already funnels through `compute`
(`pipeline.rs:211`), and every caller already runs it with no store lock
held (g-mesh `find_references`, complete):

| path | call site |
|---|---|
| bulk walk batch, incl. workspace reindex (`workspace_reindex.rs:282-283` -> `walk_one_language`) | `daemon/bulk_index.rs:398`, then `commit_batch` `:399` |
| backfill: cold start (`daemon/activation.rs:180`), `g-mesh reindex` (`cli/reindex.rs:133`), generation-mismatch wipe (`schema::reset`, `schema.rs:553`, then the structural walk with `embedding: None`, `activation.rs:215`, then backfill) | `embedding/backfill.rs:150`, then `store_vectors` `:152` |
| single file change | `watcher/apply.rs:345`, then `store_vectors` `:349` |

`compute` becomes: build the texts; hash them; one `SELECT` for the batch's
keys; embed only the misses; insert the misses in one transaction; return the
same `Vec<ComputedEmbedding>` in the same order. `store` and its GM-396
staleness check (`pipeline.rs:276`) are unchanged: a cached vector is
checked against the current row like a fresh one. The ONNX session is loaded
on the first miss only, so an all-hit reindex never loads the model; the
fingerprint needs only the two files' bytes (memoized). `embed_query`
(`search_code`) is not cached.

**Delete-first window.** With a warm cache the rust re-walk is the
structural walk plus ~6k point lookups, so the not-found window
(137s measured) and the partial-vector window (~516s) should both shrink to
the structural walk's length: seconds, well under ~30s here (the whole
repo's structural-only walk was ~31s on GM-393). S5 measures it. Fixing
delete-first itself (keep old rows until the language's re-walk commits, or
replace per file) is a **separate follow-up task**: it changes workspace
reindex semantics and linking, not embedding, and would still matter on a
cold cache or with the cache disabled.

### 6. Config, kill switch, observability
- Global config (`~/.g-mesh/config.toml`, the cache being machine-wide):
  `[embeddingCache] enabled = true, maxSizeMb = 512`. Env
  `G_MESH_EMBEDDING_CACHE=off` overrides it: S5's cache-disabled control
  arm, and a field escape hatch.
- `compute` counts `hits`, `misses` (= symbols embedded), `cache_errors`.
  Each unit logs one line to stderr at its end, e.g.
  `g-mesh daemon: embeddings [workspace-reindex rust]: 5676 texts, 5670 cache hits, 6 embedded, 0 cache errors, 4.1s`.
  The watcher logs only when something was embedded. `BackfillSummary`
  gains `cache_hits`, so `g-mesh reindex` prints both numbers.

### 7. Tests, each with its control
The counts come from a test seam: `compute` takes the embedder as a
parameter internally, so unit tests use a deterministic fake that counts
calls; the byte-identity tests use the real model (`load_real_pipeline`,
`core/tests/embedding_generation_pipeline.rs:163`).

| test | asserts | control (revert this, the test fails) |
|---|---|---|
| unchanged symbol, workspace reindex | fixture indexed, workspace reindex fired: embed calls 0, hits = N | disable the lookup in `compute` -> N calls |
| changed doc comment | edit one doc comment, reindex: exactly 1 call, that node's vector changes and equals a fresh embed | key on node id instead of text -> 0 calls, stale vector |
| model change | same texts, second fingerprint: N calls | drop the fingerprint from the key -> 0 calls |
| byte identity | real model: cached vs fresh `to_bits()` equal for every node; `search_code` top-10 ids and distances equal, cache on vs `off` | flip one mantissa bit in the decoder -> inequality |
| concurrent writers | 4 processes on one cache, overlapping keys: no error, every key present once, identical bytes | drop `busy_timeout` / `OR IGNORE` -> busy or constraint errors surface |
| cache failure degrades | cache file is garbage, or a writer holds `BEGIN EXCLUSIVE`: indexing succeeds, all vectors stored | propagate the open/busy error -> indexing fails |

## Consequences
- A warm reindex embeds only changed texts: the measured 510-604s of
  inference becomes proportional to what changed. A cold cache (first run on
  a machine, new model, new `PIPELINE_EPOCH`) costs what it costs today plus
  one fingerprint hash.
- Vectors in a project index can now come from another project's run. That
  is sound only while embedding is deterministic for a fingerprint; the
  byte-identity test is what keeps that assumption checked.
- A new shared file in `G_MESH_HOME` that several daemons write; the GC and
  corruption handling are new code that must never fail indexing.
- `PIPELINE_EPOCH` is a manual invariant: changing `text_to_embed` or
  pooling without bumping it serves stale vectors. A test pins the epoch to
  a hash of `text_to_embed`'s output for fixed inputs, so a format change
  fails it.
- Follow-up: delete-first in `workspace_reindex.rs:276` (above).

## Owner's answers
1. Default size bound: 512 MiB.
2. The `ort` version is part of the fingerprint: an ONNX Runtime bump refills
   the cache instead of trusting determinism across runtimes.
3. `model.onnx` is hashed, memoized by `(path, size, mtime_ns)`; the pinned
   sha256 is not trusted in its place.
4. `g-mesh reindex` uses the cache; `G_MESH_EMBEDDING_CACHE=off` is the way
   to recompute everything. The byte-identity test guards against a cache
   that would otherwise make a bad vector survive a reindex.
5. Delete-first in workspace reindex is its own task in the same batch.
6. `g-mesh status` does not report the cache; the per-unit log lines are
   enough for now.
