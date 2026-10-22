//! `debug-embed-eval compare`: per-node vector parity between two runs of
//! the same corpus, for session or batching settings that must not move a
//! search score by more than the tolerance.
//!
//! Vectors are unit length and scores are dot products, so a per-node L2
//! distance of at most `max_l2` bounds every score change by `max_l2`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::config::{CorporaFile, VariantsFile};
use super::queries::hex;
use super::{
    candidate_texts, load_nodes, read_json, read_vectors, snapshot_path, write_json, EvalDir, Manifest,
};

/// File in a run directory listing the candidate node ids, one per line, in
/// `vectors.bin` row order.
const NODE_IDS_FILE: &str = "node_ids.txt";

/// The default per-node tolerance: the bound `parity` already accepts for
/// harness-versus-production scores.
const DEFAULT_MAX_L2: f64 = 1e-4;

/// How many offending node ids an error names.
const NAMED_OFFENDERS: usize = 10;

#[derive(Debug, Args)]
pub struct CompareArgs {
    #[command(flatten)]
    pub dir: EvalDir,
    /// The baseline run directory (`<out>/<variant>/<corpus>`).
    #[arg(long)]
    pub baseline: PathBuf,
    /// The candidate run directory of the same corpus.
    #[arg(long)]
    pub candidate: PathBuf,
    /// Largest per-node L2 distance that passes.
    #[arg(long, default_value_t = DEFAULT_MAX_L2)]
    pub max_l2: f64,
    /// Also write the result as JSON here (written before a failing exit).
    #[arg(long)]
    pub json: Option<PathBuf>,
    /// Compare runs of different variants or node sets, over the node ids
    /// both hold (a control between two deliberately different rows).
    #[arg(long)]
    pub allow_variant_mismatch: bool,
}

/// The per-node comparison of two runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Comparison {
    pub compared: usize,
    pub bit_identical: usize,
    pub max_l2: f64,
    pub max_l2_node: Option<String>,
    pub min_cos: f64,
    pub min_cos_node: Option<String>,
    pub tolerance: f64,
    pub over_tolerance: usize,
    /// Up to [`NAMED_OFFENDERS`] node ids over the tolerance, worst first.
    pub offenders: Vec<(String, f64)>,
}

impl Comparison {
    pub fn passes(&self) -> bool {
        self.over_tolerance == 0
    }
}

pub fn run(args: &CompareArgs) -> Result<()> {
    let baseline_manifest: Manifest = read_json(&args.baseline.join("manifest.json"))?;
    let candidate_manifest: Manifest = read_json(&args.candidate.join("manifest.json"))?;
    check_comparable(&baseline_manifest, &candidate_manifest, args.allow_variant_mismatch)?;

    let eval_dir = &args.dir.eval_dir;
    let baseline_ids = node_ids(eval_dir, &args.baseline, &baseline_manifest)?;
    let candidate_ids = node_ids(eval_dir, &args.candidate, &candidate_manifest)?;
    let baseline = read_vectors(
        &args.baseline.join("vectors.bin"),
        baseline_manifest.node_count,
        baseline_manifest.dimension,
    )?;
    let candidate = read_vectors(
        &args.candidate.join("vectors.bin"),
        candidate_manifest.node_count,
        candidate_manifest.dimension,
    )?;

    let comparison = compare_vectors(&baseline_ids, &baseline, &candidate_ids, &candidate, args.max_l2);
    if comparison.compared == 0 {
        bail!("the two runs share no node ids, nothing to compare");
    }
    if let Some(path) = &args.json {
        write_json(path, &comparison)?;
    }
    println!(
        "compared {} nodes: {} bit-identical ({:.2}%), max L2 {:.3e} at {}, min cos {:.9} at {}, {} over L2 {:.1e}",
        comparison.compared,
        comparison.bit_identical,
        100.0 * comparison.bit_identical as f64 / comparison.compared as f64,
        comparison.max_l2,
        comparison.max_l2_node.as_deref().unwrap_or("-"),
        comparison.min_cos,
        comparison.min_cos_node.as_deref().unwrap_or("-"),
        comparison.over_tolerance,
        comparison.tolerance,
    );
    if !comparison.passes() {
        let named: Vec<String> =
            comparison.offenders.iter().map(|(id, l2)| format!("{id} ({l2:.3e})")).collect();
        bail!(
            "{} of {} nodes exceed L2 {:.1e}: {}",
            comparison.over_tolerance,
            comparison.compared,
            comparison.tolerance,
            named.join(", ")
        );
    }
    Ok(())
}

/// Refuses runs whose vectors cannot mean the same thing: another corpus or
/// dimension always; another variant fingerprint, snapshot or node set
/// unless `allow_mismatch` (then nodes are matched by id).
fn check_comparable(baseline: &Manifest, candidate: &Manifest, allow_mismatch: bool) -> Result<()> {
    if baseline.corpus != candidate.corpus {
        bail!("the runs are of different corpora: {} and {}", baseline.corpus, candidate.corpus);
    }
    if baseline.dimension != candidate.dimension {
        bail!("the runs' vectors differ in width: {} and {}", baseline.dimension, candidate.dimension);
    }
    if allow_mismatch {
        return Ok(());
    }
    let mut differences = Vec::new();
    if baseline.variant_fingerprint != candidate.variant_fingerprint {
        differences.push(format!("variant ({} vs {})", baseline.variant, candidate.variant));
    }
    if baseline.snapshot_sha256 != candidate.snapshot_sha256 {
        differences.push("snapshot".to_string());
    }
    if baseline.node_ids_sha256 != candidate.node_ids_sha256 || baseline.node_count != candidate.node_count {
        differences.push(format!("node set ({} vs {} nodes)", baseline.node_count, candidate.node_count));
    }
    if !differences.is_empty() {
        bail!(
            "the runs differ in {}; pass --allow-variant-mismatch to compare the node ids both hold",
            differences.join(", ")
        );
    }
    Ok(())
}

/// The run's candidate node ids in row order: its `node_ids.txt`, or else
/// recomputed from the corpus snapshot and the variant. Either way checked
/// against the manifest's node-id hash.
fn node_ids(eval_dir: &Path, run: &Path, manifest: &Manifest) -> Result<Vec<String>> {
    let path = run.join(NODE_IDS_FILE);
    let ids: Vec<String> = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
            .lines()
            .map(str::to_string)
            .collect()
    } else {
        let corpora = CorporaFile::load(eval_dir)?;
        let variants = VariantsFile::load(eval_dir)?;
        corpora.get(&manifest.corpus)?;
        let variant = variants.get(&manifest.variant)?;
        let nodes = load_nodes(&snapshot_path(eval_dir, &manifest.corpus))?;
        let (candidates, _) = candidate_texts(&nodes, variant, variants.settings.arm_seed);
        candidates.into_iter().map(|i| nodes[i].id.clone()).collect()
    };
    if node_ids_sha256(ids.iter().map(String::as_str)) != manifest.node_ids_sha256
        || ids.len() != manifest.node_count
    {
        bail!("{}: the node ids do not match the manifest's node-id hash", run.display());
    }
    Ok(ids)
}

/// The manifest's `node_ids_sha256` of these ids.
pub(crate) fn node_ids_sha256<'a>(ids: impl IntoIterator<Item = &'a str>) -> String {
    let mut hash = Sha256::new();
    for id in ids {
        hash.update(id.as_bytes());
        hash.update(b"\n");
    }
    hex(&hash.finalize())
}

/// Writes `node_ids.txt` into a run directory.
pub(crate) fn write_node_ids<'a>(run: &Path, ids: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut text = String::new();
    for id in ids {
        text.push_str(id);
        text.push('\n');
    }
    let path = run.join(NODE_IDS_FILE);
    std::fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// Compares the vectors of the node ids both runs hold, in the baseline's
/// order. A node fails when its L2 distance exceeds `max_l2`.
pub(crate) fn compare_vectors(
    baseline_ids: &[String],
    baseline: &[Vec<f32>],
    candidate_ids: &[String],
    candidate: &[Vec<f32>],
    max_l2: f64,
) -> Comparison {
    let candidate_row: HashMap<&str, usize> =
        candidate_ids.iter().enumerate().map(|(row, id)| (id.as_str(), row)).collect();
    let mut comparison = Comparison {
        compared: 0,
        bit_identical: 0,
        max_l2: 0.0,
        max_l2_node: None,
        min_cos: 1.0,
        min_cos_node: None,
        tolerance: max_l2,
        over_tolerance: 0,
        offenders: Vec::new(),
    };
    let mut offenders: Vec<(String, f64)> = Vec::new();
    for (id, a) in baseline_ids.iter().zip(baseline) {
        let Some(&row) = candidate_row.get(id.as_str()) else { continue };
        let b = &candidate[row];
        comparison.compared += 1;
        if a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits()) {
            comparison.bit_identical += 1;
        }
        let l2 = l2_distance(a, b);
        let cos = cosine(a, b);
        if comparison.max_l2_node.is_none() || l2 > comparison.max_l2 {
            comparison.max_l2 = l2;
            comparison.max_l2_node = Some(id.clone());
        }
        if comparison.min_cos_node.is_none() || cos < comparison.min_cos {
            comparison.min_cos = cos;
            comparison.min_cos_node = Some(id.clone());
        }
        // NaN fails too: a vector that is not a number is no match.
        if l2.is_nan() || l2 > max_l2 {
            comparison.over_tolerance += 1;
            offenders.push((id.clone(), l2));
        }
    }
    offenders.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    offenders.truncate(NAMED_OFFENDERS);
    comparison.offenders = offenders;
    comparison
}

/// L2 distance in f64; vectors of different length are infinitely apart.
fn l2_distance(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return f64::INFINITY;
    }
    a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum::<f64>().sqrt()
}

/// Cosine in f64; 0 for a zero vector.
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum();
    let na: f64 = a.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}
