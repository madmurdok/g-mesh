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
    Ok(Box::new(LspBridge::new(LANGUAGE, root, config).language_ids(&LANGUAGE_IDS)))
}

/// Turns the manifest's `config` into the one the server for `root` runs
/// with: the resolved command, then the manifest's own args after any the
/// candidate needs first (`npx`'s package).
fn prepare(config: &mut SemanticConfig, root: &Path) -> Result<()> {
    let resolved = resolve(&config.command, root, HOST_SCRIPT_EXTENSIONS)?;
    let mut args = resolved.prefix_args;
    args.extend(config.args.iter().cloned());
    eprintln!(
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
