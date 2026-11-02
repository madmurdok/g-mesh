//! `g-mesh model fetch` in a project configured with a non-default
//! embedding model, which the command refuses to fetch: the rerank model,
//! which does not depend on the embedding model, is still fetched. Driven as
//! a subprocess because the refusal is decided from the process's cwd.
//!
//! No network: the rerank files are placed at their pinned sizes first, and
//! `fetch` keeps a file of its pinned size.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use g_mesh::config::{self, ProjectConfig};
use g_mesh::embedding::model::MODEL_DIR_ENV;
use g_mesh::embedding::rerank::RERANK_MODEL_DIR_ENV;
use g_mesh::storage::connection::project_dir;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// The rerank module's source, for its pinned sizes (crate-private there).
const RERANK_SOURCE: &str = include_str!("../../src/embedding/rerank.rs");

/// The value of `pub(crate) const <name>: u64 = <value>;` in the rerank
/// module.
fn pinned_size(name: &str) -> u64 {
    let prefix = format!("pub(crate) const {name}: u64 = ");
    let line = RERANK_SOURCE
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` in rerank.rs"));
    line.trim_end_matches(';').replace('_', "").parse().unwrap_or_else(|_| panic!("not a size: {line}"))
}

/// A project whose `[embedding] model` is not the default, so `model fetch`
/// refuses the embedding model; its state directory goes on drop.
struct ConfiguredProject {
    dir: tempfile::TempDir,
}

impl ConfiguredProject {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project");
        let mut project_config = ProjectConfig::default();
        project_config.embedding.model = "some-other-model".to_string();
        config::write_project_config(dir.path(), &project_config)
            .expect("failed to write the project config");
        Self { dir }
    }

    /// `g-mesh model fetch <args>` from the project, with the embedding
    /// model directory at `embedding_dir` and the rerank one at `rerank_dir`.
    fn fetch(&self, args: &[&str], embedding_dir: &Path, rerank_dir: &Path) -> Output {
        Command::new(BIN)
            .args(["model", "fetch"])
            .args(args)
            .current_dir(self.dir.path())
            .env(MODEL_DIR_ENV, embedding_dir)
            .env(RERANK_MODEL_DIR_ENV, rerank_dir)
            .output()
            .expect("failed to run `g-mesh model fetch`")
    }
}

impl Drop for ConfiguredProject {
    fn drop(&mut self) {
        if let Ok(state_dir) = project_dir(self.dir.path()) {
            let _ = std::fs::remove_dir_all(state_dir);
        }
    }
}

fn texts(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

const REFUSAL: &str = "can only download g-mesh's default";

/// A refused embedding fetch still fetches the rerank files, then fails with
/// the refusal; the embedding directory is not created.
///
/// *Control:* drop the `fetch_rerank` call from `fetch`'s refusal branch ->
/// the rerank files are never reported.
#[test]
fn a_refused_embedding_fetch_still_fetches_the_rerank_model() {
    let project = ConfiguredProject::new();
    let models = tempfile::tempdir().expect("failed to create a model root");
    let embedding_dir: PathBuf = models.path().join("some-other-model");
    let rerank_dir = models.path().join("rerank");
    std::fs::create_dir_all(&rerank_dir).expect("failed to create the rerank directory");
    for (name, size) in [
        ("model.onnx", pinned_size("RERANK_ONNX_SIZE")),
        ("tokenizer.json", pinned_size("RERANK_TOKENIZER_SIZE")),
    ] {
        File::create(rerank_dir.join(name)).unwrap().set_len(size).unwrap();
    }

    let output = project.fetch(&[], &embedding_dir, &rerank_dir);
    let (stdout, stderr) = texts(&output);

    assert!(!output.status.success(), "the embedding refusal still fails the command: {stdout}");
    assert!(stderr.contains(REFUSAL), "{stderr}");
    assert_eq!(stdout.matches("already present").count(), 2, "{stdout}");
    assert!(stdout.contains(&format!("rerank model ready in {}", rerank_dir.display())), "{stdout}");
    assert!(!stdout.contains("downloading"), "{stdout}");
    assert!(!embedding_dir.exists(), "a refused fetch creates no embedding directory");
}

/// With `--no-rerank`, the refusal is all there is: no rerank directory.
#[test]
fn a_refused_fetch_with_no_rerank_fetches_nothing() {
    let project = ConfiguredProject::new();
    let models = tempfile::tempdir().expect("failed to create a model root");
    let embedding_dir = models.path().join("some-other-model");
    let rerank_dir = models.path().join("rerank");

    let output = project.fetch(&["--no-rerank"], &embedding_dir, &rerank_dir);
    let (stdout, stderr) = texts(&output);

    assert!(!output.status.success(), "{stdout}");
    assert!(stderr.contains(REFUSAL), "{stderr}");
    assert!(!stdout.contains("rerank"), "{stdout}");
    assert!(!rerank_dir.exists() && !embedding_dir.exists(), "nothing created");
}
