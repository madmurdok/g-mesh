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

/// Cold-pass milliseconds used instead of a run's `timings.json`: g-mesh's is
/// the A0 median of `docs/results/gm-453-onnx-tuning.md`, the pass time
/// `docs/design/GM-441-prioritised-embedding.md` builds its seconds on.
const PASS_MS_OVERRIDE: &[(&str, f64)] = &[("g-mesh", 353_792.0)];

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
        let measured_s = match PASS_MS_OVERRIDE.iter().find(|(c, _)| c == corpus) {
            Some(&(_, ms)) => ms / 1000.0,
            None => {
                let timings: serde_json::Value = read_json(&dir.join("timings.json"))?;
                timings["embedNodesMs"].as_f64().context("timings.json has no embedNodesMs")? / 1000.0
            }
        };

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

#[cfg(test)]
mod tests {
    use super::super::{queries::ExpectedSymbol, queries::QueryKind, top_hits};
    use super::*;

    use EmbedTier::{Private, Public, Test};

    /// Interleaved tiers: index `i` is test, public, private by `i % 3`.
    fn interleaved(n: usize) -> Vec<EmbedTier> {
        (0..n).map(|i| [Test, Public, Private][i % 3]).collect()
    }

    fn is_permutation(order: &[usize], n: usize) -> bool {
        let mut sorted = order.to_vec();
        sorted.sort_unstable();
        sorted == (0..n).collect::<Vec<_>>()
    }

    /// Harness behaviours 1-4: id order whatever the tiers; tier and
    /// reversed-tier orders group by tier with ascending index inside a
    /// group even when indices interleave; the oracle puts gold first; each
    /// order is a permutation.
    /// Control: make `order` leave `ScheduleArm::Tier` in id order.
    #[test]
    fn each_arm_orders_the_candidates_by_its_rule_and_keeps_ascending_index_within_a_group() {
        let tiers = interleaved(9);
        let gold = [false, false, true, false, true, false, false, false, true];
        assert_eq!(order(ScheduleArm::Id, &tiers, &gold), (0..9).collect::<Vec<_>>());
        assert_eq!(order(ScheduleArm::Tier, &tiers, &gold), vec![1, 4, 7, 2, 5, 8, 0, 3, 6]);
        assert_eq!(order(ScheduleArm::TierReversed, &tiers, &gold), vec![0, 3, 6, 2, 5, 8, 1, 4, 7]);
        assert_eq!(order(ScheduleArm::Oracle, &tiers, &gold), vec![2, 4, 8, 0, 1, 3, 5, 6, 7]);
        for arm in ScheduleArm::ALL {
            assert!(is_permutation(&order(arm, &tiers, &gold), 9), "{arm:?}");
        }
    }

    /// Harness behaviour 5: a checkpoint at the first prefix whose cost
    /// reaches each 5% of the total, so a costly first node moves the
    /// points; every extra length is kept; ascending, no duplicate, no 0,
    /// ends at `n`.
    /// Control: space the checkpoints by count (`n * step / 20`) instead of
    /// by cost; the skewed case then reads `[2, 4, ...]`.
    #[test]
    fn checkpoints_fall_at_each_5_percent_of_the_cost_plus_the_tier_ends() {
        let cumulative = |costs: &[f64]| {
            let mut c = vec![0.0];
            for cost in costs {
                c.push(c.last().unwrap() + cost);
            }
            c
        };
        let uniform = cumulative(&[1.0; 40]);
        assert_eq!(checkpoint_lengths(&uniform, &[]), (1..=20).map(|s| 2 * s).collect::<Vec<_>>());

        let mut skewed_costs = vec![1.0; 41];
        skewed_costs[0] = 40.0;
        let skewed = cumulative(&skewed_costs);
        let mut want = vec![1];
        want.extend((11..=20).map(|s| 4 * s - 39));
        assert_eq!(checkpoint_lengths(&skewed, &[]), want);

        let with_extra = checkpoint_lengths(&uniform, &[7, 0, 40, 7]);
        assert!(with_extra.contains(&7));
        assert!(with_extra.windows(2).all(|w| w[0] < w[1]), "{with_extra:?}");
        assert_eq!(with_extra.first(), Some(&2));
        assert_eq!(with_extra.last(), Some(&40));
    }

    /// Scores with many ties, so the tie-break decides most ranks.
    fn tied_scores(n: usize, salt: usize) -> Vec<f64> {
        (0..n).map(|i| ((i * 7 + salt * 3) % 5) as f64 / 5.0).collect()
    }

    /// Harness behaviour 6 (design note item 9): over every candidate the
    /// replay ranks exactly as a full pass (`top_hits`), ties by ascending
    /// index whatever order the embedded set comes in; over a prefix it
    /// returns only embedded candidates, at most `KEPT_HITS`.
    /// Control: drop `.then(a.cmp(b))` from `top_embedded`'s comparator.
    #[test]
    fn top_embedded_over_every_candidate_ranks_as_a_full_pass() {
        let scores = tied_scores(250, 1);
        let all_reversed: Vec<usize> = (0..250).rev().collect();
        assert_eq!(top_embedded(&scores, &all_reversed), top_hits(&scores));

        let prefix: Vec<usize> = (0..250).filter(|i| i % 4 != 0).rev().collect();
        let top = top_embedded(&scores, &prefix);
        assert_eq!(top.len(), KEPT_HITS);
        assert!(top.iter().all(|m| m % 4 != 0), "only embedded candidates rank");
        let few = top_embedded(&scores, &[9, 3, 5]);
        assert_eq!(few.len(), 3);
        assert!(few
            .windows(2)
            .all(|w| scores[w[0]] > scores[w[1]] || (scores[w[0]] == scores[w[1]] && w[0] < w[1])));
    }

    fn query(id: &str) -> Query {
        Query {
            id: id.into(),
            corpus: "c".into(),
            language: "rust".into(),
            kind: QueryKind::Positive,
            shape: "phrase".into(),
            text: id.into(),
            expected: vec![ExpectedSymbol {
                file_path: "f".into(),
                qualified_name: "q".into(),
                kind: "Function".into(),
            }],
            derivation: "fixture".into(),
            author: "test".into(),
            mechanical: false,
        }
    }

    /// 12 interleaved candidates, three scored queries with tied scores.
    fn replay() -> Replay {
        let n = 12;
        let cost_ms: Vec<f64> = (0..n).map(|m| 1.0 + (m % 4) as f64).collect();
        Replay {
            queries: vec![query("q0"), query("q1"), query("q2")],
            ids: (0..n).map(|m| format!("n{m:02}")).collect(),
            tiers: interleaved(n),
            scores: (0..3).map(|q| tied_scores(n, q)).collect(),
            expected: vec![vec![0], vec![4, 5], vec![11]],
            run_nodes: n,
            predicted_run_ms: cost_ms.iter().sum(),
            cost_ms,
            measured_s: 10.0,
        }
    }

    fn args(out: &Path) -> ScheduleArgs {
        ScheduleArgs {
            dir: EvalDir { eval_dir: std::path::PathBuf::new() },
            run_root: std::path::PathBuf::new(),
            variant: "v".into(),
            filter: ScheduleFilter::None,
            curve: std::path::PathBuf::new(),
            arm: Vec::new(),
            corpus: Vec::new(),
            out: out.to_path_buf(),
            final_rankings_only: false,
        }
    }

    fn rankings(path: &Path) -> Vec<RankingLine> {
        std::fs::read_to_string(path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// Harness behaviours 7-9 (design note items 9 and 11): per arm, only
    /// the 100% checkpoint is `reranked`; `frac` and `t` never fall and `t`
    /// ends at `passS`; the tier arm's tier ends are the cumulative tier
    /// counts and are checkpoints; at 100% every arm writes the same
    /// rankings, and those are the stored run's (`top_hits` over all
    /// candidates, the run's expected ids).
    /// Controls: write `reranked: true` at every checkpoint; make `order`
    /// leave `ScheduleArm::Tier` in id order (the tier ends read 11/12/10).
    #[test]
    fn every_arm_ends_in_the_stored_runs_rankings_and_reranks_only_at_100_percent() {
        let out = tempfile::tempdir().unwrap();
        let args = args(out.path());
        let replay = replay();
        let mut finals = Vec::new();
        for arm in ScheduleArm::ALL {
            replay_arm(&args, "v", "c", &replay, arm).unwrap();
            let dir = out.path().join(arm.name()).join("c");
            let schedule: serde_json::Value = read_json(&dir.join("schedule.json")).unwrap();
            let checkpoints = schedule["checkpoints"].as_array().unwrap();
            let reranked: Vec<u64> = checkpoints
                .iter()
                .filter(|c| c["reranked"].as_bool().unwrap())
                .map(|c| c["embedded"].as_u64().unwrap())
                .collect();
            assert_eq!(reranked, vec![12], "{arm:?}: reranked only at 100%");
            let (t, frac): (Vec<f64>, Vec<f64>) =
                checkpoints.iter().map(|c| (c["t"].as_f64().unwrap(), c["frac"].as_f64().unwrap())).unzip();
            assert!(t.windows(2).all(|w| w[0] <= w[1]) && frac.windows(2).all(|w| w[0] <= w[1]), "{arm:?}");
            assert_eq!(t.last(), schedule["passS"].as_f64().as_ref(), "{arm:?}");
            assert_eq!(frac.last(), Some(&1.0), "{arm:?}");
            if arm == ScheduleArm::Tier {
                let ends = &schedule["tierEnds"];
                let embedded: Vec<u64> = ["public", "private", "test"]
                    .iter()
                    .map(|t| ends[t]["embedded"].as_u64().unwrap())
                    .collect();
                assert_eq!(embedded, vec![4, 8, 12]);
                let lengths: Vec<u64> = checkpoints.iter().map(|c| c["embedded"].as_u64().unwrap()).collect();
                assert!(embedded.iter().all(|e| lengths.contains(e)), "{lengths:?}");
            }
            finals.push(std::fs::read_to_string(dir.join("rankings.jsonl")).unwrap());
        }
        assert!(finals.iter().all(|f| *f == finals[0]), "100% rankings differ between arms");

        let lines = rankings(&out.path().join("id").join("c").join("rankings.jsonl"));
        for (q, line) in lines.iter().enumerate() {
            let stored: Vec<(String, f64)> = top_hits(&replay.scores[q])
                .into_iter()
                .map(|m| (replay.ids[m].clone(), replay.scores[q][m]))
                .collect();
            assert_eq!(line.hits, stored, "{}: the stored run's ranking", line.id);
            let expected: Vec<String> = replay.expected[q].iter().map(|&m| replay.ids[m].clone()).collect();
            assert_eq!(line.expected, expected);
        }
    }

    /// Harness behaviour 10: before its answer is embedded a query has no
    /// expected rank, and a prefix ranks only embedded candidates.
    /// Control: rank `top_hits(scores)` in `rank_checkpoint` (all
    /// candidates): `q0` finds `n00` at once.
    #[test]
    fn a_query_whose_answer_is_not_embedded_has_no_rank() {
        let replay = replay();
        let embedded: Vec<usize> = (1..12).collect();
        let outcomes = rank_checkpoint(&replay, &embedded, Path::new("")).unwrap();
        assert_eq!(outcomes[0].first_expected_rank, None, "n00 is not embedded");
        let all: Vec<usize> = (0..12).collect();
        let outcomes = rank_checkpoint(&replay, &all, Path::new("")).unwrap();
        assert!(outcomes[0].first_expected_rank.is_some());
    }
}
