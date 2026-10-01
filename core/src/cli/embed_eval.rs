//! `g-mesh debug-embed-eval`: the embedding search-quality eval
//! (`docs/architecture/embedding-eval.md`). Hidden; an instrument for
//! choosing the embedding model, not a user-facing command.
//!
//! Four steps, each a subcommand:
//!
//! - `snapshot`: copy a corpus's index (built by this workspace with the
//!   model off) to `work/<corpus>.sqlite`, after checking the checkout is at
//!   the pinned revision, and record its sha256. Every variant reads the same
//!   snapshot, so only the model varies between arms.
//! - `run`: embed every node text the variant's text form produces (the
//!   `structured` form is production's own `text_to_embed`, which is why this
//!   lives in the crate) and every query with one
//!   variant, rank brute force by cosine, and write `manifest.json`,
//!   `vectors.bin`, `query_vectors.bin`, `rankings.jsonl` and `timings.json`
//!   under `<out>/<variant>/<corpus>/`.
//! - `report`: compute D5-D7 over run directories and apply D9.
//! - `parity`: D7's harness-parity check against the production ranking
//!   (`search_code`) and production vectors on the same index.
//!
//! Paths in `corpora.toml`, `variants.toml` and the query files are relative
//! to the eval directory (`--eval-dir`, default `eval/embedding`).

mod bm25;
mod churn;
mod config;
mod context;
mod cost;
mod decision;
mod metrics;
mod progress;
mod queries;
mod rng;

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::embedding::model::{
    cosine_similarity, EmbeddingModel, EncoderSpec, ONNX_FILE_NAME, TOKENIZER_FILE_NAME,
};
use crate::embedding::text::{full_text, text_to_embed};

use config::{Arm, ContextForm, CorporaFile, Role, TextForm, Variant, VariantsFile};
use metrics::{Bound, Floors, Outcome, KEPT_HITS};
use queries::{hex, Query};
use rng::Rng;

#[derive(Debug, Subcommand)]
pub enum EmbedEvalCommand {
    /// Copy a corpus index to the eval's snapshot and record its hash.
    Snapshot(SnapshotArgs),
    /// Embed and rank one variant over one or more corpora.
    Run(RunArgs),
    /// Compute the metrics over run directories and apply the decision rule.
    Report(ReportArgs),
    /// Compare the harness's reference ranking with production `search_code`.
    Parity(ParityArgs),
    /// Count the embedded texts synthetic edits invalidate, per variant.
    Churn(churn::ChurnArgs),
    /// The embedding cost model: calibrate latency per token count, predict
    /// a variant's pass time from it.
    #[command(subcommand)]
    Cost(cost::CostCommand),
}

#[derive(Debug, Args)]
pub struct EvalDir {
    /// The eval directory holding corpora.toml, variants.toml and queries/.
    #[arg(long, default_value = "eval/embedding")]
    pub eval_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct SnapshotArgs {
    #[command(flatten)]
    pub dir: EvalDir,
    #[arg(long)]
    pub corpus: String,
    /// The index.db built from the corpus's pinned checkout.
    #[arg(long)]
    pub index_db: PathBuf,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub dir: EvalDir,
    #[arg(long)]
    pub variant: String,
    /// Corpora to run; all of corpora.toml when omitted.
    #[arg(long)]
    pub corpus: Vec<String>,
    /// Where run directories go; `<eval-dir>/work/runs` when omitted.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Embed the node texts and stop (the D11 pass-time measurement).
    #[arg(long)]
    pub embed_only: bool,
    /// Recompute vectors even when a matching earlier run could be reused.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    #[command(flatten)]
    pub dir: EvalDir,
    /// Run directories, one per variant (`<out>/<variant>`).
    #[arg(required = true)]
    pub runs: Vec<PathBuf>,
    /// D11 measurements per variant (costs.toml); cost gates are skipped
    /// without it.
    #[arg(long)]
    pub costs: Option<PathBuf>,
    /// Also write the full report as JSON here.
    #[arg(long)]
    pub json: Option<PathBuf>,
    /// Score the candidates against this run instead of `settings.reference`
    /// (e.g. a shorter input against the shipped model).
    #[arg(long)]
    pub reference: Option<String>,
}

/// The `variants.toml` row that embeds what production embeds: the shipped
/// int8 weights and `embedding::text::text_to_embed`'s text.
const PRODUCTION_VARIANT: &str = "jina-v2-base-code-int8-structured";

#[derive(Debug, Args)]
pub struct ParityArgs {
    #[command(flatten)]
    pub dir: EvalDir,
    #[arg(long, default_value = "task-tracker-mcp")]
    pub corpus: String,
    /// A production index of the same snapshot, with its `vectors` filled
    /// by the daemon.
    #[arg(long)]
    pub index_db: PathBuf,
    /// The variant whose run is compared: the one embedding exactly what
    /// production does (model, weights and text form).
    #[arg(long, default_value = PRODUCTION_VARIANT)]
    pub variant: String,
    /// That variant's run directory.
    #[arg(long)]
    pub run: PathBuf,
    /// How many authored queries (in id order) to compare.
    #[arg(long, default_value_t = 20)]
    pub count: usize,
}

pub fn run(command: EmbedEvalCommand) -> Result<()> {
    match command {
        EmbedEvalCommand::Snapshot(args) => snapshot(&args),
        EmbedEvalCommand::Run(args) => run_variant(&args),
        EmbedEvalCommand::Report(args) => report(&args),
        EmbedEvalCommand::Parity(args) => parity(&args),
        EmbedEvalCommand::Churn(args) => churn::run(&args),
        EmbedEvalCommand::Cost(command) => cost::run(&command),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(hex(&hasher.finalize()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(path, text + "\n").with_context(|| format!("failed to write {}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_vectors(path: &Path, vectors: &[Vec<f32>]) -> Result<()> {
    let mut bytes = Vec::with_capacity(vectors.iter().map(|v| v.len() * 4).sum());
    for v in vectors {
        for x in v {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
    }
    std::fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

fn read_vectors(path: &Path, count: usize, dimension: usize) -> Result<Vec<Vec<f32>>> {
    let bytes = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    if bytes.len() != count * dimension * 4 {
        bail!("{} holds {} bytes, expected {count} x {dimension} f32", path.display(), bytes.len());
    }
    Ok(bytes
        .chunks_exact(dimension * 4)
        .map(|row| row.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
        .collect())
}

fn snapshot_path(eval_dir: &Path, corpus: &str) -> PathBuf {
    eval_dir.join("work").join(format!("{corpus}.sqlite"))
}

fn snapshot_record_path(eval_dir: &Path, corpus: &str) -> PathBuf {
    eval_dir.join("work").join(format!("{corpus}.snapshot.json"))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotRecord {
    corpus: String,
    revision: String,
    source: String,
    git_ref: Option<String>,
    sha256: String,
    source_db: String,
    nodes: usize,
    embeddable: usize,
    indexer_version: Option<String>,
}

/// One node of the snapshot. `text` is `full_text`'s output; only nodes
/// with text are ranking candidates, exactly as the `vectors` join makes them
/// in `search_code` (`text_to_embed` is `Some` exactly when `full_text` is).
#[derive(Debug, Clone)]
struct Node {
    id: String,
    kind: String,
    name: String,
    qualified_name: String,
    file_path: String,
    language: String,
    native_kind: Option<String>,
    /// The module path (empty for TypeScript).
    container: Option<String>,
    start_line: i64,
    end_line: i64,
    text: Option<String>,
    doc: Option<String>,
    signature: Option<String>,
}

impl Node {
    /// The text a variant embeds for this node. Every form is `Some` exactly
    /// when `text` is, so the candidate set does not depend on the form.
    fn text_for(&self, form: TextForm) -> Option<String> {
        match form {
            TextForm::Full => self.text.clone(),
            TextForm::FirstParagraph => {
                full_text(self.doc.as_deref().map(first_paragraph), self.signature.as_deref())
            }
            TextForm::Structured => text_to_embed(self.doc.as_deref(), self.signature.as_deref()),
        }
    }
}

/// `doc` up to its first blank (whitespace-only) line, trimmed; all of it
/// when it has none.
fn first_paragraph(doc: &str) -> &str {
    let doc = doc.trim();
    let mut end = doc.len();
    let mut offset = 0;
    for line in doc.split_inclusive('\n') {
        if line.trim().is_empty() {
            end = offset;
            break;
        }
        offset += line.len();
    }
    doc[..end].trim_end()
}

fn load_nodes(db: &Path) -> Result<Vec<Node>> {
    let conn =
        Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("failed to open {}", db.display()))?;
    let mut stmt = conn.prepare(
        "SELECT id, kind, qualifiedName, filePath, language, docComment, signature, name, nativeKind, \
         container, startLine, endLine FROM nodes ORDER BY id",
    )?;
    let nodes = stmt
        .query_map([], |row| {
            let doc: Option<String> = row.get(5)?;
            let signature: Option<String> = row.get(6)?;
            Ok(Node {
                id: row.get(0)?,
                kind: row.get(1)?,
                name: row.get(7)?,
                qualified_name: row.get(2)?,
                file_path: row.get(3)?,
                language: row.get(4)?,
                native_kind: row.get(8)?,
                container: row.get(9)?,
                start_line: row.get(10)?,
                end_line: row.get(11)?,
                text: full_text(doc.as_deref(), signature.as_deref()),
                doc,
                signature,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(nodes)
}

/// Checks the snapshot against its record and the pinned revision, and
/// returns its sha256.
fn verified_snapshot(eval_dir: &Path, corpora: &CorporaFile, corpus: &str) -> Result<String> {
    let pinned = corpora.get(corpus)?;
    let record: SnapshotRecord = read_json(&snapshot_record_path(eval_dir, corpus))
        .with_context(|| format!("no snapshot for {corpus}; run `debug-embed-eval snapshot` first"))?;
    if record.revision != pinned.revision {
        bail!("snapshot of {corpus} is at {}, corpora.toml pins {}", record.revision, pinned.revision);
    }
    let sha = sha256_file(&snapshot_path(eval_dir, corpus))?;
    if sha != record.sha256 {
        bail!(
            "snapshot of {corpus} changed since it was recorded (sha256 {sha}, recorded {})",
            record.sha256
        );
    }
    Ok(sha)
}

// ---------------------------------------------------------------------------
// snapshot
// ---------------------------------------------------------------------------

fn snapshot(args: &SnapshotArgs) -> Result<()> {
    let eval_dir = &args.dir.eval_dir;
    let corpora = CorporaFile::load(eval_dir)?;
    let corpus = corpora.get(&args.corpus)?;

    let checkout = eval_dir.join(&corpus.checkout);
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["rev-parse", "HEAD"])
        .output()
        .with_context(|| format!("failed to run git in {}", checkout.display()))?;
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    if head != corpus.revision {
        bail!("{} is at {head:?}, corpora.toml pins {}", checkout.display(), corpus.revision);
    }

    let out = snapshot_path(eval_dir, &corpus.id);
    std::fs::create_dir_all(out.parent().unwrap())?;
    if out.exists() {
        std::fs::remove_file(&out)?;
    }
    // A plain file copy after folding the WAL into the main file: `VACUUM
    // INTO` rejects the `nodes` table's generated column on this SQLite.
    {
        let source = Connection::open(&args.index_db)
            .with_context(|| format!("failed to open {}", args.index_db.display()))?;
        source
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .context("failed to checkpoint the source index")?;
    }
    std::fs::copy(&args.index_db, &out)
        .with_context(|| format!("failed to copy {} to {}", args.index_db.display(), out.display()))?;
    // A read-only open of a WAL database needs a writable -shm beside it;
    // the snapshot is read by many runs, so it is left in rollback mode.
    let (nodes, embeddable, indexer_version) = {
        let conn = Connection::open(&out)?;
        conn.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))?;
        let nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        let indexer: Option<String> =
            conn.query_row("SELECT indexer_version FROM meta WHERE id = 1", [], |r| r.get(0)).ok();
        drop(conn);
        let embeddable = load_nodes(&out)?.iter().filter(|n| n.text.is_some()).count();
        (nodes as usize, embeddable, indexer)
    };
    let record = SnapshotRecord {
        corpus: corpus.id.clone(),
        revision: corpus.revision.clone(),
        source: corpus.source.clone(),
        git_ref: corpus.git_ref.clone(),
        sha256: sha256_file(&out)?,
        source_db: args.index_db.display().to_string(),
        nodes,
        embeddable,
        indexer_version,
    };
    write_json(&snapshot_record_path(eval_dir, &corpus.id), &record)?;
    println!("{}", serde_json::to_string_pretty(&record)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    variant: String,
    variant_fingerprint: String,
    corpus: String,
    revision: String,
    snapshot_sha256: String,
    /// Hash of the candidate node ids in order; vectors.bin rows follow it.
    node_ids_sha256: String,
    node_count: usize,
    dimension: usize,
    query_ids: Vec<String>,
    query_files: Vec<(String, String)>,
    model_files: Vec<(String, String, u64)>,
    /// `hf_repo@revision:onnx_file` of the encoder, for model arms.
    model_source: Option<String>,
    /// D5's truncation confound: share of node texts over 512 and 1024
    /// tokens under this variant's tokenizer (model arms only).
    token_share_over_512: Option<f64>,
    token_share_over_1024: Option<f64>,
    gmesh_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RankingLine {
    id: String,
    /// Candidate node ids that count as a hit.
    expected: Vec<String>,
    /// `(node id, score)`, best first, at most `KEPT_HITS`.
    hits: Vec<(String, f64)>,
    top_language: Option<String>,
    /// Whether the expected symbols have a doc comment; absent for a query
    /// with none, and in runs stored before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_doc: Option<TargetDoc>,
    /// The query shares a sub-token (length >= 4, D3's rule) with an
    /// expected symbol's file path or parent name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path_overlap: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TargetDoc {
    Doc,
    Sig,
    Mixed,
}

/// `targetDoc` and `pathOverlap` of a query over its expected nodes; both
/// `None` when it has none. They depend on the snapshot and the query only,
/// never on the variant.
fn query_labels(
    query: &str,
    expected: &[usize],
    nodes: &[Node],
    parents: &[Option<context::Parent>],
) -> (Option<TargetDoc>, Option<bool>) {
    if expected.is_empty() {
        return (None, None);
    }
    let documented =
        expected.iter().filter(|&&i| nodes[i].doc.as_deref().is_some_and(|d| !d.trim().is_empty())).count();
    let target_doc = match documented {
        0 => TargetDoc::Sig,
        n if n == expected.len() => TargetDoc::Doc,
        _ => TargetDoc::Mixed,
    };
    let words = queries::words(query);
    let overlap = expected.iter().any(|&i| {
        let mut tokens = queries::sub_tokens(&nodes[i].file_path);
        for name in parents[i].iter().flat_map(|p| &p.names) {
            tokens.extend(queries::sub_tokens(name));
        }
        tokens.iter().any(|t| words.contains(t))
    });
    (Some(target_doc), Some(overlap))
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Timings {
    load_ms: f64,
    embed_nodes_ms: f64,
    node_count: usize,
    query_embed_ms: Vec<f64>,
    query_embed_ms_median: Option<f64>,
    rank_ms: f64,
}

fn fingerprint(variant: &Variant) -> String {
    hex(&Sha256::digest(fingerprint_text(variant).as_bytes()))
}

/// The variant's `Debug` form, without the `context` and `text` fields when
/// they are the default: a default must leave the fingerprints of runs
/// stored before the field existed valid.
fn fingerprint_text(variant: &Variant) -> String {
    let mut debug = format!("{variant:?}");
    if variant.context == ContextForm::None {
        debug = debug.replacen(", context: None }", " }", 1);
    }
    if variant.text == TextForm::Full {
        debug = debug.replacen(", text: Full }", " }", 1);
    }
    debug
}

fn run_dir(args: &RunArgs, variant: &str, corpus: &str) -> PathBuf {
    let base = args.out.clone().unwrap_or_else(|| args.dir.eval_dir.join("work").join("runs"));
    base.join(variant).join(corpus)
}

/// Each expected symbol of each query as candidate indices. Collects every
/// failure before failing, so one run lists all the drift to fix.
fn resolve_expected(
    queries: &[Query],
    nodes: &[Node],
    candidate_of: &HashMap<usize, usize>,
) -> Result<Vec<Vec<usize>>> {
    let mut by_key: HashMap<(&str, &str, &str), Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        by_key.entry((n.file_path.as_str(), n.qualified_name.as_str(), n.kind.as_str())).or_default().push(i);
    }
    let mut errors = Vec::new();
    let mut resolved = Vec::with_capacity(queries.len());
    for q in queries {
        let mut hits = Vec::new();
        for e in &q.expected {
            match by_key.get(&(e.file_path.as_str(), e.qualified_name.as_str(), e.kind.as_str())) {
                None => errors.push(format!(
                    "{}: {} {} ({}) is not in the snapshot",
                    q.id, e.file_path, e.qualified_name, e.kind
                )),
                Some(indices) => {
                    for i in indices {
                        match candidate_of.get(i) {
                            Some(&c) => hits.push(c),
                            None => errors.push(format!(
                                "{}: {} {} has no embeddable text, so no model could find it",
                                q.id, e.file_path, e.qualified_name
                            )),
                        }
                    }
                }
            }
        }
        hits.sort_unstable();
        hits.dedup();
        resolved.push(hits);
    }
    if !errors.is_empty() {
        bail!("{} expected symbol(s) did not resolve:\n  {}", errors.len(), errors.join("\n  "));
    }
    Ok(resolved)
}

fn shuffle_words(text: &str, rng: &mut Rng) -> String {
    let mut words: Vec<&str> = text.split_whitespace().collect();
    rng.shuffle(&mut words);
    words.join(" ")
}

/// The ranking candidates (nodes with text, by index) and the text `run`
/// embeds for each under `variant`'s text form and context, before the
/// encoder's document prefix. `cost predict` counts exactly these.
fn candidate_texts(nodes: &[Node], variant: &Variant, arm_seed: u64) -> (Vec<usize>, Vec<String>) {
    let candidates: Vec<usize> = (0..nodes.len()).filter(|&i| nodes[i].text.is_some()).collect();
    let mut all_texts = context::embed_texts(nodes, variant.text, variant.context, arm_seed);
    let texts = candidates.iter().map(|&i| all_texts[i].take().unwrap()).collect();
    (candidates, texts)
}

/// The model's tokenizer with no padding and no truncation: it counts the
/// tokens (special ones included) `EmbeddingModel` truncates to
/// `max_sequence_length`.
fn plain_tokenizer(model_dir: &Path) -> Result<tokenizers::Tokenizer> {
    let path = model_dir.join(TOKENIZER_FILE_NAME);
    let mut tokenizer = tokenizers::Tokenizer::from_file(&path)
        .map_err(|err| anyhow::anyhow!("failed to load tokenizer {}: {err}", path.display()))?;
    tokenizer.with_padding(None);
    tokenizer
        .with_truncation(None)
        .map_err(|err| anyhow::anyhow!("failed to clear tokenizer truncation: {err}"))?;
    Ok(tokenizer)
}

fn token_shares(model_dir: &Path, texts: &[&str]) -> Result<(f64, f64)> {
    let tokenizer = plain_tokenizer(model_dir)?;
    let mut over_512 = 0usize;
    let mut over_1024 = 0usize;
    for text in texts {
        let n = tokenizer
            .encode(*text, true)
            .map_err(|err| anyhow::anyhow!("failed to tokenize: {err}"))?
            .get_ids()
            .len();
        over_512 += usize::from(n > 512);
        over_1024 += usize::from(n > 1024);
    }
    let total = texts.len().max(1) as f64;
    Ok((over_512 as f64 / total, over_1024 as f64 / total))
}

fn model_files(model_dir: &Path) -> Result<Vec<(String, String, u64)>> {
    let mut out = Vec::new();
    for name in [ONNX_FILE_NAME, "model.onnx_data", TOKENIZER_FILE_NAME] {
        let path = model_dir.join(name);
        if path.exists() {
            let size = std::fs::metadata(&path)?.len();
            out.push((name.to_string(), sha256_file(&path)?, size));
        }
    }
    Ok(out)
}

/// Top `KEPT_HITS` of `scores`, best first, ties by candidate order (node id
/// ascending), as `search_code` orders them.
fn top_hits(scores: &[f64]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    order.truncate(KEPT_HITS);
    order
}

fn run_variant(args: &RunArgs) -> Result<()> {
    let eval_dir = &args.dir.eval_dir;
    let corpora = CorporaFile::load(eval_dir)?;
    let variants = VariantsFile::load(eval_dir)?;
    let variant = variants.get(&args.variant)?.clone();
    let corpus_ids: Vec<String> = if args.corpus.is_empty() {
        corpora.corpora.iter().map(|c| c.id.clone()).collect()
    } else {
        args.corpus.clone()
    };

    // One model load for every corpus; its time is recorded per corpus run.
    let load_started = Instant::now();
    let encoder = match variant.arm {
        Arm::Model | Arm::WordsShuffled => {
            let base = variants.encoder_of(&variant)?;
            let dir = base.model_dir(eval_dir)?;
            let spec = base.encoder_spec()?;
            Some((base.clone(), dir.clone(), EmbeddingModel::load_with_spec(&dir, spec)?))
        }
        _ => None,
    };
    let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;

    for corpus in &corpus_ids {
        run_corpus(args, &corpora, &variants, &variant, encoder.as_ref(), load_ms, corpus)
            .with_context(|| format!("variant {} on {corpus}", variant.name))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_corpus(
    args: &RunArgs,
    corpora: &CorporaFile,
    variants: &VariantsFile,
    variant: &Variant,
    encoder: Option<&(Variant, PathBuf, EmbeddingModel)>,
    load_ms: f64,
    corpus: &str,
) -> Result<()> {
    let eval_dir = &args.dir.eval_dir;
    let snapshot_sha = verified_snapshot(eval_dir, corpora, corpus)?;
    let nodes = load_nodes(&snapshot_path(eval_dir, corpus))?;
    let (candidates, owned_texts) = candidate_texts(&nodes, variant, variants.settings.arm_seed);
    let candidate_of: HashMap<usize, usize> = candidates.iter().enumerate().map(|(c, &i)| (i, c)).collect();
    let texts: Vec<&str> = owned_texts.iter().map(String::as_str).collect();
    let node_ids_sha = {
        let mut h = Sha256::new();
        for &i in &candidates {
            h.update(nodes[i].id.as_bytes());
            h.update(b"\n");
        }
        hex(&h.finalize())
    };

    let (queries, query_files) = queries::load(eval_dir, corpus)?;
    let language = &corpora.get(corpus)?.language;
    if let Some(q) = queries.iter().find(|q| &q.language != language) {
        bail!("query {} is {}, corpus {corpus} is {language}", q.id, q.language);
    }
    let expected = resolve_expected(&queries, &nodes, &candidate_of)?;

    let dir = run_dir(args, &variant.name, corpus);
    std::fs::create_dir_all(&dir)?;
    let manifest_path = dir.join("manifest.json");

    let dimension = match variant.arm {
        Arm::Model | Arm::WordsShuffled => encoder.unwrap().2.spec().dimension,
        Arm::Random => variant.dimension.unwrap(),
        Arm::Shuffled => variants.get(variant.reference.as_deref().unwrap())?.dimension.unwrap_or(0),
        Arm::Bm25 => 0,
    };
    let (model_files, token_share_over_512, token_share_over_1024) = match (variant.arm, encoder) {
        (Arm::Model, Some((base, model_dir, _))) => {
            let prefixed: Vec<String> =
                texts.iter().map(|t| format!("{}{t}", base.document_prefix)).collect();
            let refs: Vec<&str> = prefixed.iter().map(String::as_str).collect();
            let (a, b) = token_shares(model_dir, &refs)?;
            (model_files(model_dir)?, Some(a), Some(b))
        }
        _ => (Vec::new(), None, None),
    };
    let manifest = Manifest {
        variant: variant.name.clone(),
        variant_fingerprint: fingerprint(variant),
        corpus: corpus.to_string(),
        revision: corpora.get(corpus)?.revision.clone(),
        snapshot_sha256: snapshot_sha,
        node_ids_sha256: node_ids_sha,
        node_count: candidates.len(),
        dimension,
        query_ids: queries.iter().map(|q| q.id.clone()).collect(),
        query_files,
        model_files,
        model_source: encoder.map(|(base, _, _)| {
            format!(
                "{}@{}:{}",
                base.hf_repo.as_deref().unwrap_or("?"),
                base.revision.as_deref().unwrap_or("?"),
                base.onnx_file.as_deref().unwrap_or("?")
            )
        }),
        token_share_over_512,
        token_share_over_1024,
        gmesh_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let previous: Option<Manifest> = if args.force { None } else { read_json(&manifest_path).ok() };
    let same_nodes = previous.as_ref().is_some_and(|p| {
        p.variant_fingerprint == manifest.variant_fingerprint
            && p.snapshot_sha256 == manifest.snapshot_sha256
            && p.node_ids_sha256 == manifest.node_ids_sha256
            && p.dimension == manifest.dimension
    });
    let same_queries = same_nodes
        && previous
            .as_ref()
            .is_some_and(|p| p.query_ids == manifest.query_ids && p.query_files == manifest.query_files);

    let mut timings = Timings { load_ms, node_count: candidates.len(), ..Timings::default() };
    // Node and query streams are separate, so reusing cached node vectors
    // cannot shift which query vectors a seeded arm draws.
    let mut arm_rng = Rng::new(variants.settings.arm_seed);
    let mut query_rng = Rng::new(variants.settings.arm_seed.wrapping_add(1));

    // --- node vectors -----------------------------------------------------
    let node_vectors: Vec<Vec<f32>> = if variant.arm == Arm::Bm25 {
        Vec::new()
    } else if same_nodes && dir.join("vectors.bin").exists() {
        read_vectors(&dir.join("vectors.bin"), candidates.len(), dimension)?
    } else {
        let started = Instant::now();
        let vectors = match variant.arm {
            Arm::Model => {
                let (base, _, model) = encoder.unwrap();
                let mut progress = progress::stderr(&variant.name, corpus, "embed", texts.len());
                texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let v = model.embed(&format!("{}{t}", base.document_prefix));
                        progress.tick(i + 1);
                        v
                    })
                    .collect::<Result<Vec<_>>>()?
            }
            Arm::WordsShuffled => {
                let (base, _, model) = encoder.unwrap();
                let mut progress = progress::stderr(&variant.name, corpus, "embed", texts.len());
                texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let v = model.embed(&format!(
                            "{}{}",
                            base.document_prefix,
                            shuffle_words(t, &mut arm_rng)
                        ));
                        progress.tick(i + 1);
                        v
                    })
                    .collect::<Result<Vec<_>>>()?
            }
            Arm::Random => (0..candidates.len()).map(|_| arm_rng.unit_gaussian_vector(dimension)).collect(),
            Arm::Shuffled => {
                let reference = reference_run(args, variant, corpus, &manifest)?;
                let source = read_vectors(&reference.join("vectors.bin"), candidates.len(), dimension)?;
                arm_rng.derangement(candidates.len()).into_iter().map(|j| source[j].clone()).collect()
            }
            Arm::Bm25 => unreachable!(),
        };
        timings.embed_nodes_ms = started.elapsed().as_secs_f64() * 1000.0;
        write_vectors(&dir.join("vectors.bin"), &vectors)?;
        vectors
    };
    write_json(&manifest_path, &manifest)?;

    if args.embed_only {
        write_json(&dir.join("timings.json"), &timings)?;
        eprintln!(
            "{} / {corpus}: embedded {} nodes in {:.1} s",
            variant.name,
            candidates.len(),
            timings.embed_nodes_ms / 1000.0
        );
        return Ok(());
    }

    // --- query vectors and ranking -----------------------------------------
    let started = Instant::now();
    let scores_per_query: Vec<Vec<f64>> = if variant.arm == Arm::Bm25 {
        let index = bm25::Bm25::new(texts.iter().copied());
        let mut progress = progress::stderr(&variant.name, corpus, "score", queries.len());
        queries
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let scores = index.scores(&q.text);
                progress.tick(i + 1);
                scores
            })
            .collect()
    } else {
        let query_vectors: Vec<Vec<f32>> = if same_queries && dir.join("query_vectors.bin").exists() {
            read_vectors(&dir.join("query_vectors.bin"), queries.len(), dimension)?
        } else {
            let vectors = match variant.arm {
                Arm::Model | Arm::WordsShuffled => {
                    let (base, _, model) = encoder.unwrap();
                    let mut out = Vec::with_capacity(queries.len());
                    let mut progress = progress::stderr(&variant.name, corpus, "query", queries.len());
                    for q in &queries {
                        let t = Instant::now();
                        out.push(model.embed(&format!("{}{}", base.query_prefix, q.text))?);
                        timings.query_embed_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                        progress.tick(out.len());
                    }
                    out
                }
                Arm::Random => queries.iter().map(|_| query_rng.unit_gaussian_vector(dimension)).collect(),
                Arm::Shuffled => {
                    let reference = reference_run(args, variant, corpus, &manifest)?;
                    read_vectors(&reference.join("query_vectors.bin"), queries.len(), dimension)?
                }
                Arm::Bm25 => unreachable!(),
            };
            write_vectors(&dir.join("query_vectors.bin"), &vectors)?;
            vectors
        };
        let mut progress = progress::stderr(&variant.name, corpus, "score", query_vectors.len());
        query_vectors
            .iter()
            .enumerate()
            .map(|(i, qv)| {
                let scores = node_vectors.iter().map(|nv| f64::from(cosine_similarity(qv, nv))).collect();
                progress.tick(i + 1);
                scores
            })
            .collect()
    };

    let parents = context::parents(&nodes);
    let mut out = std::io::BufWriter::new(std::fs::File::create(dir.join("rankings.jsonl"))?);
    for ((q, scores), expected) in queries.iter().zip(&scores_per_query).zip(&expected) {
        let top = top_hits(scores);
        let expected_nodes: Vec<usize> = expected.iter().map(|&c| candidates[c]).collect();
        let (target_doc, path_overlap) = query_labels(&q.text, &expected_nodes, &nodes, &parents);
        let line = RankingLine {
            target_doc,
            path_overlap,
            id: q.id.clone(),
            expected: expected.iter().map(|&c| nodes[candidates[c]].id.clone()).collect(),
            top_language: top.first().map(|&c| nodes[candidates[c]].language.clone()),
            hits: top.iter().map(|&c| (nodes[candidates[c]].id.clone(), scores[c])).collect(),
        };
        writeln!(out, "{}", serde_json::to_string(&line)?)?;
    }
    out.flush()?;
    timings.rank_ms = started.elapsed().as_secs_f64() * 1000.0;
    timings.query_embed_ms_median = median(&timings.query_embed_ms);
    write_json(&dir.join("timings.json"), &timings)?;
    eprintln!(
        "{} / {corpus}: {} queries ranked over {} nodes",
        variant.name,
        queries.len(),
        candidates.len()
    );
    Ok(())
}

/// The reference model's run directory for a shuffled arm, checked to be
/// over the same nodes and queries.
fn reference_run(args: &RunArgs, variant: &Variant, corpus: &str, manifest: &Manifest) -> Result<PathBuf> {
    let reference = variant.reference.as_deref().unwrap();
    let dir = run_dir(args, reference, corpus);
    let theirs: Manifest = read_json(&dir.join("manifest.json"))
        .with_context(|| format!("run the reference {reference} on {corpus} first"))?;
    if theirs.node_ids_sha256 != manifest.node_ids_sha256 || theirs.query_ids != manifest.query_ids {
        bail!("the reference run in {} is over different nodes or queries; re-run it", dir.display());
    }
    Ok(dir)
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    Some(if v.len().is_multiple_of(2) { (v[mid - 1] + v[mid]) / 2.0 } else { v[mid] })
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CostsFile {
    #[serde(rename = "variant")]
    variants: Vec<CostRow>,
}

#[derive(Debug, Deserialize)]
struct CostRow {
    name: String,
    pass_seconds: f64,
    max_rss_bytes: f64,
    model_bytes: f64,
    query_latency_ms: f64,
}

/// Everything a report needs from one variant's run directory.
struct ArmRun {
    name: String,
    variant: Variant,
    outcomes: Vec<Outcome>,
    manifests: Vec<Manifest>,
    /// Per scored positive: (language, |E|, candidate count), for D7's chance level.
    chance_inputs: Vec<(String, usize, usize)>,
    /// The split labels its rankings carry, by query id.
    labels: Labels,
}

/// `(targetDoc, pathOverlap)` by query id.
type Labels = HashMap<String, (Option<TargetDoc>, Option<bool>)>;

/// The descriptive splits of recall@10 and MRR; never gated.
const SPLITS: [&str; 5] =
    ["targetDoc=doc", "targetDoc=sig", "targetDoc=mixed", "pathOverlap=true", "pathOverlap=false"];

fn in_split(split: &str, labels: &Labels, query_id: &str) -> bool {
    let (doc, overlap) = labels.get(query_id).copied().unwrap_or((None, None));
    match split {
        "targetDoc=doc" => doc == Some(TargetDoc::Doc),
        "targetDoc=sig" => doc == Some(TargetDoc::Sig),
        "targetDoc=mixed" => doc == Some(TargetDoc::Mixed),
        "pathOverlap=true" => overlap == Some(true),
        "pathOverlap=false" => overlap == Some(false),
        _ => unreachable!("unknown split {split}"),
    }
}

/// Every arm's labels in one map, so an arm stored before the labels
/// existed (the reference) splits by the labels of the arms that carry
/// them. The labels depend on the snapshot and the queries only, so two
/// arms that disagree are an error.
fn merged_labels(arms: &[ArmRun]) -> Result<Labels> {
    let mut merged = Labels::new();
    for arm in arms {
        for (id, &label) in &arm.labels {
            match merged.get(id) {
                Some(&seen) if seen != label => {
                    bail!("query {id} has labels {seen:?} in one run and {label:?} in {}", arm.name)
                }
                _ => {
                    merged.insert(id.clone(), label);
                }
            }
        }
    }
    Ok(merged)
}

/// Each split's size and pooled recall@10 and MRR for one arm.
fn split_summary(arm: &ArmRun, labels: &Labels) -> serde_json::Value {
    let o = &arm.outcomes;
    let splits: serde_json::Map<String, serde_json::Value> = SPLITS
        .iter()
        .map(|&split| {
            let keep = |x: &Outcome| in_split(split, labels, &x.query_id);
            let r10 = recall_groups(o, 10, keep);
            let mrr = metrics::group_by_language(
                o,
                |x| metrics::is_scored_positive(x) && keep(x),
                Outcome::reciprocal_rank,
            );
            let n: usize = r10.values().map(Vec::len).sum();
            (
                split.to_string(),
                json!({"n": n, "recall@10": metrics::pooled_mean(&r10), "mrr": metrics::pooled_mean(&mrr)}),
            )
        })
        .collect();
    serde_json::Value::Object(splits)
}

/// Each split's paired recall@10 and MRR deltas (`arm - reference`) with
/// their bootstrap bounds.
fn split_deltas(
    reference: &ArmRun,
    arm: &ArmRun,
    labels: &Labels,
    settings: &config::Settings,
) -> Result<serde_json::Value> {
    let bound = |g: &metrics::Groups| {
        bound_of(g, settings).map(|b| json!({"point": b.point, "lower": b.lower, "upper": b.upper}))
    };
    let mut out = serde_json::Map::new();
    for split in SPLITS {
        let keep = |x: &Outcome| metrics::is_scored_positive(x) && in_split(split, labels, &x.query_id);
        let recall =
            metrics::paired_deltas(&reference.outcomes, &arm.outcomes, keep, |x| Some(x.hit_at(10)))?;
        let mrr =
            metrics::paired_deltas(&reference.outcomes, &arm.outcomes, keep, |x| Some(x.reciprocal_rank()))?;
        let n: usize = recall.values().map(Vec::len).sum();
        out.insert(
            split.to_string(),
            json!({"n": n, "recall@10Delta": bound(&recall), "mrrDelta": bound(&mrr)}),
        );
    }
    Ok(serde_json::Value::Object(out))
}

fn load_arm(eval_dir: &Path, variants: &VariantsFile, run: &Path) -> Result<ArmRun> {
    let name = run
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} does not name a variant", run.display()))?
        .to_string();
    let variant = variants.get(&name)?.clone();
    let mut outcomes = Vec::new();
    let mut manifests = Vec::new();
    let mut chance_inputs = Vec::new();
    let mut labels = Labels::new();
    let mut corpus_dirs: Vec<PathBuf> = std::fs::read_dir(run)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("rankings.jsonl").exists())
        .collect();
    corpus_dirs.sort();
    for dir in corpus_dirs {
        let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
        if manifest.variant_fingerprint != fingerprint(&variant) {
            bail!("{} was run with a different definition of {name}; re-run it", dir.display());
        }
        let (queries, files) = queries::load(eval_dir, &manifest.corpus)?;
        if files != manifest.query_files {
            bail!(
                "the query files of {} changed since {} was run (D3 freeze)",
                manifest.corpus,
                dir.display()
            );
        }
        let by_id: HashMap<&str, &Query> = queries.iter().map(|q| (q.id.as_str(), q)).collect();
        let reader = BufReader::new(std::fs::File::open(dir.join("rankings.jsonl"))?);
        for line in reader.lines() {
            let line: RankingLine = serde_json::from_str(&line?)?;
            let q = by_id.get(line.id.as_str()).with_context(|| format!("unknown query {}", line.id))?;
            let first_expected_rank =
                line.hits.iter().position(|(id, _)| line.expected.contains(id)).map(|p| p + 1);
            if line.target_doc.is_some() || line.path_overlap.is_some() {
                labels.insert(line.id.clone(), (line.target_doc, line.path_overlap));
            }
            if q.positive() && !q.mechanical {
                chance_inputs.push((q.language.clone(), line.expected.len(), manifest.node_count));
            }
            outcomes.push(Outcome {
                query_id: q.id.clone(),
                corpus: q.corpus.clone(),
                language: q.language.clone(),
                positive: q.positive(),
                mechanical: q.mechanical,
                overlap: q.overlap(),
                held_out: q.held_out(),
                first_expected_rank,
                top_score: line.hits.first().map(|(_, s)| *s),
                top_language: line.top_language.clone(),
            });
        }
        manifests.push(manifest);
    }
    if outcomes.is_empty() {
        bail!("{} holds no rankings", run.display());
    }
    Ok(ArmRun { name, variant, outcomes, manifests, chance_inputs, labels })
}

fn bound_of(groups: &metrics::Groups, settings: &config::Settings) -> Option<Bound> {
    metrics::bootstrap(groups, settings.bootstrap_resamples, settings.bootstrap_seed)
}

/// Q5 (D9): the paired held-out false-alarm deltas `candidate - reference`,
/// each arm judged at its own floors. Returns the per-language groups, the
/// pooled bound the gate reads (NaN when nothing pairs), and the report
/// fields: the pooled delta, and each language's delta with its own bound,
/// reported rather than gated so the pooled gate can be read beside it.
fn false_alarm_deltas(
    reference: &[Outcome],
    reference_floors: &Floors,
    candidate: &[Outcome],
    candidate_floors: &Floors,
    settings: &config::Settings,
) -> (metrics::Groups, Bound, serde_json::Value) {
    let fa = metrics::paired_at_own_floors(
        reference,
        reference_floors,
        candidate,
        candidate_floors,
        metrics::false_alarm_indicator,
    );
    let nan = Bound { point: f64::NAN, lower: f64::NAN, upper: f64::NAN };
    let pooled = bound_of(&fa, settings).unwrap_or(nan);
    let by_language: BTreeMap<String, serde_json::Value> = fa
        .iter()
        .map(|(l, v)| {
            let one: metrics::Groups = BTreeMap::from([(l.clone(), v.clone())]);
            let b = bound_of(&one, settings);
            (
                l.clone(),
                json!({
                    "n": v.len(),
                    "point": b.as_ref().map(|b| b.point),
                    "lower": b.as_ref().map(|b| b.lower),
                    "upper": b.as_ref().map(|b| b.upper),
                }),
            )
        })
        .collect();
    let report = json!({
        "falseAlarmDeltaByLanguage": by_language,
        "falseAlarmDelta": {"point": pooled.point, "lower": pooled.lower, "upper": pooled.upper},
    });
    (fa, pooled, report)
}

fn recall_groups(outcomes: &[Outcome], k: usize, keep: impl Fn(&Outcome) -> bool) -> metrics::Groups {
    metrics::group_by_language(outcomes, |o| metrics::is_scored_positive(o) && keep(o), |o| o.hit_at(k))
}

fn summary(arm: &ArmRun, settings: &config::Settings) -> serde_json::Value {
    let o = &arm.outcomes;
    let r10 = recall_groups(o, 10, |_| true);
    let mrr = metrics::group_by_language(o, metrics::is_scored_positive, Outcome::reciprocal_rank);
    let by_corpus: BTreeMap<String, f64> = {
        let mut m: BTreeMap<String, (f64, usize)> = BTreeMap::new();
        for x in o.iter().filter(|x| metrics::is_scored_positive(x)) {
            let e = m.entry(x.corpus.clone()).or_default();
            e.0 += x.hit_at(10);
            e.1 += 1;
        }
        m.into_iter().map(|(c, (h, n))| (c, h / n as f64)).collect()
    };
    let per_language: BTreeMap<String, f64> =
        r10.iter().map(|(l, v)| (l.clone(), v.iter().sum::<f64>() / v.len() as f64)).collect();
    let share_512: BTreeMap<String, Option<f64>> =
        arm.manifests.iter().map(|m| (m.corpus.clone(), m.token_share_over_512)).collect();
    let share_1024: BTreeMap<String, Option<f64>> =
        arm.manifests.iter().map(|m| (m.corpus.clone(), m.token_share_over_1024)).collect();
    json!({
        "recall@1": metrics::pooled_mean(&recall_groups(o, 1, |_| true)),
        "recall@5": metrics::pooled_mean(&recall_groups(o, 5, |_| true)),
        "recall@10": bound_of(&r10, settings).map(|b| json!({"point": b.point, "lower": b.lower, "upper": b.upper})),
        "mrr": bound_of(&mrr, settings).map(|b| json!({"point": b.point, "lower": b.lower, "upper": b.upper})),
        "recall@10ByLanguage": per_language,
        "recall@10ByCorpus": by_corpus,
        "recall@10OverlapTrue": metrics::pooled_mean(&recall_groups(o, 10, |x| x.overlap)),
        "recall@10OverlapFalse": metrics::pooled_mean(&recall_groups(o, 10, |x| !x.overlap)),
        "tokenShareOver512": share_512,
        "tokenShareOver1024": share_1024,
    })
}

fn floors_and_rates(arm: &ArmRun) -> (Floors, BTreeMap<String, Option<f64>>, serde_json::Value) {
    let floors = metrics::fit_floors(&arm.outcomes);
    let languages: Vec<String> = {
        let mut l: Vec<String> = arm.outcomes.iter().map(|o| o.language.clone()).collect();
        l.sort();
        l.dedup();
        l
    };
    let false_alarm: BTreeMap<String, Option<f64>> = languages
        .iter()
        .map(|l| (l.clone(), metrics::false_alarm(&arm.outcomes, &floors, Some(l)).value()))
        .collect();
    let rate = |p| {
        let r = metrics::confident_wrong_rate(&arm.outcomes, &floors, p);
        json!({"events": r.events, "total": r.total, "rate": r.value()})
    };
    let detail = json!({
        "floors": floors,
        "falseAlarmHeldOut": false_alarm,
        "confidentWrongPositives": rate(metrics::Population::Positives),
        "confidentWrongAbsent": rate(metrics::Population::Absent),
        "confidentWrongCombined": rate(metrics::Population::Combined),
    });
    (floors, false_alarm, detail)
}

fn report(args: &ReportArgs) -> Result<()> {
    let eval_dir = &args.dir.eval_dir;
    let variants = VariantsFile::load(eval_dir)?;
    let settings = &variants.settings;
    let arms: Vec<ArmRun> =
        args.runs.iter().map(|r| load_arm(eval_dir, &variants, r)).collect::<Result<_>>()?;
    let labels = merged_labels(&arms)?;
    let reference_name = args.reference.as_deref().unwrap_or(&settings.reference);
    let reference = arms
        .iter()
        .find(|a| a.name == reference_name)
        .with_context(|| format!("the reference {reference_name} is not among the runs"))?;
    let costs: Option<BTreeMap<String, decision::Costs>> = match &args.costs {
        None => None,
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let file: CostsFile = toml::from_str(&text)?;
            Some(
                file.variants
                    .into_iter()
                    .map(|r| {
                        (
                            r.name,
                            decision::Costs {
                                pass_seconds: r.pass_seconds,
                                max_rss_bytes: r.max_rss_bytes,
                                model_bytes: r.model_bytes,
                                query_latency_ms: r.query_latency_ms,
                            },
                        )
                    })
                    .collect(),
            )
        }
    };

    let r10_bound = |arm: &ArmRun| bound_of(&recall_groups(&arm.outcomes, 10, |_| true), settings);
    let reference_r10 = r10_bound(reference).context("the reference scored no positives")?;
    let (reference_floors, _, _) = floors_and_rates(reference);

    // --- D7: controls ---------------------------------------------------------
    let mut validity_errors = Vec::new();
    let mut controls = serde_json::Map::new();
    let control_bounds: Vec<(String, Bound)> = arms
        .iter()
        .filter(|a| a.variant.role == Role::Control)
        .filter_map(|a| r10_bound(a).map(|b| (a.name.clone(), b)))
        .collect();
    for (name, bound) in &control_bounds {
        let check = metrics::broken_arm_check(reference_r10, *bound);
        if !check.passes {
            validity_errors.push(format!("reference is not clearly above broken arm {name}"));
        }
        controls.insert(name.clone(), json!({"gapPoints": check.gap_points, "ratio": check.ratio, "boundsSeparate": check.bounds_separate, "passes": check.passes}));
    }
    for arm in arms.iter().filter(|a| a.variant.arm == Arm::Random) {
        let mut chance_groups = metrics::Groups::new();
        for (language, e, n) in &arm.chance_inputs {
            chance_groups.entry(language.clone()).or_default().push(metrics::chance_recall(&[*e], *n, 10));
        }
        let chance = metrics::pooled_mean(&chance_groups).unwrap_or(0.0);
        let observed = metrics::pooled_mean(&recall_groups(&arm.outcomes, 10, |_| true)).unwrap_or(0.0);
        let ok = metrics::random_arm_is_at_chance(observed, chance);
        if !ok {
            validity_errors.push(format!("random arm recall@10 {observed:.3} exceeds 3x chance {chance:.4} + 0.02: the harness leaks order"));
        }
        controls.insert(
            format!("{}-chance", arm.name),
            json!({"observed": observed, "chance": chance, "passes": ok}),
        );
    }
    if control_bounds.is_empty() {
        validity_errors.push("no control arm among the runs".to_string());
    }
    // Harness parity #2: the reference's re-derived floors against the shipped ones.
    let mut floor_parity = serde_json::Map::new();
    for (language, fitted) in &reference_floors {
        let shipped = crate::mcp::shipped_similarity_floor(language);
        let gated = language != "typescript";
        let ok = (fitted - shipped).abs() <= 0.03 + 1e-9;
        if gated && !ok {
            validity_errors.push(format!(
                "reference floor for {language} {fitted:.2} is not within 0.03 of shipped {shipped:.2}"
            ));
        }
        floor_parity.insert(
            language.clone(),
            json!({"fitted": fitted, "shipped": shipped, "gated": gated, "passes": ok}),
        );
    }
    let reference_valid: Result<(), String> =
        if validity_errors.is_empty() { Ok(()) } else { Err(validity_errors.join("; ")) };

    // --- per-arm summaries and D9 -------------------------------------------
    let mut arms_json = serde_json::Map::new();
    let mut verdicts = Vec::new();
    for arm in &arms {
        let (floors, false_alarm, floor_detail) = floors_and_rates(arm);
        let mut entry = summary(arm, settings);
        entry["floors"] = floor_detail;
        entry["splits"] = split_summary(arm, &labels);
        entry["discordanceVsReference@10"] =
            json!(metrics::discordance(&reference.outcomes, &arm.outcomes, 10));

        if matches!(arm.variant.role, Role::Cost | Role::Quality) && arm.name != reference.name {
            let recall = metrics::paired_deltas(
                &reference.outcomes,
                &arm.outcomes,
                metrics::is_scored_positive,
                |o| Some(o.hit_at(10)),
            )?;
            let mrr = metrics::paired_deltas(
                &reference.outcomes,
                &arm.outcomes,
                metrics::is_scored_positive,
                |o| Some(o.reciprocal_rank()),
            )?;
            let (ref_floors, _, _) = floors_and_rates(reference);
            // Each arm's confident-wrong and false alarm at its own floors, paired by query.
            let paired = |indicator: fn(&Outcome, &Floors) -> Option<f64>| {
                metrics::paired_at_own_floors(
                    &reference.outcomes,
                    &ref_floors,
                    &arm.outcomes,
                    &floors,
                    indicator,
                )
            };
            let cw = paired(metrics::confident_wrong);
            let (fa, false_alarm_delta, false_alarm_report) =
                false_alarm_deltas(&reference.outcomes, &ref_floors, &arm.outcomes, &floors, settings);
            let nan = Bound { point: f64::NAN, lower: f64::NAN, upper: f64::NAN };
            let recall_bound = bound_of(&recall, settings).context("no paired positives")?;
            let evidence = decision::QualityEvidence {
                recall10_delta: recall_bound,
                mrr_delta: bound_of(&mrr, settings).context("no paired positives")?,
                recall10_delta_by_language: recall
                    .iter()
                    .map(|(l, v)| (l.clone(), v.iter().sum::<f64>() / v.len() as f64))
                    .collect(),
                confident_wrong_delta: bound_of(&cw, settings).unwrap_or(nan),
                false_alarm_delta,
                false_alarm_delta_by_language: fa
                    .iter()
                    .map(|(l, v)| (l.clone(), v.iter().sum::<f64>() / v.len() as f64))
                    .collect(),
                false_alarm_by_language: false_alarm.clone(),
            };
            let quality = decision::quality_gates(arm.variant.role, &evidence);

            // The candidate must beat the broken arms by the same margins.
            let mut valid = reference_valid.clone();
            if valid.is_ok() {
                if let Some(own) = r10_bound(arm) {
                    for (name, bound) in &control_bounds {
                        if !metrics::broken_arm_check(own, *bound).passes {
                            valid =
                                Err(format!("not clearly above broken arm {name}: presumed misconfigured"));
                        }
                    }
                }
            }
            let cost_gates = costs.as_ref().and_then(|c| {
                Some(decision::cost_gates(arm.variant.role, c.get(&arm.name)?, c.get(&reference.name)?))
            });
            let verdict =
                decision::decide(arm.variant.role, valid, recall_bound, &quality, cost_gates.as_deref());
            entry["gates"] = json!(quality
                .iter()
                .chain(cost_gates.iter().flatten())
                .map(|g| json!({"id": g.id, "passed": g.passed, "detail": g.detail}))
                .collect::<Vec<_>>());
            entry["verdict"] = json!(format!("{verdict:?}"));
            // Reported, not gated: see `false_alarm_deltas`.
            for key in ["falseAlarmDeltaByLanguage", "falseAlarmDelta"] {
                entry[key] = false_alarm_report[key].clone();
            }
            entry["splitDeltas"] = split_deltas(reference, arm, &labels, settings)?;
            verdicts.push((arm.name.clone(), verdict));
        }
        arms_json.insert(arm.name.clone(), entry);
    }

    // Ties among passing candidates: lowest pass time, then RSS, then size.
    let mut passing: Vec<&String> =
        verdicts.iter().filter(|(_, v)| *v == decision::Verdict::Pass).map(|(n, _)| n).collect();
    if let Some(c) = &costs {
        passing.sort_by(|a, b| {
            let (x, y) = (&c[a.as_str()], &c[b.as_str()]);
            x.pass_seconds
                .total_cmp(&y.pass_seconds)
                .then(x.max_rss_bytes.total_cmp(&y.max_rss_bytes))
                .then(x.model_bytes.total_cmp(&y.model_bytes))
        });
    }

    let full = json!({
        "reference": reference.name,
        "validity": {"controls": controls, "floorParity": floor_parity, "errors": validity_errors},
        "arms": arms_json,
        "winner": passing.first(),
        "settings": {"bootstrapSeed": settings.bootstrap_seed, "bootstrapResamples": settings.bootstrap_resamples},
    });
    if let Some(path) = &args.json {
        write_json(path, &full)?;
    }

    // Text summary.
    println!(
        "reference: {}  recall@10 {:.3} [{:.3}, {:.3}]",
        reference.name, reference_r10.point, reference_r10.lower, reference_r10.upper
    );
    match &reference_valid {
        Ok(()) => println!("validity: controls and floor parity pass"),
        Err(e) => println!("validity: FAILED - {e}"),
    }
    for arm in &arms {
        let e = &arms_json[&arm.name];
        println!(
            "{:<28} r@10 {}  mrr {}  verdict {}",
            arm.name,
            e["recall@10"]["point"].as_f64().map_or("-".into(), |v| format!("{v:.3}")),
            e["mrr"]["point"].as_f64().map_or("-".into(), |v| format!("{v:.3}")),
            e.get("verdict").and_then(|v| v.as_str()).unwrap_or("-"),
        );
    }
    match passing.first() {
        Some(w) => println!("winner: {w} (still subject to D10's agent-level veto)"),
        None => println!("winner: none - keep {}", reference.name),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// parity
// ---------------------------------------------------------------------------

const PARITY_TOLERANCE: f64 = 1e-4;

fn parity(args: &ParityArgs) -> Result<()> {
    let eval_dir = &args.dir.eval_dir;
    let variants = VariantsFile::load(eval_dir)?;
    let reference = variants.get(&args.variant)?;
    let dir = args.run.join(&args.corpus);
    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    if manifest.variant != reference.name {
        bail!("{} is a run of {}, not of {}", dir.display(), manifest.variant, reference.name);
    }

    let (queries, _) = queries::load(eval_dir, &args.corpus)?;
    let rankings: HashMap<String, RankingLine> =
        BufReader::new(std::fs::File::open(dir.join("rankings.jsonl"))?)
            .lines()
            .map(|l| Ok(serde_json::from_str::<RankingLine>(&l?)?))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();
    let query_vectors =
        read_vectors(&dir.join("query_vectors.bin"), manifest.query_ids.len(), manifest.dimension)?;
    let node_vectors = read_vectors(&dir.join("vectors.bin"), manifest.node_count, manifest.dimension)?;
    let snapshot_nodes = load_nodes(&snapshot_path(eval_dir, &args.corpus))?;
    let candidate_ids: Vec<&str> =
        snapshot_nodes.iter().filter(|n| n.text.is_some()).map(|n| n.id.as_str()).collect();
    let vector_of: HashMap<&str, &Vec<f32>> = candidate_ids.iter().copied().zip(&node_vectors).collect();
    let query_index: HashMap<&str, usize> =
        manifest.query_ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();

    // Production's own loader and query embedding, for the query-vector half.
    let production_model = EmbeddingModel::load(&reference.model_dir(eval_dir)?)?;
    if production_model.spec() != EncoderSpec::production() {
        bail!("EmbeddingModel::load no longer uses the production spec");
    }

    crate::storage::vectors::register_extension();
    let conn = Connection::open_with_flags(&args.index_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open {}", args.index_db.display()))?;
    let production_rows: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0))?;

    let mut selected: Vec<&Query> = queries.iter().filter(|q| !q.mechanical).collect();
    selected.sort_by(|a, b| a.id.cmp(&b.id));
    selected.truncate(args.count);

    let mut failures = Vec::new();
    for q in &selected {
        let qv = &query_vectors[query_index[q.id.as_str()]];
        let production_qv = production_model.embed(&q.text)?;
        let query_cos = f64::from(cosine_similarity(qv, &production_qv));
        if (1.0 - query_cos).abs() > PARITY_TOLERANCE {
            failures
                .push(format!("{}: query vector differs from production's (cosine {query_cos:.6})", q.id));
        }
        let production = crate::mcp::top_k_for_eval(&conn, &production_qv, 10)?;
        let harness: Vec<&(String, f64)> = rankings[&q.id].hits.iter().take(10).collect();
        let same_order = production.len() == harness.len()
            && production.iter().zip(&harness).all(|((pid, _), (hid, _))| pid == hid);
        let max_score_diff =
            production.iter().zip(&harness).map(|((_, ps), (_, hs))| (ps - hs).abs()).fold(0.0f64, f64::max);
        if !same_order || max_score_diff > PARITY_TOLERANCE {
            failures.push(format!(
                "{}: top-10 differs (same order: {same_order}, max score diff {max_score_diff:.2e})",
                q.id
            ));
        }
        for (id, _) in &production {
            let stored: Vec<u8> =
                conn.query_row("SELECT embedding FROM vectors WHERE nodeId = ?1", [id], |r| r.get(0))?;
            let stored: Vec<f32> =
                stored.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            match vector_of.get(id.as_str()) {
                Some(v) if f64::from(cosine_similarity(v, &stored)) >= 1.0 - PARITY_TOLERANCE => {}
                Some(_) => failures
                    .push(format!("{}: node {id}'s production vector differs from the harness's", q.id)),
                None => failures.push(format!(
                    "{}: production ranked node {id}, which the harness has no text for",
                    q.id
                )),
            }
        }
    }

    println!(
        "parity on {}: {} queries, production vectors {production_rows}, harness candidates {}",
        args.corpus,
        selected.len(),
        manifest.node_count
    );
    if production_rows as usize != manifest.node_count {
        failures.push(format!(
            "production embedded {production_rows} nodes, the harness ranks {}: text selection differs",
            manifest.node_count
        ));
    }
    if failures.is_empty() {
        println!("parity: PASS (top-10 order equal, scores within {PARITY_TOLERANCE:e})");
        Ok(())
    } else {
        for f in &failures {
            println!("  {f}");
        }
        bail!("parity: FAILED ({} findings)", failures.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ties go to the lower candidate index (node id ascending), as
    /// `search_code`'s `ORDER BY score DESC, id ASC`. Control: ranking
    /// ascending fails the small case. Control: dropping the
    /// `.then(a.cmp(&b))` tie-break AND switching to `sort_unstable_by`
    /// fails the large case: 600 candidates over three interleaved scores
    /// is past the length where std's unstable sort stops using insertion
    /// sort, and its partitioning reorders equal keys (std's unstable sort
    /// is deterministic, so this is reproducible for a given toolchain).
    /// Dropping only the tie-break is not observable: a stable sort of
    /// `0..n` already keeps ties in index order.
    #[test]
    fn top_hits_rank_by_score_then_candidate_order() {
        let scores = [0.2, 0.9, 0.5, 0.9, 0.1];
        assert_eq!(top_hits(&scores), vec![1, 3, 2, 0, 4]);

        let scores: Vec<f64> = (0..600).map(|i| ((i * 7) % 3) as f64).collect();
        let mut expected: Vec<usize> = (0..600).filter(|&i| scores[i] == 2.0).collect();
        expected.extend((0..600).filter(|&i| scores[i] == 1.0));
        expected.truncate(KEPT_HITS);
        assert_eq!(top_hits(&scores), expected);
    }

    #[test]
    fn top_hits_keep_at_most_the_kept_count() {
        let scores: Vec<f64> = (0..250).map(|i| i as f64).collect();
        let top = top_hits(&scores);
        assert_eq!(top.len(), KEPT_HITS);
        assert_eq!(top[0], 249);
    }

    #[test]
    fn vectors_round_trip_through_the_binary_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.bin");
        let vectors = vec![vec![1.0f32, -2.5, 3.25], vec![0.0, 0.5, -0.125]];
        write_vectors(&path, &vectors).unwrap();
        assert_eq!(read_vectors(&path, 2, 3).unwrap(), vectors);
        assert!(read_vectors(&path, 3, 3).is_err());
    }

    #[test]
    fn first_paragraph_stops_at_the_first_blank_line() {
        // Control: returning `doc.trim()` unchanged fails the first two.
        assert_eq!(
            first_paragraph("Summary line\ncontinued.\n\nDetails.\n\nMore."),
            "Summary line\ncontinued."
        );
        assert_eq!(first_paragraph("  Summary.\n   \t\nDetails."), "Summary.");
        assert_eq!(first_paragraph("One paragraph\nonly.\n"), "One paragraph\nonly.");
        let node = Node {
            id: "n".into(),
            kind: "Function".into(),
            name: "f".into(),
            qualified_name: "f".into(),
            file_path: "f.rs".into(),
            language: "rust".into(),
            native_kind: Some("function".into()),
            container: None,
            start_line: 1,
            end_line: 1,
            text: full_text(Some("Short.\n\nLong tail."), Some("fn f()")),
            doc: Some("Short.\n\nLong tail.".into()),
            signature: Some("fn f()".into()),
        };
        assert_eq!(node.text_for(TextForm::Full).as_deref(), Some("Short.\n\nLong tail.\n\nfn f()"));
        assert_eq!(node.text_for(TextForm::FirstParagraph).as_deref(), Some("Short.\n\nfn f()"));
    }

    fn node(doc: Option<&str>, signature: Option<&str>) -> Node {
        Node {
            id: "n".into(),
            kind: "Function".into(),
            name: "f".into(),
            qualified_name: "f".into(),
            file_path: "f.rs".into(),
            language: "rust".into(),
            native_kind: Some("function".into()),
            container: None,
            start_line: 1,
            end_line: 1,
            text: full_text(doc, signature),
            doc: doc.map(Into::into),
            signature: signature.map(Into::into),
        }
    }

    /// The structured form trims the doc and appends the signature in
    /// `text_to_embed`'s layout, and is `Some` exactly when `text` is.
    /// Control: making the `Structured` arm return `self.text.clone()` fails
    /// the first assertion; dropping `embedding::text::text_to_embed`'s
    /// `.or_else(..)` fallback fails the code-only one.
    #[test]
    fn structured_form_trims_the_doc_before_the_signature() {
        let doc = "Short.\n\n# Examples\n\n```\nf();\n```\n\n@param x y";
        assert_eq!(
            node(Some(doc), Some("fn f()")).text_for(TextForm::Structured).as_deref(),
            Some("Short.\n\nfn f()")
        );
        // Absent doc: the signature alone, as in the full form.
        assert_eq!(node(None, Some("fn f()")).text_for(TextForm::Structured).as_deref(), Some("fn f()"));
        // A doc of only code and no signature falls back to the full text
        // rather than dropping the node from the candidate set.
        let code = "```\nf();\n```";
        assert_eq!(node(Some(code), None).text_for(TextForm::Structured).as_deref(), Some(code));
        // A doc of only code with a signature: the signature alone.
        assert_eq!(
            node(Some(code), Some("fn f()")).text_for(TextForm::Structured).as_deref(),
            Some("fn f()")
        );
        assert_eq!(node(None, None).text_for(TextForm::Structured), None);
    }

    /// The eval's `structured` form is the text production embeds, node for
    /// node, on the real and written docs of `embedding::text`'s fixture; and
    /// `full` is still the untrimmed text the stored `full` runs embedded.
    /// Control: giving the `Structured` arm its own trim (e.g.
    /// `full_text(self.doc.as_deref().map(first_paragraph), ..)`) fails the
    /// first two assertions; making `load_nodes`/`node` fill `text` with
    /// `text_to_embed` fails the third.
    #[test]
    fn the_structured_form_is_the_production_text() {
        for case in crate::embedding::text::tests::fixture() {
            let (doc, signature, label) = (case.doc.as_deref(), case.signature.as_deref(), case.label());
            let node = node(doc, signature);
            assert_eq!(node.text_for(TextForm::Structured), text_to_embed(doc, signature), "{label}");
            assert_eq!(node.text_for(TextForm::Structured), case.expected, "{label}");
            assert_eq!(node.text_for(TextForm::Full), full_text(doc, signature), "{label}");
        }
    }

    /// `parity`'s default variant embeds what production embeds: the
    /// production encoder spec, the pinned revision and int8 weights file, the
    /// production text form, no context header and no prefixes. Control:
    /// defaulting `--variant` to `settings.reference` (fp32, full text) fails
    /// the weights-file and text-form assertions.
    #[test]
    fn parity_defaults_to_the_production_configuration() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            parity: ParityArgs,
        }
        let args = Cli::parse_from(["parity", "--index-db", "i.sqlite", "--run", "r"]).parity;
        let file = VariantsFile::parse(include_str!("../../../eval/embedding/variants.toml")).unwrap();
        let variant = file.get(&args.variant).unwrap();
        assert_eq!(variant.encoder_spec().unwrap(), crate::embedding::model::EncoderSpec::production());
        assert_eq!(variant.revision.as_deref(), Some(crate::cli::model::MODEL_REVISION));
        assert_eq!(variant.onnx_file.as_deref(), Some(crate::embedding::model::DEFAULT_ONNX_REMOTE_PATH));
        assert_eq!(variant.text, TextForm::Structured);
        assert_eq!(variant.context, ContextForm::None);
        assert_eq!((variant.query_prefix.as_str(), variant.document_prefix.as_str()), ("", ""));
    }

    /// A default `text` leaves the fingerprint of every run made before the
    /// field existed unchanged. Control: hashing `format!("{variant:?}")`
    /// fails the first assertion (and `report` then refuses stored runs as
    /// "run with a different definition").
    #[test]
    fn default_text_form_keeps_earlier_fingerprints() {
        let file = VariantsFile::parse(
            r#"
            [settings]
            reference = "r"
            bootstrap_seed = 1
            bootstrap_resamples = 10
            arm_seed = 1

            [[variant]]
            name = "r"
            arm = "model"
            role = "reference"
            pooling = "mean"
            dimension = 768
            max_tokens = 1024

            [[variant]]
            name = "p"
            arm = "model"
            role = "cost"
            pooling = "mean"
            dimension = 768
            max_tokens = 1024
            text = "first-paragraph"

            [[variant]]
            name = "s"
            arm = "model"
            role = "cost"
            pooling = "mean"
            dimension = 768
            max_tokens = 1024
            text = "structured"

            [[variant]]
            name = "n"
            arm = "model"
            role = "quality"
            pooling = "mean"
            dimension = 768
            max_tokens = 1024
            text = "first-paragraph"
            context = "none"

            [[variant]]
            name = "c"
            arm = "model"
            role = "quality"
            pooling = "mean"
            dimension = 768
            max_tokens = 1024
            context = "path-parent"
            "#,
        )
        .unwrap();
        let plain = fingerprint_text(file.get("r").unwrap());
        assert!(!plain.contains("text:"), "{plain}");
        assert!(plain.ends_with("reference: None }"), "{plain}");
        let cut = fingerprint_text(file.get("p").unwrap());
        assert!(cut.ends_with("text: FirstParagraph }"), "{cut}");
        let structured = fingerprint_text(file.get("s").unwrap());
        assert!(structured.ends_with("text: Structured }"), "{structured}");
        let explicit_none = fingerprint_text(file.get("n").unwrap());
        assert!(!explicit_none.contains("context:"), "{explicit_none}");
        assert!(explicit_none.ends_with("text: FirstParagraph }"), "{explicit_none}");
        let context = fingerprint_text(file.get("c").unwrap());
        assert!(context.ends_with("text: Full, context: PathParent }"), "{context}");
    }

    /// Controls: counting a whitespace-only doc as documented makes the
    /// second query `mixed`; dropping the parent names from the tokens makes
    /// the third `false`.
    #[test]
    fn query_labels_split_on_docs_and_path_or_parent_words() {
        use context::test_node;
        let mut nodes = vec![
            test_node("1", "Type", "struct", "m::Session", "src/transport/pool.rs", "rust", None),
            test_node(
                "2",
                "Function",
                "method",
                "m::Session::send",
                "src/transport/pool.rs",
                "rust",
                Some("fn send()"),
            ),
            test_node("3", "Function", "function", "m::free", "src/misc.rs", "rust", Some("fn free()")),
        ];
        nodes[1].doc = Some("Sends it.".into());
        nodes[2].doc = Some("  \n ".into());
        let parents = context::parents(&nodes);
        assert_eq!(query_labels("anything", &[], &nodes, &parents), (None, None));
        assert_eq!(
            query_labels("send over the transport", &[1], &nodes, &parents),
            (Some(TargetDoc::Doc), Some(true))
        );
        assert_eq!(query_labels("send", &[1, 2], &nodes, &parents), (Some(TargetDoc::Mixed), Some(false)));
        assert_eq!(query_labels("free", &[2], &nodes, &parents), (Some(TargetDoc::Sig), Some(false)));
        assert_eq!(query_labels("the session sends", &[1], &nodes, &parents).1, Some(true));
    }

    /// The fingerprints of the stored int8 and first-paragraph runs
    /// (`variantFingerprint` in their manifests), so neither the `context`
    /// field nor anything else invalidates them. Control: removing the
    /// `context: None` strip in `fingerprint_text` fails both.
    #[test]
    fn stored_runs_keep_their_fingerprints() {
        let file = VariantsFile::parse(include_str!("../../../eval/embedding/variants.toml")).unwrap();
        let fp = |name: &str| fingerprint(file.get(name).unwrap());
        assert_eq!(
            fp("jina-v2-base-code-int8"),
            "d560cf97145365a4b8ba7d04a3a8c2bbecf1ba5e7995a1a97acfc6b15fa292c1"
        );
        assert_eq!(
            fp("jina-v2-base-code-int8-first-paragraph"),
            "d2c1917980ead9fb44ddecaad600d9b881f18b5bfc84f7495ae7f30de6a822af"
        );
    }

    /// Words are permuted, not dropped or altered. Control: returning the
    /// text unshuffled fails the inequality for this seed.
    #[test]
    fn shuffling_words_keeps_the_multiset_of_words() {
        let text = "Returns the charset declared in the Content-Type header of a response";
        let shuffled = shuffle_words(text, &mut Rng::new(398));
        let mut a: Vec<&str> = text.split_whitespace().collect();
        let mut b: Vec<&str> = shuffled.split_whitespace().collect();
        assert_ne!(a, b);
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
    }

    #[test]
    fn median_of_even_and_odd_counts() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[]), None);
    }

    /// Q5 report fields (`falseAlarmDelta`, `falseAlarmDeltaByLanguage`).
    /// Floors 0.5 for both arms and languages; all queries held-out positives.
    /// A rank-1 query is a false alarm (1) below the floor, else 0; a rank-2
    /// query has no false-alarm value but is confident-wrong (1) above it.
    ///   go   g1 R .7 -> C .4: fa +1      g2 R .7 -> C .7: fa 0
    ///        g3 rank 2, R .4 -> C .7: cw +1, no fa
    ///   rust r1..r3 R .4 -> C .7: fa -1 each
    ///        r4 rank 2, R .7 -> C .4: cw -1, no fa
    /// fa: go [1, 0] (n 2), rust [-1, -1, -1] (n 3).
    /// go: point .5; a resample's mean is 0, .5 or 1 with p 1/4, 1/2, 1/4, so
    ///   the 5th/95th percentiles are 0 and 1. rust: constant, all bounds -1.
    /// pooled = mean of language means = (.5 + -1) / 2 = -.25; resamples are
    ///   (m_go - 1) / 2 in {-.5, -.25, 0}, p 1/4 each at the ends: -.5 and 0.
    /// Controls (each must fail this test):
    /// (a) `metrics::false_alarm_indicator` -> `metrics::confident_wrong` in
    ///     `false_alarm_deltas` (cw is go [0, 0, 1], rust [0, 0, 0, -1]).
    /// (b) a wrong key: `l.clone()` -> `format!("{l}x")`, or keying each
    ///     entry by the other language.
    /// (c) the pooled bound from one language only
    ///     (`bound_of(&BTreeMap::from([fa.first_key_value()...]))`), or each
    ///     language's entry reporting `pooled` instead of its own `b`.
    #[test]
    fn false_alarm_report_gives_pooled_and_per_language_q5_bounds() {
        let q = |id: &str, language: &str, rank: usize, score: f64| Outcome {
            query_id: id.to_string(),
            corpus: "c".to_string(),
            language: language.to_string(),
            positive: true,
            mechanical: false,
            overlap: false,
            held_out: true,
            first_expected_rank: Some(rank),
            top_score: Some(score),
            top_language: Some(language.to_string()),
        };
        let reference = vec![
            q("g1", "go", 1, 0.7),
            q("g2", "go", 1, 0.7),
            q("g3", "go", 2, 0.4),
            q("r1", "rust", 1, 0.4),
            q("r2", "rust", 1, 0.4),
            q("r3", "rust", 1, 0.4),
            q("r4", "rust", 2, 0.7),
        ];
        let candidate = vec![
            q("g1", "go", 1, 0.4),
            q("g2", "go", 1, 0.7),
            q("g3", "go", 2, 0.7),
            q("r1", "rust", 1, 0.7),
            q("r2", "rust", 1, 0.7),
            q("r3", "rust", 1, 0.7),
            q("r4", "rust", 2, 0.4),
        ];
        let floors: Floors = [("go".to_string(), 0.5), ("rust".to_string(), 0.5)].into();
        let settings = config::Settings {
            reference: "r".to_string(),
            bootstrap_seed: 398,
            bootstrap_resamples: 2000,
            arm_seed: 1,
        };
        let (groups, pooled, report) =
            false_alarm_deltas(&reference, &floors, &candidate, &floors, &settings);
        let expected_groups: metrics::Groups =
            [("go".to_string(), vec![1.0, 0.0]), ("rust".to_string(), vec![-1.0, -1.0, -1.0])].into();
        assert_eq!(groups, expected_groups);
        assert_eq!(pooled, Bound { point: -0.25, lower: -0.5, upper: 0.0 });
        assert_eq!(report["falseAlarmDelta"], json!({"point": -0.25, "lower": -0.5, "upper": 0.0}));
        assert_eq!(
            report["falseAlarmDeltaByLanguage"],
            json!({
                "go": {"n": 2, "point": 0.5, "lower": 0.0, "upper": 1.0},
                "rust": {"n": 3, "point": -1.0, "lower": -1.0, "upper": -1.0},
            })
        );
    }
}
