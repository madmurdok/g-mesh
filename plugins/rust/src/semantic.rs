//! The Rust plugin's semantic tier: finding rust-analyzer, and handing it to
//! the SDK's generic [`LspBridge`].
//!
//! Everything about *driving* a language server is the SDK's
//! (`plugins/sdk/src/lsp/`, whose module doc carries the decision table).
//! What is left for a language's own plugin is the one question the bridge
//! cannot answer generically - **which binary** - and that question turned
//! out to have a sharper edge than the task that scheduled this work
//! expected.
//!
//! # Decision: find it, then prove it
//!
//! The obvious rule is "`PATH` first, then `rustup which rust-analyzer`", and
//! the obvious implementation of it is to take the first path that exists.
//! That implementation is wrong on the most ordinary Rust installation there
//! is, and this is the measurement that says so - taken on a machine with
//! rustup and no rust-analyzer component installed:
//!
//! ```text
//! $ which rust-analyzer
//! /Users/…/.cargo/bin/rust-analyzer          # found!
//! $ ls -l /Users/…/.cargo/bin/rust-analyzer
//! … rust-analyzer -> rustup                  # a proxy, not a server
//! $ rust-analyzer --version
//! error: Unknown binary 'rust-analyzer' in official toolchain 'stable-…'
//! $ rustup which rust-analyzer
//! error: unknown binary 'rust-analyzer' in toolchain 'stable-…'
//! ```
//!
//! `~/.cargo/bin` holds a rustup *proxy* for every binary rustup knows how to
//! forward, whether or not the component behind it is installed. So `PATH`
//! resolution succeeds, the file exists, it is executable, and it is not a
//! language server: it prints an error and exits 1. Handed to the bridge,
//! that is a server which starts and immediately dies - which is not
//! `ErrorKind::NotFound`, so the bridge's permanent "this language has no
//! semantic tier" degradation never fires, and the plugin re-spawns the proxy
//! once per pass until `MAX_SERVER_STARTS` quietly stops it four starts
//! later.
//!
//! So a candidate is not accepted for existing. It is accepted for answering
//! `--version`, which takes about twenty milliseconds once per process and is
//! the only thing that tells a server apart from a shim that forwards to one
//! that is not installed. The version it prints goes into the log line,
//! because "which rust-analyzer answered this pass" is the first thing anyone
//! asks of a semantic index that looks wrong.
//!
//! # Why the failure is the factory's and not the bridge's
//!
//! [`crate::extractor`]'s tier never fails; this one can. The SDK gives the
//! two failures different shapes on purpose
//! (`g_mesh_plugin_sdk::semantic::LazyEngine`): a factory that returns `Err`
//! is reported **once**, at the first `semanticPass`, and the language is
//! structural-only for the rest of the process's life, every later pass
//! answering an empty, *incomplete* diff without starting anything. That is
//! exactly the degradation this task asks for ("log once and an empty
//! diff"), and it is strictly better than letting the bridge discover the
//! problem, which costs a process spawn per pass to reach the same
//! conclusion.
//!
//! Incomplete, not complete, is the load-bearing half: it leaves
//! `language_state.semanticPassAt` unset, which is what keeps Rust's
//! receiver-call gap listed in the generated MCP instructions
//! (`core::mcp::instructions::has_open_receiver_gap`). Installing the
//! component and restarting the daemon then gets the pass; a
//! completed-but-empty pass would have recorded "done" and never asked again.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use g_mesh_plugin_sdk::lsp::{LspBridge, SemanticConfig};
use g_mesh_plugin_sdk::SemanticEngine;

/// The language id this plugin speaks, for `didOpen` and for log lines.
const LANGUAGE: &str = "rust";

/// Builds the semantic engine for `root` - the
/// [`SemanticEngineFactory`](g_mesh_plugin_sdk::SemanticEngineFactory) body,
/// called on the first `semanticPass` and never before.
pub fn engine(root: &Path) -> Result<Box<dyn SemanticEngine>> {
    let mut config = SemanticConfig::from_manifest()
        .context("reading [plugin.semantic] from this plugin's manifest")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this plugin's manifest has no [plugin.semantic] section, so there is no language \
                 server to run - see plugins/rust/plugin.toml"
            )
        })?;

    let (command, version) = resolve(&config.command)?;
    eprintln!("[{LANGUAGE}] semantic tier: {} ({version})", command.display());
    config.command = command;
    Ok(Box::new(LspBridge::new(LANGUAGE, root, config)))
}

/// The server to run, and the version string it answered with.
///
/// A bare `command` (the manifest's default, `rust-analyzer`) is looked up on
/// `PATH` by the operating system - `Command::new` does that for us, on every
/// platform, including Windows' `PATHEXT` rules that a hand-rolled `PATH`
/// walk gets wrong - and, if that candidate does not behave like a server,
/// `rustup which` is asked for the toolchain's own copy.
///
/// A `command` that is a *path* is not searched for: someone who wrote a path
/// into the manifest meant that file, and quietly running a different binary
/// because theirs did not work would be the worst possible answer. It is
/// still probed, so a path that is wrong fails with the same one log line as
/// a name that is missing rather than as a server that dies at handshake.
fn resolve(command: &Path) -> Result<(PathBuf, String)> {
    let mut failures: Vec<String> = Vec::new();
    for candidate in candidates(command) {
        match probe(&candidate) {
            Ok(version) => return Ok((candidate, version)),
            Err(err) => failures.push(format!("{}: {err:#}", candidate.display())),
        }
    }
    bail!(
        "no usable rust-analyzer: {}. Install it with `rustup component add rust-analyzer`, or point \
         [plugin.semantic] command in plugins/rust/plugin.toml at one",
        failures.join("; ")
    )
}

/// The binaries worth probing, in order: what the manifest says, then - only
/// for a bare name - whatever `rustup which` resolves that name to.
fn candidates(command: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![command.to_path_buf()];
    let bare = command.components().count() == 1 && !command.is_absolute();
    if !bare {
        return candidates;
    }
    let name = command.to_string_lossy().into_owned();
    if let Some(path) = rustup_which(&name) {
        if path != candidates[0] {
            candidates.push(path);
        }
    }
    candidates
}

/// `rustup which <name>`, or `None` if rustup is not installed, does not know
/// the name, or the toolchain does not have that component.
///
/// The failure is silent here and reported by [`resolve`] as part of one
/// message, because on a machine with no rustup at all this is not news: the
/// `PATH` candidate is then the only one there ever was.
fn rustup_which(name: &str) -> Option<PathBuf> {
    let output = Command::new("rustup").args(["which", name]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Runs `<candidate> --version` and returns what it printed.
///
/// This is the whole of the "prove it" half of this module - see the module
/// doc for the rustup proxy it exists to reject. `--version` is the one
/// argument every language server that ships as a CLI binary supports, it
/// starts no workspace, and it exits immediately, so the cost is one spawn
/// per plugin process rather than per pass.
fn probe(candidate: &Path) -> Result<String> {
    let output = Command::new(candidate)
        .arg("--version")
        .output()
        .with_context(|| format!("could not run {}", candidate.display()))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.lines().next().unwrap_or("").trim();
        bail!("`--version` exited {} ({said})", output.status);
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if version.is_empty() { "no version reported".to_string() } else { version })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path names one binary and is never searched around - see
    /// [`resolve`]'s doc.
    #[test]
    fn a_command_that_is_a_path_is_the_only_candidate() {
        assert_eq!(candidates(Path::new("/opt/ra/rust-analyzer")).len(), 1);
        assert_eq!(candidates(Path::new("servers/rust-analyzer")).len(), 1);
    }

    /// A bare name is a `PATH` lookup first, and `rustup which` only as a
    /// second candidate - never the other way round, and never instead.
    #[test]
    fn a_bare_name_keeps_its_path_lookup_first() {
        let candidates = candidates(Path::new("rust-analyzer"));
        assert_eq!(candidates[0], PathBuf::from("rust-analyzer"), "the bare name stays a PATH lookup");
        assert!(candidates.len() <= 2, "{candidates:?}");
        if let Some(second) = candidates.get(1) {
            assert!(second.is_absolute(), "rustup answers with a path: {second:?}");
        }
    }

    /// The probe is what separates a server from a shim, so it has to fail
    /// for a binary that runs and exits non-zero - not only for one that is
    /// absent. `false` is the smallest such program every unix has; on
    /// Windows the absent-binary half is the one that runs.
    #[test]
    fn a_binary_that_exits_non_zero_is_not_a_server() {
        assert!(probe(Path::new("/nonexistent/rust-analyzer")).is_err(), "a binary that is not there");
        #[cfg(unix)]
        assert!(probe(Path::new("/usr/bin/false")).is_err(), "a binary that runs and refuses");
    }

    /// The shipped manifest is read by this module at run time and by nothing
    /// in this crate at build time, so without a test it is a file that can
    /// rot silently - and its `implementation_kinds` entry is the one string
    /// that has to agree with `extractor::decls`' `nativeKind` for the whole
    /// implementation sweep to find anything at all.
    #[test]
    fn the_shipped_manifest_configures_the_server_this_module_expects() {
        let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
        let config = SemanticConfig::from_manifest_at(manifest)
            .expect("plugins/rust/plugin.toml parses")
            .expect("and declares a [plugin.semantic] section");

        assert_eq!(config.command, PathBuf::from("rust-analyzer"), "a bare name, so PATH is tried first");
        assert_eq!(config.engine, "rust-analyzer", "the label on every edge this tier emits");
        assert_eq!(
            config.implementation_kinds,
            vec!["trait"],
            "must equal the `nativeKind` decls.rs gives a `trait_item`"
        );
        let options = config.initialization_options.expect("rust-analyzer is configured for a batch pass");
        assert_eq!(options.get("checkOnSave"), Some(&serde_json::json!(false)), "{options}");
        assert_eq!(
            options.get("cachePriming"),
            None,
            "cache priming stays on: turning it off moves the indexing work into the first request's \
             ten-second budget - see plugin.toml's own comment, and the measurement in it: {options}"
        );
    }

    /// `plugin.toml`'s `plugin_version` is what core now announces for this
    /// plugin everywhere - `g-mesh plugins list`, and the handshake, since
    /// GM-290 hands the plugin the manifest core read. Two sources for one
    /// number is how it drifted three releases unnoticed; this is the check
    /// that stops it, and it needs no built binary to run.
    #[test]
    fn the_manifest_version_matches_the_crates() {
        let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
        let text = std::fs::read_to_string(manifest).expect("plugins/rust/plugin.toml is readable");
        let parsed: toml::Value = toml::from_str(&text).expect("plugins/rust/plugin.toml parses");
        assert_eq!(
            parsed["plugin"]["plugin_version"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "plugin.toml's plugin_version must track Cargo.toml's version - see that file's comment"
        );
    }

    /// And the whole resolution says what to do about it rather than only
    /// that it failed.
    #[test]
    fn nothing_usable_names_the_remedy() {
        let err = resolve(Path::new("/nonexistent/rust-analyzer")).expect_err("must not resolve");
        let message = format!("{err:#}");
        assert!(message.contains("rustup component add rust-analyzer"), "{message}");
        assert!(message.contains("/nonexistent/rust-analyzer"), "{message}");
    }
}
