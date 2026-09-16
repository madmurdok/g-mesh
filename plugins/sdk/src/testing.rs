//! Running `g-mesh plugins check` from a plugin crate's own `#[test]`.
//!
//! # Why the SDK ships this
//!
//! The SDK does not make a plugin conformant; the conformance kit decides
//! that, and it decides it about a *binary* run against a *fixture*. So the
//! gap this closes is not knowledge, it is friction: without it, checking a
//! plugin means remembering a command, and a check that has to be remembered
//! is a check that runs when someone suspects a problem rather than when one
//! appears. With it, a plugin crate writes
//!
//! ```no_run
//! # use g_mesh_plugin_sdk::testing::PluginCheck;
//! #[test]
//! fn the_plugin_is_conformant() {
//!     PluginCheck::new("rust", env!("CARGO_BIN_EXE_g-mesh-plugin-rust"), "tests/fixtures/project")
//!         .semantic_pass(true)
//!         .exclude_dirs(&["target"])
//!         .run()
//!         .expect("the conformance kit could not be run")
//!         .assert_conformant();
//! }
//! ```
//!
//! and every one of the kit's checks runs on every `cargo test`.
//!
//! # Finding the two binaries
//!
//! **The plugin's**: the caller passes it, as `env!("CARGO_BIN_EXE_<name>")`.
//! Cargo sets that variable for every integration test of the package that
//! declares the binary, it is exact, and it forces cargo to build the binary
//! before the test runs. No build script, no search, nothing to configure -
//! and it is the one method that works from the plugin crate's own tests,
//! which is where this is meant to be called.
//!
//! **Core's**: it is not this crate's binary, so there is no environment
//! variable for it, and the three ways to get one were weighed:
//!
//! - *A build script that builds core.* Rejected: it would make `cargo build`
//!   on a plugin crate compile a statically linked ONNX Runtime and a bundled
//!   SQLite, minutes of it, for a binary only the tests use.
//! - *An environment variable only.* Rejected as the sole mechanism: it would
//!   make `cargo test` fail out of the box, which is the moment a new plugin
//!   author meets this crate.
//! - *Search the build directory, with the variable as an override.* Chosen.
//!   A workspace shares one target directory, and the test binary running
//!   this code is inside it (`target/<profile>/deps/…`), so core's binary -
//!   if it has been built - is a fixed two directories up. `cargo test` at
//!   the workspace root builds it as a matter of course, because core's own
//!   integration tests use `CARGO_BIN_EXE_g-mesh`.
//!
//! So: [`G_MESH_BIN_ENV`] if set, else `g-mesh` beside the test binary's
//! profile directory. Neither found is an error naming what to run, never a
//! silent skip - a conformance test that passes because it did not run is the
//! failure mode this whole kit exists to remove.
//!
//! # The scratch plugin directory
//!
//! The kit takes a *plugin directory*: one named after the language, holding
//! a `plugin.toml` whose `command` it spawns. A cargo-built plugin has no
//! such directory - its binary is in `target/debug/` under whatever name the
//! `[[bin]]` gave it - so [`PluginCheck::run`] writes one into a scratch
//! directory for the length of the check, with `command` pointing at the
//! absolute path of the binary it was handed. It also points the plugin at
//! that manifest through [`MANIFEST_PATH_ENV`](crate::MANIFEST_PATH_ENV), so
//! the walk uses the same `exclude_dirs` the manifest declares rather than
//! only the in-code spec - which means this helper exercises the manifest
//! path, not just the fallback.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::manifest::MANIFEST_PATH_ENV;

/// An explicit path to the `g-mesh` binary, for a layout the search below
/// does not cover - a plugin crate outside g-mesh's own workspace, or an
/// installed g-mesh being checked against.
pub const G_MESH_BIN_ENV: &str = "G_MESH_BIN";

/// What the kit said about one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The rule was judged and held.
    Pass,
    /// The rule was judged and broken.
    Fail,
    /// The run never produced the evidence this check reads. Not a pass: see
    /// core's `cli::plugin_check::checks` on why "not instrumented" and "not
    /// reached" are reported rather than waved through.
    Skip,
}

/// A conformance run: `g-mesh plugins check` against a fixture, with a
/// scratch manifest built for a cargo-built plugin binary.
#[derive(Debug, Clone)]
pub struct PluginCheck {
    language: String,
    plugin_binary: PathBuf,
    plugin_version: String,
    fixture: PathBuf,
    extensions: Vec<String>,
    exclude_dirs: Vec<String>,
    entry_points: Vec<String>,
    watch_files: Vec<String>,
    semantic_pass: bool,
    receiver_calls: &'static str,
    receiver_calls_structural: &'static str,
    expect: Option<PathBuf>,
    core_binary: Option<PathBuf>,
}

impl PluginCheck {
    /// `language` is the plugin's wire identifier; `plugin_binary` is
    /// `env!("CARGO_BIN_EXE_<name>")`; `fixture` is a small project in that
    /// language, relative to the calling crate's manifest directory or
    /// absolute.
    ///
    /// Extensions default to `.<language>`, which is right for a toy and
    /// wrong for most real plugins - call [`PluginCheck::extensions`].
    pub fn new(
        language: impl Into<String>,
        plugin_binary: impl Into<PathBuf>,
        fixture: impl Into<PathBuf>,
    ) -> Self {
        let language = language.into();
        Self {
            extensions: vec![format!(".{language}")],
            language,
            plugin_binary: plugin_binary.into(),
            plugin_version: "0.0.0-under-test".to_string(),
            fixture: fixture.into(),
            exclude_dirs: Vec::new(),
            entry_points: Vec::new(),
            watch_files: Vec::new(),
            semantic_pass: false,
            receiver_calls: "unresolved",
            receiver_calls_structural: "unresolved",
            expect: None,
            core_binary: None,
        }
    }

    /// The extensions the manifest claims - lowercase and dot-prefixed.
    pub fn extensions(mut self, extensions: &[&str]) -> Self {
        self.extensions = extensions.iter().map(|e| (*e).to_string()).collect();
        self
    }

    /// `[plugin.workspace] exclude_dirs`.
    pub fn exclude_dirs(mut self, exclude_dirs: &[&str]) -> Self {
        self.exclude_dirs = exclude_dirs.iter().map(|d| (*d).to_string()).collect();
        self
    }

    /// `[plugin.workspace] entry_points`.
    pub fn entry_points(mut self, entry_points: &[&str]) -> Self {
        self.entry_points = entry_points.iter().map(|e| (*e).to_string()).collect();
        self
    }

    /// `[plugin.workspace] watch_files`.
    pub fn watch_files(mut self, watch_files: &[&str]) -> Self {
        self.watch_files = watch_files.iter().map(|f| (*f).to_string()).collect();
        self
    }

    /// `[plugin.capabilities] semantic_pass`. `true` puts the run under
    /// `capabilities.semantic-engine-lazy` - the check that the engine was
    /// not started before the first `semanticPass` - and `false` under
    /// `capabilities.semantic-pass-undeclared`, which fails if core is ever
    /// sent one or the marker appears at all.
    pub fn semantic_pass(mut self, semantic_pass: bool) -> Self {
        self.semantic_pass = semantic_pass;
        self
    }

    /// `[plugin.capabilities] receiver_calls` and
    /// `receiver_calls_structural`, as the manifest spells them
    /// (`"resolved"` / `"unresolved"`).
    pub fn receiver_calls(mut self, best_tier: &'static str, structural: &'static str) -> Self {
        self.receiver_calls = best_tier;
        self.receiver_calls_structural = structural;
        self
    }

    /// An `expect.toml` for `--expect`, checked against the linked index
    /// after the contract checks.
    pub fn expect(mut self, expect: impl Into<PathBuf>) -> Self {
        self.expect = Some(expect.into());
        self
    }

    /// The `g-mesh` binary to run, overriding both [`G_MESH_BIN_ENV`] and the
    /// search.
    pub fn core_binary(mut self, core_binary: impl Into<PathBuf>) -> Self {
        self.core_binary = Some(core_binary.into());
        self
    }

    /// Runs the kit.
    ///
    /// `Err` means the run could not be *set up* - core's binary was not
    /// found, the fixture does not exist, the scratch directory could not be
    /// written. A plugin that fails checks is an `Ok` whose [`CheckOutcome`]
    /// says so, because that is a result, not an accident.
    pub fn run(&self) -> Result<CheckOutcome> {
        let core = self.resolve_core_binary()?;
        let fixture = fs::canonicalize(&self.fixture)
            .with_context(|| format!("fixture {} does not exist", self.fixture.display()))?;
        let plugin_binary = fs::canonicalize(&self.plugin_binary).with_context(|| {
            format!(
                "plugin binary {} does not exist - pass env!(\"CARGO_BIN_EXE_<name>\"), which cargo \
                 builds before the test runs",
                self.plugin_binary.display()
            )
        })?;

        let scratch = Scratch::create("g-mesh-sdk-check")?;
        let plugin_dir = scratch.path().join(&self.language);
        fs::create_dir_all(&plugin_dir)
            .with_context(|| format!("failed to create {}", plugin_dir.display()))?;
        let manifest_path = plugin_dir.join("plugin.toml");
        fs::write(&manifest_path, self.manifest(&plugin_binary))
            .with_context(|| format!("failed to write {}", manifest_path.display()))?;

        let mut command = Command::new(&core);
        command
            .args(["plugins", "check"])
            .arg(&plugin_dir)
            .arg("--fixture")
            .arg(&fixture)
            // So the plugin's own walk reads the same `exclude_dirs` the
            // manifest declares. Without it a cargo-built binary would fall
            // back to its in-code spec and this helper would never exercise
            // the manifest path at all.
            .env(MANIFEST_PATH_ENV, &manifest_path);
        if let Some(expect) = &self.expect {
            command.arg("--expect").arg(expect);
        }

        let output =
            command.output().with_context(|| format!("failed to run `{} plugins check`", core.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        Ok(CheckOutcome { success: output.status.success(), outcomes: parse_report(&stdout), stdout, stderr })
    }

    /// Runs the kit and fails the test unless nothing failed - the one-line
    /// form for a plugin that just wants the gate.
    pub fn assert_conformant(&self) {
        self.run().expect("the conformance kit could not be run").assert_conformant();
    }

    /// The `plugin.toml` the kit reads, with `command` resolved to an
    /// absolute path so it does not matter where it is written.
    fn manifest(&self, plugin_binary: &Path) -> String {
        let list = |values: &[String]| {
            let items: Vec<String> = values.iter().map(|value| format!("{value:?}")).collect();
            format!("[{}]", items.join(", "))
        };
        format!(
            "# Written by g_mesh_plugin_sdk::testing::PluginCheck for one conformance run.\n\
             [plugin]\n\
             language = {:?}\n\
             protocol_version = {}\n\
             plugin_version = {:?}\n\n\
             [plugin.spawn]\n\
             command = {:?}\n\n\
             [plugin.languages]\n\
             extensions = {}\n\n\
             [plugin.capabilities]\n\
             semantic_pass = {}\n\
             receiver_calls = {:?}\n\
             receiver_calls_structural = {:?}\n\n\
             [plugin.workspace]\n\
             watch_files = {}\n\
             exclude_dirs = {}\n\
             entry_points = {}\n",
            self.language,
            g_mesh_wire::CURRENT_PROTOCOL_VERSION,
            self.plugin_version,
            plugin_binary.display().to_string(),
            list(&self.extensions),
            self.semantic_pass,
            self.receiver_calls,
            self.receiver_calls_structural,
            list(&self.watch_files),
            list(&self.exclude_dirs),
            list(&self.entry_points),
        )
    }

    fn resolve_core_binary(&self) -> Result<PathBuf> {
        choose_core_binary(
            self.core_binary.clone(),
            std::env::var_os(G_MESH_BIN_ENV).filter(|value| !value.is_empty()).map(PathBuf::from),
            core_binary_near_this_test(),
        )
    }
}

/// The precedence between the three ways of naming core's binary, as a pure
/// function of what each one found.
///
/// Split out of [`PluginCheck::resolve_core_binary`] so the rule - and
/// especially the error when nothing found one - is testable without setting
/// a process-global environment variable or hiding the build directory the
/// test itself is running from.
fn choose_core_binary(
    explicit: Option<PathBuf>,
    from_env: Option<PathBuf>,
    found: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(core) = explicit.or(from_env).or(found) {
        return Ok(core);
    }
    bail!(
        "could not find the `g-mesh` binary. `cargo test` at the workspace root builds it; to run this \
         test alone, build it first (`cargo build -p g-mesh --bin g-mesh`) or set {G_MESH_BIN_ENV} to \
         its path"
    )
}

/// `g-mesh` in the build profile directory this test binary lives under.
///
/// The test binary is `<target>/<profile>/deps/<name>-<hash>`, so the binary
/// is its grandparent's `g-mesh`. Both the parent and the grandparent are
/// tried, because a test binary is not always under `deps/` (a doctest
/// runner, a custom target directory layout).
fn core_binary_near_this_test() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) { "g-mesh.exe" } else { "g-mesh" };
    exe.ancestors().skip(1).take(3).map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
}

/// One run's report.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    /// Whether `g-mesh plugins check` exited 0 - which it does exactly when
    /// no check failed.
    pub success: bool,
    /// The rendered report, check ids and all. Printed verbatim by
    /// [`CheckOutcome::assert_conformant`] on failure, because a conformance
    /// failure's diagnosis is in the finding, not in the check's name.
    pub stdout: String,
    /// The plugin's own stderr, passed through by the kit. Where a plugin's
    /// log ends up, and usually where the cause is.
    pub stderr: String,
    /// Check id -> verdict, in report order.
    pub outcomes: BTreeMap<String, Verdict>,
}

impl CheckOutcome {
    /// Every check the plugin failed.
    pub fn failures(&self) -> Vec<&str> {
        self.outcomes
            .iter()
            .filter(|(_, verdict)| **verdict == Verdict::Fail)
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Every check the kit skipped, with its reason still only in
    /// [`CheckOutcome::stdout`]. Worth asserting on in a plugin's own test: a
    /// check that starts skipping is a check that stopped running.
    pub fn skipped(&self) -> Vec<&str> {
        self.outcomes
            .iter()
            .filter(|(_, verdict)| **verdict == Verdict::Skip)
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// What the kit said about one check, or `None` if it did not report it
    /// at all - which is itself worth failing on, since a check that vanished
    /// from the report is one no assertion is holding any more.
    pub fn verdict(&self, check: &str) -> Option<Verdict> {
        self.outcomes.get(check).copied()
    }

    /// Panics unless the run succeeded and nothing failed, printing the whole
    /// report.
    pub fn assert_conformant(&self) {
        let failures = self.failures();
        assert!(
            failures.is_empty() && self.success,
            "the plugin failed {} conformance check(s): {}\n\n{}\n--- the plugin's stderr ---\n{}",
            failures.len(),
            failures.join(", "),
            self.stdout,
            self.stderr
        );
    }
}

/// Pulls `  PASS  <id>` lines out of the rendered report.
///
/// Parsing the human report rather than asking for a machine-readable one:
/// the kit has no `--json`, and the report's own shape is what core's
/// `tests/plugin_check.rs` already parses the same way - so this stays
/// correct exactly as long as that suite does.
fn parse_report(stdout: &str) -> BTreeMap<String, Verdict> {
    let mut outcomes = BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("  ") else { continue };
        for (word, verdict) in [("PASS", Verdict::Pass), ("FAIL", Verdict::Fail), ("SKIP", Verdict::Skip)] {
            if let Some(tail) = rest.strip_prefix(word).and_then(|tail| tail.strip_prefix("  ")) {
                if let Some(id) = tail.split_whitespace().next() {
                    outcomes.insert(id.to_string(), verdict);
                }
            }
        }
    }
    outcomes
}

/// A uniquely named temporary directory, removed on drop.
///
/// Hand-rolled rather than `tempfile`, which would be a dependency of the
/// *library* - this module is public API, not test-only code, so a dev
/// dependency would not cover it. The same reasoning, and very nearly the
/// same code, as core's own `cli::plugin_check::session::Scratch`.
struct Scratch(PathBuf);

impl Scratch {
    fn create(prefix: &str) -> Result<Self> {
        let base = std::env::temp_dir();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
        for attempt in 0..64 {
            let path = base.join(format!("{prefix}-{}-{nanos}-{attempt}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err).with_context(|| format!("failed to create {}", path.display())),
            }
        }
        bail!("failed to create a scratch directory under {}", base.display())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_is_parsed_into_verdicts() {
        let report = "\
g-mesh plugins check: toy
  PASS  session
  FAIL  shape
        placeholder node \"n1\" has no `target`
  SKIP  capabilities.semantic-engine-lazy
        not applicable: the manifest declares semantic_pass = false
not a check line
";
        let outcomes = parse_report(report);
        assert_eq!(outcomes.get("session"), Some(&Verdict::Pass));
        assert_eq!(outcomes.get("shape"), Some(&Verdict::Fail));
        assert_eq!(outcomes.get("capabilities.semantic-engine-lazy"), Some(&Verdict::Skip));
        assert_eq!(outcomes.len(), 3, "a finding's own line is not a check: {outcomes:?}");
    }

    #[test]
    fn the_written_manifest_is_the_shape_read_manifest_requires() {
        let check = PluginCheck::new("toy", std::env::current_exe().unwrap(), ".")
            .extensions(&[".toy"])
            .exclude_dirs(&["vendor"])
            .semantic_pass(true);
        let manifest = check.manifest(Path::new("/tmp/plugin-binary"));

        assert!(manifest.contains("language = \"toy\""), "{manifest}");
        assert!(
            manifest.contains(&format!("protocol_version = {}", g_mesh_wire::CURRENT_PROTOCOL_VERSION)),
            "{manifest}"
        );
        assert!(manifest.contains("command = \"/tmp/plugin-binary\""), "{manifest}");
        assert!(manifest.contains("extensions = [\".toy\"]"), "{manifest}");
        assert!(manifest.contains("exclude_dirs = [\"vendor\"]"), "{manifest}");
        assert!(manifest.contains("semantic_pass = true"), "{manifest}");
        // ...and it is valid TOML, which is the half an assertion on
        // substrings cannot cover.
        let parsed: toml::Value = toml::from_str(&manifest).expect("the manifest must parse as TOML");
        assert_eq!(parsed["plugin"]["language"].as_str(), Some("toy"));
        assert_eq!(parsed["plugin"]["workspace"]["entry_points"].as_array().map(Vec::len), Some(0));
    }

    /// No core binary found anywhere must be an error that says what to run -
    /// never a skip, and never a pass. A conformance test that quietly did
    /// not run is the exact failure this whole kit exists to remove.
    #[test]
    fn a_core_binary_nothing_found_is_an_actionable_error_rather_than_a_skip() {
        let err = choose_core_binary(None, None, None).expect_err("nothing found must not succeed");
        let message = format!("{err:#}");
        assert!(message.contains("cargo build -p g-mesh"), "{message}");
        assert!(message.contains(G_MESH_BIN_ENV), "{message}");
    }

    #[test]
    fn an_explicit_core_binary_outranks_the_environment_which_outranks_the_search() {
        let (explicit, from_env, found) =
            (PathBuf::from("/explicit"), PathBuf::from("/env"), PathBuf::from("/found"));
        let choose =
            |a: Option<PathBuf>, b: Option<PathBuf>, c: Option<PathBuf>| choose_core_binary(a, b, c).unwrap();
        assert_eq!(choose(Some(explicit.clone()), Some(from_env.clone()), Some(found.clone())), explicit);
        assert_eq!(choose(None, Some(from_env.clone()), Some(found.clone())), from_env);
        assert_eq!(choose(None, None, Some(found.clone())), found);
    }

    /// A plugin binary that is not there is reported as that, with the
    /// remedy, rather than as an opaque spawn failure from inside the kit.
    #[test]
    fn a_missing_plugin_binary_names_the_env_var_that_would_have_found_it() {
        let check = PluginCheck::new("toy", "/nonexistent/g-mesh-plugin-toy", ".")
            .core_binary(std::env::current_exe().unwrap());
        let err = check.run().expect_err("a plugin binary that does not exist must fail");
        assert!(format!("{err:#}").contains("CARGO_BIN_EXE"), "{err:#}");
    }

    #[test]
    fn a_scratch_directory_is_removed_when_it_is_dropped() {
        let path = {
            let scratch = Scratch::create("g-mesh-sdk-scratch-test").unwrap();
            let path = scratch.path().to_path_buf();
            assert!(path.is_dir());
            path
        };
        assert!(!path.exists(), "{} outlived its Scratch", path.display());
    }
}
