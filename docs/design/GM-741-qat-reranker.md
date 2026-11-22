# GM-741: ship the QAT-trained L6 reranker (design)

Design slice GM-741/S1, 2026-11-22, branch `feat/GM-741-qat-reranker` off
`release-4.36.0`. Model: GM-734 QAT RR cross-encoder,
`g-mesh-data/gpu-runs/gm730-rr-qat-27c57b8dd91d-20261121T0020-a1/out/qat/`.
Owner decision on hosting (handoff note, 2026-11-21, verbatim): "Свой
репозиторий на HF (Recommended)".

## 0. Summary

- **What ships.** The GM-734 quint8 export replaces today's
  `cross-encoder/ms-marco-MiniLM-L6-v2` quint8 as the default reranker. Same
  architecture, same tokenizer (byte-identical), same graph inputs and output,
  27 KB smaller on disk. It is fetched from our own Hugging Face repo, pinned by
  commit and sha256, through today's download path (Hugging Face first, the
  `G_MESH_MODEL_BASE_URL` mirror before it when set).
- **Rollback.** A reranker *variant* setting, `[rerank] model` in the global
  config and `G_MESH_RERANK_MODEL` in the environment: `qat` (default) or `l6`
  (today's). Each variant carries its own repo, revision, files, directory and
  blend weight. Today's model directory is left untouched, so `l6` works
  without a download for every user who has it now.
- **Calibration.** The only production consumer of the cross-encoder's score
  is the blend `logit + BETA * cosine` (`BETA` = 40, fitted on today's L6).
  The no-match floors, the F4 verdict and the exact-name skip read only the
  embedding cosine. The new model's logits sit on a much narrower scale (one
  smoke probe, 3.1). So `BETA` is re-fitted for the new model on eval v2 FIT;
  floors stay.
- **Go rule** (section 6): eval v2 held-out, today's embedder + new reranker at
  its FIT-tuned beta vs today's stack, paired bootstrap. Non-inferiority on CW
  positives (NL and name), name misled per language, and CW absent pinned at
  exactly zero change as a control.
- **Owner actions** (section 2): a Hugging Face org, three licence checks, then
  the upload. The Claude-terms check is not settled by this note (2.3).

## 1. Hosting and pinning

### 1.1 Today

`core/src/embedding/rerank.rs:36-50` pins today's reranker:

| constant | value |
|---|---|
| `RERANK_MODEL_NAME` (directory, log name) | `ms-marco-MiniLM-L6-v2` |
| `RERANK_MODEL_REPO` | `cross-encoder/ms-marco-MiniLM-L6-v2` |
| `RERANK_MODEL_REVISION` | `233902d25c440f23af6f7d6e94d2946bac0bee0a` |
| `RERANK_ONNX_REMOTE_PATH` | `onnx/model_quint8_avx2.onnx` |
| `RERANK_ONNX_SIZE` / `_SHA256` | 23,200,716 / `c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9` |
| `RERANK_TOKENIZER_SIZE` / `_SHA256` | 711,396 / `d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66` |

`core/src/cli/model.rs:211-230` (`RERANK_FILES`) turns them into two
`RemoteFile`s written as `model.onnx` and `tokenizer.json`. Sources
(`model.rs:97-150`): `G_MESH_MODEL_BASE_URL` first when set, as a flat mirror
serving `<base>/ms-marco-MiniLM-L6-v2/model.onnx`; then Hugging Face,
`https://huggingface.co/<repo>/resolve/<revision>/<remote_path>`, today
`https://huggingface.co/cross-encoder/ms-marco-MiniLM-L6-v2/resolve/233902d25c440f23af6f7d6e94d2946bac0bee0a/onnx/model_quint8_avx2.onnx`
(asserted by the test at `model.rs:1149-1170`). Each download is streamed,
hashed and renamed only when its sha256 matches. Directory:
`$G_MESH_RERANK_MODEL_DIR`, else `~/.g-mesh/models/ms-marco-MiniLM-L6-v2`
(`rerank_model_dir_from`, `rerank.rs:96-102`). The daemon checks both files
by sha256 before loading (`check_rerank_files`, `rerank.rs:115-136`);
`g-mesh status` checks presence and size only (`RerankModelState::of`,
`rerank.rs:346-359`).

### 1.2 The new model (hashes computed in this slice with `shasum -a 256`)

| file | size | sha256 | note |
|---|---|---|---|
| `onnx-quint8/model.onnx` | 23,172,943 | `9eb66866978b8b7bf02e7f46cd6f005019151a1ebf03b65e0de4883ab44ea633` | the shipped file |
| `onnx-quint8/tokenizer.json` | 711,396 | `d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66` | identical to today's |
| `onnx-quint8/config.json` | 794 | `380e02c93f431831be65d99a4e7e5f67c133985bf2e77d9d4eba46847190bacc` | identical to today's; the loader does not read it |
| `onnx-fp32/model.onnx` | 90,991,023 | `ae3fa29c428ef7eb47975053d4943273c0031a90f001382f73f60f8a065c6586` | provenance only; not downloaded by g-mesh |

The values match `export.json` of the run. ORT smoke (Python onnxruntime
1.19.2, one thread, one pair per run): the quint8 graph loads, inputs
`input_ids`, `attention_mask`, `token_type_ids`, output `logits`, the same
names `CrossEncoder::score` uses. On three probe pairs ("parse the config
file" against a matching, a related and an unrelated signature) today's L6
gives -2.52 / -5.63 / -11.31 and the new model -4.99 / -4.78 / -6.12: a spread
of 1.3 logits against 8.8. That is three pairs, not a measurement, but it is
why section 5 re-fits the blend weight.

### 1.3 The new repo

Proposed layout of `<hf-org>/g-mesh-reranker-l6-qat` (org from owner action
A1):

| path in the repo | source | read by g-mesh |
|---|---|---|
| `onnx/model_quint8.onnx` | `onnx-quint8/model.onnx` | yes, as `model.onnx` |
| `tokenizer.json` | `onnx-quint8/tokenizer.json` | yes |
| `config.json` | `onnx-quint8/config.json` | no |
| `onnx/model.onnx` | `onnx-fp32/model.onnx` | no (reproducibility, re-quantisation) |
| `README.md` | model card: base model, teacher, data, training run, eval numbers, the MS MARCO note (2.2) | no |
| `LICENSE` | Apache-2.0 text (base model's licence) | no |

g-mesh pins the **commit hash** the upload produces (not `main`), plus the
size and sha256 of the two files it reads. The commit is unknown until the
upload: it is a must-confirm item for the slice that writes it (section 9).

### D1. Directory name and migration

**Today.** One reranker, in `~/.g-mesh/models/ms-marco-MiniLM-L6-v2`. `g-mesh
model fetch` re-downloads any file whose size differs from the pin
(`on_disk`, `model.rs:466`), then renames over the old file.

**Change.** If the new model kept that directory, the first `model fetch`
would overwrite today's file (the sizes differ: 23,172,943 vs 23,200,716), and
rollback to `l6` would need a new download. Example: a user fetches, sees a
regression, sets `G_MESH_RERANK_MODEL=l6`; with one shared directory the
daemon refuses the file (sha mismatch) and search runs unranked until they
fetch again.

| option | benefit | risk |
|---|---|---|
| **a. New directory `~/.g-mesh/models/g-mesh-reranker-l6-qat` (Recommended)** | Today's files stay where they are, so `l6` works with no download; the two never overwrite each other; the mirror prefix is the directory name, as today | 23 MB more on disk while both exist; a user who never re-fetches has no new model (handled by D3) |
| b. Same directory, overwrite | No extra disk | Rollback needs a re-download; a mid-fetch failure leaves the user with neither model's pinned pair |

Migration under (a): nothing is moved or deleted. `model fetch` fetches the
selected variant (default `qat`) into its directory. The flat mirror serves
`<base>/g-mesh-reranker-l6-qat/model.onnx` and `.../tokenizer.json`; whoever
runs a mirror must add that directory. `G_MESH_RERANK_MODEL_DIR` keeps
overriding the directory of whichever variant is selected.

## 2. Owner actions before publishing (checklist)

- [ ] **A1. Hugging Face account or org** that will own the repo, e.g. an org
  named for g-mesh. A personal account works but ties the download URL to a
  person.
- [ ] **A2. Base-model licence** (2.1). Confirm Apache-2.0 obligations are met
  by the repo layout (licence text, notice of changes in the card).
- [ ] **A3. MS MARCO provenance** of the base model (2.2): decide whether it
  blocks publishing. It applies equally to today's shipped L6.
- [ ] **A4. Claude-generated training queries** (2.3): read the current terms
  that governed the GM-456 generation and decide. This is the open item.
- [ ] **A5. Code corpora licences** (2.4): confirm the recorded licence files.
- [ ] **A6. Upload** the six files of 1.3 (`huggingface-cli upload`), then
  report the commit hash. The implement slice pins it; nothing is published
  by an agent.

### 2.1 Base model: cross-encoder/ms-marco-MiniLM-L6-v2

Fetched from its Hugging Face page in this slice: licence tag
**apache-2.0**; base model `microsoft/MiniLM-L12-H384-uncased` (the page's
model tree); training data `sentence-transformers/msmarco` ("MS Marco Passage
Ranking"); no further restriction on the page. Apache-2.0 allows publishing a
derivative; it needs the licence text and a statement that the files were
changed (section 4 of the licence). The model card and `LICENSE` in 1.3 do
both.

The teacher whose scores the student learned from (GM-730 RR-T) is
`Alibaba-NLP/gte-reranker-modernbert-base`: its page says **apache-2.0** and no
further restriction (fetched in this slice). Only its scores were used; no
teacher weights are published.

### 2.2 MS MARCO terms (inherited by the base model)

`microsoft.github.io/msmarco` (fetched in this slice): "The MS MARCO datasets
are intended for non-commercial research purposes only", and "made available
free of charge without extending any license or other intellectual property
rights". The base model's weights are Apache-2.0 regardless, and g-mesh
already ships them unchanged. Publishing a fine-tune changes nothing about
this, but our repo would be a new place that redistributes it. **Open for the
owner**: whether g-mesh's use is non-commercial in that sense, and whether
the card should state the provenance (recommended: state it).

### 2.3 Training on Claude-generated queries (GM-456 pairs)

**What the data is.** GM-456 D2 (owner): queries generated by Opus 5.5 and
judged by Sonnet 5.5 through headless Claude Code on the subscription
(`docs/design/GM-456-training-data.md` section 2b), not through an API key.
The QAT model was trained on windows built from those queries.

**What I found.**
- Commercial Terms (anthropic.com/legal/commercial-terms, fetched): D.4
  forbids to "access the Services to build a competing product or service,
  including to train competing AI models"; B says the customer "owns its
  Outputs". These terms cover API-key use, which is not how the data was made.
- Consumer Terms (anthropic.com/legal/consumer-terms), which govern a Pro/Max
  subscription and so Claude Code on it: **the fetch failed** (connection
  refused). From memory, and therefore unverified: they forbid, except via an
  API key or with explicit permission, using the services "to develop any
  products or services that compete with our Services, including to develop or
  train any artificial intelligence or machine learning algorithms or models".

**What stays open.** (1) The exact current wording of the Consumer Terms;
(2) whether a 22M-parameter code-search reranker inside a code-indexing tool
"competes" with Anthropic's services, or falls under "train any ... models" in
the consumer wording regardless of competition; (3) whether shipping the
weights inside g-mesh is already covered by the same question as publishing
them on Hugging Face (it is: both distribute the trained model). This note
does not decide it. If the answer is no, the options are to retrain on queries
generated through an API key under the Commercial Terms, or to stop GM-741.

### 2.4 Code corpora

GM-456 section 1 admits only MIT, Apache-2.0 or BSD repos (mdBook, MPL-2.0,
was dropped); the 23 pins are listed there with their licences and S2 recorded
each licence file's sha256 at the pin (`g-mesh-data/gm456/setup.log`). The
published repo holds weights only, no training windows or code, so the
licences' redistribution and notice clauses are not triggered by the upload.
**Owner check:** confirm the recorded licence files (the "dual MIT" readings
are the S2 author's, GitHub reports one SPDX id), and keep the training data
out of the repo.

## 3. Rollback

### D2. Mechanism

**Today.** One pinned model; `[rerank] enabled = false` or
`G_MESH_RERANK=off` turns ranking off, nothing selects another model.

**Change.** A variant setting resolved exactly like the switch
(`config/embedding.rs:106-135`: environment first, then the global config,
then the default):

```toml
[rerank]
model = "l6"      # or "qat" (default)
```

`G_MESH_RERANK_MODEL=l6` overrides the file. An unknown value logs one line
naming the accepted values and uses the default. Each variant is one entry of
a static table: name, directory name, repo, revision, ONNX remote path, the
two files' sizes and sha256, and the blend weight. Example: after the release,
a user sees worse results on their repo, sets `G_MESH_RERANK_MODEL=l6`,
restarts the daemon; search_code is today's ranking again, from the files
already in `~/.g-mesh/models/ms-marco-MiniLM-L6-v2`, at beta 40.

| option | benefit | risk |
|---|---|---|
| **a. Variant setting, config key + env, default `qat` (Recommended)** | Rollback without a rebuild or download; the same binary serves both arms of the latency A/B (section 7); per-variant beta keeps `l6` exactly today's | Two pinned models to maintain; one more setting to test |
| b. Env only | Less code | Not persistent for a user who starts the daemon from an editor; config is where `[rerank] enabled` already lives |
| c. No switch: roll back by releasing the old build | No code | Every user must downgrade g-mesh; no in-field A/B |

Default: `qat`. The `l6` entry is retired by a later task once the new model
has been out for a release with no regression report.

### D3. A user who upgrades and does not re-fetch

**Today.** A missing model gives `Absent`: search_code keeps the embedding
order, logs once, and the first page carries a one-time hint.

**Change.** After the upgrade the default `qat` directory is empty for every
existing user until they run `g-mesh model fetch` (the installer only prints
that advice: `scripts/install.sh:844-846`, `install.ps1:645-647`).

| option | benefit | risk |
|---|---|---|
| **a. Fall back to `l6` when the default `qat` is absent and `l6` is present, log it once, show it in `g-mesh status` (Recommended)** | No user loses ranking on upgrade; they get today's ranking until they fetch | The daemon may run the old model without the user noticing, beyond the status line and the log |
| b. No fallback: `Absent` and the hint | Simplest; the hint already says what to run | Every upgrader runs unranked (GM-443's rerank gain lost) until they fetch |

Fallback applies only to the default. An explicit `model = "qat"` or `"l6"`
that is absent gives `Absent`, as today, so a rollback test cannot be
silently served by the other model. The installer text changes to "run g-mesh
model fetch again: it adds the new rerank model".

## 4. Rust vs Python ORT parity

**Today.** `rerank.rs:1041` (`#[ignore]`) checks the Rust crate against
`testdata/rerank_parity.json` (18 cases x 30 rows, written by
`eval/embedding/gm464_rerank_fixture.py` from Python ORT) at
`LOGIT_TOLERANCE = 0.25`. GM-452 caveat b: on 300 shared pairs Python ORT vs
the Rust cache gave mean |d| 0.0052, max 0.198, for today's L6.

**Method for the new model.**
1. Pairs: `eval/embedding/gm700_rust_pairs.py dump` writes every v2 window
   pair (K = 30, all queries, ship text). The tokenizer is identical, so the
   pair set and texts are today's.
2. Rust: `dump_logits_for_the_offline_eval` (`rerank.rs:1110`) with
   `G_MESH_RERANK_MODEL=qat` and `G_MESH_RERANK_MODEL_DIR` at the local quint8
   directory, fresh process (the shipped kernel path). This is also the score
   cache for the stack eval (section 6).
3. Python: the same files through onnxruntime with `rerank_eval`'s settings
   (one pair per run), on a sample of **120 whole windows** (seed 741; 15 NL +
   15 name per language, held-out and FIT mixed as drawn), about 3,600 pairs.
4. Compare per pair and per window.

**Tolerance (all must hold; S1, superseded for the gate by 4.1):**
- mean |d| <= 0.01 logits, and mean |d| <= 0.005 x the new model's logit
  standard deviation over the sample (scale-free: the new logits are
  narrower, so an absolute bound alone is looser than it was for today's L6);
- max |d| <= 0.20 (GM-452's observed max for today's L6);
- the blend top-1 at the shipped beta is identical in >= 119 of 120 windows,
  and no window's top-3 set differs.

A failure stops the measure slice: the stack numbers would come from a kernel
path that does not match the reference. The 18-case fixture is regenerated
from the new model (`gm464_rerank_fixture.py` with the new model and beta), so
the `#[ignore]` test keeps guarding the shipped variant; `LOGIT_TOLERANCE` is
re-derived as 4 x the observed max |d|, capped at 0.25 (must-confirm: the new
logit scale may make 0.25 too loose to catch a wrong model).

### 4.1 Re-derived parity bound (owner 2026-11-22)

GM-741/S13. The first parity run (S8, `docs/results/gm-741-qat-reranker.md`
section 1, commit `f8e20a38`) failed two of the five bounds above. Owner
decision, verbatim: "Перевывести границу и взять новую выборку
(Recommended)". This subsection replaces the **Tolerance** list above for the
gate; the method steps 1-4 and the fixture paragraph stay. It is committed
before any new parity run. The S8 numbers are quoted below only to explain
what went wrong; no bound below takes a value from them, and the S8 sample is
never re-judged under these bounds.

**What the gate is for.** The stack numbers of section 6 are computed from the
Rust logits (method step 2), so the go rule already measures what the shipped
kernel does, near-tie orderings included. Parity guards something else: that
the Rust kernel path (`ort` 2.0.0-rc.9, `rerank.rs`) runs the new model as
the reference does (Python onnxruntime 1.19.2, `rerank_eval`), so that a
wrong file, a wrong input, a wrong output read or a numeric defect cannot
hide behind a good stack number. Both models are the same architecture and
the same quint8 format on the same two runtimes, so the reference for "how
far apart may the two runtimes be" is today's shipped L6 on the same pairs.

#### 4.1.1 Why the S8 bounds were mis-specified

1. **The sd-relative mean bound assumed kernel noise scales with the logit
   sd.** It does not. Rust-vs-ORT differences come from the integer GEMMs,
   requantization and fused activations inside the encoder (different ORT
   builds and optimizer passes); the logit sd is the spread of the final
   head's output across pairs, most of it between queries. Nothing makes the
   first proportional to the second. The coefficient 0.005 was not derived
   either: it was picked so the bound would be tighter than the absolute one.
   With the new model's narrow scale (sd 0.70) it asked for a mean |d| of
   0.0035, about two thirds of the 0.0052 today's L6 shows on the same two
   runtimes (GM-452 caveat b). The bar was stricter than what the shipped
   model clears, for a reason unrelated to correctness.
2. **The pooled logit sd is also the wrong yardstick for ranking.** Rows
   change places when an error exceeds the gap between neighbouring blend
   scores (`logit + beta * cosine`) within one window; the pooled sd mixes in
   the between-query offsets that never affect a window's order.
3. **"No window's top-3 set differs" bounds the sample's near-ties, not the
   kernel.** Two runtimes that are not bit-identical swap rows 3 and 4
   whenever the reference gap between them is smaller than the two rows'
   errors. How many windows of a sample hold such a pair is a property of the
   model's score spacing and of the draw; a correct kernel fails the bound
   with a probability that grows with the sample. Today's L6 was never held to
   it (GM-452 compared pairs, not window sets).

#### 4.1.2 The bounds (all must hold, on the fresh sample of 4.1.3)

Notation: `d = rust - python` per pair; `s = logit + beta * cosine` the blend
score, at beta 5.0 for `qat` (fitted in S8, not refitted) and 40.0 for `l6`.
**Reference arm:** today's L6 (`ce-minilm-quint8` Python vs
`ce-minilm-quint8-rust`), measured the same way on the same windows and the
same pairs, in the same run.

| id | bound | principle | why it protects the shown ranking |
|---|---|---|---|
| P1 | mean \|d\| of `qat` <= mean \|d\| of `l6` (point estimates, same pairs; plus the absolute ceiling mean \|d\| <= 0.01 kept) | No worse than shipped: the kernel path may not add more average error to the new model than it adds to today's | The average perturbation every window sees is no larger than the one users already get today |
| P2 | max \|d\| <= 0.20 (kept) | GM-452's max for today's L6 | No single row can jump far enough to cross a clear gap |
| P3 | blend top-1 identical in >= n - 1 windows (kept; 119 of 120) | The first row is what the user reads; one disagreement in 120 is the S1 allowance, unchanged | Bounds the row the F4 page shows first |
| P4 | **tie-aware top-3**: zero windows with a *non-tie* top-3 difference (definition below) | A swap is a defect signal only when the reference gap it crosses is larger than the error today's kernel already makes | Rows 1-3 never change across a gap the shipped product resolves today |

**P4 definition.** Margin `m = 2 x p99(|d|)` of the **`l6` reference arm** on
the same pairs (computed in the same run). For a window, let `A` be the
Python top-3 set and `R` the Rust top-3 set by `s` (stable sort, as in
`gm741_parity.py`). If `A != R`, the window is a non-tie difference when some
`x` in `A \ R` and `y` in `R \ A` have a Python gap `s_py(x) - s_py(y) > m`;
otherwise every swap is a tie and the window counts as agreeing.

Why this margin and not another:
- Two rows' reference scores can cross only if their gap is smaller than
  `|d_x| + |d_y|`. With both errors inside today's p99, that is `2 x p99`. A
  crossing over a larger gap needs an error beyond today's typical kernel
  error: a defect, or noise the new model amplifies. Both are what the gate
  must catch.
- The margin comes from the **reference** arm, not the candidate, so a
  defective candidate cannot widen its own tolerance.
- p99 rather than max: the max of 3,600 pairs is one outlier (GM-452: 0.198),
  and `2 x max` approaches `2 x` the P2 ceiling, where any swap would be
  forgiven and P4 would measure nothing.
- Rows within `m` of each other are already ordered arbitrarily by today's
  kernel; which of them shows third is not a regression, and its effect on
  quality is already inside the section 6 numbers, which were scored on the
  Rust logits.

**Units caveat (stated, not hidden).** `d` is in logits for both arms, which
is the right unit for kernel error (P1, P2, P4's margin). In blend units the
new model's spacing is narrower (beta 5 vs 40), so the same absolute error
reorders near-ties more often than it does for today's L6. That effect is a
property of the model, not of the kernel; the stack eval already contains it,
and P4 still fails any crossing wider than today's error.

**Reported, not gated** (both arms): raw top-3 set agreement, the `l6`
arm's tie-aware top-3 and top-1 at beta 40, top-10 set agreement, mean signed
d, p99 |d|, the logit sd, and the Python gap of every top-3 swap.

#### 4.1.3 Fresh-sample protocol

- **Windows:** 120, as in S8: 15 NL + 15 name per language (go, python,
  rust, typescript), from the v2 K = 30 window set, held-out and FIT mixed as
  drawn.
- **Disjoint from S8:** the S8 windows are exactly `draw()` with seed 741 in
  `gm741_parity.py` (reproducible from code, no scratch file needed). Each
  cell's pool is the cell minus those 15; the new draw is
  `random.Random(74113).sample(pool, 15)` per cell, cells in sorted order.
  Seed 74113 = GM-741, slice 13; fixed here. A cell with fewer than 15
  windows left is a STOP, not a smaller cell.
- **Pairs:** every pair of each window, ship text, the same tokenizer and
  window construction as S8 (`gm700_rust_pairs.setup`), about 3,600 pairs.
- **Python:** onnxruntime via `rerank_eval.CrossEncoder`, one pair per run,
  for both `qat` and `l6`.
- **Rust:** the logits already dumped for every pair: `qat` from the S8 full
  dump (`ce-l6-qat-quint8-rust` in `rerank_cache_gm741_qat`), `l6` from
  `ce-minilm-quint8-rust` in `rerank_cache_gm700_rust`. Logits are
  deterministic; no new Rust run is needed. A fresh-window pair missing from
  either Rust cache is a STOP.
- **Order:** draw, then write the window list and its sha256 to the result
  before scoring; then score both arms; then evaluate P1-P4 once.
- **Code** (measure slice): `gm741_parity.py` gains `--seed` and
  `--exclude-seed`, a `--ce` choice for the arm, reads the Rust logits from
  the cache, computes P4 with `m` from the `l6` arm, and exits 1 when any of
  P1-P4 fails. The old `mean <= 0.005 x sd` and `top3_all_same` checks are
  removed, not left as reported failures.

#### 4.1.4 On a fail

Any of P1-P4 failing on the fresh sample stops the gate for good: the measure
slice records the numbers and stops; section 7 is not run. There is no
further re-derivation, no third sample, no larger sample, and no exception
for a "near miss". GM-741 then does not ship the QAT model as the default in
4.36.0; the owner chooses between leaving it out of the release and opening an
investigation of the Rust kernel path for this model (a diagnosis task, not a
bound change). A missing reference arm (a Rust or Python `l6` logit that
cannot be produced) is a STOP, not a fallback to GM-452's 0.0052.

On a pass, the S8 stack numbers (section 6, all from the same Rust dump)
stand without a re-run, and the measure slice continues with section 7.

## 5. Downstream calibration

Who reads the cross-encoder's score (g-mesh calls in the scratch report):

| consumer | where | reads CE score? | decision |
|---|---|---|---|
| Blend order `logit + BETA * cosine` | `blended_order` (`rerank.rs:155`), its only caller `Reranker::order` (`rerank.rs:566`), its only caller `first_page_reranked` (`mcp/search_code.rs:481`) | **yes** | **re-fit BETA** for `qat`; `l6` keeps 40 |
| Similarity floors / no-match verdict | `similarity::verdict` / `low_similarity` / `below_floor` (`mcp/similarity.rs:290-415`), called from `search_code::handle` on `judged`, the **embedding-order** first page (`search_code.rs:503`) | no, cosine only | keep |
| F4 (eval name for the shipped verdict: blend order shown, floor on the cosine-order top-1) | eval scripts only (`gm461_learned_verdict.py:13`) | the blend picks the shown top; the verdict uses cosine | keep the floors; the blend part follows BETA |
| Exact-name skip | `rerank::confident` (`rerank.rs:177`) | no | keep |
| GM-461 learned verdict (logistic on cosine, logit, gaps) | not in production: NL "задача на внедрение" is a follow-up; name kept F4 | would | that follow-up must fit on the shipped variant's logits; flagged in section 9 |
| GM-701 floor OR blend threshold | not in production (closed null, 2026-11-11) | would | nothing to re-fit |
| `g-mesh status`, `OutcomeRecord` | `cli/status.rs:242-264` | no (presence, size, outcome) | report the variant |

### D4. Blend weight

**Today.** `BETA = 40` (`rerank.rs:63`), chosen for today's L6 logits.
Ordering is `logit + 40 * cosine`; a window's cosines span about 0.1 to 0.3,
so the cosine moves a row by 4 to 12 logits, comparable to today's logit
spread.

**Change.** The new model's logits are about 7 times narrower on the probe
(1.3 vs 8.8). At beta 40 the cosine would decide most windows and most of the
GM-734 ranking gain (dev MRR 0.405 to 0.522, measured on the model's own
ordering) would not reach users.

| option | benefit | risk |
|---|---|---|
| **a. Re-fit beta on eval v2 FIT, per variant (Recommended)** | Same procedure as GM-452's J stack (`gm461 --tune-beta-on-fit`, `gm463_bakeoff.tune_beta`: best FIT NL r@10, then MRR, then the smaller beta, grid 0-320 with edge doubling); judged on held-out only | One more fitted constant; FIT/held-out split of v2 is the only guard against overfitting it |
| b. Keep 40 | No fit | Likely hides most of the gain; the go rule may fail for a reason unrelated to the model |
| c. Beta 0 (cross-encoder only) | Uses the model as trained | Loses the cosine signal GM-443 found useful; not what any stack measured |

Floors are kept: they read the embedding cosine of the embedding-order page,
and the embedder does not change. CW absent therefore cannot move between the
two stacks (section 6, R4).

**How the re-fit is produced** (measure slice, from `eval/embedding`, `D`
the v2 work dir):
1. `gm700_rust_pairs.py dump` then the Rust dump of section 4 step 2, then
   `gm700_rust_pairs.py import` into a new cache dir under a new arm name
   (`ce-l6-qat-quint8-rust`).
2. `gm461_learned_verdict.py --work $D --cache-dir <qat cache> --tune-beta-on-fit --per-query-out pq_qat.jsonl`
   (the FIT beta is printed and stored as `beta_tuned_on_fit`), plus context
   runs at `--beta 40` and `--beta 0`.
3. Base: `gm461_learned_verdict.py --work $D --per-query-out pq_base.jsonl`
   (reproduces GM-461 F4: NL CW positives 296/681, absent 37/164; that is the
   control that the base harness is today's).
4. `gm452_paired_bootstrap.py --base-stack pq_base.jsonl --stack pq_qat.jsonl --base-arm F4 --stack-arm F4 --seed 741 --resamples 10000`.

Script changes this needs (in the edit map): the shipped-sha STOP in
`gm701_floor_or_blend.setup` (`:44-57`, via `gm699_separability.SHIPPED_SHA`
`:26`) and the hard-coded arm names (`gm699_separability.py:25`,
`gm700_rust_pairs.py:24`) take a `--ce` choice; defaults stay today's and
must give byte-identical outputs (the GM-452/S2 check).

## 6. Pre-registered go rule (eval v2 stack)

**Arms.** Base = today's stack: `jina-v2-base-code-int8-structured` cosine,
shipped floors (go .57 / python .59 / rust .57 / typescript .53), today's L6
quint8 (Rust cache `rerank_cache_gm700_rust`), beta 40, F4. Candidate = same
embedder and floors, the QAT quint8 (Rust-scored, section 4), beta tuned on
FIT (D4a), F4. Held-out only; paired bootstrap by query id, stratified by
language, seed 741, 10,000 resamples, percentile 95% CI; deltas are candidate
minus base in points. Definitions as GM-452: CW = confident wrong (wrong top
shown with no warning; absent queries count as wrong), misled = warning on a
right top.

**GO iff all of:**

| id | population | metric | rule |
|---|---|---|---|
| R1 | NL pooled | CW positives | upper bound of delta <= +2.0 |
| R2 | NL, each of go / python / rust / typescript | CW positives | point delta <= +3.0 |
| R3 | name pooled | CW positives | upper bound of delta <= +2.0 |
| R4 | NL and name, every language | CW absent | delta exactly 0 (a control: the verdict is cosine-only; any non-zero cell stops the slice as a harness defect, it is not a NO-GO) |
| R5 | name, each language | misled | point delta <= +1.0, and pooled upper bound <= +2.0 |

Reported, not gated: NL misled per language, held-out MRR and r@10, the
context arms at beta 40 and beta 0, the number of held-out queries whose top
row differs between arms (must be > 0, else the A/B measured nothing).

Why these numbers: the acceptance criterion asks for "no worse", so the rule
is non-inferiority. +2.0 pts is about half of today's pooled CI half-width on
NL CW positives in GM-452 (+-3.9), tight enough to catch a real regression and
loose enough that a null model passes. The per-language point bound (+3.0)
catches a regression hidden by the pool. Name misled keeps GM-697's 3% budget
in spirit: +1 pt per language is a third of it. A NO-GO keeps `l6` as default;
the `qat` variant code can still ship off by default only if the owner says
so.

## 7. Latency and RSS

Driver `eval/embedding/gm464_latency.py` as in GM-700/S1
(`docs/results/gm-700-baseline-latency.md`): product path (`g-mesh
mcp-shim`, real daemon), g-mesh's own index, 30 queries, an untimed warm-up
pass, 3 timed passes, a fresh daemon per arm, load gate (1-min load < 4 held
30 s), the whole run under `caffeinate -i -s`.

- One release binary of the GM-741 branch; arms by `G_MESH_RERANK_MODEL`:
  `old` = `l6` at beta 40, `new` = `qat` at its fitted beta, `off` =
  `G_MESH_RERANK=off`. Order `old new new old old new off`.
- Per arm record: `uptime` before and after; the driver under
  `/usr/bin/time -p` (`real`/`user`/`sys`; `real` with `user` near 0 means it
  was waiting); daemon CPU s / wall s; daemon RSS by `ps -o rss= -p <pid>`
  after the warm-up and after each pass (report the max); `pmset -g therm`
  CPU speed limit at start and end.
- Controls: rerank-on share (pages not in descending cosine order) 90/90 for
  `old` and `new`, 0/90 for `off`; the number of queries whose first page
  differs between `old` and `new` > 0.
- Pass: pooled p50 of `new` <= pooled p50 of `old` + 10%, every `new` arm
  median <= the highest `old` arm median + 10%, and max RSS of `new` <= `old`
  + 10 MB. A failure with the gate open in every arm is a finding; with the
  gate held open less than every arm, the run repeats once.

## 8. Edit map for the code slice

### Change

| file:lines | symbol | change |
|---|---|---|
| `core/src/embedding/rerank.rs:36-50` | `RERANK_MODEL_*`, `RERANK_ONNX_*`, `RERANK_TOKENIZER_*` | replace with a `RerankVariant` table (`Qat`, `L6`): name, dir name, repo, revision, onnx remote path, sizes, sha256s, beta. `L6` holds today's values unchanged; `Qat` holds 1.2's values, the commit from A6 and the fitted beta |
| `rerank.rs:60-63` | `BETA` | becomes the variant's field; keep `L6`'s 40 |
| `rerank.rs:92-102` | `rerank_model_dir_from` | takes the variant; default dir is `~/.g-mesh/models/<variant dir>` |
| `rerank.rs:104-111` | `env_rerank_model_dir` (test) | follow the signature |
| `rerank.rs:113-136` | `check_rerank_files` | variant's shas; the refusal message names the variant and `g-mesh model fetch` (drop the "switched to int8" wording for `qat`) |
| `rerank.rs:152-171` | `blended_order` | take `beta` as an argument |
| `rerank.rs:201-237` | `CrossEncoder`, `load` | keep the variant (for beta and log name); load checks its files |
| `rerank.rs:318-323` | `RerankSettings` | add `variant` and whether it was chosen explicitly (for D3) |
| `rerank.rs:341-359` | `RerankModelState::of` | sizes of the selected variant; with D3a, report the fallback |
| `rerank.rs:379-386` | `OutcomeRecord` | add `variant` (`#[serde(default)]` so an old daemon's record still reads) |
| `rerank.rs:432-452` | `Reranker::new` | loader resolves D3a: default `qat` absent and `l6` present -> load `l6`, log once |
| `rerank.rs:520-560` | `Reranker::outcome` | log line names the variant instead of `RERANK_MODEL_NAME` |
| `rerank.rs:563-596` | `Reranker::order` | pass the loaded variant's beta to `blended_order` |
| `core/src/config/mod.rs:144-165` | `GlobalConfig`, `RerankConfig` | add `model: Option<String>` (`[rerank] model`) |
| `core/src/config/embedding.rs:24-50` | `embedding_settings`, `rerank_settings` | fill the variant |
| `config/embedding.rs:72-78` | `rerank_model_dir` | by variant |
| `config/embedding.rs:94-135` | `Switches`, `resolve` | resolve the variant: env, then file, then default; unknown value -> one log line, default |
| `core/src/knobs.rs:55-57, 179-180` | knobs | add `G_MESH_RERANK_MODEL` (`Kind::User`, "`l6` or `qat`; the rerank model") |
| `core/src/cli/model.rs:97-107` | `Source` doc | mirror path names the variant dir |
| `model.rs:208-230` | `RERANK_FILES` | built from the selected variant (a fn or one const per variant) |
| `model.rs:233-240` | `run` (`ModelCommand::Fetch`) | fetch the selected variant |
| `model.rs:370-417` | `fetch`, `fetch_rerank` | doc and output name the variant |
| `model.rs:496-537` | `status` | print variant, repo, revision; with D3a, say when the fallback is in use |
| `core/src/cli/status.rs:242-264, 817-850` | `RerankStatus`, `rerank_status`, its renderer | show the variant |
| `scripts/install.sh:844-846`, `scripts/install.ps1:645-647` | upgrade text | "run g-mesh model fetch again: it adds the new rerank model" |
| `core/src/embedding/testdata/rerank_parity.json` | fixture | regenerate from the `qat` files at the fitted beta (`gm464_rerank_fixture.py`, needs a `--ce` choice at `:48`) |
| `rerank.rs:1041-1085` | `the_real_model_reproduces_...` | `LOGIT_TOLERANCE` per section 4 |
| `model.rs:1112-1131, 1149-1170` | `a_stale_fp32_rerank_file_is_replaced_by_fetch`, `the_pinned_rerank_model_is_the_quint8_export` | pin both variants' URLs, shas and dirs |
| `eval/embedding/gm699_separability.py:25-26`, `gm701_floor_or_blend.py:44-57`, `gm700_rust_pairs.py:24`, `gm464_rerank_fixture.py:48`, `rerank_eval.py:105-117` | eval harness | a `--ce` choice (`l6` default, `qat`) carrying arm name, model dir and sha; defaults byte-identical |
| `docs/adr/0044-qat-reranker-variant.md` (new) | ADR | the variant setting, the new default, the fallback, the re-fitted beta; ADR 0016/0043 get a "superseded in part" line |

### Read (do not change)

`mcp/search_code.rs:481-518` (`first_page_reranked`: the window, `judged` on
the embedding order), `mcp/similarity.rs:272-415` (floors and verdict, cosine
only), `rerank.rs:177-199` (`confident`), `mcp/search_code_rerank_tests.rs`
(stubs: `reversing` `:318`, `permuting` `:457` assume logit steps of 100 beat
`0.3 * beta`; true for any beta < 333).

### Behaviours the tests must pin

1. With no setting, the daemon loads `qat` from
   `~/.g-mesh/models/g-mesh-reranker-l6-qat` and orders by the `qat` beta.
2. `G_MESH_RERANK_MODEL=l6` (and `[rerank] model = "l6"`) loads today's files
   from today's directory at beta 40; the env wins over the file.
3. An unknown variant value logs one line and uses the default.
4. Default `qat` absent, `l6` present: `l6` is loaded, logged once, and status
   says so. Explicit `qat` absent: `Absent`, no fallback.
5. `check_rerank_files` refuses the other variant's `model.onnx` by sha, and
   the message names the selected variant and `g-mesh model fetch`.
6. `model fetch` downloads the selected variant into its own directory and
   leaves the other variant's directory untouched (bytes and mtime).
7. Upstream URL of `qat` is `https://huggingface.co/<repo>/resolve/<commit>/onnx/model_quint8.onnx`;
   the flat mirror URL is `<base>/g-mesh-reranker-l6-qat/model.onnx`.
8. `blended_order` uses the beta it is given (a pair whose order flips
   between beta 40 and the `qat` beta).
9. `OutcomeRecord` without a `variant` field (an older daemon) still reads.
10. The regenerated parity fixture's `beta` equals the `qat` variant's beta
    and its model sha equals the `qat` pin.

## 9. Must confirm in later slices

- **Slice order.** The measure work (parity, beta fit, go rule, latency)
  needs only local files, and the code only needs the commit hash at the end.
  Recommended order: S2 code with every pin except the `qat` commit and beta
  (placeholders that a test rejects) -> S6 measure -> owner A1-A6 and upload
  only on GO -> a short implement step setting commit, beta and the fixture
  -> S3 review -> S4 tests -> S5 verify. This publishes nothing on a NO-GO.
  The owner decides this re-slicing.
- The upload's commit hash, and that `resolve/<commit>/onnx/model_quint8.onnx`
  and `.../tokenizer.json` return the sha256s of 1.2 (download and hash once).
- The new model's logit scale on the full v2 pair set (mean, sd, window
  spread), against the 3-pair probe.
- The FIT-tuned beta and whether it hit a grid edge (`grid_added`); if > 320,
  re-check the stub tests' assumption (`search_code_rerank_tests.rs:457`).
- R4: CW absent deltas exactly 0 in every cell.
- The parity tolerances of section 4 hold, and the re-derived
  `LOGIT_TOLERANCE`.
- The base harness reproduces GM-461 F4 (296/681, 37/164).
- `G_MESH_RERANK_MODEL_DIR` with D3a: an override directory is used for the
  selected variant only; no fallback probes another directory.
- The GM-461 NL learned-verdict follow-up task fits on the shipped variant's
  logits (it uses the logit as a feature).
- Results doc `docs/results/gm-741-*.md` and the GPU training history row
  (orchestrator).
- Owner answers on 2.2 and 2.3 recorded verbatim before upload.

## 10. Decisions for the owner

| id | decision | recommended |
|---|---|---|
| D1 | Model directory | a. New `~/.g-mesh/models/g-mesh-reranker-l6-qat`, today's left in place |
| D2 | Rollback mechanism | a. `[rerank] model` + `G_MESH_RERANK_MODEL`, default `qat` |
| D3 | Upgrader without the new files | a. Fall back to `l6` for the default only, logged, shown in status |
| D4 | Blend weight | a. Re-fit beta on v2 FIT for `qat`; floors kept |
| D5 | Go rule | Section 6 as written (R1-R5) |
| D6 | Slice order | Measure before publishing; upload only on GO (section 9) |
| A4 | Claude-generated training data | Owner reads the current Consumer Terms and decides; no recommendation from this slice |
