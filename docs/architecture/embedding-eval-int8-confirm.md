# Confirmatory int8 study: jina-v2-base-code int8 vs fp32 on new queries (GM-422)

Status: pre-registration, written before any new query exists (GM-422/S6,
2026-09-29, branch `docs/GM-422-int8-q5-floor-fit`). Nothing below has been
authored or embedded. Once the first new query is written, this note is not
edited except to append a dated "Deviations" entry, as in
[`embedding-eval.md`](embedding-eval.md). Sections marked **owner decides**
are the only open points; each must be closed, and the choice recorded here,
before the first new query is authored.

## Why a separate study

GM-398 kept fp32. int8 matched it on retrieval (Q1 +0.5 [-1.0], Q2 -0.003
[-0.012], Q4 -1.4 [+1.0]) and failed only Q5, the held-out false-alarm gate:
+2.1 [-1.4, +6.2] against a +5 limit. [GM-422/S1-S2](../results/gm-422-int8-q5.md)
found that failure to be mostly floor-fit noise on a small sample: Go has 16
paired held-out queries, one of its two discordant queries comes only from
the 0.56 vs 0.57 Go floors, and the D9 point condition (Δ <= 0) fails a truly
equal candidate 22-48% of the time.

The owner asked for more tests of int8 (2026-09-29): "давай расширим выборки
для int8. По текущим тестам у меня складывается впечатление что она не хуже
или почти не хуже. Надо погонять еще тесты и попробовать придумать что нам
нехватает для того чтобы использовать int8 вмесо fp32."

D2 of `embedding-eval.md` forbids extending the GM-398 set after measuring,
and its stage 2 applies only to a recall@10 bound in [-7, -5), which is not
int8's case. So this is a new study with its own queries, fixed here before
any of them exist. The GM-398 queries are not re-used as test data; they
supply only the planning inputs below and the frozen floors (section 4).

**What it decides.** int8 is switched in if and only if it passes every gate
of section 2 on the new queries. GM-398's int8 verdict (fail on Q5) is not
overridden by pooling; it is replaced by this study's verdict. There is no
third study: if int8 fails here, fp32 stays, and a later int8 proposal needs
a new reason, not a new sample. That keeps the chance that a truly +5-worse
int8 is switched in at this study's own false-pass rate, rather than adding
one more 5% draw per attempt.

## 1. Power: how many new queries

### What is being estimated

With the floors frozen (section 4), every new query is held-out, and the
gates measure the configuration that would actually ship: int8 at its
GM-398 floors (0.57 / 0.57 / 0.55 / 0.53) against fp32 at its own (0.56 /
0.58 / 0.56 / 0.55). The floor-fit noise that S2 blamed for the failure is
then no longer a random quantity in the test. It is a fixed property of what
ships, and the Go floor gap (0.57 vs 0.56) is part of int8's real false-alarm
cost. So the question the study can answer is "does int8, shipped with the
floors GM-398 fitted for it, false-alarm or answer wrongly more than fp32?"

### Formula

Q5 is a paired mean of d in {-1, 0, +1} over the queries both arms rank
first, averaged over the four languages with equal weight. With n_l new
positives in language l, a pair rate r_l (both arms rank the answer first),
and a per-pair variance s_l² (about the discordance rate p_l):

    SE(Q5)² = (1/16) · Σ_l  s_l² / (r_l · n_l)

Rule B passes when Δ̂ + 1.645·SE <= +5. A candidate with true Δ = δ passes
with probability about Φ((5 - δ)/SE - 1.645), so power 1-β needs
SE <= (5 - δ) / (1.645 + z_β). Q4 has the same form over every held-out row,
positives and absent, with no pair-rate factor. Q1 and Q2 are D2's
recall@10 and MRR formulas.

Inputs, from GM-398's authored queries of both halves at the frozen floors,
drawn 200,000 times (`int8_confirm_power.py`, and the table it prints):

| language | pair rate r | Q5 p, equal | Q5 p (Δ), int8 as measured | Q4 p, equal | Q4 p (Δ), measured |
|---|---|---|---|---|---|
| go | 0.30 | 0.036 | 0.066 (+6.6) | 0.026 | 0.088 (-7.2) |
| python | 0.39-0.42 | 0.002 | 0.000 (0.0) | 0.021 | 0.040 (+2.5) |
| rust | 0.13-0.15 | 0.006 | 0.000 (0.0) | 0.031 | 0.041 (-0.9) |
| typescript | 0.44-0.47 | 0.028 | 0.023 (-2.3) | 0.027 | 0.071 (+4.0) |

"Equal" is fp32's own records with both arms' top scores jittered by
independent noise (total SD 0.012, S1's measured quantization shift), both
judged at fp32's floors, so the true Δ is 0. "Measured" is int8's real
records at its own floors. Its true pooled Q5 Δ at the frozen floors is
**+1.1 points** (Go +6.6, TypeScript -2.3). The Go figure rests on 2
discordant pairs out of 30, so it is itself uncertain. Q4 as measured is
-0.4 pooled: int8 answers wrongly less often in Go.

Plugging in: for an equal int8 at 90% power, SE <= 1.71 points, which gives
n >= 48 positives per language. For int8 as measured (δ = +1.1), SE <= 1.33,
which gives n >= 91. Q4 needs fewer (its per-row SD is similar and every row
counts). Rust dominates the Q5 sum through its low pair rate (0.13), and Go
through its discordance.

### Simulation

The normal formula is optimistic when discordant pairs are rare. So the
decision uses a simulation: [`eval/embedding/int8_confirm_power.py`](../../eval/embedding/int8_confirm_power.py).
It reuses `q5_floor_sensitivity.py`'s ported loaders, floor fit, indicators,
`Boot` and `resampled_floors`. Each rep draws a new study of n positive and
n/4 absent queries per language with replacement from GM-398's authored
queries of that language, both arms' outcomes attached, all judged as
held-out at the frozen floors. It then applies Q1-Q5 as the harness does
(paired, languages weighted equally, one-sided 95% percentile bootstrap).
Reps: 400 (100 for the joint CI), 2,000 resamples each, seed 4226, jitter
SD 0.012.

**Control** (the run stops on failure): the floors refit from the stored
rankings equal GM-398's, `Boot` reproduces int8's Q5 +2.1 [-1.4, +6.2], and
the record pools reproduce S1's held-out Q5 pairs (Go 16 with +2/-0, Python
20, Rust 6, TypeScript 25 with +0/-1). All pass.

Scenarios: **equal** (true Δ = 0), **measured** (int8 as it is, true Q5
+1.1, Q4 -0.4), and **harm5**: equal, plus a share of the candidate's queries
made to fail the gate (Q5: right-first positives pushed below every floor;
Q4: held-out rows turned into clearing wrong answers). The share is set so
that the true Δ is +5 points in every language, exactly at the margin.

Share of reps passing (equal and measured: higher is better; harm5: this is
the false-pass rate). "all" = Q1-Q5 together. The joint columns are rule B
with the joint CI (section 2).

| n pos / lang (abs) | scenario | Q5 A | Q5 B | Q4 A | Q4 B | Q1, Q2, Q3 | all, rule A | all, rule B | Q5 joint | Q4 joint |
|---|---|---|---|---|---|---|---|---|---|---|
| 25 (6) | equal | 78% | 80% | 62% | 96% | 100/100/100% | 48% | 76% | 51% | 55% |
| 25 (6) | measured | 64% | 64% | 67% | 84% | 93/95/97% | 37% | 45% | 62% | 14% |
| 25 (6) | harm5 | **18%** | **20%** | **1%** | **7%** | 100/100/100% | **0%** | **1%** | **9%** | **0%** |
| 50 (12) | equal | 65% | 86% | 62% | 100% | 100/100/100% | 42% | 86% | 61% | 82% |
| 50 (12) | measured | 44% | 64% | 60% | 97% | 99/98/100% | 26% | 60% | 83% | 23% |
| 50 (12) | harm5 | **8%** | **16%** | **1%** | **9%** | 100/100/100% | **0%** | **1%** | **3%** | **1%** |
| 100 (25) | equal | 56% | 96% | 54% | 100% | 100/100/100% | 31% | 96% | 92% | 97% |
| 100 (25) | measured | 24% | 89% | 70% | 100% | 100/100/100% | 18% | 89% | 97% | 46% |
| 100 (25) | harm5 | **0%** | **12%** | **0%** | **6%** | 100/100/100% | **0%** | **2%** | **5%** | **0%** |
| 150 (38) | equal | 54% | 99% | 53% | 100% | 100/100/100% | 26% | 99% | 90% | 98% |
| 150 (38) | measured | 16% | 93% | 70% | 100% | 100/100/100% | 12% | 93% | 100% | 52% |
| 150 (38) | harm5 | **0%** | **8%** | **0%** | **6%** | 100/100/100% | **0%** | **0%** | **0%** | **0%** |
| 200 (50) | equal | 54% | 100% | 54% | 100% | 100/100/100% | 30% | 100% | 98% | 100% |
| 200 (50) | measured | 13% | 98% | 73% | 100% | 100/100/100% | 10% | 98% | 100% | 48% |
| 200 (50) | harm5 | **0%** | **11%** | **0%** | **6%** | 100/100/100% | **0%** | **0%** | **1%** | **0%** |
| 300 (75) | equal | 52% | 100% | 52% | 100% | 100/100/100% | 27% | 100% | 100% | 98% |
| 300 (75) | measured | 7% | 100% | 75% | 100% | 100/100/100% | 6% | 100% | 100% | 76% |
| 300 (75) | harm5 | **0%** | **9%** | **0%** | **6%** | 100/100/100% | **0%** | **0%** | **2%** | **0%** |
| 400 (100) | equal | 52% | 100% | 52% | 100% | 100/100/100% | 25% | 100% | 100% | 98% |
| 400 (100) | measured | 4% | 100% | 79% | 100% | 100/100/100% | 4% | 100% | 100% | 71% |
| 400 (100) | harm5 | **0%** | **8%** | **0%** | **6%** | 100/100/100% | **0%** | **0%** | **0%** | **0%** |

In harm5 the "all" columns are near 0% because Q4 and Q5 are both harmed at
once. The false-pass rate of one gate is its own column. Runtime: real 710 s,
user 470 s, sys 206 s, load average 5-7 on 8 cores. The run was CPU-bound.
The rows for equal and measured are byte-identical to an earlier run that
differed only in the Q4 harm share; that run's Q4 share had been calibrated
at a 1:1 positive:absent mix, which put its true Δ at +4.4.

### Reading

- **Rule A cannot be powered at any n.** An equal candidate passes its point
  condition about half the time on each of Q4 and Q5. All five gates pass
  together at most 48% of the time, falling to 25% as n grows, because ties
  at exactly 0 become rarer. int8 as measured has a true Q5 of +1.1 at the
  frozen floors, so under A it fails with certainty as n grows (4% pass at
  400). More queries make rule A worse for int8, not better.
- **Rule B separates equal from +5 from about 100 positives per language.**
  At 150 per language, an equal int8 passes all gates 99% of the time and
  int8 as measured 93%. A +5-worse candidate passes Q5 8% of the time and
  Q4 6%. The nominal rate is 5%. The excess comes from the percentile
  bootstrap with rare discordant pairs, and it does not shrink with n
  (8-12% from 100 to 400).
- **The joint CI caps int8's chance whatever n is.** It is well calibrated
  (0-5% false pass at 100 or more). But the fit-set noise it adds comes from
  GM-398's fit half, which the new queries do not enlarge. So int8 as
  measured passes Q4 under the joint CI only 46-76% of the time even at
  400 per language, while its Q4 point estimate is -0.4. Q4 is the gate
  most sensitive to the floors: a confident-wrong rate of about 45% puts
  many top hits near a floor.
- **Q1-Q3 are not the constraint.** int8 as measured passes them 99-100%
  of the time from 50 per language up.

### Required n

**150 new positives and 38 absent queries per language: 600 positives and
150 absent, 750 queries.** At that size, rule B gives 99% (equal) and 93%
(as measured) power, with a 6-8% false-pass rate at the margin. 200 per
language (1,000 queries) raises "as measured" to 98% for 250 more queries.
Both are within the ~1,000 limit.

**Go gets priority.** Go is authored first, and Go is not cut if the budget
shrinks. It gets **200 positives and 50 absent**, not 150. The reason: the
Go-only Q5 Δ, which is where GM-398's failure sat, is reported beside the
pooled gate. At 150, Go has about 45 paired queries and a per-language SE of
about 3.7 points. At 200 it has about 60 pairs and an SE of about 3.2. That
is still not a +5 per-language gate: at the frozen floors Go's own expected
Δ is +6.6, and D9 pools by design (owner decision 2026-09-27). Extra Go
queries lower the pooled SE a little, so the 150-per-language simulation is
conservative for this allocation.

| language | corpus | positives | absent |
|---|---|---|---|
| go | gin | 200 | 50 |
| python | py-requests | 150 | 38 |
| rust | ripgrep / g-mesh | 75 / 75 | 19 / 19 |
| typescript | excalidraw / task-tracker-mcp | 90 / 60 | 19 / 19 |
| **total** | | **650** | **164** (814 queries) |

TypeScript is split 90/60, not 50/50, because task-tracker-mcp's frame is
136 targets and GM-398 consumed 53 of them (50 used, 3 skipped). About 80
remain before overlap with GM-398's expected sets, and the skip rate there
was about 6%. Remaining capacity elsewhere, using GM-398's consumed prefix
and skip rate: gin about 450 positives (frame 685), py-requests about 250
(frame 427), and ripgrep, g-mesh and excalidraw well over 1,000.

## 2. The decision rule (owner decides; fixed before authoring)

Every option keeps the rest of D9: validity (D7's broken arms), Q1 and Q2
(point and bound), Q3 (per language >= -10), and cost. Cost is taken from
GM-398, since it does not depend on the queries: int8 wins on size (0.26x)
and RSS (0.48x) and misses pass time (0.69x vs 0.60x). Q4 and Q5 are judged
on the new queries at the frozen floors under one of these rules.

| rule | Q4 and Q5 condition | CI | equal int8 passes all gates (150 / 200 per lang) | int8 as measured passes all | +5-worse passes Q5 / Q4 |
|---|---|---|---|---|---|
| **A**: D9 as is | Δ <= 0 and upper <= +5 | fixed (floors frozen) | 26% / 30% | 12% / 10% | 0% / 0% |
| **B**: bound only | upper <= +5 | fixed (floors frozen) | **99% / 100%** | **93% / 98%** | **8% / 6%** (8-12% / 6-9% over n = 100-400) |
| **J**: bound only, joint CI | upper <= +5 | fit-set floor noise propagated (O4) | 88% / 98% (Q5 x Q4 joint) | about 52% / 48% (Q4 joint caps it) | 0% / 0% (0-5% over n = 100-400) |

**Recommendation: rule B at frozen floors.**

- It is the only rule under which an equal int8 is likely to pass, and more
  data helps it.
- Freezing the floors removes S2's main objection to B. With refit floors,
  the fixed CI ignored floor noise, and 17% of at-margin candidates passed.
  Here the floors are constants of the configuration being judged, and the
  bootstrap covers all the randomness that is left. The at-margin rate falls
  to 6-8% at the recommended n.
- It is the rule S2 already recommended for every model. It is not tuned to
  int8: it was chosen before any new data exist, and int8's own Go floor gap
  counts against it.
- **Risks**: B passes 6-12% of exactly-+5 candidates, not 5%. If the owner
  weighs that above power, the fix is a stricter bound (97.5% one-sided, as
  in D2's stage 2), not the joint CI. That was not simulated, and it would
  cost power roughly like moving from 150 to 100 per language. The switch
  also accepts a real Go false-alarm excess of about +6.6 points (pooled
  +1.1) that comes from the floors int8 ships with. The pooled gate
  tolerates it by design. A per-language Go gate would reject int8 at any n.
- **Rule A** is D9 unchanged. It is recorded to show that keeping it makes
  the study pointless: int8's chance of passing falls as n grows.
- **Rule J** is calibrated, but it answers a different question: "is int8
  equal under any floors a fit could have produced?" The new queries cannot
  shrink its floor-noise term, so at most about half of int8-as-measured
  passes, and at the margin it is at least as strict as B.

**Scope.** This rule governs this study. Whether D9 itself changes for
future candidates (S2's proposal) is a separate owner decision. This note
does not edit `embedding-eval.md`.

## 3. Query protocol

D3 of `embedding-eval.md` applies unchanged, with these fixed choices:

1. **Corpora and snapshots**: the six D1 corpora at their pins, and the D4
   snapshots GM-398 embedded, identified by sha256 (from
   `eval/embedding/work/<corpus>.snapshot.json`). A snapshot with a
   different sha stops the study.

   | corpus | snapshot sha256 | embeddable nodes |
   |---|---|---|
   | gin | `1e013b7630a1f154dbe946ca243ee881d5dfe0bea9861816a100c74b4e068c62` | 1,547 |
   | py-requests | `d24c70266c36a27af0d923c8fc6ab65d37cdad3c0a71d8c7c7c38de3e8afe7fd` | 961 |
   | ripgrep | `d00f8a256d17c0cda5fe81c1a0c744a697d8c2cce9d5a6075e11f9ff3954aca0` | 3,428 |
   | g-mesh | `1fd9a1563cd55afa2eb9be061735dc56394027ad061bfac64e2ac0c65d03fd02` | 6,774 |
   | excalidraw | `3c7bcb4601c64f93bf8b6b043f8140f8b0f9781a6e292e0b236fdfd2102dca4d` | 2,758 |
   | task-tracker-mcp | `6e2381ba9123712a3114c5a6127fe4f29e0a27e372147ac392c8ad2ce74041ff` | 157 |

2. **Target-first sampling with a new seed: 4222.** `sample_targets.py`
   runs over the snapshot itself (GM-398's frame databases no longer exist,
   and the snapshot is the node set that is embedded). It uses the same
   filters (embeddable, not test or fixture, present at the pin) and the
   same 6/3/1 strata pattern. The frame's sha256 goes into the target
   file's `meta`.
3. **Disjoint from GM-398.** Before ordering, the frame drops:
   (a) every target GM-398 consumed, meaning rows 1..k of
   `targets/<corpus>.jsonl`, where k is the highest target number used or
   skipped; and (b) every symbol in the `expected` set of any GM-398
   authored query, matched on `filePath` plus `qualifiedName`. The dropped
   count goes into `meta`. A new query may list a GM-398 target as an
   *additional* acceptable answer (D3 step 3) only when it plainly
   qualifies. Its own target must be new. Query texts must not repeat a
   GM-398 text. The harness check is exact match after lowercasing, and the
   verify slice also checks for near-duplicates. `sample_targets.py` needs
   an `--exclude` input for this (implement slice).
4. **Blind to models.** As D3 step 5, and stricter:
   - Authors are fresh agents briefed only with D3, this section and the
     target list. They do not see GM-398's or GM-422's results documents,
     any `rankings.jsonl`, the S1 discordant-query table, or the floors.
   - No author calls `search_code`, the semantic rung, or any embedding.
   - Absent queries follow D3 step 4.
   - Shapes alternate phrase and sentence.
   - At least half of each corpus's positives have `overlap = false` (D3
     step 6).
5. **Ids and files.** Ids are `<GM-398 prefix>-c<NNN>` (for example
   `gin-c001`), so no id collides with GM-398's. Queries go in
   `eval/embedding/confirm/queries/<corpus>.jsonl` and targets in
   `eval/embedding/confirm/targets/<corpus>.jsonl`, in D3's record format.
   `derivation` names seed 4222 and the target number. There are no
   mechanical queries, since floors are not fitted (section 4).
6. **Freeze and hash.** When all six files are written and checked
   (`check_queries.py`, the harness's resolve step, and the verify slice), a
   single commit adds them. The same commit adds
   `docs/results/gm-422-int8-confirm.md` with each file's sha256 and query
   counts. This happens before any arm is run on them. The run manifests
   record the same hashes (`queryFiles`), and the analysis stops on a
   mismatch, as `q5_floor_sensitivity.py` does. A later fix is a logged
   deviation applied to every arm (D3 step 7).
7. **Every new query is held-out.** The `sha256(id)` parity split is not
   used for the verdict.

## 4. Floors and old data (decided)

- **Floors are frozen at GM-398's fitted values and are not refit.** fp32
  uses go 0.56, python 0.58, rust 0.56, typescript 0.55. int8 uses go 0.57,
  python 0.57, rust 0.55, typescript 0.53. These are the floors a switch
  would ship (D6: "a switch ships the fitted floors"), and they were fitted
  before this study was conceived. Freezing them makes every new query
  test data, which doubles the usable n over a fit/held-out split, and it
  turns the floor-fit noise from a random term into a known property of the
  configuration. Refitting on "GM-398 fit + new fit half" would spend half
  of the new queries on the fit. It would also let the new data move the
  floors that the new data then judge, and it would leave a second verdict
  to choose between.
- **Old and new data are not combined for the verdict.** GM-398's queries
  are the data that prompted this study. Pooling them would be exactly the
  extension after measuring that D2 forbids, with a known selection effect.
  They are used only for the frozen floors and the planning inputs of
  section 1. The results document may show an old+new table as
  descriptive, labelled "not gated".
- **Secondary, reported, not gated**: the harness's own `report` on the new
  queries plus the unchanged mechanical fit queries, with floors refit (D6
  as written), and Q5 with both arms at fp32's floors (S1's shared-floor
  view). If the gated verdict and these disagree, that is reported. It does
  not change the verdict.
- **Analysis tooling**: the harness `report` refits floors, so the gated
  numbers come from a script (`eval/embedding/int8_confirm_report.py`,
  implement slice). It reuses `q5_floor_sensitivity.py`'s ported code paths,
  takes the floors above as constants, and treats every `-c` query as
  held-out. It uses the harness bootstrap (SplitMix64, seed 398, 10,000
  resamples, one-sided 95%). Its first step is the control from section 5.

## 5. Controls and cost

### Controls (each must pass, or there is no verdict and fp32 stays)

1. **Frozen-floor reproduction.** On the stored GM-398 runs, the report
   script reproduces the GM-398 floors and int8's Q5 +2.1 [-1.4, +6.2]
   (as `int8_confirm_power.py` already does).
2. **Node vectors unchanged.** The confirm runs re-use GM-398's node vectors:
   the harness skips node embedding when the variant fingerprint, snapshot
   sha, node-id hash and dimension match (`embed_eval.rs`, `same_nodes`).
   The script checks that each confirm run's `vectors.bin` sha256 equals
   the GM-398 run's, for fp32, int8, random and shuffled.
3. **Broken arms (D7).** `random` and `shuffled` are run on the new queries.
   fp32's pooled recall@10 must exceed each by at least 20 points, each must
   be at most 0.25x fp32, and the one-sided bounds must not overlap. int8
   must clear the same margins, and random's recall@10 must be within D7's
   chance band. Both arms re-use stored vectors (shuffled derives from
   fp32's), so they cost seconds.
4. **Planted-harm arms (does the gate see harm in *this* sample?).** The
   script derives two arms from int8's real new rankings, with seed 4223:
   - **int8-harm10**: about 11-12% of its right-first positives pushed
     below every floor (Q5), and 16-36% of its held-out rows made
     clearing-wrong (Q4). The shares are `int8_confirm_power.py`'s `harm5`
     shares doubled, so the true Δ is +10.
   - **int8-harm5**: the same at +5.

   int8-harm10 must fail Q4 and Q5 under the chosen rule. The simulation
   says a +10 candidate always fails (GM-422/S2: 0% pass under every rule),
   so a pass would mean the sample cannot see harm, and the study is void.
   int8-harm5 is reported. The simulation expects it to fail Q5 about 92%
   of the time and Q4 about 94% at the recommended n.
5. **Null arm.** fp32 against itself gives exactly 0 on every gate and
   passes. This checks the script's pairing.

### Cost

- **Authoring: 814 queries** (650 positives, 164 absent), with about 170
  more targets read and skipped at GM-398's skip rates (6-38% per corpus).
  GM-398 wrote 500 authored queries inside one implement slice that also
  built the harness: 145 turns, $18 at the orchestrator, 20.9 h wall
  including an overnight usage-limit pause. The authoring share of that
  slice was not recorded separately. Expect about 1.6x GM-398's authoring:
  one Opus author agent per corpus (six, at most three at a time), Go
  first, plus a verify slice. The verify slice uses a fresh agent to audit a
  seeded 10% sample of each file (expected sets by structural lookup,
  blindness, disjointness, overlap share). Plan a working day of agent time
  and roughly $30-50. This is an estimate, not a measurement.
- **Embedding**, from S14's 749.4 s per fp32 pass over g-mesh's 6,774 nodes
  (0.111 s/node) and 514.3 s per int8 pass (0.076 s/node):
  - **With node vectors reused** (control 2, the plan): only the 814 queries
    are embedded. GM-398's per-query means were 40-200 ms for fp32 and
    10-14 ms for int8, which comes to about 1-3 min for fp32 and about 10 s
    for int8. Random and shuffled take seconds.
  - **Worst case, no reuse**: all six corpora, 15,625 nodes, take about
    1,730 s for fp32 and 1,190 s for int8: **about 49 min** on a quiet
    machine. The optional words-shuffled arm adds another fp32 pass
    (about 29 min).
  - Timing runs are not needed: cost is taken from GM-398.
- **Analysis**: seconds per arm (10,000-resample bootstrap).

## Owner decides (before authoring)

1. The rule: **B** (recommended), A or J (section 2). With B, optionally a
   97.5% bound instead of 95%. That is stricter at the margin and not
   simulated here.
2. n: **814 queries, Go 200 + 50** (recommended), or 1,000 with 200
   positives in every language (int8-as-measured power 93% -> 98%).
3. Whether this study's verdict replaces GM-398's int8 verdict, as proposed
   under "What it decides". The proposal: switch if and only if int8 passes
   here, with no third study.
4. Whether to accept the Go false-alarm excess at the shipped floors
   (expected +6.6 points in Go, +1.1 pooled). The pooled gate allows it. The
   only alternative is refitting int8's Go floor, which is a separate
   floor-policy change, not part of this study.

## Deviations

(none yet)
