//! `debug-embed-eval probe`: interactive query latency while something else
//! (an embedding pass) loads the machine. Embeds `burst` queries back to
//! back, sleeps `interval_ms`, and repeats until the stop file exists,
//! writing one JSON line per query.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Args;
use serde::Serialize;

use super::config::{Arm, VariantsFile};
use super::{queries, EvalDir};
use crate::embedding::model::{EmbeddingModel, InferenceOptions};

/// How often the stop file is checked while sleeping between bursts.
const STOP_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Args)]
pub struct ProbeArgs {
    #[command(flatten)]
    pub dir: EvalDir,
    #[arg(long)]
    pub variant: String,
    /// The corpus whose query set is embedded.
    #[arg(long)]
    pub corpus: String,
    /// Queries embedded back to back per burst.
    #[arg(long, default_value_t = 5)]
    pub burst: usize,
    /// Idle time between bursts.
    #[arg(long, default_value_t = 2000)]
    pub interval_ms: u64,
    /// Stop once this file exists.
    #[arg(long)]
    pub until: PathBuf,
    /// One `{t, queryId, ms}` line per query.
    #[arg(long)]
    pub out: PathBuf,
    /// Load the model with the `G_MESH_EMBED_*` options instead of the
    /// defaults (`search_code`'s session as it is today).
    #[arg(long)]
    pub options_from_env: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeLine<'a> {
    /// Seconds since the probe started, at the query's start.
    t: f64,
    query_id: &'a str,
    ms: f64,
}

pub fn run(args: &ProbeArgs) -> Result<()> {
    if args.burst == 0 {
        bail!("--burst must be at least 1");
    }
    let eval_dir = &args.dir.eval_dir;
    let variants = VariantsFile::load(eval_dir)?;
    let variant = variants.get(&args.variant)?;
    if variant.arm != Arm::Model {
        bail!("variant {} is not a model arm; the probe embeds queries", variant.name);
    }
    let base = variants.encoder_of(variant)?;
    let options =
        if args.options_from_env { InferenceOptions::from_env() } else { InferenceOptions::default() };
    let model =
        EmbeddingModel::load_with_options(&base.model_dir(eval_dir)?, base.encoder_spec()?, &options)?;
    let (queries, _) = queries::load(eval_dir, &args.corpus)?;
    if queries.is_empty() {
        bail!("corpus {} has no queries", args.corpus);
    }
    let texts: Vec<String> = queries.iter().map(|q| format!("{}{}", base.query_prefix, q.text)).collect();

    let file = std::fs::File::create(&args.out)
        .with_context(|| format!("failed to create {}", args.out.display()))?;
    let mut out = std::io::LineWriter::new(file);
    let started = Instant::now();
    let mut next = 0usize;
    let mut embedded = 0usize;
    while !args.until.exists() {
        for _ in 0..args.burst {
            let index = next % texts.len();
            next += 1;
            let t = started.elapsed().as_secs_f64();
            let call = Instant::now();
            model.embed(&texts[index])?;
            let ms = call.elapsed().as_secs_f64() * 1000.0;
            let line = ProbeLine { t, query_id: &queries[index].id, ms };
            writeln!(out, "{}", serde_json::to_string(&line)?)?;
            embedded += 1;
        }
        sleep_unless_stopped(Duration::from_millis(args.interval_ms), &args.until);
    }
    out.flush()?;
    println!("probe: {embedded} queries in {:.1} s", started.elapsed().as_secs_f64());
    Ok(())
}

/// Sleeps for `total`, returning early once `stop` exists.
fn sleep_unless_stopped(total: Duration, stop: &Path) {
    let deadline = Instant::now() + total;
    while !stop.exists() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(STOP_POLL));
    }
}
