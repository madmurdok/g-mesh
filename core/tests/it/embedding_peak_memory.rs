//! GM-393: one over-long input must not be able to cost the daemon gigabytes.
//!
//! Bulk-indexing g-mesh's own repository drove the daemon to 15.3 GB max RSS,
//! against ~0.2 GB for the same walk with no model loaded. The cause was the
//! embedding step's input length, not the walk: self-attention's working
//! memory grows with the square of the token count, `EmbeddingModel::load`
//! used to truncate at the model's full 8192 tokens, and ONNX Runtime's CPU
//! arena keeps whatever the largest run needed. One 8192-token doc comment
//! was enough (see `embedding::model::DEFAULT_MAX_SEQUENCE_LENGTH` for the
//! measurements).
//!
//! This test is its own file, and the only test in it, on purpose: it reads
//! the process-wide peak RSS (`getrusage`), which any other test running in
//! the same process would pollute. Cargo runs each integration-test file as
//! its own binary.
//!
//! # Real model weights
//!
//! `#[ignore]`d for the same reason every other weight-dependent test is (see
//! `tests/embedding_generation_pipeline.rs`): CI and a fresh checkout have no
//! ~600 MiB model on disk. Run it explicitly:
//!
//!     g-mesh model fetch      (or core/scripts/fetch-embedding-model.sh)
//!     cargo test -p g-mesh --release --test embedding_peak_memory -- --ignored
//!
//! `--release` only for speed; on pre-fix code the over-long input alone took
//! ~65 s in a release build.

#![cfg(unix)]

use g_mesh::config::EmbeddingConfig;
use g_mesh::embedding::{default_model_dir, EmbeddingModel};

/// Peak resident set size of this process so far, in bytes.
fn peak_rss_bytes() -> u64 {
    // SAFETY: `getrusage` only writes into the zeroed struct it is handed.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(rc, 0, "getrusage failed: {}", std::io::Error::last_os_error());
    let max_rss = usage.ru_maxrss as u64;
    // macOS reports bytes, Linux (and the other Unixes this builds on) KiB.
    if cfg!(target_os = "macos") {
        max_rss
    } else {
        max_rss * 1024
    }
}

/// How much one over-long input may raise the peak. Measured on the default
/// model: truncated at 1024 tokens, it raised RSS by ~10 MB over a short
/// input; at 2048 tokens by ~600 MB; at the old 8192 by ~13.5 GB. 1 GiB sits
/// between "bounded" and the first length that is visibly not.
const MAX_PEAK_GROWTH_BYTES: u64 = 1 << 30;

#[test]
#[ignore = "needs the real model weights; see this file's module doc comment"]
fn an_over_long_input_does_not_raise_peak_memory_by_gigabytes() {
    let dir = default_model_dir(&EmbeddingConfig::default().model).unwrap();
    let model = EmbeddingModel::load(&dir)
        .unwrap_or_else(|err| panic!("could not load the real model from {}: {err:#}", dir.display()));

    // Warm up with a short input first, so that whatever the first inference
    // allocates regardless of length is part of the baseline rather than
    // counted against the long one.
    model.embed("fn add(a: i32, b: i32) -> i32 { a + b }").unwrap();
    let baseline = peak_rss_bytes();

    // Distinct words, so the text cannot tokenize into a few long merges: this
    // is well past 8192 tokens, i.e. past even the model's own limit, which is
    // exactly the shape of a very long module doc comment.
    let long: String = (0..6_000).map(|i| format!("value{i} ")).collect();
    let vector = model.embed(&long).unwrap();
    assert_eq!(vector.len(), g_mesh::embedding::EMBEDDING_DIM);

    let growth = peak_rss_bytes().saturating_sub(baseline);
    assert!(
        growth <= MAX_PEAK_GROWTH_BYTES,
        "embedding one over-long input raised peak RSS by {} MiB (baseline {} MiB); \
         the limit is {} MiB. The input is probably no longer truncated to a \
         bounded length before inference - see GM-393 and \
         `embedding::model::DEFAULT_MAX_SEQUENCE_LENGTH`.",
        growth >> 20,
        baseline >> 20,
        MAX_PEAK_GROWTH_BYTES >> 20,
    );
}
