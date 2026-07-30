//! Thin entry point: the whole command surface lives in `cli`, where it can
//! be unit-tested without taking over a real process's argv.

use g_mesh::cli;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("g-mesh: {err:#}");
        std::process::exit(1);
    }
}
