//! The release packaging scripts ship the JS/TS plugin as the cargo
//! binary built from `plugins/typescript`, beside a manifest copied from
//! `plugins/typescript/plugin.toml`, and refuse a staged archive without it.
//!
//! Each script runs for real, as a copy in a scratch repo root (or, for
//! `release-smoke.sh`, in place against a scratch stage), with `cargo`,
//! `rustc`, `rustup`, the other bundlers and the staged `g-mesh` replaced by
//! small shell stand-ins. No release build is paid for: what is under test is
//! the scripts' staging and checking, not the binaries they stage. Bash only,
//! so Unix only: the release is packaged from a Unix shell.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use g_mesh::daemon::manifest::read_manifest;

const HOST: &str = "x86_64-unknown-linux-gnu";
const WINDOWS: &str = "x86_64-pc-windows-msvc";
/// The spawn command `plugins/typescript/plugin.toml` declares for a checkout;
/// `bundle-plugin.sh` rewrites exactly this line.
const DEV_COMMAND: &str = r#"command = "${G_MESH_BIN_DIR}/g-mesh-plugin-typescript""#;

fn repo_root() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/..")).to_path_buf()
}

fn write_executable(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn copy_from_repo(root: &Path, relative: &str) {
    let target = root.join(relative);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::copy(repo_root().join(relative), &target)
        .unwrap_or_else(|err| panic!("failed to copy {relative}: {err}"));
}

/// `rustc`, `rustup` and `cargo` stand-ins. `rustc -vV` reports `HOST`;
/// `cargo build --profile P --target T` copies `$FAKE_CARGO_PAYLOAD` to
/// `$FAKE_REPO_ROOT/target/T/<P's dir>/$FAKE_CARGO_OUTPUT`, where both scripts
/// look for what they built.
fn fake_toolchain(bin: &Path) {
    write_executable(&bin.join("rustc"), &format!("#!/bin/sh\necho 'host: {HOST}'\n"));
    write_executable(&bin.join("rustup"), "#!/bin/sh\nexit 0\n");
    write_executable(
        &bin.join("cargo"),
        r#"#!/bin/bash
set -eu
profile=""
target=""
while [ $# -gt 0 ]; do
	case "$1" in
	--profile) profile="$2"; shift 2 ;;
	--target) target="$2"; shift 2 ;;
	*) shift ;;
	esac
done
[ "$profile" = dev ] && profile=debug
out="$FAKE_REPO_ROOT/target/$target/$profile"
mkdir -p "$out"
cp "$FAKE_CARGO_PAYLOAD" "$out/$FAKE_CARGO_OUTPUT"
chmod +x "$out/$FAKE_CARGO_OUTPUT"
"#,
    );
}

fn path_with(bin: &Path) -> String {
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// ---------------------------------------------------------------------------
// scripts/bundle-plugin.sh
// ---------------------------------------------------------------------------

/// A scratch repo root holding a copy of `bundle-plugin.sh` and of the real
/// `plugins/typescript/plugin.toml`, passed through `edit_manifest`.
struct BundleRoot {
    root: tempfile::TempDir,
}

impl BundleRoot {
    fn new(edit_manifest: impl FnOnce(String) -> String) -> Self {
        let root = tempfile::tempdir().expect("failed to create a scratch repo root");
        copy_from_repo(root.path(), "scripts/bundle-plugin.sh");
        let manifest = fs::read_to_string(repo_root().join("plugins/typescript/plugin.toml")).unwrap();
        let target = root.path().join("plugins/typescript/plugin.toml");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, edit_manifest(manifest)).unwrap();
        fake_toolchain(&root.path().join("fake-bin"));
        // The staged plugin's handshake, as far as the host smoke test reads it.
        write_executable(
            &root.path().join("payload/g-mesh-plugin-typescript"),
            "#!/bin/sh\necho '{\"language\":\"typescript\"}'\n",
        );
        BundleRoot { root }
    }

    fn dest(&self) -> PathBuf {
        self.root.path().join("dist/plugins")
    }

    fn stage(&self) -> PathBuf {
        self.dest().join("typescript")
    }

    fn run(&self, target: &str) -> Output {
        let exe = exe_for(target);
        Command::new("bash")
            .arg(self.root.path().join("scripts/bundle-plugin.sh"))
            .arg(target)
            .arg(self.dest())
            .env("PATH", path_with(&self.root.path().join("fake-bin")))
            .env("CARGO_PROFILE", "release")
            .env("FAKE_REPO_ROOT", self.root.path())
            .env("FAKE_CARGO_PAYLOAD", self.root.path().join("payload/g-mesh-plugin-typescript"))
            .env("FAKE_CARGO_OUTPUT", exe)
            .output()
            .expect("failed to run bash")
    }
}

fn exe_for(target: &str) -> &'static str {
    if target.contains("-windows-") {
        "g-mesh-plugin-typescript.exe"
    } else {
        "g-mesh-plugin-typescript"
    }
}

fn entries_of(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("failed to list {}: {err}", dir.display()))
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// Asserts the staged manifest is the repo's, byte for byte, below a header of
/// comment lines, with only the spawn command rewritten to `./<exe>`.
fn assert_staged_manifest_is_the_repo_one_with_only_the_command_rewritten(stage: &Path, exe: &str) {
    let source = fs::read_to_string(repo_root().join("plugins/typescript/plugin.toml")).unwrap();
    assert_eq!(
        source.matches(DEV_COMMAND).count(),
        1,
        "the repo manifest names the dev command exactly once"
    );
    let expected = source.replace(DEV_COMMAND, &format!(r#"command = "./{exe}""#));
    let staged = fs::read_to_string(stage.join("plugin.toml")).unwrap();
    let header = staged.strip_suffix(expected.as_str()).unwrap_or_else(|| {
        panic!("the staged manifest is not the repo's with only the command rewritten:\n{staged}")
    });
    assert!(
        header.lines().all(|line| line.starts_with('#')),
        "only comment lines may precede the copied manifest:\n{header}"
    );
}

/// Behaviour 6: the installed manifest is `plugins/typescript/plugin.toml`
/// with only `[plugin.spawn] command` changed, naming the binary staged beside
/// it.
///
/// Control: keep `${G_MESH_BIN_DIR}` in the sed replacement - the script's own
/// post-check dies and this fails.
#[test]
fn the_staged_typescript_manifest_is_the_repo_manifest_with_only_the_command_rewritten() {
    let bundle = BundleRoot::new(|manifest| manifest);
    let output = bundle.run(HOST);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout_of(&output).contains("handshake ok"), "{}", describe(&output));
    assert_staged_manifest_is_the_repo_one_with_only_the_command_rewritten(
        &bundle.stage(),
        "g-mesh-plugin-typescript",
    );
}

/// Read through `read_manifest` from the staged directory, a release install
/// refuses, strips and links TypeScript exactly as a checkout does, and
/// reports the same `plugin_version`.
///
/// Control: make the script drop `[plugin.reexports]` from its copy
/// (`sed ... | sed '/^\[plugin.reexports\]/,$d'`) - this fails.
#[test]
fn the_staged_typescript_manifest_declares_the_repo_manifests_query_shapes_and_reexport_rules() {
    let bundle = BundleRoot::new(|manifest| manifest);
    let output = bundle.run(HOST);
    assert!(output.status.success(), "{}", describe(&output));

    let staged = read_manifest(&bundle.stage()).expect("the staged manifest must satisfy read_manifest");
    let repo = read_manifest(&repo_root().join("plugins/typescript")).unwrap();
    assert_eq!(staged.non_symbol_queries, repo.non_symbol_queries);
    assert_eq!(staged.symbol_query_prefixes, repo.symbol_query_prefixes);
    assert_eq!(staged.symbol_query_prefixes.strip, vec!["@".to_string()]);
    assert_eq!(staged.reexports, repo.reexports);
    assert!(staged.reexports.named_shadows_glob);
    assert_eq!(staged.plugin_version, repo.plugin_version);
    assert_eq!(staged.command, bundle.stage().join("g-mesh-plugin-typescript"));
}

/// Behaviour 5: the stage holds the manifest and the cargo binary, nothing
/// else, even when an earlier bundle left other files in the same place.
///
/// Control: drop `rm -rf "$stage"` - the seeded leftovers survive and this
/// fails.
#[test]
fn the_typescript_stage_holds_only_the_manifest_and_the_binary() {
    let bundle = BundleRoot::new(|manifest| manifest);
    let stale = bundle.stage();
    fs::create_dir_all(stale.join("node_modules/some-package")).unwrap();
    fs::write(stale.join("LICENSE-nodejs"), "stale").unwrap();
    fs::write(stale.join("sea-prep.blob"), "stale").unwrap();

    let output = bundle.run(HOST);
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(
        entries_of(&bundle.stage()),
        BTreeSet::from(["plugin.toml".to_string(), "g-mesh-plugin-typescript".to_string()])
    );
}

/// Behaviour 8 (and 6 for Windows): a supported target other than the host is
/// bundled, not refused for being foreign, and its manifest names the `.exe`.
///
/// Control: re-add `[ "$target" = "$host" ] || die ...` after the supported
/// target check - this fails.
#[test]
fn a_supported_non_host_target_is_bundled_with_its_exe_named_in_the_manifest() {
    let bundle = BundleRoot::new(|manifest| manifest);
    let output = bundle.run(WINDOWS);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout_of(&output).contains("smoke test skipped"), "{}", describe(&output));
    assert_eq!(
        entries_of(&bundle.stage()),
        BTreeSet::from(["plugin.toml".to_string(), "g-mesh-plugin-typescript.exe".to_string()])
    );
    assert_staged_manifest_is_the_repo_one_with_only_the_command_rewritten(
        &bundle.stage(),
        "g-mesh-plugin-typescript.exe",
    );
}

/// Behaviour 8, other half: an unsupported target is refused before building.
///
/// Control: drop the `SUPPORTED_TARGETS` check - the stand-in cargo builds
/// anything and this fails.
#[test]
fn an_unsupported_target_is_refused() {
    let bundle = BundleRoot::new(|manifest| manifest);
    let output = bundle.run("riscv64gc-unknown-linux-gnu");
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("unsupported target: riscv64gc-unknown-linux-gnu"),
        "{}",
        describe(&output)
    );
    assert!(!bundle.stage().exists(), "nothing may be staged for an unsupported target");
}

/// Behaviour 7: if `plugins/typescript/plugin.toml` stops spelling the dev
/// command the way the script rewrites it, bundling stops and says so instead
/// of shipping the dev command.
///
/// Control: drop the `grep -qF "$marker"` guard - the script then dies later
/// with "failed to rewrite the command line", not this message, and this fails.
#[test]
fn a_repo_manifest_without_the_rewritten_command_line_stops_the_bundle() {
    let bundle = BundleRoot::new(|manifest| {
        let edited =
            manifest.replace(DEV_COMMAND, r#"command   =   "${G_MESH_BIN_DIR}/g-mesh-plugin-typescript""#);
        assert_ne!(edited, manifest, "the fixture must actually respell the command");
        edited
    });
    let output = bundle.run(HOST);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(stderr_of(&output).contains("no longer contains"), "{}", describe(&output));
}

// ---------------------------------------------------------------------------
// scripts/build-targets.sh
// ---------------------------------------------------------------------------

/// A scratch repo root holding a copy of `build-targets.sh`, stand-in
/// bundlers, and a stand-in `g-mesh` whose `plugins list` prints
/// `$FAKE_PLUGINS_LIST`. The TypeScript bundler stages `plugin.toml`, and its
/// binary only when `$FAKE_STAGE_TS_BINARY` is 1.
fn build_targets_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("failed to create a scratch repo root");
    let path = root.path();
    copy_from_repo(path, "scripts/build-targets.sh");
    write_executable(
        &path.join("scripts/bundle-plugin.sh"),
        r#"#!/bin/bash
set -eu
mkdir -p "$2/typescript"
echo 'language = "typescript"' >"$2/typescript/plugin.toml"
if [ "${FAKE_STAGE_TS_BINARY:-}" = 1 ]; then
	touch "$2/typescript/g-mesh-plugin-typescript"
fi
"#,
    );
    for (script, language) in [
        ("bundle-go-plugin.sh", "go"),
        ("bundle-rust-plugin.sh", "rust"),
        ("bundle-python-plugin.sh", "python"),
    ] {
        write_executable(
            &path.join("scripts").join(script),
            &format!("#!/bin/sh\nmkdir -p \"$2/{language}\"\n"),
        );
    }
    for file in ["LICENSE", "LICENSE-MIT", "LICENSE-APACHE", "README.md"] {
        fs::write(path.join(file), "").unwrap();
    }
    fs::create_dir_all(path.join("core")).unwrap();
    fake_toolchain(&path.join("fake-bin"));
    write_executable(
        &path.join("payload/g-mesh"),
        r#"#!/bin/sh
case "$1" in
--version) echo "g-mesh 9.9.9" ;;
plugins) printf '%b' "$FAKE_PLUGINS_LIST" ;;
esac
"#,
    );
    root
}

const ALL_PLUGINS_LISTED: &str =
    "typescript  4.0.0  bundled\\ngo  4.0.0  bundled\\nrust  4.0.0  bundled\\npython  4.0.0  bundled\\n";

fn run_build_targets(root: &Path, stage_ts_binary: bool, plugins_list: &str) -> Output {
    Command::new("bash")
        .arg(root.join("scripts/build-targets.sh"))
        .arg(HOST)
        .env("PATH", path_with(&root.join("fake-bin")))
        .env("CARGO_PROFILE", "release")
        .env("G_MESH_VERSION", "9.9.9")
        .env("DIST_DIR", root.join("dist"))
        .env_remove("G_MESH_SKIP_PLUGIN_BUNDLE")
        .env("FAKE_REPO_ROOT", root)
        .env("FAKE_CARGO_PAYLOAD", root.join("payload/g-mesh"))
        .env("FAKE_CARGO_OUTPUT", "g-mesh")
        .env("FAKE_STAGE_TS_BINARY", if stage_ts_binary { "1" } else { "0" })
        .env("FAKE_PLUGINS_LIST", plugins_list)
        .output()
        .expect("failed to run bash")
}

/// The setup control for the two tests below: with the binary staged and every
/// plugin listed, the build passes its smoke test.
#[test]
fn build_targets_passes_a_stage_with_the_typescript_binary_and_every_plugin_listed() {
    let root = build_targets_root();
    let output = run_build_targets(root.path(), true, ALL_PLUGINS_LISTED);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout_of(&output).contains("artifact:"), "{}", describe(&output));
}

/// Behaviour 1: `plugins list` reads manifests only, so a stage with
/// `typescript/plugin.toml` and no binary still lists `typescript`; the build
/// must fail on the missing binary itself.
///
/// Control: drop the `[ -f "$stage_dir/plugins/typescript/$ts_exe" ]` term -
/// the build passes and this fails.
#[test]
fn build_targets_fails_when_the_staged_typescript_binary_is_missing() {
    let root = build_targets_root();
    let output = run_build_targets(root.path(), false, ALL_PLUGINS_LISTED);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("its binary plugins/typescript/g-mesh-plugin-typescript is missing"),
        "{}",
        describe(&output)
    );
}

/// Behaviour 2: a `typescript` row that is a manifest error, not a version, is
/// not a discovered plugin.
///
/// Control: put the pattern back to `grep -q "typescript"` - the build passes
/// and this fails.
#[test]
fn build_targets_fails_when_plugins_list_reports_the_typescript_manifest_as_an_error() {
    let root = build_targets_root();
    let listed = ALL_PLUGINS_LISTED
        .replace("typescript  4.0.0  bundled", "typescript  error: failed to parse plugin.toml");
    let output = run_build_targets(root.path(), true, &listed);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("does not discover the typescript plugin staged beside it"),
        "{}",
        describe(&output)
    );
}

// ---------------------------------------------------------------------------
// scripts/release-smoke.sh
// ---------------------------------------------------------------------------

/// A staged artifact whose `g-mesh` stand-in reports a non-empty index for
/// any `reindex`, so the script passes unless a staged-files check stops it.
/// `ts_plugin` names the TypeScript binary to stage, if any.
fn smoke_stage(bin_name: &str, ts_plugin: Option<&str>) -> tempfile::TempDir {
    let stage = tempfile::tempdir().expect("failed to create a scratch stage");
    write_executable(&stage.path().join(bin_name), "#!/bin/sh\necho 'index: 5 nodes, 3 edges'\n");
    if let Some(name) = ts_plugin {
        write_executable(&stage.path().join("plugins/typescript").join(name), "#!/bin/sh\n");
    }
    stage
}

fn run_release_smoke(stage: &Path, target: &str) -> Output {
    Command::new("bash")
        .arg(repo_root().join("scripts/release-smoke.sh"))
        .arg(stage)
        .arg(target)
        .output()
        .expect("failed to run bash")
}

/// Behaviour 3: a stage without the TypeScript binary is refused before the
/// reindex, naming the missing file. Without this check the script still fails,
/// later and for another reason (`reindex` exits 2), so the message and the
/// absence of the reindex are what tell the two apart.
///
/// Control: drop `$ts_plugin_bin` from the staged-files loop - the stand-in
/// reindex runs and passes, and this fails.
#[test]
fn release_smoke_refuses_a_stage_without_the_typescript_binary_before_reindexing() {
    let stage = smoke_stage("g-mesh", None);
    let output = run_release_smoke(stage.path(), HOST);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        stderr_of(&output).contains("staged binary not found:")
            && stderr_of(&output)
                .contains("/plugins/typescript/g-mesh-plugin-typescript (did the packaging step"),
        "{}",
        describe(&output)
    );
    assert!(!stdout_of(&output).contains("reindexing"), "no reindex may run: {}", describe(&output));
}

/// Behaviour 4: on Windows the required binary is the `.exe`; a stage with an
/// extensionless one is refused.
///
/// Control: hardcode `g-mesh-plugin-typescript` (no suffix) - this stage then
/// passes and this fails.
#[test]
fn release_smoke_requires_the_exe_on_windows() {
    let stage = smoke_stage("g-mesh.exe", Some("g-mesh-plugin-typescript"));
    let output = run_release_smoke(stage.path(), WINDOWS);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        stderr_of(&output)
            .contains("/plugins/typescript/g-mesh-plugin-typescript.exe (did the packaging step"),
        "{}",
        describe(&output)
    );
    assert!(!stdout_of(&output).contains("reindexing"), "no reindex may run: {}", describe(&output));
}

/// Behaviour 4, other half: a Windows stage holding the `.exe` passes.
///
/// Control: hardcode `g-mesh-plugin-typescript` (no suffix) - this stage is
/// refused and this fails.
#[test]
fn release_smoke_accepts_a_windows_stage_holding_the_exe() {
    let stage = smoke_stage("g-mesh.exe", Some("g-mesh-plugin-typescript.exe"));
    let output = run_release_smoke(stage.path(), WINDOWS);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout_of(&output).contains("PASS"), "{}", describe(&output));
}

/// The setup control for the refusal above: the same stage with the binary
/// passes, so the refusal is about the binary and not the harness.
#[test]
fn release_smoke_accepts_a_stage_holding_the_typescript_binary() {
    let stage = smoke_stage("g-mesh", Some("g-mesh-plugin-typescript"));
    let output = run_release_smoke(stage.path(), HOST);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout_of(&output).contains("PASS"), "{}", describe(&output));
}

// ---------------------------------------------------------------------------
// .github/workflows/release.yml
// ---------------------------------------------------------------------------

/// Behaviour 9: release runners no longer install Node, record a Node version,
/// or report a "Node runtime inside" column - no artifact embeds one.
///
/// Control: re-add an `actions/setup-node` step - this fails.
#[test]
fn the_release_workflow_sets_up_and_reports_no_node_runtime() {
    let workflow = fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap();
    for forbidden in ["actions/setup-node", "echo \"node=", "Node runtime inside"] {
        assert!(!workflow.contains(forbidden), "release.yml still contains {forbidden:?}");
    }
}
