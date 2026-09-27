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
//! - `run`: embed every node text `text_to_embed` produces (the crate's own
//!   function, which is why this lives in the crate) and every query with one
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
mod config;
mod decision;
mod metrics;
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
use crate::embedding::pipeline::text_to_embed;

use config::{Arm, CorporaFile, Role, Variant, VariantsFile};
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
}

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
    /// The reference variant's run directory.
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

/// One node of the snapshot. `text` is `text_to_embed`'s output; only nodes
/// with text are ranking candidates, exactly as the `vectors` join makes them
/// in `search_code`.
struct Node {
    id: String,
    kind: String,
    qualified_name: String,
    file_path: String,
    language: String,
    text: Option<String>,
}

fn load_nodes(db: &Path) -> Result<Vec<Node>> {
    let conn =
        Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("failed to open {}", db.display()))?;
    let mut stmt = conn.prepare(
        "SELECT id, kind, qualifiedName, filePath, language, docComment, signature FROM nodes ORDER BY id",
    )?;
    let nodes = stmt
        .query_map([], |row| {
            let doc: Option<String> = row.get(5)?;
            let signature: Option<String> = row.get(6)?;
            Ok(Node {
                id: row.get(0)?,
                kind: row.get(1)?,
                qualified_name: row.get(2)?,
                file_path: row.get(3)?,
                language: row.get(4)?,
                text: text_to_embed(doc.as_deref(), signature.as_deref()),
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
    hex(&Sha256::digest(format!("{variant:?}").as_bytes()))
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

fn token_shares(model_dir: &Path, texts: &[&str]) -> Result<(f64, f64)> {
    let path = model_dir.join(TOKENIZER_FILE_NAME);
    let mut tokenizer = tokenizers::Tokenizer::from_file(&path)
        .map_err(|err| anyhow::anyhow!("failed to load tokenizer {}: {err}", path.display()))?;
    tokenizer.with_padding(None);
    tokenizer
        .with_truncation(None)
        .map_err(|err| anyhow::anyhow!("failed to clear tokenizer truncation: {err}"))?;
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
    let candidates: Vec<usize> = (0..nodes.len()).filter(|&i| nodes[i].text.is_some()).collect();
    let candidate_of: HashMap<usize, usize> = candidates.iter().enumerate().map(|(c, &i)| (i, c)).collect();
    let texts: Vec<&str> = candidates.iter().map(|&i| nodes[i].text.as_deref().unwrap()).collect();
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
                texts
                    .iter()
                    .map(|t| model.embed(&format!("{}{t}", base.document_prefix)))
                    .collect::<Result<Vec<_>>>()?
            }
            Arm::WordsShuffled => {
                let (base, _, model) = encoder.unwrap();
                texts
                    .iter()
                    .map(|t| {
                        model.embed(&format!("{}{}", base.document_prefix, shuffle_words(t, &mut arm_rng)))
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
        queries.iter().map(|q| index.scores(&q.text)).collect()
    } else {
        let query_vectors: Vec<Vec<f32>> = if same_queries && dir.join("query_vectors.bin").exists() {
            read_vectors(&dir.join("query_vectors.bin"), queries.len(), dimension)?
        } else {
            let vectors = match variant.arm {
                Arm::Model | Arm::WordsShuffled => {
                    let (base, _, model) = encoder.unwrap();
                    let mut out = Vec::with_capacity(queries.len());
                    for q in &queries {
                        let t = Instant::now();
                        out.push(model.embed(&format!("{}{}", base.query_prefix, q.text))?);
                        timings.query_embed_ms.push(t.elapsed().as_secs_f64() * 1000.0);
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
        query_vectors
            .iter()
            .map(|qv| node_vectors.iter().map(|nv| f64::from(cosine_similarity(qv, nv))).collect())
            .collect()
    };

    let mut out = std::io::BufWriter::new(std::fs::File::create(dir.join("rankings.jsonl"))?);
    for ((q, scores), expected) in queries.iter().zip(&scores_per_query).zip(&expected) {
        let top = top_hits(scores);
        let line = RankingLine {
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
    Ok(ArmRun { name, variant, outcomes, manifests, chance_inputs })
}

fn bound_of(groups: &metrics::Groups, settings: &config::Settings) -> Option<Bound> {
    metrics::bootstrap(groups, settings.bootstrap_resamples, settings.bootstrap_seed)
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
    let reference = arms
        .iter()
        .find(|a| a.name == settings.reference)
        .with_context(|| format!("the reference {} is not among the runs", settings.reference))?;
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
        entry["discordanceVsReference@10"] =
            json!(metrics::discordance(&reference.outcomes, &arm.outcomes, 10));

        if matches!(arm.variant.role, Role::Cost | Role::Quality) {
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
            let fa = paired(metrics::false_alarm_indicator);
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
                false_alarm_delta: bound_of(&fa, settings).unwrap_or(nan),
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
    let reference = variants.get(&variants.settings.reference)?;
    let dir = args.run.join(&args.corpus);
    let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
    if manifest.variant != reference.name {
        bail!("{} is a run of {}, not of the reference {}", dir.display(), manifest.variant, reference.name);
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
            "production embedded {production_rows} nodes, the harness ranks {}: text_to_embed selection differs",
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
    /// `search_code`'s `ORDER BY score DESC, id ASC`. Control: dropping the
    /// `.then(a.cmp(&b))` tie-break with an unstable sort can reorder the
    /// tied pair; ranking ascending fails the first element.
    #[test]
    fn top_hits_rank_by_score_then_candidate_order() {
        let scores = [0.2, 0.9, 0.5, 0.9, 0.1];
        assert_eq!(top_hits(&scores), vec![1, 3, 2, 0, 4]);
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
}
