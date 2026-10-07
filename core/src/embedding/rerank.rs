//! The cross-encoder that reorders `search_code`'s top rows.
//!
//! `search_code` takes the embedding ranking's first [`WINDOW`] rows, scores
//! each (query, symbol text) pair with `cross-encoder/ms-marco-MiniLM-L6-v2`,
//! and orders the window by `logit + BETA * cosine`. Rows after the window
//! keep the embedding order, and the score shown stays the cosine. Design:
//! ADR 0016 (`docs/adr/0016-cross-encoder-rerank.md`).
//!
//! Like the embedding model, the cross-encoder is fetched by `g-mesh model
//! fetch` and loaded lazily, on the first call that reranks. A missing or
//! refused model, or the switch (`[rerank] enabled` in the global config,
//! `G_MESH_RERANK=off`) turned off, leaves `search_code` on the embedding
//! order and nothing else changes.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use sha2::{Digest, Sha256};
use tokenizers::tokenizer::Tokenizer;
use tokenizers::TruncationParams;

use crate::embedding::model::{ONNX_FILE_NAME, TOKENIZER_FILE_NAME};

/// The directory name under `~/.g-mesh/models/`, and the name the log uses.
pub(crate) const RERANK_MODEL_NAME: &str = "ms-marco-MiniLM-L6-v2";
pub(crate) const RERANK_MODEL_REPO: &str = "cross-encoder/ms-marco-MiniLM-L6-v2";
/// The revision the blend's `BETA` was fitted against; the weights and
/// tokenizer below are its files.
pub(crate) const RERANK_MODEL_REVISION: &str = "233902d25c440f23af6f7d6e94d2946bac0bee0a";
/// The fp32 export, written to [`ONNX_FILE_NAME`] in the model directory.
pub(crate) const RERANK_ONNX_REMOTE_PATH: &str = "onnx/model.onnx";
pub(crate) const RERANK_ONNX_SIZE: u64 = 91_011_230;
pub(crate) const RERANK_ONNX_SHA256: &str =
    "5d3e70fd0c9ff14b9b5169a51e957b7a9c74897afd0a35ce4bd318150c1d4d4a";
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
pub const BETA: f64 = 80.0;

/// Each (query, text) pair is truncated to this many tokens, longest side
/// first.
pub const MAX_PAIR_TOKENS: usize = 512;

/// Pairs per inference batch, taken in token-length order so one long text
/// does not pad the whole window.
const CHUNK: usize = 16;

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
    /// each chunk padded to its longest pair and masked.
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
    fn from_environment() -> Self {
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

/// The rerank as `search_code` sees it: switched off, unavailable, or a
/// loaded scorer. Resolved on the first call that asks and held for the
/// daemon's lifetime, so a config change takes effect on restart.
pub struct Reranker {
    settings: SettingsSource,
    loader: ScorerLoader,
    scorer: OnceLock<Option<Box<dyn Scorer>>>,
    log: Log,
}

impl Reranker {
    /// The production rerank: settings from the global config and the
    /// environment, the real model, the daemon log.
    pub fn from_environment() -> Self {
        Self {
            settings: Box::new(RerankSettings::from_environment),
            loader: Box::new(|dir| Ok(Box::new(CrossEncoder::load(dir)?) as Box<dyn Scorer>)),
            scorer: OnceLock::new(),
            log: Box::new(|line| crate::log_line!("{line}")),
        }
    }

    /// Never reranks, and never reads a setting or a file.
    pub fn off() -> Self {
        Self {
            settings: Box::new(|| RerankSettings { enabled: false, model_dir: Err(anyhow!("off")) }),
            loader: Box::new(|_| bail!("off")),
            scorer: OnceLock::from(None),
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
            scorer: OnceLock::new(),
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
        self.scorer
            .get_or_init(|| {
                let settings = (self.settings)();
                if !settings.enabled {
                    return None;
                }
                match settings.model_dir.and_then(|dir| (self.loader)(&dir)) {
                    Ok(scorer) => Some(scorer),
                    Err(err) => {
                        (self.log)(&format!(
                            "g-mesh daemon: rerank model {RERANK_MODEL_NAME} is not available ({err:#}) - \
                             search_code keeps the embedding order. Run g-mesh model fetch to enable it."
                        ));
                        None
                    }
                }
            })
            .as_deref()
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
        doc_comment: Option<String>,
        signature: Option<String>,
        text: String,
        cosine: f64,
        logit: f64,
    }

    /// `eval/embedding/gm464_rerank_fixture.py`'s output: the Python
    /// reference's logits and F4 order for int8-structured's top 30.
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
        // 0 + 80 * 0.5 == 20 + 80 * 0.25 == 40 exactly.
        let order = blended_order(&[0.0, 20.0, 0.0, 20.0, -1.0], &[0.5, 0.25, 0.5, 0.25, 0.75]).unwrap();
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
        // Three exact blend levels: 40, 50, 60 (logit 10k + 80 * 0.5).
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

    // -----------------------------------------------------------------------
    // Tests that need the real cross-encoder (~87 MiB, not in the repo):
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

    /// Order parity with the Python reference: this crate's text for each
    /// row, scored by the real model, gives the reference's logits within
    /// 1e-4 and its order, allowing a swap only where two blends are within
    /// 1e-3 of each other.
    #[test]
    #[ignore = "needs the real cross-encoder; see the comment above"]
    fn the_real_model_reproduces_the_reference_logits_and_order() {
        let model = load_real();
        for case in fixture().cases {
            let texts: Vec<String> = case
                .rows
                .iter()
                .map(|row| {
                    crate::embedding::text::text_to_embed(
                        row.doc_comment.as_deref(),
                        row.signature.as_deref(),
                    )
                    .unwrap_or_default()
                })
                .collect();
            let logits = model.score(&case.query, &texts).unwrap();
            for ((row, &logit), text) in case.rows.iter().zip(&logits).zip(&texts) {
                let delta = (f64::from(logit) - row.logit).abs();
                assert!(
                    delta < 1e-4,
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
            for (position, (ours, theirs)) in order.iter().zip(&case.order).enumerate() {
                assert!(
                    ours == theirs || (blend(ours) - blend(theirs)).abs() < 1e-3,
                    "{}: position {position} is {ours}, the reference has {theirs}",
                    case.query_id
                );
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
}
