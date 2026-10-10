//! The TypeScript plugin's semantic tier: finding vtsls and handing it to the
//! SDK's generic [`LspBridge`].
//!
//! Everything about *driving* a language server is the SDK's
//! (`plugins/sdk/src/lsp/`). What is left here is which binary to run. The
//! design, with the traces behind every choice, is
//! `docs/architecture/gm-325-typescript-lsp-semantics.md`.
//!
//! # The server: vtsls
//!
//! `@vtsls/language-server` wraps tsserver and bundles its own TypeScript
//! (5.9.3 when traced), so it answers whatever TypeScript the project itself
//! pins - including a TypeScript 7 project, where typescript-language-server
//! fails at `initialize`. Its `definition` names the **bound overload** at an
//! overloaded call, so `overload_disambiguation` stays at its default and
//! the bridge's containment path binds it (design note §1.4).
//!
//! # Resolution: `PATH`, then the project's `node_modules/.bin`, then `npx`
//!
//! The SDK's [`npm_candidates`], as for pyright. vtsls answers `--version`
//! itself and exits, so there is no CLI twin: every candidate is probed as
//! the server it would run, and the `npx` probe is the server's argv with
//! `--version`. The project's own `typescript` is deliberately **not** a
//! candidate: TypeScript 6 and older have no LSP entry point, and 7's native
//! server cannot bind an imported overload (§2).
//!
//! # Failure
//!
//! A factory that returns `Err` is reported once, at the first
//! `semanticPass`, and every later pass answers an empty, *incomplete* diff
//! without starting anything (`g_mesh_plugin_sdk::semantic::LazyEngine`).
//! Incomplete leaves the semantic tier marked missing, so installing vtsls
//! and restarting the daemon gets the pass.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use g_mesh_plugin_sdk::lsp::{
    self, npm_candidates, Candidate, LspBridge, NpmServer, Resolved, SemanticConfig, HOST_SCRIPT_EXTENSIONS,
    PROBE_BUDGET,
};
use g_mesh_plugin_sdk::SemanticEngine;

/// The language id this plugin speaks, for `didOpen` and for log lines.
const LANGUAGE: &str = "typescript";

/// The `languageId` each extension is opened under. tsserver picks a file's
/// script kind from it, not from the file name, so a `.tsx` file opened as
/// `typescript` would be parsed without JSX. `.ts`, `.mts` and `.cts` are
/// `typescript`, the bridge's default.
const LANGUAGE_IDS: [(&str, &str); 5] = [
    (".tsx", "typescriptreact"),
    (".jsx", "javascriptreact"),
    (".js", "javascript"),
    (".mjs", "javascript"),
    (".cjs", "javascript"),
];

/// How long vtsls's first question may take - the SDK's `Budgets::warm_up`.
///
/// With `useSyntaxServer = "never"` the one tsserver answers nothing until it
/// has loaded the projects of the files opened before the first question, and
/// vtsls reports no progress the bridge's readiness could wait on. GM-325
/// measured that load on excalidraw (docs/results/gm-325-ts-semantic-gaps.md,
/// section 3): 12.8-24 s without `node_modules`, 24-33 s with them, at a load
/// average of 4-5; under load 12 the pass timed out four 10 s windows, so the
/// load ran past 30 s there too. 120 s is about 3.6x the slowest measured
/// load, which leaves room for a busier machine or a larger project, and is
/// still small against the whole-project pass's 15-minute floor. It is paid
/// once per server, and only in full by a server that never answers.
const WARM_UP: Duration = Duration::from_secs(120);

/// The npm package's language-server bin, the manifest's `command`.
const SERVER_BIN: &str = "vtsls";

/// The npm *package* that bin belongs to: `npx --package` wants this, and
/// `npx --yes vtsls` would ask the registry for a package called `vtsls`.
const NPM_PACKAGE: &str = "@vtsls/language-server";

/// vtsls as an npm package. The server answers `--version` itself, so it is
/// its own probe.
const VTSLS: NpmServer<'static> =
    NpmServer { package: NPM_PACKAGE, bin: SERVER_BIN, probe_bin: SERVER_BIN, twin: same_binary };

/// Builds the semantic engine for `root` - the
/// [`SemanticEngineFactory`](g_mesh_plugin_sdk::SemanticEngineFactory) body,
/// called on the first `semanticPass` and never before.
pub fn engine(root: &Path) -> Result<Box<dyn SemanticEngine>> {
    let mut config = SemanticConfig::from_manifest()
        .context("reading [plugin.semantic] from this plugin's manifest")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this plugin's manifest has no [plugin.semantic] section, so there is no language \
                 server to run - see plugins/typescript/plugin.toml"
            )
        })?;
    prepare(&mut config, root)?;
    Ok(Box::new(LspBridge::new(LANGUAGE, root, config).language_ids(&LANGUAGE_IDS).warm_up(WARM_UP)))
}

/// Turns the manifest's `config` into the one the server for `root` runs
/// with: the resolved command, then the manifest's own args after any the
/// candidate needs first (`npx`'s package).
fn prepare(config: &mut SemanticConfig, root: &Path) -> Result<()> {
    let resolved = resolve(&config.command, root, HOST_SCRIPT_EXTENSIONS)?;
    let mut args = resolved.prefix_args;
    args.extend(config.args.iter().cloned());
    g_mesh_plugin_sdk::log_line!(
        "[{LANGUAGE}] semantic tier: {} ({}, found on {})",
        resolved.command.display(),
        resolved.version,
        resolved.origin
    );
    config.command = resolved.command;
    config.args = args;
    Ok(())
}

/// The server to run, the args it needs, and the version it answered with.
///
/// `script_extensions` is [`HOST_SCRIPT_EXTENSIONS`] in production; it is a
/// parameter so tests can exercise the Windows spellings from any host.
fn resolve(command: &Path, root: &Path, script_extensions: &[&str]) -> Result<Resolved> {
    lsp::resolve(
        candidates(command, root, script_extensions),
        PROBE_BUDGET,
        "vtsls",
        "Install it with npm install -g @vtsls/language-server (or in the project, whose \
         node_modules/.bin is looked in), or point [plugin.semantic] command in \
         plugins/typescript/plugin.toml at a TypeScript language server",
    )
}

/// The spellings worth probing, in order - [`npm_candidates`] for vtsls.
fn candidates(command: &Path, root: &Path, script_extensions: &[&str]) -> Vec<Candidate> {
    npm_candidates(command, root, &VTSLS, script_extensions)
}

/// vtsls's probe path: the server itself, which answers `--version`.
fn same_binary(command: &Path) -> PathBuf {
    command.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use g_mesh_plugin_sdk::lsp::{OverloadDisambiguation, ServerReadiness, WINDOWS_SCRIPT_EXTENSIONS};

    const MANIFEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml");

    fn manifest() -> toml::Value {
        let text = std::fs::read_to_string(MANIFEST).expect("plugins/typescript/plugin.toml is readable");
        toml::from_str(&text).expect("plugins/typescript/plugin.toml parses")
    }

    /// A bare `vtsls` is `PATH`, then the project's `node_modules/.bin`, then
    /// `npx` with the package named - each spelled bare and then with every
    /// script extension before the next origin. Every installed candidate is
    /// probed as itself, and the `npx` probe is the server's own argv.
    #[test]
    fn a_bare_vtsls_is_path_then_the_projects_node_modules_then_npx() {
        let root = Path::new("/projects/thing");
        let candidates = candidates(Path::new(SERVER_BIN), root, &WINDOWS_SCRIPT_EXTENSIONS);
        let local = root.join("node_modules/.bin");
        let spelled: Vec<(&str, PathBuf)> =
            candidates.iter().map(|candidate| (candidate.origin, candidate.command.clone())).collect();
        assert_eq!(
            spelled,
            vec![
                ("PATH", PathBuf::from("vtsls")),
                ("PATH", PathBuf::from("vtsls.cmd")),
                ("the project's node_modules/.bin", local.join("vtsls")),
                ("the project's node_modules/.bin", local.join("vtsls.cmd")),
                ("npx", PathBuf::from("npx")),
                ("npx", PathBuf::from("npx.cmd")),
            ],
            "{candidates:#?}"
        );
        let npx_args = vec!["--yes", "--package", "@vtsls/language-server", "vtsls"];
        for candidate in &candidates {
            let (probe, probe_args) = &candidate.probe;
            if candidate.origin == "npx" {
                assert_eq!(candidate.prefix_args, npx_args, "{candidate:#?}");
                assert_eq!(probe, &candidate.command, "{candidate:#?}");
                assert_eq!(probe_args, &npx_args, "the probe runs the server's own bin: {candidate:#?}");
            } else {
                assert!(candidate.prefix_args.is_empty(), "{candidate:#?}");
                assert_eq!(probe, &candidate.command, "vtsls is its own probe: {candidate:#?}");
                assert!(probe_args.is_empty(), "{candidate:#?}");
            }
        }
    }

    /// Nothing usable says which server is missing, how to install it, and
    /// where to point the plugin instead.
    #[test]
    fn nothing_usable_names_vtsls_and_the_remedy() {
        let err =
            resolve(Path::new("/nonexistent/vtsls"), Path::new("/projects/thing"), HOST_SCRIPT_EXTENSIONS)
                .expect_err("must not resolve");
        let message = format!("{err:#}");
        assert!(message.contains("no usable vtsls"), "{message}");
        assert!(message.contains("npm install -g @vtsls/language-server"), "{message}");
        assert!(message.contains("plugins/typescript/plugin.toml"), "{message}");
    }

    /// The shipped manifest's semantic section and capabilities, which this
    /// module reads at run time and nothing reads at build time.
    #[test]
    fn the_shipped_manifest_configures_vtsls() {
        let config = SemanticConfig::from_manifest_at(Path::new(MANIFEST))
            .expect("plugins/typescript/plugin.toml parses")
            .expect("and declares a [plugin.semantic] section");
        assert_eq!(config.command, PathBuf::from(SERVER_BIN), "a bare name, so PATH is tried first");
        assert_eq!(config.args, vec!["--stdio"]);
        assert_eq!(config.engine, "vtsls");
        assert_eq!(config.readiness, ServerReadiness::OnDemand);
        assert!(config.implementation_kinds.is_empty(), "{:?}", config.implementation_kinds);
        assert_eq!(config.overload_disambiguation, OverloadDisambiguation::None);
        assert_eq!(
            config.settings.get(""),
            Some(&serde_json::json!({ "typescript": { "tsserver": { "useSyntaxServer": "never" } } })),
            "{:?}",
            config.settings
        );

        let parsed = manifest();
        assert!(
            parsed["plugin"]["semantic"].get("overload_disambiguation").is_none(),
            "left at its default: vtsls names the bound overload"
        );
        let capabilities = &parsed["plugin"]["capabilities"];
        assert_eq!(capabilities["semantic_pass"].as_bool(), Some(true));
        assert_eq!(capabilities["semantic_sweep"].as_bool(), Some(false));
        assert_eq!(capabilities["receiver_calls"].as_str(), Some("resolved"));
        assert_eq!(capabilities["receiver_calls_structural"].as_str(), Some("unresolved"));
    }

    /// Each extension opens under the `languageId` tsserver parses it by;
    /// `.ts`, `.mts` and `.cts` match no pair and take the bridge's language.
    #[test]
    fn each_extension_has_the_language_id_tsserver_parses_it_by() {
        let id = |file: &str| {
            LANGUAGE_IDS
                .iter()
                .find(|(extension, _)| file.ends_with(extension))
                .map_or(LANGUAGE, |(_, id)| id)
        };
        let ids: Vec<(&str, &str)> = ["a.tsx", "a.jsx", "a.js", "a.mjs", "a.cjs", "a.ts", "a.mts", "a.cts"]
            .into_iter()
            .map(|file| (file, id(file)))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("a.tsx", "typescriptreact"),
                ("a.jsx", "javascriptreact"),
                ("a.js", "javascript"),
                ("a.mjs", "javascript"),
                ("a.cjs", "javascript"),
                ("a.ts", "typescript"),
                ("a.mts", "typescript"),
                ("a.cts", "typescript"),
            ]
        );
    }

    /// vtsls's first question gets a two-minute warm-up, and `engine` hands
    /// it to the bridge. The bridge keeps its budgets private, so the wiring
    /// is read from this file's own `engine` body.
    #[test]
    fn the_engine_gives_vtsls_a_two_minute_warm_up() {
        assert_eq!(WARM_UP, Duration::from_secs(120));
        let source = include_str!("semantic.rs");
        let engine = &source[source.find("pub fn engine(").expect("this file defines `engine`")..];
        let body = &engine[..engine.find("\n}\n").expect("`engine` ends")];
        assert!(body.contains(".warm_up(WARM_UP)"), "`engine` must pass WARM_UP to the bridge:\n{body}");
    }
}
