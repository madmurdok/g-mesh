# GM-700/S15: why Rust's int8 cross-encoder logits differ from Python's

Branch `perf/GM-700-rerank-speedup` at 22f427e6, model
`onnx/model_qint8_avx512_vnni.onnx` (MiniLM-L6, dynamic int8, signed
weights), CHUNK 1. Machine: Intel i7-1068NG7 (Ice Lake, AVX512F + AVX512VNNI),
macOS 25.6.0 x86_64. Rust links ort-sys's static ORT
(`git-branch=rel-1.20.0, git-commit-id=c4fb724`, from `ort::info()`); Python
uses the onnxruntime 1.19.2 wheel.

## Cause

macOS enables the AVX512 register state lazily: a fresh process reads
`XCR0 = 0x7` (no opmask/ZMM state) until it executes its first AVX512
instruction, after which it reads `0xe7`. ORT's MLAS picks its int8 GEMM
kernels once per process from `XCR0`, so a Rust process that has not run an
AVX512 instruction before its first session gets the AVX2 `U8S8` kernel. That
kernel's `vpmaddubsw` saturates u8 x s8 pair sums at int16, which the
`avx512_vnni` export (quantized without `reduce_range`) overflows. The Python
process has already run AVX512 code by the time ORT initialises MLAS, so it
gets the AVX512-VNNI kernel, which does not saturate.

So it is not g-mesh's input, graph or session options, and not the ORT
version: it is which MLAS kernel the process ends up with.

## Evidence (540 fixture pairs, one pair per run)

| check | result |
|---|---|
| Rust vs Python tokenization (ids, type ids, mask, shapes `[1, n]` i64) | 0/540 differ |
| fp32 model, Rust vs Python on Rust's inputs | mean 0, max 0 |
| int8, Rust (Level3) vs Python (ENABLE_ALL) on Rust's inputs | mean 0.274, max 0.620 |
| Python running the graph Rust optimised and saved (`with_optimized_model_path`) | equals Python's own logits exactly; still 0.274 from Rust |
| optimisations off in both (Rust `Disable`, Python `ORT_DISABLE_ALL`) | still mean 0.274, max 0.579 apart |
| Python with numpy's AVX512 dispatch off (`NPY_DISABLE_CPU_FEATURES`) | unchanged (0.274) |
| `XCR0` in a fresh C process / after one `vpxord zmm0` | `0x7` / `0xe7` |
| **Rust, one `vpxord zmm0, zmm0, zmm0` before the session is created** | **equals Python: mean 0, max 0 (540/540)** |
| `model_quint8_avx2` export (u8 weights, no saturation), Rust vs Python | mean 0.005, max 0.204 |

The flipped variable in the decisive probe is only whether the process has
touched AVX512 before ORT's first session; inputs, file, options and binary are
the same.

Distance to the fp32 logits (Python fp32 = Rust fp32): Python int8 (VNNI
kernel) mean 0.091, max 0.680; Rust int8 (AVX2 kernel) mean 0.273, max 0.677.
The Rust logits are the saturated ones, three times further from fp32; they
should be what any x86 machine with neither AVX512 nor AVX-VNNI computes with
this export, on any OS (inferred from the kernel choice, not measured on such a
machine).

## Consequences

- The quality measured in docs/results/gm-700-offline.md is the VNNI path's.
  Shipped g-mesh on this Mac (and on any AVX2-only x86) scores with the
  saturating path, which nobody has measured.
- MLAS's choice is per process. `core/src/embedding/model.rs` creates the
  embedder's ORT session too, so a workaround (touch AVX512 when
  `sysctl hw.optional.avx512f` is 1) must run before the first ORT session
  of the process, not in `CrossEncoder::load`, and it only fixes macOS with
  AVX512; AVX2-only machines keep saturating.
- The parity test at 1e-4 can hold only on the kernel family the fixture was
  made with. Regenerating the fixture from Rust would bake in one machine's
  kernel, not a reference.

## Recommendation for the quality slice

Measure the Rust scorer, not Python: a small `#[ignore]`d Rust test or bin that
reads the held-out NL pairs (same rules as gm-700-offline.md) and writes one
logit per pair, run twice in fresh processes: as shipped (AVX2 `U8S8` path)
and with the AVX512 touch (VNNI path, equal to Python). If the AVX2 path
loses quality, the choice is between the `quint8_avx2` export (u8 weights, no
saturation on AVX2; Rust vs Python differ by mean 0.005), the AVX512 touch
plus accepting saturation on AVX2-only machines, or fp32. Keep the Python
fixture as the VNNI reference until that decision; the parity test needs the
AVX512 touch (or a skip off VNNI hardware) whichever export ships.

## Probe

An env-driven `#[ignore]`d test in a throwaway worktree: `PROBE_MODEL`,
`PROBE_TOK`, `PROBE_OPT` (0/1/3), optional `PROBE_SAVE` (optimised graph
path) and `PROBE_AVX512` (run `vpxord zmm0, zmm0, zmm0` first); it tokenizes
each fixture row's `text` like `CrossEncoder::score`, runs it at `[1, n]` and
writes ids, type ids, mask and logit per pair. A Python script fed those exact
inputs to onnxruntime sessions of each variant.

## Owner decision (2026-11-09)

"Перейти на quint8_avx2 (Recommended)". The product ships `onnx/model_quint8_avx2.onnx`, which has u8 weights and so does not saturate on the AVX2 kernel. This replaces the Q4 choice of qint8.
