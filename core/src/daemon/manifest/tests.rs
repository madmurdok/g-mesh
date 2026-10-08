use super::*;
use std::sync::Mutex;

/// Guards every test below that touches [`PLUGIN_ROOTS_OVERRIDE_ENV`]: it
/// is process-wide state, and `cargo test` runs this module's tests on
/// multiple threads by default, so two of them setting/clearing the same
/// variable at once would be a genuine race - same reasoning as
/// `daemon::lifecycle`'s `ENV_LOCK` for its own env-var tests.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The bundled TypeScript plugin's source directory, reached the same way
/// [`bundled_roots`] reaches it.
fn bundled_typescript_plugin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript")
}

// The bundled plugins' `plugin_version` follows the cargo-workspace rule in
// `docs/architecture/plugin-modularity.md` ("`plugin_version`: two rules, not
// one"): it equals the plugin crate's version, and each plugin crate's own
// `the_manifest_version_matches_the_crates` test enforces that.

/// The version that actually leaves the plugin at runtime, reported in its
/// handshake.
///
/// This asks whether the manifest core reads *without* running a plugin
/// agrees with what the running plugin says. Those two can drift, because `plugin.toml` is read by
/// `g-mesh plugins list` on plugins that were never built and so cannot
/// be derived from anything at runtime.
///
/// Core prints it when `handshake::verify` refuses a protocol mismatch -
/// "protocol version mismatch with typescript plugin (plugin version X)" -
/// while `g-mesh plugins list` prints the manifest's copy. The two had
/// drifted: the manifest said 2.1.0 and the wire said 0.1.0, so the two
/// screens a person consults about one plugin named different versions,
/// and the one shown at the worst possible moment was three releases
/// stale.
///
/// Checked by spawning the real plugin rather than by reading its source:
/// what matters is the value that arrives on core's stdin. A source-text
/// check would pass just as happily on a build that was never rerun after
/// the version changed.
#[test]
fn the_bundled_plugins_handshake_reports_the_version_its_manifest_declares() {
    let dir = bundled_typescript_plugin_dir();
    let manifest = read_manifest(&dir).expect("failed to read the bundled plugin's manifest");

    // Same check `daemon::plugin::PluginState::spawn` makes before
    // spawning for real, so an unbuilt binary fails naming the build
    // command rather than as a bare spawn error.
    if let Some(hint) = crate::daemon::plugin::missing_workspace_binary_hint(&manifest.command) {
        panic!("{hint}");
    }

    let mut plugin = std::process::Command::new(&manifest.command)
        .args(&manifest.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn the bundled plugin - is it built? (`cargo build --workspace`)");
    let mut stdout = std::io::BufReader::new(plugin.stdout.take().expect("the plugin has no stdout"));
    let handshake = crate::protocol::handshake::perform(&mut stdout);
    drop(plugin.stdin.take());
    let _ = plugin.kill();
    let _ = plugin.wait();

    let handshake = handshake.expect("the bundled plugin did not complete a handshake");
    assert_eq!(
        handshake.plugin_version, manifest.plugin_version,
        "the bundled plugin's handshake reports {}, but its plugin.toml declares {} - \
         either plugin.toml is stale or the plugin needs rebuilding",
        handshake.plugin_version, manifest.plugin_version
    );
}

#[test]
fn default_roots_bundled_entry_resolves_to_the_sibling_plugins_directory() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var(PLUGIN_ROOTS_OVERRIDE_ENV);

    let roots = default_roots();
    let bundled = roots.last().expect("default_roots must return at least the bundled root");

    assert!(
        bundled.join("typescript").join(MANIFEST_FILE_NAME).is_file(),
        "expected {} to contain typescript/plugin.toml",
        bundled.display()
    );
}

/// The half of the answer a release archive needs: an unpacked install has
/// no `CARGO_MANIFEST_DIR` to resolve, so discovery has to be able to find
/// `plugins/` beside the binary that is running. Ordered ahead of the
/// checkout root - see [`bundled_roots`].
#[test]
fn the_installed_root_sits_beside_the_executable_and_outranks_the_checkout_root() {
    let roots = bundled_roots();

    let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    assert_eq!(
        roots.first(),
        Some(&exe_dir.join("plugins")),
        "the installed root must be `plugins/` next to the running executable"
    );
    assert_eq!(roots.len(), 2, "installed and checkout roots, in that order");
}

#[test]
fn the_override_env_var_replaces_the_entire_default_roots_list() {
    let _guard = ENV_LOCK.lock().unwrap();
    let override_dir = tempfile::tempdir().unwrap();
    std::env::set_var(PLUGIN_ROOTS_OVERRIDE_ENV, override_dir.path());

    let roots = default_roots();

    std::env::remove_var(PLUGIN_ROOTS_OVERRIDE_ENV);

    assert_eq!(roots, vec![override_dir.path().to_path_buf()]);
}

/// Writes `plugin.toml` under a directory named `dir_name` (inside a
/// fresh tempdir) with `body` as its contents, and returns that
/// directory - the shape every test here needs, since `language` must
/// equal the directory's own name to parse successfully.
fn plugin_dir(dir_name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let plugin_dir = root.path().join(dir_name);
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(plugin_dir.join(MANIFEST_FILE_NAME), body).unwrap();
    (root, plugin_dir)
}

/// Builds a fresh tempdir root containing one `<dir_name>/plugin.toml`
/// per entry in `plugins` - the multi-language, single-root analog of
/// [`plugin_dir`], for `discover()` tests that need more than one
/// language directory under a root. Returns the root path itself
/// (unlike `plugin_dir`, which returns a plugin subdirectory).
pub(crate) fn discovery_root(plugins: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    for (dir_name, body) in plugins {
        let plugin_dir = root.path().join(dir_name);
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join(MANIFEST_FILE_NAME), body).unwrap();
    }
    let path = root.path().to_path_buf();
    (root, path)
}

/// A well-formed `plugin.toml` body for `discover()` tests, parameterized
/// over the fields those tests actually vary - `language` (must match
/// its containing directory name, same as [`well_formed_toml`]),
/// `plugin_version` (used to tell two same-language manifests apart),
/// and `extensions` (used to construct routing conflicts).
pub(crate) fn manifest_toml(language: &str, plugin_version: &str, extensions: &[&str]) -> String {
    let extensions = extensions.iter().map(|ext| format!("\"{ext}\"")).collect::<Vec<_>>().join(", ");
    format!(
        r#"
[plugin]
language = "{language}"
protocol_version = {version}
plugin_version = "{plugin_version}"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [{extensions}]
"#,
        version = CURRENT_PROTOCOL_VERSION,
    )
}

#[test]
fn a_language_in_an_earlier_root_shadows_the_same_language_in_a_later_root() {
    let (_root1, root1) = discovery_root(&[("python", &manifest_toml("python", "1.0.0", &[".py"]))]);
    let (_root2, root2) = discovery_root(&[("python", &manifest_toml("python", "2.0.0", &[".py"]))]);

    let discovered = discover(&[root1, root2]).unwrap();

    assert_eq!(discovered.manifests.len(), 1);
    assert_eq!(discovered.manifests["python"].plugin_version, "1.0.0");
    assert_eq!(discovered.routing.get(".py"), Some(&"python".to_string()));
}

#[test]
fn two_different_languages_claiming_the_same_extension_is_a_hard_error() {
    let (_root, root) = discovery_root(&[
        ("python", &manifest_toml("python", "1.0.0", &[".foo"])),
        ("go", &manifest_toml("go", "1.0.0", &[".foo"])),
    ]);

    let err = discover(std::slice::from_ref(&root)).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("python"), "{message}");
    assert!(message.contains("go"), "{message}");
    assert!(
        message.contains(&root.join("python").join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()),
        "{message}"
    );
    assert!(
        message.contains(&root.join("go").join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()),
        "{message}"
    );
}

#[test]
fn a_root_that_does_not_exist_contributes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let missing_root = root.path().join("does-not-exist");

    let discovered = discover(&[missing_root]).unwrap();

    assert!(discovered.manifests.is_empty());
    assert!(discovered.routing.is_empty());
}

#[test]
fn an_empty_root_directory_contributes_nothing() {
    let root = tempfile::tempdir().unwrap();

    let discovered = discover(&[root.path().to_path_buf()]).unwrap();

    assert!(discovered.manifests.is_empty());
    assert!(discovered.routing.is_empty());
}

#[test]
fn discovery_with_two_languages_and_no_conflicts_populates_manifests_and_routing() {
    let (_root, root) = discovery_root(&[
        ("python", &manifest_toml("python", "1.0.0", &[".py", ".pyi"])),
        ("go", &manifest_toml("go", "1.0.0", &[".go"])),
    ]);

    let discovered = discover(&[root]).unwrap();

    assert_eq!(discovered.manifests.len(), 2);
    assert!(discovered.manifests.contains_key("python"));
    assert!(discovered.manifests.contains_key("go"));
    assert_eq!(discovered.routing.get(".py"), Some(&"python".to_string()));
    assert_eq!(discovered.routing.get(".pyi"), Some(&"python".to_string()));
    assert_eq!(discovered.routing.get(".go"), Some(&"go".to_string()));
}

fn well_formed_toml() -> String {
    format!(
        r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py", ".pyi"]

[plugin.fingerprint]
ignore = ["node_modules"]
"#,
        version = CURRENT_PROTOCOL_VERSION,
    )
}

#[test]
fn parses_a_well_formed_manifest_into_the_expected_struct() {
    let (_root, dir) = plugin_dir("python", &well_formed_toml());

    let manifest = read_manifest(&dir).unwrap();

    assert_eq!(manifest.language, "python");
    assert_eq!(manifest.protocol_version, CURRENT_PROTOCOL_VERSION);
    assert_eq!(manifest.plugin_version, "0.1.0");
    // "node" has no path separator, so it stays a bare command for
    // `$PATH` lookup rather than being joined against `dir`.
    assert_eq!(manifest.command, PathBuf::from("node"));
    // "dist/src/index.js" does have a separator, so it is resolved
    // against the manifest's own directory.
    assert_eq!(manifest.args, vec![dir.join("dist/src/index.js").to_string_lossy().into_owned()]);
    assert_eq!(manifest.extensions, vec![".py".to_string(), ".pyi".to_string()]);
    assert_eq!(manifest.fingerprint_ignore, vec!["node_modules".to_string()]);
    assert_eq!(manifest.manifest_dir, dir);
}

#[test]
fn rejects_malformed_toml_naming_the_manifest_path() {
    let (_root, dir) = plugin_dir("python", "this is not [ valid toml");

    let err = read_manifest(&dir).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
}

#[test]
fn rejects_a_manifest_missing_a_required_field_naming_the_path_and_field() {
    // No `plugin_version` under `[plugin]`.
    let body = format!(
        r#"
[plugin]
language = "python"
protocol_version = {version}

[plugin.spawn]
command = "node"

[plugin.languages]
extensions = [".py"]
"#,
        version = CURRENT_PROTOCOL_VERSION,
    );
    let (_root, dir) = plugin_dir("python", &body);

    let err = read_manifest(&dir).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
    assert!(message.contains("plugin_version"), "{message}");
}

#[test]
fn rejects_language_not_matching_the_directory_name() {
    let body = well_formed_toml(); // declares language = "python"
    let (_root, dir) = plugin_dir("not-python", &body);

    let err = read_manifest(&dir).unwrap_err();
    let message = err.to_string();
    assert!(message.contains("python"), "{message}");
    assert!(message.contains("not-python"), "{message}");
}

#[test]
fn rejects_an_unrecognized_protocol_version() {
    let bad_version = CURRENT_PROTOCOL_VERSION + 1;
    let body = format!(
        r#"
[plugin]
language = "python"
protocol_version = {bad_version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]
"#,
    );
    let (_root, dir) = plugin_dir("python", &body);

    let err = read_manifest(&dir).unwrap_err();
    let message = err.to_string();
    assert!(message.contains(&bad_version.to_string()), "{message}");
    assert!(message.contains(&CURRENT_PROTOCOL_VERSION.to_string()), "{message}");
}

#[test]
fn a_manifest_with_no_fingerprint_table_defaults_to_an_empty_ignore_list() {
    let body = format!(
        r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]
"#,
        version = CURRENT_PROTOCOL_VERSION,
    );
    let (_root, dir) = plugin_dir("python", &body);

    let manifest = read_manifest(&dir).unwrap();

    assert_eq!(manifest.fingerprint_ignore, Vec::<String>::new());
}

/// This task's own acceptance criterion, stated directly: a manifest
/// that never mentions `[plugin.capabilities]` or `[plugin.workspace]`
/// at all parses to the conservative defaults documented on
/// [`Capabilities::default`] and [`WorkspaceConfig::default`] - no
/// semantic pass, receiver calls unresolved at both tiers, nothing
/// watched, nothing excluded, no entry points. Same fixture body as
/// [`a_manifest_with_no_fingerprint_table_defaults_to_an_empty_ignore_list`],
/// applying the same "an absent optional table is not an error" rule to
/// the two newer sections.
#[test]
fn a_manifest_with_no_capabilities_or_workspace_table_defaults_conservatively() {
    let body = format!(
        r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]
"#,
        version = CURRENT_PROTOCOL_VERSION,
    );
    let (_root, dir) = plugin_dir("python", &body);

    let manifest = read_manifest(&dir).unwrap();

    assert_eq!(manifest.capabilities, Capabilities::default());
    assert!(!manifest.capabilities.semantic_pass);
    assert!(!manifest.capabilities.semantic_sweep);
    assert!(!manifest.capabilities.files_created);
    assert_eq!(manifest.capabilities.receiver_calls, ReceiverCallResolution::Unresolved);
    assert_eq!(manifest.capabilities.receiver_calls_structural, ReceiverCallResolution::Unresolved);
    assert_eq!(manifest.workspace, WorkspaceConfig::default());
    assert!(manifest.workspace.watch_files.is_empty());
    assert!(manifest.workspace.exclude_dirs.is_empty());
    assert!(manifest.workspace.entry_points.is_empty());
}

/// The positive case: every `[plugin.capabilities]` and
/// `[plugin.workspace]` field set to a non-default value parses into
/// the expected typed struct - including a genuine glob (`*.csproj`,
/// straight from the architecture doc's paper-stress-test example)
/// alongside an exact file name (`go.mod`), proving both live in
/// `watch_files` the same way (see this module's doc comment).
#[test]
fn parses_capabilities_and_workspace_from_a_well_formed_manifest() {
    let body = format!(
        r#"
[plugin]
language = "go"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "./g-mesh-plugin-go"

[plugin.languages]
extensions = [".go"]

[plugin.capabilities]
semantic_pass = true
semantic_sweep = true
files_created = true
receiver_calls = "resolved"
receiver_calls_structural = "unresolved"

[plugin.workspace]
watch_files = ["go.mod", "go.work", "*.csproj"]
exclude_dirs = ["vendor", "testdata"]
entry_points = ["lib.rs", "main.rs", "mod.rs"]
"#,
        version = CURRENT_PROTOCOL_VERSION,
    );
    let (_root, dir) = plugin_dir("go", &body);

    let manifest = read_manifest(&dir).unwrap();

    assert!(manifest.capabilities.semantic_pass);
    assert!(manifest.capabilities.semantic_sweep);
    assert!(manifest.capabilities.files_created);
    assert_eq!(manifest.capabilities.receiver_calls, ReceiverCallResolution::Resolved);
    assert_eq!(manifest.capabilities.receiver_calls_structural, ReceiverCallResolution::Unresolved);

    let watch_file_patterns: Vec<&str> = manifest.workspace.watch_files.iter().map(Glob::glob).collect();
    assert_eq!(watch_file_patterns, vec!["go.mod", "go.work", "*.csproj"]);
    // An exact name matches only itself; a glob matches the shape the
    // paper stress test needed it for (C#'s project files, which have
    // no fixed name) - both through the same `Glob::compile_matcher`,
    // proving `watch_files` needs no separate "exact name" code path.
    let go_mod = manifest.workspace.watch_files[0].compile_matcher();
    assert!(go_mod.is_match("go.mod"));
    assert!(!go_mod.is_match("other.mod"));
    let csproj = manifest.workspace.watch_files[2].compile_matcher();
    assert!(csproj.is_match("MyProject.csproj"));
    assert!(!csproj.is_match("MyProject.sln"));

    assert_eq!(manifest.workspace.exclude_dirs, vec!["vendor".to_string(), "testdata".to_string()]);
    assert_eq!(
        manifest.workspace.entry_points,
        vec!["lib.rs".to_string(), "main.rs".to_string(), "mod.rs".to_string()]
    );
}

/// A `[plugin.capabilities]` table that sets only one field still fills
/// the rest from [`Capabilities::default`] rather than erroring on the
/// missing ones or leaving them at some other implicit value - the
/// container-level `#[serde(default)]` on [`Capabilities`] is what makes
/// this work, and it is exactly the behavior a plugin author relies on
/// when they only have something to say about one field.
#[test]
fn a_partially_specified_capabilities_table_defaults_the_rest() {
    let body = format!(
        r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]

[plugin.capabilities]
semantic_pass = true
"#,
        version = CURRENT_PROTOCOL_VERSION,
    );
    let (_root, dir) = plugin_dir("python", &body);

    let manifest = read_manifest(&dir).unwrap();

    assert!(manifest.capabilities.semantic_pass);
    // A plugin that runs a semantic pass is not swept unless it says so.
    assert!(!manifest.capabilities.semantic_sweep);
    assert!(!manifest.capabilities.files_created);
    assert_eq!(manifest.capabilities.receiver_calls, ReceiverCallResolution::Unresolved);
    assert_eq!(manifest.capabilities.receiver_calls_structural, ReceiverCallResolution::Unresolved);
}

/// This task's other acceptance criterion: an unrecognized
/// `receiver_calls` value is a hard failure naming the manifest path.
/// Caught by TOML parsing itself (see [`ReceiverCallResolution`]'s
/// `Deserialize` impl), so this shares its assertion shape with
/// [`rejects_malformed_toml_naming_the_manifest_path`] rather than with
/// the hand-written `bail!` checks below it - both are "TOML parsing
/// failed", just for a different reason.
#[test]
fn rejects_an_invalid_receiver_calls_value_naming_the_manifest_path() {
    let body = format!(
        r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]

[plugin.capabilities]
receiver_calls = "maybe"
"#,
        version = CURRENT_PROTOCOL_VERSION,
    );
    let (_root, dir) = plugin_dir("python", &body);

    let err = read_manifest(&dir).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
    assert!(message.contains("maybe"), "{message}");
}

/// This task's third acceptance criterion: a `[plugin.workspace]
/// watch_files` entry that is not a valid glob is a hard failure naming
/// the manifest path. Unlike the `receiver_calls` case above, TOML
/// parsing cannot catch this by itself - any string is syntactically
/// valid TOML - so [`read_manifest`]'s own glob-compilation step is what
/// has to reject it, matching this module's `bail!`/`.with_context`
/// error style rather than a `Deserialize` error.
#[test]
fn rejects_an_invalid_glob_in_watch_files_naming_the_manifest_path() {
    let body = format!(
        r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]

[plugin.workspace]
watch_files = ["[unclosed"]
"#,
        version = CURRENT_PROTOCOL_VERSION,
    );
    let (_root, dir) = plugin_dir("python", &body);

    let err = read_manifest(&dir).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
    assert!(message.contains("[unclosed"), "{message}");
}

#[test]
fn a_bare_command_with_no_path_separator_is_left_for_path_lookup() {
    assert_eq!(resolve_path_entry("node", Path::new("/plugins/python")), PathBuf::from("node"));
}

#[test]
fn a_command_with_a_path_separator_is_resolved_against_the_manifest_directory() {
    assert_eq!(
        resolve_path_entry("./g-mesh-plugin-python", Path::new("/plugins/python")),
        Path::new("/plugins/python").join("./g-mesh-plugin-python"),
    );
}

#[test]
fn an_arg_with_no_path_separator_is_left_untouched() {
    assert_eq!(resolve_arg("--verbose", Path::new("/plugins/python")), "--verbose");
}

// -----------------------------------------------------------------
// GM-404: `${G_MESH_BIN_DIR}` follows the running executable's profile
// -----------------------------------------------------------------

#[test]
fn the_bin_dir_placeholder_expands_to_the_given_directory_not_the_manifest_directory() {
    let bin_dir = Path::new("/ws/target/release");
    let resolved =
        resolve_command("${G_MESH_BIN_DIR}/g-mesh-plugin-rust", Path::new("/ws/plugins/rust"), Some(bin_dir))
            .unwrap();
    assert_eq!(resolved, bin_dir.join("g-mesh-plugin-rust"));
}

#[test]
fn the_bin_dir_placeholder_without_a_known_bin_dir_is_an_error() {
    let err = resolve_command("${G_MESH_BIN_DIR}/g-mesh-plugin-rust", Path::new("/p"), None).unwrap_err();
    assert!(err.to_string().contains("running executable's directory is unknown"), "{err:#}");
}

#[test]
fn the_bin_dir_placeholder_anywhere_but_the_start_is_an_error() {
    let bin_dir = Some(Path::new("/ws/target/debug"));
    for value in ["./x/${G_MESH_BIN_DIR}/p", "${G_MESH_BIN_DIR}", "${G_MESH_BIN_DIR}/"] {
        assert!(resolve_command(value, Path::new("/p"), bin_dir).is_err(), "{value} must be rejected");
    }
}

#[test]
fn bin_dir_of_steps_over_a_cargo_test_deps_directory() {
    let debug = Path::new("/ws").join("target").join("debug");
    assert_eq!(bin_dir_of(&debug.join("deps").join("g_mesh-abc123")), Some(debug.clone()));
    assert_eq!(bin_dir_of(&debug.join("g-mesh")), Some(debug));
}

/// Both checked-in cargo-workspace plugin manifests, read in place, must
/// resolve their `command` inside the running executable's own profile
/// directory - the directory this test binary itself was built into -
/// rather than a path spelled relative to `plugins/<language>/`.
#[test]
fn the_bundled_cargo_plugins_resolve_their_command_in_the_running_profile_directory() {
    let bin_dir = current_bin_dir().expect("the test binary must know its own directory");
    for language in ["rust", "python"] {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins").join(language);
        let manifest = read_manifest(&dir).unwrap();
        assert_eq!(
            manifest.command.parent(),
            Some(bin_dir.as_path()),
            "{language}: resolved {}",
            manifest.command.display()
        );
    }
}

// -----------------------------------------------------------------
// GM-335: Windows `.exe` suffix fallback
// -----------------------------------------------------------------
//
// `exe_suffixed` and `resolve_exe_suffix` both take the platform suffix
// as a parameter rather than reading `std::env::consts::EXE_SUFFIX`
// internally, specifically so the Windows arm (`".exe"`) can be
// exercised from any host - these tests pass it explicitly rather than
// `cfg!(windows)`-gating anything away on macOS/Linux.

#[test]
fn exe_suffixed_appends_the_suffix_to_an_extensionless_path() {
    assert_eq!(
        exe_suffixed(Path::new("/plugins/python/g-mesh-plugin-python"), ".exe"),
        Some(PathBuf::from("/plugins/python/g-mesh-plugin-python.exe")),
    );
}

#[test]
fn exe_suffixed_is_none_for_an_empty_suffix() {
    // The non-Windows case: `std::env::consts::EXE_SUFFIX` is `""` there,
    // and this must be a no-op.
    assert_eq!(exe_suffixed(Path::new("/plugins/python/g-mesh-plugin-python"), ""), None);
}

#[test]
fn exe_suffixed_is_none_for_a_path_that_already_has_an_extension() {
    assert_eq!(exe_suffixed(Path::new("/plugins/example/index.js"), ".exe"), None);
}

/// The fix under test: a cargo-workspace plugin's binary, present only
/// under its Windows spelling, must still resolve. This is the test that
/// catches GM-335 being reintroduced - it fails if `resolve_exe_suffix`
/// is reverted to returning `resolved` unconditionally (verified below by
/// disabling the fix and re-running).
#[test]
fn resolve_exe_suffix_falls_back_to_the_suffixed_spelling_when_only_it_exists() {
    let dir = tempfile::tempdir().unwrap();
    let unsuffixed = dir.path().join("g-mesh-plugin-python");
    let suffixed = dir.path().join("g-mesh-plugin-python.exe");
    fs::write(&suffixed, b"").unwrap();
    // Deliberately not creating `unsuffixed` - this is exactly the shape
    // `cargo build --workspace` leaves on Windows: only the `.exe` exists.

    assert_eq!(resolve_exe_suffix(unsuffixed, ".exe"), suffixed);
}

#[test]
fn resolve_exe_suffix_prefers_the_unsuffixed_spelling_when_it_exists() {
    let dir = tempfile::tempdir().unwrap();
    let unsuffixed = dir.path().join("g-mesh-plugin-go");
    fs::write(&unsuffixed, b"").unwrap();
    // A `.exe` sibling existing too would be surprising for this plugin,
    // but even so the unsuffixed spelling that's actually there wins -
    // this is the Go plugin's real shape (its own build step never
    // produces a suffixed binary at all).

    assert_eq!(resolve_exe_suffix(unsuffixed.clone(), ".exe"), unsuffixed);
}

#[test]
fn resolve_exe_suffix_leaves_a_genuinely_missing_path_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let unsuffixed = dir.path().join("g-mesh-plugin-python");
    // Neither spelling exists - the "never built" case.

    assert_eq!(resolve_exe_suffix(unsuffixed.clone(), ".exe"), unsuffixed);
}

#[test]
fn resolve_exe_suffix_is_a_no_op_on_a_platform_with_no_exe_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let unsuffixed = dir.path().join("g-mesh-plugin-python");
    fs::write(dir.path().join("g-mesh-plugin-python.exe"), b"").unwrap();

    // Even with a `.exe` sibling sitting right there, an empty suffix
    // (what `std::env::consts::EXE_SUFFIX` actually is on macOS/Linux)
    // must never switch to it.
    assert_eq!(resolve_exe_suffix(unsuffixed.clone(), ""), unsuffixed);
}

// -----------------------------------------------------------------
// GM-337: Windows' extended-length (`\\?\`) spelling
// -----------------------------------------------------------------
//
// `plain_win32_path` takes a `&str` and is not `cfg`-gated, for the same
// reason `exe_suffixed` takes its suffix as an argument: the Windows arm
// is the only arm that does anything, and it has to be exercisable from
// a host that cannot produce such a path in the first place. The inputs
// below are the two real ones from CI run 35451298477 - the fake
// plugin's temp directory and the bundled TS plugin's checkout
// directory, both as `fs::canonicalize` spelled them there.

#[test]
fn plain_win32_path_rewrites_a_canonicalized_disk_path() {
    assert_eq!(
        plain_win32_path(r"\\?\D:\a\g-mesh\g-mesh\plugins\typescript").as_deref(),
        Some(r"D:\a\g-mesh\g-mesh\plugins\typescript"),
    );
    assert_eq!(
        plain_win32_path(r"\\?\C:\Users\runneradmin\AppData\Local\Temp\.tmpPBFGol\fake").as_deref(),
        Some(r"C:\Users\runneradmin\AppData\Local\Temp\.tmpPBFGol\fake"),
    );
}

#[test]
fn plain_win32_path_rewrites_a_canonicalized_unc_path() {
    assert_eq!(
        plain_win32_path(r"\\?\UNC\build\share\plugins\go").as_deref(),
        Some(r"\\build\share\plugins\go")
    );
}

#[test]
fn plain_win32_path_leaves_a_path_with_no_ordinary_spelling_alone() {
    // A device namespace name is not a drive and not a UNC share, so
    // there is nothing to rewrite it *to* - dropping the prefix would
    // name something else entirely.
    assert_eq!(plain_win32_path(r"\\?\pipe\g-mesh"), None);
    assert_eq!(plain_win32_path(r"\\?\Volume{9f3a}\plugins\go"), None);
}

#[test]
fn plain_win32_path_leaves_an_ordinary_path_alone() {
    // Including every path any non-Windows host can produce: this
    // function runs unconditionally, so a Unix canonicalization has to
    // fall straight through it.
    assert_eq!(plain_win32_path(r"D:\a\g-mesh\plugins\go"), None);
    assert_eq!(plain_win32_path("/private/var/folders/t7/plugins/go"), None);
}

#[test]
fn plain_win32_path_keeps_the_prefix_on_a_path_too_long_to_spell_without_it() {
    // The prefix is the *only* way to write a path this long, so
    // rewriting it would turn a working path into an unopenable one.
    let long = format!(r"\\?\C:\{}", "d".repeat(300));
    assert_eq!(plain_win32_path(&long), None);

    // ...and the boundary is real: 259 characters is the longest
    // ordinary path Windows accepts (`MAX_PATH` counts the NUL).
    let at_limit = format!(r"\\?\C:\{}", "d".repeat(259 - 3));
    assert_eq!(plain_win32_path(&at_limit).map(|p| p.chars().count()), Some(259));
    let over_limit = format!(r"\\?\C:\{}", "d".repeat(259 - 2));
    assert_eq!(plain_win32_path(&over_limit), None);
}

/// The fix under test, at the granularity the kit uses it:
/// `plugin_check::check` hands `plain_spelling` the result of
/// `fs::canonicalize`, and everything a plugin is invoked with is joined
/// onto whatever comes back. Fails if `plain_win32_path` is reverted to
/// returning `None` unconditionally (verified by doing exactly that).
#[test]
fn plain_spelling_is_what_a_manifest_directory_is_joined_onto() {
    assert_eq!(
        plain_spelling(PathBuf::from(r"\\?\C:\Users\runneradmin\AppData\Local\Temp\.tmpPBFGol\fake")),
        PathBuf::from(r"C:\Users\runneradmin\AppData\Local\Temp\.tmpPBFGol\fake"),
    );
}

#[test]
fn plain_spelling_leaves_a_path_it_cannot_rewrite_exactly_as_it_was() {
    let unix = PathBuf::from("/private/var/folders/t7/plugin_check/fake");
    assert_eq!(plain_spelling(unix.clone()), unix);
}

/// End-to-end through `read_manifest`: a `plugin.toml` shaped exactly
/// like `plugins/python/plugin.toml` (a relative `command` into
/// `../../target/debug/...`), with only the `.exe` spelling present on
/// disk, must resolve `command` to that `.exe` path - not fail, and not
/// silently keep the unsuffixed spelling that `Command::spawn` could
/// never find. This one exercises `resolve_path_entry` itself, which
/// always uses the real [`std::env::consts::EXE_SUFFIX`], so what it
/// proves differs by host: the fallback on Windows, and that the fallback
/// stays a no-op everywhere else. It runs on every platform for exactly
/// that reason - an earlier version returned early when the constant was
/// empty, so it had never executed anywhere but Windows CI by the time it
/// got there, and it arrived broken (GM-335).
///
/// The assertion is on the contract rather than on a spelling: that the
/// resolved command is a file that exists, under the platform's own name
/// for it. Comparing `PathBuf`s literally would fail on Windows for a
/// reason that has nothing to do with the fix - `dir.join("../a/b")`
/// keeps the forward slashes the manifest wrote, while `with_file_name`
/// rebuilds the last component with a backslash, so two paths naming the
/// same file compare unequal.
#[test]
fn a_manifest_command_resolves_to_the_exe_suffixed_binary_when_only_it_exists() {
    let body = format!(
        r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "../target/debug/g-mesh-plugin-python"

[plugin.languages]
extensions = [".py"]
"#,
        version = CURRENT_PROTOCOL_VERSION,
    );
    // One `..`, not two: `plugin_dir` puts the manifest at `<root>/python`,
    // one level under the temp root, where the real tree has it two
    // (`plugins/python`). The count has to match the fixture it is
    // resolved against, or the path lands outside the tempdir entirely -
    // which is the other half of how this test arrived broken.
    let (root, dir) = plugin_dir("python", &body);
    let target_debug = root.path().join("target").join("debug");
    fs::create_dir_all(&target_debug).unwrap();
    let built = target_debug.join(format!("g-mesh-plugin-python{}", std::env::consts::EXE_SUFFIX));
    fs::write(&built, b"").unwrap();

    let manifest = read_manifest(&dir).unwrap();
    assert!(
        manifest.command.is_file(),
        "the resolved command must name a file that exists; got {}",
        manifest.command.display()
    );
    assert_eq!(
        manifest.command.file_name(),
        built.file_name(),
        "the resolved command must carry this platform's executable spelling; got {}",
        manifest.command.display()
    );
}

/// The bundled JS/TS plugin's own `plugin.toml` (`plugins/typescript/plugin.toml`,
/// embedded at compile time so this test tracks the committed file, not a
/// copy) must itself satisfy `read_manifest`. Its directory on disk used
/// to be named `js-ts`, not `typescript` - a mismatch this module's own
/// (now resolved) header used to flag - so this still copies the exact
/// file contents into a tempdir rather than trusting any particular path,
/// which is what lets it confirm the content itself is well-formed and
/// matches the plugin's real handshake/version values independent of
/// where the repo happens to keep the file.
#[test]
fn the_bundled_js_ts_plugin_manifest_parses_once_directory_named_correctly() {
    const BUNDLED_JS_TS_MANIFEST: &str = include_str!("../../../../plugins/typescript/plugin.toml");

    let (_root, dir) = plugin_dir("typescript", BUNDLED_JS_TS_MANIFEST);

    let manifest = read_manifest(&dir).unwrap();

    assert_eq!(manifest.language, "typescript");
    assert_eq!(manifest.protocol_version, CURRENT_PROTOCOL_VERSION);
    let bin_dir = current_bin_dir().expect("the test binary must know its own directory");
    assert_eq!(manifest.command.parent(), Some(bin_dir.as_path()), "resolved {}", manifest.command.display());
    assert!(
        manifest
            .command
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("g-mesh-plugin-typescript")),
        "resolved {}",
        manifest.command.display()
    );
    assert!(manifest.args.is_empty(), "{:?}", manifest.args);
    assert!(manifest.extensions.contains(&".ts".to_string()));
    assert!(manifest.extensions.contains(&".tsx".to_string()));
    assert!(manifest.extensions.contains(&".js".to_string()));

    // This task's acceptance criterion for the bundled manifest: it
    // carries the capabilities, not just the fields this test already
    // checked before this task.
    // A semantic tier (vtsls) that resolves receiver calls, over a
    // structural tier that does not.
    assert!(manifest.capabilities.semantic_pass);
    assert!(!manifest.capabilities.semantic_sweep);
    assert!(manifest.capabilities.files_created);
    assert_eq!(manifest.capabilities.receiver_calls, ReceiverCallResolution::Resolved);
    assert_eq!(manifest.capabilities.receiver_calls_structural, ReceiverCallResolution::Unresolved);
    assert_eq!(manifest.workspace.entry_points, vec!["index".to_string()]);
}

/// Task 155's actual acceptance criterion for the rename: discovery must
/// find the bundled plugin at its *real* on-disk location, not just at a
/// copy under a conveniently-named tempdir - `default_roots()`'s bundled
/// entry (`CARGO_MANIFEST_DIR/../plugins`) really does contain a
/// `typescript/plugin.toml` today, where before task 155 it contained
/// `js-ts/plugin.toml` and could not satisfy `read_manifest`'s
/// language-equals-directory-name rule at all.
#[test]
fn the_real_bundled_plugin_directory_satisfies_read_manifest_directly() {
    let bundled_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");

    let manifest = read_manifest(&bundled_root.join("typescript"))
        .expect("the real bundled plugin directory must satisfy read_manifest");

    assert_eq!(manifest.language, "typescript");
}

/// [`semantic_pass_capable_languages`]'s own acceptance criterion: only
/// the manifests that actually declared `capabilities.semantic_pass =
/// true` come back, sorted, and a manifest that said nothing (the
/// conservative default - see [`Capabilities::default`]) is excluded
/// exactly like one that said `false` explicitly.
#[test]
fn semantic_pass_capable_languages_returns_only_capable_manifests_sorted() {
    let capable = |language: &str| PluginManifest {
        language: language.to_string(),
        protocol_version: CURRENT_PROTOCOL_VERSION,
        plugin_version: "0.0.0".to_string(),
        command: PathBuf::from("true"),
        args: Vec::new(),
        extensions: Vec::new(),
        fingerprint_ignore: Vec::new(),
        manifest_dir: PathBuf::from("/dev/null"),
        capabilities: Capabilities { semantic_pass: true, ..Capabilities::default() },
        workspace: WorkspaceConfig::default(),
        non_symbol_queries: Default::default(),
        symbol_query_prefixes: Default::default(),
        reexports: Default::default(),
    };
    let not_capable =
        |language: &str| PluginManifest { capabilities: Capabilities::default(), ..capable(language) };

    let mut manifests = HashMap::new();
    manifests.insert("rust".to_string(), capable("rust"));
    manifests.insert("go".to_string(), not_capable("go"));
    manifests.insert("typescript".to_string(), capable("typescript"));

    assert_eq!(
        semantic_pass_capable_languages(&manifests),
        vec!["rust".to_string(), "typescript".to_string()],
        "go declared no semantic_pass capability and must be excluded; the rest sorted"
    );
}

fn with_non_symbol_queries(table: &str) -> String {
    format!("{}\n[plugin.non_symbol_queries]\n{table}\n", well_formed_toml())
}

/// Control: drop `non_symbol_queries: plugin.non_symbol_queries` from
/// `read_manifest`'s result (use `Default::default()`) - both fields come
/// back empty and this fails.
#[test]
fn parses_non_symbol_queries() {
    let (_root, dir) =
        plugin_dir("python", &with_non_symbol_queries("starts_with = [\"@\", \".\"]\ncontains = [\"/\"]"));

    let manifest = read_manifest(&dir).unwrap();

    assert_eq!(manifest.non_symbol_queries.starts_with, vec!["@".to_string(), ".".to_string()]);
    assert_eq!(manifest.non_symbol_queries.contains, vec!["/".to_string()]);
    assert!(manifest.non_symbol_queries.matches("@scope/pkg"));
    assert!(manifest.non_symbol_queries.matches(".models"));
    assert!(manifest.non_symbol_queries.matches("a/b"));
    assert!(!manifest.non_symbol_queries.matches("models"));
}

/// Control: remove `#[serde(default)]` from `RawPlugin::non_symbol_queries`
/// - a manifest without the table no longer parses.
#[test]
fn a_manifest_with_no_non_symbol_queries_table_refuses_nothing() {
    let (_root, dir) = plugin_dir("python", &well_formed_toml());

    let manifest = read_manifest(&dir).unwrap();

    assert_eq!(manifest.non_symbol_queries, NonSymbolShapes::default());
    assert!(!manifest.non_symbol_queries.matches("@scope/pkg"));
}

/// An empty string is a prefix and an infix of every query.
///
/// Control: remove the `validate_non_symbol_queries` call from
/// `read_manifest` - both manifests load and this fails.
#[test]
fn rejects_an_empty_string_in_non_symbol_queries_naming_the_manifest_path_and_key() {
    for (table, key) in [("starts_with = [\"@\", \"\"]", "starts_with"), ("contains = [\"\"]", "contains")] {
        let (_root, dir) = plugin_dir("python", &with_non_symbol_queries(table));

        let message = format!("{:#}", read_manifest(&dir).unwrap_err());

        assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
        assert!(message.contains("[plugin.non_symbol_queries]") && message.contains(key), "{message}");
    }
}

/// A misspelt key would otherwise be silently ignored and refuse nothing.
///
/// Control: remove `#[serde(deny_unknown_fields)]` from `NonSymbolShapes` -
/// the manifest loads and this fails.
#[test]
fn rejects_an_unknown_key_in_non_symbol_queries() {
    let (_root, dir) = plugin_dir("python", &with_non_symbol_queries("start_with = [\"@\"]"));

    let message = format!("{:#}", read_manifest(&dir).unwrap_err());

    assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
    assert!(message.contains("start_with"), "{message}");
}

fn with_symbol_query_prefixes(starts_with: &str, table: &str) -> String {
    format!(
        "{}\n[plugin.non_symbol_queries]\nstarts_with = {starts_with}\n\n[plugin.symbol_query_prefixes]\n{table}\n",
        well_formed_toml()
    )
}

/// T1: the table parses, and a manifest without it strips nothing.
///
/// Control: build `read_manifest`'s result with
/// `symbol_query_prefixes: Default::default()` - the first assertion fails.
#[test]
fn parses_symbol_query_prefixes_and_defaults_to_none() {
    let (_root, dir) = plugin_dir("python", &with_symbol_query_prefixes("[\"@\", \".\"]", "strip = [\"@\"]"));
    assert_eq!(read_manifest(&dir).unwrap().symbol_query_prefixes.strip, vec!["@".to_string()]);

    let (_root, dir) = plugin_dir("python", &well_formed_toml());
    assert_eq!(read_manifest(&dir).unwrap().symbol_query_prefixes, SymbolQueryPrefixes::default());
}

/// T2: an empty prefix would rewrite every query to itself.
///
/// Control: remove the `prefix.is_empty()` check from
/// `validate_symbol_query_prefixes` - the error is then the `starts_with`
/// one, which says nothing about an empty string, and this fails.
#[test]
fn rejects_an_empty_strip_prefix() {
    let (_root, dir) = plugin_dir("python", &with_symbol_query_prefixes("[\"@\"]", "strip = [\"\"]"));

    let message = format!("{:#}", read_manifest(&dir).unwrap_err());

    assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
    assert!(
        message.contains("[plugin.symbol_query_prefixes]") && message.contains("empty string"),
        "{message}"
    );
}

/// T3: a misspelt key would otherwise be silently ignored and strip nothing.
///
/// Control: remove `#[serde(deny_unknown_fields)]` from
/// `SymbolQueryPrefixes` - the manifest loads and this fails.
#[test]
fn rejects_an_unknown_key_in_symbol_query_prefixes() {
    let (_root, dir) = plugin_dir("python", &with_symbol_query_prefixes("[\"@\"]", "strips = [\"@\"]"));

    let message = format!("{:#}", read_manifest(&dir).unwrap_err());

    assert!(message.contains("strips"), "{message}");
}

/// T4: a strip prefix the language does not also refuse as typed is
/// rejected, naming the prefix and both tables.
///
/// Control: remove the `starts_with.contains(prefix)` check from
/// `validate_symbol_query_prefixes` - the manifest loads and this fails.
#[test]
fn rejects_a_strip_prefix_missing_from_starts_with() {
    let (_root, dir) = plugin_dir("python", &with_symbol_query_prefixes("[\".\"]", "strip = [\"@\"]"));

    let message = format!("{:#}", read_manifest(&dir).unwrap_err());

    assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
    assert!(message.contains("\"@\""), "{message}");
    assert!(message.contains("[plugin.symbol_query_prefixes]"), "{message}");
    assert!(message.contains("[plugin.non_symbol_queries] starts_with"), "{message}");
}

// -----------------------------------------------------------------
// `[plugin.reexports] named_shadows_glob` and `later_import_binds`
// -----------------------------------------------------------------

fn with_reexports(table: &str) -> String {
    format!("{}\n[plugin.reexports]\n{table}\n", well_formed_toml())
}

/// Absent means no shadowing; `true` parses; `link_rules` collects exactly
/// the languages that declare it.
///
/// Controls: build `read_manifest`'s result with `reexports:
/// Default::default()` - the `true` manifest reads false; remove
/// `#[serde(default)]` from `RawPlugin::reexports` - the manifest without the
/// table no longer loads; drop the `filter` from `link_rules` - `python`
/// (declaring nothing) gets the rule.
#[test]
fn parses_reexports_and_defaults_to_no_shadowing() {
    let (_absent_root, absent) = plugin_dir("python", &well_formed_toml());
    let absent = read_manifest(&absent).unwrap();
    assert_eq!(absent.reexports, ReexportRules::default());
    assert!(!absent.reexports.named_shadows_glob);

    let (_false_root, explicit_false) = plugin_dir("python", &with_reexports("named_shadows_glob = false"));
    assert!(!read_manifest(&explicit_false).unwrap().reexports.named_shadows_glob);

    let (_true_root, declared) = plugin_dir("python", &with_reexports("named_shadows_glob = true"));
    let declared = read_manifest(&declared).unwrap();
    assert!(declared.reexports.named_shadows_glob);

    let renamed = |language: &str, manifest: &PluginManifest| PluginManifest {
        language: language.to_string(),
        ..manifest.clone()
    };
    let rules = link_rules([&renamed("rust", &declared), &renamed("python", &absent)]);
    assert_eq!(rules, LinkRules::with_named_shadows_glob(["rust"]));
    assert!(rules.named_shadows_glob("rust"));
    assert!(!rules.named_shadows_glob("python"));
}

/// A misspelt key would otherwise be silently ignored and shadow nothing.
///
/// Control: remove `#[serde(deny_unknown_fields)]` from `ReexportRules` - the
/// manifest loads and this fails.
#[test]
fn rejects_an_unknown_key_in_reexports() {
    let (_root, dir) = plugin_dir("python", &with_reexports("named_shadow_glob = true"));

    let message = format!("{:#}", read_manifest(&dir).unwrap_err());

    assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
    assert!(message.contains("named_shadow_glob"), "{message}");
}

/// `later_import_binds`: absent or `false` is no rule, `true` parses, and
/// `link_rules` reports it for exactly the language that declares it, beside
/// another language's `named_shadows_glob`.
///
/// Controls: drop the `.with_later_import_binds(..)` call from `link_rules` -
/// `python` reads false; drop its `filter` - `rust` (declaring only
/// `named_shadows_glob`) gets the rule too.
#[test]
fn parses_later_import_binds_and_link_rules_reports_it_for_its_language_only() {
    let (_absent_root, absent) = plugin_dir("python", &well_formed_toml());
    assert!(!read_manifest(&absent).unwrap().reexports.later_import_binds);

    let (_false_root, explicit_false) = plugin_dir("python", &with_reexports("later_import_binds = false"));
    assert!(!read_manifest(&explicit_false).unwrap().reexports.later_import_binds);

    let (_later_root, later) = plugin_dir("python", &with_reexports("later_import_binds = true"));
    let later = read_manifest(&later).unwrap();
    assert!(later.reexports.later_import_binds);
    assert!(!later.reexports.named_shadows_glob);

    let (_shadow_root, shadowing) = plugin_dir("python", &with_reexports("named_shadows_glob = true"));
    let shadowing = read_manifest(&shadowing).unwrap();
    let renamed = |language: &str, manifest: &PluginManifest| PluginManifest {
        language: language.to_string(),
        ..manifest.clone()
    };
    let rules = link_rules([&renamed("rust", &shadowing), &later]);
    assert_eq!(rules, LinkRules::with_named_shadows_glob(["rust"]).with_later_import_binds(["python"]));
    assert!(rules.later_import_binds("python"));
    assert!(!rules.later_import_binds("rust"));
    assert!(!rules.named_shadows_glob("python"));
}

/// The two keys are opposite answers to "which import binds a name", so a
/// manifest setting both is refused, naming the manifest; either one alone
/// loads.
///
/// Control: remove the both-keys `bail!` from `read_manifest` - the manifest
/// loads and this fails.
#[test]
fn rejects_a_manifest_setting_both_reexport_rules() {
    let (_root, dir) =
        plugin_dir("python", &with_reexports("named_shadows_glob = true\nlater_import_binds = true"));

    let message = format!("{:#}", read_manifest(&dir).unwrap_err());

    assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
    assert!(message.contains("named_shadows_glob"), "{message}");
    assert!(message.contains("later_import_binds"), "{message}");
}

/// The checked-in manifests declare `named_shadows_glob` for Rust and
/// TypeScript (ADR 0020) and `later_import_binds` for Python, and
/// neither for Go, so the rules the daemon builds from them are exactly
/// those.
///
/// Control: remove `[plugin.reexports]` from `plugins/rust/plugin.toml` or
/// from `plugins/python/plugin.toml` (or add `named_shadows_glob = true` to
/// Python's, which `read_manifest` then refuses) - this fails.
#[test]
fn the_checked_in_manifests_declare_each_languages_reexport_rule() {
    let plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    let manifests: Vec<PluginManifest> = ["rust", "typescript", "python", "go"]
        .iter()
        .map(|language| read_manifest(&plugins.join(language)).unwrap())
        .collect();
    for manifest in &manifests {
        let language = manifest.language.as_str();
        let shadows = matches!(language, "rust" | "typescript");
        assert_eq!(manifest.reexports.named_shadows_glob, shadows, "{language}");
        assert_eq!(manifest.reexports.later_import_binds, language == "python", "{language}");
    }
    assert_eq!(
        link_rules(&manifests),
        LinkRules::with_named_shadows_glob(["rust", "typescript"]).with_later_import_binds(["python"])
    );
}
