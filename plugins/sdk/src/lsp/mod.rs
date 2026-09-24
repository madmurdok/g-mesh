//! `LspBridge`: a [`SemanticEngine`](crate::SemanticEngine) that answers a
//! structural pass's open sites by driving a language server, with nothing in
//! it that knows which language.
//!
//! # What this is for
//!
//! `docs/architecture/multi-language-plugins.md` ("Interfaces > Plugin SDK")
//! sketches it in five lines: spawn the server the manifest names, run
//! `initialize` and mirror the SDK's file cache into it, ask
//! `textDocument/definition` at each open site and `textDocument/implementation`
//! on each trait-like node, map every location back to a node by position, and
//! bound the whole thing. This module is that, and the argument for each
//! decision the sketch left open is in the module or type it belongs to:
//!
//! | Decision | Where it is argued |
//! |---|---|
//! | 1. The manifest's `[plugin.semantic]`, read by the plugin, not by core | [`config`] |
//! | 2. No LSP crate: six requests, by hand | [`client`] |
//! | 3. Column units, negotiated and converted | [`position`] |
//! | 4. Readiness when `$/progress` is optional | [`LspBridge`] |
//! | 5. What a semantic answer may retract | [`LspBridge`] |
//! | 6. Per-request, per-pass and concurrency budgets | [`Budgets`] |
//! | 7. Reporting a pass that did not finish | [`SemanticAnswer`](crate::SemanticAnswer) |
//! | 8. A server's readiness *shape*, declared rather than waited out | [`config`] and [`ServerReadiness`] |
//!
//! # Generic by construction
//!
//! The acceptance condition for the task that built this was that no language
//! name appears in it, and that is a structural property rather than a
//! discipline: every language-shaped fact the bridge needs is a value it is
//! handed.
//!
//! - **Which server to run** is [`SemanticConfig::command`] - a string from
//!   the plugin's own `plugin.toml`.
//! - **What to label its edges** is [`SemanticConfig::engine`].
//! - **Which nodes have implementors** is
//!   [`SemanticConfig::implementation_kinds`] - the `nativeKind`s a language
//!   calls "trait", "interface", "protocol". This is the one place a bridge
//!   would otherwise have had to know a language, and it is a string
//!   comparison against a list the manifest supplies.
//! - **What to ask about** is [`OpenSite`](crate::OpenSite), which the
//!   extractor produced, carrying a position and a name and nothing about
//!   syntax.
//! - **What an answer means** is [`SdkIndex`](crate::SdkIndex): a location
//!   becomes a node by position, and the node's own `container` and
//!   `qualifiedName` become the address. No parsing, no naming rules, no
//!   knowledge of what a module path looks like.
//!
//! The language's *name* is carried through only as a label for log lines and
//! for `didOpen`'s `languageId`, which is a string the server matches against
//! its own configuration.
//!
//! # A plugin wires it up like this
//!
//! ```no_run
//! use g_mesh_plugin_sdk::lsp::{LspBridge, SemanticConfig};
//! use g_mesh_plugin_sdk::{run, PluginSpec, SemanticEngine};
//! # use g_mesh_plugin_sdk::{Extractor, FileGraph, RelPath};
//! # struct MyExtractor;
//! # impl Extractor for MyExtractor {
//! #     const LANGUAGE: &'static str = "mylang";
//! #     type Project = ();
//! #     fn load_project(&self, _root: &std::path::Path) -> anyhow::Result<()> { Ok(()) }
//! #     fn extract(&self, _p: &(), _path: &RelPath, _source: &str) -> FileGraph { FileGraph::default() }
//! # }
//!
//! fn main() -> ! {
//!     run(
//!         MyExtractor,
//!         PluginSpec::new("mylang", env!("CARGO_PKG_VERSION"), &[".ml"]),
//!         // Called on the first `semanticPass` and never before, with the
//!         // project root - so nothing here runs during structural work.
//!         Some(Box::new(|root| {
//!             let config = SemanticConfig::from_manifest()?
//!                 .ok_or_else(|| anyhow::anyhow!("plugin.toml has no [plugin.semantic] section"))?;
//!             Ok(Box::new(LspBridge::new("mylang", root, config)) as Box<dyn SemanticEngine>)
//!         })),
//!     )
//! }
//! ```

mod bridge;
mod client;
mod config;
mod position;

pub use bridge::{Budgets, LspBridge};
pub use config::{SemanticConfig, ServerReadiness};

pub(crate) use client::kill_live_servers;
