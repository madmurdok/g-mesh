//! The ONNX embedding model itself: load it once, then embed text with it.
//!
//! # What runs
//!
//! [`EmbeddingModel`] pairs two files that always travel together:
//!
//! - `model.onnx` - the network, executed by ONNX Runtime through the `ort`
//!   crate, on the CPU, with no execution provider registered.
//! - `tokenizer.json` - the Hugging Face tokenizer that produced the token ids
//!   the network was trained on. Using any other tokenizer with these weights
//!   silently produces garbage vectors rather than an error, which is why the
//!   two are loaded from one directory as a unit.
//!
//! The default model is `jina-embeddings-v2-base-code` (the value
//! [`crate::config::EmbeddingConfig`] already defaults to): 12 layers, hidden
//! size 768, mean pooling, ALiBi positions so it accepts up to 8192 tokens
//! (though [`EmbeddingModel::load`] truncates to far fewer - see
//! [`DEFAULT_MAX_SEQUENCE_LENGTH`]). Nothing in this module is specific to it beyond the two shape checks -
//! swapping in another BERT-style ONNX encoder with `input_ids` /
//! `attention_mask` inputs and a `last_hidden_state` output means pointing
//! [`EmbeddingModel::load`] at a different directory, not editing code. What
//! *is* specific is the vector these weights produce, hence
//! `embeddingVersion` in the storage schema - see REQUIREMENTS.md's
//! "Инвалидация эмбеддингов при смене embedding-модели".
//!
//! # Where the weights come from
//!
//! They are not vendored - `model.onnx` alone is ~154 MiB, which has no
//! business in a git repo. This module *reads* an already-present model
//! directory and never downloads anything; fetching is a separate, explicit
//! step so that no g-mesh code path can quietly reach the network. For the
//! default model the files are, pinned to the revision this was verified
//! against:
//!
//! ```text
//! https://huggingface.co/jinaai/jina-embeddings-v2-base-code
//!   revision 516f4baf13dec4ddddda8631e019b5737c8bc250
//!   onnx/model_quantized.onnx -> <model dir>/model.onnx
//!   tokenizer.json            -> <model dir>/tokenizer.json
//! ```
//!
//! The int8 quantization, not the fp32 `onnx/model.onnx`
//! (`docs/adr/0011-embedding-model-int8.md`). It is written as `model.onnx`:
//! the loader's file name is the same for every encoder it loads (the
//! embedding eval harness included), so only the fetcher knows which export
//! of the repository it came from.
//!
//! `g-mesh model fetch` (see [`crate::cli::model`]) does exactly that, into the
//! directory [`resolve_model_dir`] resolves to - it is a user-typed command,
//! never something this module or the daemon triggers.
//! `core/scripts/fetch-embedding-model.sh` is the same download as a shell
//! one-liner, kept for a source checkout that has no built binary yet; both
//! share this module's directory resolution and file names, and a test in
//! `cli::model` fails if the script's pinned revision ever drifts from the
//! binary's. The model is Apache-2.0, so redistributing it with g-mesh later is
//! possible; that is a packaging decision, not this module's.
//!
//! # Determinism
//!
//! [`EmbeddingModel::embed`] is a pure function of its input for a given
//! loaded model: the same text always yields bit-identical floats. That is
//! load-bearing rather than incidental - stored vectors are only comparable to
//! query vectors if re-embedding is stable, and an index would otherwise drift
//! against itself between runs. It is upheld by pinning ONNX Runtime to
//! deterministic compute and by pooling in a fixed order (see
//! [`EmbeddingModel::embed`]).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use serde::{Deserialize, Serialize};
use tokenizers::tokenizer::Tokenizer;
use tokenizers::TruncationParams;

/// File names expected inside a model directory. Both are the names Hugging
/// Face uses, so a directory can be populated by copying files straight out of
/// a model repo without renaming (`onnx/model.onnx` flattened to
/// `model.onnx`).
///
/// `pub(crate)` because `cli::model` writes exactly these files: the fetcher
/// naming one of them differently would produce a directory this loader
/// rejects, so the two read the same constant rather than each spelling it
/// out.
pub(crate) const ONNX_FILE_NAME: &str = "model.onnx";
pub(crate) const TOKENIZER_FILE_NAME: &str = "tokenizer.json";

/// The default model's weights file inside its repository at
/// `cli::model::MODEL_REVISION`: the int8 export (ADR 0011). Written to
/// [`ONNX_FILE_NAME`] in the model directory.
pub(crate) const DEFAULT_ONNX_REMOTE_PATH: &str = "onnx/model_quantized.onnx";

/// Byte size of [`DEFAULT_ONNX_REMOTE_PATH`] at the pinned revision, from the
/// downloaded file (and Hugging Face's `x-linked-size` for it).
pub(crate) const DEFAULT_ONNX_SIZE: u64 = 161_895_621;

/// SHA-256 of [`DEFAULT_ONNX_REMOTE_PATH`] at the pinned revision, computed
/// from the downloaded file and matching Hugging Face's LFS `x-linked-etag`
/// for it. `g-mesh model fetch` verifies every download against it.
pub(crate) const DEFAULT_ONNX_SHA256: &str =
    "ed45870251c9f0cf656e78aab0d37a23489066df8a222bb1c8caf8a45f2cb16d";

/// Which weights of the default model its vectors are tagged with, as part of
/// [`embedding_version`]. Changed whenever the pinned weights change, so every
/// vector the old weights made is owed a re-embed: `embedding::backfill` and
/// `storage::language_swap` both re-embed a row whose `embeddingVersion` is
/// not the pipeline's. Vectors from the fp32 weights carry the bare model
/// name.
const DEFAULT_WEIGHTS_TAG: &str = "int8";

/// Which text form every model's vectors are tagged with, as part of
/// [`embedding_version`]: `embedding::text::text_to_embed`'s. Changed
/// whenever that function's output changes for any node, so every vector made
/// from the old text is owed a re-embed, under any model. Vectors from the
/// untrimmed doc comment carry no text tag. The embedding cache needs no
/// change with it: its key is the text itself, so the re-embed misses only
/// for the texts that changed.
pub(crate) const TEXT_FORM_TAG: &str = "structured";

/// The `embeddingVersion` a pipeline configured with `model_name` stores its
/// vectors under: the default model's name plus [`DEFAULT_WEIGHTS_TAG`], or
/// any other model's name as configured (its weights are not g-mesh's to
/// pin), then [`TEXT_FORM_TAG`].
pub(crate) fn embedding_version(model_name: &str) -> String {
    if model_name == crate::config::EmbeddingConfig::default().model {
        format!("{model_name}+{DEFAULT_WEIGHTS_TAG}+{TEXT_FORM_TAG}")
    } else {
        format!("{model_name}+{TEXT_FORM_TAG}")
    }
}

/// Whether `model_dir`'s weights are the default model's pinned ones, by size.
///
/// A stat, not a digest: the case it exists for is an fp32 `model.onnx`
/// (641,517,466 bytes) in the default model's directory, which size alone
/// tells apart, and the daemon asks this before every load. A missing
/// file is not this check's business: [`EmbeddingModel::load`] reports it.
/// Loading the old weights anyway would tag fp32 vectors with the int8
/// [`embedding_version`], and nothing would ever re-embed them.
pub(crate) fn check_default_weights(model_dir: &Path) -> Result<()> {
    let path = model_dir.join(ONNX_FILE_NAME);
    let Ok(meta) = std::fs::metadata(&path) else { return Ok(()) };
    if meta.len() != DEFAULT_ONNX_SIZE {
        bail!(
            "{} is {} bytes, not the {DEFAULT_ONNX_SIZE} bytes of the pinned weights (g-mesh switched \
             to the int8 model; this is likely the fp32 file it replaced).\n\
             Replace it: g-mesh model fetch --dir {}",
            path.display(),
            meta.len(),
            model_dir.display()
        );
    }
    Ok(())
}

/// ONNX graph input/output names. BERT-style encoders exported by
/// `transformers` use exactly these; they are addressed by name rather than by
/// position so that a model exporting extra inputs (`token_type_ids`) fails
/// loudly instead of being fed the wrong tensor.
const INPUT_IDS: &str = "input_ids";
const ATTENTION_MASK: &str = "attention_mask";
const LAST_HIDDEN_STATE: &str = "last_hidden_state";
/// Fed as all zeros, and only when [`EncoderSpec::token_type_ids`] says the
/// graph declares it; a graph that declares it without the spec saying so
/// still fails loudly at inference.
const TOKEN_TYPE_IDS: &str = "token_type_ids";

/// Width of a `jina-embeddings-v2-base-code` vector. Checked at inference time
/// rather than trusted, so pointing g-mesh at a differently-shaped model is a
/// clear error instead of a corrupt index.
pub const EMBEDDING_DIM: usize = 768;

/// Longest input the default model *can* accept (ALiBi positions, 8192
/// tokens). This is the architecture's limit, not what [`EmbeddingModel::load`]
/// truncates to - see [`DEFAULT_MAX_SEQUENCE_LENGTH`] for that, and for why
/// the two differ.
pub const MAX_SEQUENCE_LENGTH: usize = 8192;

/// What [`EmbeddingModel::load`] truncates every input to. Anything longer is
/// truncated by the tokenizer rather than rejected: an over-long docstring
/// should still produce a usable vector.
///
/// # GM-393: why not [`MAX_SEQUENCE_LENGTH`]
///
/// Self-attention's working memory grows with the *square* of the input
/// length, and ONNX Runtime's CPU arena keeps whatever the largest run
/// needed for the rest of the session's life. Measured on the default model
/// (macOS x86_64, one `embed` call per length, RSS after each; the loaded
/// model alone is ~1.45 GB of RSS, much of it the mapped weights):
///
/// | input tokens | RSS after | time |
/// |---:|---:|---:|
/// | 1024 | 1.46 GB | 1.7 s |
/// | 2048 | 2.09 GB | 4.5 s |
/// | 4096 | 5.09 GB | 18 s |
/// | 8192 | 14.9 GB | 65 s |
///
/// and it stayed at 14.9 GB for every short input after that. Doc comments
/// long enough to hit 8192 are not hypothetical: g-mesh's own module docs
/// reach 33k characters, and bulk-indexing this repository drove the daemon
/// to 15.3 GB max RSS (GM-393). Disabling the arena returns the memory
/// afterwards but not the peak (4.6 GB max RSS for one 4096-token run either
/// way), and makes every inference slower, so the only thing that actually
/// bounds the daemon is bounding the input.
///
/// 1024 is where that bound costs nothing measurable: a 1024-token run left
/// RSS within 10 MB of a 128-token one, while 2048 already added ~600 MB. It
/// is also twice the 512 tokens `jina-embeddings-v2` was pre-trained on, so
/// what gets cut is the tail of a very long doc comment, not its summary.
/// [`load_with_max_sequence_length`](EmbeddingModel::load_with_max_sequence_length)
/// is still there for a caller that deliberately wants more.
pub const DEFAULT_MAX_SEQUENCE_LENGTH: usize = 1024;

/// Overrides where [`default_model_dir`] looks. Exists so tests and
/// development checkouts can point at a model directory outside the user's
/// home without a config file; ordinary runs never set it.
pub const MODEL_DIR_ENV: &str = "G_MESH_MODEL_DIR";

/// How the encoder's hidden states become one vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Mean over the tokens the attention mask marks as real.
    Mean,
    /// The first position's hidden state (`[CLS]` / `<s>`).
    Cls,
}

/// What an encoder needs to be run correctly beyond its two files: how its
/// output is pooled, how wide its vectors are, how much input it is given,
/// and whether its graph takes `token_type_ids`.
///
/// [`EncoderSpec::production`] is what the daemon runs; any other value is
/// used only by the embedding eval (`cli::embed_eval`) to score candidate
/// models through this same loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderSpec {
    pub pooling: Pooling,
    /// Checked against the model's output shape on every call.
    pub dimension: usize,
    /// Tokenizer truncation length, special tokens included.
    pub max_sequence_length: usize,
    /// Feed an all-zero `token_type_ids` input (BERT-family exports).
    pub token_type_ids: bool,
}

impl EncoderSpec {
    /// The default model as the daemon runs it: mean pooling,
    /// [`EMBEDDING_DIM`] wide, truncated to [`DEFAULT_MAX_SEQUENCE_LENGTH`],
    /// no `token_type_ids`.
    pub const fn production() -> Self {
        Self {
            pooling: Pooling::Mean,
            dimension: EMBEDDING_DIM,
            max_sequence_length: DEFAULT_MAX_SEQUENCE_LENGTH,
            token_type_ids: false,
        }
    }
}

/// Environment variables that tune the ONNX session and the batching of
/// [`EmbeddingModel::embed_batch`]. Absent or empty means the default; an
/// invalid value logs one line and falls back to the default.
pub const INTRA_THREADS_ENV: &str = "G_MESH_EMBED_INTRA_THREADS";
pub const INTER_THREADS_ENV: &str = "G_MESH_EMBED_INTER_THREADS";
pub const OPT_LEVEL_ENV: &str = "G_MESH_EMBED_OPT_LEVEL";
pub const SPIN_ENV: &str = "G_MESH_EMBED_SPIN";
pub const BATCH_ENV: &str = "G_MESH_EMBED_BATCH";
pub const TOKEN_BUDGET_ENV: &str = "G_MESH_EMBED_TOKEN_BUDGET";
pub const BUCKET_ENV: &str = "G_MESH_EMBED_BUCKET";

/// Default cap on padded tokens (`texts x longest text`) per batched call:
/// eight texts at [`DEFAULT_MAX_SEQUENCE_LENGTH`].
pub const DEFAULT_TOKEN_BUDGET: usize = 8192;

/// ONNX Runtime graph optimisation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OptLevel {
    Disable,
    Basic,
    Extended,
    All,
}

impl From<OptLevel> for GraphOptimizationLevel {
    fn from(level: OptLevel) -> Self {
        match level {
            OptLevel::Disable => GraphOptimizationLevel::Disable,
            OptLevel::Basic => GraphOptimizationLevel::Level1,
            OptLevel::Extended => GraphOptimizationLevel::Level2,
            OptLevel::All => GraphOptimizationLevel::Level3,
        }
    }
}

/// How the session is built and how [`EmbeddingModel::embed_batch`] groups
/// texts. [`InferenceOptions::default`] is the session and calling pattern
/// [`EmbeddingModel::load`] has always used: every `None` leaves ONNX
/// Runtime's own default in place, and `batch_size == 1` makes
/// `embed_batch` a plain loop over [`EmbeddingModel::embed`].
///
/// Not part of the embedding cache key or of `embedding_version`: a setting
/// that changes vectors beyond rounding would mix old and new vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceOptions {
    /// Intra-op threads; `None` = ORT default.
    pub intra_threads: Option<usize>,
    /// Inter-op threads; `Some` also turns on parallel execution, without
    /// which ORT ignores the count.
    pub inter_threads: Option<usize>,
    pub optimization: OptLevel,
    /// Whether intra-op workers spin while idle; `None` = ORT default.
    pub intra_spinning: Option<bool>,
    /// Most texts per batched call; 1 = one `embed` call per text.
    pub batch_size: usize,
    /// Most padded tokens per batched call. A text longer than this still
    /// runs, alone.
    pub token_budget: usize,
    /// Sort texts by token length before forming batches.
    pub bucket_by_length: bool,
}

impl Default for InferenceOptions {
    fn default() -> Self {
        Self {
            intra_threads: None,
            inter_threads: None,
            optimization: OptLevel::All,
            intra_spinning: None,
            batch_size: 1,
            token_budget: DEFAULT_TOKEN_BUDGET,
            bucket_by_length: false,
        }
    }
}

impl InferenceOptions {
    /// Reads the `G_MESH_EMBED_*` variables of the process environment.
    pub fn from_env() -> Self {
        Self::from_env_with(|name| std::env::var(name).ok())
    }

    /// [`from_env`](Self::from_env) over any variable lookup. Never fails:
    /// an unparsable value is logged and the default kept.
    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Self {
        let mut options = Self::default();
        let value = |name: &str| get(name).map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty());
        let invalid = |name: &str, raw: &str| {
            crate::log_line!("g-mesh: ignoring {name}={raw:?}: not a valid value, using the default");
        };
        let positive = |raw: &str| raw.parse::<usize>().ok().filter(|&n| n >= 1);
        let flag = |raw: &str| match raw {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };

        if let Some(raw) = value(INTRA_THREADS_ENV) {
            match raw.as_str() {
                "0" => {}
                "physical" => options.intra_threads = Some(physical_cores()),
                "logical" => options.intra_threads = Some(logical_cores()),
                other => match positive(other) {
                    Some(n) => options.intra_threads = Some(n),
                    None => invalid(INTRA_THREADS_ENV, &raw),
                },
            }
        }
        if let Some(raw) = value(INTER_THREADS_ENV) {
            match positive(&raw) {
                Some(n) => options.inter_threads = Some(n),
                None => invalid(INTER_THREADS_ENV, &raw),
            }
        }
        if let Some(raw) = value(OPT_LEVEL_ENV) {
            match raw.as_str() {
                "disable" => options.optimization = OptLevel::Disable,
                "basic" => options.optimization = OptLevel::Basic,
                "extended" => options.optimization = OptLevel::Extended,
                "all" => options.optimization = OptLevel::All,
                _ => invalid(OPT_LEVEL_ENV, &raw),
            }
        }
        if let Some(raw) = value(SPIN_ENV) {
            match flag(&raw) {
                Some(spin) => options.intra_spinning = Some(spin),
                None => invalid(SPIN_ENV, &raw),
            }
        }
        if let Some(raw) = value(BATCH_ENV) {
            match positive(&raw) {
                Some(n) => options.batch_size = n,
                None => invalid(BATCH_ENV, &raw),
            }
        }
        if let Some(raw) = value(TOKEN_BUDGET_ENV) {
            match positive(&raw) {
                Some(n) => options.token_budget = n,
                None => invalid(TOKEN_BUDGET_ENV, &raw),
            }
        }
        if let Some(raw) = value(BUCKET_ENV) {
            match flag(&raw) {
                Some(bucket) => options.bucket_by_length = bucket,
                None => invalid(BUCKET_ENV, &raw),
            }
        }
        options
    }
}

/// Logical CPUs (hardware threads) available to this process.
pub fn logical_cores() -> usize {
    std::thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(1)
}

/// Physical CPU cores: `hw.physicalcpu` on macOS, distinct
/// `(physical id, core id)` pairs of `/proc/cpuinfo` on Linux, and
/// [`logical_cores`] wherever neither answers. Never more than
/// [`logical_cores`]: the OS counts are the host's and ignore the affinity
/// mask and cgroup quota `logical_cores` honours.
pub fn physical_cores() -> usize {
    let logical = logical_cores();
    physical_cores_from_os().filter(|&n| n >= 1).map_or(logical, |physical| physical.min(logical))
}

#[cfg(target_os = "macos")]
fn physical_cores_from_os() -> Option<usize> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated, `value` is a c_int and `size`
    // holds its size, as sysctlbyname requires for an integer OID.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.physicalcpu".as_ptr(),
            (&raw mut value).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then(|| usize::try_from(value).ok()).flatten()
}

#[cfg(target_os = "linux")]
fn physical_cores_from_os() -> Option<usize> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    Some(physical_cores_in_cpuinfo(&cpuinfo)).filter(|&n| n >= 1)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn physical_cores_from_os() -> Option<usize> {
    None
}

/// Distinct `(physical id, core id)` pairs in `/proc/cpuinfo` text; 0 when
/// the file names no core ids.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn physical_cores_in_cpuinfo(cpuinfo: &str) -> usize {
    let mut cores = std::collections::BTreeSet::new();
    let mut package = String::new();
    for line in cpuinfo.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        match key.trim() {
            "physical id" => package = value.trim().to_string(),
            "core id" => {
                cores.insert((package.clone(), value.trim().to_string()));
            }
            _ => {}
        }
    }
    cores.len()
}

/// What [`EmbeddingModel::embed_batch_with_stats`] ran: the number of
/// encoder calls, and for the batched path the real and padded token counts
/// (`None` on the one-call-per-text path, which pads nothing).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchStats {
    pub batches: usize,
    pub real_tokens: Option<usize>,
    pub padded_tokens: Option<usize>,
}

impl BatchStats {
    pub fn add(&mut self, other: BatchStats) {
        self.batches += other.batches;
        self.real_tokens = sum_counts(self.real_tokens, other.real_tokens);
        self.padded_tokens = sum_counts(self.padded_tokens, other.padded_tokens);
    }
}

fn sum_counts(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

/// Groups text indices into batches for [`EmbeddingModel::embed_batch`].
///
/// Every index in `0..lengths.len()` appears exactly once. With `bucket`
/// the indices are taken in order of `(length, index)`, otherwise in input
/// order. A batch grows while it holds fewer than `batch_size` texts and
/// `(count + 1) x longest` stays within `token_budget`; a text longer than
/// the budget forms a batch of its own.
pub(crate) fn plan_batches(
    lengths: &[usize],
    batch_size: usize,
    token_budget: usize,
    bucket: bool,
) -> Vec<Vec<usize>> {
    let batch_size = batch_size.max(1);
    let mut order: Vec<usize> = (0..lengths.len()).collect();
    if bucket {
        order.sort_by_key(|&i| (lengths[i], i));
    }
    let mut batches = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut longest = 0usize;
    for index in order {
        let length = lengths[index];
        let widened = longest.max(length);
        if !current.is_empty()
            && (current.len() >= batch_size || (current.len() + 1).saturating_mul(widened) > token_budget)
        {
            batches.push(std::mem::take(&mut current));
            longest = 0;
        }
        longest = longest.max(length);
        current.push(index);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// A loaded ONNX encoder plus its tokenizer.
///
/// Loading is expensive (hundreds of MiB read and graph-optimized, ~1s), so a
/// caller is expected to build one of these once and share it. `ort`'s
/// `Session` is `Send + Sync` and runs inference behind `&self`, so this type
/// is too - no interior mutability or pooling needed for the daemon to embed
/// from several request handlers at once.
pub struct EmbeddingModel {
    session: Session,
    tokenizer: Tokenizer,
    spec: EncoderSpec,
    options: InferenceOptions,
    /// Token id written into the padded tail of a batched input; masked out.
    pad_id: i64,
}

/// Hand-written rather than derived: the fields are a loaded ONNX graph and a
/// full vocabulary, neither of which anyone wants rendered into a log line or
/// a test failure message.
impl std::fmt::Debug for EmbeddingModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingModel").finish_non_exhaustive()
    }
}

impl EmbeddingModel {
    /// Loads `model.onnx` and `tokenizer.json` from `model_dir`.
    ///
    /// Fails with an actionable error if either file is missing - a missing
    /// model is the expected state of a fresh machine, not a bug, so the
    /// message names the file and points at the fetch script rather than
    /// surfacing a bare `NotFound`.
    ///
    /// Truncates to [`DEFAULT_MAX_SEQUENCE_LENGTH`], not to the model's full
    /// [`MAX_SEQUENCE_LENGTH`] - see the former for the memory cost that
    /// decides it.
    pub fn load(model_dir: &Path) -> Result<Self> {
        Self::load_with_max_sequence_length(model_dir, DEFAULT_MAX_SEQUENCE_LENGTH)
    }

    /// [`load`](Self::load) with a different truncation limit. A shorter one
    /// caps the worst case an accidentally huge input can cost even further;
    /// a longer one buys more context per input at a cost that grows with its
    /// square (see [`DEFAULT_MAX_SEQUENCE_LENGTH`]).
    ///
    /// Values above [`MAX_SEQUENCE_LENGTH`] are not rejected here - that
    /// constant describes the current default model, and this loader is
    /// otherwise model-agnostic - but going past a model's trained context
    /// degrades quality rather than erroring, so raise it only knowingly.
    pub fn load_with_max_sequence_length(model_dir: &Path, max_sequence_length: usize) -> Result<Self> {
        Self::load_with_spec(model_dir, EncoderSpec { max_sequence_length, ..EncoderSpec::production() })
    }

    /// [`load`](Self::load) for an encoder described by `spec` rather than
    /// the default model's [`EncoderSpec::production`].
    pub fn load_with_spec(model_dir: &Path, spec: EncoderSpec) -> Result<Self> {
        Self::load_with_options(model_dir, spec, &InferenceOptions::default())
    }

    /// [`load_with_spec`](Self::load_with_spec) with the session built and
    /// batches formed as `options` says. Each option reaches the session
    /// builder only when it is set, so the default options build exactly the
    /// session `load` always has.
    pub fn load_with_options(
        model_dir: &Path,
        spec: EncoderSpec,
        options: &InferenceOptions,
    ) -> Result<Self> {
        let max_sequence_length = spec.max_sequence_length;
        if max_sequence_length == 0 {
            bail!("max_sequence_length must be at least 1");
        }
        if spec.dimension == 0 {
            bail!("the encoder's dimension must be at least 1");
        }

        let onnx_path = model_dir.join(ONNX_FILE_NAME);
        let tokenizer_path = model_dir.join(TOKENIZER_FILE_NAME);
        for path in [&onnx_path, &tokenizer_path] {
            if !path.exists() {
                bail!(
                    "embedding model file {} not found.\n\
                     Fetch the model first: g-mesh model fetch --dir {}",
                    path.display(),
                    model_dir.display()
                );
            }
        }

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            // `tokenizers` errors are boxed `dyn Error` and are not `'static`
            // in a way `anyhow` accepts directly, so they are rendered here.
            .map_err(|err| anyhow!("failed to load tokenizer {}: {err}", tokenizer_path.display()))?;
        // Truncate in the tokenizer rather than by chopping the id vector:
        // the tokenizer knows to keep the model's special tokens (`<s>` /
        // `</s>`) around the truncated span, which a naive `Vec::truncate`
        // would strip off the end.
        tokenizer
            .with_truncation(Some(TruncationParams { max_length: max_sequence_length, ..Default::default() }))
            .map_err(|err| anyhow!("failed to configure tokenizer truncation: {err}"))?;
        // Some exported tokenizer.json files carry a padding config; a batch
        // of one needs none, and pooling must see only the text's own tokens.
        tokenizer.with_padding(None);
        let pad_id = tokenizer.token_to_id("<pad>").map_or(0, i64::from);

        let mut builder = Session::builder()
            .context("failed to create ONNX session builder")?
            .with_optimization_level(options.optimization.into())
            .context("failed to set ONNX graph optimization level")?;
        if let Some(threads) = options.intra_threads {
            builder = builder.with_intra_threads(threads).context("failed to set ONNX intra-op threads")?;
        }
        if let Some(threads) = options.inter_threads {
            builder = builder
                .with_parallel_execution(true)
                .context("failed to enable ONNX parallel execution")?
                .with_inter_threads(threads)
                .context("failed to set ONNX inter-op threads")?;
        }
        if let Some(spin) = options.intra_spinning {
            builder = builder.with_intra_op_spinning(spin).context("failed to set ONNX intra-op spinning")?;
        }
        let session = builder
            // Fixes the floating-point reduction order inside ONNX Runtime, so
            // two runs of the same input cannot differ in the last bits.
            .with_deterministic_compute(true)
            .context("failed to enable deterministic compute")?
            .commit_from_file(&onnx_path)
            .with_context(|| format!("failed to load ONNX model {}", onnx_path.display()))?;

        Ok(Self { session, tokenizer, spec, options: *options, pad_id })
    }

    /// The options this model was loaded with.
    pub fn options(&self) -> InferenceOptions {
        self.options
    }

    /// The spec this model was loaded with.
    pub fn spec(&self) -> EncoderSpec {
        self.spec
    }

    /// Embeds `text` into a unit-length vector of the spec's dimension
    /// ([`EMBEDDING_DIM`] for the default model).
    ///
    /// Three steps, all of them fixed-order and therefore deterministic:
    ///
    /// 1. Tokenize (truncating to the configured maximum).
    /// 2. Run the encoder, giving one hidden vector per token.
    /// 3. Pool those vectors as the spec says (the default model: mean over
    ///    the real, non-padding tokens), then L2-normalize.
    ///
    /// Mean pooling is not a choice - it is what the default model was trained with
    /// (`1_Pooling/config.json`: `pooling_mode_mean_tokens`), and taking the
    /// `<s>` vector instead would produce vectors that simply do not match the
    /// training objective.
    ///
    /// Normalization *is* a choice: the upstream sentence-transformers config
    /// has no `Normalize` module, so raw vectors have arbitrary magnitude.
    /// Returning unit vectors makes cosine similarity equal to a plain dot
    /// product, which is what the vector store and `search_code` want, and
    /// costs nothing because cosine similarity ignores magnitude anyway.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding =
            self.tokenizer.encode(text, true).map_err(|err| anyhow!("failed to tokenize input: {err}"))?;

        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| i64::from(id)).collect();
        let mask: Vec<i64> = encoding.get_attention_mask().iter().map(|&m| i64::from(m)).collect();
        if ids.is_empty() {
            // Only reachable with a tokenizer whose post-processor adds no
            // special tokens; for this model even "" yields `<s></s>`.
            bail!("input tokenized to zero tokens, nothing to embed");
        }
        debug_assert_eq!(ids.len(), mask.len());

        let sequence_length = ids.len();
        // Batch of one, unpadded; `embed_batch` is the padded path.
        let input_shape = [1usize, sequence_length];
        let input_ids = Tensor::from_array((input_shape, ids)).context("failed to build input_ids tensor")?;
        let attention_mask = Tensor::from_array((input_shape, mask.clone()))
            .context("failed to build attention_mask tensor")?;

        let outputs = if self.spec.token_type_ids {
            let token_type_ids = Tensor::from_array((input_shape, vec![0i64; sequence_length]))
                .context("failed to build token_type_ids tensor")?;
            self.session.run(ort::inputs![
                INPUT_IDS => input_ids,
                ATTENTION_MASK => attention_mask,
                TOKEN_TYPE_IDS => token_type_ids
            ]?)
        } else {
            self.session.run(ort::inputs![INPUT_IDS => input_ids, ATTENTION_MASK => attention_mask]?)
        }
        .context("ONNX inference failed")?;
        let hidden_state = outputs
            .get(LAST_HIDDEN_STATE)
            .ok_or_else(|| anyhow!("model produced no `{LAST_HIDDEN_STATE}` output"))?;
        let (shape, values) = hidden_state
            .try_extract_raw_tensor::<f32>()
            .context("failed to read `last_hidden_state` as f32")?;

        pool(self.spec.pooling, self.spec.dimension, shape, values, &mask, sequence_length)
    }

    /// Embeds every text, result `i` belonging to `texts[i]`.
    ///
    /// With `batch_size == 1` (the default) this is exactly one
    /// [`embed`](Self::embed) call per text, so its vectors are bit-identical
    /// to `embed`'s.
    pub fn embed_batch(&self, texts: &[&str]) -> Vec<Result<Vec<f32>>> {
        self.embed_batch_with_stats(texts).0
    }

    /// [`embed_batch`](Self::embed_batch), also reporting what ran.
    ///
    /// With `batch_size > 1` the texts are tokenized, grouped by
    /// [`plan_batches`], each group padded to its longest text (padding is
    /// masked out) and run as one `[B, L]` call. Results are written back by
    /// original index, so the grouping never reorders the output. A group
    /// whose inference fails is retried one text at a time with `embed`, so
    /// one bad text fails alone; a group of one is a plain `embed` call.
    pub fn embed_batch_with_stats(&self, texts: &[&str]) -> (Vec<Result<Vec<f32>>>, BatchStats) {
        if self.options.batch_size <= 1 {
            let results = texts.iter().map(|text| self.embed(text)).collect();
            return (results, BatchStats { batches: texts.len(), ..BatchStats::default() });
        }

        let mut out: Vec<Option<Result<Vec<f32>>>> = (0..texts.len()).map(|_| None).collect();
        let mut stats = BatchStats { batches: 0, real_tokens: Some(0), padded_tokens: Some(0) };
        // (original index, ids, mask) of every text that tokenized.
        let mut encoded: Vec<(usize, Vec<i64>, Vec<i64>)> = Vec::with_capacity(texts.len());
        for (index, text) in texts.iter().enumerate() {
            match self.tokenizer.encode(*text, true) {
                Ok(encoding) if !encoding.get_ids().is_empty() => encoded.push((
                    index,
                    encoding.get_ids().iter().map(|&id| i64::from(id)).collect(),
                    encoding.get_attention_mask().iter().map(|&m| i64::from(m)).collect(),
                )),
                // `embed` reports the same failure for this text.
                _ => out[index] = Some(self.embed(text)),
            }
        }

        let lengths: Vec<usize> = encoded.iter().map(|(_, ids, _)| ids.len()).collect();
        let plan = plan_batches(
            &lengths,
            self.options.batch_size,
            self.options.token_budget,
            self.options.bucket_by_length,
        );
        for batch in plan {
            stats.batches += 1;
            let longest = batch.iter().map(|&b| lengths[b]).max().unwrap_or(0);
            stats.real_tokens =
                stats.real_tokens.map(|n| n + batch.iter().map(|&b| lengths[b]).sum::<usize>());
            stats.padded_tokens = stats.padded_tokens.map(|n| n + batch.len() * longest);
            if let [only] = batch.as_slice() {
                let index = encoded[*only].0;
                out[index] = Some(self.embed(texts[index]));
                continue;
            }
            let rows: Vec<(&[i64], &[i64])> =
                batch.iter().map(|&b| (encoded[b].1.as_slice(), encoded[b].2.as_slice())).collect();
            match self.run_padded(&rows, longest) {
                Ok(vectors) => {
                    for (&b, vector) in batch.iter().zip(vectors) {
                        out[encoded[b].0] = Some(vector);
                    }
                }
                Err(_) => {
                    for &b in &batch {
                        let index = encoded[b].0;
                        out[index] = Some(self.embed(texts[index]));
                    }
                }
            }
        }

        let results = out
            .into_iter()
            .map(|slot| slot.unwrap_or_else(|| Err(anyhow!("text was not embedded"))))
            .collect();
        (results, stats)
    }

    /// Runs `rows` (ids and mask per text) as one `[B, longest]` call, each
    /// row padded with the pad id under mask 0, and pools each row. The outer
    /// error is the call failing; an inner one is that row's pooling failing.
    fn run_padded(&self, rows: &[(&[i64], &[i64])], longest: usize) -> Result<Vec<Result<Vec<f32>>>> {
        let batch = rows.len();
        let mut ids = Vec::with_capacity(batch * longest);
        let mut mask = Vec::with_capacity(batch * longest);
        for (row_ids, row_mask) in rows {
            ids.extend_from_slice(row_ids);
            ids.resize(ids.len() + longest - row_ids.len(), self.pad_id);
            mask.extend_from_slice(row_mask);
            mask.resize(mask.len() + longest - row_mask.len(), 0);
        }
        let input_shape = [batch, longest];
        let input_ids = Tensor::from_array((input_shape, ids)).context("failed to build input_ids tensor")?;
        let attention_mask = Tensor::from_array((input_shape, mask.clone()))
            .context("failed to build attention_mask tensor")?;
        let outputs = if self.spec.token_type_ids {
            let token_type_ids = Tensor::from_array((input_shape, vec![0i64; batch * longest]))
                .context("failed to build token_type_ids tensor")?;
            self.session.run(ort::inputs![
                INPUT_IDS => input_ids,
                ATTENTION_MASK => attention_mask,
                TOKEN_TYPE_IDS => token_type_ids
            ]?)
        } else {
            self.session.run(ort::inputs![INPUT_IDS => input_ids, ATTENTION_MASK => attention_mask]?)
        }
        .context("ONNX inference failed")?;
        let hidden_state = outputs
            .get(LAST_HIDDEN_STATE)
            .ok_or_else(|| anyhow!("model produced no `{LAST_HIDDEN_STATE}` output"))?;
        let (shape, values) = hidden_state
            .try_extract_raw_tensor::<f32>()
            .context("failed to read `last_hidden_state` as f32")?;
        Ok((0..batch)
            .map(|row| {
                let row_mask = &mask[row * longest..(row + 1) * longest];
                pool_row(self.spec.pooling, self.spec.dimension, shape, values, row_mask, row, longest)
            })
            .collect())
    }
}

/// Pools `[1, sequence_length, dimension]` hidden states into one vector
/// (mean over the tokens `mask` marks as real, or the first position), then
/// L2-normalizes.
///
/// Split out from [`EmbeddingModel::embed`] so the arithmetic that decides
/// what a vector *means* can be tested without 154 MiB of weights on disk.
fn pool(
    pooling: Pooling,
    dimension: usize,
    shape: &[i64],
    values: &[f32],
    mask: &[i64],
    sequence_length: usize,
) -> Result<Vec<f32>> {
    if shape != [1, sequence_length as i64, dimension as i64] {
        bail!(
            "unexpected `{LAST_HIDDEN_STATE}` shape {shape:?}; expected [1, {sequence_length}, {dimension}]. \
             The model directory is probably not a {dimension}-dimensional BERT-style encoder."
        );
    }
    pool_checked(pooling, dimension, values, mask, sequence_length)
}

/// [`pool`] for row `row` of padded `[B, sequence_length, dimension]` hidden
/// states, with `mask` that row's mask (padding marked 0). The same
/// arithmetic as `pool` over that row's slice.
pub(crate) fn pool_row(
    pooling: Pooling,
    dimension: usize,
    shape: &[i64],
    values: &[f32],
    mask: &[i64],
    row: usize,
    sequence_length: usize,
) -> Result<Vec<f32>> {
    let rows = match shape {
        [rows, length, width] if *length == sequence_length as i64 && *width == dimension as i64 => *rows,
        _ => bail!(
            "unexpected `{LAST_HIDDEN_STATE}` shape {shape:?}; expected [B, {sequence_length}, {dimension}]. \
             The model directory is probably not a {dimension}-dimensional BERT-style encoder."
        ),
    };
    if row as i64 >= rows {
        bail!("row {row} is outside the `{LAST_HIDDEN_STATE}` batch of {rows}");
    }
    let width = sequence_length * dimension;
    let Some(row_values) = values.get(row * width..(row + 1) * width) else {
        bail!("`{LAST_HIDDEN_STATE}` holds {} values, too few for shape {shape:?}", values.len());
    };
    pool_checked(pooling, dimension, row_values, mask, sequence_length)
}

/// Pooling and normalisation of one text's `[sequence_length, dimension]`
/// hidden states, already shape-checked.
fn pool_checked(
    pooling: Pooling,
    dimension: usize,
    values: &[f32],
    mask: &[i64],
    sequence_length: usize,
) -> Result<Vec<f32>> {
    let mut pooled = vec![0f32; dimension];
    match pooling {
        Pooling::Mean => {
            let mut token_count = 0f32;
            // Iterating positions in order (rather than, say, in parallel) is
            // what keeps float addition associative-order-stable across runs.
            for (position, &m) in mask.iter().enumerate().take(sequence_length) {
                if m == 0 {
                    continue;
                }
                token_count += 1.0;
                let row = &values[position * dimension..(position + 1) * dimension];
                for (slot, &value) in pooled.iter_mut().zip(row) {
                    *slot += value;
                }
            }
            if token_count == 0.0 {
                bail!("attention mask marked every token as padding, nothing to pool");
            }
            for slot in &mut pooled {
                *slot /= token_count;
            }
        }
        Pooling::Cls => pooled.copy_from_slice(&values[..dimension]),
    }

    let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for slot in &mut pooled {
            *slot /= norm;
        }
    }
    Ok(pooled)
}

/// Cosine similarity of two embeddings, in `[-1, 1]`.
///
/// Returns `0.0` for a zero-length vector rather than `NaN`: callers rank by
/// this value, and a `NaN` would poison a sort rather than simply sorting
/// last.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "cosine similarity of differently-sized vectors");
    let mut dot = 0f32;
    let mut norm_a = 0f32;
    let mut norm_b = 0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Where the weights for `model_name` are expected to live:
/// `~/.g-mesh/models/<model_name>/`, or `$G_MESH_MODEL_DIR` if set.
///
/// `~/.g-mesh/` is already where g-mesh keeps everything else it owns (indexes
/// and config), and models are per-machine rather than per-project - the same
/// 154 MiB should not be downloaded once per indexed repository - so they sit
/// beside `projects/` rather than inside it.
///
/// Deliberately anchored to the real home even when `G_MESH_HOME`
/// ([`crate::paths::g_mesh_home`]) points g-mesh's *state* elsewhere: that
/// override exists so a test run stops writing project directories into the
/// developer's home, and weights are an immutable per-machine cache rather
/// than per-run state. Moving them with it would turn every isolated run into
/// a 154 MiB download. Use `G_MESH_MODEL_DIR` to move them on purpose.
pub fn default_model_dir(model_name: &str) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(MODEL_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().context("could not resolve home directory")?;
    Ok(home.join(".g-mesh").join("models").join(model_name))
}

/// The full resolution order for a model directory: an explicitly given path,
/// else [`default_model_dir`] (`$G_MESH_MODEL_DIR`, else
/// `~/.g-mesh/models/<model_name>`).
///
/// Lives here, beside the loader that *reads* those directories, because the
/// writer (`g-mesh model fetch`, see [`crate::cli::model`]) and the reader
/// disagreeing about the location is the one failure neither side can detect:
/// the fetcher would report success and the loader would still report a
/// missing model. Only one of them may own the answer, and it is this
/// function.
pub fn resolve_model_dir(explicit: Option<&Path>, model_name: &str) -> Result<PathBuf> {
    match explicit {
        Some(dir) => Ok(dir.to_path_buf()),
        None => default_model_dir(model_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Name of the model the tests below expect in [`default_model_dir`].
    /// Deliberately the same string [`crate::config::EmbeddingConfig`]
    /// defaults to - if that default ever changes, these tests should start
    /// looking somewhere else too.
    fn test_model_dir() -> PathBuf {
        default_model_dir(&crate::config::EmbeddingConfig::default().model).unwrap()
    }

    fn load_test_model() -> EmbeddingModel {
        let dir = test_model_dir();
        EmbeddingModel::load(&dir)
            .unwrap_or_else(|err| panic!("could not load the real model from {}: {err:#}", dir.display()))
    }

    // -----------------------------------------------------------------------
    // Tests that need no weights
    // -----------------------------------------------------------------------

    #[test]
    fn loading_a_directory_without_weights_says_which_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let err = EmbeddingModel::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("model.onnx"), "{err}");
        assert!(err.contains("g-mesh model fetch"), "{err}");
    }

    #[test]
    fn the_default_model_directory_lives_under_the_g_mesh_home() {
        // Only meaningful when the override is unset, which is the normal case
        // and the only one the default path claims to describe.
        if std::env::var_os(MODEL_DIR_ENV).is_some() {
            return;
        }
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            default_model_dir("some-model").unwrap(),
            home.join(".g-mesh").join("models").join("some-model")
        );
    }

    /// An explicit directory wins over both the env override and the home
    /// default - the `--dir` argument of `g-mesh model fetch` relies on this,
    /// and so does anyone who set `G_MESH_MODEL_DIR` and then asked for a
    /// different directory once.
    #[test]
    fn an_explicit_directory_wins_over_the_env_override_and_the_default() {
        let explicit = Path::new("/somewhere/else");
        assert_eq!(resolve_model_dir(Some(explicit), "some-model").unwrap(), explicit);
        assert_eq!(resolve_model_dir(None, "some-model").unwrap(), default_model_dir("some-model").unwrap());
    }

    #[test]
    fn mean_pooling_ignores_masked_out_positions() {
        // Two positions, second one padding: the pooled vector must be the
        // first row's direction, not the average of both.
        let mut values = vec![0f32; 2 * EMBEDDING_DIM];
        values[0] = 3.0;
        values[EMBEDDING_DIM] = 100.0;
        values[EMBEDDING_DIM + 1] = 100.0;

        let pooled =
            pool(Pooling::Mean, EMBEDDING_DIM, &[1, 2, EMBEDDING_DIM as i64], &values, &[1, 0], 2).unwrap();

        assert!((pooled[0] - 1.0).abs() < 1e-6, "{}", pooled[0]);
        assert!(pooled[1].abs() < 1e-6, "{}", pooled[1]);
    }

    #[test]
    fn mean_pooling_returns_a_unit_vector() {
        let mut values = vec![0f32; EMBEDDING_DIM];
        values[0] = 2.0;
        values[1] = -3.0;
        values[2] = 6.0;

        let pooled =
            pool(Pooling::Mean, EMBEDDING_DIM, &[1, 1, EMBEDDING_DIM as i64], &values, &[1], 1).unwrap();

        let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "{norm}");
        // 2/7, -3/7, 6/7
        assert!((pooled[0] - 2.0 / 7.0).abs() < 1e-6, "{}", pooled[0]);
    }

    #[test]
    fn a_wrongly_shaped_model_output_is_rejected_rather_than_reinterpreted() {
        let values = vec![0f32; 16];
        let err = pool(Pooling::Mean, EMBEDDING_DIM, &[1, 1, 16], &values, &[1], 1).unwrap_err().to_string();
        assert!(err.contains("unexpected"), "{err}");
    }

    /// CLS pooling takes position 0 even when later positions are real
    /// tokens, and still returns a unit vector.
    #[test]
    fn cls_pooling_takes_the_first_position_only() {
        let dimension = 4;
        let values = vec![3.0, 0.0, 4.0, 0.0, /* second token */ 100.0, 100.0, 100.0, 100.0];

        let pooled = pool(Pooling::Cls, dimension, &[1, 2, dimension as i64], &values, &[1, 1], 2).unwrap();

        assert_eq!(pooled.len(), dimension);
        assert!((pooled[0] - 0.6).abs() < 1e-6, "{}", pooled[0]);
        assert!(pooled[1].abs() < 1e-6, "{}", pooled[1]);
        assert!((pooled[2] - 0.8).abs() < 1e-6, "{}", pooled[2]);
    }

    /// The width is the spec's, not the default model's: a 384-wide output
    /// passes a 384 spec and fails the production one.
    #[test]
    fn pooling_checks_the_declared_dimension() {
        let values = vec![1.0f32; 384];
        assert_eq!(pool(Pooling::Mean, 384, &[1, 1, 384], &values, &[1], 1).unwrap().len(), 384);
        assert!(pool(Pooling::Mean, EMBEDDING_DIM, &[1, 1, 384], &values, &[1], 1).is_err());
    }

    /// `load` and `load_with_max_sequence_length` run the default model
    /// exactly as before the spec existed.
    #[test]
    fn the_production_spec_is_mean_pooling_768_wide_truncated_at_1024() {
        assert_eq!(
            EncoderSpec::production(),
            EncoderSpec {
                pooling: Pooling::Mean,
                dimension: 768,
                max_sequence_length: 1024,
                token_type_ids: false
            }
        );
    }

    #[test]
    fn cosine_similarity_of_identical_and_opposite_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_of_a_zero_vector_is_zero_not_nan() {
        let zero = vec![0.0, 0.0, 0.0];
        let a = vec![1.0, 2.0, 3.0];
        assert_eq!(cosine_similarity(&zero, &a), 0.0);
    }

    // -----------------------------------------------------------------------
    // Tests that need the real weights
    //
    // These are `#[ignore]`d because they need ~154 MiB of model files that
    // are not in the repo, and they take seconds rather than milliseconds. To
    // run them:
    //
    //     g-mesh model fetch      (or core/scripts/fetch-embedding-model.sh)
    //     cd core && cargo test embedding -- --ignored --test-threads=1
    //
    // `--test-threads=1` only keeps peak memory to one loaded model; the
    // assertions do not depend on it.
    // -----------------------------------------------------------------------

    #[test]
    #[ignore = "needs the real model weights; see the comment above"]
    fn embedding_produces_a_unit_vector_of_the_documented_width() {
        let model = load_test_model();
        let vector = model.embed("fn main() { println!(\"hi\"); }").unwrap();

        assert_eq!(vector.len(), EMBEDDING_DIM);
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected a unit vector, got norm {norm}");
    }

    /// Acceptance criterion #1: embedding the same text twice is bit-identical.
    #[test]
    #[ignore = "needs the real model weights; see the comment above"]
    fn embedding_the_same_text_twice_gives_identical_vectors() {
        let model = load_test_model();
        let text = "def parse_config(path: str) -> dict:\n    \"\"\"Read a TOML config file.\"\"\"";

        let first = model.embed(text).unwrap();
        let second = model.embed(text).unwrap();

        assert_eq!(first, second, "embeddings must be bit-identical across runs");
    }

    /// Determinism has to survive a fresh `Session`, not just a repeated call
    /// on a warm one - an index written today is compared against vectors
    /// computed by a process started tomorrow.
    #[test]
    #[ignore = "needs the real model weights; see the comment above"]
    fn embedding_is_identical_across_separately_loaded_models() {
        let text = "SELECT id, name FROM users WHERE active = 1";

        let first = load_test_model().embed(text).unwrap();
        let second = load_test_model().embed(text).unwrap();

        assert_eq!(first, second);
    }

    /// Acceptance criterion #2: semantically related code is closer than
    /// unrelated code.
    ///
    /// The two "similar" snippets are the same idea in two languages with no
    /// shared identifiers beyond `sum`/`total`, so a win here cannot be
    /// explained by token overlap alone.
    #[test]
    #[ignore = "needs the real model weights; see the comment above"]
    fn semantically_similar_code_scores_higher_than_unrelated_code() {
        let model = load_test_model();

        let sum_in_python = "def total(values):\n    result = 0\n    for v in values:\n        result += v\n    return result";
        let sum_in_rust = "fn sum(items: &[i32]) -> i32 {\n    let mut acc = 0;\n    for i in items {\n        acc += i;\n    }\n    acc\n}";
        let unrelated = "server.listen(8080, () => console.log('http server ready'));";

        let a = model.embed(sum_in_python).unwrap();
        let b = model.embed(sum_in_rust).unwrap();
        let c = model.embed(unrelated).unwrap();

        let similar = cosine_similarity(&a, &b);
        let dissimilar = cosine_similarity(&a, &c);

        assert!(
            similar > dissimilar,
            "two summation functions ({similar}) should be closer than a summation \
             function and an HTTP server ({dissimilar})"
        );
    }

    /// The same ranking has to hold for the case `search_code` actually
    /// serves: a natural-language query against code. This is the property
    /// `jina-embeddings-v2-base-code` was picked for over a general-purpose
    /// embedder (REQUIREMENTS.md, "Embedding-модель для локального запуска").
    #[test]
    #[ignore = "needs the real model weights; see the comment above"]
    fn a_natural_language_query_is_closer_to_the_code_it_describes() {
        let model = load_test_model();

        let query = model.embed("read a file from disk and return its contents as a string").unwrap();
        let matching = model
            .embed("fn read_to_string(path: &Path) -> io::Result<String> { fs::read_to_string(path) }")
            .unwrap();
        let other =
            model.embed("fn spawn_worker_thread(pool: &ThreadPool) { pool.execute(|| {}); }").unwrap();

        let hit = cosine_similarity(&query, &matching);
        let miss = cosine_similarity(&query, &other);
        assert!(hit > miss, "query->matching {hit} should beat query->unrelated {miss}");
    }

    /// Long inputs are truncated, not rejected - and truncation must not
    /// change the vector's shape or break normalization.
    #[test]
    #[ignore = "needs the real model weights; see the comment above"]
    fn an_over_long_input_is_truncated_rather_than_failing() {
        let model = EmbeddingModel::load_with_max_sequence_length(&test_model_dir(), 16).unwrap();
        let long = "let x = 1;\n".repeat(500);

        let vector = model.embed(&long).unwrap();

        assert_eq!(vector.len(), EMBEDDING_DIM);
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "{norm}");
    }

    // -----------------------------------------------------------------------
    // GM-453: inference options, batch planning, padded pooling
    // -----------------------------------------------------------------------

    /// A lookup over fixed `(name, value)` pairs, for `from_env_with`.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> =
            pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        move |name: &str| map.get(name).cloned()
    }

    /// B1: the default options are today's session and calling pattern.
    ///
    /// Control: make the default `batch_size` 8 and this fails.
    #[test]
    fn default_inference_options_are_todays_session() {
        let options = InferenceOptions::default();
        assert_eq!(options.intra_threads, None);
        assert_eq!(options.inter_threads, None);
        assert_eq!(options.optimization, OptLevel::All);
        assert_eq!(options.intra_spinning, None);
        assert_eq!(options.batch_size, 1);
        assert_eq!(options.token_budget, 8192);
        assert!(!options.bucket_by_length);
        assert!(matches!(GraphOptimizationLevel::from(options.optimization), GraphOptimizationLevel::Level3));
    }

    /// B1/B2: no variable set reads as the default options.
    #[test]
    fn an_empty_environment_gives_the_default_options() {
        assert_eq!(InferenceOptions::from_env_with(env_of(&[])), InferenceOptions::default());
    }

    /// B2: every variable is read; each one moves its own field off the
    /// default.
    ///
    /// Control: drop the `BUCKET_ENV` block from `from_env_with` and
    /// `bucket_by_length` stays false here.
    #[test]
    fn every_embed_variable_is_read() {
        let options = InferenceOptions::from_env_with(env_of(&[
            ("G_MESH_EMBED_INTRA_THREADS", "3"),
            ("G_MESH_EMBED_INTER_THREADS", "2"),
            ("G_MESH_EMBED_OPT_LEVEL", "basic"),
            ("G_MESH_EMBED_SPIN", "0"),
            ("G_MESH_EMBED_BATCH", "16"),
            ("G_MESH_EMBED_TOKEN_BUDGET", "4096"),
            ("G_MESH_EMBED_BUCKET", "1"),
        ]));
        assert_eq!(
            options,
            InferenceOptions {
                intra_threads: Some(3),
                inter_threads: Some(2),
                optimization: OptLevel::Basic,
                intra_spinning: Some(false),
                batch_size: 16,
                token_budget: 4096,
                bucket_by_length: true,
            }
        );
    }

    /// B2: the documented variable names are the ones read.
    #[test]
    fn the_variable_names_are_the_documented_ones() {
        assert_eq!(INTRA_THREADS_ENV, "G_MESH_EMBED_INTRA_THREADS");
        assert_eq!(INTER_THREADS_ENV, "G_MESH_EMBED_INTER_THREADS");
        assert_eq!(OPT_LEVEL_ENV, "G_MESH_EMBED_OPT_LEVEL");
        assert_eq!(SPIN_ENV, "G_MESH_EMBED_SPIN");
        assert_eq!(BATCH_ENV, "G_MESH_EMBED_BATCH");
        assert_eq!(TOKEN_BUDGET_ENV, "G_MESH_EMBED_TOKEN_BUDGET");
        assert_eq!(BUCKET_ENV, "G_MESH_EMBED_BUCKET");
    }

    /// B2: `physical` and `logical` resolve to the core counts; `0` is the
    /// ORT default.
    #[test]
    fn intra_threads_accept_core_rules_and_zero() {
        let read = |value: &str| {
            InferenceOptions::from_env_with(env_of(&[("G_MESH_EMBED_INTRA_THREADS", value)])).intra_threads
        };
        assert_eq!(read("physical"), Some(physical_cores()));
        assert_eq!(read("logical"), Some(logical_cores()));
        assert_eq!(read("0"), None);
        assert_eq!(read("7"), Some(7));
    }

    /// B2: values are read case- and space-insensitively.
    #[test]
    fn values_ignore_case_and_surrounding_space() {
        let options = InferenceOptions::from_env_with(env_of(&[
            ("G_MESH_EMBED_INTRA_THREADS", " Logical "),
            ("G_MESH_EMBED_OPT_LEVEL", "EXTENDED"),
            ("G_MESH_EMBED_BATCH", " 4\n"),
        ]));
        assert_eq!(options.intra_threads, Some(logical_cores()));
        assert_eq!(options.optimization, OptLevel::Extended);
        assert_eq!(options.batch_size, 4);
        for (value, level) in
            [("disable", OptLevel::Disable), ("basic", OptLevel::Basic), ("all", OptLevel::All)]
        {
            let read = InferenceOptions::from_env_with(env_of(&[("G_MESH_EMBED_OPT_LEVEL", value)]));
            assert_eq!(read.optimization, level, "{value}");
        }
    }

    /// B2: an invalid, empty or zero value keeps that field's default and
    /// never panics; the other variables are still read.
    #[test]
    fn invalid_values_fall_back_to_the_default() {
        let bad = ["", "  ", "abc", "-1", "1.5", "0", "99999999999999999999999999"];
        for value in bad {
            let options = InferenceOptions::from_env_with(env_of(&[
                ("G_MESH_EMBED_INTER_THREADS", value),
                ("G_MESH_EMBED_BATCH", value),
                ("G_MESH_EMBED_TOKEN_BUDGET", value),
            ]));
            assert_eq!(options, InferenceOptions::default(), "value {value:?}");
        }
        for value in ["", "fast", "-1", "1.5", "99999999999999999999999999"] {
            let options = InferenceOptions::from_env_with(env_of(&[("G_MESH_EMBED_INTRA_THREADS", value)]));
            assert_eq!(options.intra_threads, None, "value {value:?}");
        }
        for value in ["", "yes", "true", "2", "-1"] {
            let options = InferenceOptions::from_env_with(env_of(&[
                ("G_MESH_EMBED_SPIN", value),
                ("G_MESH_EMBED_BUCKET", value),
            ]));
            assert_eq!(options, InferenceOptions::default(), "value {value:?}");
        }
        for value in ["", "level3", "max", "3"] {
            let options = InferenceOptions::from_env_with(env_of(&[("G_MESH_EMBED_OPT_LEVEL", value)]));
            assert_eq!(options.optimization, OptLevel::All, "value {value:?}");
        }

        let mixed = InferenceOptions::from_env_with(env_of(&[
            ("G_MESH_EMBED_BATCH", "lots"),
            ("G_MESH_EMBED_BUCKET", "1"),
        ]));
        assert_eq!(mixed.batch_size, 1);
        assert!(mixed.bucket_by_length, "one bad variable must not discard the others");
    }

    /// S14 M2: the physical count is at least one and never more than the
    /// logical count. (On a host whose OS count is already below the
    /// logical count, reverting the `min` is not observable here.)
    #[test]
    fn physical_cores_never_exceed_logical_cores() {
        let physical = physical_cores();
        assert!(physical >= 1);
        assert!(physical <= logical_cores(), "{physical} > {}", logical_cores());
    }

    /// Distinct `(physical id, core id)` pairs: hyperthread siblings count
    /// once, the same core id on two packages counts twice.
    #[test]
    fn cpuinfo_counts_distinct_package_and_core_pairs() {
        let cpuinfo = "\
processor\t: 0\nphysical id\t: 0\ncore id\t\t: 0\n\n\
processor\t: 1\nphysical id\t: 0\ncore id\t\t: 1\n\n\
processor\t: 2\nphysical id\t: 0\ncore id\t\t: 0\n\n\
processor\t: 3\nphysical id\t: 1\ncore id\t\t: 0\n\n\
processor\t: 4\nphysical id\t: 1\ncore id\t\t: 1\n\n\
processor\t: 5\nphysical id\t: 1\ncore id\t\t: 1\n";
        assert_eq!(physical_cores_in_cpuinfo(cpuinfo), 4);
        assert_eq!(physical_cores_in_cpuinfo("processor\t: 0\nmodel name\t: x\n"), 0);
        assert_eq!(physical_cores_in_cpuinfo(""), 0);
    }

    /// The batch-plan invariants of §3 for one input.
    fn assert_plan_invariants(lengths: &[usize], batch_size: usize, budget: usize, bucket: bool) {
        let plan = plan_batches(lengths, batch_size, budget, bucket);
        let context =
            format!("lengths {lengths:?} batch {batch_size} budget {budget} bucket {bucket}: {plan:?}");
        let flat: Vec<usize> = plan.iter().flatten().copied().collect();

        let mut sorted = flat.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..lengths.len()).collect::<Vec<_>>(), "every index once; {context}");
        assert!(plan.iter().all(|batch| !batch.is_empty()), "no empty batch; {context}");

        let cap = batch_size.max(1);
        for batch in &plan {
            assert!(batch.len() <= cap, "batch over batch_size; {context}");
            let longest = batch.iter().map(|&i| lengths[i]).max().unwrap();
            if batch.len() > 1 {
                assert!(batch.len() * longest <= budget, "multi-text batch over the budget; {context}");
            }
        }
        for (i, &length) in lengths.iter().enumerate() {
            if length > budget {
                assert!(
                    plan.iter().any(|batch| batch == &vec![i]),
                    "over-budget text {i} not alone; {context}"
                );
            }
        }

        if bucket {
            let keys: Vec<(usize, usize)> = flat.iter().map(|&i| (lengths[i], i)).collect();
            assert!(
                keys.windows(2).all(|w| w[0] <= w[1]),
                "bucketed order not by (length, index); {context}"
            );
        } else {
            assert_eq!(flat, (0..lengths.len()).collect::<Vec<_>>(), "input order kept; {context}");
        }

        // Greedy: the next batch's first text would not have fit.
        for pair in plan.windows(2) {
            let (current, next) = (&pair[0], pair[1][0]);
            let widened = current.iter().map(|&i| lengths[i]).max().unwrap().max(lengths[next]);
            assert!(
                current.len() >= cap || (current.len() + 1) * widened > budget,
                "batch closed while the next text fit; {context}"
            );
        }
    }

    /// B5 on the design note's worked example.
    #[test]
    fn plan_batches_matches_the_design_example() {
        let lengths = [900, 12, 15, 880, 14];
        assert_eq!(plan_batches(&lengths, 4, 8192, true), vec![vec![1, 4, 2, 3], vec![0]]);
        assert_eq!(plan_batches(&lengths, 4, 8192, false), vec![vec![0, 1, 2, 3], vec![4]]);
        // A budget that holds two 900-token texts but not three.
        assert_eq!(plan_batches(&lengths, 4, 1800, false), vec![vec![0, 1], vec![2, 3], vec![4]]);
    }

    /// B5: an over-budget text runs alone; a batch size of 0 is 1.
    #[test]
    fn plan_batches_isolates_over_budget_texts_and_treats_zero_as_one() {
        assert_eq!(plan_batches(&[10, 5000, 10], 8, 100, false), vec![vec![0], vec![1], vec![2]]);
        assert_eq!(plan_batches(&[3, 3, 3], 0, 100, false), vec![vec![0], vec![1], vec![2]]);
        assert_eq!(plan_batches(&[], 4, 100, true), Vec::<Vec<usize>>::new());
        // Equal lengths: ties go by index.
        assert_eq!(plan_batches(&[7, 7, 7, 7], 2, 100, true), vec![vec![0, 1], vec![2, 3]]);
    }

    /// B5 over a deterministic sweep of lengths, sizes, budgets and both
    /// orders.
    ///
    /// Control: make `plan_batches` ignore the budget (drop the
    /// `(current.len() + 1) * widened > token_budget` clause) and this fails.
    #[test]
    fn plan_batches_keeps_its_invariants_over_a_sweep() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..300 {
            let n = (next() % 24) as usize;
            let lengths: Vec<usize> = (0..n).map(|_| 1 + (next() % 1200) as usize).collect();
            for batch_size in [0, 1, 2, 3, 8, 32] {
                for budget in [1, 64, 1024, 2048, 8192] {
                    for bucket in [false, true] {
                        assert_plan_invariants(&lengths, batch_size, budget, bucket);
                    }
                }
            }
        }
    }

    /// `[rows, length, dim]` hidden states whose real positions hold
    /// distinct values and whose padding holds a huge value that would
    /// dominate any pooled vector it leaked into.
    fn padded_states(lengths: &[usize], longest: usize, dim: usize) -> (Vec<f32>, Vec<Vec<i64>>) {
        let mut values = Vec::with_capacity(lengths.len() * longest * dim);
        let mut masks = Vec::new();
        for (row, &length) in lengths.iter().enumerate() {
            for position in 0..longest {
                for d in 0..dim {
                    values.push(if position < length {
                        ((row * 31 + position * 7 + d * 3) % 17) as f32 - 8.0 + 0.25 * row as f32
                    } else {
                        1.0e6
                    });
                }
            }
            masks.push((0..longest).map(|p| i64::from(p < length)).collect());
        }
        (values, masks)
    }

    /// B7: pooling row `b` of a padded batch gives the same bits as pooling
    /// that text alone, for mean and CLS pooling.
    ///
    /// Control: make `pool_row` slice row 0 for every row and rows 1 and 2
    /// get row 0's vector.
    #[test]
    fn pooling_a_padded_row_equals_pooling_the_text_alone() {
        let dim = 6;
        let lengths = [3usize, 5, 1];
        let longest = 5;
        let (values, masks) = padded_states(&lengths, longest, dim);
        let shape = [lengths.len() as i64, longest as i64, dim as i64];
        for pooling in [Pooling::Mean, Pooling::Cls] {
            for (row, &length) in lengths.iter().enumerate() {
                let alone: Vec<f32> = values[row * longest * dim..(row * longest + length) * dim].to_vec();
                let expected =
                    pool(pooling, dim, &[1, length as i64, dim as i64], &alone, &vec![1; length], length)
                        .unwrap();
                let got = pool_row(pooling, dim, &shape, &values, &masks[row], row, longest).unwrap();
                assert_eq!(
                    got.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                    expected.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                    "{pooling:?} row {row}"
                );
            }
        }
        // The rows really differ, so a row mix-up cannot pass by accident.
        let first = pool_row(Pooling::Mean, dim, &shape, &values, &masks[0], 0, longest).unwrap();
        let second = pool_row(Pooling::Mean, dim, &shape, &values, &masks[1], 1, longest).unwrap();
        assert_ne!(first, second);
    }

    /// B7: a wrong shape, a row past the batch, or too few values is an
    /// error, not a misread.
    #[test]
    fn pool_row_rejects_bad_shapes_and_rows() {
        let dim = 4;
        let (values, masks) = padded_states(&[2, 3], 3, dim);
        let shape = [2, 3, dim as i64];
        assert!(
            pool_row(Pooling::Mean, dim, &shape, &values, &masks[0], 2, 3).is_err(),
            "row past the batch"
        );
        assert!(
            pool_row(Pooling::Mean, dim, &[2, 4, dim as i64], &values, &masks[0], 0, 3).is_err(),
            "length"
        );
        assert!(pool_row(Pooling::Mean, dim + 1, &shape, &values, &masks[0], 0, 3).is_err(), "width");
        assert!(pool_row(Pooling::Mean, dim, &[2, 3], &values, &masks[0], 0, 3).is_err(), "rank");
        assert!(
            pool_row(Pooling::Mean, dim, &[3, 3, dim as i64], &values, &masks[0], 2, 3).is_err(),
            "too few values"
        );
    }

    // --- real-model tests (ignored, like the ones above) --------------------

    /// The §6.2 fixed sample plus texts of distinct, shuffled token lengths:
    /// a 1-token text, a signature, a doc + signature, SQL, Python, and a
    /// text far over 1,024 tokens (truncated).
    fn shuffled_length_texts() -> Vec<String> {
        vec![
            "struct Point { x: f64, y: f64 }\nimpl Point { fn norm(&self) -> f64 { (self.x * self.x + self.y * self.y).sqrt() } }"
                .to_string(),
            "x".to_string(),
            "let total = 0;\n".repeat(400),
            "fn read(path: &Path) -> io::Result<String>".to_string(),
            "SELECT id, name, email, created_at, updated_at, last_login, role, team_id, country, city \
             FROM users WHERE active = 1 AND role = 'admin' ORDER BY name, created_at DESC LIMIT 100 OFFSET 200"
                .to_string(),
            "/// Reads the whole file into a string.\n\nfn read_to_string(path: &Path) -> io::Result<String>"
                .to_string(),
            "def parse(path):\n    with open(path) as f:\n        return toml.load(f)".to_string(),
        ]
    }

    fn token_lengths(model: &EmbeddingModel, texts: &[&str]) -> Vec<usize> {
        texts.iter().map(|t| model.tokenizer.encode(*t, true).unwrap().get_ids().len()).collect()
    }

    fn bits_of(vector: &[f32]) -> Vec<u32> {
        vector.iter().map(|v| v.to_bits()).collect()
    }

    fn l2(a: &[f32], b: &[f32]) -> f64 {
        a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum::<f64>().sqrt()
    }

    /// B3 (D2): with the default options (batch 1) `embed_batch` is today's
    /// per-text `embed`, bit for bit, and pads nothing.
    ///
    /// Control: make the default `batch_size` 8; `load` then batches and the
    /// stats carry token counts (and on int8 the vectors move).
    #[test]
    #[ignore = "needs the real model weights; see the comment above"]
    fn batch_size_one_is_bit_identical_to_embed() {
        let owned = shuffled_length_texts();
        let texts: Vec<&str> = owned.iter().map(String::as_str).collect();
        for model in [
            load_test_model(),
            EmbeddingModel::load_with_options(
                &test_model_dir(),
                EncoderSpec::production(),
                &InferenceOptions { bucket_by_length: true, ..InferenceOptions::default() },
            )
            .unwrap(),
        ] {
            let (batched, stats) = model.embed_batch_with_stats(&texts);
            assert_eq!(batched.len(), texts.len());
            for (text, result) in texts.iter().zip(batched) {
                assert_eq!(bits_of(&result.unwrap()), bits_of(&model.embed(text).unwrap()), "{text:.40}");
            }
            assert_eq!(stats, BatchStats { batches: texts.len(), real_tokens: None, padded_tokens: None });
        }
    }

    /// B4: with batch > 1 and bucketing, result `i` belongs to text `i`.
    ///
    /// Bucketing sorts by `(length, index)`, so with distinct token lengths
    /// any permutation of the input forms the same batches with the same
    /// rows, and each text's batched vector must follow it to its new
    /// position bit for bit. The per-text tolerance against `embed` is
    /// asserted only on the fp32 weights named by
    /// `G_MESH_TEST_FP32_MODEL_DIR` (if set); on the default int8 weights the
    /// distance is printed, not asserted (D6, pending measurement).
    ///
    /// Control: write the batched results in batch order (a running
    /// position instead of `out[encoded[b].0]`) and the permuted run no
    /// longer matches.
    #[test]
    #[ignore = "needs the real model weights; see the comment above"]
    fn batched_results_stay_in_input_order() {
        let owned = shuffled_length_texts();
        let texts: Vec<&str> = owned.iter().map(String::as_str).collect();
        let options = InferenceOptions {
            batch_size: 4,
            token_budget: DEFAULT_TOKEN_BUDGET,
            bucket_by_length: true,
            ..InferenceOptions::default()
        };
        let model = EmbeddingModel::load_with_options(&test_model_dir(), EncoderSpec::production(), &options)
            .unwrap();

        let lengths = token_lengths(&model, &texts);
        let mut distinct = lengths.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), lengths.len(), "the sample needs distinct token lengths: {lengths:?}");
        assert!(lengths.contains(&DEFAULT_MAX_SEQUENCE_LENGTH), "one text is truncated: {lengths:?}");

        let (batched, stats) = model.embed_batch_with_stats(&texts);
        let batched: Vec<Vec<f32>> = batched.into_iter().map(Result::unwrap).collect();
        assert!(stats.batches < texts.len(), "some texts must share a batch: {stats:?}");
        assert!(stats.padded_tokens.unwrap() >= stats.real_tokens.unwrap());

        let permutation = [5usize, 2, 6, 0, 3, 1, 4];
        let permuted: Vec<&str> = permutation.iter().map(|&i| texts[i]).collect();
        let again = model.embed_batch(&permuted);
        for (position, &original) in permutation.iter().enumerate() {
            assert_eq!(
                bits_of(again[position].as_ref().unwrap()),
                bits_of(&batched[original]),
                "permuted position {position} must hold text {original}'s vector"
            );
        }

        let worst = texts
            .iter()
            .zip(&batched)
            .map(|(text, vector)| l2(vector, &model.embed(text).unwrap()))
            .fold(0.0, f64::max);
        crate::log_line!(
            "batched vs embed on the default weights: max L2 {worst:.3e} (recorded, not asserted)"
        );

        if let Some(dir) = std::env::var_os("G_MESH_TEST_FP32_MODEL_DIR") {
            let fp32 =
                EmbeddingModel::load_with_options(Path::new(&dir), EncoderSpec::production(), &options)
                    .unwrap();
            let results = fp32.embed_batch(&texts);
            for (text, result) in texts.iter().zip(results) {
                let distance = l2(&result.unwrap(), &fp32.embed(text).unwrap());
                assert!(distance <= 1e-4, "fp32 batched vs embed L2 {distance:.3e} for {text:.40}");
            }
        } else {
            crate::log_line!("G_MESH_TEST_FP32_MODEL_DIR not set: the fp32 L2 <= 1e-4 check did not run");
        }
    }

    /// B6: a batch whose inference fails is retried one text at a time, so
    /// every text still gets its own `embed` vector.
    ///
    /// The failure is forced by an out-of-vocabulary pad id: the padded
    /// call's embedding lookup fails, while `embed` (unpadded) succeeds.
    ///
    /// Control: drop the per-text fallback (put an `Err` in each slot of a
    /// failed batch) and every text errors.
    #[test]
    #[ignore = "needs the real model weights; see the comment above"]
    fn a_failing_batch_falls_back_to_one_call_per_text() {
        let options =
            InferenceOptions { batch_size: 8, bucket_by_length: true, ..InferenceOptions::default() };
        let mut model =
            EmbeddingModel::load_with_options(&test_model_dir(), EncoderSpec::production(), &options)
                .unwrap();
        let texts = ["fn a()", "fn read(path: &Path) -> String", "SELECT 1 FROM t WHERE x = 2 AND y = 3"];
        let lengths = token_lengths(&model, &texts);
        assert!(lengths.windows(2).any(|w| w[0] != w[1]), "the batch must need padding: {lengths:?}");

        let healthy = model.embed_batch(&texts);
        assert!(healthy.iter().all(Result::is_ok));

        model.pad_id = 50_000_000;
        let (results, stats) = model.embed_batch_with_stats(&texts);
        assert_eq!(stats.batches, 1, "the three texts form one batch: {stats:?}");
        for (text, result) in texts.iter().zip(results) {
            let vector = result.unwrap_or_else(|err| panic!("{text}: {err:#}"));
            assert_eq!(bits_of(&vector), bits_of(&model.embed(text).unwrap()), "{text}");
        }
    }
}
