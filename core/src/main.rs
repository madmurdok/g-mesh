use std::path::PathBuf;

use clap::{Parser, Subcommand};
use g_mesh::{daemon, shim};

#[derive(Parser)]
#[command(name = "g-mesh", version, about = "Local source code indexer for AI agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Stdio<->AF_UNIX proxy spawned by MCP clients; bootstraps the per-project daemon on demand.
    McpShim,
    /// Internal daemon entry point, bootstrapped by mcp-shim - not invoked directly by users.
    #[command(hide = true)]
    Daemon {
        /// Project root this daemon serves; passed explicitly by the shim
        /// rather than inferred from cwd, which a detached process must not
        /// depend on.
        #[arg(long)]
        project_root: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::McpShim => shim::run(),
        Command::Daemon { project_root } => daemon::run(&project_root),
    };

    if let Err(err) = result {
        eprintln!("g-mesh: {err:#}");
        std::process::exit(1);
    }
}
