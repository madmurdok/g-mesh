//! The plugin has no networking API in its source: its normal dependency
//! graph, on every target, holds no HTTP or socket crate. Development
//! dependencies are not shipped and are not checked.
//!
//! Control: add `ureq` to `[dependencies]` in this crate's `Cargo.toml` ->
//! this test names it.

use std::path::Path;
use std::process::Command;

/// Crates whose presence means the binary can open a connection.
const NETWORK_CRATES: [&str; 10] =
    ["reqwest", "hyper", "ureq", "h2", "curl", "isahc", "attohttpc", "surf", "socket2", "async-std"];

/// Crates that open sockets only with one of these features on.
const NETWORK_FEATURES: [(&str, &str); 2] = [("tokio", "net"), ("mio", "net")];

/// One `<name> <version> <features>` line per package in the normal graph.
fn normal_dependency_graph() -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let output = Command::new(cargo)
        .args(["tree", "--offline", "--locked", "--edges", "normal", "--target", "all", "--prefix", "none"])
        .args(["--format", "{p} {f}", "--manifest-path"])
        .arg(&manifest)
        .output()
        .expect("failed to run `cargo tree`");
    assert!(output.status.success(), "cargo tree failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("cargo tree printed non-UTF-8")
}

#[test]
fn the_normal_dependency_graph_has_no_http_or_socket_crate() {
    let graph = normal_dependency_graph();
    assert!(
        graph.lines().any(|line| line.starts_with("g-mesh-plugin-typescript ")),
        "the graph is this crate's: {graph}"
    );

    let mut found = Vec::new();
    for line in graph.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        if NETWORK_CRATES.contains(&name) {
            found.push(line.to_string());
        }
        // `{f}` prints the enabled features comma-separated after the version
        // (and the source path, for a path dependency); a repeated package
        // ends in `(*)`.
        let last = line.split_whitespace().rfind(|token| *token != "(*)").unwrap_or("");
        let features: Vec<&str> = last.split(',').collect();
        for (krate, feature) in NETWORK_FEATURES {
            if name == krate && features.contains(&feature) {
                found.push(line.to_string());
            }
        }
    }
    assert_eq!(found, Vec::<String>::new(), "networking crates in the normal dependency graph");
}
