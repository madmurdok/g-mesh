//! The cross-encoder that reorders `search_code`'s top rows.
//!
//! `search_code` takes the embedding ranking's first [`WINDOW`] rows, scores
//! each (query, symbol text) pair with `cross-encoder/ms-marco-MiniLM-L6-v2`,
//! and orders the window by `logit + BETA * cosine`. Rows after the window
//! keep the embedding order, and the score shown stays the cosine. A window
//! whose first row's name is the query ([`confident`]) is not scored and
//! keeps the embedding order. Design: ADR 0016
//! (`docs/adr/0016-cross-encoder-rerank.md`); the int8 weights, one pair per
//! run and the exact-name skip: ADR 0043
//! (`docs/adr/0043-int8-rerank-one-pair-per-run.md`).
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
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokenizers::tokenizer::Tokenizer;
use tokenizers::TruncationParams;

use crate::embedding::ce_text::split_words;
use crate::embedding::model::{ONNX_FILE_NAME, TOKENIZER_FILE_NAME};

/// The directory name under `~/.g-mesh/models/`, and the name the log uses.
pub(crate) const RERANK_MODEL_NAME: &str = "ms-marco-MiniLM-L6-v2";
pub(crate) const RERANK_MODEL_REPO: &str = "cross-encoder/ms-marco-MiniLM-L6-v2";
/// The revision the blend's `BETA` was fitted against; the weights and
/// tokenizer below are its files.
pub(crate) const RERANK_MODEL_REVISION: &str = "233902d25c440f23af6f7d6e94d2946bac0bee0a";
/// The official dynamic-int8 (unsigned, `quint8`, AVX2) export, written to
/// [`ONNX_FILE_NAME`] in the model directory (ADR 0043).
pub(crate) const RERANK_ONNX_REMOTE_PATH: &str = "onnx/model_quint8_avx2.onnx";
pub(crate) const RERANK_ONNX_SIZE: u64 = 23_200_716;
pub(crate) const RERANK_ONNX_SHA256: &str =
    "c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9";
pub(crate) const RERANK_TOKENIZER_SIZE: u64 = 711_396;
pub(crate) const RERANK_TOKENIZER_SHA256: &str =
    "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66";

/// Overrides the cross-encoder's directory. Separate from
/// `G_MESH_MODEL_DIR`, which names the embedding model's own directory.
pub const RERANK_MODEL_DIR_ENV: &str = "G_MESH_RERANK_MODEL_DIR";

/// `off` (any case) switches the rerank off, whatever the global config says.
pub const RERANK_ENV: &str = "G_MESH_RERANK";

/// How many of the embedding ranking's first rows are reordered.
pub const WINDOW: usize = 30;

/// The cosine's weight in the blend `logit + BETA * cosine`.
pub const BETA: f64 = 40.0;

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

/// The cross-encoder's directory: `$G_MESH_RERANK_MODEL_DIR`, else
/// `~/.g-mesh/models/ms-marco-MiniLM-L6-v2`, beside the embedding model.
pub fn default_rerank_model_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(RERANK_MODEL_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().context("could not resolve home directory")?;
    Ok(home.join(".g-mesh").join("models").join(RERANK_MODEL_NAME))
}

/// Whether `dir` holds the pinned weights and tokenizer, by sha256. The
/// blend's `BETA` was fitted against these exact files.
pub(crate) fn check_rerank_files(dir: &Path) -> Result<()> {
    for (name, sha) in [(ONNX_FILE_NAME, RERANK_ONNX_SHA256), (TOKENIZER_FILE_NAME, RERANK_TOKENIZER_SHA256)]
    {
        let path = dir.join(name);
        if !path.exists() {
            bail!("{} not found", path.display());
        }
        let actual = file_sha256(&path)?;
        if actual != sha {
            if name == ONNX_FILE_NAME {
                bail!(
                    "{} has sha256 {actual}, not the pinned {sha} (g-mesh switched the rerank to the \
                     int8 model; this is likely the fp32 file it replaced).\n\
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

/// The window's new order, as indices into it: by `logit + BETA * cosine`,
/// descending, with ties in the window's own (embedding) order. `None` when
/// a logit is not finite or the lengths differ.
pub fn blended_order(logits: &[f32], cosines: &[f64]) -> Option<Vec<usize>> {
    if logits.len() != cosines.len() || logits.iter().any(|logit| !logit.is_finite()) {
        return None;
    }
    let blended: Vec<f64> =
        logits.iter().zip(cosines).map(|(&logit, &cosine)| f64::from(logit) + BETA * cosine).collect();
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

/// `cross-encoder/ms-marco-MiniLM-L6-v2` on onnxruntime.
pub struct CrossEncoder {
    session: Session,
    tokenizer: Tokenizer,
    pad_id: u32,
}

impl CrossEncoder {
    /// Loads the model in `dir` after checking its files against the pinned
    /// shas ([`check_rerank_files`]).
    pub fn load(dir: &Path) -> Result<Self> {
        check_rerank_files(dir)?;
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

/// Whether the rerank is on, and where its model is: read once, on the
/// first call that would rerank.
pub(crate) struct RerankSettings {
    pub(crate) enabled: bool,
    pub(crate) model_dir: Result<PathBuf>,
}

impl RerankSettings {
    /// The global config's `[rerank] enabled`, overridden by
    /// [`RERANK_ENV`]`=off`, and [`default_rerank_model_dir`].
    pub(crate) fn from_environment() -> Self {
        let enabled = !env_disables_rerank(std::env::var_os(RERANK_ENV).as_deref())
            && crate::config::read_global_config().map(|config| config.rerank.enabled).unwrap_or_else(|err| {
                crate::log_line!("g-mesh: failed to read the global config ({err:#}) - the rerank keeps its default (on)");
                true
            });
        Self { enabled, model_dir: default_rerank_model_dir() }
    }
}

fn env_disables_rerank(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case("off"))
}

type SettingsSource = Box<dyn Fn() -> RerankSettings + Send + Sync>;
type ScorerLoader = Box<dyn Fn(&Path) -> Result<Box<dyn Scorer>> + Send + Sync>;
type Log = Box<dyn Fn(&str) + Send + Sync>;

/// What the rerank model is on disk, as `g-mesh status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankModelState {
    /// Both files present at their pinned sizes, and the rerank switched on.
    Present,
    /// Switched on, but a file is absent or the wrong size.
    Missing,
    /// Switched off by `[rerank] enabled = false` or [`RERANK_ENV`]`=off`,
    /// whatever is on disk.
    Disabled,
}

impl RerankModelState {
    /// The state under the global config, the environment and the default
    /// model directory.
    pub fn from_environment() -> Self {
        Self::of(RerankSettings::from_environment())
    }

    /// Checks presence and size only, as `g-mesh model status` does: no
    /// sha256 over the 23 MB weights. A same-size file with other bytes
    /// reads `Present` here while the daemon refuses it.
    pub(crate) fn of(settings: RerankSettings) -> Self {
        if !settings.enabled {
            return Self::Disabled;
        }
        let complete = settings.model_dir.is_ok_and(|dir| {
            [(ONNX_FILE_NAME, RERANK_ONNX_SIZE), (TOKENIZER_FILE_NAME, RERANK_TOKENIZER_SIZE)]
                .iter()
                .all(|(name, size)| std::fs::metadata(dir.join(name)).is_ok_and(|meta| meta.len() == *size))
        });
        if complete {
            Self::Present
        } else {
            Self::Missing
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
    /// mismatch, a corrupt file, an ORT error), or the directory could not
    /// be resolved.
    Failed,
    Loaded,
}

/// What a daemon writes to its state directory once it resolves the rerank:
/// read by `g-mesh status` only while the daemon with `pid` is the live one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    pub pid: u32,
    pub outcome: RerankOutcome,
    /// The model directory the daemon resolved; `None` when it could not.
    pub dir: Option<PathBuf>,
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
    Loaded(Box<dyn Scorer>),
}

impl Outcome {
    fn kind(&self) -> RerankOutcome {
        match self {
            Self::Off => RerankOutcome::Off,
            Self::Absent => RerankOutcome::Absent,
            Self::Failed => RerankOutcome::Failed,
            Self::Loaded(_) => RerankOutcome::Loaded,
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
    /// The production rerank: settings from the global config and the
    /// environment, the real model, the daemon log.
    pub fn from_environment() -> Self {
        Self {
            settings: Box::new(RerankSettings::from_environment),
            loader: Box::new(|dir| Ok(Box::new(CrossEncoder::load(dir)?) as Box<dyn Scorer>)),
            outcome: OnceLock::new(),
            missing_hint_sent: AtomicBool::new(false),
            outcome_file: OnceLock::new(),
            log: Box::new(|line| crate::log_line!("{line}")),
        }
    }

    /// Never reranks, and never reads a setting or a file.
    pub fn off() -> Self {
        Self {
            settings: Box::new(|| RerankSettings { enabled: false, model_dir: Err(anyhow!("off")) }),
            loader: Box::new(|_| bail!("off")),
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
        Self {
            settings: Box::new(settings),
            loader: Box::new(loader),
            outcome: OnceLock::new(),
            missing_hint_sent: AtomicBool::new(false),
            outcome_file: OnceLock::new(),
            log: Box::new(log),
        }
    }

    /// The real model in `dir`, switched on, logging to `log`.
    #[cfg(test)]
    pub(crate) fn real_at(dir: &Path, log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        let dir = dir.to_path_buf();
        Self::with_parts(
            move || RerankSettings { enabled: true, model_dir: Ok(dir.clone()) },
            |dir| Ok(Box::new(CrossEncoder::load(dir)?) as Box<dyn Scorer>),
            log,
        )
    }

    /// The scorer, loading it on the first call. `None` when the rerank is
    /// switched off, or when the model cannot be loaded: that is logged once,
    /// and every later call answers from the same outcome.
    pub(crate) fn scorer(&self) -> Option<&dyn Scorer> {
        match self.outcome() {
            Outcome::Loaded(scorer) => Some(scorer.as_ref()),
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
            let dir = settings.model_dir.as_ref().ok().cloned();
            let outcome = if !settings.enabled {
                Outcome::Off
            } else {
                match settings.model_dir.and_then(|dir| (self.loader)(&dir)) {
                    Ok(scorer) => Outcome::Loaded(scorer),
                    Err(err) => {
                        (self.log)(&format!(
                            "g-mesh daemon: rerank model {RERANK_MODEL_NAME} is not available ({err:#}) - \
                             search_code keeps the embedding order. Run g-mesh model fetch to enable it."
                        ));
                        let absent = dir.as_ref().is_some_and(|dir| {
                            [ONNX_FILE_NAME, TOKENIZER_FILE_NAME].iter().any(|name| !dir.join(name).exists())
                        });
                        if absent {
                            Outcome::Absent
                        } else {
                            Outcome::Failed
                        }
                    }
                }
            };
            if let Some(path) = self.outcome_file.get() {
                let record = OutcomeRecord { pid: std::process::id(), outcome: outcome.kind(), dir };
                let written = serde_json::to_string(&record)
                    .map_err(anyhow::Error::from)
                    .and_then(|json| std::fs::write(path, json).map_err(anyhow::Error::from));
                if let Err(err) = written {
                    (self.log)(&format!(
                        "g-mesh daemon: failed to record the rerank outcome at {} ({err:#})",
                        path.display()
                    ));
                }
            }
            outcome
        })
    }

    /// The window's reranked order ([`blended_order`]) under `scorer`, or
    /// `None` when this call's scoring fails: logged once per failing call,
    /// and the caller keeps the embedding order.
    pub(crate) fn order(
        &self,
        scorer: &dyn Scorer,
        query: &str,
        texts: &[String],
        cosines: &[f64],
    ) -> Option<Vec<usize>> {
        let logits = match scorer.score(query, texts) {
            Ok(logits) => logits,
            Err(err) => {
                (self.log)(&format!(
                    "g-mesh daemon: the rerank failed for one search_code call ({err:#}) - it keeps the \
                     embedding order"
                ));
                return None;
            }
        };
        let order = blended_order(&logits, cosines);
        if order.is_none() {
            (self.log)(&format!(
                "g-mesh daemon: the rerank produced {} logits for {} rows, or a non-finite one - this \
                 search_code call keeps the embedding order",
                logits.len(),
                cosines.len()
            ));
        }
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!((fixture.k, fixture.beta), (WINDOW, BETA));
        assert!(fixture.cases.len() >= 18);
    }

    /// The Python reference's own logits, blended here, give its order:
    /// the blend's weight and tiebreak match `gm464_ce_structured.py`'s F4.
    #[test]
    fn the_blend_of_the_reference_logits_gives_the_reference_order() {
        for case in fixture().cases {
            let logits: Vec<f32> = case.rows.iter().map(|row| row.logit as f32).collect();
            let cosines: Vec<f64> = case.rows.iter().map(|row| row.cosine).collect();
            let order = blended_order(&logits, &cosines).unwrap();
            assert_eq!(ids_in(&case, &order), case.order, "{}", case.query_id);
        }
    }

    #[test]
    fn ties_keep_the_embedding_order() {
        // 0 + 40 * 0.5 == 10 + 40 * 0.25 == 20 exactly.
        let order = blended_order(&[0.0, 10.0, 0.0, 10.0, -1.0], &[0.5, 0.25, 0.5, 0.25, 0.75]).unwrap();
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
        let order = blended_order(&logits, &cosines).unwrap();
        let expected: Vec<usize> =
            [2, 1, 0].iter().flat_map(|&level| (0..WINDOW).filter(move |i| i % 3 == level)).collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn a_non_finite_logit_or_a_length_mismatch_gives_no_order() {
        assert_eq!(blended_order(&[f32::NAN, 1.0], &[0.5, 0.4]), None);
        assert_eq!(blended_order(&[f32::INFINITY], &[0.5]), None);
        assert_eq!(blended_order(&[1.0], &[0.5, 0.4]), None);
    }

    #[test]
    fn the_environment_switch_turns_the_rerank_off() {
        assert!(env_disables_rerank(Some("off".as_ref())));
        assert!(env_disables_rerank(Some("OFF".as_ref())));
        assert!(!env_disables_rerank(Some("on".as_ref())));
        assert!(!env_disables_rerank(None));
    }

    #[test]
    fn intra_threads_are_capped_at_four() {
        let threads = intra_threads();
        assert!((1..=MAX_THREADS).contains(&threads), "{threads}");
    }

    #[test]
    fn a_directory_without_the_model_is_refused_with_the_missing_file_named() {
        let dir = tempfile::tempdir().unwrap();
        let err = CrossEncoder::load(dir.path()).err().unwrap().to_string();
        assert!(err.contains(ONNX_FILE_NAME), "{err}");
    }

    #[test]
    fn other_weights_are_refused_by_sha() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ONNX_FILE_NAME), "not the weights").unwrap();
        std::fs::write(dir.path().join(TOKENIZER_FILE_NAME), "{}").unwrap();
        let err = check_rerank_files(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not the pinned"), "{err}");
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
            move || RerankSettings {
                enabled,
                model_dir: model_dir.clone().ok_or_else(|| anyhow!("no home directory")),
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
                dir: Some(empty.path().to_path_buf())
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
            Some(OutcomeRecord { pid: 7, outcome: RerankOutcome::Failed, dir: Some("/m".into()) })
        );
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(OutcomeRecord::read(&path), None);
        assert_eq!(OutcomeRecord::read(&state.path().join("absent")), None);
    }

    /// The on-disk check `g-mesh status` falls back to: by size only, and
    /// `Disabled` whatever is on disk.
    ///
    /// *Control:* drop the `enabled` check from `RerankModelState::of` ->
    /// the switched-off case with files reads `Present`.
    #[test]
    fn the_on_disk_state_is_judged_by_the_switch_and_the_pinned_sizes() {
        let settings = |enabled: bool, dir: Option<&Path>| RerankSettings {
            enabled,
            model_dir: dir.map(Path::to_path_buf).ok_or_else(|| anyhow!("no home directory")),
        };
        let pinned = tempfile::tempdir().unwrap();
        File::create(pinned.path().join(ONNX_FILE_NAME)).unwrap().set_len(RERANK_ONNX_SIZE).unwrap();
        File::create(pinned.path().join(TOKENIZER_FILE_NAME))
            .unwrap()
            .set_len(RERANK_TOKENIZER_SIZE)
            .unwrap();
        let wrong_size = dir_with_both_files();
        let empty = tempfile::tempdir().unwrap();

        assert_eq!(RerankModelState::of(settings(true, Some(pinned.path()))), RerankModelState::Present);
        assert_eq!(RerankModelState::of(settings(false, Some(pinned.path()))), RerankModelState::Disabled);
        assert_eq!(RerankModelState::of(settings(true, Some(wrong_size.path()))), RerankModelState::Missing);
        assert_eq!(RerankModelState::of(settings(true, Some(empty.path()))), RerankModelState::Missing);
        assert_eq!(RerankModelState::of(settings(true, None)), RerankModelState::Missing);
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

    fn load_real() -> CrossEncoder {
        let dir = default_rerank_model_dir().unwrap();
        CrossEncoder::load(&dir).unwrap_or_else(|err| panic!("could not load the cross-encoder: {err:#}"))
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
        let model = load_real();
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
            let order = ids_in(&case, &blended_order(&logits, &cosines).unwrap());
            let blend = |id: &str| {
                let row = case.rows.iter().find(|row| row.id == id).unwrap();
                row.logit + BETA * row.cosine
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
            assert!((alone[0] - together[i]).abs() < 1e-4, "{i}: {} vs {}", alone[0], together[i]);
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
