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
    Daemon,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::McpShim => shim::run(),
        Command::Daemon => daemon::run(),
    }
}
