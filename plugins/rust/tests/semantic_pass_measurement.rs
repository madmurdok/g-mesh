//! GM-319: what one whole-project semantic pass actually costs, measured
//! against a real `rust-analyzer` over a real corpus.
//!
//! GM-314 counted the *questions* each plugin emits and found three of four
//! real corpora over `Budgets::max_sites`; what it could not measure - and
//! said so - was `rust-analyzer`'s per-request latency, which is the other
//! half of any argument about where that ceiling belongs. Nothing else in this
//! repository drives a full pass over a corpus: the conformance fixtures are
//! ten to twelve files, and a ceiling of tens of thousands cannot be
//! calibrated against them.
//!
//! This is the harness that closes that gap. It is `#[ignore]`d and needs a
//! corpus in `GM319_CORPUS`, so `cargo test --workspace` never runs it and CI
//! never needs one - the same contract `src/census.rs` (GM-314) has.
//!
//! ```text
//! GM319_CORPUS=<dir> cargo test -p g-mesh-plugin-rust --test \
//!     semantic_pass_measurement -- --ignored --nocapture
//! ```
//!
//! `GM319_MAX_SITES` overrides the ceiling for one run, which is what makes
//! this an A/B rather than a single reading: set it to the pre-GM-319 20,000
//! and the pass reports itself incomplete with part of the corpus unasked;
//! leave it alone and the shipped ceiling is exercised. Two runs that differ
//! in exactly that one value are the cheap observation that the arms are
//! distinguishable at all.
//!
//! The configuration is read from the plugin's own `plugin.toml` rather than
//! rebuilt here, because a latency measured under different
//! `initializationOptions` is a measurement of something this plugin does not
//! ship - `cachePriming` alone moves the cost between the readiness budget and
//! the per-request one (see that manifest's own note).

use std::path::{Path, PathBuf};
use std::time::Instant;

use g_mesh_plugin_rust::extractor::RustExtractor;
use g_mesh_plugin_rust::project::ProjectContext;
use g_mesh_plugin_sdk::lsp::{Budgets, LspBridge, SemanticConfig};
use g_mesh_plugin_sdk::wire::SourceTier;
use g_mesh_plugin_sdk::{walk_project, Extractor, OpenSiteKind, RelPath, SdkIndex, SemanticEngine};

/// The manifest this plugin ships, found from the test binary's own crate
/// directory so the run does not depend on where it was started from.
fn manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin.toml")
}

/// Every file the corpus's own manifest would have walked, extracted into an
/// index, plus the question count `lsp::bridge::questions` would build from
/// it - reproduced exactly as `src/census.rs` reproduces it, and for the same
/// reason: the number that matters is the one the bridge would ask, not the
/// one the extractor emitted.
fn index_corpus(root: &Path) -> (SdkIndex, usize, usize) {
    let project = ProjectContext::load(root).expect("the corpus is a loadable Rust project");
    let files = walk_project(root, &[".rs".to_string()], &["target".to_string()]);
    let mut index = SdkIndex::new();
    let mut questions = 0usize;
    let mut site_bytes = 0usize;
    for path in &files {
        let Ok(source) = std::fs::read_to_string(root.join(path.as_str())) else { continue };
        let rel = RelPath::new(path.as_str());
        let graph = RustExtractor.extract(&project, &rel, &source);
        let implementations =
            graph.open_sites.iter().filter(|site| site.kind == OpenSiteKind::Implementation).count();
        let traits = graph.nodes.iter().filter(|node| node.native_kind.as_deref() == Some("trait")).count();
        questions += graph.open_sites.len() - implementations + traits;
        for site in &graph.open_sites {
            site_bytes += site.from_id.len()
                + site.name.len()
                + site.from_container.as_deref().map_or(0, str::len)
                + path.as_str().len();
        }
        index.insert(rel, source, graph);
    }
    (index, questions, site_bytes)
}

/// One whole-project pass, timed, against whatever `rust-analyzer` the
/// manifest resolves to.
#[test]
#[ignore = "GM-319 measurement; needs GM319_CORPUS and a real rust-analyzer"]
fn whole_project_pass_cost() {
    let root = PathBuf::from(std::env::var("GM319_CORPUS").expect("GM319_CORPUS"));
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    let built = Instant::now();
    let (index, questions, site_bytes) = index_corpus(&root);
    let build = built.elapsed();

    let config = SemanticConfig::from_manifest_at(&manifest())
        .expect("the manifest parses")
        .expect("the manifest has a [plugin.semantic] section");
    let mut budgets = Budgets::default();
    if let Ok(over) = std::env::var("GM319_MAX_SITES") {
        budgets.max_sites = over.parse().expect("GM319_MAX_SITES is a number");
    }

    println!("GM319-META\tcorpus\t{}", root.display());
    println!("GM319-META\tfiles_indexed\t{}", index.len());
    println!("GM319-META\tbridge_questions\t{questions}");
    println!("GM319-META\tmax_sites\t{}", budgets.max_sites);
    println!("GM319-META\tconcurrency\t{}", budgets.concurrency);
    println!("GM319-META\trequest_timeout_s\t{}", budgets.request.as_secs());
    println!("GM319-META\topen_site_string_bytes\t{site_bytes}");
    println!("GM319-META\tindex_build_s\t{:.2}", build.as_secs_f64());

    let mut bridge = LspBridge::with_budgets("rust", &root, config, budgets);
    let started = Instant::now();
    let answer = bridge.answer(&[], &index).expect("the bridge answers rather than failing");
    let elapsed = started.elapsed();

    let edges = answer.diff.upsert_edges.iter().filter(|edge| edge.source == SourceTier::Semantic).count();
    println!("GM319-META\tpass_wall_s\t{:.2}", elapsed.as_secs_f64());
    println!("GM319-META\tpass_complete\t{}", answer.complete);
    println!("GM319-META\tsemantic_edges\t{edges}");
    println!("GM319-META\tplaceholder_nodes\t{}", answer.diff.upsert_nodes.len());
    println!("GM319-META\tretracted_edges\t{}", answer.diff.delete_edge_ids.len());
    let asked = questions.min(budgets.max_sites.max(1));
    println!(
        "GM319-META\tms_per_question_at_concurrency\t{:.3}",
        elapsed.as_secs_f64() * 1000.0 / asked as f64
    );
    println!(
        "GM319-META\tms_per_request_serialized\t{:.3}",
        elapsed.as_secs_f64() * 1000.0 * budgets.concurrency as f64 / asked as f64
    );
}
