//! The `g-mesh` command-line surface: every subcommand the binary exposes,
//! declared once here with clap's derive API, plus the dispatch that hands
//! each one to the module that answers it.
//!
//! Two of these subcommands are not really "commands" in the user-facing
//! sense and their argv contract is load-bearing:
//!
//! - `g-mesh mcp-shim` is what an MCP client is registered against (e.g.
//!   `claude mcp add g-mesh -- g-mesh mcp-shim`). It takes no arguments and
//!   must keep taking none: a registration that stops parsing is invisible
//!   until someone's editor quietly loses its tools.
//! - `g-mesh daemon --project-root <path>` is spawned by the shim itself
//!   (`shim::spawn_detached_daemon`), never typed by a human, so it is hidden
//!   from `--help` while staying exactly as parseable as before.
//!
//! Everything else is a human-facing command, and every one of them is
//! implemented as of release 0.11.0 - the surface was declared up front, the
//! way the MCP tool schemas were, so `--help` described the finished CLI
//! before it grew a command at a time; `dispatch` below no longer has a
//! `not_implemented` fallback to fall into.
//!
//! `model` was added after that surface was declared, because a binary
//! installed without this repository had no way to obtain the embedding
//! weights and therefore no way to use `search_code` at all (see
//! [`crate::cli::model`]). It is also the only command here that opens a
//! network connection, which that module keeps true structurally.

// CLI output goes to the user's terminal, not the shared daemon log, so
// `eprintln!` is fine here and in every submodule (GM-520, clippy.toml).
#![allow(clippy::disallowed_macros)]

pub mod agent_instructions;
pub mod clean;
pub mod config_wizard;
pub mod debug_candidates;
pub mod embed_eval;
pub mod init;
pub mod model;
pub mod plugin_check;
pub mod plugin_install;
pub mod plugins;
pub mod reindex;
pub mod status;
pub mod stop;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::languages::LanguageOutcome;
use crate::{daemon, shim};

/// The exit code of a command that did its work and printed its result, but
/// only in part: `g-mesh init`/`reindex` when some, not all, discovered
/// languages failed to index (ADR 0021). The index is written and usable.
pub const PARTIAL_FAILURE_EXIT_CODE: i32 = 2;

/// A command's error that carries its own exit code, for an outcome that is
/// neither success nor the plain failure every other error exits 1 with.
#[derive(Debug)]
pub struct ExitStatus {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitStatus {}

/// The process exit code for a command's error: an [`ExitStatus`]'s own
/// code, 1 for every other error.
pub fn exit_code(err: &anyhow::Error) -> i32 {
    err.downcast_ref::<ExitStatus>().map_or(1, |status| status.code)
}

/// One stderr line per language a bulk walk did not index: a `Failed`
/// language with its error, a `PluginAbsent` one with its file count and the
/// command that installs its plugin.
pub fn language_outcome_lines(outcomes: &BTreeMap<String, LanguageOutcome>) -> Vec<String> {
    outcomes
        .iter()
        .filter_map(|(language, outcome)| match outcome {
            LanguageOutcome::Indexed { .. } => None,
            LanguageOutcome::Failed { error } => Some(format!(
                "g-mesh: {language} failed to index and is not in the index: {}",
                crate::languages::error_on_one_line(error)
            )),
            LanguageOutcome::PluginAbsent { files } => {
                let what = match files {
                    Some(files) => format!("{files} file(s) not indexed"),
                    None => "its files are not indexed".to_string(),
                };
                // Every PluginAbsent language is a catalogue one; a name the
                // catalogue lacks still gets its line, just without a command.
                Some(match crate::languages::entry(language) {
                    Some(entry) => format!(
                        "g-mesh: {language} has no plugin installed - {what}; install it with `{}`",
                        entry.install_command()
                    ),
                    None => format!("g-mesh: {language} has no plugin installed - {what}"),
                })
            }
        })
        .collect()
}

/// Prints [`language_outcome_lines`] to stderr, then fails with
/// [`PARTIAL_FAILURE_EXIT_CODE`] when any language `Failed`. A walk where
/// every discovered language failed never gets here: it is an error of its own.
pub(crate) fn report_language_outcomes(outcomes: &BTreeMap<String, LanguageOutcome>) -> Result<()> {
    for line in language_outcome_lines(outcomes) {
        eprintln!("{line}");
    }
    if outcomes.values().any(|outcome| matches!(outcome, LanguageOutcome::Failed { .. })) {
        return Err(ExitStatus {
            code: PARTIAL_FAILURE_EXIT_CODE,
            message: "some languages failed to index - the index holds every other language".to_string(),
        }
        .into());
    }
    Ok(())
}

#[derive(Debug, Parser)]
#[command(name = "g-mesh", version, about = "Local source code indexer for AI agents")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Set this project's g-mesh state up with default settings (optional -
    /// the zero-config path works without it).
    Init {
        /// Auto-install project-instruction files for the named coding
        /// agent(s) - e.g. `--agent claude,gemini`. Repeatable and
        /// comma-delimited; omitted means none are written, matching
        /// `init`'s existing no-flags behavior exactly.
        #[arg(long, value_delimiter = ',')]
        agent: Vec<AgentTarget>,
    },
    /// Edit g-mesh settings interactively.
    Config {
        /// Edit the global `~/.g-mesh/config.toml` instead of this project's
        /// settings.
        #[arg(long)]
        global: bool,
    },
    /// Report the current project's daemon, plugin, index and per-language
    /// state. Cheap by default: it does not walk the project's files.
    Status {
        /// Also walk the project for index coverage and files awaiting
        /// reindex (one `stat` per source file).
        #[arg(long)]
        full: bool,
        /// Print the whole report as one JSON object instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Wipe and rebuild the current project's index from scratch.
    Reindex,
    /// Inspect the installed language plugins, or check one against the
    /// plugin contract.
    ///
    /// `plugin` is a hidden alias: the architecture doc and GM-276 spell the
    /// conformance kit `g-mesh plugin check` - see `cli::plugin_check`'s doc
    /// for why it lives in this group instead.
    #[command(alias = "plugin")]
    Plugins {
        #[command(subcommand)]
        command: PluginsCommand,
    },
    /// Fetch or inspect the embedding model that `search_code` needs.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Delete cached project indexes under `~/.g-mesh/projects/`.
    Clean(CleanArgs),
    /// Stop the current project's daemon core and its plugin process.
    Stop,
    /// Stdio<->AF_UNIX proxy spawned by MCP clients; bootstraps the
    /// per-project daemon on demand.
    McpShim,
    /// Internal daemon entry point, bootstrapped by mcp-shim - not invoked
    /// directly by users.
    #[command(hide = true)]
    Daemon {
        /// Project root this daemon serves; passed explicitly by the shim
        /// rather than inferred from cwd, which a detached process must not
        /// depend on.
        #[arg(long)]
        project_root: PathBuf,
    },
    /// Prints how multi-project detection sees a folder (D10 in
    /// `docs/architecture/lazy-indexing.md`): the mode decision, the
    /// candidates, and what the walk cost. For support and for measuring
    /// the walk (M4) - not a user-facing command.
    #[command(hide = true)]
    DebugCandidates {
        /// Folder to inspect; the current directory when omitted.
        dir: Option<PathBuf>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Scores embedding models against the search-quality eval's query set
    /// (`docs/architecture/embedding-eval.md`): snapshot, run, report,
    /// parity. An instrument for choosing the model - not a user-facing
    /// command.
    #[command(hide = true)]
    DebugEmbedEval {
        #[command(subcommand)]
        command: embed_eval::EmbedEvalCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// List the language plugins installed under `~/.g-mesh/plugins/`.
    List,
    /// Run a plugin against a fixture project through the real index and
    /// linker, and report every conformance check it fails.
    Check(plugin_check::PluginCheckArgs),
    /// Install a language plugin beside this g-mesh binary: from this
    /// version's GitHub release, or with `--from` from a local archive or
    /// directory without touching the network.
    Install(plugin_install::InstallArgs),
    /// Delete an installed language plugin's directory.
    Remove {
        /// The plugin's language, as `plugins list` names it.
        language: String,
    },
}

/// The embedding model's own commands, grouped under `model` rather than
/// spelled `fetch-model`/`model-status` at the top level, for the same reason
/// `plugins` is: they act on one thing, and the group is where a later
/// `model rm` or `model verify` belongs.
///
/// `fetch` and `plugins install <language>` are the only commands in the whole
/// CLI that open a network connection, and they do so *because the user typed
/// them* - see [`crate::cli::model`] for how that stays true.
#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Download the embedding model's weights (~154 MiB) and the search
    /// rerank model's (~87 MiB) into the directories their loaders read.
    Fetch {
        /// Where to put the embedding weights. Defaults to
        /// `$G_MESH_MODEL_DIR`, else `~/.g-mesh/models/<model>` - the same
        /// resolution the loader uses, so the two cannot disagree. The rerank
        /// model goes to `$G_MESH_RERANK_MODEL_DIR`, else
        /// `~/.g-mesh/models/ms-marco-MiniLM-L6-v2`.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Skip the rerank model; `search_code` then keeps the embedding
        /// order.
        #[arg(long)]
        no_rerank: bool,
    },
    /// Report whether the weights are present, and where they are expected.
    Status {
        /// Directory to inspect, resolved exactly as `fetch --dir` is.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

/// A coding agent/tool `init --agent` can auto-install project-instruction
/// files for.
///
/// `AgentsMd` alone writes only `AGENTS.md` - useful for the many tools
/// (Cursor, Windsurf, GitHub Copilot, OpenAI Codex CLI, Kimi Code CLI, Aider,
/// ...) that read it natively and need nothing else. `Claude` and `Gemini`
/// each additionally bridge `CLAUDE.md`/`GEMINI.md` to it, since neither tool
/// reads `AGENTS.md` on its own - see `cli::agent_instructions` for why one
/// shared file plus small bridges is the whole design.
///
/// clap's `ValueEnum` derive kebab-cases variant names by default, so
/// `AgentsMd` parses as `--agent agents-md` - confirmed by
/// `agent_flag_kebab_cases_agents_md` below rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentTarget {
    AgentsMd,
    Claude,
    Gemini,
}

/// `clean`'s target is one positional argument because the documented forms -
/// a project id, `expired`, `orphaned`, `all` - are alternatives for the same
/// slot, not independent flags. Telling them apart is the `clean` command's
/// own job (`cli::clean`); parsing only has to carry the word through.
#[derive(Debug, Args)]
pub struct CleanArgs {
    /// A project id (the `<hash>` directory name under
    /// `~/.g-mesh/projects/`), or one of `expired` (idle longer than
    /// `cleanup.idleThresholdDays`), `orphaned` (the project directory has
    /// been deleted) and `all`. Omitted means the current directory's
    /// project.
    pub target: Option<String>,
    /// Confirms `clean all` and `clean orphaned`, which without it only
    /// report what they would have deleted.
    #[arg(long)]
    pub force: bool,
}

/// Parses argv and runs the command it names.
///
/// Parse failures never reach here: clap prints its own diagnostic and exits
/// with its standard status. What this returns is the *command's* outcome,
/// which `main` turns into a `g-mesh: ...` message and [`exit_code`]'s code.
pub fn run() -> Result<()> {
    dispatch(Cli::parse().command)
}

/// Split out from [`run`] so the dispatch table can be exercised without
/// taking over the process's real argv.
fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Init { agent } => init::run(&agent),
        Command::Config { global } => config_wizard::run(global),
        Command::Status { full, json } => status::run(full, json),
        Command::Reindex => reindex::run(),
        Command::Plugins { command } => match command {
            PluginsCommand::List => plugins::run(),
            PluginsCommand::Check(args) => plugin_check::run(&args),
            PluginsCommand::Install(args) => plugin_install::install(&args),
            PluginsCommand::Remove { language } => plugin_install::remove(&language),
        },
        Command::Model { command } => model::run(&command),
        Command::Clean(args) => clean::run(&args),
        Command::Stop => stop::run(),
        Command::McpShim => shim::run(),
        Command::Daemon { project_root } => daemon::run(&project_root),
        Command::DebugCandidates { dir, json } => debug_candidates::run(dir, json),
        Command::DebugEmbedEval { command } => embed_eval::run(command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("g-mesh").chain(args.iter().copied()))
    }

    fn command_of(args: &[&str]) -> Command {
        parse(args).unwrap_or_else(|e| panic!("`g-mesh {}` must parse: {e}", args.join(" "))).command
    }

    /// clap's own consistency check over the whole declared surface (duplicate
    /// names, conflicting short flags, and so on).
    #[test]
    fn the_declared_surface_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// The registration contract MCP clients are wired against: bare
    /// `mcp-shim`, no arguments, and nothing extra tolerated after it.
    #[test]
    fn mcp_shim_parses_exactly_as_it_always_has() {
        assert!(matches!(command_of(&["mcp-shim"]), Command::McpShim));
        assert!(parse(&["mcp-shim", "extra"]).is_err(), "mcp-shim takes no arguments");
    }

    /// The other half of that contract: what `shim::spawn_detached_daemon`
    /// puts on the command line.
    #[test]
    fn daemon_still_requires_and_parses_its_project_root() {
        match command_of(&["daemon", "--project-root", "/tmp/some-project"]) {
            Command::Daemon { project_root } => {
                assert_eq!(project_root, PathBuf::from("/tmp/some-project"));
            }
            other => panic!("expected the daemon subcommand, got {other:?}"),
        }

        let missing = parse(&["daemon"]).expect_err("--project-root is required");
        assert_eq!(missing.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn config_takes_an_optional_global_flag() {
        assert!(matches!(command_of(&["config"]), Command::Config { global: false }));
        assert!(matches!(command_of(&["config", "--global"]), Command::Config { global: true }));
    }

    #[test]
    fn plugins_requires_its_own_subcommand() {
        assert!(matches!(
            command_of(&["plugins", "list"]),
            Command::Plugins { command: PluginsCommand::List }
        ));

        let bare = parse(&["plugins"]).expect_err("`plugins` alone names no action");
        assert_eq!(bare.kind(), ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand);
        assert!(parse(&["plugins", "uninstall"]).is_err(), "only `list` and `check` exist today");
    }

    /// `plugins check` takes the plugin directory positionally and requires
    /// `--fixture`; the design doc's singular `plugin check` spelling parses
    /// to the very same command. `--expect` is optional (GM-277) - absent by
    /// default, since the design doc's own reasoning (report.rs's module
    /// doc, echoed in `expectations`'s) is that a flag which parses and does
    /// nothing would read as "expectations passed".
    #[test]
    fn plugins_check_takes_a_plugin_dir_and_a_required_fixture() {
        for group in ["plugins", "plugin"] {
            match command_of(&[group, "check", "plugins/typescript", "--fixture", "/tmp/fixture"]) {
                Command::Plugins { command: PluginsCommand::Check(args) } => {
                    assert_eq!(args.plugin_dir, PathBuf::from("plugins/typescript"));
                    assert_eq!(args.fixture, PathBuf::from("/tmp/fixture"));
                    assert_eq!(args.expect, None);
                }
                other => panic!("expected `{group} check`, got {other:?}"),
            }
        }

        let missing = parse(&["plugins", "check", "plugins/typescript"]).expect_err("--fixture is required");
        assert_eq!(missing.kind(), ErrorKind::MissingRequiredArgument);

        match command_of(&["plugins", "check", "plugins/typescript", "--fixture", "/f", "--expect", "e.toml"])
        {
            Command::Plugins { command: PluginsCommand::Check(args) } => {
                assert_eq!(args.expect, Some(PathBuf::from("e.toml")));
            }
            other => panic!("expected `plugins check`, got {other:?}"),
        }
    }

    /// `model` groups its own subcommands the way `plugins` does, and both of
    /// them take an optional `--dir`, which is what lets a fetch land
    /// somewhere other than the per-user default.
    #[test]
    fn model_requires_a_subcommand_and_both_take_an_optional_dir() {
        assert!(matches!(
            command_of(&["model", "fetch"]),
            Command::Model { command: ModelCommand::Fetch { dir: None, no_rerank: false } }
        ));
        assert!(matches!(
            command_of(&["model", "fetch", "--no-rerank"]),
            Command::Model { command: ModelCommand::Fetch { dir: None, no_rerank: true } }
        ));
        assert!(matches!(
            command_of(&["model", "status"]),
            Command::Model { command: ModelCommand::Status { dir: None } }
        ));

        match command_of(&["model", "fetch", "--dir", "/tmp/weights"]) {
            Command::Model { command: ModelCommand::Fetch { dir, .. } } => {
                assert_eq!(dir, Some(PathBuf::from("/tmp/weights")));
            }
            other => panic!("expected `model fetch`, got {other:?}"),
        }
        match command_of(&["model", "status", "--dir", "/tmp/weights"]) {
            Command::Model { command: ModelCommand::Status { dir } } => {
                assert_eq!(dir, Some(PathBuf::from("/tmp/weights")));
            }
            other => panic!("expected `model status`, got {other:?}"),
        }

        let bare = parse(&["model"]).expect_err("`model` alone names no action");
        assert_eq!(bare.kind(), ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand);
        assert!(parse(&["model", "rm"]).is_err(), "only `fetch` and `status` exist today");
        assert!(parse(&["model", "fetch", "/tmp/weights"]).is_err(), "--dir is a flag, not positional");
    }

    /// All four documented `clean` forms land in the same positional slot.
    #[test]
    fn clean_accepts_its_five_documented_forms() {
        let cases = [
            (vec![], None, false),
            (vec!["a1b2c3d4e5f6a7b8"], Some("a1b2c3d4e5f6a7b8"), false),
            (vec!["expired"], Some("expired"), false),
            (vec!["orphaned"], Some("orphaned"), false),
            (vec!["orphaned", "--force"], Some("orphaned"), true),
            (vec!["all"], Some("all"), false),
            (vec!["all", "--force"], Some("all"), true),
        ];

        for (args, expected_target, expected_force) in cases {
            let mut argv = vec!["clean"];
            argv.extend_from_slice(&args);
            match command_of(&argv) {
                Command::Clean(clean) => {
                    assert_eq!(clean.target.as_deref(), expected_target, "target of {argv:?}");
                    assert_eq!(clean.force, expected_force, "--force of {argv:?}");
                }
                other => panic!("expected the clean subcommand, got {other:?}"),
            }
        }

        assert!(parse(&["clean", "all", "extra"]).is_err(), "clean takes at most one target");
    }

    #[test]
    fn the_argument_free_commands_take_no_arguments() {
        for name in ["init", "status", "reindex", "stop"] {
            assert!(parse(&[name]).is_ok(), "`g-mesh {name}` must parse");
            assert!(parse(&[name, "extra"]).is_err(), "`g-mesh {name}` takes no arguments");
        }
    }

    /// `init` without `--agent` at all - the existing, unchanged default -
    /// parses to an empty target list.
    #[test]
    fn init_without_agent_parses_to_an_empty_target_list() {
        assert!(matches!(command_of(&["init"]), Command::Init { agent } if agent.is_empty()));
    }

    /// clap's `ValueEnum` derive kebab-cases variant names by default, so
    /// `AgentsMd` must parse as `agents-md` - the exact thing
    /// `AgentTarget`'s doc comment claims, checked here rather than assumed.
    #[test]
    fn agent_flag_kebab_cases_agents_md() {
        match command_of(&["init", "--agent", "agents-md"]) {
            Command::Init { agent } => assert_eq!(agent, vec![AgentTarget::AgentsMd]),
            other => panic!("expected the init subcommand, got {other:?}"),
        }
    }

    /// `--agent` is repeatable and comma-delimited, and both forms land in
    /// the same list in the order given.
    #[test]
    fn agent_flag_accepts_comma_delimited_and_repeated_values() {
        match command_of(&["init", "--agent", "claude,gemini"]) {
            Command::Init { agent } => {
                assert_eq!(agent, vec![AgentTarget::Claude, AgentTarget::Gemini])
            }
            other => panic!("expected the init subcommand, got {other:?}"),
        }

        match command_of(&["init", "--agent", "claude", "--agent", "gemini"]) {
            Command::Init { agent } => {
                assert_eq!(agent, vec![AgentTarget::Claude, AgentTarget::Gemini])
            }
            other => panic!("expected the init subcommand, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_agent_value_is_a_clap_error() {
        let err = parse(&["init", "--agent", "chatgpt"]).expect_err("chatgpt is not a supported target");
        assert_eq!(err.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn an_unknown_subcommand_is_a_clap_error() {
        let err = parse(&["frobnicate"]).expect_err("no such subcommand");
        assert_eq!(err.kind(), ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn an_unknown_flag_is_a_clap_error() {
        let err = parse(&["status", "--verbose"]).expect_err("no such flag");
        assert_eq!(err.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn a_missing_subcommand_is_a_clap_error() {
        let err = parse(&[]).expect_err("a subcommand is required");
        assert_eq!(err.kind(), ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand);
    }

    /// `--help` is the CLI's documentation: every human-facing command has to
    /// be in it, and the shim's private daemon entry point has to stay out.
    #[test]
    fn help_lists_every_human_facing_command_and_hides_the_daemon() {
        let help = Cli::command().render_help().to_string();
        for name in ["init", "config", "status", "reindex", "plugins", "model", "clean", "stop", "mcp-shim"] {
            assert!(help.contains(name), "`--help` must mention `{name}`:\n{help}");
        }

        // Asked of the declared surface rather than of the rendered text,
        // which mentions the word "daemon" in other commands' descriptions.
        let daemon = Cli::command()
            .get_subcommands()
            .find(|sub| sub.get_name() == "daemon")
            .expect("the daemon subcommand must still exist")
            .clone();
        assert!(daemon.is_hide_set(), "the shim's private daemon entry point must stay out of --help");
    }

    #[test]
    fn debug_candidates_parses_and_stays_hidden() {
        let cli = Cli::try_parse_from(["g-mesh", "debug-candidates", "/some/dir", "--json"]).unwrap();
        match cli.command {
            Command::DebugCandidates { dir, json } => {
                assert_eq!(dir, Some(PathBuf::from("/some/dir")));
                assert!(json);
            }
            other => panic!("expected debug-candidates, got {other:?}"),
        }
        let sub = Cli::command()
            .get_subcommands()
            .find(|sub| sub.get_name() == "debug-candidates")
            .expect("debug-candidates must exist")
            .clone();
        assert!(sub.is_hide_set(), "debug-candidates is for support and M4, not --help");
    }

    // -----------------------------------------------------------------
    // Per-language outcome lines and exit codes (ADR 0021, resolved at
    // review 3)
    // -----------------------------------------------------------------

    fn outcomes(pairs: &[(&str, LanguageOutcome)]) -> BTreeMap<String, LanguageOutcome> {
        pairs.iter().map(|(language, outcome)| (language.to_string(), outcome.clone())).collect()
    }

    /// One line per `Failed` (with its error) and per `PluginAbsent` (with
    /// its file count and the install command); none for `Indexed`.
    ///
    /// Controls: drop the error from the `Failed` line, the count or the
    /// install command from the `PluginAbsent` line, or emit a line for
    /// `Indexed` - the exact comparison fails.
    #[test]
    fn each_language_not_indexed_gets_one_line_naming_what_to_do() {
        let lines = language_outcome_lines(&outcomes(&[
            ("go", LanguageOutcome::PluginAbsent { files: None }),
            ("python", LanguageOutcome::PluginAbsent { files: Some(12) }),
            ("rust", LanguageOutcome::Indexed { files: 40 }),
            (
                "typescript",
                LanguageOutcome::Failed { error: "the plugin's entry point x does not exist".to_string() },
            ),
        ]));

        assert_eq!(
            lines,
            [
                "g-mesh: go has no plugin installed - its files are not indexed; install it with `g-mesh plugins install go`",
                "g-mesh: python has no plugin installed - 12 file(s) not indexed; install it with `g-mesh plugins install python`",
                "g-mesh: typescript failed to index and is not in the index: the plugin's entry point x does not exist",
            ]
        );
    }

    /// A `Failed` language's stored chain (`languages::failed_error`, one
    /// cause per line) is printed on ONE stderr line, every cause joined by
    /// ": ", outermost first (GM-330, ADR 0021 section 2).
    ///
    /// Control: print the stored error raw in `language_outcome_lines` (no
    /// `error_on_one_line`) - the line contains "\n" and the exact
    /// comparison fails.
    #[test]
    fn a_failed_languages_whole_chain_is_on_one_stderr_line() {
        use anyhow::Context;

        let inner: Result<(), std::io::Error> = Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid type: map, expected a string\nat line 3",
        ));
        let err = inner.context("reading the manifest").context("loading the rust plugin").unwrap_err();
        let stored = crate::languages::failed_error(&err);
        assert_eq!(stored.lines().count(), 3, "the fixture must be a multi-line chain: {stored}");

        let lines = language_outcome_lines(&outcomes(&[("rust", LanguageOutcome::Failed { error: stored })]));

        assert_eq!(
            lines,
            ["g-mesh: rust failed to index and is not in the index: loading the rust plugin: \
              reading the manifest: invalid type: map, expected a string at line 3"]
        );
        assert!(!lines[0].contains('\n'));
    }

    /// Exit 0 when every discovered language indexed, absent plugins
    /// included; 2 when some `Failed`. (1, all failed, is the walk's own
    /// error and never reaches `report_language_outcomes`.)
    ///
    /// Controls: make `report_language_outcomes` fail on `PluginAbsent` too -
    /// the first `expect` fails; return `Ok` for a `Failed` - `expect_err`
    /// fails; give `ExitStatus` code 1 - the `assert_eq` on 2 fails.
    #[test]
    fn exit_code_is_zero_with_absent_plugins_and_two_with_a_failed_language() {
        report_language_outcomes(&outcomes(&[
            ("rust", LanguageOutcome::Indexed { files: 1 }),
            ("python", LanguageOutcome::PluginAbsent { files: Some(3) }),
        ]))
        .expect("absent plugins are a valid install: exit 0");
        report_language_outcomes(&BTreeMap::new()).expect("no outcomes: exit 0");

        let err = report_language_outcomes(&outcomes(&[
            ("rust", LanguageOutcome::Indexed { files: 1 }),
            ("python", LanguageOutcome::Failed { error: "boom".to_string() }),
        ]))
        .expect_err("a failed language is a partial failure");
        assert_eq!(exit_code(&err), PARTIAL_FAILURE_EXIT_CODE);
        assert_eq!(PARTIAL_FAILURE_EXIT_CODE, 2);
    }

    /// Every error that is not an `ExitStatus` keeps exit code 1, including
    /// one with context wrapped around it; an `ExitStatus` keeps its own code
    /// through context too.
    ///
    /// Control: make `exit_code` always return 1 (main's old
    /// `process::exit(1)`) - the code-2 assertion fails.
    #[test]
    fn exit_code_is_one_for_a_plain_error_and_the_status_code_otherwise() {
        assert_eq!(exit_code(&anyhow::anyhow!("every discovered language failed to index")), 1);
        let status: anyhow::Error = ExitStatus { code: 2, message: "partial".to_string() }.into();
        assert_eq!(exit_code(&status.context("while running init")), 2);
    }
}
