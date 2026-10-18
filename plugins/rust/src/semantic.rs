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
//!
//! The same factory failure is how a project is refused the tier when a
//! cargo config in effect for it sets a dynamic-loader variable in `[env]`
//! ([ADR 0037](../../../docs/adr/0037-rust-tier-refuses-cargo-env-loader-vars.md)).
//! A config that turns hostile after the factory ran is caught by
//! `CheckedBridge`'s re-check instead: the server stops and every later
//! pass is an `Err`, which the SDK answers as an empty, incomplete diff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result};
use g_mesh_plugin_sdk::lsp::{self, is_bare, Candidate, LspBridge, SemanticConfig, PROBE_BUDGET};
use g_mesh_plugin_sdk::{RelPath, SdkIndex, SemanticAnswer, SemanticEngine};

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
///
/// A project `.cargo/config.toml`/`CARGO_HOME` config whose `[env]` sets a
/// dynamic-loader variable is refused to the structural tier, because cargo
/// applies that table to the `rustc` children of `cargo metadata` and
/// `force = true` defeats any environment override: this returns `Err`
/// naming the key and the file, and the plugin stays structural-only.
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
    if let Some(refusal) = cargo_env_loader_var(root).refusal() {
        anyhow::bail!("{refusal}");
    }
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
    if let Some(sysroot) = &toolchain.sysroot {
        pin_sysroot(&mut config.initialization_options, sysroot);
        let rust_src_path = rust_src_path(std::env::var("RUST_SRC_PATH").ok().as_deref());
        let source = std_source(config.initialization_options.as_ref(), root, sysroot);
        if let Some(hint) = std_hint(&source, sysroot, toolchain.pin.as_deref(), rust_src_path.as_deref()) {
            g_mesh_plugin_sdk::log_line!("{hint}");
        }
    }
    // GM-550: rust-analyzer's start-up phases can be separated by more than
    // the bridge's 2s settle under load (2.04s and 2.47s measured), so its
    // readiness is its own `quiescent` status rather than a quiet period.
    // rust-analyzer's own file watcher would rerun `cargo metadata` on a
    // manifest change before `CheckedBridge` could re-check the configs, so
    // the server is told the client watches files: it registers its globs
    // here and reloads only when `CheckedBridge::workspace_changed` asks.
    // The bridge forwards the `.rs` files a pass is sent, inside
    // `CheckedBridge::answer`, so even a created or deleted one (which makes
    // rust-analyzer refetch the workspace) reaches it only after a re-check.
    let bridge = LspBridge::new(LANGUAGE, root, config).quiescent_signal().client_watches_files();
    Ok(Box::new(CheckedBridge::new(root, bridge)))
}

/// The engine [`engine`] returns: the bridge, behind a re-check of the cargo
/// configs before every pass, every server start and every project reload.
///
/// The configs can change after the factory checked them (a checkout that
/// adds `.cargo/config.toml`), and rust-analyzer reruns `cargo metadata` on
/// a reload. [`SemanticEngine::workspace_changed`] asks for that reload only
/// after a clean re-check. A hostile or unreadable config stops the server
/// and makes every later pass `Err`: the plugin is structural-only from then
/// on. rust-analyzer runs no watcher of its own (the bridge claims client
/// file watching), so it hears of a change on disk only from this engine:
/// the source files a pass is sent, forwarded by the bridge inside
/// [`SemanticEngine::answer`], and a reload when
/// [`SemanticEngine::workspace_changed`] asks for one, each after a clean
/// re-check. A manifest or cargo config is never forwarded.
struct CheckedBridge {
    root: PathBuf,
    /// `None` once refused: dropping the bridge shuts its server down.
    bridge: Option<LspBridge>,
    refusal: Option<String>,
}

impl CheckedBridge {
    fn new(root: &Path, bridge: LspBridge) -> Self {
        Self { root: root.to_path_buf(), bridge: Some(bridge), refusal: None }
    }

    /// The bridge, when the configs are still clean; otherwise stops the
    /// server (once) and answers `None`.
    fn checked(&mut self) -> Option<&mut LspBridge> {
        if self.refusal.is_none() {
            if let Some(refusal) = cargo_env_loader_var(&self.root).refusal() {
                g_mesh_plugin_sdk::log_line!("[{LANGUAGE}] {refusal}; stopping the language server");
                self.bridge = None;
                self.refusal = Some(refusal);
            }
        }
        self.bridge.as_mut()
    }
}

impl SemanticEngine for CheckedBridge {
    fn answer(&mut self, files: &[RelPath], index: &SdkIndex) -> Result<SemanticAnswer> {
        match self.checked() {
            Some(bridge) => bridge.answer(files, index),
            None => Err(anyhow::anyhow!("{}", self.refusal.as_deref().unwrap_or("semantic tier refused"))),
        }
    }

    fn workspace_changed(&mut self) {
        let Some(bridge) = self.checked() else { return };
        bridge.workspace_changed();
        match bridge.request("rust-analyzer/reloadWorkspace", serde_json::Value::Null) {
            Ok(_) => {}
            Err(error) => {
                g_mesh_plugin_sdk::log_line!("[{LANGUAGE}] could not ask the server to reload: {error:#}")
            }
        }
    }

    fn prepare(&mut self) {
        if let Some(bridge) = self.checked() {
            bridge.prepare();
        }
    }

    fn set_pass_deadline(&mut self, deadline: Option<Instant>) {
        if let Some(bridge) = self.bridge.as_mut() {
            bridge.set_pass_deadline(deadline);
        }
    }
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
    /// `rustc --print sysroot` of [`Toolchain::rustc`], run in
    /// [`neutral_dir`]. The server gets it as `cargo.sysroot` (GM-603), so it
    /// loads std from this toolchain and never runs `rustup component add
    /// rust-src` itself. `None` when there is no `rustc` or it fails.
    sysroot: Option<PathBuf>,
}

impl Toolchain {
    fn detect() -> Self {
        let rustup_bin = on_path("rustup").map(|rustup| std::fs::canonicalize(&rustup).unwrap_or(rustup));
        if rustup_bin.is_none() {
            // Not rustup-managed: no proxy reads a toolchain file, and the
            // binaries on `PATH` are the user's own.
            let rustc = on_path("rustc");
            let sysroot = rustc.as_deref().and_then(sysroot_of);
            return Toolchain { rustc, cargo: on_path("cargo"), sysroot, ..Toolchain::default() };
        }
        let pin = std::env::var("RUSTUP_TOOLCHAIN").ok().filter(|pin| !pin.trim().is_empty()).or_else(|| {
            rustup(&["default"]).and_then(|out| out.split_whitespace().next().map(str::to_string))
        });
        let which = |name: &str| {
            let pin = pin.as_deref()?;
            rustup(&["which", "--toolchain", pin, name]).map(PathBuf::from)
        };
        let rustc = which("rustc");
        let sysroot = rustc.as_deref().and_then(sysroot_of);
        Toolchain {
            rust_analyzer: which("rust-analyzer"),
            rustc,
            cargo: which("cargo"),
            sysroot,
            pin,
            rustup: rustup_bin,
        }
    }
}

/// `<rustc> --print sysroot`, run in [`neutral_dir`]. `rustc` is the
/// toolchain's own binary (never a proxy), so the answer is that toolchain's
/// sysroot whatever `rust-toolchain.toml` is around.
fn sysroot_of(rustc: &Path) -> Option<PathBuf> {
    let output = Command::new(rustc).args(["--print", "sysroot"]).current_dir(neutral_dir()).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!out.is_empty()).then(|| PathBuf::from(out))
}

/// Sets `cargo.sysroot` in the server's `initializationOptions` to `sysroot`
/// (GM-603). rust-analyzer's default, `"discover"`, runs `rustup component
/// add rust-src` when the source is missing: a network download from the
/// semantic tier (ADR 0033) and, on CI, an install racing the suite. A path
/// is `RustLibSource::Path`, which never installs anything. A `cargo.sysroot`
/// already present (the manifest's, a test's) is kept, and every other key
/// is left as it is; options that are not a JSON object are left alone.
fn pin_sysroot(options: &mut Option<serde_json::Value>, sysroot: &Path) {
    let options = options.get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(options) = options.as_object_mut() else {
        return;
    };
    let cargo = options.entry("cargo").or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(cargo) = cargo.as_object_mut() else {
        return;
    };
    cargo
        .entry("sysroot")
        .or_insert_with(|| serde_json::Value::String(sysroot.to_string_lossy().into_owned()));
}

/// Where rust-analyzer will look for the standard library source, read from
/// the effective `cargo.sysroot`/`cargo.sysrootSrc` in `options` (after
/// [`pin_sysroot`], so a user-set value wins over `detected`).
#[derive(Debug, PartialEq, Eq)]
enum StdSource {
    /// `cargo.sysroot` is `null`: rust-analyzer loads no sysroot, so no
    /// rust-src would make std load.
    Disabled,
    /// A sysroot: rust-src is `lib/rustlib/src/rust/library` under it, unless
    /// `RUST_SRC_PATH` names one.
    Sysroot(PathBuf),
    /// `cargo.sysrootSrc`: the library source itself; rust-analyzer then
    /// takes it as given and does not consult `RUST_SRC_PATH`.
    Library(PathBuf),
}

/// The [`StdSource`] rust-analyzer will use, by its documented semantics of
/// `cargo.sysroot` ("relative path to the sysroot, or \"discover\" ...
/// Unsetting this disables sysroot loading") and `cargo.sysrootSrc`
/// ("relative path to the sysroot library sources ... defaults to
/// `{cargo.sysroot}/lib/rustlib/src/rust/library`"). Relative paths are
/// joined to the workspace `root` (rust-analyzer joins them to its root
/// path); an absolute one replaces it. `"discover"` runs `rustc --print
/// sysroot` under the pinned toolchain, so it is `detected`. A missing key,
/// a value of another type (which rust-analyzer rejects for its default,
/// `"discover"`) and options that are not JSON objects are `detected` too.
/// `cargo.sysrootSrc` counts only while a sysroot is loaded at all.
fn std_source(options: Option<&serde_json::Value>, root: &Path, detected: &Path) -> StdSource {
    let cargo = options.and_then(|options| options.get("cargo")).and_then(serde_json::Value::as_object);
    let sysroot = match cargo.and_then(|cargo| cargo.get("sysroot")) {
        Some(serde_json::Value::Null) => return StdSource::Disabled,
        Some(serde_json::Value::String(path)) if path != "discover" => root.join(path),
        _ => detected.to_path_buf(),
    };
    match cargo.and_then(|cargo| cargo.get("sysrootSrc")).and_then(serde_json::Value::as_str) {
        Some(library) => StdSource::Library(root.join(library)),
        None => StdSource::Sysroot(sysroot),
    }
}

/// Where rust-src puts the standard library source under a sysroot, the
/// directory rust-analyzer loads when `cargo.sysrootSrc` is unset.
const SYSROOT_LIBRARY: &str = "lib/rustlib/src/rust/library";

/// Whether rust-analyzer loads std from the library source `library`: it has
/// the `core` crate's root, `core/src/lib.rs` (or the older `libcore/lib.rs`).
/// rust-analyzer runs `cargo metadata` on `library/Cargo.toml` when that
/// exists and otherwise stitches the crates from these roots, and reports a
/// library without `core` as missing it, so `Cargo.toml` decides nothing.
fn has_core_source(library: &Path) -> bool {
    library.join("core/src/lib.rs").is_file() || library.join("libcore/lib.rs").is_file()
}

/// The library source rust-analyzer takes from `RUST_SRC_PATH` (`value`,
/// read as UTF-8: rust-analyzer ignores a value that is not): an absolute
/// path that has a `core` entry. `None` for any other value, which
/// rust-analyzer ignores in favour of the sysroot's own source. A path it
/// takes still loads no std unless it has a `core` source
/// ([`has_core_source`]).
fn rust_src_path(value: Option<&str>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute() && path.join("core").exists())
}

/// The line logged when the library source taken from `RUST_SRC_PATH`
/// (`library`) has no `core` source; `None` when it has one.
fn rust_src_path_hint(library: &Path) -> Option<String> {
    (!has_core_source(library)).then(|| {
        format!(
            "[{LANGUAGE}] std not loaded: RUST_SRC_PATH {} has no standard library source (no \
             core/src/lib.rs); point it at a `library` directory of rust-src or unset it",
            library.display()
        )
    })
}

/// The line [`engine`] logs when std will not load from `source`:
/// [`rust_src_hint`] for the `detected` sysroot (the `pin` toolchain's), a
/// line naming the path for a user-set `cargo.sysroot` or
/// `cargo.sysrootSrc` that does not exist or has no `core` source. `None`
/// when sysroot loading is disabled: no install would load std.
/// `rust_src_path` is [`rust_src_path`]'s answer; for a sysroot it replaces
/// the sysroot's own source ([`rust_src_path_hint`]), and rust-analyzer
/// ignores it for a `cargo.sysrootSrc`.
fn std_hint(
    source: &StdSource,
    detected: &Path,
    pin: Option<&str>,
    rust_src_path: Option<&Path>,
) -> Option<String> {
    match source {
        StdSource::Disabled => None,
        StdSource::Sysroot(sysroot) if sysroot == detected => rust_src_hint(sysroot, pin, rust_src_path),
        StdSource::Sysroot(sysroot) => {
            if let Some(library) = rust_src_path {
                rust_src_path_hint(library)
            } else if !sysroot.is_dir() {
                Some(format!(
                    "[{LANGUAGE}] std not loaded: cargo.sysroot {} does not exist; point it at a \
                     toolchain's sysroot (`rustc --print sysroot`)",
                    sysroot.display()
                ))
            } else if !has_core_source(&sysroot.join(SYSROOT_LIBRARY)) {
                Some(format!(
                    "[{LANGUAGE}] std not loaded: cargo.sysroot {} has no standard library source \
                     (no {SYSROOT_LIBRARY}/core/src/lib.rs); install rust-src for that toolchain or \
                     point cargo.sysroot at one that has it",
                    sysroot.display()
                ))
            } else {
                None
            }
        }
        StdSource::Library(library) if !library.is_dir() => Some(format!(
            "[{LANGUAGE}] std not loaded: cargo.sysrootSrc {} does not exist; point it at a \
             `library` directory of rust-src",
            library.display()
        )),
        StdSource::Library(library) => (!has_core_source(library)).then(|| {
            format!(
                "[{LANGUAGE}] std not loaded: cargo.sysrootSrc {} has no standard library source \
                 (no core/src/lib.rs); point it at a `library` directory of rust-src",
                library.display()
            )
        }),
    }
}

/// The line [`engine`] logs when the server will not load std from the
/// detected `sysroot`. `RUST_SRC_PATH`, which rust-analyzer consults
/// first, replaces the sysroot's source when it applies (`rust_src_path`,
/// [`rust_src_path_hint`]); otherwise std loads when the sysroot has a
/// `core` source under [`SYSROOT_LIBRARY`]. The plugin never installs
/// rust-src, so the line names the command that does when the toolchain is
/// rustup's (`pin`), else where the source goes. `None` when std will load.
fn rust_src_hint(sysroot: &Path, pin: Option<&str>, rust_src_path: Option<&Path>) -> Option<String> {
    if let Some(library) = rust_src_path {
        return rust_src_path_hint(library);
    }
    let library = sysroot.join(SYSROOT_LIBRARY);
    if has_core_source(&library) {
        return None;
    }
    Some(match pin {
        Some(pin) => format!(
            "[{LANGUAGE}] std not loaded: rust-src is not installed for toolchain {pin}; run `rustup \
             component add rust-src --toolchain {pin}`"
        ),
        None => format!(
            "[{LANGUAGE}] std not loaded: rust-src is not installed in sysroot {} (toolchain not \
             rustup-managed); install the standard library source under {}",
            sysroot.display(),
            library.display()
        ),
    })
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
/// `[build]` and `[target.*]` `rustflags`, so no `rustflags` entry of a
/// project config (`-Zcodegen-backend`, `-Zcrate-attr`, ...) reaches a
/// `rustc` that cargo runs for the server. It does not cover host units
/// when cargo honours the config's `[unstable]` table (a nightly cargo, or
/// `RUSTC_BOOTSTRAP=1`): `host-config` with `target-applies-to-host = false`
/// gives them `[host]` `rustflags` instead. `CARGO_UNSTABLE_HOST_CONFIG=false`
/// outranks that `[unstable]` key, so host units get the empty
/// `CARGO_ENCODED_RUSTFLAGS` or no flags at all. Stable cargo ignores the
/// variable. The config's `[env]` table reaches `rustc` by another route,
/// which no variable here outranks (`force = true` overrides any inherited
/// value): a dynamic-loader variable in it is refused by [`engine`] (see
/// [`cargo_env_loader_var`]), and its other entries stay outside this map.
/// `CARGO_NET_OFFLINE` reaches every cargo the server runs (`metadata`,
/// `locate-project`, `config get`), so none of them downloads a dependency
/// or refreshes a registry index.
fn pinned_env(toolchain: &Toolchain) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("CARGO_NET_OFFLINE".to_string(), "true".to_string());
    env.insert("CARGO_ENCODED_RUSTFLAGS".to_string(), String::new());
    env.insert("CARGO_UNSTABLE_HOST_CONFIG".to_string(), "false".to_string());
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

/// The dynamic-loader variables a cargo config `[env]` table may not set
/// for the server to start: each one makes the loader map a file the
/// project names into every `rustc` and `rustup` that cargo starts.
const LOADER_VARS: [&str; 6] = [
    "DYLD_INSERT_LIBRARIES",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_LIBRARY_PATH",
    "LD_PRELOAD",
    "LD_AUDIT",
    "LD_LIBRARY_PATH",
];

/// What [`cargo_env_loader_var`] concluded about the cargo configs in effect
/// for a project root.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoaderCheck {
    /// Every config cargo would read was read, and none sets a loader var.
    Clean,
    /// `file` sets `var` in its `[env]` table.
    Found { var: String, file: PathBuf },
    /// `file` may exist but could not be read (`error`), so whether it sets
    /// a loader var is unknown.
    Unchecked { file: PathBuf, error: String },
}

impl LoaderCheck {
    fn unchecked(file: &Path, error: impl std::fmt::Display) -> Self {
        Self::Unchecked { file: file.to_path_buf(), error: error.to_string() }
    }

    /// Why the semantic tier is refused, or `None` when it is not.
    fn refusal(&self) -> Option<String> {
        match self {
            Self::Clean => None,
            Self::Found { var, file } => Some(format!(
                "semantic tier refused: the cargo config {} sets {var} in [env], a dynamic-loader \
                 variable cargo would pass to every rustc it starts for rust-analyzer; this project \
                 is indexed at the structural tier only (docs/adr/0037)",
                file.display()
            )),
            Self::Unchecked { file, error } => Some(format!(
                "semantic tier refused: could not read {}: {error}; refused rather than unchecked, \
                 since a cargo config there could set a dynamic-loader variable in [env]; this \
                 project is indexed at the structural tier only (docs/adr/0037)",
                file.display()
            )),
        }
    }
}

/// Whether an I/O error means "there is no such file here" - the one error
/// that proves nothing is there to check.
fn absent(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
}

/// The first [`LOADER_VARS`] key that a cargo config in effect for `root`
/// sets in its `[env]` table, and the config file that sets it.
///
/// The configs are the ones cargo reads for a command run in `root`, in its
/// order: `.cargo/config.toml` then the legacy `.cargo/config` in `root` and
/// in each ancestor up to the filesystem root, then `config.toml` and
/// `config` in `$CARGO_HOME` (default `~/.cargo`). Each file's `include`
/// (a path, an array of paths, or tables with a `path`, relative to the
/// including file's directory) is followed transitively. The configs of
/// `root`'s immediate subdirectories are read too, because rust-analyzer
/// runs `cargo metadata` in a subdirectory holding the project's
/// `Cargo.toml` when `root` holds none.
///
/// Both `[env]` forms count: `KEY = "value"` and `KEY = { value = "...",
/// force = ..., relative = ... }`. A file that does not parse is matched by
/// text instead, so a syntax this parser does not accept and cargo does
/// cannot hide a key.
///
/// Fails closed: only a path that does not exist (`NotFound`, or a parent
/// that is not a directory) is skipped. Any other error listing `root`,
/// `stat`ing or reading a config, and a config path that exists but is not
/// a regular file, is [`LoaderCheck::Unchecked`], never `Clean`.
fn cargo_env_loader_var(root: &Path) -> LoaderCheck {
    let mut dirs: Vec<PathBuf> = Vec::new();
    match std::fs::read_dir(root) {
        Ok(children) => {
            let mut subdirs = Vec::new();
            for entry in children {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => return LoaderCheck::unchecked(root, error),
                };
                let path = entry.path();
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => {
                        if path.file_name().is_some_and(|name| name != ".cargo") {
                            subdirs.push(path);
                        }
                    }
                    Ok(_) => {}
                    Err(error) if absent(&error) => {}
                    Err(error) => return LoaderCheck::unchecked(&path, error),
                }
            }
            subdirs.sort();
            dirs.extend(subdirs.into_iter().map(|dir| dir.join(".cargo")));
        }
        Err(error) if absent(&error) => {}
        Err(error) => return LoaderCheck::unchecked(root, error),
    }
    dirs.splice(0..0, root.ancestors().map(|dir| dir.join(".cargo")));
    if let Some(cargo_home) = cargo_home() {
        dirs.push(cargo_home);
    }

    let mut visited = std::collections::HashSet::new();
    let mut pending: Vec<PathBuf> =
        dirs.iter().flat_map(|dir| [dir.join("config.toml"), dir.join("config")]).collect();
    // Depth-first in discovery order: a file's includes are read before the
    // next discovered file, so the first match is the nearest config's.
    pending.reverse();
    while let Some(file) = pending.pop() {
        match std::fs::metadata(&file) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return LoaderCheck::unchecked(&file, "it exists but is not a regular file"),
            Err(error) if absent(&error) => continue,
            Err(error) => return LoaderCheck::unchecked(&file, error),
        }
        let key = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
        if !visited.insert(key) {
            continue;
        }
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) if absent(&error) => continue,
            Err(error) => return LoaderCheck::unchecked(&file, error),
        };
        let table = match text.parse::<toml::Table>() {
            Ok(table) => table,
            Err(_) => {
                if let Some(var) = LOADER_VARS.iter().find(|var| text.contains(*var)) {
                    return LoaderCheck::Found { var: var.to_string(), file };
                }
                continue;
            }
        };
        if let Some(env) = table.get("env").and_then(toml::Value::as_table) {
            if let Some(var) = LOADER_VARS.iter().find(|var| env.contains_key(**var)) {
                return LoaderCheck::Found { var: var.to_string(), file };
            }
        }
        let base = file.parent().unwrap_or(Path::new("."));
        let mut includes =
            config_includes(table.get("include")).into_iter().map(|path| base.join(path)).collect::<Vec<_>>();
        includes.reverse();
        pending.extend(includes);
    }
    LoaderCheck::Clean
}

/// The paths a cargo config's `include` value names: a string, or an array
/// of strings and of tables with a `path`.
fn config_includes(include: Option<&toml::Value>) -> Vec<PathBuf> {
    let path_of = |value: &toml::Value| match value {
        toml::Value::String(path) => Some(PathBuf::from(path)),
        toml::Value::Table(table) => table.get("path").and_then(toml::Value::as_str).map(PathBuf::from),
        _ => None,
    };
    match include {
        Some(toml::Value::Array(items)) => items.iter().filter_map(path_of).collect(),
        Some(value) => path_of(value).into_iter().collect(),
        None => Vec::new(),
    }
}

/// Cargo's home directory: `$CARGO_HOME` when set and non-empty, else
/// `.cargo` in the user's home directory.
fn cargo_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME").filter(|home| !home.is_empty()) {
        return Some(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".cargo"))
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

    /// An empty `CARGO_ENCODED_RUSTFLAGS` outranks every config `rustflags`
    /// (GM-572): absent lets `.cargo/config.toml` `[target.*] rustflags`
    /// through, and any non-empty value is a flag of ours, not "no flags".
    #[test]
    fn the_server_env_pins_empty_encoded_rustflags() {
        for toolchain in
            [Toolchain { pin: Some("gm572-pin".to_string()), ..Toolchain::default() }, Toolchain::default()]
        {
            let env = pinned_env(&toolchain);
            assert_eq!(env.get("CARGO_ENCODED_RUSTFLAGS").map(String::as_str), Some(""), "{env:?}");
        }
    }

    /// `CARGO_UNSTABLE_HOST_CONFIG=false` outranks a config's `[unstable]
    /// host-config = true`, which under `RUSTC_BOOTSTRAP=1` hands host units
    /// `[host]` `rustflags` past the empty `CARGO_ENCODED_RUSTFLAGS`.
    ///
    /// Controls: delete the insert in `pinned_env`, or set it to "true".
    #[test]
    fn the_server_env_turns_off_unstable_host_config() {
        for toolchain in
            [Toolchain { pin: Some("gm572-pin".to_string()), ..Toolchain::default() }, Toolchain::default()]
        {
            let env = pinned_env(&toolchain);
            assert_eq!(env.get("CARGO_UNSTABLE_HOST_CONFIG").map(String::as_str), Some("false"), "{env:?}");
        }
    }

    /// The host-config channel end to end, with the real `pinned_env`: a
    /// project whose `.cargo/config.toml` turns on `[unstable] host-config`
    /// and names a `-Zcodegen-backend` dylib in `[host]` (config A) or
    /// `[host.<triple>]` (config B) `rustflags`. `RUSTC_BOOTSTRAP=1 cargo
    /// metadata` with `pinned_env` loads no dylib; the same call without
    /// `CARGO_UNSTABLE_HOST_CONFIG` loads it, so the channel is live and the
    /// first result means something. The marker is removed before every run
    /// and read after it, so each assertion names the run that fired.
    ///
    /// A unit test because it needs the private `pinned_env`; rust-analyzer's
    /// own cargo calls do not take this channel (ADR 0032 D2), so the engine
    /// pass in `tests/semantic_runs_no_project_code.rs` cannot observe it.
    /// The wrapper variables stay empty in both runs, as `pinned_env` has
    /// them: a user's global `rustc-wrapper` (sccache) runs rustc in its own
    /// process and hides the marker.
    ///
    /// Control: delete the `CARGO_UNSTABLE_HOST_CONFIG` insert in
    /// `pinned_env` - the pinned run loads the dylib.
    #[cfg(unix)]
    #[test]
    fn pinned_env_keeps_host_config_rustflags_out_of_a_bootstrapped_cargo() {
        let scratch = Scratch::new("hostcfg");
        let root = &scratch.0;
        let mark = root.join("MARK_host_backend");
        let source = root.join("marker.c");
        std::fs::write(
            &source,
            format!(
                "#include <stdio.h>\n__attribute__((constructor)) static void fire(void) {{\n    \
                 FILE *f = fopen({:?}, \"a\");\n    if (f) {{ fputs(\"x\\n\", f); fclose(f); }}\n}}\n",
                mark.to_string_lossy()
            ),
        )
        .unwrap();
        let dylib = root.join(if cfg!(target_os = "macos") { "marker.dylib" } else { "marker.so" });
        let shared: &[&str] =
            if cfg!(target_os = "macos") { &["-dynamiclib"] } else { &["-shared", "-fPIC"] };
        match Command::new("cc").args(shared).arg("-o").arg(&dylib).arg(&source).output() {
            Ok(output) if output.status.success() => {}
            result => {
                println!("SKIPPED: `cc` did not build the marker dylib ({result:?})");
                return;
            }
        }

        let toolchain = Toolchain::detect();
        let cargo = toolchain.cargo.clone().unwrap_or_else(|| PathBuf::from("cargo"));
        let rustc = toolchain.rustc.clone().unwrap_or_else(|| PathBuf::from("rustc"));
        let version = Command::new(&rustc).arg("-vV").output().unwrap();
        let triple = String::from_utf8_lossy(&version.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("host: ").map(str::to_string))
            .expect("`rustc -vV` names the host");
        let project = root.join("fx");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(project.join(".cargo")).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();

        let pinned = pinned_env(&toolchain);
        let mut unpinned = pinned.clone();
        unpinned.remove("CARGO_UNSTABLE_HOST_CONFIG");
        // Everything cargo, rustc and rustup set for this test process, so the
        // child sees only the inherited user environment plus the map.
        let inherited: Vec<String> = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| {
                (name.starts_with("CARGO") && name != "CARGO_HOME")
                    || name.starts_with("RUSTC")
                    || name.starts_with("RUSTDOC")
                    || name == "RUSTFLAGS"
            })
            .collect();
        let fires = |env: &BTreeMap<String, String>| {
            let _ = std::fs::remove_file(&mark);
            let mut command = Command::new(&cargo);
            for name in &inherited {
                command.env_remove(name);
            }
            let output = command
                .args(["metadata", "--format-version", "1"])
                .envs(env)
                .env("RUSTC_BOOTSTRAP", "1")
                .current_dir(&project)
                .output()
                .unwrap();
            (mark.is_file(), String::from_utf8_lossy(&output.stderr).into_owned())
        };

        let backend = format!("-Zcodegen-backend={}", dylib.to_string_lossy());
        for (label, table) in [("A", "host".to_string()), ("B", format!("host.{triple}"))] {
            std::fs::write(
                project.join(".cargo/config.toml"),
                format!(
                    "target-applies-to-host = false\n\n[unstable]\nhost-config = true\n\
                     target-applies-to-host = true\n\n[{table}]\nrustflags = [{backend:?}]\n"
                ),
            )
            .unwrap();
            let (fired, stderr) = fires(&unpinned);
            assert!(
                fired,
                "sanity, config {label} ([{table}]): without CARGO_UNSTABLE_HOST_CONFIG the bootstrapped \
                 cargo never loaded the project's dylib, so the pinned run would prove nothing: {stderr}"
            );
            let (fired, stderr) = fires(&pinned);
            assert!(
                !fired,
                "config {label} ([{table}]): `pinned_env` let `RUSTC_BOOTSTRAP=1 cargo metadata` load the \
                 project's `-Zcodegen-backend` dylib: {stderr}"
            );
        }
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

    /// GM-603: with no options at all, `pin_sysroot` creates
    /// `{"cargo": {"sysroot": <path>}}`, so rust-analyzer takes
    /// `RustLibSource::Path` and never runs `rustup component add rust-src`.
    ///
    /// Control: make `pin_sysroot` return before the `cargo.sysroot` insert.
    #[test]
    fn pin_sysroot_sets_cargo_sysroot_to_the_detected_sysroot() {
        let sysroot = Path::new("/gm603/toolchains/stable-x86_64-apple-darwin");
        let mut options = None;
        pin_sysroot(&mut options, sysroot);
        assert_eq!(options, Some(serde_json::json!({ "cargo": { "sysroot": sysroot.to_string_lossy() } })));

        let mut options = Some(serde_json::json!({ "checkOnSave": false }));
        pin_sysroot(&mut options, sysroot);
        assert_eq!(
            options,
            Some(
                serde_json::json!({ "checkOnSave": false, "cargo": { "sysroot": sysroot.to_string_lossy() } })
            )
        );
    }

    /// A `cargo.sysroot` the manifest or a test already set wins, and the
    /// other `cargo` keys survive the merge.
    ///
    /// Controls: replace `entry("sysroot").or_insert_with(..)` with an
    /// unconditional `insert`; or replace the `cargo` object instead of
    /// merging into it.
    #[test]
    fn pin_sysroot_keeps_an_existing_sysroot_and_the_other_cargo_keys() {
        let sysroot = Path::new("/gm603/detected");
        let mut options = Some(serde_json::json!({
            "cargo": { "sysroot": "/gm603/manifest", "buildScripts": { "enable": false } },
            "procMacro": { "enable": false }
        }));
        pin_sysroot(&mut options, sysroot);
        assert_eq!(
            options,
            Some(serde_json::json!({
                "cargo": { "sysroot": "/gm603/manifest", "buildScripts": { "enable": false } },
                "procMacro": { "enable": false }
            }))
        );

        let mut options = Some(serde_json::json!({ "cargo": { "buildScripts": { "enable": false } } }));
        pin_sysroot(&mut options, sysroot);
        assert_eq!(
            options,
            Some(serde_json::json!({
                "cargo": { "buildScripts": { "enable": false }, "sysroot": "/gm603/detected" }
            }))
        );
    }

    /// GM-603 B2: a sysroot without `lib/rustlib/src/rust/library/core/src/lib.rs`
    /// gets the hint naming the toolchain and the rustup command; one with
    /// it, or any sysroot while `RUST_SRC_PATH` names a library with a
    /// `core` source, gets none. The env var is a parameter, so no
    /// process-global state is touched.
    ///
    /// Controls: drop the `has_core_source` check (the "has rust-src" case gets a
    /// hint); drop the `rust_src_path` check (the `RUST_SRC_PATH` case
    /// gets a hint); change the remedy's `rustup component add rust-src
    /// --toolchain` wording.
    #[cfg(unix)]
    #[test]
    fn rust_src_hint_names_the_rustup_command_only_when_rust_src_is_missing() {
        let scratch = Scratch::new("gm603-rust-src");
        let sysroot = &scratch.0;
        let pin = "stable-x86_64-apple-darwin";

        let hint = rust_src_hint(sysroot, Some(pin), None).expect("no rust-src, no RUST_SRC_PATH: a hint");
        assert!(hint.contains(&format!("rustup component add rust-src --toolchain {pin}")), "{hint}");
        assert!(hint.contains("std not loaded"), "{hint}");
        let src = scratch.0.join("rust-src-path");
        std::fs::create_dir_all(src.join("core/src")).unwrap();
        std::fs::write(src.join("core/src/lib.rs"), "").unwrap();
        assert_eq!(rust_src_hint(sysroot, Some(pin), Some(&src)), None, "RUST_SRC_PATH set: no hint");
        let unpinned = rust_src_hint(sysroot, None, None).expect("no rust-src without rustup: a hint");
        assert!(unpinned.contains(&sysroot.display().to_string()), "{unpinned}");

        let core = sysroot.join("lib/rustlib/src/rust/library/core/src/lib.rs");
        std::fs::create_dir_all(core.parent().unwrap()).unwrap();
        std::fs::write(&core, "").unwrap();
        assert_eq!(rust_src_hint(sysroot, Some(pin), None), None, "rust-src installed: no hint");
        assert_eq!(rust_src_hint(sysroot, None, None), None, "rust-src installed: no hint");
    }

    /// A fake sysroot under `base/name` whose rust-src has (`true`) a
    /// `library/core/src/lib.rs` or is absent.
    #[cfg(unix)]
    fn fake_sysroot(base: &Path, name: &str, with_rust_src: bool) -> PathBuf {
        let sysroot = base.join(name);
        std::fs::create_dir_all(&sysroot).unwrap();
        if with_rust_src {
            let core = sysroot.join("lib/rustlib/src/rust/library/core/src");
            std::fs::create_dir_all(&core).unwrap();
            std::fs::write(core.join("lib.rs"), "").unwrap();
        }
        sysroot
    }

    /// GM-615: the hint follows the effective `cargo.sysroot`. A user sysroot
    /// without rust-src gets the hint naming that path, and no rustup command
    /// (rustup would install into the detected toolchain), even though the
    /// detected sysroot has rust-src.
    ///
    /// Control: `std_source` returns `detected` for a user path.
    #[cfg(unix)]
    #[test]
    fn std_hint_for_a_user_sysroot_without_rust_src_names_that_sysroot() {
        let scratch = Scratch::new("gm615-user-missing");
        let detected = fake_sysroot(&scratch.0, "detected", true);
        let user = fake_sysroot(&scratch.0, "user", false);
        let options = serde_json::json!({ "cargo": { "sysroot": user.to_str().unwrap() } });

        let source = std_source(Some(&options), &scratch.0, &detected);
        let hint = std_hint(&source, &detected, Some("stable-x"), None)
            .expect("user sysroot lacks rust-src: a hint");
        assert!(hint.contains(&user.display().to_string()), "{hint}");
        assert!(!hint.contains("rustup component add"), "{hint}");
    }

    /// GM-615: a user sysroot that has rust-src needs no hint although the
    /// detected sysroot lacks it.
    ///
    /// Control: same as above (`std_source` returns `detected`).
    #[cfg(unix)]
    #[test]
    fn std_hint_for_a_user_sysroot_with_rust_src_is_none_although_detected_lacks_it() {
        let scratch = Scratch::new("gm615-user-present");
        let detected = fake_sysroot(&scratch.0, "detected", false);
        let user = fake_sysroot(&scratch.0, "user", true);
        let options = serde_json::json!({ "cargo": { "sysroot": user.to_str().unwrap() } });

        let source = std_source(Some(&options), &scratch.0, &detected);
        assert_eq!(std_hint(&source, &detected, Some("stable-x"), None), None);
    }

    /// GM-615: no user sysroot (missing key, no options, `"discover"`, a
    /// non-string value) means the detected one; the hint then keeps the
    /// rustup command.
    ///
    /// Control: drop the `path != "discover"` guard in `std_source`.
    #[cfg(unix)]
    #[test]
    fn std_source_is_the_detected_sysroot_unless_the_user_sets_a_path() {
        let scratch = Scratch::new("gm615-discover");
        let detected = fake_sysroot(&scratch.0, "detected", false);
        let expected = StdSource::Sysroot(detected.clone());

        for options in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({ "cargo": {} })),
            Some(serde_json::json!({ "cargo": { "sysroot": "discover" } })),
            Some(serde_json::json!({ "cargo": { "sysroot": 7 } })),
            Some(serde_json::json!("not an object")),
        ] {
            assert_eq!(std_source(options.as_ref(), &scratch.0, &detected), expected, "{options:?}");
        }
        let hint =
            std_hint(&expected, &detected, Some("stable-x"), None).expect("detected lacks rust-src: a hint");
        assert!(hint.contains("rustup component add rust-src --toolchain stable-x"), "{hint}");
    }

    /// GM-615: `cargo.sysroot: null` disables sysroot loading, so no hint
    /// whatever is installed.
    ///
    /// Control: delete the `Value::Null` arm of `std_source`.
    #[cfg(unix)]
    #[test]
    fn std_hint_is_none_when_the_sysroot_is_null() {
        let scratch = Scratch::new("gm615-null");
        let detected = fake_sysroot(&scratch.0, "detected", false);
        let options = serde_json::json!({ "cargo": { "sysroot": null } });

        let source = std_source(Some(&options), &scratch.0, &detected);
        assert_eq!(source, StdSource::Disabled);
        assert_eq!(std_hint(&source, &detected, Some("stable-x"), None), None);
    }

    /// GM-615: a relative `cargo.sysroot` is joined to the workspace root; an
    /// absolute one replaces it.
    ///
    /// Control: `root.join(path)` -> `PathBuf::from(path)`.
    #[test]
    fn std_source_joins_a_relative_sysroot_to_the_workspace_root() {
        let root = Path::new("/ws/root");
        let detected = Path::new("/detected");
        let relative = serde_json::json!({ "cargo": { "sysroot": "tools/sysroot" } });
        let absolute = serde_json::json!({ "cargo": { "sysroot": "/abs/sysroot" } });

        assert_eq!(
            std_source(Some(&relative), root, detected),
            StdSource::Sysroot(PathBuf::from("/ws/root/tools/sysroot"))
        );
        assert_eq!(
            std_source(Some(&absolute), root, detected),
            StdSource::Sysroot(PathBuf::from("/abs/sysroot"))
        );
    }

    /// GM-615: `cargo.sysrootSrc` is the library itself: the hint is decided by
    /// its `core` source alone, not by the sysroot's rust-src or `RUST_SRC_PATH`.
    ///
    /// Control: `std_source` returns `StdSource::Sysroot(sysroot)` for a
    /// `sysrootSrc`.
    #[cfg(unix)]
    #[test]
    fn std_hint_for_sysroot_src_depends_only_on_that_directorys_cargo_toml() {
        let scratch = Scratch::new("gm615-sysroot-src");
        let detected = fake_sysroot(&scratch.0, "detected", true);
        let missing = scratch.0.join("lib-missing");
        let present = scratch.0.join("lib-present");
        std::fs::create_dir_all(&missing).unwrap();
        std::fs::create_dir_all(present.join("core/src")).unwrap();
        std::fs::write(present.join("core/src/lib.rs"), "").unwrap();
        let src_options = |library: &str| serde_json::json!({ "cargo": { "sysrootSrc": library } });

        // Relative to the root; the detected sysroot's own rust-src is irrelevant.
        let source = std_source(Some(&src_options("lib-missing")), &scratch.0, &detected);
        assert_eq!(source, StdSource::Library(missing.clone()));
        let hint = std_hint(&source, &detected, Some("stable-x"), None).expect("no core source: a hint");
        assert!(hint.contains(&missing.display().to_string()), "{hint}");
        assert!(hint.contains("cargo.sysrootSrc"), "{hint}");
        assert!(!hint.contains("rustup component add"), "{hint}");
        // RUST_SRC_PATH does not apply to an explicit sysrootSrc.
        assert!(std_hint(&source, &detected, Some("stable-x"), Some(&present)).is_some());

        let source = std_source(Some(&src_options("lib-present")), &scratch.0, &detected);
        assert_eq!(std_hint(&source, &detected, None, None), None);

        // With the detected sysroot lacking rust-src, a present sysrootSrc still needs no hint.
        let bare = fake_sysroot(&scratch.0, "bare", false);
        let source = std_source(Some(&src_options("lib-present")), &scratch.0, &bare);
        assert_eq!(std_hint(&source, &bare, Some("stable-x"), None), None);
    }

    /// GM-615: the rustup command is named only for the detected sysroot (the
    /// pinned toolchain's); any other sysroot gets the generic remedy.
    ///
    /// Control: `pin.filter(|_| sysroot == detected)` -> `pin` in `std_hint`.
    #[cfg(unix)]
    #[test]
    fn std_hint_names_rustup_only_for_the_detected_sysroot() {
        let scratch = Scratch::new("gm615-rustup-only");
        let detected = fake_sysroot(&scratch.0, "detected", false);
        let other = fake_sysroot(&scratch.0, "other", false);
        let pin = Some("stable-x");

        let own = std_hint(&StdSource::Sysroot(detected.clone()), &detected, pin, None).expect("a hint");
        assert!(own.contains("rustup component add rust-src --toolchain stable-x"), "{own}");
        let foreign = std_hint(&StdSource::Sysroot(other.clone()), &detected, pin, None).expect("a hint");
        assert!(!foreign.contains("rustup component add"), "{foreign}");
        assert!(foreign.contains(&other.display().to_string()), "{foreign}");
    }

    /// `sysroot_of` runs `<rustc> --print sysroot` and returns its trimmed
    /// stdout; a failing or silent `rustc` gives `None`. The fake `rustc`
    /// is written by a child shell so no file descriptor to it is open in
    /// this (multithreaded) process when it is executed (`ETXTBSY`).
    ///
    /// Controls: return `None` unconditionally from `sysroot_of`; drop the
    /// `status.success()` check (the failing fake yields its stdout).
    #[cfg(unix)]
    #[test]
    fn sysroot_of_answers_rustc_print_sysroot() {
        let scratch = Scratch::new("gm603-sysroot-of");
        let fake = |name: &str, body: &str| {
            let path = scratch.0.join(name);
            let status = Command::new("sh")
                .arg("-c")
                .arg("printf '%s\\n' \"$2\" > \"$1\" && chmod 755 \"$1\"")
                .arg("sh")
                .arg(&path)
                .arg(format!("#!/bin/sh\n{body}"))
                .status()
                .unwrap();
            assert!(status.success());
            path
        };
        let good = fake(
            "rustc-good",
            "[ \"$1\" = --print ] && [ \"$2\" = sysroot ] && [ $# -eq 2 ] || exit 3\necho '  /gm603/sysroot  '",
        );
        assert_eq!(sysroot_of(&good), Some(PathBuf::from("/gm603/sysroot")));
        let failing = fake("rustc-failing", "echo /gm603/sysroot\nexit 1");
        assert_eq!(sysroot_of(&failing), None, "a failing rustc has no sysroot");
        let silent = fake("rustc-silent", "exit 0");
        assert_eq!(sysroot_of(&silent), None, "an empty answer is no sysroot");
        assert_eq!(sysroot_of(&scratch.0.join("no-such-rustc")), None);
    }

    /// `Toolchain::detect` fills `sysroot` from the toolchain's own `rustc`:
    /// whenever it found a `rustc`, the sysroot it reports has
    /// `lib/rustlib` (every rustc sysroot does, with or without rust-src).
    /// Skips on a machine with no `rustc` at all.
    ///
    /// Control: drop `sysroot` from either `Toolchain` literal in `detect`
    /// (leave it `None`).
    #[test]
    fn detect_fills_the_sysroot_of_the_toolchains_rustc() {
        let toolchain = Toolchain::detect();
        let Some(rustc) = &toolchain.rustc else {
            println!("SKIPPED: no rustc found");
            return;
        };
        let sysroot =
            toolchain.sysroot.as_ref().unwrap_or_else(|| panic!("{rustc:?} has no sysroot: {toolchain:?}"));
        assert!(sysroot.join("lib/rustlib").is_dir(), "{sysroot:?} is not a rustc sysroot");
        assert_eq!(toolchain.sysroot, sysroot_of(rustc));
    }

    /// `engine` pins the sysroot and logs the hint after `pinned_env`, and
    /// before the bridge takes the config. The engine cannot be run here
    /// without starting rust-analyzer, so this reads `engine`'s body, like
    /// the tests above.
    ///
    /// Controls: delete the `pin_sysroot` call in `engine`; or move it below
    /// `LspBridge::new`; or pass `None` for `rust_src_path`.
    #[test]
    fn the_engine_pins_the_sysroot_and_hints_after_the_pinned_env() {
        let source = include_str!("semantic.rs");
        let engine = &source[source.find("pub fn engine(").expect("this file defines `engine`")..];
        let body = &engine[..engine.find("\n}\n").expect("`engine` ends")];
        let at =
            |needle: &str| body.find(needle).unwrap_or_else(|| panic!("`engine` lacks `{needle}`:\n{body}"));
        let env = at("config.env.extend(pinned_env(&toolchain));");
        let pin = at("pin_sysroot(&mut config.initialization_options, sysroot);");
        let source = at("std_source(config.initialization_options.as_ref(), root, sysroot)");
        let hint = at("std_hint(&source, sysroot, toolchain.pin.as_deref(), rust_src_path.as_deref())");
        let bridge = at("LspBridge::new(LANGUAGE, root, config)");
        assert!(env < pin && pin < source && source < hint && hint < bridge, "{body}");
        assert!(body.contains("std::env::var(\"RUST_SRC_PATH\")"), "{body}");
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

    // -----------------------------------------------------------------------
    // GM-587: a cargo config `[env]` loader variable refuses the semantic
    // tier. Every detection runs in a child of this test binary with
    // `CARGO_HOME` pointed at a scratch directory, so no assertion reads the
    // real `~/.cargo` and no other test sees the variable change.
    // -----------------------------------------------------------------------

    /// The child's fixture root; set only for the child.
    const LOADER_CHILD_ROOT: &str = "G_MESH_RS_GM587_LOADER_CHILD_ROOT";
    /// `detect` runs [`cargo_env_loader_var`], `engine` runs [`engine`].
    const LOADER_CHILD_MODE: &str = "G_MESH_RS_GM587_LOADER_CHILD_MODE";

    /// A scratch directory (canonical, so ancestors and file names compare
    /// exactly) that removes itself.
    struct LoaderScratch(PathBuf);

    impl LoaderScratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("g-mesh-rs-gm587-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            LoaderScratch(root.canonicalize().unwrap())
        }

        /// Writes `contents` at `rel`, creating its directories.
        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for LoaderScratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Runs the child in `mode` on `root` with `CARGO_HOME=cargo_home`, and
    /// returns the line it printed after `GM587-CHILD: `.
    fn loader_child(mode: &str, root: &Path, cargo_home: &Path) -> String {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "semantic::tests::cargo_env_loader_var_child", "--nocapture"])
            .env(LOADER_CHILD_ROOT, root)
            .env(LOADER_CHILD_MODE, mode)
            .env("CARGO_HOME", cargo_home)
            .env(g_mesh_plugin_sdk::MANIFEST_PATH_ENV, concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"))
            .output()
            .unwrap();
        let text =
            format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "the child failed:\n{text}");
        text.lines()
            .find_map(|line| line.strip_prefix("GM587-CHILD: "))
            .unwrap_or_else(|| panic!("the child test did not run:\n{text}"))
            .to_string()
    }

    /// What [`cargo_env_loader_var`] finds for `scratch/root_rel`, with
    /// `scratch/home_rel` as `CARGO_HOME`: `Some((key, file))` or `None`.
    fn detect(scratch: &LoaderScratch, root_rel: &str, home_rel: &str) -> Option<(String, PathBuf)> {
        let home = scratch.0.join(home_rel);
        std::fs::create_dir_all(&home).unwrap();
        let root = scratch.0.join(root_rel);
        std::fs::create_dir_all(&root).unwrap();
        let line = loader_child("detect", &root, &home);
        if line == "none" {
            return None;
        }
        assert!(!line.starts_with("unchecked "), "the check could not read a config: {line}");
        let (key, file) = line.split_once('|').expect("the child prints key|file");
        Some((key.to_string(), PathBuf::from(file)))
    }

    /// The child half of the tests below; a no-op unless
    /// [`LOADER_CHILD_ROOT`] names a fixture.
    #[test]
    fn cargo_env_loader_var_child() {
        let Some(root) = std::env::var_os(LOADER_CHILD_ROOT).map(PathBuf::from) else {
            return;
        };
        match std::env::var(LOADER_CHILD_MODE).as_deref() {
            Ok("detect") => match cargo_env_loader_var(&root) {
                LoaderCheck::Found { var, file } => println!("GM587-CHILD: {var}|{}", file.display()),
                LoaderCheck::Clean => println!("GM587-CHILD: none"),
                LoaderCheck::Unchecked { file, error } => {
                    println!("GM587-CHILD: unchecked {}: {}", file.display(), error.replace('\n', " "))
                }
            },
            Ok("engine") => match engine(&root) {
                Err(error) => println!("GM587-CHILD: err {}", format!("{error:#}").replace('\n', " ")),
                Ok(_) => println!("GM587-CHILD: ok"),
            },
            mode => panic!("unknown child mode {mode:?}"),
        }
    }

    /// The `{ value, force = true }` form: what a hostile project writes,
    /// because `force` beats any environment override.
    #[test]
    fn a_forced_loader_var_in_the_root_config_is_found() {
        let scratch = LoaderScratch::new("forced");
        let config = scratch.write(
            "ws/.cargo/config.toml",
            "[env]\nDYLD_INSERT_LIBRARIES = { value = \"/x/evil.dylib\", force = true }\n",
        );
        assert_eq!(detect(&scratch, "ws", "home"), Some(("DYLD_INSERT_LIBRARIES".to_string(), config)));
    }

    #[test]
    fn a_scalar_loader_var_is_found() {
        let scratch = LoaderScratch::new("scalar");
        let config = scratch.write("ws/.cargo/config.toml", "[env]\nLD_PRELOAD = \"/x/evil.so\"\n");
        assert_eq!(detect(&scratch, "ws", "home"), Some(("LD_PRELOAD".to_string(), config)));
    }

    #[test]
    fn a_table_form_without_force_is_found() {
        let scratch = LoaderScratch::new("noforce");
        let config = scratch.write(
            "ws/.cargo/config.toml",
            "[env]\nLD_AUDIT = { value = \"evil.so\", force = false, relative = true }\n",
        );
        assert_eq!(detect(&scratch, "ws", "home"), Some(("LD_AUDIT".to_string(), config)));
    }

    /// Every key of D3's list is refused, not only the two the fixture uses.
    #[test]
    fn every_listed_loader_var_is_found() {
        let scratch = LoaderScratch::new("keys");
        for key in [
            "DYLD_INSERT_LIBRARIES",
            "DYLD_FRAMEWORK_PATH",
            "DYLD_LIBRARY_PATH",
            "LD_PRELOAD",
            "LD_AUDIT",
            "LD_LIBRARY_PATH",
        ] {
            let config = scratch.write("ws/.cargo/config.toml", &format!("[env]\n{key} = \"/x\"\n"));
            assert_eq!(detect(&scratch, "ws", "home"), Some((key.to_string(), config)), "{key}");
        }
    }

    /// A benign `[env]`, and a loader name outside `[env]`, are not refused.
    #[test]
    fn a_benign_env_table_is_not_refused() {
        let scratch = LoaderScratch::new("benign");
        scratch.write(
            "ws/.cargo/config.toml",
            "[env]\nRUST_LOG = \"debug\"\nMY_FLAG = { value = \"1\", force = true }\n\n\
             [alias]\nLD_PRELOAD = \"build\"\n",
        );
        scratch.write("home/config.toml", "[env]\nOTHER = \"x\"\n");
        assert_eq!(detect(&scratch, "ws", "home"), None);
    }

    /// Cargo reads `.cargo/config.toml` in every ancestor of the directory
    /// it runs in.
    #[test]
    fn a_loader_var_in_a_parent_directory_config_is_found() {
        let scratch = LoaderScratch::new("parent");
        let config = scratch.write(
            "outer/.cargo/config.toml",
            "[env]\nDYLD_LIBRARY_PATH = { value = \"/x\", force = true }\n",
        );
        scratch.write("outer/ws/Cargo.toml", "[workspace]\n");
        assert_eq!(detect(&scratch, "outer/ws", "home"), Some(("DYLD_LIBRARY_PATH".to_string(), config)));
    }

    #[test]
    fn a_loader_var_in_the_legacy_config_file_is_found() {
        let scratch = LoaderScratch::new("legacy");
        let config = scratch.write("ws/.cargo/config", "[env]\nLD_LIBRARY_PATH = \"/x\"\n");
        assert_eq!(detect(&scratch, "ws", "home"), Some(("LD_LIBRARY_PATH".to_string(), config)));
    }

    #[test]
    fn a_loader_var_in_the_cargo_home_config_is_found() {
        let scratch = LoaderScratch::new("cargohome");
        scratch.write("ws/Cargo.toml", "[workspace]\n");
        let config = scratch.write("home/config.toml", "[env]\nDYLD_FRAMEWORK_PATH = \"/x\"\n");
        assert_eq!(detect(&scratch, "ws", "home"), Some(("DYLD_FRAMEWORK_PATH".to_string(), config)));
    }

    /// rust-analyzer runs `cargo metadata` in a subdirectory holding the
    /// `Cargo.toml` when the root holds none, so that subdirectory's
    /// config is in effect too.
    #[test]
    fn a_loader_var_in_an_immediate_subdirectory_config_is_found() {
        let scratch = LoaderScratch::new("subdir");
        let config = scratch.write("ws/crate/.cargo/config.toml", "[env]\nLD_PRELOAD = \"/x\"\n");
        scratch.write("ws/crate/Cargo.toml", "[package]\nname = \"c\"\n");
        assert_eq!(detect(&scratch, "ws", "home"), Some(("LD_PRELOAD".to_string(), config)));
    }

    /// `include` as a string, relative to the including file's directory.
    #[test]
    fn a_loader_var_in_an_included_file_is_found() {
        let scratch = LoaderScratch::new("include-str");
        scratch.write("ws/.cargo/config.toml", "include = \"extra.toml\"\n");
        let config =
            scratch.write("ws/.cargo/extra.toml", "[env]\nLD_PRELOAD = { value = \"/x\", force = true }\n");
        assert_eq!(detect(&scratch, "ws", "home"), Some(("LD_PRELOAD".to_string(), config)));
    }

    /// `include` as an array of strings and `{ path }` tables, followed
    /// transitively.
    #[test]
    fn includes_in_array_and_table_form_are_followed_transitively() {
        let scratch = LoaderScratch::new("include-arr");
        scratch
            .write("ws/.cargo/config.toml", "include = [\"benign.toml\", { path = \"sub/first.toml\" }]\n");
        scratch.write("ws/.cargo/benign.toml", "[env]\nRUST_LOG = \"x\"\n");
        scratch
            .write("ws/.cargo/sub/first.toml", "include = [{ path = \"second.toml\", optional = true }]\n");
        let config = scratch.write("ws/.cargo/sub/second.toml", "[env]\nDYLD_INSERT_LIBRARIES = \"/x\"\n");
        assert_eq!(detect(&scratch, "ws", "home"), Some(("DYLD_INSERT_LIBRARIES".to_string(), config)));
    }

    /// A config this parser rejects still cannot hide a key: it is matched
    /// by text.
    #[test]
    fn an_unparseable_config_is_matched_by_text() {
        let scratch = LoaderScratch::new("unparseable");
        let config = scratch.write("ws/.cargo/config.toml", "[env\nLD_PRELOAD = \"/x\"\n");
        assert_eq!(detect(&scratch, "ws", "home"), Some(("LD_PRELOAD".to_string(), config)));
    }

    /// `engine` refuses with an error naming the key and the file, so the
    /// plugin stays structural (D1).
    #[test]
    fn the_engine_refuses_naming_the_key_and_the_config_file() {
        let scratch = LoaderScratch::new("engine");
        std::fs::create_dir_all(scratch.0.join("home")).unwrap();
        let config = scratch.write(
            "ws/.cargo/config.toml",
            "[env]\nDYLD_INSERT_LIBRARIES = { value = \"/x/evil.dylib\", force = true }\n",
        );
        let line = loader_child("engine", &scratch.0.join("ws"), &scratch.0.join("home"));
        let reason = line.strip_prefix("err ").unwrap_or_else(|| panic!("the engine did not refuse: {line}"));
        assert!(reason.contains("DYLD_INSERT_LIBRARIES"), "the refusal does not name the key: {reason}");
        assert!(
            reason.contains(&config.display().to_string()),
            "the refusal does not name {}: {reason}",
            config.display()
        );
    }

    // -----------------------------------------------------------------------
    // The check fails closed: a config path it cannot stat, read or list is
    // a refusal naming that path, never "clean".
    // -----------------------------------------------------------------------

    /// What [`cargo_env_loader_var`] could not read for `root` (with
    /// `scratch/home` as `CARGO_HOME`): the file it names and the error.
    /// Panics when the check came back clean or found a key instead.
    fn unchecked(scratch: &LoaderScratch, root: &Path) -> (PathBuf, String) {
        let home = scratch.0.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let line = loader_child("detect", root, &home);
        let rest = line
            .strip_prefix("unchecked ")
            .unwrap_or_else(|| panic!("a config the check could not read did not refuse: {line}"));
        let (file, error) = rest.split_once(": ").expect("the child prints `unchecked <file>: <error>`");
        (PathBuf::from(file), error.to_string())
    }

    /// Sets `path`'s mode, and puts `restore` back on drop so the scratch
    /// directory can be removed even when the test fails.
    #[cfg(unix)]
    struct ModeGuard {
        path: PathBuf,
        restore: u32,
    }

    #[cfg(unix)]
    impl ModeGuard {
        fn set(path: &Path, mode: u32, restore: u32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            ModeGuard { path: path.to_path_buf(), restore }
        }
    }

    #[cfg(unix)]
    impl Drop for ModeGuard {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.restore));
        }
    }

    /// Whether this process is denied a mode-000 file. Root (and any process
    /// with the capability to bypass file modes) is not, so a test that needs
    /// an unreadable path skips there instead of passing for the wrong reason.
    #[cfg(unix)]
    fn file_modes_are_enforced(scratch: &LoaderScratch) -> bool {
        let probe = scratch.write("mode-probe", "x");
        let enforced = {
            let _locked = ModeGuard::set(&probe, 0o000, 0o644);
            std::fs::read(&probe).is_err()
        };
        if !enforced {
            println!("SKIPPED: this process reads a mode-000 file (root?), so no path is unreadable to it");
        }
        enforced
    }

    /// A `.cargo/config.toml` that exists but is a directory is not "no
    /// config here": the check refuses, naming it, and so does the engine.
    #[test]
    fn a_config_path_that_is_a_directory_refuses_naming_it() {
        let scratch = LoaderScratch::new("config-is-dir");
        let config = scratch.0.join("ws/.cargo/config.toml");
        std::fs::create_dir_all(&config).unwrap();
        let (file, error) = unchecked(&scratch, &scratch.0.join("ws"));
        assert_eq!(file, config, "the refusal names another path ({error})");

        let line = loader_child("engine", &scratch.0.join("ws"), &scratch.0.join("home"));
        let reason = line.strip_prefix("err ").unwrap_or_else(|| panic!("the engine did not refuse: {line}"));
        assert!(
            reason.contains(&config.display().to_string()),
            "the refusal does not name {}: {reason}",
            config.display()
        );
    }

    /// A config whose directory cannot be searched `stat`s with
    /// `PermissionDenied`, not `NotFound`: whatever it says is unknown, so
    /// the check refuses, naming the config path.
    #[cfg(unix)]
    #[test]
    fn a_config_whose_directory_is_unsearchable_refuses_naming_it() {
        let scratch = LoaderScratch::new("config-dir-locked");
        if !file_modes_are_enforced(&scratch) {
            return;
        }
        let config = scratch.write("ws/.cargo/config.toml", "[env]\nLD_PRELOAD = \"/x/evil.so\"\n");
        let _locked = ModeGuard::set(&scratch.0.join("ws/.cargo"), 0o000, 0o755);
        let (file, error) = unchecked(&scratch, &scratch.0.join("ws"));
        assert_eq!(file, config, "the refusal names another path ({error})");
    }

    /// A config that `stat`s as a regular file but cannot be read refuses,
    /// naming it.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_config_file_refuses_naming_it() {
        let scratch = LoaderScratch::new("config-unreadable");
        if !file_modes_are_enforced(&scratch) {
            return;
        }
        let config = scratch.write("ws/.cargo/config.toml", "[env]\nLD_PRELOAD = \"/x/evil.so\"\n");
        let _locked = ModeGuard::set(&config, 0o000, 0o644);
        let (file, error) = unchecked(&scratch, &scratch.0.join("ws"));
        assert_eq!(file, config, "the refusal names another path ({error})");
    }

    /// A root that cannot be listed hides its subdirectories' configs: the
    /// check refuses, naming the root. The root stays searchable (`-wx`), so
    /// its own `.cargo/config.toml` is a clean `NotFound` and only the
    /// listing fails.
    #[cfg(unix)]
    #[test]
    fn an_unlistable_root_refuses_naming_it() {
        let scratch = LoaderScratch::new("root-unlistable");
        if !file_modes_are_enforced(&scratch) {
            return;
        }
        let root = scratch.0.join("ws");
        scratch.write("ws/app/.cargo/config.toml", "[env]\nLD_PRELOAD = \"/x/evil.so\"\n");
        let _locked = ModeGuard::set(&root, 0o300, 0o755);
        let (file, error) = unchecked(&scratch, &root);
        assert_eq!(file, root, "the refusal names another path ({error})");
    }
}
