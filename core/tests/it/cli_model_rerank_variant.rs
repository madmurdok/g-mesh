//! `g-mesh model fetch` and `g-mesh model status` under each rerank variant
//! setting, as a subprocess with its own home: the variant comes from the
//! process environment and the global config, which only a fresh process
//! reads as the product does.
//!
//! No network: every model file is placed at its pinned size first, and
//! `fetch` keeps a file of its pinned size.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use g_mesh::embedding::model::MODEL_DIR_ENV;
use g_mesh::embedding::rerank::{RERANK_ENV, RERANK_MODEL_DIR_ENV, RERANK_MODEL_ENV};

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

const RERANK_SOURCE: &str = include_str!("../../src/embedding/rerank.rs");
const EMBEDDING_SOURCE: &str = include_str!("../../src/embedding/model.rs");
const MODEL_CLI_SOURCE: &str = include_str!("../../src/cli/model.rs");

const STAND_IN_NOTE: &str = "rerank model qat is not published yet - using l6, at its own beta";
const L6_DIR_NAME: &str = "ms-marco-MiniLM-L6-v2";
const QAT_DIR_NAME: &str = "g-mesh-reranker-l6-qat";

fn size(text: &str) -> u64 {
    text.trim()
        .trim_end_matches([',', ';'])
        .replace('_', "")
        .parse()
        .unwrap_or_else(|_| panic!("not a size: {text}"))
}

/// `field`'s value in the `spec` (`L6`, `QAT`) `VariantSpec` of the rerank
/// module.
fn spec_size(spec: &str, field: &str) -> u64 {
    let prefix = format!("{field}: ");
    let header = format!("pub(crate) const {spec}: VariantSpec");
    RERANK_SOURCE
        .lines()
        .skip_while(|line| !line.starts_with(&header))
        .take_while(|line| *line != "};")
        .find_map(|line| line.trim().strip_prefix(&prefix).map(size))
        .unwrap_or_else(|| panic!("no `{prefix}` in rerank.rs's {spec}"))
}

fn l6_size(field: &str) -> u64 {
    spec_size("L6", field)
}

/// The embedding model's (`model.onnx`, `tokenizer.json`) pinned sizes.
fn embedding_sizes() -> [(&'static str, u64); 2] {
    let onnx = EMBEDDING_SOURCE
        .lines()
        .find_map(|line| line.strip_prefix("pub(crate) const DEFAULT_ONNX_SIZE: u64 = ").map(size))
        .expect("DEFAULT_ONNX_SIZE in embedding/model.rs");
    let tokenizer = MODEL_CLI_SOURCE
        .lines()
        .skip_while(|line| !line.starts_with("const FILES: [RemoteFile; 2]"))
        .take_while(|line| *line != "];")
        .filter_map(|line| line.trim().strip_prefix("size: "))
        .nth(1)
        .map(size)
        .expect("the tokenizer's size in cli/model.rs's FILES");
    [("model.onnx", onnx), ("tokenizer.json", tokenizer)]
}

fn place(dir: &Path, files: [(&str, u64); 2]) {
    std::fs::create_dir_all(dir).unwrap();
    for (name, size) in files {
        File::create(dir.join(name)).unwrap().set_len(size).unwrap();
    }
}

/// A home of its own: `~/.g-mesh` (the global config, the rerank models'
/// default directories) and the embedding model, present at its sizes. `HOME`
/// redirects the home directory on Unix only.
struct Home {
    home: tempfile::TempDir,
    cwd: tempfile::TempDir,
    embedding_dir: PathBuf,
}

impl Home {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let embedding_dir = home.path().join("embedding");
        place(&embedding_dir, embedding_sizes());
        Self { home, cwd: tempfile::tempdir().unwrap(), embedding_dir }
    }

    fn g_mesh_home(&self) -> PathBuf {
        self.home.path().join(".g-mesh")
    }

    #[cfg(unix)]
    fn rerank_dir(&self, dir_name: &str) -> PathBuf {
        self.g_mesh_home().join("models").join(dir_name)
    }

    fn place_l6(&self, dir: &Path) {
        place(dir, [("model.onnx", l6_size("onnx_size")), ("tokenizer.json", l6_size("tokenizer_size"))]);
    }

    /// `qat`'s files in a directory of their own, as the measurement setup
    /// keeps them (its only copy while it is unpublished).
    fn place_qat(&self) -> PathBuf {
        let dir = self.home.path().join("qat");
        place(
            &dir,
            [
                ("model.onnx", spec_size("QAT", "onnx_size")),
                ("tokenizer.json", spec_size("QAT", "tokenizer_size")),
            ],
        );
        dir
    }

    #[cfg(unix)]
    fn write_global_config(&self, toml: &str) {
        std::fs::create_dir_all(self.g_mesh_home()).unwrap();
        std::fs::write(self.g_mesh_home().join("config.toml"), toml).unwrap();
    }

    /// `g-mesh model <args>` with `env` set over a clean rerank setting.
    /// `HOME` redirects `dirs::home_dir` on Unix only; on Windows the real
    /// profile directory is used.
    fn model(&self, args: &[&str], env: &[(&str, &Path)]) -> Output {
        let mut command = Command::new(BIN);
        command
            .arg("model")
            .args(args)
            .current_dir(self.cwd.path())
            .env("HOME", self.home.path())
            .env("G_MESH_HOME", self.g_mesh_home())
            .env(MODEL_DIR_ENV, &self.embedding_dir)
            // Unreachable: a download attempt fails here instead of reaching out.
            .env("G_MESH_MODEL_BASE_URL", "http://127.0.0.1:9")
            .env_remove(RERANK_ENV)
            .env_remove(RERANK_MODEL_ENV)
            .env_remove(RERANK_MODEL_DIR_ENV)
            .env_remove("G_MESH_RERANK_BETA");
        for (name, value) in env {
            command.env(name, value);
        }
        command.output().expect("failed to run `g-mesh model`")
    }
}

fn stdout_of(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "exit {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}", output.status.code());
    stdout
}

/// `model fetch` with `qat` asked for and not published: the note comes
/// first, `l6` is fetched into its own directory, nothing is downloaded,
/// no `qat` directory appears, and the command succeeds.
#[cfg(unix)]
fn assert_fetch_stands_in_l6(home: &Home, output: &Output) {
    let stdout = stdout_of(output);
    assert_eq!(stdout.lines().next(), Some(STAND_IN_NOTE), "{stdout}");
    let l6_dir = home.rerank_dir(L6_DIR_NAME);
    assert!(stdout.contains(&format!("rerank model l6 ready in {}", l6_dir.display())), "{stdout}");
    assert!(!stdout.contains("downloading") && !stdout.contains("UNPUBLISHED"), "{stdout}");
    assert!(!home.rerank_dir(QAT_DIR_NAME).exists(), "no qat directory");
}

// `dirs::home_dir` ignores `HOME` on Windows, so the default rerank model
// directory cannot be redirected there: the tests below that rely on it run
// on Unix only.
/// *Control:* drop the `!selected.published()` arm of `settle_rerank` ->
/// the fetch is refused as unpublished and exits non-zero.
#[test]
#[cfg(unix)]
fn fetch_with_qat_in_the_environment_fetches_l6_and_succeeds() {
    let home = Home::new();
    home.place_l6(&home.rerank_dir(L6_DIR_NAME));
    let output = home.model(&["fetch"], &[(RERANK_MODEL_ENV, Path::new("qat"))]);
    assert_fetch_stands_in_l6(&home, &output);
}

#[test]
#[cfg(unix)]
fn fetch_with_qat_in_the_global_config_fetches_l6_and_succeeds() {
    let home = Home::new();
    home.place_l6(&home.rerank_dir(L6_DIR_NAME));
    home.write_global_config("[rerank]\nmodel = \"qat\"\n");
    let output = home.model(&["fetch"], &[]);
    assert_fetch_stands_in_l6(&home, &output);
}

/// The environment's `l6` beats the config's `qat`: no note.
#[test]
#[cfg(unix)]
fn the_environment_variant_beats_the_global_config() {
    let home = Home::new();
    home.place_l6(&home.rerank_dir(L6_DIR_NAME));
    home.write_global_config("[rerank]\nmodel = \"qat\"\n");
    let stdout = stdout_of(&home.model(&["fetch"], &[(RERANK_MODEL_ENV, Path::new("l6"))]));
    assert!(!stdout.contains("not published"), "{stdout}");
    assert!(stdout.contains("rerank model l6 ready in"), "{stdout}");
}

/// With no variant set, `G_MESH_RERANK_MODEL_DIR` holding `l6`'s files is
/// still where `l6` is fetched and reported, as before the variants.
#[test]
fn a_rerank_directory_with_no_variant_set_keeps_holding_l6() {
    let home = Home::new();
    let own = home.home.path().join("my-l6");
    home.place_l6(&own);
    let stdout = stdout_of(&home.model(&["fetch"], &[(RERANK_MODEL_DIR_ENV, &own)]));
    assert!(stdout.contains(&format!("rerank model l6 ready in {}", own.display())), "{stdout}");
    assert_eq!(stdout.matches("already present").count(), 4, "{stdout}");
    assert!(!stdout.contains("not published"), "{stdout}");

    let status = stdout_of(&home.model(&["status"], &[(RERANK_MODEL_DIR_ENV, &own)]));
    assert!(status.contains(&format!("directory: {}", own.display())), "{status}");
    assert!(status.contains("search_code can rerank with this model"), "{status}");
}

/// `model status` with `qat` asked for: `l6` is reported, the note right
/// after its line.
#[test]
#[cfg(unix)]
fn status_with_qat_asked_for_reports_l6_with_the_note() {
    let home = Home::new();
    let stdout = stdout_of(&home.model(&["status"], &[(RERANK_MODEL_ENV, Path::new("qat"))]));
    let lines: Vec<&str> = stdout.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.starts_with("rerank model: l6 ("))
        .unwrap_or_else(|| panic!("no l6 line:\n{stdout}"));
    assert_eq!(lines.get(at + 1), Some(&STAND_IN_NOTE), "{stdout}");
}

/// The measurement setup with its files missing: `qat` is reported, and the
/// reader is told the fetch cannot download it rather than sent to it.
///
/// *Control:* make `model status` tell an unpublished variant "run `g-mesh
/// model fetch` to download it" (drop the `published()` branch) -> it
/// does.
#[test]
fn status_of_the_measurement_setup_does_not_send_the_reader_to_fetch() {
    let home = Home::new();
    let qat_dir = home.home.path().join("qat");
    std::fs::create_dir_all(&qat_dir).unwrap();
    let stdout = stdout_of(&home.model(
        &["status"],
        &[
            (RERANK_MODEL_ENV, Path::new("qat")),
            (RERANK_MODEL_DIR_ENV, &qat_dir),
            ("G_MESH_RERANK_BETA", Path::new("5")),
        ],
    ));
    assert!(stdout.contains("rerank model: qat ("), "{stdout}");
    assert!(!stdout.contains(STAND_IN_NOTE), "{stdout}");
    assert!(stdout.contains("not published yet, so `g-mesh model fetch` cannot download it"), "{stdout}");
    assert!(!stdout.contains("run `g-mesh model fetch`"), "{stdout}");
}

fn onnx_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("model.onnx")).unwrap().len()
}

/// `qat` asked for with `G_MESH_RERANK_MODEL_DIR` holding its files but no
/// beta: `l6` stands in from its own directory; that directory is neither
/// fetched into nor reported, and `qat`'s `model.onnx` stays.
///
/// *Control:* pass `dir_override` to the stand-in in `settle_rerank` ->
/// fetch and status name the `qat` directory.
#[test]
#[cfg(unix)]
fn the_stand_in_never_touches_a_directory_set_for_qat() {
    let home = Home::new();
    let l6_dir = home.rerank_dir(L6_DIR_NAME);
    home.place_l6(&l6_dir);
    let qat_dir = home.place_qat();
    let env = [(RERANK_MODEL_ENV, Path::new("qat")), (RERANK_MODEL_DIR_ENV, qat_dir.as_path())];

    let fetch = stdout_of(&home.model(&["fetch"], &env));
    assert!(fetch.contains(&format!("rerank model l6 ready in {}", l6_dir.display())), "{fetch}");
    assert!(!fetch.contains(&qat_dir.display().to_string()), "{fetch}");
    assert_eq!(onnx_len(&qat_dir), spec_size("QAT", "onnx_size"));

    let status = stdout_of(&home.model(&["status"], &env));
    assert!(status.contains(&format!("directory: {}", l6_dir.display())), "{status}");
    assert!(!status.contains(&qat_dir.display().to_string()), "{status}");
    assert!(status.contains("search_code can rerank with this model"), "{status}");
}

/// `l6` selected with `G_MESH_RERANK_MODEL_DIR` holding `qat`'s files:
/// `model fetch` refuses before replacing anything and says why.
///
/// *Control:* drop the `other_variant_in` check in `fetch_rerank` -> the
/// fetch starts replacing `qat`'s `model.onnx` (and fails on the dead
/// mirror without the refusal).
#[test]
fn fetch_never_replaces_another_variants_model() {
    let home = Home::new();
    let qat_dir = home.place_qat();
    for variant in [None, Some("l6")] {
        let mut env = vec![(RERANK_MODEL_DIR_ENV, qat_dir.as_path())];
        if let Some(variant) = variant {
            env.push((RERANK_MODEL_ENV, Path::new(variant)));
        }
        let output = home.model(&["fetch"], &env);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{variant:?}: {stdout}\n{stderr}");
        assert!(
            stderr.contains(&format!("{} holds rerank model qat, not l6", qat_dir.display())),
            "{variant:?}: {stderr}"
        );
        assert!(!stdout.contains("replacing") && !stdout.contains("downloading model.onnx"), "{stdout}");
        assert_eq!(onnx_len(&qat_dir), spec_size("QAT", "onnx_size"));
    }
}
