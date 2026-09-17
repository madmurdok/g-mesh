//! What the bridge needs to know about a server it has never heard of, and
//! where that comes from.
//!
//! # Decision 1: the manifest, read by the plugin, ignored by core
//!
//! The design doc puts the server's command in `plugin.toml`
//! (`docs/architecture/multi-language-plugins.md`, "Interfaces > Plugin SDK >
//! LspBridge": "spawns the configured server (command from the manifest's
//! `[plugin.semantic] command`)"), and that is what this reads. What it does
//! *not* do is add the section to core's own manifest parser
//! (`core::daemon::manifest::read_manifest`), and the reason is what core
//! would do with it: nothing. Core never spawns the server, never talks to
//! it, never counts it (the memory limit samples a *process tree*, so the
//! server is already included without core knowing what it is), and routes
//! nothing by it. A field core parses is a field core validates, and
//! `read_manifest`'s rule is that a malformed manifest is a hard failure - so
//! teaching it this section would mean a plugin whose server command has a
//! typo fails to be discovered *at all*, taking its structural tier down with
//! it, to protect a string only the plugin ever reads.
//!
//! The SDK already re-reads `plugin.toml` for its own walk
//! ([`crate::manifest`], whose module doc argues the case), so reading three
//! more keys out of the same file costs one more struct and no new mechanism.
//! Core's parser ignores unknown sections (it has no `deny_unknown_fields`),
//! which is what makes the two readers of one file possible at all.
//!
//! # Why not the plugin's own code
//!
//! A plugin *could* hand [`LspBridge`](super::LspBridge) a config built in
//! Rust, and for the command name alone that would be no worse. The manifest
//! wins on the thing a command name is attached to: which binary a given
//! installation runs. `plugin.toml` is what an installed layout ships beside
//! the plugin binary and what a user can edit to point at a server that is
//! not on `PATH`, or to add an argument their toolchain needs, without
//! rebuilding the plugin. Both are supported - [`SemanticConfig`]'s fields are
//! public and `from_manifest` is one constructor of several - because a
//! plugin's test suite needs to build one in memory.
//!
//! # The environment: additions, not an allowlist
//!
//! The task that built this asked whether the manifest should carry an env
//! *allowlist*. It should not, and the reason is what a language server is: a
//! program that finds a toolchain. `rust-analyzer` reads `PATH`, `HOME`,
//! `CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`, `CARGO_TARGET_DIR` and
//! more; `gopls` reads `GOPATH`, `GOMODCACHE`, `GOFLAGS`, `GOPRIVATE`; a JVM
//! server reads `JAVA_HOME`. An allowlist is that list, per language, written
//! down by someone who is not the language's maintainer, and its failure mode
//! is silent: the server starts, finds no toolchain, and answers nothing -
//! which is indistinguishable from a project with no cross-file calls.
//!
//! So the server inherits the plugin's environment, which is the daemon's,
//! which is the developer's. [`SemanticConfig::env`] adds to it (or overrides
//! a variable) for the cases a manifest genuinely needs to state - a log
//! level, a server-specific switch. Nothing here removes a variable: a plugin
//! that wants a scrubbed environment has a bigger question to answer than
//! this file can.
//!
//! # Settings: `initializationOptions` is not the only channel, and for some
//! servers it is not a channel at all (GM-299)
//!
//! GM-289 modelled "the server's own settings" as
//! [`SemanticConfig::initialization_options`] alone, because that is how
//! rust-analyzer takes them and the specification presents it as the place a
//! client states what it wants. LSP has a second channel - the server *asks*,
//! with a `workspace/configuration` request naming a section - and pyright
//! uses only that one. Measured against pyright-langserver 1.1.414 on the
//! Python plugin's own conformance fixture, same fixture and same settings
//! value, one variable changed:
//!
//! ```text
//! initializationOptions {"python":{"analysis":{"typeCheckingMode":"off"}}}
//!   -> 4 diagnostics, severity 1     (the default "standard" run, unchanged)
//! workspace/configuration reply {"analysis":{"typeCheckingMode":"off"}}
//!   -> 1 diagnostic,  severity 2     (and the log line "Setting pythonPath…")
//! ```
//!
//! So [`SemanticConfig::settings`] exists beside
//! [`SemanticConfig::initialization_options`] rather than replacing it: the
//! two are different mechanisms, servers differ in which they read, and a
//! bridge that knows only one silently ships a server running on defaults.
//! Neither is language-specific - `workspace/configuration` is a base-protocol
//! method - so both live here, and a manifest states whichever its server
//! actually reads.
//!
//! # Decision 8: readiness is a fact about the server, so it is stated here
//! and not tuned in [`Budgets`](super::Budgets) (GM-310)
//!
//! [`ServerReadiness`] is the second manifest key - after
//! [`implementation_kinds`](SemanticConfig::implementation_kinds) - that
//! exists because a language server has a *shape* the bridge cannot infer.
//! The shape is: does this server gate its answers on a start-up index, or
//! does it resolve each question as it is asked? Traced on the two servers
//! this bridge drives, they are not near each other:
//!
//! ```text
//! pyright 1.1.414          one `$/progress` token, 0.42-0.49s long, begun
//!                          1.26-1.33s AFTER didOpen - and the first correct
//!                          cross-file answer arrives 41-146ms BEFORE it
//!                          begins. Three reps, all three agreeing.
//! rust-analyzer 1.97.1     13 tokens (6 distinct names) over 12.3s, and the
//!                          receiver call `square.area()` answers empty until
//!                          13.4s. Nothing is answerable early by policy.
//! ```
//!
//! The obvious alternative was a number: let a plugin shorten
//! [`Budgets::settle`](super::Budgets::settle), which
//! [`LspBridge::with_budgets`](super::LspBridge::with_budgets) already allows.
//! It is refused because that one number does **two** jobs whose right values
//! for pyright point in opposite directions. Job A is start-up readiness, and
//! pyright wants ~0 there. Job B is GM-309's post-edit scepticism - an empty
//! answer arriving before the server has begun reporting progress for an edit
//! is deferred and re-asked one settle later - and pyright wants that settle
//! to stay *above* its own didOpen-to-progress gap, measured here at
//! 1.26-1.33s. A settle short enough to win A loses B on the same server, and
//! re-opens the exact bug GM-309 closed. So the shape gets its own key and
//! the number keeps its meaning.
//!
//! What makes an `on-demand` claim safe to *make* is that it removes only the
//! blanket wait, never the scepticism: see [`ServerReadiness::OnDemand`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// What a server does before it can answer - the manifest's one claim about
/// its *shape*, as opposed to its command line.
///
/// See this module's Decision 8 for the traces, and
/// [`super::LspBridge`]'s readiness rules for what each value does to a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServerReadiness {
    /// The server builds an index at start-up and answers nothing truthful
    /// until it has. The bridge waits for one continuous
    /// [`Budgets::settle`](super::Budgets::settle) of quiet before asking its
    /// first question, and reports the pass incomplete rather than believing
    /// anything it is told before that.
    ///
    /// The default, and deliberately: a server nobody has traced is assumed to
    /// be this one. Getting it wrong in this direction costs a start-up
    /// latency; getting it wrong in the other could cost an edge.
    #[default]
    Indexed,
    /// The server resolves each question when it is asked, so its first
    /// question is as answerable as its thousandth and its `$/progress` - if
    /// it reports any - is background work rather than a gate.
    ///
    /// **This skips the start-up quiet period and nothing else.** Every other
    /// sceptical rule keeps running, and GM-309 is what makes that enough:
    /// `LspBridge::sync_documents` calls `LspClient::mark_edited` after every
    /// `didOpen`, so when the first question goes out the client
    /// has been quiet for milliseconds rather than for the life of the
    /// process, and `run_pass`'s deferral test - an empty answer while the
    /// client is not quiet for a full settle is re-asked once, later - covers
    /// every answer in that window. A deferred question returns only after the
    /// client has been *continuously* quiet for a whole settle, which a server
    /// that is really indexing cannot provide.
    ///
    /// So the worst case of this claim being wrong about a server is the
    /// latency that server has today, paid per unanswered question instead of
    /// up front, with the same answers at the end.
    ///
    /// The precise guarantee, which is worth stating exactly rather than
    /// generously: **this value cannot record "no target" in any case where
    /// [`Indexed`](ServerReadiness::Indexed) would not record it too.** Both
    /// end up believing a second empty answer given after a full continuous
    /// settle of quiet, so a server that indexes for seconds while reporting
    /// no `$/progress` at all defeats both equally - that exposure is
    /// `Budgets::settle`'s, not this key's, and it is the same one GM-290
    /// accepted when it made a silent server ready by the clock. What this
    /// value adds is nothing: the deferral, the re-ask and the quiet period
    /// are bit for bit the ones an `indexed` server gets from its second pass
    /// onward. What it does cost, when wrong, is a burst of
    /// questions a starting server has to field while it is starting - which
    /// is measurable (see the design doc's GM-310 notes) and is why this is a
    /// claim a plugin makes deliberately rather than a default.
    OnDemand,
}

impl ServerReadiness {
    /// The manifest spellings, which are the words the design doc uses.
    fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "indexed" => Some(Self::Indexed),
            "on-demand" => Some(Self::OnDemand),
            _ => None,
        }
    }
}

/// How to start and address one language server.
///
/// Every field but [`command`](SemanticConfig::command) has a defensible
/// default, so a manifest that names a server on `PATH` and nothing else is a
/// complete configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticConfig {
    /// argv\[0\]: a bare name looked up on `PATH`, or a path. A relative path
    /// is resolved against the manifest's own directory when the config came
    /// from a manifest - the same rule core applies to `[plugin.spawn]
    /// command`, so the two are spelled the same way.
    pub command: PathBuf,
    /// Extra argv entries, in order.
    pub args: Vec<String>,
    /// Environment variables to set for the server, on top of the ones this
    /// process already has - see this module's doc on why this is not an
    /// allowlist.
    pub env: BTreeMap<String, String>,
    /// The `engine` label on every edge the bridge emits - the free-text field
    /// beside `SourceTier::Semantic` that says *which* engine produced an edge
    /// (`rust-analyzer`, `gopls`). Defaults to the command's file stem, which
    /// is the right answer often enough that a manifest rarely states it.
    pub engine: String,
    /// The `nativeKind`s whose nodes are asked `textDocument/implementation` -
    /// a language's word for "trait", "interface", "protocol".
    ///
    /// This is the one place the bridge could have been language-specific and
    /// is not: nothing in the graph marks a node as "the kind of type other
    /// types implement", because that is a fact about a language and not about
    /// a node. Naming the kinds in the manifest keeps the knowledge where the
    /// language is (`implementation_kinds = ["trait"]` for Rust) and the
    /// bridge a string comparison. Empty - the default - means the bridge asks
    /// no implementation questions at all, which is correct for a language
    /// that has no such concept.
    pub implementation_kinds: Vec<String>,
    /// `initialize`'s `initializationOptions`, verbatim.
    ///
    /// Passed through rather than modelled: it is a server-specific blob
    /// (rust-analyzer's `cachePriming`, gopls' `build.directoryFilters`), and
    /// a typed surface here would be a list of every server's options that
    /// this crate would then have to track. A manifest writes it as an
    /// ordinary TOML table and it arrives as the JSON object the server
    /// expects.
    pub initialization_options: Option<serde_json::Value>,
    /// What to answer a server's own `workspace/configuration` request with,
    /// keyed by the `section` it asks for (`python`, `pyright`, `gopls`).
    ///
    /// The second settings channel, and for pyright the only one that works -
    /// see this module's doc for the measurement. A section the server asks
    /// for and this map does not hold is answered `null`, which is "no
    /// configuration for that section" and is what every server handles; a
    /// section this map holds and the server never asks for is simply never
    /// sent. Empty - the default - is exactly the behaviour before this
    /// existed.
    ///
    /// A plugin may add to it at run time, which is the point of it being a
    /// public field: a setting like pyright's `python.pythonPath` names a
    /// path inside the project being indexed, and a manifest shipped beside
    /// the plugin binary cannot know that path.
    pub settings: BTreeMap<String, serde_json::Value>,
    /// Whether this server gates its answers on a start-up index - see
    /// [`ServerReadiness`] and this module's Decision 8.
    ///
    /// [`ServerReadiness::Indexed`] by default, so a manifest that says
    /// nothing gets exactly the behaviour every manifest had before this key
    /// existed.
    pub readiness: ServerReadiness,
}

impl SemanticConfig {
    /// A config for `command`, with every default.
    pub fn new(command: impl Into<PathBuf>) -> Self {
        let command = command.into();
        let engine = command
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "lsp".to_string());
        Self {
            command,
            args: Vec::new(),
            env: BTreeMap::new(),
            engine,
            implementation_kinds: Vec::new(),
            initialization_options: None,
            settings: BTreeMap::new(),
            readiness: ServerReadiness::default(),
        }
    }

    /// Reads `[plugin.semantic]` from this plugin's own manifest, wherever
    /// [`crate::manifest`] finds it.
    ///
    /// `Ok(None)` means there is no manifest, or it has no `[plugin.semantic]`
    /// section - a plugin with no LSP tier, which is not an error. An `Err` is
    /// a section that is there and unusable (unreadable file, malformed TOML,
    /// no `command`): silently degrading to "no semantic tier" there would
    /// hide a typo behind exactly the same symptom as a language that has no
    /// server, which is the failure this crate spends most of its doc comments
    /// refusing to produce.
    pub fn from_manifest() -> Result<Option<Self>> {
        match crate::manifest::manifest_path() {
            Some(path) => Self::from_manifest_at(&path),
            None => Ok(None),
        }
    }

    /// [`SemanticConfig::from_manifest`] with the search already done - the
    /// testable half, for the same reason [`crate::manifest`] splits its own
    /// resolution that way (a process-global environment variable is not
    /// something parallel tests can each set).
    pub fn from_manifest_at(path: &Path) -> Result<Option<Self>> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            // A manifest that is not there is the "no manifest" case, not a
            // broken one: a plugin binary run straight out of `target/`.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
        };
        let raw: RawManifest = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {} as TOML", path.display()))?;
        let Some(semantic) = raw.plugin.and_then(|plugin| plugin.semantic) else { return Ok(None) };

        anyhow::ensure!(
            !semantic.command.trim().is_empty(),
            "{}: [plugin.semantic] command is empty",
            path.display()
        );
        let command = resolve_command(&semantic.command, path);
        let mut config = Self::new(command);
        config.args = semantic.args;
        config.env = semantic.env;
        if let Some(engine) = semantic.engine.filter(|engine| !engine.trim().is_empty()) {
            config.engine = engine;
        }
        config.implementation_kinds = semantic.implementation_kinds;
        config.initialization_options = semantic.initialization_options.map(to_json);
        // Unknown spellings are reported rather than defaulted, and for the
        // reason `implementation_kinds` records: this key changes when the
        // bridge is willing to believe a server, so a typo that silently read
        // as the default would be a manifest saying one thing and a pass doing
        // another - with nothing in the log to say which.
        if let Some(readiness) = &semantic.readiness {
            config.readiness = ServerReadiness::parse(readiness).ok_or_else(|| {
                anyhow::anyhow!(
                    "{}: [plugin.semantic] readiness is {readiness:?}, which is neither \
                     \"indexed\" nor \"on-demand\"",
                    path.display()
                )
            })?;
        }
        // A `settings` that is not a table is the manifest saying something
        // this reader cannot act on, and the rule for a present-and-broken
        // section is the same one `command` follows: report it rather than
        // silently shipping a server on defaults.
        if let Some(settings) = semantic.settings {
            let toml::Value::Table(table) = settings else {
                anyhow::bail!(
                    "{}: [plugin.semantic] settings must be a table of LSP sections",
                    path.display()
                )
            };
            config.settings = table.into_iter().map(|(section, value)| (section, to_json(value))).collect();
        }
        Ok(Some(config))
    }
}

/// A relative `command` is relative to the manifest, an absolute or bare one
/// is itself - the same rule core resolves `[plugin.spawn] command` by, so a
/// manifest author has one rule to know rather than two.
///
/// "Bare" is the case that has to stay untouched: `rust-analyzer` with no
/// separator is a `PATH` lookup, and joining it to a directory would turn it
/// into a path that does not exist.
fn resolve_command(command: &str, manifest: &Path) -> PathBuf {
    let path = PathBuf::from(command);
    let bare = path.components().count() == 1 && !path.is_absolute();
    if bare || path.is_absolute() {
        return path;
    }
    match manifest.parent() {
        Some(dir) => dir.join(path),
        None => path,
    }
}

/// TOML to JSON, for `initializationOptions`.
///
/// Written out rather than transcoded through serde so that every shape has a
/// stated answer, including the one TOML has and JSON does not: a datetime
/// becomes its RFC 3339 string, which is what a JSON-speaking server would
/// have been sent anyway.
fn to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(string) => serde_json::Value::String(string),
        toml::Value::Integer(integer) => serde_json::Value::from(integer),
        toml::Value::Float(float) => serde_json::Number::from_f64(float)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(boolean) => serde_json::Value::Bool(boolean),
        toml::Value::Datetime(datetime) => serde_json::Value::String(datetime.to_string()),
        toml::Value::Array(array) => serde_json::Value::Array(array.into_iter().map(to_json).collect()),
        toml::Value::Table(table) => {
            serde_json::Value::Object(table.into_iter().map(|(key, value)| (key, to_json(value))).collect())
        }
    }
}

/// Only the section this module reads. Deliberately permissive about
/// everything else in the file, including `[plugin]` itself being absent -
/// this reader's job is to find one optional table, not to validate a
/// manifest core has already validated.
#[derive(Debug, Deserialize)]
struct RawManifest {
    plugin: Option<RawPlugin>,
}

#[derive(Debug, Deserialize)]
struct RawPlugin {
    semantic: Option<RawSemantic>,
}

#[derive(Debug, Deserialize)]
struct RawSemantic {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    implementation_kinds: Vec<String>,
    #[serde(default)]
    initialization_options: Option<toml::Value>,
    #[serde(default)]
    settings: Option<toml::Value>,
    #[serde(default)]
    readiness: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `contents` to a scratch `plugin.toml` and reads it back. `None`
    /// reads a path that does not exist.
    fn read(tag: &str, contents: Option<&str>) -> Result<Option<SemanticConfig>> {
        let dir = std::env::temp_dir().join(format!("g-mesh-sdk-semantic-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plugin.toml");
        if let Some(contents) = contents {
            std::fs::write(&path, contents).unwrap();
        }
        let read = SemanticConfig::from_manifest_at(&path);
        let _ = std::fs::remove_dir_all(&dir);
        read
    }

    #[test]
    fn a_manifest_with_no_semantic_section_has_no_config_and_is_not_an_error() {
        let manifest = "[plugin]\nlanguage = \"toy\"\nplugin_version = \"1.0.0\"\n\n\
                        [plugin.languages]\nextensions = [\".toy\"]\n";
        assert_eq!(read("none", Some(manifest)).unwrap(), None);
        assert_eq!(read("missing", None).unwrap(), None);
    }

    #[test]
    fn every_field_defaults_to_something_defensible() {
        let config = read("minimal", Some("[plugin.semantic]\ncommand = \"toy-server\"\n"))
            .unwrap()
            .expect("the section is there");
        assert_eq!(config.command, PathBuf::from("toy-server"), "a bare command stays a PATH lookup");
        assert_eq!(config.engine, "toy-server", "the engine label defaults to the command's stem");
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
        assert!(config.implementation_kinds.is_empty());
        assert_eq!(config.initialization_options, None);
        assert!(config.settings.is_empty(), "no settings is the pre-GM-299 behaviour: answer null");
        assert_eq!(
            config.readiness,
            ServerReadiness::Indexed,
            "a manifest that says nothing about readiness gets the pre-GM-310 wait"
        );
    }

    /// The GM-310 key, all three ways it can go. A value this reader does not
    /// know is an error rather than a silent default, because the default is
    /// the *slow* answer: a typo'd `on_demand` would read as `indexed`, cost
    /// the two seconds the key exists to remove, and leave nothing anywhere
    /// saying why.
    #[test]
    fn the_readiness_key_is_read_and_an_unknown_value_is_reported() {
        let of = |tag: &str, value: &str| {
            read(tag, Some(&format!("[plugin.semantic]\ncommand = \"toy-server\"\nreadiness = {value}\n")))
        };
        assert_eq!(of("on-demand", "\"on-demand\"").unwrap().unwrap().readiness, ServerReadiness::OnDemand);
        assert_eq!(of("indexed", "\"indexed\"").unwrap().unwrap().readiness, ServerReadiness::Indexed);
        // Stated explicitly and stated by omission are the same configuration -
        // `plugins/rust/plugin.toml` says it out loud so the trace behind it
        // has somewhere to live.
        assert_eq!(
            of("indexed", "\"indexed\"").unwrap().unwrap(),
            read("absent", Some("[plugin.semantic]\ncommand = \"toy-server\"\n")).unwrap().unwrap()
        );

        for bad in ["\"on_demand\"", "\"ondemand\"", "\"lazy\"", "true", "\"\""] {
            assert!(of("bad", bad).is_err(), "{bad} is not a readiness");
        }
    }

    #[test]
    fn every_field_is_read_and_initialization_options_become_json() {
        let config = read(
            "full",
            Some(
                "[plugin.semantic]\n\
                 command = \"servers/toy-server\"\n\
                 args = [\"--stdio\"]\n\
                 engine = \"toy-analyzer\"\n\
                 readiness = \"on-demand\"\n\
                 implementation_kinds = [\"protocol\"]\n\n\
                 [plugin.semantic.env]\nTOY_LOG = \"error\"\n\n\
                 [plugin.semantic.initialization_options]\n\
                 priming = true\n\
                 depth = 3\n\
                 roots = [\"a\", \"b\"]\n\n\
                 [plugin.semantic.settings.toy]\n\
                 mode = \"basic\"\n\n\
                 [plugin.semantic.settings.toy.analysis]\n\
                 depth = 1\n",
            ),
        )
        .unwrap()
        .expect("the section is there");

        assert!(config.command.is_absolute(), "a relative command resolves against the manifest");
        assert!(config.command.ends_with("servers/toy-server"));
        assert_eq!(config.args, vec!["--stdio"]);
        assert_eq!(config.engine, "toy-analyzer");
        assert_eq!(config.readiness, ServerReadiness::OnDemand);
        assert_eq!(config.implementation_kinds, vec!["protocol"]);
        assert_eq!(config.env.get("TOY_LOG").map(String::as_str), Some("error"));
        assert_eq!(
            config.initialization_options,
            Some(serde_json::json!({ "priming": true, "depth": 3, "roots": ["a", "b"] }))
        );
        // Keyed by the section a server asks `workspace/configuration` for,
        // with whatever nesting that section's own schema has below it.
        assert_eq!(config.settings.keys().collect::<Vec<_>>(), vec!["toy"]);
        assert_eq!(
            config.settings["toy"],
            serde_json::json!({ "mode": "basic", "analysis": { "depth": 1 } })
        );
    }

    /// `settings` that is not a table of sections is a manifest this reader
    /// cannot act on - reported, never shipped as "the server runs on
    /// defaults".
    #[test]
    fn settings_that_are_not_a_table_of_sections_are_reported() {
        let manifest = "[plugin.semantic]\ncommand = \"toy-server\"\nsettings = \"python\"\n";
        assert!(read("scalar-settings", Some(manifest)).is_err());
    }

    /// A section that is present and broken is an error, not a silent "no
    /// semantic tier" - see [`SemanticConfig::from_manifest`].
    #[test]
    fn a_broken_section_is_reported_rather_than_read_as_absent() {
        assert!(read("no-command", Some("[plugin.semantic]\nargs = []\n")).is_err());
        assert!(read("empty-command", Some("[plugin.semantic]\ncommand = \"  \"\n")).is_err());
        assert!(read("not-toml", Some("[plugin.semantic\ncommand =")).is_err());
    }
}
