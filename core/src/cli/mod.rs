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
//! Everything else is a human-facing command. The ones that are not built yet
//! parse their documented arguments and then fail with a plain "not
//! implemented yet" - the surface is declared up front, the way the MCP tool
//! schemas were, so `--help` describes the finished CLI rather than growing a
//! command at a time.

pub mod clean;
pub mod config_wizard;
pub mod init;
pub mod reindex;
pub mod status;
pub mod stop;

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};

use crate::{daemon, shim};

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
    Init,
    /// Edit g-mesh settings interactively.
    Config {
        /// Edit the global `~/.g-mesh/config.toml` instead of this project's
        /// settings.
        #[arg(long)]
        global: bool,
    },
    /// Report the current project's daemon, plugin and index state.
    Status,
    /// Wipe and rebuild the current project's index from scratch.
    Reindex,
    /// Inspect the installed language plugins.
    Plugins {
        #[command(subcommand)]
        command: PluginsCommand,
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
}

#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// List the language plugins installed under `~/.g-mesh/plugins/`.
    List,
}

/// `clean`'s target is one positional argument because the three documented
/// forms - a project id, `expired`, `all` - are alternatives for the same
/// slot, not independent flags. Telling them apart is the `clean` command's
/// own job (`cli::clean`); parsing only has to carry the word through.
#[derive(Debug, Args)]
pub struct CleanArgs {
    /// A project id (the `<hash>` directory name under
    /// `~/.g-mesh/projects/`), the literal `expired`, or the literal `all`.
    /// Omitted means the current directory's project.
    pub target: Option<String>,
    /// Confirms `clean all`, which without it only reports how many projects
    /// it would have deleted.
    #[arg(long)]
    pub force: bool,
}

/// Parses argv and runs the command it names.
///
/// Parse failures never reach here: clap prints its own diagnostic and exits
/// with its standard status. What this returns is the *command's* outcome,
/// which `main` turns into a `g-mesh: ...` message and exit code 1.
pub fn run() -> Result<()> {
    dispatch(Cli::parse().command)
}

/// Split out from [`run`] so the dispatch table can be exercised without
/// taking over the process's real argv.
fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Init => init::run(),
        Command::Config { global } => config_wizard::run(global),
        Command::Status => status::run(),
        Command::Reindex => reindex::run(),
        Command::Plugins { command } => match command {
            PluginsCommand::List => not_implemented("plugins list"),
        },
        Command::Clean(args) => clean::run(&args),
        Command::Stop => stop::run(),
        Command::McpShim => shim::run(),
        Command::Daemon { project_root } => daemon::run(&project_root),
    }
}

fn not_implemented(name: &str) -> Result<()> {
    bail!("`g-mesh {name}` is not implemented yet")
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
        assert!(parse(&["plugins", "uninstall"]).is_err(), "only `list` exists today");
    }

    /// All four documented `clean` forms land in the same positional slot.
    #[test]
    fn clean_accepts_its_four_documented_forms() {
        let cases = [
            (vec![], None, false),
            (vec!["a1b2c3d4e5f6a7b8"], Some("a1b2c3d4e5f6a7b8"), false),
            (vec!["expired"], Some("expired"), false),
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
        for name in
            ["init", "config", "status", "reindex", "plugins", "clean", "stop", "mcp-shim"]
        {
            assert!(help.contains(name), "`--help` must mention `{name}`:\n{help}");
        }

        // Asked of the declared surface rather than of the rendered text,
        // which mentions the word "daemon" in other commands' descriptions.
        let daemon = Cli::command()
            .get_subcommands()
            .find(|sub| sub.get_name() == "daemon")
            .expect("the daemon subcommand must still exist")
            .clone();
        assert!(
            daemon.is_hide_set(),
            "the shim's private daemon entry point must stay out of --help"
        );
    }

    #[test]
    fn unimplemented_commands_fail_rather_than_pretending_to_work() {
        let err = dispatch(Command::Plugins { command: PluginsCommand::List })
            .expect_err("plugins list is not built yet");
        assert!(err.to_string().contains("not implemented"), "unexpected message: {err}");
    }
}
