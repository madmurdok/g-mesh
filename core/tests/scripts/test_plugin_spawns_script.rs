//! `scripts/test-plugin-spawns.sh observe` wraps the binary of every plugin
//! `build-targets.sh --plugins` lists and writes each map row's plugins in
//! that list's order, cargo plugins first.
//!
//! The script runs for real in a scratch checkout with a stand-in `cargo`
//! whose `nextest run` starts the plugin binaries as one core test would, so
//! no build is paid for. Bash and python3 only, so Unix only.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The plugin list of build-targets.sh as it stands in `PLUGINS=( ... )`.
const PLUGIN_LIST: &str = "PLUGINS=(\n\ttypescript:cargo\n\tgo:go\n\trust:cargo\n\tpython:cargo\n)\n";

/// The same list with a fifth, cargo-built plugin `foo`.
const WITH_FOO: &str =
    "PLUGINS=(\n\ttypescript:cargo\n\tgo:go\n\trust:cargo\n\tpython:cargo\n\tfoo:cargo\n)\n";

/// What every stand-in plugin binary holds.
const PLUGIN_STAND_IN: &str = "#!/bin/sh\nexit 0\n";

fn repo_root() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/..")).to_path_buf()
}

fn write_executable(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A scratch checkout: the real observer and section scripts, build-targets.sh
/// listing `plugin_list`, a core `it` binary of modules `alpha` and `beta`,
/// stand-in binaries for the typescript, foo and go plugins, and a stand-in
/// `cargo` whose `nextest run` starts each of them as test `alpha::spawns`.
fn observer_root(plugin_list: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for script in [
        "scripts/test-plugin-spawns.sh",
        "scripts/test-sections.sh",
        "scripts/prune-stale-objects.sh",
        "scripts/lib/runnable.sh",
    ] {
        write_executable(&root.join(script), &fs::read_to_string(repo_root().join(script)).unwrap());
    }
    let build_targets = fs::read_to_string(repo_root().join("scripts/build-targets.sh")).unwrap();
    assert_eq!(
        build_targets.matches(PLUGIN_LIST).count(),
        1,
        "build-targets.sh no longer holds {PLUGIN_LIST:?}"
    );
    write_executable(
        &root.join("scripts/build-targets.sh"),
        &build_targets.replace(PLUGIN_LIST, plugin_list),
    );
    fs::create_dir_all(root.join("core/tests/it")).unwrap();
    fs::write(root.join("core/tests/it/main.rs"), "mod alpha;\nmod beta;\n").unwrap();
    for binary in plugin_binaries(root) {
        write_executable(&binary, PLUGIN_STAND_IN);
    }
    write_executable(
        &root.join("bin/cargo"),
        r#"#!/bin/bash
case "$1 $2" in
"nextest run")
	for p in typescript foo; do
		NEXTEST_BINARY_ID=g-mesh::it NEXTEST_TEST_NAME=alpha::spawns "$CARGO_TARGET_DIR/debug/g-mesh-plugin-$p"
	done
	NEXTEST_BINARY_ID=g-mesh::it NEXTEST_TEST_NAME=alpha::spawns "$PWD/plugins/go/g-mesh-plugin-go"
	;;
*) echo '{}' ;;
esac
"#,
    );
    dir
}

fn plugin_binaries(root: &Path) -> [PathBuf; 3] {
    [
        root.join("target/debug/g-mesh-plugin-typescript"),
        root.join("target/debug/g-mesh-plugin-foo"),
        root.join("plugins/go/g-mesh-plugin-go"),
    ]
}

fn observe_write(root: &Path) -> Output {
    Command::new("bash")
        .arg(root.join("scripts/test-plugin-spawns.sh"))
        .args(["observe", "--write"])
        .current_dir(root)
        .env("PATH", format!("{}:{}", root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .expect("failed to run bash")
}

/// The plugins the written map lists for `key`.
fn map_row(root: &Path, key: &str) -> Vec<String> {
    let map = fs::read_to_string(root.join("scripts/test-plugin-spawns.txt")).unwrap();
    let row = map
        .lines()
        .find(|line| line.split_whitespace().next() == Some(key))
        .unwrap_or_else(|| panic!("no row {key} in the written map:\n{map}"));
    row.split_whitespace().skip(1).map(str::to_owned).collect()
}

/// A plugin added to build-targets.sh's list is wrapped and observed, and a
/// row lists its plugins in that list's order: cargo plugins, then go ones.
///
/// Control: restore the literal `ORDER = ["typescript", "rust", "python", "go"]`
/// - `foo` drops out of the row and this fails.
#[test]
fn a_plugin_added_to_the_list_is_observed_and_rows_follow_the_list_order() {
    let root = observer_root(WITH_FOO);
    let output = observe_write(root.path());
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(map_row(root.path(), "it:alpha"), ["typescript", "foo", "go"], "{}", describe(&output));
    assert_eq!(map_row(root.path(), "it:beta"), ["none"], "{}", describe(&output));
    for binary in plugin_binaries(root.path()) {
        assert_eq!(
            fs::read_to_string(&binary).unwrap(),
            PLUGIN_STAND_IN,
            "{} not restored",
            binary.display()
        );
    }
}
