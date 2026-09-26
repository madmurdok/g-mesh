//! The Go plugin's fingerprint must not move when nothing but the state of
//! the git checkout around it changes.
//!
//! `daemon::plugin::fingerprint` digests the plugin directory's bytes, the
//! compiled `g-mesh-plugin-go` included, and the index's generation is built
//! from it - so anything that changes the binary without changing the
//! plugin's source wipes every index on the next daemon start. Go stamps the
//! enclosing repository's revision and "tree is dirty" bit into a binary by
//! default, so without `-buildvcs=false` editing an unrelated file (say, the
//! root `Cargo.toml`) and rebuilding is enough to trigger that wipe.
//!
//! The test builds a copy of `plugins/go` inside a scratch git repository
//! with the same flags `core/build.rs` uses, dirties a file outside the
//! plugin directory, rebuilds, and compares fingerprints. It needs `go` and
//! `git` on `PATH`, like the rest of the suite's Go plugin tests.

use std::fs;
use std::path::Path;
use std::process::Command;

use g_mesh::daemon::manifest::read_manifest;
use g_mesh::daemon::plugin::fingerprint;

include!("../go_plugin_build_flags.rs");

const PLUGIN_BINARY: &str = "g-mesh-plugin-go";

fn plugin_source_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../plugins/go"))
}

/// Copies the plugin's tree minus any binary already built in it, so the
/// copy's fingerprint reflects only what this test builds.
fn copy_plugin_source(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        let file_type = entry.file_type().unwrap();
        if file_type.is_dir() {
            copy_plugin_source(&entry.path(), &target);
        } else if file_type.is_file() && entry.file_name() != PLUGIN_BINARY {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn run(command: &mut Command) {
    let output = command.output().unwrap_or_else(|err| panic!("failed to run {command:?}: {err}"));
    assert!(
        output.status.success(),
        "{command:?} failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(repo: &Path, args: &[&str]) {
    run(Command::new("git")
        .args([
            "-c",
            "user.name=g-mesh test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(repo));
}

/// Builds the plugin the way `core/build.rs` does. `GOFLAGS` is cleared so a
/// contributor's own environment cannot supply the flag this test is about.
fn build_plugin(plugin_dir: &Path) {
    run(Command::new("go")
        .arg("build")
        .args(GO_PLUGIN_BUILD_FLAGS)
        .args(["-o", PLUGIN_BINARY, "."])
        .env_remove("GOFLAGS")
        .current_dir(plugin_dir));
}

fn plugin_fingerprint(plugin_dir: &Path) -> String {
    fingerprint(&read_manifest(plugin_dir).expect("the copied plugin.toml must load"))
}

#[test]
fn the_go_plugin_fingerprint_ignores_the_checkouts_git_state() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path();
    let plugin_dir = repo.join("plugins").join("go");
    copy_plugin_source(plugin_source_dir(), &plugin_dir);
    let unrelated = repo.join("Cargo.toml");
    fs::write(&unrelated, "[workspace]\n").unwrap();

    git(repo, &["init", "--quiet"]);
    // The binary is never committed, just as `plugins/go/.gitignore` keeps
    // it out of the real checkout.
    fs::write(repo.join(".gitignore"), format!("/plugins/go/{PLUGIN_BINARY}\n")).unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "--quiet", "-m", "clean"]);

    build_plugin(&plugin_dir);
    let clean = plugin_fingerprint(&plugin_dir);

    fs::write(&unrelated, "[workspace]\n# edited\n").unwrap();
    build_plugin(&plugin_dir);
    let dirty = plugin_fingerprint(&plugin_dir);

    assert_eq!(
        clean, dirty,
        "editing a file outside plugins/go and rebuilding changed the Go plugin's fingerprint - \
         the build is stamping git state into the binary"
    );
}

/// The release bundle builds its own copy of the binary, and an installed
/// plugin's fingerprint is taken from that copy, so it must pass the same
/// flags as `core/build.rs`.
#[test]
fn the_go_plugin_bundle_script_passes_the_same_build_flags() {
    let script_path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/bundle-go-plugin.sh"));
    let script = fs::read_to_string(script_path).unwrap();
    let build_lines: Vec<&str> = script.lines().filter(|line| line.contains("\"$GO_BIN\" build")).collect();
    assert_eq!(
        build_lines.len(),
        1,
        "expected exactly one `go build` in {}: {build_lines:?}",
        script_path.display()
    );
    assert!(GO_PLUGIN_BUILD_FLAGS.contains(&"-buildvcs=false"));
    for flag in GO_PLUGIN_BUILD_FLAGS {
        assert!(
            build_lines[0].contains(flag),
            "{} does not pass {flag}: {}",
            script_path.display(),
            build_lines[0]
        );
    }
}
