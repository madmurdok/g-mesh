//! `debug-embed-eval schedule`: replays an embedding order over a stored run.
//!
//! The run's vectors do not depend on when a node was embedded, so ranking
//! every query over the prefix of an order the backfill would have embedded
//! gives the partial pages a user would have seen at that point. Each arm
//! orders the candidates, a cost curve (`cost calibrate`) turns the prefix
//! length into seconds, and at checkpoints (every 5% of the pass cost, plus
//! the end of each tier) the queries are ranked over the embedded prefix.
//!
//! Tiers come from `embedding::text::embed_tier`, so the replay classifies
//! nodes exactly as the backfill does. The command never reranks: the
//! rankings of the 100% checkpoint go to `eval/embedding/gm441_rerank.py`.
//!
//! Output, per arm and corpus, under `--out`:
//! `<arm>/<corpus>/schedule.json`, `<arm>/<corpus>/rankings.jsonl` (the 100%
//! checkpoint) and `<arm>/<corpus>/checkpoints/<embedded>/rankings.jsonl`.

use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use serde::Serialize;

use super::config::{Arm, CorporaFile, EmbedFilter, VariantsFile};
use super::cost::{predicted_ms, Curve};
use super::metrics::{self, Outcome, KEPT_HITS};
use super::queries::{self, Query};
use super::{
    candidate_texts, compare, plain_tokenizer, read_json, read_vectors, resolve_expected, sha256_file,
    snapshot_path, verified_snapshot, write_json, EvalDir, Manifest, RankingLine,
};
use crate::embedding::model::{cosine_similarity, ONNX_FILE_NAME};
use crate::embedding::text::{embed_tier, EmbedInput, EmbedTier};

#[derive(Debug, Args)]
pub struct ScheduleArgs {
    #[command(flatten)]
    pub dir: EvalDir,
    /// The directory holding `<variant>/<corpus>` run directories.
    #[arg(long)]
    pub run_root: std::path::PathBuf,
    #[arg(long)]
    pub variant: String,
    /// `noise` masks the nodes production's noise rule leaves out; `none`
    /// replays the run's whole candidate set.
    #[arg(long, value_enum, default_value = "none")]
    pub filter: ScheduleFilter,
    /// The `cost calibrate` curve that turns a prefix into seconds.
    #[arg(long)]
    pub curve: std::path::PathBuf,
    /// Orders to replay; all of them when omitted.
    #[arg(long, value_enum)]
    pub arm: Vec<ScheduleArm>,
    /// Corpora to replay; all of corpora.toml when omitted.
    #[arg(long)]
    pub corpus: Vec<String>,
    #[arg(long)]
    pub out: std::path::PathBuf,
    /// Write `rankings.jsonl` for the 100% checkpoint only.
    #[arg(long)]
    pub final_rankings_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScheduleFilter {
    None,
    Noise,
}

/// The order a pass embeds the candidates in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScheduleArm {
    /// Ascending node id: the order before tiers.
    Id,
    /// Public, private, then test nodes; ascending id within a tier.
    Tier,
    /// Nodes that are some scored query's answer first (an upper bound).
    Oracle,
    /// Test, private, then public nodes (the control).
    TierReversed,
}

impl ScheduleArm {
    const ALL: [ScheduleArm; 4] =
        [ScheduleArm::Id, ScheduleArm::Tier, ScheduleArm::Oracle, ScheduleArm::TierReversed];

    fn name(self) -> &'static str {
        match self {
            ScheduleArm::Id => "id",
            ScheduleArm::Tier => "tier",
            ScheduleArm::Oracle => "oracle",
            ScheduleArm::TierReversed => "tier-reversed",
        }
    }
}

/// The candidate indices in the order `arm` embeds them. Every order is
/// stable, so ties keep ascending index.
fn order(arm: ScheduleArm, tiers: &[EmbedTier], gold: &[bool]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..tiers.len()).collect();
    match arm {
        ScheduleArm::Id => {}
        ScheduleArm::Tier => order.sort_by_key(|&i| tiers[i]),
        ScheduleArm::TierReversed => order.sort_by_key(|&i| std::cmp::Reverse(tiers[i])),
        ScheduleArm::Oracle => order.sort_by_key(|&i| !gold[i]),
    }
    order
}

/// Prefix lengths to rank at: the first prefix whose cost reaches each 5% of
/// the pass, and every `extra` length (tier ends). `cumulative[k]` is the
/// cost of the first `k` nodes. Ascending, without duplicates.
fn checkpoint_lengths(cumulative: &[f64], extra: &[usize]) -> Vec<usize> {
    let total = cumulative.last().copied().unwrap_or(0.0);
    let mut lengths: Vec<usize> = (1..=20)
        .map(|step| {
            let target = total * f64::from(step) / 20.0;
            cumulative.partition_point(|&c| c < target).min(cumulative.len() - 1)
        })
        .collect();
    lengths.extend_from_slice(extra);
    lengths.retain(|&k| k > 0);
    lengths.sort_unstable();
    lengths.dedup();
    lengths
}

/// The best `KEPT_HITS` of `embedded` (indices into `scores`), best first,
/// ties by ascending index, as `top_hits` orders a full pass.
fn top_embedded(scores: &[f64], embedded: &[usize]) -> Vec<usize> {
    let mut picked = embedded.to_vec();
    let by_score = |a: &usize, b: &usize| scores[*b].total_cmp(&scores[*a]).then(a.cmp(b));
    if picked.len() > KEPT_HITS {
        picked.select_nth_unstable_by(KEPT_HITS, by_score);
        picked.truncate(KEPT_HITS);
    }
    picked.sort_by(by_score);
    picked
}

#[derive(Debug, Serialize)]
struct Checkpoint {
    /// Seconds into the pass.
    t: f64,
    /// Share of the pass cost.
    frac: f64,
    embedded: usize,
    /// Distinct answer nodes of scored queries that are embedded.
    gold_embedded: usize,
    recall10: Option<f64>,
    mrr: Option<f64>,
    /// Whether a rerank applies at this checkpoint.
    reranked: bool,
}

#[derive(Debug, Serialize)]
struct TierEnd {
    nodes: usize,
    embedded: usize,
    t: f64,
    frac: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleFile {
    arm: &'static str,
    variant: String,
    corpus: String,
    filter: &'static str,
    /// Candidates after the filter.
    nodes: usize,
    /// Candidates of the stored run.
    run_nodes: usize,
    gold_total: usize,
    /// The stored run's measured node-embedding time.
    measured_pass_s: f64,
    /// The curve's prediction for the whole stored run.
    predicted_run_pass_s: f64,
    /// The curve's prediction for the pass after the filter, rescaled to the
    /// measured run: `t` at 100%.
    pass_s: f64,
    tier_ends: BTreeMap<&'static str, TierEnd>,
    checkpoints: Vec<Checkpoint>,
}

/// One corpus's stored run, masked and ready to be ranked at any prefix.
struct Replay {
    queries: Vec<Query>,
    /// Node id of each kept candidate, in candidate order.
    ids: Vec<String>,
    tiers: Vec<EmbedTier>,
    /// Predicted embedding milliseconds of each kept candidate.
    cost_ms: Vec<f64>,
    /// `scores[q][m]`: cosine of query `q` and kept candidate `m`.
    scores: Vec<Vec<f64>>,
    /// Kept candidates each query expects, ascending.
    expected: Vec<Vec<usize>>,
    run_nodes: usize,
    predicted_run_ms: f64,
    measured_s: f64,
}

pub fn run(args: &ScheduleArgs) -> Result<()> {
    let eval_dir = &args.dir.eval_dir;
    let corpora = CorporaFile::load(eval_dir)?;
    let variants = VariantsFile::load(eval_dir)?;
    let variant = variants.get(&args.variant)?;
    if variant.arm != Arm::Model {
        bail!("schedule replays model arms; {} is {:?}", variant.name, variant.arm);
    }
    let curve: Curve = read_json(&args.curve)?;
    let model_dir = variant.model_dir(eval_dir)?;
    let sha = sha256_file(&model_dir.join(ONNX_FILE_NAME))?;
    if sha != curve.onnx_sha256 {
        bail!("{} runs {}, not the model the curve was calibrated on", variant.name, model_dir.display());
    }
    let tokenizer = plain_tokenizer(&model_dir)?;
    let max_tokens = variant.encoder_spec()?.max_sequence_length;
    let arms: Vec<ScheduleArm> =
        if args.arm.is_empty() { ScheduleArm::ALL.to_vec() } else { args.arm.clone() };
    let corpus_ids: Vec<String> = if args.corpus.is_empty() {
        corpora.corpora.iter().map(|c| c.id.clone()).collect()
    } else {
        args.corpus.clone()
    };

    for corpus in &corpus_ids {
        let snapshot_sha = verified_snapshot(eval_dir, &corpora, corpus)?;
        let nodes = super::load_nodes(&snapshot_path(eval_dir, corpus))?;
        let (candidates, texts) = candidate_texts(&nodes, variant, variants.settings.arm_seed);
        let dir = args.run_root.join(&variant.name).join(corpus);
        let manifest: Manifest = read_json(&dir.join("manifest.json"))?;
        let ids_sha = compare::node_ids_sha256(candidates.iter().map(|&i| nodes[i].id.as_str()));
        if manifest.snapshot_sha256 != snapshot_sha || manifest.node_ids_sha256 != ids_sha {
            bail!("{} does not match the snapshot of {corpus}; re-run it", dir.display());
        }

        let costs: Vec<f64> = texts
            .iter()
            .map(|t| {
                let n = tokenizer
                    .encode(format!("{}{t}", variant.document_prefix), true)
                    .map_err(|err| anyhow::anyhow!("failed to tokenize: {err}"))?
                    .get_ids()
                    .len();
                Ok(predicted_ms(&[n], max_tokens, &curve.fit))
            })
            .collect::<Result<_>>()?;
        let timings: serde_json::Value = read_json(&dir.join("timings.json"))?;
        let measured_s =
            timings["embedNodesMs"].as_f64().context("timings.json has no embedNodesMs")? / 1000.0;

        let (queries, query_files) = queries::load(eval_dir, corpus)?;
        if manifest.query_ids != queries.iter().map(|q| q.id.clone()).collect::<Vec<_>>()
            || manifest.query_files != query_files
        {
            bail!("the queries of {corpus} changed since {} was run", dir.display());
        }
        let candidate_of: std::collections::HashMap<usize, usize> =
            candidates.iter().enumerate().map(|(c, &i)| (i, c)).collect();
        let resolved = resolve_expected(&queries, &nodes, &candidate_of, variant.filter)?;
        let node_vectors = read_vectors(&dir.join("vectors.bin"), candidates.len(), manifest.dimension)?;
        let query_vectors = read_vectors(&dir.join("query_vectors.bin"), queries.len(), manifest.dimension)?;

        let kept: Vec<usize> = (0..candidates.len())
            .filter(|&c| {
                args.filter == ScheduleFilter::None || !nodes[candidates[c]].filtered(EmbedFilter::Noise)
            })
            .collect();
        let mut kept_of = vec![None; candidates.len()];
        for (m, &c) in kept.iter().enumerate() {
            kept_of[c] = Some(m);
        }
        let replay = Replay {
            ids: kept.iter().map(|&c| nodes[candidates[c]].id.clone()).collect(),
            tiers: kept
                .iter()
                .map(|&c| {
                    let n = &nodes[candidates[c]];
                    let input = EmbedInput {
                        kind: &n.kind,
                        name: &n.name,
                        qualified_name: &n.qualified_name,
                        file_path: &n.file_path,
                        doc: n.doc.as_deref(),
                        signature: n.signature.as_deref(),
                        generated: false,
                    };
                    embed_tier(&input, &n.visibility)
                })
                .collect(),
            cost_ms: kept.iter().map(|&c| costs[c]).collect(),
            scores: query_vectors
                .iter()
                .map(|qv| kept.iter().map(|&c| f64::from(cosine_similarity(qv, &node_vectors[c]))).collect())
                .collect(),
            expected: resolved.iter().map(|r| r.hits.iter().filter_map(|&c| kept_of[c]).collect()).collect(),
            queries,
            run_nodes: candidates.len(),
            predicted_run_ms: costs.iter().sum(),
            measured_s,
        };
        for &arm in &arms {
            replay_arm(args, &variant.name, corpus, &replay, arm)?;
        }
    }
    Ok(())
}

fn replay_arm(
    args: &ScheduleArgs,
    variant: &str,
    corpus: &str,
    replay: &Replay,
    arm: ScheduleArm,
) -> Result<()> {
    let n = replay.ids.len();
    let mut is_gold = vec![false; n];
    for (q, expected) in replay.queries.iter().zip(&replay.expected) {
        if q.positive() && !q.mechanical {
            for &m in expected {
                is_gold[m] = true;
            }
        }
    }
    let order = order(arm, &replay.tiers, &is_gold);
    let mut position = vec![0usize; n];
    let mut cumulative = vec![0.0; n + 1];
    for (p, &m) in order.iter().enumerate() {
        position[m] = p;
        cumulative[p + 1] = cumulative[p] + replay.cost_ms[m];
    }
    let total_ms = cumulative[n];
    let seconds = |k: usize| cumulative[k] / replay.predicted_run_ms * replay.measured_s;

    let mut tier_ends = BTreeMap::new();
    let mut tier_lengths = Vec::new();
    for tier in EmbedTier::ALL {
        let members: Vec<usize> = (0..n).filter(|&m| replay.tiers[m] == tier).collect();
        if let Some(last) = members.iter().map(|&m| position[m] + 1).max() {
            tier_lengths.push(last);
            tier_ends.insert(
                tier.as_str(),
                TierEnd {
                    nodes: members.len(),
                    embedded: last,
                    t: seconds(last),
                    frac: cumulative[last] / total_ms,
                },
            );
        }
    }

    let dir = args.out.join(arm.name()).join(corpus);
    std::fs::create_dir_all(&dir)?;
    let mut checkpoints = Vec::new();
    for k in checkpoint_lengths(&cumulative, &tier_lengths) {
        let embedded: Vec<usize> = (0..n).filter(|&m| position[m] < k).collect();
        let last = k == n;
        let path = if last {
            dir.join("rankings.jsonl")
        } else if args.final_rankings_only {
            std::path::PathBuf::new()
        } else {
            dir.join("checkpoints").join(format!("{k:06}")).join("rankings.jsonl")
        };
        let outcomes = rank_checkpoint(replay, &embedded, &path)?;
        if last && !args.final_rankings_only {
            let copy = dir.join("checkpoints").join(format!("{k:06}")).join("rankings.jsonl");
            std::fs::create_dir_all(copy.parent().unwrap())?;
            std::fs::copy(&path, &copy)?;
        }
        let r10 = metrics::group_by_language(&outcomes, metrics::is_scored_positive, |o| o.hit_at(10));
        let mrr =
            metrics::group_by_language(&outcomes, metrics::is_scored_positive, Outcome::reciprocal_rank);
        checkpoints.push(Checkpoint {
            t: seconds(k),
            frac: cumulative[k] / total_ms,
            embedded: k,
            gold_embedded: (0..n).filter(|&m| is_gold[m] && position[m] < k).count(),
            recall10: metrics::pooled_mean(&r10),
            mrr: metrics::pooled_mean(&mrr),
            reranked: last,
        });
    }
    let n_checkpoints = checkpoints.len();
    write_json(
        &dir.join("schedule.json"),
        &ScheduleFile {
            arm: arm.name(),
            variant: variant.to_string(),
            corpus: corpus.to_string(),
            filter: if args.filter == ScheduleFilter::Noise { "noise" } else { "none" },
            nodes: n,
            run_nodes: replay.run_nodes,
            gold_total: is_gold.iter().filter(|&&g| g).count(),
            measured_pass_s: replay.measured_s,
            predicted_run_pass_s: replay.predicted_run_ms / 1000.0,
            pass_s: seconds(n),
            tier_ends,
            checkpoints,
        },
    )?;
    eprintln!("{}/{corpus}: {n_checkpoints} checkpoints", arm.name());
    Ok(())
}

/// Ranks every query over `embedded`, writes the rankings to `path` (not at
/// all when `path` is empty) and returns the outcomes.
fn rank_checkpoint(replay: &Replay, embedded: &[usize], path: &Path) -> Result<Vec<Outcome>> {
    let mut out = if path.as_os_str().is_empty() {
        None
    } else {
        std::fs::create_dir_all(path.parent().unwrap())?;
        Some(BufWriter::new(std::fs::File::create(path)?))
    };
    let mut outcomes = Vec::with_capacity(replay.queries.len());
    for (q, (scores, expected)) in replay.queries.iter().zip(replay.scores.iter().zip(&replay.expected)) {
        let top = top_embedded(scores, embedded);
        outcomes.push(Outcome {
            query_id: q.id.clone(),
            corpus: q.corpus.clone(),
            language: q.language.clone(),
            positive: q.positive(),
            mechanical: q.mechanical,
            overlap: q.overlap(),
            held_out: q.held_out(),
            first_expected_rank: top.iter().position(|m| expected.contains(m)).map(|p| p + 1),
            top_score: top.first().map(|&m| scores[m]),
            top_language: None,
        });
        if let Some(out) = out.as_mut() {
            let line = RankingLine {
                id: q.id.clone(),
                expected: expected.iter().map(|&m| replay.ids[m].clone()).collect(),
                hits: top.iter().map(|&m| (replay.ids[m].clone(), scores[m])).collect(),
                top_language: None,
                target_doc: None,
                path_overlap: None,
            };
            writeln!(out, "{}", serde_json::to_string(&line)?)?;
        }
    }
    if let Some(mut out) = out {
        out.flush()?;
    }
    Ok(outcomes)
}
