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
//! size 768, mean pooling, ALiBi positions so it accepts up to 8192 tokens.
//! Nothing in this module is specific to it beyond the two shape checks -
//! swapping in another BERT-style ONNX encoder with `input_ids` /
//! `attention_mask` inputs and a `last_hidden_state` output means pointing
//! [`EmbeddingModel::load`] at a different directory, not editing code. What
//! *is* specific is the vector these weights produce, hence
//! `embeddingVersion` in the storage schema - see REQUIREMENTS.md's
//! "Инвалидация эмбеддингов при смене embedding-модели".
//!
//! # Where the weights come from
//!
//! They are not vendored - `model.onnx` alone is ~610 MiB, which has no
//! business in a git repo. This module *reads* an already-present model
//! directory and never downloads anything; fetching is a separate, explicit
//! step so that no g-mesh code path can quietly reach the network. For the
//! default model the files are, pinned to the revision this was verified
//! against:
//!
//! ```text
//! https://huggingface.co/jinaai/jina-embeddings-v2-base-code
//!   revision 516f4baf13dec4ddddda8631e019b5737c8bc250
//!   onnx/model.onnx  -> <model dir>/model.onnx
//!   tokenizer.json   -> <model dir>/tokenizer.json
//! ```
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

/// ONNX graph input/output names. BERT-style encoders exported by
/// `transformers` use exactly these; they are addressed by name rather than by
/// position so that a model exporting extra inputs (`token_type_ids`) fails
/// loudly instead of being fed the wrong tensor.
const INPUT_IDS: &str = "input_ids";
const ATTENTION_MASK: &str = "attention_mask";
const LAST_HIDDEN_STATE: &str = "last_hidden_state";

/// Width of a `jina-embeddings-v2-base-code` vector. Checked at inference time
/// rather than trusted, so pointing g-mesh at a differently-shaped model is a
/// clear error instead of a corrupt index.
pub const EMBEDDING_DIM: usize = 768;

/// Longest input the default model accepts (ALiBi positions, 8192 tokens).
/// Anything longer is truncated by the tokenizer rather than rejected: an
/// over-long docstring should still produce a usable vector.
pub const MAX_SEQUENCE_LENGTH: usize = 8192;

/// Overrides where [`default_model_dir`] looks. Exists so tests and
/// development checkouts can point at a model directory outside the user's
/// home without a config file; ordinary runs never set it.
pub const MODEL_DIR_ENV: &str = "G_MESH_MODEL_DIR";

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
    pub fn load(model_dir: &Path) -> Result<Self> {
        Self::load_with_max_sequence_length(model_dir, MAX_SEQUENCE_LENGTH)
    }

    /// [`load`](Self::load) with a shorter truncation limit. Useful when the
    /// caller knows its inputs are short (symbol signatures) and wants to cap
    /// the worst case an accidentally huge input can cost.
    ///
    /// Values above [`MAX_SEQUENCE_LENGTH`] are not rejected here - that
    /// constant describes the current default model, and this loader is
    /// otherwise model-agnostic - but going past a model's trained context
    /// degrades quality rather than erroring, so raise it only knowingly.
    pub fn load_with_max_sequence_length(model_dir: &Path, max_sequence_length: usize) -> Result<Self> {
        if max_sequence_length == 0 {
            bail!("max_sequence_length must be at least 1");
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

        let session = Session::builder()
            .context("failed to create ONNX session builder")?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .context("failed to set ONNX graph optimization level")?
            // Fixes the floating-point reduction order inside ONNX Runtime, so
            // two runs of the same input cannot differ in the last bits.
            .with_deterministic_compute(true)
            .context("failed to enable deterministic compute")?
            .commit_from_file(&onnx_path)
            .with_context(|| format!("failed to load ONNX model {}", onnx_path.display()))?;

        Ok(Self { session, tokenizer })
    }

    /// Embeds `text` into a unit-length vector of [`EMBEDDING_DIM`] floats.
    ///
    /// Three steps, all of them fixed-order and therefore deterministic:
    ///
    /// 1. Tokenize (truncating to the configured maximum).
    /// 2. Run the encoder, giving one hidden vector per token.
    /// 3. Mean-pool those vectors over the real (non-padding) tokens, then
    ///    L2-normalize.
    ///
    /// Mean pooling is not a choice - it is what this model was trained with
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
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|err| anyhow!("failed to tokenize input: {err}"))?;

        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| i64::from(id)).collect();
        let mask: Vec<i64> = encoding.get_attention_mask().iter().map(|&m| i64::from(m)).collect();
        if ids.is_empty() {
            // Only reachable with a tokenizer whose post-processor adds no
            // special tokens; for this model even "" yields `<s></s>`.
            bail!("input tokenized to zero tokens, nothing to embed");
        }
        debug_assert_eq!(ids.len(), mask.len());

        let sequence_length = ids.len();
        // Batch of one. Batching several texts would mean padding them to a
        // common length, which changes nothing about the result (the
        // attention mask already excludes padding, both in the network and in
        // `mean_pool`) but is not needed yet.
        let input_shape = [1usize, sequence_length];
        let input_ids = Tensor::from_array((input_shape, ids)).context("failed to build input_ids tensor")?;
        let attention_mask = Tensor::from_array((input_shape, mask.clone()))
            .context("failed to build attention_mask tensor")?;

        let outputs = self
            .session
            .run(ort::inputs![INPUT_IDS => input_ids, ATTENTION_MASK => attention_mask]?)
            .context("ONNX inference failed")?;
        let hidden_state = outputs
            .get(LAST_HIDDEN_STATE)
            .ok_or_else(|| anyhow!("model produced no `{LAST_HIDDEN_STATE}` output"))?;
        let (shape, values) = hidden_state
            .try_extract_raw_tensor::<f32>()
            .context("failed to read `last_hidden_state` as f32")?;

        mean_pool(shape, values, &mask, sequence_length)
    }
}

/// Mean-pools `[1, sequence_length, EMBEDDING_DIM]` hidden states over the
/// tokens `mask` marks as real, then L2-normalizes.
///
/// Split out from [`EmbeddingModel::embed`] so the arithmetic that decides
/// what a vector *means* can be tested without 610 MiB of weights on disk.
fn mean_pool(shape: &[i64], values: &[f32], mask: &[i64], sequence_length: usize) -> Result<Vec<f32>> {
    if shape != [1, sequence_length as i64, EMBEDDING_DIM as i64] {
        bail!(
            "unexpected `{LAST_HIDDEN_STATE}` shape {shape:?}; expected [1, {sequence_length}, {EMBEDDING_DIM}]. \
             The model directory is probably not a {EMBEDDING_DIM}-dimensional BERT-style encoder."
        );
    }

    let mut pooled = vec![0f32; EMBEDDING_DIM];
    let mut token_count = 0f32;
    // Iterating positions in order (rather than, say, in parallel) is what
    // keeps float addition associative-order-stable across runs.
    for (position, &m) in mask.iter().enumerate().take(sequence_length) {
        if m == 0 {
            continue;
        }
        token_count += 1.0;
        let row = &values[position * EMBEDDING_DIM..(position + 1) * EMBEDDING_DIM];
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
/// 610 MiB should not be downloaded once per indexed repository - so they sit
/// beside `projects/` rather than inside it.
///
/// Deliberately anchored to the real home even when `G_MESH_HOME`
/// ([`crate::paths::g_mesh_home`]) points g-mesh's *state* elsewhere: that
/// override exists so a test run stops writing project directories into the
/// developer's home, and weights are an immutable per-machine cache rather
/// than per-run state. Moving them with it would turn every isolated run into
/// a 612 MiB download. Use `G_MESH_MODEL_DIR` to move them on purpose.
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
        EmbeddingModel::load(&dir).unwrap_or_else(|err| {
            panic!("could not load the real model from {}: {err:#}", dir.display())
        })
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
        assert_eq!(
            resolve_model_dir(None, "some-model").unwrap(),
            default_model_dir("some-model").unwrap()
        );
    }

    #[test]
    fn mean_pooling_ignores_masked_out_positions() {
        // Two positions, second one padding: the pooled vector must be the
        // first row's direction, not the average of both.
        let mut values = vec![0f32; 2 * EMBEDDING_DIM];
        values[0] = 3.0;
        values[EMBEDDING_DIM] = 100.0;
        values[EMBEDDING_DIM + 1] = 100.0;

        let pooled = mean_pool(&[1, 2, EMBEDDING_DIM as i64], &values, &[1, 0], 2).unwrap();

        assert!((pooled[0] - 1.0).abs() < 1e-6, "{}", pooled[0]);
        assert!(pooled[1].abs() < 1e-6, "{}", pooled[1]);
    }

    #[test]
    fn mean_pooling_returns_a_unit_vector() {
        let mut values = vec![0f32; EMBEDDING_DIM];
        values[0] = 2.0;
        values[1] = -3.0;
        values[2] = 6.0;

        let pooled = mean_pool(&[1, 1, EMBEDDING_DIM as i64], &values, &[1], 1).unwrap();

        let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "{norm}");
        // 2/7, -3/7, 6/7
        assert!((pooled[0] - 2.0 / 7.0).abs() < 1e-6, "{}", pooled[0]);
    }

    #[test]
    fn a_wrongly_shaped_model_output_is_rejected_rather_than_reinterpreted() {
        let values = vec![0f32; 16];
        let err = mean_pool(&[1, 1, 16], &values, &[1], 1).unwrap_err().to_string();
        assert!(err.contains("unexpected"), "{err}");
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
    // These are `#[ignore]`d because they need ~610 MiB of model files that
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
        let matching =
            model.embed("fn read_to_string(path: &Path) -> io::Result<String> { fs::read_to_string(path) }").unwrap();
        let other = model.embed("fn spawn_worker_thread(pool: &ThreadPool) { pool.execute(|| {}); }").unwrap();

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
}
