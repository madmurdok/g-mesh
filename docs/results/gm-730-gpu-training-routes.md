# GM-730: GPU training routes for code search, results

Three routes were run on spot GPUs under the [design](../design/GM-730-gpu-training-routes.md)
(release 4.35.0): RR (reranker distillation), J (jina-v2-base-code fine-tune)
and R3 (full gte-small student). Only J ends with a ship verdict. Every number
below comes from a slice summary or a run directory; a number that could not be
traced to one is marked **untraced**.

## Summary per route

| route | verdict | key metrics vs base | gates | cost (cost.json) | wall time |
|---|---|---|---|---|---|
| RR | **Closed.** fp32 student wins on dev, the quint8 export fails C-P2 | dev MRR 0.5186 (grid) / 0.5188 (final) vs 0.4036 for the off-the-shelf L6 on the same windows; dev r@10 0.8618 vs 0.7308 (grid) | S4 pass (paired dMRR lower bound +0.0974 > 0); C-P pass (5.72e-06); **C-P2 FAIL** (quint8 dev MRR 0.4754 < 0.4852); shuffled control and v2 eval never ran | $3.670 | grid 2.945 billed h incl. one spot interruption; final 0.490 h; teacher 0.718 h over two runs |
| R3 | **Stopped at S4.** Route not viable | best dev MRR 0.4006 (lr5e-5-a1.0) vs base epoch 0 0.3078; S4 needs >= 0.41 | S4 STOP ("cannot reach a cost candidate"); no final job, no v2 eval | $1.470 (cost.json sum; S60 reports $1.466) | grid 120 min billed 1.994 h |
| J | **Ship (D8).** Full fine-tune, `full-lr1e-5-a0` | dev MRR 0.5448 vs 0.4310, dev r@10 0.8114 vs 0.6886; v2 MRR 0.501 vs base int8 0.380, v2 r@10 0.733 vs 0.608 | S4 pass (gain +0.1139 >= 0.034); S5, C-P2, C-S, C-R2 pass; D8 ship | $7.125 (S65 reports $7.126) | grid 393 min, final 81 min |

GM-730 total: $12.267 (S65). The per-run sum below is $12.266 (rounding).
The route cap raises are recorded in the budget table of the design note.

### J against the shipped adapter (v2, same harness)

From `g-mesh-data/gm730/measure/report_jina.txt`: the adapter
(`int8-structured-adapter-full`) has r@10 0.655, MRR 0.423; J
(`gm730-jina-full-int8-structured`) has r@10 0.733, MRR 0.501. The controls
land where they must: shuffled J 0.608 / 0.380 (equal to base int8), the
requantised base 0.608 / 0.380, words-shuffled J 0.657 / 0.412. The report's
own verdict for J is `Undecided("quality gates pass; costs (D11) not
measured")`. The `report_jina_vs_adapter.txt` header says `validity: FAILED`
(reference floors for go, python and rust not within 0.03 of the shipped
numbers), so the paired J-vs-adapter delta from that report is not used here;
the two points above are from the base-referenced report.

### RR

- Epoch-0 base (off-the-shelf L6): dev MRR 0.4036, r@10 0.7308.
- Grid: 8 configs. Winner lr5e-5, tau_t 1, alpha 0.5: dev MRR 0.5186, r@10
  0.8618, dMRR +0.1150 (lower +0.0974). Every alpha 0 config gained at most
  +0.0142, so the teacher signal carries the gain.
- Final: dev MRR 0.5188 (+0.1152 [+0.0976, +0.1329]). C-P2 requantisation
  (S53, laptop CPU) tried four quantiser arms; none passes the fixed floor
  0.48522: A0 0.47683, A1 0.47830, A2 0.47491, A3b 0.47667 (A2 passes the
  relative floor only). Route closed on the quint8 gate.
- The recommendation in the design note was owner-gated: options after the
  C-P2 failure were to stop RR, rerun with another quantisation, or evaluate
  fp32 only (S43). The follow-up is GM-734 (QAT), reported separately.

### R3

Probe 1.152 s/step, 966 steps/config; the first launch stopped at the ETA gate
(111.2 min > 60). After the limits were sized to the probe the grid ran 120
min. Winner lr5e-5-a1.0 reached dev MRR 0.4006, under the 0.41 gate.

### J

- Epoch-0 dev MRR 0.4310, r@10 0.6886.
- Grid (8 configs: 4 full, 4 LoRA, 393 min on g5.xlarge with gradient
  checkpointing): winner `full-lr1e-5-a0`, dev MRR 0.5448, r@10 0.8114. The
  per-config table is not reproduced here (**untraced** in the sources read).
- Final on g5.xlarge us-east-1c, 81 min. S5 (shuffled control): dev MRR gain
  +0.0000 [+0.0000, +0.0000]. C-S and C-R2 pass (identical top-10 on all 3431
  queries for C-R2).

## Per-run cost ledger

Source: each run directory's `cost.json` under `g-mesh-data/gpu-runs/`.

| run | route / job | instance, AZ | hours | USD | outcome |
|---|---|---|---|---|---|
| gm730-rr-teacher-ce8f0eb6f299-20261119T1305 | RR teacher | g5.2xlarge, us-east-1f | 0.3422 | 0.2956 | failed, C-T NaN logits |
| gm730-rr-teacher-ce8f0eb6f299-20261119T1410 | RR teacher (eager attn + guard) | g5.2xlarge, us-east-1f | 0.3756 | 0.3244 | done, C-T diff 1.235e-05 |
| gm730-rr-grid-2b017a2d423d-20261119T1441 | RR grid (fp32) | g5.2xlarge, us-east-1f | 0.0364 | 0.0315 | stopped, ETA gate (309.9 > 96 min) |
| gm730-rr-grid-5751539ab9a9-20261119T1517 | RR grid (bf16) | g5.2xlarge, us-east-1f | 0.0367 | 0.0317 | stopped, ETA gate (156.2 > 96 min) |
| gm730-rr-grid-d24cc51d6898-20261119T1645 | RR grid (limits resized) | g5.2xlarge, us-east-1f then 1d | 2.9453 | 2.5311 | done after one spot interruption |
| gm730-rr-final-3e75efa5b2cc-20261119T2040 | RR final | g5.2xlarge, us-east-1b | 0.4897 | 0.4559 | failed, C-P2 |
| gm730-gte-grid-fa13663d1d8a-20261120T0925 | R3 grid | g5.xlarge, us-west-2a | 0.0331 | 0.0239 | stopped, ETA gate (111.2 > 60 min) |
| gm730-gte-grid-87ceeab9fc5e-20261120T0941 | R3 grid | g5.xlarge, us-west-2a | 1.9942 | 1.4460 | done, S4 stop |
| gm730-jina-grid-1ac08e930915-20261119T2352 | J grid (no checkpointing) | g5.xlarge, us-east-1f | 0.0322 | 0.0203 | failed, CUDA OOM at 22 GiB |
| gm730-jina-grid-g6e-6d5efb4a2da0-20261120T1402 | J grid | g6e.xlarge, us-east-1a | 0.2489 | 0.3957 | stopped, ETA gate (139.5 > 130 min) |
| gm730-jina-grid-g6e-8182f0a9f1b5-20261120T1439 | J grid | g6e.xlarge, us-east-1a | 0.2483 | 0.3948 | failed, CUDA OOM at 44 GiB |
| gm730-jina-grid-7e52ebacef05-20261120T1711 | J grid (grad checkpointing) | g5.xlarge, us-east-1c | 6.5500 | 5.2361 | done, 8 of 8 steps |
| gm730-jina-final-db3a53552cfe-20261120T2357 | J final | g5.xlarge, us-east-1c | 1.3478 | 1.0786 | done |
| | **RR $3.6702, R3 $1.4699, J $7.1255** | | | **12.2656** | S65 total: $12.267 |

`gm730-rr-qat-27c57b8dd91d-20261121T0020-a1` ($0.5099, g5.xlarge) belongs to
GM-734, not to this total. Spot-request cancellations cost nothing; the
capacity-failed launch attempts (8 and 9 before the RR grid and final, 10x120s
for the first g6e launch) billed $0.

## Budget decisions by the owner

Quoted from the design note (section 10 and the section 5 budget table).

- D1: "RR → J → R3 (Recommended)"
- D2: "$20, жёсткий стоп $25 (Recommended)"
- D8: "MRR > 0, r@10 ≥ −1 пт (Recommended)"
- D3-D7: "Да, все рекомендованные (Recommended)"
- J grid after the alpha 0.5 OOM on L40S, 2026-11-20: "Включить gradient
  checkpointing и перезапустить (Recommended)" (hyperparameters and grid unchanged).
- J cap before the final, 2026-11-21: "Для,финала бюджет расширим"; the J route
  cap went from $8 to $8.95 (commit 0639ec80).
- RR cap went from $4 to $4.50 on 2026-11-20 because the grid spend ($3.21)
  left the final job $0.22 short (design note; commit 5be75fea). The note
  gives no verbatim option label for this change.

## Incidents and fixes

| incident | what happened | fix |
|---|---|---|
| RR-T NaN teacher logits (S21) | C-T failed with NaN in 6890/625698 train and 904/96575 dev logits | eager attention plus a finite-logit guard in the scorer (d028aacf, tests 41423d4b); rerun passed |
| ETA gate (S2) | RR grid stopped twice (309.9 and 156.2 min vs 96), R3 once (111.2 vs 60), J on g6e once (139.5 vs 130) | RR grid moved to bf16 (29f518b1, probe 1.805 to 0.910 s/step); limits sized to the measured probe ETA (c415165f, e289b448, d128f31c); the gate itself worked as designed |
| OOM on A10G (S54) | J full fine-tune, batch 32, OOM 70 s into step 1 on 22 GiB | not fixable by size alone, see next row |
| OOM on L40S (S57) | the g6e relaunch passed the ETA gate and step 1 (MRR 0.5457) then OOM at step 2 (alpha 0.5, teacher) on 44 GiB | gradient checkpointing (fa5f0b8e; tests 22ba2254, 52e50ae4), about 1.4x slower; the grid then ran on g5.xlarge in 393 min for $5.236 |
| spot capacity | RR grid: 8 failed attempts then a spot interruption at 19:18Z after 141 min, relaunched on attempt 2; RR final: 9 failed attempts; J g6e: 10x120s retries once, 5 retries once | GPU family fallback jobs g6e, g6 (5fdfcc85); launcher resume covered the interruption; capacity failures billed $0 |
| stale spot request (S28) | a fulfilled spot request of a terminated instance held the 8 vCPU G/VT quota and blocked two launches | cancelled by hand, then launcher cancels the run's spot requests and deletes its instant fleets when an attempt ends (cbdd630e) |
| reap | a detached run killed within 10 minutes left its fleet, and a failed cancel hid a live request from verify-clean | `reap` releases young ended fleets, keeps a fleet whose request was not cancelled, exits 1 on a failed release (85875933) |
| IAM `s3:GetObjectTagging` (S43) | the `checkpoint.prev` copy failed with AccessDenied on the instance role in every attempt of the RR final; non-fatal | instance role gets the object-tagging read (GM-733, tests f20d3449, merge 1a95f14a) |
| launcher DNS crash (S57) | launcher crashed at 14:13Z on a DNS failure during the first g6e launch | resumed; no spend lost beyond the run itself |
| budget refusal | `budget.py` refused the RR final ($3.21 + $1.01 > $4) and the J final ($6.05 + $2.60 > $8) | the owner raised the route caps, see above |

## Open items

- Shipping J (`gm730-jina-full-int8-structured`) as the default model is a
  separate task; this slice only reports the evidence. Costs (D11) of J were
  not measured by the report.
- RR continues in GM-734 (QAT), reported separately; the fp32 student result
  above is informational until a quantised export passes C-P2.
- R3 is closed; no follow-up.
- The J per-config grid table and the J v2 per-corpus breakdown are not in the
  sources read for this doc (**untraced**).
