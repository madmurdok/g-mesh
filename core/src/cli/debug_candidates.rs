//! `g-mesh debug-candidates [DIR] [--json]`: the multi-project detection a
//! daemon would run for `DIR` (D10 in `docs/architecture/lazy-indexing.md`),
//! printed. Hidden; for support and for measuring the walk (M4).
//!
//! Prints the real mode decision, then the walk. When rules 1 or 2 settled
//! the decision without a walk, the walk is run anyway so its cost and
//! candidates can be inspected; `walkNeeded` says which case it was.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::json;

use crate::daemon::candidates::{self, Limits, Mode, SingleReason, Walk};

pub fn run(dir: Option<PathBuf>, as_json: bool) -> Result<()> {
    let dir = match dir {
        Some(dir) => dir,
        None => std::env::current_dir().context("failed to resolve the current directory")?,
    };
    let root = dir.canonicalize().with_context(|| format!("failed to resolve {}", dir.display()))?;

    let detection = candidates::detect(&root, Limits::default());
    let walk = if detection.walked {
        Walk {
            candidates: detection.candidates.clone(),
            entries_read: detection.entries_read,
            elapsed: detection.elapsed,
            truncated: detection.truncated,
        }
    } else {
        candidates::walk(&root, Limits::default())
    };
    let (mode, reason) = match detection.mode {
        Mode::Multi => ("multi", "two or more candidates"),
        Mode::Single(SingleReason::RootMarker) => ("single", "the folder has a project marker itself"),
        Mode::Single(SingleReason::CompletedIndex) => ("single", "the folder already has a completed index"),
        Mode::Single(SingleReason::FewCandidates) => ("single", "fewer than two candidates"),
    };

    if as_json {
        let candidates: Vec<_> = walk
            .candidates
            .iter()
            .map(|c| {
                json!({
                    "relPath": c.rel_path,
                    "absPath": c.abs_path.display().to_string(),
                    "markers": c.markers,
                    "isWorktree": c.is_worktree,
                })
            })
            .collect();
        let report = json!({
            "root": root.display().to_string(),
            "mode": mode,
            "reason": reason,
            "decisionElapsedMs": detection.elapsed.as_secs_f64() * 1000.0,
            "walkNeeded": detection.walked,
            "candidates": candidates,
            "candidateCount": walk.candidates.len(),
            "entriesRead": walk.entries_read,
            "elapsedMs": walk.elapsed.as_secs_f64() * 1000.0,
            "truncated": walk.truncated,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("root:          {}", root.display());
    println!(
        "mode:          {mode} ({reason}), decided in {:.3} ms",
        detection.elapsed.as_secs_f64() * 1000.0
    );
    if !detection.walked {
        println!("walk:          not needed for the decision; run anyway for inspection");
    }
    println!(
        "walk:          {} entries read in {:.3} ms{}",
        walk.entries_read,
        walk.elapsed.as_secs_f64() * 1000.0,
        if walk.truncated { ", truncated at a limit" } else { "" }
    );
    println!("candidates:    {}", walk.candidates.len());
    for c in &walk.candidates {
        let worktree = if c.is_worktree { " (worktree)" } else { "" };
        println!("  {}  [{}]{worktree}", c.rel_path, c.markers.join(", "));
    }
    Ok(())
}
