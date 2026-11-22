//! The cross-encoder that reorders `search_code`'s top rows.
//!
//! `search_code` takes the embedding ranking's first [`WINDOW`] rows, scores
//! each (query, symbol text) pair with the selected [`RerankVariant`]'s
//! cross-encoder, and orders the window by `logit + beta * cosine`, `beta`
//! being the variant's own. Rows after the window keep the embedding order,
//! and the score shown stays the cosine. A window whose first row's name is
//! the query ([`confident`]) is not scored and keeps the embedding order.
//! Design: ADR 0016 (`docs/adr/0016-cross-encoder-rerank.md`); the int8
//! weights, one pair per run and the exact-name skip: ADR 0043
//! (`docs/adr/0043-int8-rerank-one-pair-per-run.md`); the variants, the
//! default and its fallback: ADR 0044 (`docs/adr/0044-qat-reranker-variant.md`).
//!
//! Like the embedding model, the cross-encoder is fetched by `g-mesh model
//! fetch` and loaded lazily, on the first call that reranks. A missing or
//! refused model, or the switch (`[rerank] enabled` in the global config,
//! `G_MESH_RERANK=off`) turned off, leaves `search_code` on the embedding
//! order and nothing else changes.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use anyhow::{anyhow, bail, Context, Result};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokenizers::tokenizer::Tokenizer;
use tokenizers::TruncationParams;

use crate::embedding::ce_text::split_words;
use crate::embedding::model::{ONNX_FILE_NAME, TOKENIZER_FILE_NAME};

/// Which cross-encoder reranks: `[rerank] model` in the global config or
/// [`RERANK_MODEL_ENV`], `l6` by default (ADR 0044).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RerankVariant {
    /// `cross-encoder/ms-marco-MiniLM-L6-v2` fine-tuned on code search
    /// pairs with quantization-aware training (GM-734), quint8 export.
    Qat,
    /// `cross-encoder/ms-marco-MiniLM-L6-v2` itself, the official quint8
    /// export (ADR 0043).
    L6,
}

/// Everything pinned for one [`RerankVariant`].
#[derive(Debug)]
pub(crate) struct VariantSpec {
    /// The value `[rerank] model` and [`RERANK_MODEL_ENV`] take.
    pub(crate) name: &'static str,
    /// The directory name under `~/.g-mesh/models/`, and the flat mirror's
    /// prefix.
    pub(crate) dir_name: &'static str,
    pub(crate) repo: &'static str,
    /// A commit hash, never a branch: the shas below are its files.
    pub(crate) revision: &'static str,
    /// Written to [`ONNX_FILE_NAME`] in the model directory.
    pub(crate) onnx_remote_path: &'static str,
    pub(crate) onnx_size: u64,
    pub(crate) onnx_sha256: &'static str,
    pub(crate) tokenizer_size: u64,
    pub(crate) tokenizer_sha256: &'static str,
    /// The cosine's weight in the blend `logit + beta * cosine`, fitted
    /// against this variant's logits.
    pub(crate) beta: f64,
}

/// Marks a `qat` pin that does not exist until the model is uploaded:
/// [`VariantSpec::check_published`] refuses to download from it.
pub(crate) const UNPUBLISHED: &str = "UNPUBLISHED";

/// Marks a beta not fitted yet: [`RerankVariant::beta`] refuses it.
pub(crate) const UNFITTED_BETA: f64 = f64::NAN;

pub(crate) const QAT: VariantSpec = VariantSpec {
    name: "qat",
    dir_name: "g-mesh-reranker-l6-qat",
    repo: "UNPUBLISHED-hf-org/g-mesh-reranker-l6-qat",
    revision: UNPUBLISHED,
    onnx_remote_path: "onnx/model_quint8.onnx",
    onnx_size: 23_172_943,
    onnx_sha256: "9eb66866978b8b7bf02e7f46cd6f005019151a1ebf03b65e0de4883ab44ea633",
    tokenizer_size: 711_396,
    tokenizer_sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    beta: UNFITTED_BETA,
};

pub(crate) const L6: VariantSpec = VariantSpec {
    name: "l6",
    dir_name: "ms-marco-MiniLM-L6-v2",
    repo: "cross-encoder/ms-marco-MiniLM-L6-v2",
    revision: "233902d25c440f23af6f7d6e94d2946bac0bee0a",
    onnx_remote_path: "onnx/model_quint8_avx2.onnx",
    onnx_size: 23_200_716,
    onnx_sha256: "c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9",
    tokenizer_size: 711_396,
    tokenizer_sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    beta: 40.0,
};

impl RerankVariant {
    pub const DEFAULT: Self = Self::L6;
    pub const ALL: [Self; 2] = [Self::L6, Self::Qat];

    pub(crate) const fn spec(self) -> &'static VariantSpec {
        match self {
            Self::Qat => &QAT,
            Self::L6 => &L6,
        }
    }

    pub fn name(self) -> &'static str {
        self.spec().name
    }

    /// The variant `value` names, ignoring case and surrounding blanks.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        Self::ALL.into_iter().find(|variant| variant.name().eq_ignore_ascii_case(value))
    }

    /// The variant that stands in for this one when it is not published, or
    /// when it is the default and not fetched (ADR 0044); `None` for one
    /// that has none.
    pub fn fallback(self) -> Option<Self> {
        match self {
            Self::Qat => Some(Self::L6),
            Self::L6 => None,
        }
    }

    /// Whether the variant's pins are real: its repo and commit are not
    /// [`UNPUBLISHED`] and its beta is not [`UNFITTED_BETA`]. An unpublished
    /// variant is loaded only from [`RERANK_MODEL_DIR_ENV`] with
    /// `G_MESH_RERANK_BETA` set; otherwise its [`Self::fallback`] stands in.
    pub fn published(self) -> bool {
        #[cfg(test)]
        if PRETEND_PUBLISHED.with(std::cell::Cell::get) {
            return true;
        }
        self.spec().check_published().is_ok() && self.spec().beta.is_finite()
    }

    /// The blend weight, refused while it is [`UNFITTED_BETA`].
    pub fn beta(self) -> Result<f64> {
        let beta = self.spec().beta;
        if !beta.is_finite() {
            bail!(
                "the {} rerank model's blend weight is not fitted yet; set {} to measure it, or \
                 {}=l6",
                self.name(),
                crate::knobs::RERANK_BETA.name,
                RERANK_MODEL_ENV
            );
        }
        Ok(beta)
    }
}

#[cfg(test)]
thread_local! {
    /// Set by [`pretend_published`]: every variant reads as published on
    /// this thread.
    static PRETEND_PUBLISHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `body` with every variant reading as published on this thread, as
/// publishing `qat` will leave it, so the published path is tested before
/// the pins change.
#[cfg(test)]
pub(crate) fn pretend_published<R>(body: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            PRETEND_PUBLISHED.with(|flag| flag.set(false));
        }
    }
    PRETEND_PUBLISHED.with(|flag| flag.set(true));
    let _reset = Reset;
    body()
}

impl std::fmt::Display for RerankVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl VariantSpec {
    /// Refuses a variant whose repo or commit is still [`UNPUBLISHED`].
    pub(crate) fn check_published(&self) -> Result<()> {
        if self.revision.starts_with(UNPUBLISHED) || self.repo.starts_with(UNPUBLISHED) {
            bail!(
                "the {} rerank model is not published yet, so it cannot be downloaded; put its files \
                 in a directory named by {} or set {}=l6",
                self.name,
                RERANK_MODEL_DIR_ENV,
                RERANK_MODEL_ENV
            );
        }
        Ok(())
    }

    /// Whether `dir` holds both files at their pinned sizes; no sha256.
    pub(crate) fn sizes_match(&self, dir: &Path) -> bool {
        [(ONNX_FILE_NAME, self.onnx_size), (TOKENIZER_FILE_NAME, self.tokenizer_size)]
            .iter()
            .all(|(name, size)| std::fs::metadata(dir.join(name)).is_ok_and(|meta| meta.len() == *size))
    }
}

/// Overrides the cross-encoder's directory. Separate from
/// `G_MESH_MODEL_DIR`, which names the embedding model's own directory.
pub const RERANK_MODEL_DIR_ENV: &str = crate::knobs::RERANK_MODEL_DIR.name;

/// Selects the [`RerankVariant`], over `[rerank] model`.
pub const RERANK_MODEL_ENV: &str = crate::knobs::RERANK_MODEL.name;

/// `off` (any case) switches the rerank off, whatever the global config says.
pub const RERANK_ENV: &str = crate::knobs::RERANK.name;

/// How many of the embedding ranking's first rows are reordered.
pub const WINDOW: usize = 30;

/// Each (query, text) pair is truncated to this many tokens, longest side
/// first.
pub const MAX_PAIR_TOKENS: usize = 512;

/// Pairs per inference run. A pair's logit must not depend on the other
/// pairs it runs with; the dynamic-int8 weights quantize each run's
/// activations with that run's own scales, so only one pair per run keeps
/// that true (ADR 0043).
const CHUNK: usize = 1;

/// The most intra-op threads one rerank uses.
const MAX_THREADS: usize = 4;

const INPUT_IDS: &str = "input_ids";
const ATTENTION_MASK: &str = "attention_mask";
const TOKEN_TYPE_IDS: &str = "token_type_ids";
const LOGITS: &str = "logits";

/// Scores (query, text) pairs: one raw logit per text, higher is more
/// relevant. [`CrossEncoder`] in production; a stub in tests.
pub(crate) trait Scorer: Send + Sync {
    fn score(&self, query: &str, texts: &[String]) -> Result<Vec<f32>>;

    /// Tokens `text` takes on its own, special tokens excluded: the unit
    /// of the cross-encoder text's body budget.
    fn count_tokens(&self, text: &str) -> usize;
}

/// `variant`'s directory: `dir_override` (`$G_MESH_RERANK_MODEL_DIR`, read
/// by `config::embedding::rerank_model_dir`), else
/// `~/.g-mesh/models/<variant's dir_name>`, beside the embedding model.
pub fn rerank_model_dir_from(dir_override: Option<&Path>, variant: RerankVariant) -> Result<PathBuf> {
    if let Some(dir) = dir_override {
        return Ok(dir.to_path_buf());
    }
    let home = dirs::home_dir().context("could not resolve home directory")?;
    Ok(home.join(".g-mesh").join("models").join(variant.spec().dir_name))
}

/// The process's `$G_MESH_RERANK_MODEL`, else the default; the global config
/// is not read. For tests that need the real cross-encoder.
#[cfg(test)]
pub(crate) fn env_rerank_variant() -> RerankVariant {
    crate::knobs::lookup(RERANK_MODEL_ENV).filter(|value| !value.is_empty()).map_or(
        RerankVariant::DEFAULT,
        |value| {
            RerankVariant::parse(&value)
                .unwrap_or_else(|| panic!("{RERANK_MODEL_ENV}={value} names no variant"))
        },
    )
}

/// [`rerank_model_dir_from`] under the process's `$G_MESH_RERANK_MODEL_DIR`
/// for `variant`, for tests that need the real cross-encoder.
#[cfg(test)]
pub(crate) fn env_rerank_model_dir(variant: RerankVariant) -> PathBuf {
    let dir_override = crate::knobs::os_non_empty(&crate::knobs::RERANK_MODEL_DIR).map(PathBuf::from);
    rerank_model_dir_from(dir_override.as_deref(), variant).unwrap()
}

/// Whether `dir` holds `variant`'s pinned weights and tokenizer, by sha256.
/// The variant's beta was fitted against these exact files.
pub(crate) fn check_rerank_files(dir: &Path, variant: RerankVariant) -> Result<()> {
    let spec = variant.spec();
    for (name, sha) in [(ONNX_FILE_NAME, spec.onnx_sha256), (TOKENIZER_FILE_NAME, spec.tokenizer_sha256)] {
        let path = dir.join(name);
        if !path.exists() {
            bail!("{} not found", path.display());
        }
        let actual = file_sha256(&path)?;
        if actual != sha {
            if name == ONNX_FILE_NAME {
                bail!(
                    "{} has sha256 {actual}, not the pinned {sha} of the {variant} rerank model (another \
                     variant's or an older file).\n\
                     Replace it: g-mesh model fetch",
                    path.display()
                );
            }
            bail!("{} has sha256 {actual}, not the pinned {sha}", path.display());
        }
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The window's new order, as indices into it: by `logit + beta * cosine`,
/// descending, with ties in the window's own (embedding) order. `None` when
/// a logit is not finite or the lengths differ.
pub fn blended_order(logits: &[f32], cosines: &[f64], beta: f64) -> Option<Vec<usize>> {
    if logits.len() != cosines.len() || logits.iter().any(|logit| !logit.is_finite()) {
        return None;
    }
    let blended: Vec<f64> =
        logits.iter().zip(cosines).map(|(&logit, &cosine)| f64::from(logit) + beta * cosine).collect();
    if blended.iter().any(|s| !s.is_finite()) {
        return None;
    }
    let mut order: Vec<usize> = (0..blended.len()).collect();
    // Equal blends keep the embedding order by an explicit tiebreak on the
    // window position, not by the sort's stability: real windows do tie.
    order.sort_by(|&a, &b| {
        blended[b].partial_cmp(&blended[a]).expect("every blend is finite").then(a.cmp(&b))
    });
    Some(order)
}

/// Whether the embedding ranking's first row already answers `query`, so
/// the window keeps the embedding order without scoring: the query's words
/// ([`split_words`]) equal those of the row's last qualified-name segment,
/// and are not empty. Segments are split on `::`, `.`, `/`, `#` and `\`
/// (ADR 0043).
pub(crate) fn confident(query: &str, top_qualified_name: &str) -> bool {
    let words = split_words(last_segment(top_qualified_name));
    !words.is_empty() && split_words(query) == words
}

/// What follows the last segment separator of a qualified name, the
/// separators matched left to right as the regex `::|[./#\\]` splits.
fn last_segment(qualified_name: &str) -> &str {
    let bytes = qualified_name.as_bytes();
    let (mut start, mut i) = (0, 0);
    while i < bytes.len() {
        if bytes[i..].starts_with(b"::") {
            i += 2;
            start = i;
        } else if matches!(bytes[i], b'.' | b'/' | b'#' | b'\\') {
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    &qualified_name[start..]
}

/// A [`RerankVariant`]'s cross-encoder on onnxruntime.
pub struct CrossEncoder {
    session: Session,
    tokenizer: Tokenizer,
    pad_id: u32,
}

impl CrossEncoder {
    /// Loads `variant`'s model in `dir` after checking its files against
    /// that variant's pinned shas ([`check_rerank_files`]).
    pub fn load(dir: &Path, variant: RerankVariant) -> Result<Self> {
        check_rerank_files(dir, variant)?;
        let tokenizer_path = dir.join(TOKENIZER_FILE_NAME);
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|err| anyhow!("failed to load tokenizer {}: {err}", tokenizer_path.display()))?;
        tokenizer
            .with_truncation(Some(TruncationParams { max_length: MAX_PAIR_TOKENS, ..Default::default() }))
            .map_err(|err| anyhow!("failed to configure tokenizer truncation: {err}"))?;
        tokenizer.with_padding(None);
        let pad_id = ["[PAD]", "<pad>"]
            .iter()
            .find_map(|token| tokenizer.token_to_id(token))
            .ok_or_else(|| anyhow!("tokenizer {} has no padding token", tokenizer_path.display()))?;

        let onnx_path = dir.join(ONNX_FILE_NAME);
        let session = Session::builder()
            .context("failed to create ONNX session builder")?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .context("failed to set ONNX graph optimization level")?
            .with_intra_threads(intra_threads())
            .context("failed to set ONNX intra-op threads")?
            .with_deterministic_compute(true)
            .context("failed to enable deterministic compute")?
            .commit_from_file(&onnx_path)
            .with_context(|| format!("failed to load ONNX model {}", onnx_path.display()))?;
        Ok(Self { session, tokenizer, pad_id })
    }
}

/// `min(MAX_THREADS, physical cores)`.
fn intra_threads() -> usize {
    let cores = sysinfo::System::physical_core_count()
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(1);
    cores.clamp(1, MAX_THREADS)
}

impl Scorer for CrossEncoder {
    /// Pairs are tokenized together (`[CLS] query [SEP] text [SEP]`, with
    /// segment ids), then run in chunks of [`CHUNK`] in token-length order,
    /// each chunk padded to its longest pair and masked. At one pair per
    /// chunk the order and the padding change nothing.
    fn score(&self, query: &str, texts: &[String]) -> Result<Vec<f32>> {
        let encodings = texts
            .iter()
            .map(|text| self.tokenizer.encode((query, text.as_str()), true))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| anyhow!("failed to tokenize a rerank pair: {err}"))?;
        let mut order: Vec<usize> = (0..encodings.len()).collect();
        order.sort_by_key(|&i| encodings[i].get_ids().len());

        let mut logits = vec![0f32; encodings.len()];
        for chunk in order.chunks(CHUNK) {
            let width = chunk.iter().map(|&i| encodings[i].get_ids().len()).max().unwrap_or(0);
            if width == 0 {
                bail!("a rerank pair tokenized to zero tokens");
            }
            let mut ids = vec![i64::from(self.pad_id); chunk.len() * width];
            let mut mask = vec![0i64; chunk.len() * width];
            let mut types = vec![0i64; chunk.len() * width];
            for (row, &i) in chunk.iter().enumerate() {
                let encoding = &encodings[i];
                for (col, ((&id, &kind), &attend)) in encoding
                    .get_ids()
                    .iter()
                    .zip(encoding.get_type_ids())
                    .zip(encoding.get_attention_mask())
                    .enumerate()
                {
                    ids[row * width + col] = i64::from(id);
                    types[row * width + col] = i64::from(kind);
                    mask[row * width + col] = i64::from(attend);
                }
            }
            let shape = [chunk.len(), width];
            let input_ids = Tensor::from_array((shape, ids)).context("failed to build input_ids")?;
            let attention_mask =
                Tensor::from_array((shape, mask)).context("failed to build attention_mask")?;
            let token_type_ids =
                Tensor::from_array((shape, types)).context("failed to build token_type_ids")?;
            let outputs = self
                .session
                .run(ort::inputs![
                    INPUT_IDS => input_ids,
                    ATTENTION_MASK => attention_mask,
                    TOKEN_TYPE_IDS => token_type_ids
                ]?)
                .context("ONNX inference failed")?;
            let output = outputs.get(LOGITS).ok_or_else(|| anyhow!("model produced no `{LOGITS}` output"))?;
            let (_, values) =
                output.try_extract_raw_tensor::<f32>().context("failed to read `logits` as f32")?;
            if values.len() != chunk.len() {
                bail!("model produced {} logits for {} pairs", values.len(), chunk.len());
            }
            for (&i, &value) in chunk.iter().zip(values) {
                logits[i] = value;
            }
        }
        Ok(logits)
    }

    /// A text the tokenizer refuses counts as over any budget.
    fn count_tokens(&self, text: &str) -> usize {
        self.tokenizer.encode(text, false).map_or(usize::MAX, |encoding| encoding.get_ids().len())
    }
}

/// Whether the rerank is on, which variant, and where its model is.
#[derive(Debug)]
pub struct RerankSettings {
    pub enabled: bool,
    pub variant: RerankVariant,
    /// `variant`'s directory.
    pub model_dir: Result<PathBuf>,
    /// The fallback variant's directory, set only when `variant` is the
    /// default, chosen by no setting, and its directory is not overridden by
    /// `$G_MESH_RERANK_MODEL_DIR`. An explicit published choice never falls
    /// back.
    pub fallback_dir: Option<PathBuf>,
    /// `$G_MESH_RERANK_BETA`: replaces `variant`'s beta, and only while
    /// `variant` itself is loaded; a fallback or a stand-in for an
    /// unpublished variant ranks at its own beta.
    pub beta_override: Option<f64>,
    /// The variant a setting asked for that is not published
    /// ([`RerankVariant::published`]); `variant` is its stand-in.
    pub unpublished: Option<RerankVariant>,
}

impl RerankSettings {
    /// `l6` in `model_dir`, with no fallback and its own beta.
    #[cfg(test)]
    pub(crate) fn l6(enabled: bool, model_dir: Result<PathBuf>) -> Self {
        Self {
            enabled,
            variant: RerankVariant::L6,
            model_dir,
            fallback_dir: None,
            beta_override: None,
            unpublished: None,
        }
    }

    /// The variant and directory to load: the selected ones, or the
    /// fallback's when the selected directory lacks a file and the
    /// fallback's has both at their pinned sizes. The flag is true for the
    /// fallback, which drops the beta override.
    fn resolve_fallback(self) -> (RerankVariant, Result<PathBuf>, bool, Option<f64>) {
        if let (Ok(dir), Some(fallback_dir), Some(fallback)) =
            (&self.model_dir, &self.fallback_dir, self.variant.fallback())
        {
            if any_file_absent(dir) && fallback.spec().sizes_match(fallback_dir) {
                return (fallback, Ok(fallback_dir.clone()), true, None);
            }
        }
        (self.variant, self.model_dir, false, self.beta_override)
    }
}

fn any_file_absent(dir: &Path) -> bool {
    [ONNX_FILE_NAME, TOKENIZER_FILE_NAME].iter().any(|name| !dir.join(name).exists())
}

type SettingsSource = Box<dyn Fn() -> RerankSettings + Send + Sync>;
type ScorerLoader = Box<dyn Fn(&Path, RerankVariant) -> Result<Box<dyn Scorer>> + Send + Sync>;
type Log = Box<dyn Fn(&str) + Send + Sync>;

/// What the rerank model is on disk, as `g-mesh status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankModelState {
    /// Both files present at their pinned sizes, and the rerank switched on.
    Present,
    /// The selected default variant is missing, and the fallback variant is
    /// present: the daemon loads the fallback.
    Fallback(RerankVariant),
    /// Switched on, but a file is absent or the wrong size.
    Missing,
    /// Switched off by `[rerank] enabled = false` or [`RERANK_ENV`]`=off`,
    /// whatever is on disk.
    Disabled,
}

impl RerankModelState {
    /// Checks presence and size only, as `g-mesh model status` does: no
    /// sha256 over the 23 MB weights. A same-size file with other bytes
    /// reads `Present` here while the daemon refuses it.
    pub fn of(settings: RerankSettings) -> Self {
        if !settings.enabled {
            return Self::Disabled;
        }
        let spec = settings.variant.spec();
        if settings.model_dir.as_ref().is_ok_and(|dir| spec.sizes_match(dir)) {
            return Self::Present;
        }
        match settings.resolve_fallback() {
            (variant, _, true, _) => Self::Fallback(variant),
            _ => Self::Missing,
        }
    }
}

/// How a daemon resolved its rerank, as `search_code`'s hint and `g-mesh
/// status` name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RerankOutcome {
    /// Switched off: `search_code` keeps the embedding order by choice.
    Off,
    /// Switched on, but a model file is not in its directory.
    Absent,
    /// Switched on, the files are there, and loading them failed (a sha256
    /// mismatch, a corrupt file, an ORT error, a beta not fitted yet), or
    /// the directory could not be resolved.
    Failed,
    Loaded,
}

/// What a daemon writes to its state directory once it resolves the rerank:
/// read by `g-mesh status` only while the daemon with `pid` is the live one.
/// `variant` and `fallback` default when absent, so an older daemon's
/// record still reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    pub pid: u32,
    pub outcome: RerankOutcome,
    /// The model directory the daemon resolved; `None` when it could not.
    pub dir: Option<PathBuf>,
    /// The variant the daemon loaded or tried to; `None` when switched off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<RerankVariant>,
    /// True when `variant` is the fallback for a missing default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
    /// The unpublished variant a setting asked for; `variant` is its
    /// stand-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpublished: Option<RerankVariant>,
}

impl OutcomeRecord {
    /// The record at `path`; `None` for a missing or unparseable file.
    pub fn read(path: &Path) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }
}

/// How the first call that asked resolved the rerank.
enum Outcome {
    Off,
    Absent,
    Failed,
    Loaded { scorer: Box<dyn Scorer>, beta: f64 },
}

impl Outcome {
    fn kind(&self) -> RerankOutcome {
        match self {
            Self::Off => RerankOutcome::Off,
            Self::Absent => RerankOutcome::Absent,
            Self::Failed => RerankOutcome::Failed,
            Self::Loaded { .. } => RerankOutcome::Loaded,
        }
    }
}

/// The rerank as `search_code` sees it: switched off, unavailable, or a
/// loaded scorer. Resolved on the first call that asks and held for the
/// daemon's lifetime, so a config change takes effect on restart.
pub struct Reranker {
    settings: SettingsSource,
    loader: ScorerLoader,
    outcome: OnceLock<Outcome>,
    /// Set once the unavailable-model hint has been handed out: it goes on
    /// at most one `search_code` page per daemon.
    missing_hint_sent: AtomicBool,
    /// Where the outcome is recorded once resolved ([`OutcomeRecord`]); unset
    /// outside a daemon.
    outcome_file: OnceLock<PathBuf>,
    log: Log,
}

impl Reranker {
    /// The production rerank: `settings`, the real model, the daemon log.
    /// The model is loaded on the first call that asks.
    pub fn new(settings: RerankSettings) -> Self {
        let settings = Mutex::new(Some(settings));
        Self {
            settings: Box::new(move || {
                settings.lock().unwrap_or_else(PoisonError::into_inner).take().unwrap_or_else(|| {
                    RerankSettings {
                        enabled: false,
                        variant: RerankVariant::DEFAULT,
                        model_dir: Err(anyhow!("the rerank settings were already taken")),
                        fallback_dir: None,
                        beta_override: None,
                        unpublished: None,
                    }
                })
            }),
            loader: Box::new(|dir, variant| {
                Ok(Box::new(CrossEncoder::load(dir, variant)?) as Box<dyn Scorer>)
            }),
            outcome: OnceLock::new(),
            missing_hint_sent: AtomicBool::new(false),
            outcome_file: OnceLock::new(),
            log: Box::new(|line| crate::log_line!("{line}")),
        }
    }

    /// Never reranks, and never reads a setting or a file.
    pub fn off() -> Self {
        Self {
            settings: Box::new(|| RerankSettings {
                enabled: false,
                variant: RerankVariant::DEFAULT,
                model_dir: Err(anyhow!("off")),
                fallback_dir: None,
                beta_override: None,
                unpublished: None,
            }),
            loader: Box::new(|_, _| bail!("off")),
            outcome: OnceLock::from(Outcome::Off),
            missing_hint_sent: AtomicBool::new(false),
            outcome_file: OnceLock::new(),
            log: Box::new(|_| {}),
        }
    }

    /// A rerank with explicit settings and loader, logging to `log`.
    #[cfg(test)]
    pub(crate) fn with_parts(
        settings: impl Fn() -> RerankSettings + Send + Sync + 'static,
        loader: impl Fn(&Path) -> Result<Box<dyn Scorer>> + Send + Sync + 'static,
        log: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self::with_variant_loader(settings, move |dir, _| loader(dir), log)
    }

    /// [`Self::with_parts`] with a loader that sees the variant it loads.
    #[cfg(test)]
    pub(crate) fn with_variant_loader(
        settings: impl Fn() -> RerankSettings + Send + Sync + 'static,
        loader: impl Fn(&Path, RerankVariant) -> Result<Box<dyn Scorer>> + Send + Sync + 'static,
        log: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            settings: Box::new(settings),
            loader: Box::new(loader),
            outcome: OnceLock::new(),
            missing_hint_sent: AtomicBool::new(false),
            outcome_file: OnceLock::new(),
            log: Box::new(log),
        }
    }

    /// The real `l6` model in `dir`, switched on, logging to `log`.
    #[cfg(test)]
    pub(crate) fn real_at(dir: &Path, log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        let dir = dir.to_path_buf();
        Self::with_variant_loader(
            move || RerankSettings::l6(true, Ok(dir.clone())),
            |dir, variant| Ok(Box::new(CrossEncoder::load(dir, variant)?) as Box<dyn Scorer>),
            log,
        )
    }

    /// The scorer, loading it on the first call. `None` when the rerank is
    /// switched off, or when the model cannot be loaded: that is logged once,
    /// and every later call answers from the same outcome.
    pub(crate) fn scorer(&self) -> Option<&dyn Scorer> {
        match self.outcome() {
            Outcome::Loaded { scorer, .. } => Some(scorer.as_ref()),
            Outcome::Off | Outcome::Absent | Outcome::Failed => None,
        }
    }

    /// Records the outcome as an [`OutcomeRecord`] at `path` once it is
    /// resolved. Set before the first `search_code` call; a later call is
    /// ignored.
    pub(crate) fn record_outcome_to(&self, path: PathBuf) {
        let _ = self.outcome_file.set(path);
    }

    /// `Absent` or `Failed` on the first call that finds the model so, `None`
    /// on every call after it and whenever the model is loaded or switched
    /// off. Resolves the rerank like [`Self::scorer`].
    pub(crate) fn take_unavailable_hint(&self) -> Option<RerankOutcome> {
        let kind = self.outcome().kind();
        (matches!(kind, RerankOutcome::Absent | RerankOutcome::Failed)
            && !self.missing_hint_sent.swap(true, Ordering::Relaxed))
        .then_some(kind)
    }

    fn outcome(&self) -> &Outcome {
        self.outcome.get_or_init(|| {
            let settings = (self.settings)();
            let selected = settings.variant;
            let unpublished = settings.unpublished;
            let (outcome, variant, dir, fallback) = if settings.enabled {
                let (variant, model_dir, fallback, beta_override) = settings.resolve_fallback();
                let dir = model_dir.as_ref().ok().cloned();
                let shown_dir = dir.as_deref().unwrap_or(Path::new("?")).display();
                if let Some(asked) = unpublished {
                    (self.log)(&format!(
                        "{prefix}rerank model {asked} is not published yet - search_code ranks with \
                         {variant} from {shown_dir} at {variant}'s own beta",
                        prefix = crate::log::role_prefix()
                    ));
                } else if fallback {
                    (self.log)(&format!(
                        "{prefix}rerank model {selected} is not fetched - search_code ranks with {variant} \
                         from {shown_dir} at {variant}'s own beta until g-mesh model fetch adds it",
                        prefix = crate::log::role_prefix()
                    ));
                }
                let loaded = beta_override.map_or_else(|| variant.beta(), Ok).and_then(|beta| {
                    model_dir.and_then(|dir| (self.loader)(&dir, variant)).map(|scorer| (scorer, beta))
                });
                let outcome = match loaded {
                    Ok((scorer, beta)) => Outcome::Loaded { scorer, beta },
                    Err(err) => {
                        let advice = if variant.published() {
                            "Run g-mesh model fetch to enable it."
                        } else {
                            "It is not published yet, so g-mesh model fetch cannot add it."
                        };
                        (self.log)(&format!(
                            "{prefix}rerank model {variant} is not available ({err:#}) - \
                             search_code keeps the embedding order. {advice}",
                            prefix = crate::log::role_prefix()
                        ));
                        if dir.as_deref().is_some_and(any_file_absent) {
                            Outcome::Absent
                        } else {
                            Outcome::Failed
                        }
                    }
                };
                (outcome, Some(variant), dir, fallback)
            } else {
                (Outcome::Off, None, settings.model_dir.ok(), false)
            };
            if let Some(path) = self.outcome_file.get() {
                let record = OutcomeRecord {
                    pid: std::process::id(),
                    outcome: outcome.kind(),
                    dir,
                    variant,
                    fallback,
                    unpublished: variant.and(unpublished),
                };
                let written = serde_json::to_string(&record)
                    .map_err(anyhow::Error::from)
                    .and_then(|json| std::fs::write(path, json).map_err(anyhow::Error::from));
                if let Err(err) = written {
                    (self.log)(&format!(
                        "{prefix}failed to record the rerank outcome at {} ({err:#})",
                        path.display(),
                        prefix = crate::log::role_prefix()
                    ));
                }
            }
            outcome
        })
    }

    /// The window's reranked order ([`blended_order`] at the loaded
    /// variant's beta) under `scorer`, or `None` when this call's scoring
    /// fails: logged once per failing call, and the caller keeps the
    /// embedding order.
    pub(crate) fn order(
        &self,
        scorer: &dyn Scorer,
        query: &str,
        texts: &[String],
        cosines: &[f64],
    ) -> Option<Vec<usize>> {
        let Outcome::Loaded { beta, .. } = self.outcome() else {
            return None;
        };
        let logits = match scorer.score(query, texts) {
            Ok(logits) => logits,
            Err(err) => {
                (self.log)(&format!(
                    "{prefix}the rerank failed for one search_code call ({err:#}) - it keeps the \
                     embedding order",
                    prefix = crate::log::role_prefix()
                ));
                return None;
            }
        };
        let order = blended_order(&logits, cosines, *beta);
        if order.is_none() {
            (self.log)(&format!(
                "{prefix}the rerank produced {} logits for {} rows, or a non-finite one - this \
                 search_code call keeps the embedding order",
                logits.len(),
                cosines.len(),
                prefix = crate::log::role_prefix()
            ));
        }
        order
    }
}

#[cfg(test)]
mod variant_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// The variant `testdata/rerank_parity.json` was written from.
    const FIXTURE_VARIANT: RerankVariant = RerankVariant::L6;
    const L6_BETA: f64 = L6.beta;

    #[derive(serde::Deserialize)]
    struct Fixture {
        k: usize,
        beta: f64,
        cases: Vec<Case>,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        query_id: String,
        query: String,
        rows: Vec<FixtureRow>,
        order: Vec<String>,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureRow {
        id: String,
        kind: Option<String>,
        native_kind: Option<String>,
        name: Option<String>,
        qualified_name: Option<String>,
        file_path: Option<String>,
        start_line: usize,
        end_line: usize,
        language: Option<String>,
        doc_comment: Option<String>,
        signature: Option<String>,
        /// The node's source span from `start_line`, cut after the last
        /// line its text reads.
        src_lines: Option<Vec<String>>,
        text: String,
        cosine: f64,
        logit: f64,
    }

    impl FixtureRow {
        /// This crate's text for the row, its body counted by `tok`.
        fn ce_text(&self, tok: &dyn Fn(&str) -> usize) -> String {
            let node = crate::embedding::ce_text::CeNode {
                kind: self.kind.as_deref(),
                native_kind: self.native_kind.as_deref(),
                name: self.name.as_deref(),
                qualified_name: self.qualified_name.as_deref(),
                file_path: self.file_path.as_deref(),
                start_line: 0,
                end_line: self.end_line.saturating_sub(self.start_line),
                language: self.language.as_deref(),
                signature: self.signature.as_deref(),
                doc_comment: self.doc_comment.as_deref(),
            };
            crate::embedding::ce_text::ce_text(&node, self.src_lines.as_deref(), tok)
        }
    }

    /// `eval/embedding/gm464_rerank_fixture.py`'s output: the Python
    /// reference's texts, logits and F4 order for int8-structured's top 30.
    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("testdata/rerank_parity.json")).unwrap()
    }

    fn ids_in(case: &Case, order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| case.rows[i].id.clone()).collect()
    }

    #[test]
    fn the_fixture_was_made_with_this_window_and_blend() {
        let fixture = fixture();
        assert_eq!((fixture.k, fixture.beta), (WINDOW, FIXTURE_VARIANT.spec().beta));
        assert!(fixture.cases.len() >= 18);
    }

    /// The Python reference's own logits, blended here, give its order:
    /// the blend's weight and tiebreak match `gm464_ce_structured.py`'s F4.
    #[test]
    fn the_blend_of_the_reference_logits_gives_the_reference_order() {
        for case in fixture().cases {
            let logits: Vec<f32> = case.rows.iter().map(|row| row.logit as f32).collect();
            let cosines: Vec<f64> = case.rows.iter().map(|row| row.cosine).collect();
            let order = blended_order(&logits, &cosines, L6_BETA).unwrap();
            assert_eq!(ids_in(&case, &order), case.order, "{}", case.query_id);
        }
    }

    #[test]
    fn ties_keep_the_embedding_order() {
        // 0 + 40 * 0.5 == 10 + 40 * 0.25 == 20 exactly.
        let order =
            blended_order(&[0.0, 10.0, 0.0, 10.0, -1.0], &[0.5, 0.25, 0.5, 0.25, 0.75], L6_BETA).unwrap();
        assert_eq!(order, vec![4, 0, 1, 2, 3]);
    }

    /// A full window of exact ties, scattered so that the sort has to move
    /// rows across them: each tie keeps the window order. Under 21 rows
    /// std's unstable sort is an insertion sort and keeps ties anyway, so
    /// [`ties_keep_the_embedding_order`] alone cannot see an unstable sort.
    ///
    /// *Control:* drop the `.then(a.cmp(&b))` tiebreak and switch to
    /// `sort_unstable_by`; std's (deterministic) unstable sort then reorders
    /// these ties. The test pins the order, not the sort: with the tiebreak,
    /// any sort gives it.
    #[test]
    fn ties_across_a_full_window_keep_the_embedding_order() {
        // Three exact blend levels: 20, 30, 40 (logit 10k + 40 * 0.5).
        let logits: Vec<f32> = (0..WINDOW).map(|i| (i % 3) as f32 * 10.0).collect();
        let cosines = vec![0.5; WINDOW];
        let order = blended_order(&logits, &cosines, L6_BETA).unwrap();
        let expected: Vec<usize> =
            [2, 1, 0].iter().flat_map(|&level| (0..WINDOW).filter(move |i| i % 3 == level)).collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn a_non_finite_logit_or_a_length_mismatch_gives_no_order() {
        assert_eq!(blended_order(&[f32::NAN, 1.0], &[0.5, 0.4], L6_BETA), None);
        assert_eq!(blended_order(&[f32::INFINITY], &[0.5], L6_BETA), None);
        assert_eq!(blended_order(&[1.0], &[0.5, 0.4], L6_BETA), None);
    }

    #[test]
    fn intra_threads_are_capped_at_four() {
        let threads = intra_threads();
        assert!((1..=MAX_THREADS).contains(&threads), "{threads}");
    }

    #[test]
    fn a_directory_without_the_model_is_refused_with_the_missing_file_named() {
        let dir = tempfile::tempdir().unwrap();
        let err = CrossEncoder::load(dir.path(), RerankVariant::L6).err().unwrap().to_string();
        assert!(err.contains(ONNX_FILE_NAME), "{err}");
    }

    #[test]
    fn other_weights_are_refused_by_sha() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ONNX_FILE_NAME), "not the weights").unwrap();
        std::fs::write(dir.path().join(TOKENIZER_FILE_NAME), "{}").unwrap();
        let err = check_rerank_files(dir.path(), RerankVariant::L6).unwrap_err().to_string();
        assert!(err.contains("not the pinned"), "{err}");
    }

    /// The refusal of a file that is not the pinned int8 weights tells the
    /// user what to run.
    ///
    /// *Control:* remove the `Replace it: g-mesh model fetch` line from
    /// `check_rerank_files` -> the message no longer names the command.
    #[test]
    fn a_refused_weights_file_names_the_fetch_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ONNX_FILE_NAME), "the fp32 weights, say").unwrap();
        std::fs::write(dir.path().join(TOKENIZER_FILE_NAME), "{}").unwrap();
        let err = check_rerank_files(dir.path(), RerankVariant::L6).unwrap_err().to_string();
        assert!(err.contains("g-mesh model fetch"), "{err}");
        let load = CrossEncoder::load(dir.path(), RerankVariant::L6).err().unwrap().to_string();
        assert!(load.contains("g-mesh model fetch"), "{load}");
    }

    /// (query, qualified name, whether the first row answers the query),
    /// the expected values computed by `skip_flags` rule (a) in
    /// `eval/embedding/gm463_bakeoff.py` (`split_words` of the query equal
    /// to those of the last `::|[./#\\]` segment, both non-empty).
    const CONFIDENT_TABLE: [(&str, &str, bool); 27] = [
        ("parse config", "a::b::ParseConfig", true),
        ("parse config", "a.parse_config", true),
        ("parse config", "a/parseConfig", true),
        ("parse config", "a#parseConfig", true),
        ("parse config", "a\\ParseConfig", true),
        ("parse config", "a.b.parse_config", true),
        ("parse config", "a/b/parseConfig", true),
        ("parse config", "a.b#parseConfig", true),
        ("parse config", "a\\b\\ParseConfig", true),
        ("PARSE CONFIG", "x::parse_config", true),
        ("parse_config", "x::parseConfig", true),
        ("gc outcome", "GcOutcome", true),
        ("list_epics_with_status", "listEpicsWithStatus", true),
        ("gc outcome", "mod::GcOutcome", true),
        ("http server", "HTTPServer", true),
        ("safe int 8", "safeInt8", true),
        ("new", "Vec<T>::new", true),
        ("foo", "a.b#Foo", true),
        ("vec new", "Vec<T>::new", false),
        ("parse", "a::b::parse_config", false),
        ("parse config", "ParseConfigs", false),
        ("parse config", "a#b.c", false),
        ("a b", "a.b#Foo", false),
        ("", "a::b", false),
        ("anything", "a::", false),
        ("anything", "a.b/", false),
        ("anything", "", false),
    ];

    /// `confident` agrees with the eval's rule on every separator, on
    /// casing, snake against camel case, an empty query or segment, and
    /// generics.
    ///
    /// *Controls:* `confident` returns `false` -> the `true` rows fail;
    /// drop `#` from `last_segment` -> `a#parseConfig` reads `a parse
    /// config`; compare the words case-sensitively (skip `split_words`'
    /// lowercase) -> `PARSE CONFIG` fails.
    #[test]
    fn confident_agrees_with_the_eval_rule() {
        for (query, name, expected) in CONFIDENT_TABLE {
            assert_eq!(confident(query, name), expected, "query {query:?}, name {name:?}");
        }
    }

    // --- how a daemon resolved its rerank ----------------------------------

    /// A scorer that is never asked to score: these tests only resolve.
    struct Unused;

    impl Scorer for Unused {
        fn score(&self, _query: &str, _texts: &[String]) -> Result<Vec<f32>> {
            unreachable!("the outcome tests never score")
        }

        fn count_tokens(&self, _text: &str) -> usize {
            unreachable!("the outcome tests never build a text")
        }
    }

    /// A rerank under `enabled` and `model_dir` whose loader answers with
    /// `loads`, counting its calls in the returned counter.
    fn reranker(
        enabled: bool,
        model_dir: Option<PathBuf>,
        loads: bool,
    ) -> (Reranker, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&calls);
        let reranker = Reranker::with_parts(
            move || {
                RerankSettings::l6(enabled, model_dir.clone().ok_or_else(|| anyhow!("no home directory")))
            },
            move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                if loads {
                    Ok(Box::new(Unused) as Box<dyn Scorer>)
                } else {
                    bail!("the loader refused")
                }
            },
            |_| {},
        );
        (reranker, calls)
    }

    /// A directory holding both model files, with any bytes.
    fn dir_with_both_files() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ONNX_FILE_NAME), "weights").unwrap();
        std::fs::write(dir.path().join(TOKENIZER_FILE_NAME), "{}").unwrap();
        dir
    }

    /// The four outcomes, each from the settings and loader result that
    /// define it; only `Loaded` hands out a scorer.
    ///
    /// *Control:* classify every loader error as `Absent` (drop the
    /// file-existence check in `Reranker::outcome`) -> the files-present case
    /// reads `Absent`.
    #[test]
    fn each_outcome_comes_from_its_settings_and_loader_result() {
        let (off, off_loads) = reranker(false, Some("/stub".into()), true);
        assert_eq!(off.take_unavailable_hint(), None);
        assert!(off.scorer().is_none());
        assert_eq!(off_loads.load(Ordering::SeqCst), 0, "a switched-off rerank never loads");

        let (loaded, _) = reranker(true, Some("/stub".into()), true);
        assert!(loaded.scorer().is_some());
        assert_eq!(loaded.take_unavailable_hint(), None);

        let empty = tempfile::tempdir().unwrap();
        let (absent, _) = reranker(true, Some(empty.path().to_path_buf()), false);
        assert!(absent.scorer().is_none());
        assert_eq!(absent.take_unavailable_hint(), Some(RerankOutcome::Absent));

        let only_onnx = tempfile::tempdir().unwrap();
        std::fs::write(only_onnx.path().join(ONNX_FILE_NAME), "weights").unwrap();
        let (one_missing, _) = reranker(true, Some(only_onnx.path().to_path_buf()), false);
        assert_eq!(one_missing.take_unavailable_hint(), Some(RerankOutcome::Absent));

        let both = dir_with_both_files();
        let (failed, _) = reranker(true, Some(both.path().to_path_buf()), false);
        assert!(failed.scorer().is_none());
        assert_eq!(failed.take_unavailable_hint(), Some(RerankOutcome::Failed));

        let (no_dir, no_dir_loads) = reranker(true, None, true);
        assert_eq!(no_dir.take_unavailable_hint(), Some(RerankOutcome::Failed));
        assert_eq!(no_dir_loads.load(Ordering::SeqCst), 0, "no directory, nothing to load");
    }

    /// The hint goes out once per rerank (per daemon), however many callers
    /// ask, and never from `Reranker::off`.
    ///
    /// *Control:* `load` instead of `swap` on `missing_hint_sent` -> every
    /// call returns the outcome.
    #[test]
    fn the_unavailable_hint_is_handed_out_exactly_once() {
        let empty = tempfile::tempdir().unwrap();
        let (absent, loads) = reranker(true, Some(empty.path().to_path_buf()), false);
        assert_eq!(absent.take_unavailable_hint(), Some(RerankOutcome::Absent));
        assert_eq!(absent.take_unavailable_hint(), None);
        assert_eq!(absent.take_unavailable_hint(), None);
        assert_eq!(loads.load(Ordering::SeqCst), 1, "resolved once");

        assert_eq!(Reranker::off().take_unavailable_hint(), None);
    }

    /// Callers on several threads: exactly one gets the hint.
    #[test]
    fn concurrent_callers_get_the_hint_exactly_once() {
        let both = dir_with_both_files();
        let (failed, _) = reranker(true, Some(both.path().to_path_buf()), false);
        let handed = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8).map(|_| scope.spawn(|| failed.take_unavailable_hint())).collect();
            handles.into_iter().map(|handle| handle.join().unwrap()).collect::<Vec<_>>()
        });
        assert_eq!(handed.iter().filter(|hint| hint.is_some()).count(), 1, "{handed:?}");
        assert!(handed.iter().flatten().all(|hint| *hint == RerankOutcome::Failed), "{handed:?}");
    }

    /// The resolved outcome is written once, with this process's pid and the
    /// directory, where `record_outcome_to` pointed; nothing is written
    /// without it.
    ///
    /// *Control:* drop the write from `Reranker::outcome` -> no record.
    #[test]
    fn the_outcome_is_recorded_once_where_it_was_pointed() {
        let state = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let path = state.path().join("rerank.outcome");
        let (absent, _) = reranker(true, Some(empty.path().to_path_buf()), false);
        absent.record_outcome_to(path.clone());
        assert!(!path.exists(), "nothing is written before the outcome is resolved");
        assert!(absent.scorer().is_none());
        assert_eq!(
            OutcomeRecord::read(&path),
            Some(OutcomeRecord {
                pid: std::process::id(),
                outcome: RerankOutcome::Absent,
                dir: Some(empty.path().to_path_buf()),
                variant: Some(RerankVariant::L6),
                fallback: false,
                unpublished: None,
            })
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(absent.take_unavailable_hint(), Some(RerankOutcome::Absent));
        assert!(!path.exists(), "written once, on resolution");

        let (loaded, _) = reranker(true, None, true);
        let loaded_path = state.path().join("loaded.outcome");
        loaded.record_outcome_to(loaded_path.clone());
        let _ = loaded.scorer();
        let record = OutcomeRecord::read(&loaded_path).unwrap();
        assert_eq!((record.outcome, record.dir), (RerankOutcome::Failed, None));

        let (unset, _) = reranker(true, Some(empty.path().to_path_buf()), false);
        let _ = unset.scorer();
        assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 1, "only loaded.outcome");
    }

    #[test]
    fn the_outcome_record_uses_lowercase_words_and_ignores_garbage() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("rerank.outcome");
        std::fs::write(&path, r#"{"pid":7,"outcome":"failed","dir":"/m"}"#).unwrap();
        assert_eq!(
            OutcomeRecord::read(&path),
            Some(OutcomeRecord {
                pid: 7,
                outcome: RerankOutcome::Failed,
                dir: Some("/m".into()),
                variant: None,
                fallback: false,
                unpublished: None,
            })
        );
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(OutcomeRecord::read(&path), None);
        assert_eq!(OutcomeRecord::read(&state.path().join("absent")), None);
    }

    /// The on-disk check `g-mesh status` falls back to: by size only, and
    /// `Disabled` whatever is on disk.
    ///
    /// *Control:* drop the `enabled` check from `RerankModelState::of` ->
    /// the switched-off case with files reads `Present`. Set
    /// `L6.onnx_size` back to the fp32 file's size -> the fp32-size
    /// directory reads `Present`.
    #[test]
    fn the_on_disk_state_is_judged_by_the_switch_and_the_pinned_sizes() {
        let settings = |enabled: bool, dir: Option<&Path>| {
            RerankSettings::l6(
                enabled,
                dir.map(Path::to_path_buf).ok_or_else(|| anyhow!("no home directory")),
            )
        };
        let pinned = tempfile::tempdir().unwrap();
        File::create(pinned.path().join(ONNX_FILE_NAME)).unwrap().set_len(L6.onnx_size).unwrap();
        File::create(pinned.path().join(TOKENIZER_FILE_NAME)).unwrap().set_len(L6.tokenizer_size).unwrap();
        let wrong_size = dir_with_both_files();
        let empty = tempfile::tempdir().unwrap();
        // The size of the fp32 file the int8 model replaced (4.27.0's
        // `L6.onnx_size`).
        const FP32_ONNX_SIZE: u64 = 91_011_230;
        assert_ne!(L6.onnx_size, FP32_ONNX_SIZE);
        let fp32 = tempfile::tempdir().unwrap();
        File::create(fp32.path().join(ONNX_FILE_NAME)).unwrap().set_len(FP32_ONNX_SIZE).unwrap();
        File::create(fp32.path().join(TOKENIZER_FILE_NAME)).unwrap().set_len(L6.tokenizer_size).unwrap();

        assert_eq!(RerankModelState::of(settings(true, Some(pinned.path()))), RerankModelState::Present);
        assert_eq!(RerankModelState::of(settings(false, Some(pinned.path()))), RerankModelState::Disabled);
        assert_eq!(RerankModelState::of(settings(true, Some(wrong_size.path()))), RerankModelState::Missing);
        assert_eq!(RerankModelState::of(settings(true, Some(empty.path()))), RerankModelState::Missing);
        assert_eq!(RerankModelState::of(settings(true, None)), RerankModelState::Missing);
        assert_eq!(RerankModelState::of(settings(true, Some(fp32.path()))), RerankModelState::Missing);
        assert_eq!(RerankModelState::of(settings(false, None)), RerankModelState::Disabled);
    }

    // -----------------------------------------------------------------------
    // Tests that need the real cross-encoder (~23 MiB, not in the repo):
    //
    //     g-mesh model fetch
    //     cd core && cargo test rerank -- --ignored --test-threads=1
    //
    // `G_MESH_RERANK_MODEL_DIR` points them at another copy of the same
    // pinned files.
    // -----------------------------------------------------------------------

    /// The variant `$G_MESH_RERANK_MODEL` names (default `l6`) from its
    /// directory.
    fn load_real() -> CrossEncoder {
        load_real_variant(env_rerank_variant())
    }

    fn load_real_variant(variant: RerankVariant) -> CrossEncoder {
        let dir = env_rerank_model_dir(variant);
        CrossEncoder::load(&dir, variant)
            .unwrap_or_else(|err| panic!("could not load the {variant} cross-encoder: {err:#}"))
    }

    /// Parity with the Python reference: this crate's text for each row,
    /// scored by the real model, gives the reference's logits within
    /// `LOGIT_TOLERANCE`, and the reference's order wherever two reference
    /// blends differ by more than twice that tolerance.
    ///
    /// The int8 kernels differ by CPU path (ONNX Runtime picks one per
    /// process), so logits move by up to a fraction of a point and
    /// near-ties may swap.
    #[test]
    #[ignore = "needs the real cross-encoder; see the comment above"]
    fn the_real_model_reproduces_the_reference_logits_and_order() {
        const LOGIT_TOLERANCE: f64 = 0.25;
        let model = load_real_variant(FIXTURE_VARIANT);
        for case in fixture().cases {
            let texts: Vec<String> =
                case.rows.iter().map(|row| row.ce_text(&|text| model.count_tokens(text))).collect();
            let logits = model.score(&case.query, &texts).unwrap();
            for ((row, &logit), text) in case.rows.iter().zip(&logits).zip(&texts) {
                let delta = (f64::from(logit) - row.logit).abs();
                assert!(
                    delta < LOGIT_TOLERANCE,
                    "{} {}: logit {logit} vs the reference's {} (text equal to the reference's: {})",
                    case.query_id,
                    row.id,
                    row.logit,
                    *text == row.text
                );
            }
            let cosines: Vec<f64> = case.rows.iter().map(|row| row.cosine).collect();
            let order =
                ids_in(&case, &blended_order(&logits, &cosines, FIXTURE_VARIANT.spec().beta).unwrap());
            let blend = |id: &str| {
                let row = case.rows.iter().find(|row| row.id == id).unwrap();
                row.logit + FIXTURE_VARIANT.spec().beta * row.cosine
            };
            let position = |id: &str| order.iter().position(|other| other == id).unwrap();
            for (i, higher) in case.order.iter().enumerate() {
                for lower in &case.order[i + 1..] {
                    if blend(higher) - blend(lower) > 2.0 * LOGIT_TOLERANCE {
                        assert!(
                            position(higher) < position(lower),
                            "{}: {higher} should rank above {lower}",
                            case.query_id
                        );
                    }
                }
            }
        }
    }

    /// Exact equality: the int8 kernels give a row other logits in a batch
    /// than alone, and `CHUNK` of 1 is what rules that out.
    ///
    /// *Control:* `CHUNK = 16` -> the single-row scores differ from the
    /// batched ones.
    #[test]
    #[ignore = "needs the real cross-encoder; see the comment above"]
    fn scoring_is_deterministic_and_independent_of_the_batch() {
        let model = load_real();
        let texts: Vec<String> =
            ["fn read_config(path: &Path) -> Config", "Parses TOML.\n\nfn parse(s: &str)", "fn unrelated()"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let together = model.score("parse the config file", &texts).unwrap();
        assert_eq!(together, model.score("parse the config file", &texts).unwrap());
        for (i, text) in texts.iter().enumerate() {
            let alone = model.score("parse the config file", std::slice::from_ref(text)).unwrap();
            assert_eq!(alone[0], together[i], "{i}");
        }
    }

    /// Dev-only dump for the offline quality eval: reads JSONL rows
    /// `{"query", "text"}` from `RERANK_DUMP_IN` and writes one `{"logit"}`
    /// line per row to `RERANK_DUMP_OUT`, through `CrossEncoder::score` in a
    /// fresh process (the shipped kernel path). Consecutive rows with the
    /// same query are scored in one call. Without both variables it does
    /// nothing, so a run of every ignored test passes over it.
    #[test]
    #[ignore = "dev-only dump for the offline quality eval; needs RERANK_DUMP_IN and RERANK_DUMP_OUT"]
    fn dump_logits_for_the_offline_eval() {
        use std::io::{BufRead, Write};
        let (Ok(input), Ok(output)) = (std::env::var("RERANK_DUMP_IN"), std::env::var("RERANK_DUMP_OUT"))
        else {
            return;
        };
        let model = load_real();
        let mut out = std::io::BufWriter::new(std::fs::File::create(output).unwrap());
        let mut group: Vec<String> = Vec::new();
        let mut group_query = String::new();
        let flush = |query: &str, texts: &mut Vec<String>, out: &mut dyn Write| {
            if texts.is_empty() {
                return;
            }
            for logit in model.score(query, texts).unwrap() {
                writeln!(out, "{}", serde_json::json!({ "logit": logit })).unwrap();
            }
            texts.clear();
        };
        for line in std::io::BufReader::new(std::fs::File::open(input).unwrap()).lines() {
            let row: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            let query = row["query"].as_str().unwrap();
            if query != group_query {
                flush(&group_query, &mut group, &mut out);
                group_query = query.to_string();
            }
            group.push(row["text"].as_str().unwrap().to_string());
        }
        flush(&group_query, &mut group, &mut out);
        out.flush().unwrap();
    }
}
