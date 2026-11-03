# GM-439: ONNX Runtime execution providers for the embedding pass

Status: design (S1), for owner review. Scope: the `debug-embed-eval` harness
only. The daemon, the shipped binary and its default stay CPU.

## Question

GM-398 timed every arm on ONNX Runtime's CPU execution provider (EP). Can a
platform EP (CoreML, DirectML, CUDA) cut the jina fp32 (R) pass time with the
same model, and what does it cost to set up and to fall back?

## What the build has today

- `core/Cargo.toml:103`: `ort = "=2.0.0-rc.9"` with default features
  (`ndarray`, `half`, `download-binaries`, `copy-dylibs`) and no EP feature
  ("CPU-only, per REQUIREMENTS.md"). `ort-sys = "=2.0.0-rc.9"` is pinned
  (rc.10+ has no Intel macOS prebuilt).
- ONNX Runtime 1.20.0 comes from pyke's prebuilt *static* library
  (`ort-sys` `download-binaries`, `dist.txt` feature set `none`), cached at
  `~/Library/Caches/ort.pyke.io/dfbin/<target>/<sha>/onnxruntime/lib/`.
  No system library, no `ORT_LIB_LOCATION`.
- The prebuilt static libs already contain the CoreML EP on both
  `x86_64-apple-darwin` and `aarch64-apple-darwin` (`nm` finds
  `_OrtSessionOptionsAppendExecutionProvider_CoreML` and
  `onnxruntime::CoreMLExecutionProvider`). The Windows static lib contains
  the DirectML EP (`DmlExecutionProvider`). CUDA needs a different
  distribution (`cu12`, a dylib) and a GPU.
- Today's g-mesh binary links only Foundation/CoreFoundation (`otool -L`):
  without `ort`'s `coreml` feature the CoreML objects are not pulled in.
  `ort`'s `CoreMLExecutionProvider::register` returns "was not registered
  because its corresponding Cargo feature is not enabled" without it.

## Which EPs are reachable

| EP | Host | Verdict |
|---|---|---|
| CoreML | this Mac: Intel i7-1068NG7 (x86_64), Iris Plus (Metal 3), macOS 26.6.2, **no Neural Engine** | **Reachable.** Registers and runs jina fp32 (probe below). Measure it. |
| CoreML | CI `macos-15` (arm64 VM) | Builds, but a shared VM has no usable GPU/ANE and noisy timing. Not a measuring host. |
| DirectML | Windows: `release.yml` `windows-2022` runner only | No GPU on hosted runners (DML would run on the software adapter); no Windows host here. Excluded. |
| CUDA / TensorRT / ROCm | none | No NVIDIA/AMD compute host; `cu12` is a different (dylib) distribution. Excluded. |

CoreML on Intel: no ANE, so CoreML runs on the GPU (Metal, NeuralNetwork
format, fp16 math) or its own CPU path. `with_ane_only()` turns CoreML off
here (see fallback).

## Probe (S1, scratch crate, not in the repo)

`ort = { "=2.0.0-rc.9", features = ["coreml"] }` + `ort-sys` pin, jina fp32
(R) `model.onnx` from the main checkout's
`eval/embedding/work/models/jina-v2-base-code-fp32-r/`, one sequence of 16,
128 and 512 tokens, first call then median of 3, `deterministic_compute(true)`
as production. Scratch: `.../scratchpad/gm439/probe/` (`summary.txt`).

**Machine state: load average 37-101 during the runs** (other agents'
builds; `user` > `real` in every arm). Ratios are indicative only; the measure
slice must re-time.

- Build: links with the `coreml` feature alone; the binary then also links
  `CoreML.framework` (no build.rs change needed).
- Partitioning: `CoreMLExecutionProvider::GetCapability, number of partitions
  supported by CoreML: 97 number of nodes in the graph: 1012 number of nodes
  supported by CoreML: 795`. The word-embedding gather stays on CPU
  (`CoreML does not support input dim > 16384 ... {61056,768}`).
- Timing (ms, median of 3 / first call):

  | arm | commit | n=16 | n=128 | n=512 |
  |---|---|---|---|---|
  | CPU | 2842 | 54.5 / 46.7 | 245 / 261 | 1155 / 1063 |
  | CoreML (default: GPU+CPU) | 7074 | 346 / 1031 | 1166 / 2155 | 3029 / 7297 |
  | CoreML `with_cpu_only` | 3551 | 91 / 183 | 721 / 996 | 3255 / 5417 |

  CoreML is 2.6-6x slower per call, a new input length costs 1-7 s on its
  first call (a per-shape compile), and setup is +4.2 s.
- Accuracy vs CPU (last_hidden_state; pooled-vector cosine): CoreML GPU max
  abs diff 0.025-0.035, cosine 0.999991-0.999995; `with_cpu_only` max abs
  diff <1e-5. Not bit-identical either way.

## Fallback behaviour (`ort` rc.9, `execution_providers/mod.rs`
`apply_execution_providers`)

- Default `ExecutionProviderDispatch` is `fail_silently`: a registration
  error is logged (`tracing` warn) and the session **silently runs on CPU**
  ("No execution providers registered successfully. Falling back to CPU.").
  Probe: CUDA without its feature, exit 0, output bit-identical to CPU.
- `.error_on_failure()` turns a registration error into an `Err`. Probe: CUDA
  strict exits 1 with the "Cargo feature is not enabled" message.
- A second silent path that `error_on_failure` does **not** catch: CoreML
  registers, then claims zero nodes (`with_ane_only()` on a Mac without ANE:
  "does not have Apple Neural Engine. CoreML EP will not be used"; output
  bit-identical to CPU). Also partial: unsupported nodes run on CPU with no
  error. Only ORT's own log shows how much ran on the EP.

## Design

### Switch (harness only)

1. A cargo feature on `core`, off by default:
   `[features] ep-coreml = ["ort/coreml"]`. The shipped binary and CI builds
   do not enable it, so they do not link `CoreML.framework` and keep today's
   behaviour exactly. The measure slice builds
   `cargo build --release -p g-mesh --features ep-coreml` into its own
   target dir (`core`'s package is `g-mesh`).
2. An enum in `embedding/model.rs`: `ExecutionTarget { Cpu, CoreMl,
   CoreMlCpuOnly }`, `Cpu` default. Not part of `InferenceOptions`, so
   `InferenceOptions::from_env()` (read by the daemon's
   `EmbeddingPipeline::build`) can never pick it up: no env variable.
3. `EmbeddingModel::load_with_target(model_dir, spec, options, target)`
   builds the session as `load_with_options` does and, for a non-CPU target,
   adds `with_execution_providers([CoreMLExecutionProvider::default()
   [.with_cpu_only()].build().error_on_failure()])` before
   `with_deterministic_compute`. `load_with_options` becomes
   `load_with_target(.., ExecutionTarget::Cpu)`; its 4 production/test callers
   are unchanged. Without the feature, `CoreMl*` returns an error naming the
   feature (cfg-gated), never a silent CPU run.
4. `debug-embed-eval run --ep cpu|coreml|coreml-cpu-only` (`RunArgs`, clap
   `ValueEnum`, default `cpu`). Only `run_variant` reads it. `probe` and the
   reranker (`CrossEncoder::load`) are out of scope.

### Fallback

Fail loudly, never silently: `error_on_failure()` for a registration error,
and an explicit error when a CoreML target is asked for in a build without
the feature. The zero-node / partial-placement case cannot be caught through
`ort` rc.9's API, so the harness records the placement instead: initialise a
`tracing` subscriber for `ort=info` on stderr when `--ep` is not `cpu` (if
the binary has none for that target), and the measure slice greps the
`GetCapability` line from each run's stderr and reports partitions/nodes.
A run whose log lacks that line, or says "will not be used", is invalid.

### Run records

- `Manifest` gains `execution_provider: String` (serde default `"cpu"`, so
  old manifests read as CPU), and the reuse check (`same_nodes`) also
  requires an equal EP. Without this a CoreML run in the same `--out` would
  reuse CPU `vectors.bin` and time nothing.
- `Timings` gains `execution_provider` as well, so `timings.json` says which
  EP produced `embedNodesMs`.

## Edit map (1-based lines, branch `feat/GM-439-onnx-execution-providers`)

| File | Item | Lines | Change |
|---|---|---|---|
| `core/Cargo.toml` | `ort` dependency | 89-103 | add `[features] ep-coreml = ["ort/coreml"]`; amend the "No execution-provider feature" comment |
| `core/src/embedding/model.rs` | `InferenceOptions` / near it | 306-322 | new `ExecutionTarget` enum (not in `InferenceOptions`) |
| `core/src/embedding/model.rs` | `EmbeddingModel::load_with_options` | 604-672 | split into `load_with_target`; EP registration before `with_deterministic_compute` (665-669) |
| `core/src/cli/embed_eval.rs` | `RunArgs` | 109-127 | `--ep` |
| `core/src/cli/embed_eval.rs` | `Manifest` | 456-477 | `execution_provider` |
| `core/src/cli/embed_eval.rs` | `Timings` | 537-556 | `execution_provider` |
| `core/src/cli/embed_eval.rs` | `run_variant` | 765-796 | pass `args.ep` to `load_with_target` (line 784) |
| `core/src/cli/embed_eval.rs` | `run_corpus` manifest + reuse | 842-873 | fill the field; `same_nodes` compares it |
| `core/src/cli/embed_eval.rs` | `tests` | 1715- | Timings/Manifest fixtures get the new field |

Untouched: `embedding/pipeline.rs` `EmbeddingPipeline::build` (daemon, line
250), `cli/embed_eval/probe.rs` `run` (line 71), `embedding/rerank.rs`
`CrossEncoder::load`.

## Measure slice

Arms: `cpu`, `coreml` (GPU+CPU, the EP's real use), and `coreml-cpu-only` as
a cheap diagnostic of partition overhead (optional; drop it if the first two
already decide).

- Pass time: GM-398 D11 recipe per arm,
  `G_MESH_EMBEDDING_CACHE=off /usr/bin/time -lp g-mesh debug-embed-eval run
  --variant jina-v2-base-code-fp32-r --corpus g-mesh --embed-only --force
  --ep <arm> --out <OUT>/timed/<arm>/r<n>`, 3 rounds interleaved cpu, coreml,
  each gated by `measure_costs.sh`'s `wait_quiet` (load1 < 7). Record
  `uptime`, `user`/`sys`/`real`, max RSS, `load1` before/after, `loadMs`
  (setup) and `embedNodesMs` (pass). Report the median and the delta vs CPU.
  GM-398's CPU pass was ~749 s, so one round of both arms is ~25 min at
  parity, far longer if the probe's 3x holds. **Early stop**: if round 1's
  CoreML pass is > 1.5x the CPU pass on a quiet machine, record it and skip
  rounds 2-3 (the verdict cannot flip).
- Quality: one full run (not `--embed-only`) of `--ep coreml` over the six
  corpora, then `debug-embed-eval report` against the CPU R run for
  recall@10/MRR, and `debug-embed-eval compare --baseline <cpu run>
  --candidate <coreml run>` per corpus for per-node L2/cosine. "Unchanged"
  means: recall@10 and MRR equal to 3 decimals, and no ranking flips the
  report flags; vectors are not expected to be bit-identical (probe cosine
  ~0.99999). If the early stop fires, the quality run is limited to the
  g-mesh corpus (the timed corpus) since an EP that is slower will not ship.
- Placement: every CoreML run's stderr must hold the `GetCapability` line;
  report partitions and nodes.

## Must confirm

Verify (after the implement slice):
- A default build (`cargo build --release`) links no `CoreML.framework`
  (`otool -L`), and `debug-embed-eval run` without `--ep` writes
  `executionProvider: "cpu"` and vectors bit-identical to the base branch on
  one corpus (`compare` reports all bit-identical).
- `--ep coreml` on a default build fails with the feature-named error, exit
  non-zero (control: make it fall through to CPU and the test must fail).
- A CoreML run in the same `--out` as a CPU run does not reuse its
  `vectors.bin` (control: drop the EP from `same_nodes`).
- `InferenceOptions::from_env` has no EP input; `EmbeddingPipeline::build`
  diff is empty.

Measure:
- The `GetCapability` line is present in each CoreML run (97 partitions /
  795 of 1012 nodes in the probe).
- Load gating: no timed run with load1 >= 7; probe ratios were taken at load
  37-101 and are not results.
- The first-call-per-shape cost: report `embedNodesMs` as is (it includes
  per-length compiles, which production would pay too), and note node count
  vs distinct token lengths.

## Risks and trade-offs

- Benefit: answers the task with a switch that cannot reach users (feature
  off, no env var, daemon untouched).
- Risk: the feature adds a build configuration nobody builds in CI; it can
  rot. Acceptable for an eval-only path; drop the feature if CoreML loses.
- Risk: the probe already suggests CoreML is slower on this Intel Mac (97
  partitions, fp16 GPU, per-shape compiles). The measure slice may only
  confirm a negative; an Apple Silicon host with ANE is the case this task
  cannot measure here.
