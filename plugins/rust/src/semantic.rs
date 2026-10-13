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
//! answers carrying a `provenance` block that says the semantic tier is
//! missing (`core::mcp::provenance`). Installing the
//! component and restarting the daemon then gets the pass; a
//! completed-but-empty pass would have recorded "done" and never asked again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use g_mesh_plugin_sdk::lsp::{self, is_bare, Candidate, LspBridge, SemanticConfig, PROBE_BUDGET};
use g_mesh_plugin_sdk::SemanticEngine;

/// The language id this plugin speaks, for `didOpen` and for log lines.
const LANGUAGE: &str = "rust";

/// Builds the semantic engine for `root` - the
/// [`SemanticEngineFactory`](g_mesh_plugin_sdk::SemanticEngineFactory) body,
/// called on the first `semanticPass` and never before.
///
/// The server runs no build script, proc macro, project toolchain or
/// project-named `rustc`/wrapper: the manifest turns build scripts and proc
/// macros off, and [`pinned_env`] pins the toolchain and the compiler over
/// whatever the project's `rust-toolchain.toml` and `.cargo/config.toml` name
/// ([ADR 0032](../../../docs/adr/0032-semantic-tiers-execute-no-project-code.md)).
/// It reaches no network either: [`pinned_env`] runs every cargo it starts
/// offline, so a dependency missing from the local cargo cache is not
/// fetched and its edges stay unresolved until the user runs `cargo fetch`
/// ([ADR 0033](../../../docs/adr/0033-semantic-tiers-reach-no-network-and-write-nothing.md)).
pub fn engine(root: &Path) -> Result<Box<dyn SemanticEngine>> {
    let mut config = SemanticConfig::from_manifest()
        .context("reading [plugin.semantic] from this plugin's manifest")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this plugin's manifest has no [plugin.semantic] section, so there is no language \
                 server to run - see plugins/rust/plugin.toml"
            )
        })?;

    let toolchain = Toolchain::detect();
    let resolved = resolve(&config.command, &toolchain)?;
    g_mesh_plugin_sdk::log_line!(
        "[{LANGUAGE}] semantic tier: {} ({}, toolchain {}, cargo offline: dependencies not in the \
         local cargo cache stay unresolved until `cargo fetch`)",
        resolved.command.display(),
        resolved.version,
        toolchain.pin.as_deref().unwrap_or("not rustup-managed")
    );
    config.command = resolved.command;
    config.env.extend(pinned_env(&toolchain));
    // GM-550: rust-analyzer's start-up phases can be separated by more than
    // the bridge's 2s settle under load (2.04s and 2.47s measured), so its
    // readiness is its own `quiescent` status rather than a quiet period.
    Ok(Box::new(LspBridge::new(LANGUAGE, root, config).quiescent_signal()))
}

/// The toolchain the server and every cargo/rustc it starts run from, found
/// without consulting the project: every `rustup` call runs in
/// [`neutral_dir`], so no `rust-toolchain.toml` is in scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Toolchain {
    /// The rustup toolchain name: the daemon's own `RUSTUP_TOOLCHAIN`, else
    /// `rustup default`. `None` without rustup or without a default.
    pin: Option<String>,
    /// The `rustup` binary every proxy runs, canonicalized (symlinks
    /// resolved). `None` when rustup is not on `PATH`.
    rustup: Option<PathBuf>,
    /// The toolchain's own binaries, as absolute paths (never proxies).
    rust_analyzer: Option<PathBuf>,
    rustc: Option<PathBuf>,
    cargo: Option<PathBuf>,
}

impl Toolchain {
    fn detect() -> Self {
        let rustup_bin = on_path("rustup").map(|rustup| std::fs::canonicalize(&rustup).unwrap_or(rustup));
        if rustup_bin.is_none() {
            // Not rustup-managed: no proxy reads a toolchain file, and the
            // binaries on `PATH` are the user's own.
            return Toolchain { rustc: on_path("rustc"), cargo: on_path("cargo"), ..Toolchain::default() };
        }
        let pin = std::env::var("RUSTUP_TOOLCHAIN").ok().filter(|pin| !pin.trim().is_empty()).or_else(|| {
            rustup(&["default"]).and_then(|out| out.split_whitespace().next().map(str::to_string))
        });
        let which = |name: &str| {
            let pin = pin.as_deref()?;
            rustup(&["which", "--toolchain", pin, name]).map(PathBuf::from)
        };
        Toolchain {
            rust_analyzer: which("rust-analyzer"),
            rustc: which("rustc"),
            cargo: which("cargo"),
            pin,
            rustup: rustup_bin,
        }
    }
}

/// The environment the server starts with on top of the inherited one: the
/// pinned toolchain, its own `rustc` and `cargo`, no `rustc` wrapper, and
/// cargo offline.
///
/// Environment variables outrank `.cargo/config.toml` (`build.rustc`,
/// `build.rustc-wrapper`, `build.rustc-workspace-wrapper`, `net.offline`) and
/// `RUSTUP_TOOLCHAIN` outranks `rust-toolchain.toml`, so a project file can
/// name none of these. An empty wrapper is cargo's "no wrapper".
/// `CARGO_ENCODED_RUSTFLAGS` empty outranks `.cargo/config.toml`
/// `[build]` and `[target.*]` `rustflags`, so no project flag (`-Z`
/// codegen-backend, `-Zcrate-attr`, ...) reaches a `rustc` that
/// rust-analyzer or cargo runs.
/// `CARGO_NET_OFFLINE` reaches every cargo the server runs (`metadata`,
/// `locate-project`, `config get`), so none of them downloads a dependency
/// or refreshes a registry index.
fn pinned_env(toolchain: &Toolchain) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("CARGO_NET_OFFLINE".to_string(), "true".to_string());
    env.insert("CARGO_ENCODED_RUSTFLAGS".to_string(), String::new());
    if let Some(pin) = &toolchain.pin {
        env.insert("RUSTUP_TOOLCHAIN".to_string(), pin.clone());
    }
    if let Some(rustc) = &toolchain.rustc {
        let rustc = rustc.to_string_lossy().into_owned();
        env.insert("RUSTC".to_string(), rustc.clone());
        env.insert("CARGO_BUILD_RUSTC".to_string(), rustc);
    }
    if let Some(cargo) = &toolchain.cargo {
        env.insert("CARGO".to_string(), cargo.to_string_lossy().into_owned());
    }
    for wrapper in [
        "RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    ] {
        env.insert(wrapper.to_string(), String::new());
    }
    env
}

/// The server to run, and the version string it answered with.
///
/// A bare `command` (the manifest's default, `rust-analyzer`) is the pinned
/// toolchain's own copy first, then a `PATH` install that is not a rustup
/// proxy - see [`candidates`].
///
/// **Correction, GM-341:** an earlier version of this comment said
/// `Command::new`'s `PATH` lookup honours Windows' `PATHEXT` rules. It does
/// not - measured false for `plugins/python`'s pyright, whose npm install is
/// a `.cmd` shim `Command::new` cannot find bare, and documented
/// `CreateProcess` behaviour rather than a `PATHEXT` walk explains why: given
/// an extensionless name, Windows' own process-creation API tries only that
/// name and `<name>.exe`, nothing else - `PATHEXT` is `cmd.exe`'s rule, not
/// `CreateProcess`'s. This plugin has no *consequence* from the same mistake,
/// and that is luck rather than design: `rustup` ships `rust-analyzer` as a
/// native `rust-analyzer.exe`, so the one extension `Command::new` does try
/// is already the right one. See `plugins/python/src/semantic.rs`'s module
/// doc, Decision 1b, for the fix that plugin needed and this one did not.
///
/// A `command` that is a *path* is not searched for: someone who wrote a path
/// into the manifest meant that file, and quietly running a different binary
/// because theirs did not work would be the worst possible answer. It is
/// still probed, so a path that is wrong fails with the same one log line as
/// a name that is missing rather than as a server that dies at handshake.
///
/// Each candidate's `--version` runs under [`PROBE_BUDGET`], like every
/// plugin's, and is itself the probe: rust-analyzer answers `--version`.
fn resolve(command: &Path, toolchain: &Toolchain) -> Result<lsp::Resolved> {
    let candidates = candidates(command, toolchain)
        .into_iter()
        .map(|(command, origin)| Candidate {
            probe: (command.clone(), Vec::new()),
            command,
            prefix_args: Vec::new(),
            origin,
        })
        .collect();
    lsp::resolve(
        candidates,
        PROBE_BUDGET,
        "rust-analyzer",
        "Install it with `rustup component add rust-analyzer`, or point [plugin.semantic] command in \
         plugins/rust/plugin.toml at one",
    )
}

/// The binaries worth probing, in order, with their origins.
///
/// A path is the only candidate. A bare name is the pinned toolchain's
/// absolute binary, then the `PATH` lookup - unless that lookup is a rustup
/// proxy ([`is_rustup_proxy`]): a proxy picks its toolchain from the working
/// directory's `rust-toolchain.toml`, so it is never probed or run.
fn candidates(command: &Path, toolchain: &Toolchain) -> Vec<(PathBuf, &'static str)> {
    if !is_bare(command) {
        return vec![(command.to_path_buf(), "the path the manifest names")];
    }
    let mut candidates = Vec::new();
    if let Some(path) = &toolchain.rust_analyzer {
        candidates.push((path.clone(), "rustup which"));
    }
    match &toolchain.rustup {
        None => candidates.push((command.to_path_buf(), "PATH")),
        Some(rustup) => {
            if let Some(found) = on_path(&command.to_string_lossy()) {
                if !is_rustup_proxy(&found, rustup) && !candidates.iter().any(|(c, _)| *c == found) {
                    candidates.push((found, "PATH"));
                }
            }
        }
    }
    candidates
}

/// Whether `found` runs `rustup` (canonicalized), i.e. is a rustup proxy.
///
/// Compared by what the file *is*, not by where it is spelled: rustup's
/// proxies are symlinks (or, on some installs, hard links) to `rustup`, so a
/// proxy in another `PATH` directory than the `rustup` found first (a
/// Homebrew rustup plus `~/.cargo/bin`), or under a symlinked spelling of the
/// same directory, is still caught. A file beside the canonical `rustup` is
/// treated as a proxy too, as the directory-only rule before it did.
fn is_rustup_proxy(found: &Path, rustup: &Path) -> bool {
    let Ok(target) = std::fs::canonicalize(found) else {
        // Unresolvable (dangling, unreadable): not a server anyone can run.
        return true;
    };
    target == rustup || target.parent() == rustup.parent() || same_inode(&target, rustup)
}

/// Two names for one file (a hard link), where the platform can tell.
#[cfg(unix)]
fn same_inode(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Two names for one file: not checked off Unix.
#[cfg(not(unix))]
fn same_inode(_: &Path, _: &Path) -> bool {
    false
}

/// `rustup <args>`'s trimmed stdout, or `None` if rustup is missing or fails.
///
/// Runs in [`neutral_dir`], so no project's `rust-toolchain.toml` is in
/// scope.
fn rustup(args: &[&str]) -> Option<String> {
    let output = Command::new("rustup").args(args).current_dir(neutral_dir()).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!out.is_empty()).then_some(out)
}

/// A working directory no project can configure: this plugin binary's own
/// directory, or the system temp directory.
fn neutral_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
}

/// The first absolute `PATH` entry holding an executable file named `name`
/// (with the platform's executable suffix tried too). Relative entries are
/// skipped: they resolve against whatever directory is current.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let spellings = [name.to_string(), format!("{name}{}", std::env::consts::EXE_SUFFIX)];
    std::env::split_paths(&path).filter(|dir| dir.is_absolute()).find_map(|dir| {
        spellings.iter().map(|spelling| dir.join(spelling)).find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use g_mesh_plugin_sdk::lsp::ServerReadiness;

    /// A path names one binary and is never searched around - see
    /// [`resolve`]'s doc.
    #[test]
    fn a_command_that_is_a_path_is_the_only_candidate() {
        let toolchain = Toolchain::detect();
        assert_eq!(candidates(Path::new("/opt/ra/rust-analyzer"), &toolchain).len(), 1);
        assert_eq!(candidates(Path::new("servers/rust-analyzer"), &toolchain).len(), 1);
    }

    /// Without rustup a bare name is the `PATH` lookup alone, as written.
    #[test]
    fn without_rustup_a_bare_name_is_a_path_lookup() {
        let candidates = candidates(Path::new("rust-analyzer"), &Toolchain::default());
        assert_eq!(candidates, vec![(PathBuf::from("rust-analyzer"), "PATH")]);
    }

    /// With rustup the pinned toolchain's absolute binary comes first, and a
    /// `PATH` hit that is a rustup proxy is never a candidate.
    #[test]
    fn with_rustup_the_pinned_binary_comes_first_and_no_proxy_follows() {
        let toolchain = Toolchain::detect();
        let candidates = candidates(Path::new("rust-analyzer"), &toolchain);
        if let (Some(pinned), Some(first)) = (&toolchain.rust_analyzer, candidates.first()) {
            assert_eq!(first, &(pinned.clone(), "rustup which"), "{candidates:?}");
        }
        if let Some(rustup) = &toolchain.rustup {
            assert!(
                candidates.iter().all(|(path, _)| path.is_absolute() && !is_rustup_proxy(path, rustup)),
                "{candidates:?}"
            );
        }
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
            options.pointer("/cargo/buildScripts/enable"),
            Some(&serde_json::json!(false)),
            "no build script runs: {options}"
        );
        assert_eq!(
            options.pointer("/procMacro/enable"),
            Some(&serde_json::json!(false)),
            "no proc macro runs: {options}"
        );
        assert_eq!(
            options.get("cachePriming"),
            None,
            "cache priming stays on: turning it off moves the indexing work into the first request's \
             ten-second budget - see plugin.toml's own comment, and the measurement in it: {options}"
        );
        assert_eq!(
            config.readiness,
            ServerReadiness::Indexed,
            "rust-analyzer answers a cross-crate definition at 4.4s and a receiver call not until \
             13.4s in the same session (GM-310's trace), so nothing it says early generalises. \
             The default, stated so the trace has somewhere to live"
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
    /// that it failed, and reports the failed path with its origin in the
    /// SDK's shared format.
    #[test]
    fn nothing_usable_names_the_remedy() {
        let err = resolve(Path::new("/nonexistent/rust-analyzer"), &Toolchain::default())
            .expect_err("must not resolve");
        let message = format!("{err:#}");
        assert!(message.contains("rustup component add rust-analyzer"), "{message}");
        assert!(
            message.starts_with(
                "no usable rust-analyzer: /nonexistent/rust-analyzer (the path the manifest names): "
            ),
            "{message}"
        );
    }

    /// A path that answers `--version` resolves to itself, with what it
    /// printed and the path's origin. `echo` exits zero and prints a
    /// non-empty line for `--version` on every unix.
    #[cfg(unix)]
    #[test]
    fn a_path_that_answers_resolves_with_its_origin() {
        let resolved = resolve(Path::new("/bin/echo"), &Toolchain::default()).expect("echo answers");
        assert_eq!(resolved.command, PathBuf::from("/bin/echo"));
        assert!(resolved.prefix_args.is_empty(), "{:?}", resolved.prefix_args);
        assert_eq!(resolved.origin, "the path the manifest names");
        assert_ne!(resolved.version, "no version reported");
    }

    /// rust-analyzer's readiness is its own `quiescent` status (GM-550), and
    /// `engine` turns that on in the bridge. The bridge keeps the flag
    /// private, so the wiring is read from this file's own `engine` body, as
    /// the TypeScript plugin pins its warm-up.
    ///
    /// Control: drop `.quiescent_signal()` from `engine`.
    #[test]
    fn the_engine_reads_rust_analyzers_quiescent_status() {
        let source = include_str!("semantic.rs");
        let engine = &source[source.find("pub fn engine(").expect("this file defines `engine`")..];
        let body = &engine[..engine.find("\n}\n").expect("`engine` ends")];
        assert!(
            body.contains("LspBridge::new(LANGUAGE, root, config).quiescent_signal()"),
            "`engine` must turn on the bridge's quiescent signal:\n{body}"
        );
    }

    /// `pinned_env` carries the toolchain pin as `RUSTUP_TOOLCHAIN`, which
    /// outranks a project's `rust-toolchain.toml` for every rustup proxy the
    /// server reaches. The integration test cannot observe it: `RUSTC` and
    /// `CARGO` name absolute binaries there, so no proxy is reached.
    ///
    /// Control C2(a): delete the `RUSTUP_TOOLCHAIN` insert in `pinned_env`.
    #[test]
    fn the_server_env_pins_the_toolchain_by_name() {
        let toolchain = Toolchain { pin: Some("gm513-pin".to_string()), ..Toolchain::default() };
        let env = pinned_env(&toolchain);
        assert_eq!(env.get("RUSTUP_TOOLCHAIN").map(String::as_str), Some("gm513-pin"), "{env:?}");

        let unpinned = pinned_env(&Toolchain::default());
        assert!(!unpinned.contains_key("RUSTUP_TOOLCHAIN"), "no pin, no RUSTUP_TOOLCHAIN: {unpinned:?}");
    }

    /// `engine` starts the server with `pinned_env` on top of the manifest's
    /// env, so the unit test above is the env the server really gets.
    #[test]
    fn the_engine_starts_the_server_with_the_pinned_env() {
        let source = include_str!("semantic.rs");
        let engine = &source[source.find("pub fn engine(").expect("this file defines `engine`")..];
        let body = &engine[..engine.find("\n}\n").expect("`engine` ends")];
        assert!(body.contains("config.env.extend(pinned_env(&toolchain));"), "{body}");
    }

    /// A scratch directory, canonicalized (macOS' temp dir is a symlink),
    /// that removes itself.
    #[cfg(unix)]
    struct Scratch(PathBuf);

    #[cfg(unix)]
    impl Scratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("g-mesh-rs-gm513-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Scratch(root.canonicalize().unwrap())
        }
    }

    #[cfg(unix)]
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An executable file at `path` that exits 1.
    #[cfg(unix)]
    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The fixture for the proxy-identity tests:
    ///
    /// - `rustup-home/rustup`, the real (canonical) rustup;
    /// - `other/rust-analyzer`, a symlink to it from another directory (a
    ///   Homebrew rustup plus `~/.cargo/bin`);
    /// - `alias`, a symlink to `rustup-home`, so `alias/rust-analyzer` is a
    ///   proxy under a symlinked `PATH` spelling;
    /// - `hard/rust-analyzer`, a hard link to `rustup`;
    /// - `own/rust-analyzer`, an unrelated real binary.
    #[cfg(unix)]
    fn proxy_fixture(name: &str) -> (Scratch, PathBuf) {
        let scratch = Scratch::new(name);
        let rustup = scratch.0.join("rustup-home/rustup");
        executable(&rustup);
        std::fs::create_dir_all(scratch.0.join("other")).unwrap();
        std::os::unix::fs::symlink(&rustup, scratch.0.join("other/rust-analyzer")).unwrap();
        std::os::unix::fs::symlink("rustup", scratch.0.join("rustup-home/rust-analyzer")).unwrap();
        std::os::unix::fs::symlink(scratch.0.join("rustup-home"), scratch.0.join("alias")).unwrap();
        std::fs::create_dir_all(scratch.0.join("hard")).unwrap();
        std::fs::hard_link(&rustup, scratch.0.join("hard/rust-analyzer")).unwrap();
        executable(&scratch.0.join("own/rust-analyzer"));
        (scratch, rustup)
    }

    /// A proxy is recognised by what the file is, not where it is spelled.
    ///
    /// Control F2: `is_rustup_proxy`'s body becomes the old
    /// `found.parent() == rustup.parent()` - the first two assertions fail.
    #[cfg(unix)]
    #[test]
    fn a_proxy_is_a_proxy_wherever_it_is_spelled() {
        let (scratch, rustup) = proxy_fixture("identity");
        let root = &scratch.0;
        assert!(is_rustup_proxy(&root.join("other/rust-analyzer"), &rustup), "a symlink from another dir");
        assert!(is_rustup_proxy(&root.join("alias/rust-analyzer"), &rustup), "a symlinked PATH dir");
        assert!(is_rustup_proxy(&root.join("hard/rust-analyzer"), &rustup), "a hard link");
        assert!(is_rustup_proxy(&root.join("rustup-home/rust-analyzer"), &rustup), "beside rustup");
        assert!(!is_rustup_proxy(&root.join("own/rust-analyzer"), &rustup), "an unrelated binary");
    }

    /// The env var that makes [`candidates_never_offer_a_proxy_child`] run.
    #[cfg(unix)]
    const CANDIDATES_CHILD: &str = "G_MESH_RS_GM513_CANDIDATES_CHILD";

    /// `candidates` drops a `PATH` hit that is a proxy under any spelling and
    /// keeps an unrelated one. `candidates` reads this process's `PATH`, so the
    /// check runs in a child test process (this binary, one test, its own
    /// `PATH`) and no other test sees the change under either runner.
    #[cfg(unix)]
    #[test]
    fn candidates_never_offer_a_proxy_under_any_path_spelling() {
        let (scratch, _) = proxy_fixture("candidates");
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "semantic::tests::candidates_never_offer_a_proxy_child", "--nocapture"])
            .env(CANDIDATES_CHILD, &scratch.0)
            .output()
            .unwrap();
        let text =
            format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "the child failed:\n{text}");
        assert!(text.contains("GM513-CHILD-RAN"), "the child test did not run:\n{text}");
    }

    /// The child half of the test above; a no-op unless [`CANDIDATES_CHILD`]
    /// names the fixture.
    #[cfg(unix)]
    #[test]
    fn candidates_never_offer_a_proxy_child() {
        let Some(root) = std::env::var_os(CANDIDATES_CHILD).map(PathBuf::from) else {
            return;
        };
        let toolchain = Toolchain { rustup: Some(root.join("rustup-home/rustup")), ..Toolchain::default() };
        for dir in ["other", "alias", "hard", "rustup-home"] {
            std::env::set_var("PATH", root.join(dir));
            let found = candidates(Path::new("rust-analyzer"), &toolchain);
            assert!(found.is_empty(), "PATH={dir}: a proxy was offered: {found:?}");
        }
        std::env::set_var("PATH", root.join("own"));
        assert_eq!(
            candidates(Path::new("rust-analyzer"), &toolchain),
            vec![(root.join("own/rust-analyzer"), "PATH")],
            "sanity: a real binary on PATH is offered"
        );
        println!("GM513-CHILD-RAN");
    }
}
