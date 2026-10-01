//! `debug-embed-eval cost`: the embedding cost model, a predicted
//! node-embedding pass time that does not depend on the machine's thermal
//! state while it is measured.
//!
//! - `calibrate` embeds texts of exactly `n` tokens for a grid of `n` with
//!   the variant's encoder (the production loader and session options, batch
//!   of one), each point short and started only on a cool machine, and fits
//!   `t(n) = a + b*n + c*n^2` to the per-point medians.
//! - `predict` sums `t(min(n_i, max_tokens))` over exactly the texts `run`
//!   would embed for a variant (same builder, same prefix, token counts from
//!   the same tokenizer), per corpus and pooled, and divides by a reference
//!   variant's sum. Nothing is embedded.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use super::config::{Arm, CorporaFile, Variant, VariantsFile};
use super::{
    candidate_texts, load_nodes, median, plain_tokenizer, read_json, sha256_file, snapshot_path,
    verified_snapshot, write_json, EvalDir, Node,
};
use crate::embedding::model::{EmbeddingModel, ONNX_FILE_NAME};

pub const DEFAULT_VARIANT: &str = "jina-v2-base-code-int8";

#[derive(Debug, Subcommand)]
pub enum CostCommand {
    /// Measure embed latency per token count and fit t(n) = a + b*n + c*n^2.
    Calibrate(CalibrateArgs),
    /// Predict a variant's node-embedding pass time from a calibrated curve.
    Predict(PredictArgs),
}

#[derive(Debug, Args)]
pub struct CalibrateArgs {
    #[command(flatten)]
    pub dir: EvalDir,
    /// The model variant whose encoder is timed.
    #[arg(long, default_value = DEFAULT_VARIANT)]
    pub variant: String,
    /// Token counts to time (special tokens included, at most the variant's
    /// max_tokens).
    #[arg(long, value_delimiter = ',', default_value = "8,16,24,32,48,64,96,128,192,256,384,512,768,1024")]
    pub grid: Vec<usize>,
    /// Untimed calls before each point.
    #[arg(long, default_value_t = 3)]
    pub warmup: usize,
    /// Timed calls per point; the point is their median.
    #[arg(long, default_value_t = 15)]
    pub repeats: usize,
    /// The corpus whose node texts (in node order) the timed texts are cut
    /// from, so their token content is realistic.
    #[arg(long, default_value = "g-mesh")]
    pub corpus: String,
    /// Skip the cool-start gate (readings are still recorded).
    #[arg(long)]
    pub no_gate: bool,
    /// The gate's ceiling on the 1-minute load average.
    #[arg(long, default_value_t = 4.0)]
    pub max_load: f64,
    /// How long to wait for the gate before a point fails, in seconds.
    #[arg(long, default_value_t = 900)]
    pub gate_timeout: u64,
    /// Seconds between gate checks while waiting.
    #[arg(long, default_value_t = 15)]
    pub gate_poll: u64,
    #[arg(long, value_enum, default_value_t = FitMode::Relative)]
    pub fit: FitMode,
    /// The curve file; `<eval-dir>/work/cost/<variant>.curve.json` when
    /// omitted.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct PredictArgs {
    #[command(flatten)]
    pub dir: EvalDir,
    /// The curve `calibrate` wrote.
    #[arg(long)]
    pub curve: PathBuf,
    /// Variants to predict; repeat for several.
    #[arg(long, required = true)]
    pub variant: Vec<String>,
    /// The variant every ratio divides by.
    #[arg(long, default_value = DEFAULT_VARIANT)]
    pub reference: String,
    /// Corpora to predict over; all of corpora.toml when omitted.
    #[arg(long)]
    pub corpus: Vec<String>,
    /// Also write the prediction as JSON here.
    #[arg(long)]
    pub json: Option<PathBuf>,
}

pub fn run(command: &CostCommand) -> Result<()> {
    match command {
        CostCommand::Calibrate(args) => calibrate(args),
        CostCommand::Predict(args) => predict(args),
    }
}

// ---------------------------------------------------------------------------
// The curve
// ---------------------------------------------------------------------------

/// What the least squares minimises: absolute residuals in ms, or residuals
/// relative to each point's median (weights `1/t^2`), which keeps the short
/// texts most nodes embed from being swamped by the long points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum FitMode {
    Absolute,
    Relative,
}

/// `t(n) = a + b*n + c*n^2`, milliseconds per embed call.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fit {
    pub mode: FitMode,
    pub a_ms: f64,
    pub b_ms_per_token: f64,
    pub c_ms_per_token2: f64,
}

impl Fit {
    pub fn at(&self, n: usize) -> f64 {
        let n = n as f64;
        self.a_ms + self.b_ms_per_token * n + self.c_ms_per_token2 * n * n
    }
}

/// Weighted least squares of `a + b*n + c*n^2` through `(n, t)`. `n` is
/// scaled to `[0, 1]` before the normal equations, so `n^4` terms of a
/// 1024-token grid do not cost the small coefficients their precision.
pub fn fit_quadratic(points: &[(usize, f64)], mode: FitMode) -> Result<Fit> {
    let mut distinct: Vec<usize> = points.iter().map(|p| p.0).collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() < 3 {
        bail!("fitting a quadratic needs at least three distinct token counts, got {}", distinct.len());
    }
    let scale = *distinct.last().unwrap() as f64;
    let mut m = [[0.0f64; 4]; 3];
    for &(n, t) in points {
        if mode == FitMode::Relative && t <= 0.0 {
            bail!("a relative fit needs positive times, got {t} at n = {n}");
        }
        let w = match mode {
            FitMode::Absolute => 1.0,
            FitMode::Relative => 1.0 / (t * t),
        };
        let u = n as f64 / scale;
        let basis = [1.0, u, u * u];
        for r in 0..3 {
            for c in 0..3 {
                m[r][c] += w * basis[r] * basis[c];
            }
            m[r][3] += w * basis[r] * t;
        }
    }
    let beta = solve3(m).context("the fit's normal equations are singular")?;
    Ok(Fit {
        mode,
        a_ms: beta[0],
        b_ms_per_token: beta[1] / scale,
        c_ms_per_token2: beta[2] / (scale * scale),
    })
}

/// Gaussian elimination with partial pivoting on an augmented 3x4 matrix.
fn solve3(mut m: [[f64; 4]; 3]) -> Option<[f64; 3]> {
    for col in 0..3 {
        let pivot = (col..3).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[pivot][col].abs() < 1e-300 {
            return None;
        }
        m.swap(col, pivot);
        for row in 0..3 {
            if row != col {
                let pivot_row = m[col];
                let f = m[row][col] / pivot_row[col];
                for (x, p) in m[row].iter_mut().zip(pivot_row).skip(col) {
                    *x -= f * p;
                }
            }
        }
    }
    Some([m[0][3] / m[0][0], m[1][3] / m[1][1], m[2][3] / m[2][2]])
}

/// `(fit - measured) / measured` per point.
pub fn relative_residuals(points: &[(usize, f64)], fit: &Fit) -> Vec<f64> {
    points.iter().map(|&(n, t)| (fit.at(n) - t) / t).collect()
}

// ---------------------------------------------------------------------------
// The cool-start gate
// ---------------------------------------------------------------------------

/// `pmset -g therm` limits and the 1-minute load average at one moment;
/// `None` where the probe could not read them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thermal {
    pub cpu_speed_limit: Option<u32>,
    pub cpu_scheduler_limit: Option<u32>,
    pub cpu_available_cpus: Option<u32>,
    pub load1: Option<f64>,
}

impl Thermal {
    /// Unthrottled (speed and scheduler limits both 100) and the load under
    /// `max_load`. A reading the probe could not take is never cool.
    pub fn is_cool(&self, max_load: f64) -> bool {
        self.cpu_speed_limit == Some(100)
            && self.cpu_scheduler_limit == Some(100)
            && self.load1.is_some_and(|l| l < max_load)
    }
}

/// The `CPU_*` values of `pmset -g therm` output.
pub fn parse_pmset_therm(out: &str) -> (Option<u32>, Option<u32>, Option<u32>) {
    let value = |key: &str| {
        out.lines().find_map(|line| {
            let (k, v) = line.split_once('=')?;
            (k.trim() == key).then(|| v.trim().parse().ok()).flatten()
        })
    };
    (value("CPU_Speed_Limit"), value("CPU_Scheduler_Limit"), value("CPU_Available_CPUs"))
}

/// The 1-minute figure of `sysctl -n vm.loadavg` (`{ 1.23 1.10 1.05 }`).
pub fn parse_loadavg(out: &str) -> Option<f64> {
    out.split_whitespace().find(|w| *w != "{").and_then(|w| w.parse().ok())
}

fn probe_thermal() -> Thermal {
    let run = |cmd: &str, args: &[&str]| {
        Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let (speed, scheduler, cpus) =
        run("pmset", &["-g", "therm"]).map_or((None, None, None), |o| parse_pmset_therm(&o));
    let load1 = run("sysctl", &["-n", "vm.loadavg"]).and_then(|o| parse_loadavg(&o));
    Thermal { cpu_speed_limit: speed, cpu_scheduler_limit: scheduler, cpu_available_cpus: cpus, load1 }
}

#[derive(Debug, Clone, Copy)]
pub struct Gate {
    pub enabled: bool,
    pub max_load: f64,
    pub timeout: Duration,
    pub poll: Duration,
}

// ---------------------------------------------------------------------------
// Measuring
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub n: usize,
    pub latencies_ms: Vec<f64>,
    pub median_ms: f64,
    /// The reading the point started on, after any wait for the gate.
    pub thermal_before: Thermal,
    pub thermal_after: Thermal,
    pub cool_at_start: bool,
    pub gate_waited_s: f64,
    /// `(fit - median) / median`, filled in after the fit.
    #[serde(default)]
    pub residual: Option<f64>,
}

/// Times each `(n, text)`: wait for the gate, `warmup` untimed calls, then
/// `repeats` timed ones. The embedder, clock, probe and sleep are injected
/// so tests need neither a model nor real time.
#[allow(clippy::too_many_arguments)]
pub fn measure_points(
    texts: &[(usize, String)],
    warmup: usize,
    repeats: usize,
    gate: Gate,
    mut embed: impl FnMut(&str) -> Result<()>,
    mut clock: impl FnMut() -> Duration,
    mut probe: impl FnMut() -> Thermal,
    mut sleep: impl FnMut(Duration),
    mut log: impl FnMut(&Point),
) -> Result<Vec<Point>> {
    if repeats == 0 {
        bail!("--repeats must be at least 1");
    }
    let mut points = Vec::with_capacity(texts.len());
    for (n, text) in texts {
        let mut waited = Duration::ZERO;
        let before = loop {
            let reading = probe();
            if !gate.enabled || reading.is_cool(gate.max_load) {
                break reading;
            }
            if waited >= gate.timeout {
                bail!(
                    "n = {n}: the machine was not cool after {} s ({reading:?}); \
                     let it cool, raise --gate-timeout or pass --no-gate",
                    waited.as_secs()
                );
            }
            sleep(gate.poll);
            waited += gate.poll;
        };
        for _ in 0..warmup {
            embed(text)?;
        }
        let mut latencies = Vec::with_capacity(repeats);
        for _ in 0..repeats {
            let started = clock();
            embed(text)?;
            latencies.push((clock() - started).as_secs_f64() * 1000.0);
        }
        let point = Point {
            n: *n,
            median_ms: median(&latencies).unwrap(),
            latencies_ms: latencies,
            cool_at_start: before.is_cool(gate.max_load),
            thermal_before: before,
            thermal_after: probe(),
            gate_waited_s: waited.as_secs_f64(),
            residual: None,
        };
        log(&point);
        points.push(point);
    }
    Ok(points)
}

/// One tokenization, reduced to what `exact_text` needs: the token count
/// (special tokens included) and the byte end of each non-special token.
pub struct Encoded {
    pub len: usize,
    pub content_ends: Vec<usize>,
}

/// The longest prefix of `pool` that tokenizes to exactly `n` tokens, cut at
/// a token boundary. A cut can re-tokenize differently at its edge, so the
/// boundaries next to the naive one are tried too.
pub fn exact_text(pool: &str, n: usize, encode: &impl Fn(&str) -> Result<Encoded>) -> Result<String> {
    let whole = encode(pool)?;
    let specials = whole.len - whole.content_ends.len();
    if n <= specials {
        bail!("n = {n} leaves no room for content after {specials} special tokens");
    }
    let k = n - specials;
    if whole.content_ends.len() < k + 8 {
        bail!("the text pool has {} tokens, too few for n = {n}", whole.len);
    }
    for delta in [0i64, -1, 1, -2, 2, -3, 3, -4, 4, -5, 5, -6, 6, -7, 7, -8, 8] {
        let Some(kk) = (k as i64 + delta).try_into().ok().filter(|&kk: &usize| kk >= 1) else { continue };
        let cut = whole.content_ends[kk - 1];
        if !pool.is_char_boundary(cut) {
            continue;
        }
        let text = &pool[..cut];
        if encode(text)?.len == n {
            return Ok(text.to_string());
        }
    }
    bail!("no prefix of the text pool tokenizes to exactly {n} tokens")
}

/// The corpus's texts joined into one pool, grown until it holds at least
/// `min_tokens` tokens.
fn text_pool(
    prefix: &str,
    texts: &[String],
    min_tokens: usize,
    encode: &impl Fn(&str) -> Result<Encoded>,
) -> Result<String> {
    let mut chars = min_tokens * 8;
    loop {
        let mut pool = prefix.to_string();
        for t in texts {
            if pool.len() >= chars {
                break;
            }
            pool.push_str(t);
            pool.push_str("\n\n");
        }
        if encode(&pool)?.content_ends.len() >= min_tokens {
            return Ok(pool);
        }
        if pool.len() < chars {
            bail!("the corpus's texts hold fewer than {min_tokens} tokens");
        }
        chars *= 2;
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Curve {
    pub variant: String,
    pub model_dir: String,
    pub onnx_sha256: String,
    pub max_tokens: usize,
    pub source_corpus: String,
    /// `std::thread::available_parallelism`; the session itself uses the
    /// production loader's ONNX Runtime defaults.
    pub available_parallelism: usize,
    pub warmup: usize,
    pub repeats: usize,
    pub gate_enabled: bool,
    pub max_load: f64,
    pub load_ms: f64,
    pub fit: Fit,
    pub max_relative_residual: f64,
    pub points: Vec<Point>,
    pub gmesh_version: String,
}

fn calibrate(args: &CalibrateArgs) -> Result<()> {
    let eval_dir = &args.dir.eval_dir;
    let variants = VariantsFile::load(eval_dir)?;
    let variant = variants.get(&args.variant)?;
    if variant.arm != Arm::Model {
        bail!("variant {} is not a model arm", variant.name);
    }
    let spec = variant.encoder_spec()?;
    let mut grid = args.grid.clone();
    grid.sort_unstable();
    grid.dedup();
    if let Some(&n) = grid.iter().find(|&&n| n > spec.max_sequence_length) {
        bail!("n = {n} is over {}'s max_tokens {}", variant.name, spec.max_sequence_length);
    }
    let model_dir = variant.model_dir(eval_dir)?;

    // Texts first, so a grid the corpus cannot fill fails before the load.
    let corpora = CorporaFile::load(eval_dir)?;
    verified_snapshot(eval_dir, &corpora, &args.corpus)?;
    let nodes = load_nodes(&snapshot_path(eval_dir, &args.corpus))?;
    let (_, source) = candidate_texts(&nodes, variant, variants.settings.arm_seed);
    let tokenizer = plain_tokenizer(&model_dir)?;
    let encode = |text: &str| -> Result<Encoded> {
        let e = tokenizer.encode(text, true).map_err(|err| anyhow::anyhow!("failed to tokenize: {err}"))?;
        let content_ends = e
            .get_offsets()
            .iter()
            .zip(e.get_special_tokens_mask())
            .filter(|(_, &special)| special == 0)
            .map(|(o, _)| o.1)
            .collect();
        Ok(Encoded { len: e.get_ids().len(), content_ends })
    };
    let max_n = grid.last().copied().unwrap_or(0);
    let pool = text_pool(&variant.document_prefix, &source, max_n + 16, &encode)?;
    let texts: Vec<(usize, String)> =
        grid.iter().map(|&n| exact_text(&pool, n, &encode).map(|t| (n, t))).collect::<Result<_>>()?;

    let started = Instant::now();
    let model = EmbeddingModel::load_with_spec(&model_dir, spec)?;
    let load_ms = started.elapsed().as_secs_f64() * 1000.0;

    let gate = Gate {
        enabled: !args.no_gate,
        max_load: args.max_load,
        timeout: Duration::from_secs(args.gate_timeout),
        poll: Duration::from_secs(args.gate_poll),
    };
    let origin = Instant::now();
    let mut points = measure_points(
        &texts,
        args.warmup,
        args.repeats,
        gate,
        |t| model.embed(t).map(drop),
        || origin.elapsed(),
        probe_thermal,
        std::thread::sleep,
        |p| {
            eprintln!(
                "[cost calibrate] n = {:>4}: median {:.2} ms, speed limit {:?} -> {:?}, load {:?}, waited {:.0} s",
                p.n,
                p.median_ms,
                p.thermal_before.cpu_speed_limit,
                p.thermal_after.cpu_speed_limit,
                p.thermal_before.load1,
                p.gate_waited_s
            )
        },
    )?;

    let medians: Vec<(usize, f64)> = points.iter().map(|p| (p.n, p.median_ms)).collect();
    let fit = fit_quadratic(&medians, args.fit)?;
    let residuals = relative_residuals(&medians, &fit);
    for (p, r) in points.iter_mut().zip(&residuals) {
        p.residual = Some(*r);
    }
    let curve = Curve {
        variant: variant.name.clone(),
        model_dir: model_dir.display().to_string(),
        onnx_sha256: sha256_file(&model_dir.join(ONNX_FILE_NAME))?,
        max_tokens: spec.max_sequence_length,
        source_corpus: args.corpus.clone(),
        available_parallelism: std::thread::available_parallelism().map_or(0, |n| n.get()),
        warmup: args.warmup,
        repeats: args.repeats,
        gate_enabled: gate.enabled,
        max_load: args.max_load,
        load_ms,
        fit,
        max_relative_residual: residuals.iter().fold(0.0f64, |m, r| m.max(r.abs())),
        points,
        gmesh_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| eval_dir.join("work").join("cost").join(format!("{}.curve.json", variant.name)));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_json(&out, &curve)?;
    println!(
        "t(n) = {:.4} + {:.6}*n + {:.3e}*n^2 ms ({:?} fit), max |residual| {:.1}%",
        fit.a_ms,
        fit.b_ms_per_token,
        fit.c_ms_per_token2,
        fit.mode,
        curve.max_relative_residual * 100.0
    );
    for p in &curve.points {
        println!(
            "  n = {:>4}: median {:>8.2} ms, fit {:>8.2} ms, residual {:>+6.1}%",
            p.n,
            p.median_ms,
            fit.at(p.n),
            p.residual.unwrap_or(0.0) * 100.0
        );
    }
    println!("wrote {}", out.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Predicting
// ---------------------------------------------------------------------------

/// Token counts (special tokens included, untruncated) of every text `run`
/// embeds for `variant`: its text form and context, with the document
/// prefix, in candidate order.
pub fn token_counts(
    nodes: &[Node],
    variant: &Variant,
    arm_seed: u64,
    mut count: impl FnMut(&str) -> Result<usize>,
) -> Result<Vec<usize>> {
    let (_, texts) = candidate_texts(nodes, variant, arm_seed);
    texts.iter().map(|t| count(&format!("{}{t}", variant.document_prefix))).collect()
}

/// Predicted milliseconds of embedding texts of these token counts, each
/// truncated to `max_tokens` as the tokenizer truncates it.
pub fn predicted_ms(counts: &[usize], max_tokens: usize, fit: &Fit) -> f64 {
    counts.iter().map(|&n| fit.at(n.min(max_tokens))).sum()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CorpusPrediction {
    texts: usize,
    tokens: usize,
    predicted_s: f64,
    ratio: Option<f64>,
}

fn predict(args: &PredictArgs) -> Result<()> {
    let eval_dir = &args.dir.eval_dir;
    let curve: Curve = read_json(&args.curve)?;
    let corpora = CorporaFile::load(eval_dir)?;
    let variants = VariantsFile::load(eval_dir)?;
    let corpus_ids: Vec<String> = if args.corpus.is_empty() {
        corpora.corpora.iter().map(|c| c.id.clone()).collect()
    } else {
        args.corpus.clone()
    };
    let mut names = vec![args.reference.clone()];
    names.extend(args.variant.iter().filter(|v| **v != args.reference).cloned());

    // One tokenizer and one model check per model directory.
    let mut tokenizers: HashMap<PathBuf, tokenizers::Tokenizer> = HashMap::new();
    let mut chosen: Vec<(&Variant, PathBuf, usize)> = Vec::new();
    for name in &names {
        let variant = variants.get(name)?;
        if variant.arm != Arm::Model {
            bail!("cost predict covers model arms; {name} is {:?}", variant.arm);
        }
        let dir = variant.model_dir(eval_dir)?;
        if !tokenizers.contains_key(&dir) {
            let sha = sha256_file(&dir.join(ONNX_FILE_NAME))?;
            if sha != curve.onnx_sha256 {
                bail!(
                    "{name} runs {} (sha256 {sha}), not the model the curve was calibrated on",
                    dir.display()
                );
            }
            tokenizers.insert(dir.clone(), plain_tokenizer(&dir)?);
        }
        let max_tokens = variant.encoder_spec()?.max_sequence_length;
        if max_tokens > curve.points.iter().map(|p| p.n).max().unwrap_or(0) {
            eprintln!(
                "warning: {name}'s max_tokens {max_tokens} is past the curve's grid; t(n) extrapolates"
            );
        }
        chosen.push((variant, dir, max_tokens));
    }

    let mut table: BTreeMap<String, BTreeMap<String, CorpusPrediction>> = BTreeMap::new();
    for corpus in &corpus_ids {
        verified_snapshot(eval_dir, &corpora, corpus)?;
        let nodes = load_nodes(&snapshot_path(eval_dir, corpus))?;
        for (variant, dir, max_tokens) in &chosen {
            let tokenizer = &tokenizers[dir];
            let counts = token_counts(&nodes, variant, variants.settings.arm_seed, |t| {
                Ok(tokenizer
                    .encode(t, true)
                    .map_err(|err| anyhow::anyhow!("failed to tokenize: {err}"))?
                    .get_ids()
                    .len())
            })?;
            let row = CorpusPrediction {
                texts: counts.len(),
                tokens: counts.iter().map(|&n| n.min(*max_tokens)).sum(),
                predicted_s: predicted_ms(&counts, *max_tokens, &curve.fit) / 1000.0,
                ratio: None,
            };
            table.entry(variant.name.clone()).or_default().insert(corpus.clone(), row);
        }
    }
    let result = with_ratios(table, &args.reference);

    println!("curve {} ({}), reference {}", args.curve.display(), curve.variant, args.reference);
    println!(
        "{:<44} {:<18} {:>7} {:>10} {:>11} {:>7}",
        "variant", "corpus", "texts", "tokens", "predicted s", "ratio"
    );
    for (name, rows) in &result {
        for (corpus, r) in rows {
            println!(
                "{:<44} {:<18} {:>7} {:>10} {:>11.1} {:>7}",
                name,
                corpus,
                r.texts,
                r.tokens,
                r.predicted_s,
                r.ratio.map_or("-".to_string(), |x| format!("{x:.3}"))
            );
        }
    }
    if let Some(path) = &args.json {
        write_json(
            path,
            &serde_json::json!({
                "curve": args.curve.display().to_string(),
                "curveVariant": curve.variant,
                "fit": curve.fit,
                "reference": args.reference,
                "variants": result,
            }),
        )?;
    }
    Ok(())
}

/// Adds a `pooled` row per variant (sums over its corpora) and each row's
/// ratio to the reference's row of the same corpus.
fn with_ratios(
    mut table: BTreeMap<String, BTreeMap<String, CorpusPrediction>>,
    reference: &str,
) -> BTreeMap<String, BTreeMap<String, CorpusPrediction>> {
    for rows in table.values_mut() {
        let pooled = CorpusPrediction {
            texts: rows.values().map(|r| r.texts).sum(),
            tokens: rows.values().map(|r| r.tokens).sum(),
            predicted_s: rows.values().map(|r| r.predicted_s).sum(),
            ratio: None,
        };
        rows.insert("pooled".to_string(), pooled);
    }
    let base: BTreeMap<String, f64> = table
        .get(reference)
        .map(|rows| rows.iter().map(|(c, r)| (c.clone(), r.predicted_s)).collect())
        .unwrap_or_default();
    for rows in table.values_mut() {
        for (corpus, r) in rows.iter_mut() {
            r.ratio = base.get(corpus).filter(|&&b| b > 0.0).map(|b| r.predicted_s / b);
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::super::context::test_node;
    use super::*;

    fn fit_of(a: f64, b: f64, c: f64) -> Fit {
        Fit { mode: FitMode::Relative, a_ms: a, b_ms_per_token: b, c_ms_per_token2: c }
    }

    const GRID: [usize; 14] = [8, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024];

    /// Control: fitting `a + b*n` only (dropping the `n^2` basis in
    /// `fit_quadratic`) leaves `c` at zero and fails the `c` assertion; a
    /// wrong unscaling (`beta[2] / scale` instead of `/ scale^2`) fails it too.
    #[test]
    fn the_fit_recovers_known_coefficients() {
        let truth = fit_of(2.5, 0.031, 4.2e-5);
        for mode in [FitMode::Absolute, FitMode::Relative] {
            let points: Vec<(usize, f64)> = GRID.iter().map(|&n| (n, truth.at(n))).collect();
            let fit = fit_quadratic(&points, mode).unwrap();
            assert!((fit.a_ms - 2.5).abs() < 1e-9, "{fit:?}");
            assert!((fit.b_ms_per_token - 0.031).abs() < 1e-12, "{fit:?}");
            assert!((fit.c_ms_per_token2 - 4.2e-5).abs() < 1e-15, "{fit:?}");
            assert!(relative_residuals(&points, &fit).iter().all(|r| r.abs() < 1e-10));
        }
    }

    /// Control: weighting every point by 1 in `Relative` mode makes both
    /// modes return the same fit, failing the inequality; the residuals then
    /// lose the small-n advantage.
    #[test]
    fn a_relative_fit_weights_the_short_points() {
        let truth = fit_of(2.0, 0.03, 4e-5);
        // Noise of +-5% alternating, so no quadratic fits it exactly.
        let points: Vec<(usize, f64)> = GRID
            .iter()
            .enumerate()
            .map(|(i, &n)| (n, truth.at(n) * if i % 2 == 0 { 1.05 } else { 0.95 }))
            .collect();
        let abs = fit_quadratic(&points, FitMode::Absolute).unwrap();
        let rel = fit_quadratic(&points, FitMode::Relative).unwrap();
        assert_ne!(abs, rel);
        let worst_short =
            |f: &Fit| relative_residuals(&points[..4], f).iter().fold(0.0f64, |m, r| m.max(r.abs()));
        assert!(worst_short(&rel) < worst_short(&abs), "{rel:?} vs {abs:?}");
    }

    #[test]
    fn the_fit_needs_three_distinct_points() {
        assert!(fit_quadratic(&[(8, 1.0), (8, 1.1), (16, 2.0)], FitMode::Absolute).is_err());
    }

    /// Control: predicting on the untruncated counts (`fit.at(n)` without
    /// `.min(max_tokens)`) makes the 3000-token text cost t(3000), failing
    /// the equality.
    #[test]
    fn prediction_truncates_each_text_to_max_tokens() {
        let fit = fit_of(1.0, 0.01, 1e-5);
        let counts = [10, 3000, 1024];
        let expected = fit.at(10) + fit.at(1024) + fit.at(1024);
        assert!((predicted_ms(&counts, 1024, &fit) - expected).abs() < 1e-9);
        let expected_512 = fit.at(10) + 2.0 * fit.at(512);
        assert!((predicted_ms(&counts, 512, &fit) - expected_512).abs() < 1e-9);
    }

    const VARIANTS: &str = r#"
        [settings]
        reference = "r"
        bootstrap_seed = 1
        bootstrap_resamples = 10
        arm_seed = 7

        [[variant]]
        name = "r"
        arm = "model"
        role = "reference"
        pooling = "mean"
        dimension = 768
        max_tokens = 1024

        [[variant]]
        name = "fp"
        arm = "model"
        role = "cost"
        pooling = "mean"
        dimension = 768
        max_tokens = 1024
        text = "first-paragraph"

        [[variant]]
        name = "ctx"
        arm = "model"
        role = "cost"
        pooling = "mean"
        dimension = 768
        max_tokens = 1024
        context = "path"

        [[variant]]
        name = "prefixed"
        arm = "model"
        role = "cost"
        pooling = "mean"
        dimension = 768
        max_tokens = 1024
        document_prefix = "passage: "
    "#;

    fn nodes() -> Vec<Node> {
        let mut documented =
            test_node("a", "Function", "function_item", "m::alpha", "src/lib.rs", "rust", Some("fn alpha()"));
        documented.doc = Some("First line of prose.\n\nSecond paragraph with many more words in it.".into());
        documented.text = crate::embedding::pipeline::text_to_embed(
            documented.doc.as_deref(),
            documented.signature.as_deref(),
        );
        let plain = test_node(
            "b",
            "Function",
            "function_item",
            "m::beta",
            "src/lib.rs",
            "rust",
            Some("fn beta(x: u32)"),
        );
        let no_text = test_node("c", "Type", "struct_item", "m::Gamma", "src/lib.rs", "rust", None);
        vec![documented, plain, no_text]
    }

    fn words(t: &str) -> Result<usize> {
        Ok(t.split_whitespace().count() + 2)
    }

    /// The counts are those of `run`'s texts: candidates only (the node with
    /// no text is skipped), per text form, context and prefix. Controls:
    /// building the arm's texts from `Node::text` (the reference's texts)
    /// makes `fp`, `ctx` and `prefixed` equal `r`, failing each assertion.
    #[test]
    fn token_counts_follow_the_variants_text_form_context_and_prefix() {
        let file = VariantsFile::parse(VARIANTS).unwrap();
        let nodes = nodes();
        let count = |name: &str| token_counts(&nodes, file.get(name).unwrap(), 7, words).unwrap();
        let full = count("r");
        assert_eq!(
            full,
            vec![
                words(nodes[0].text.as_deref().unwrap()).unwrap(),
                words(nodes[1].text.as_deref().unwrap()).unwrap()
            ]
        );
        let first = count("fp");
        assert!(first[0] < full[0], "{first:?} vs {full:?}");
        assert_eq!(first[1], full[1]);
        let ctx = count("ctx");
        assert!(ctx.iter().zip(&full).all(|(c, f)| c > f), "{ctx:?} vs {full:?}");
        assert_eq!(count("prefixed"), full.iter().map(|n| n + 1).collect::<Vec<_>>());
    }

    /// Control: dividing by the arm's own pooled time (or skipping the
    /// reference lookup) makes every ratio 1.0, failing the 0.5 and 2.0
    /// assertions; summing ratios instead of seconds fails the pooled one.
    #[test]
    fn ratios_divide_by_the_reference_per_corpus_and_pooled() {
        let row = |s: f64| CorpusPrediction { texts: 1, tokens: 1, predicted_s: s, ratio: None };
        let mut table = BTreeMap::new();
        table.insert(
            "ref".to_string(),
            BTreeMap::from([("x".to_string(), row(10.0)), ("y".to_string(), row(30.0))]),
        );
        table.insert(
            "arm".to_string(),
            BTreeMap::from([("x".to_string(), row(5.0)), ("y".to_string(), row(60.0))]),
        );
        let out = with_ratios(table, "ref");
        assert_eq!(out["arm"]["x"].ratio, Some(0.5));
        assert_eq!(out["arm"]["y"].ratio, Some(2.0));
        assert_eq!(out["arm"]["pooled"].predicted_s, 65.0);
        assert!((out["arm"]["pooled"].ratio.unwrap() - 65.0 / 40.0).abs() < 1e-12);
        assert_eq!(out["ref"]["pooled"].ratio, Some(1.0));
    }

    fn cool() -> Thermal {
        Thermal {
            cpu_speed_limit: Some(100),
            cpu_scheduler_limit: Some(100),
            cpu_available_cpus: Some(4),
            load1: Some(1.0),
        }
    }

    /// Control: breaking out of the gate loop on the first reading regardless
    /// (ignoring `is_cool`) records `gate_waited_s` 0 and the hot reading,
    /// failing both assertions on the first point.
    #[test]
    fn the_gate_waits_for_a_cool_start_and_records_the_readings() {
        let hot = Thermal { cpu_speed_limit: Some(46), ..cool() };
        let busy = Thermal { load1: Some(6.0), ..cool() };
        let mut readings = vec![hot, busy, cool(), cool(), cool(), cool()].into_iter();
        let mut now = Duration::ZERO;
        let mut calls = 0;
        let gate = Gate {
            enabled: true,
            max_load: 4.0,
            timeout: Duration::from_secs(60),
            poll: Duration::from_secs(10),
        };
        let texts = vec![(8, "a".to_string()), (16, "b".to_string())];
        let clock_now = std::cell::Cell::new(Duration::ZERO);
        let points = measure_points(
            &texts,
            2,
            3,
            gate,
            |t| {
                calls += 1;
                // 1 ms per call for "a", 2 ms for "b".
                clock_now.set(clock_now.get() + Duration::from_millis(if t == "a" { 1 } else { 2 }));
                Ok(())
            },
            || clock_now.get(),
            || readings.next().unwrap(),
            |d| now += d,
            |_| {},
        )
        .unwrap();
        assert_eq!(calls, 10);
        assert_eq!(points[0].gate_waited_s, 20.0);
        assert_eq!(points[0].thermal_before, cool());
        assert!(points[0].cool_at_start);
        assert_eq!(points[0].latencies_ms, vec![1.0, 1.0, 1.0]);
        assert_eq!(points[1].median_ms, 2.0);
        assert_eq!(points[1].gate_waited_s, 0.0);
        assert_eq!(now, Duration::from_secs(20));
    }

    /// Control: dropping the timeout check loops forever on a hot machine
    /// (the probe iterator never ends), so this test hangs instead of
    /// passing; with the gate disabled the hot reading is recorded, not
    /// waited on.
    #[test]
    fn a_hot_machine_fails_the_gate_or_is_recorded_when_disabled() {
        let hot = Thermal { cpu_speed_limit: Some(60), ..cool() };
        let texts = vec![(8, "a".to_string())];
        let gate = Gate {
            enabled: true,
            max_load: 4.0,
            timeout: Duration::from_secs(30),
            poll: Duration::from_secs(10),
        };
        let run = |gate: Gate| {
            measure_points(&texts, 0, 1, gate, |_| Ok(()), || Duration::ZERO, || hot, |_| {}, |_| {})
        };
        assert!(run(gate).is_err());
        let points = run(Gate { enabled: false, ..gate }).unwrap();
        assert_eq!(points[0].thermal_before, hot);
        assert!(!points[0].cool_at_start);
    }

    #[test]
    fn pmset_and_loadavg_output_parse() {
        let out = "Note: No thermal warning level has been recorded\n\
                   2026-10-01 02:21:50 +0300 CPU Power notify\n\
                   \tCPU_Scheduler_Limit \t= 100\n\
                   \tCPU_Available_CPUs \t= 8\n\
                   \tCPU_Speed_Limit \t= 82\n";
        assert_eq!(parse_pmset_therm(out), (Some(82), Some(100), Some(8)));
        assert_eq!(parse_pmset_therm("Note: nothing"), (None, None, None));
        assert_eq!(parse_loadavg("{ 17.82 56.01 49.21 }\n"), Some(17.82));
        assert!(!Thermal { cpu_speed_limit: None, ..cool() }.is_cool(4.0));
        assert!(!Thermal { load1: Some(4.0), ..cool() }.is_cool(4.0));
        assert!(cool().is_cool(4.0));
    }

    /// A toy tokenizer: `<s>`, one token per char ("ab" merged into one),
    /// `</s>`; a text ending in 'y' splits that 'y' into two tokens, as a
    /// BPE merge can change at a cut. Control: returning the naive cut
    /// without re-encoding it (`return Ok(text)` before the length check)
    /// gives "xyy" (6 tokens) for n = 5, failing the first assertion.
    #[test]
    fn exact_text_hits_the_token_count() {
        let encode = |t: &str| -> Result<Encoded> {
            let mut ends = Vec::new();
            let b = t.as_bytes();
            let mut i = 0;
            while i < b.len() {
                i += if b[i] == b'a' && b.get(i + 1) == Some(&b'b') { 2 } else { 1 };
                ends.push(i);
            }
            if b.last() == Some(&b'y') {
                ends.push(b.len());
            }
            Ok(Encoded { len: ends.len() + 2, content_ends: ends })
        };
        let pool = "xyyzab".repeat(10);
        // The naive cut for n = 5 is "xyy" (6 tokens); "xy" has 5.
        assert_eq!(exact_text(&pool, 5, &encode).unwrap(), "xy");
        for n in 3..40 {
            let reachable = (1..=pool.len()).any(|end| encode(&pool[..end]).unwrap().len == n);
            match exact_text(&pool, n, &encode) {
                Ok(text) => assert_eq!(encode(&text).unwrap().len, n, "n = {n}: {text:?}"),
                Err(err) => assert!(!reachable, "n = {n}: {err}"),
            }
        }
        assert!(exact_text(&pool, 2, &encode).is_err());
        assert!(exact_text("xy", 10, &encode).is_err());
    }
}
