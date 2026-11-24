//! A language whose plugin fails during the real bulk walk, rendered into
//! the MCP instructions' failed item.

use rusqlite::Connection;

use crate::daemon::{bulk_index, plugin};
use crate::mcp::instructions_test_support::{
    build_instructions as build, cause_of, failed, failed_item, indexed, real_plugins,
    rendered_with_rust_failed, warm_real, MAX_CAUSE_BYTES,
};
use crate::plugin_manifest::discover;
use crate::storage::index_store::IndexStore;
use crate::storage::language_outcome::LanguageOutcome;
use crate::storage::schema;

fn empty_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn
}

/// The real failure: a plugin laid out as an installed one
/// (`command = "./g-mesh-plugin-rust"`) whose binary is gone, under a deep
/// root, walked by the real `bulk_index::run`. The stored error is the whole
/// chain, the path in an outer cause; the rendered item shows only the OS
/// cause, read from `std::io::Error` rather than written here, and no path.
///
/// Control: make `error_cause` take the first non-empty line instead of the
/// last (the item becomes the outermost context and the exact-item
/// assertion fails).
#[test]
fn a_real_missing_plugin_binary_renders_the_os_cause_without_the_path() {
    use crate::protocol::types::CURRENT_PROTOCOL_VERSION;

    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let plugins =
        home.path().join("a-plugin-root-deep-enough-that-its-path-alone-fills-the-hundred-byte-budget");
    let rust = plugins.join("rust");
    std::fs::create_dir_all(&rust).unwrap();
    std::fs::write(
        rust.join("plugin.toml"),
        format!(
            "[plugin]\nlanguage = \"rust\"\nprotocol_version = {CURRENT_PROTOCOL_VERSION}\n\
             plugin_version = \"0.0.0-test\"\n\n[plugin.spawn]\ncommand = \"./g-mesh-plugin-rust\"\n\n\
             [plugin.languages]\nextensions = [\".rs\"]\n"
        ),
    )
    .unwrap();
    crate::test_plugin::install(&plugins, "alpha", &[".alpha-src"]);
    let discovered = discover(std::slice::from_ref(&plugins)).expect("the fixture plugins must discover");
    let store = IndexStore::new(empty_store());

    let summary = bulk_index::run(project.path(), &store, &discovered)
        .expect("one failed language must not fail the walk");

    let error = match summary.outcomes.get("rust") {
        Some(LanguageOutcome::Failed { error }) => error.clone(),
        other => panic!("rust must be Failed, got {other:?}"),
    };
    // ENOENT on Unix and ERROR_FILE_NOT_FOUND on Windows are both 2.
    let os_message = std::io::Error::from_raw_os_error(2).to_string();
    assert!(error.lines().count() >= 2, "a chain, one cause per line: {error}");
    assert!(error.contains(&rust.display().to_string()), "an outer cause names the path: {error}");
    assert_eq!(error.lines().last(), Some(os_message.as_str()), "the innermost cause is the OS one: {error}");

    let rendered = rendered_with_rust_failed(&error);

    assert_eq!(failed_item(&rendered, "rust"), format!("rust ({os_message})"), "{rendered}");
    #[cfg(unix)]
    assert!(rendered.contains("rust (No such file or directory"), "{rendered}");
    assert!(!rendered.contains(&home.path().display().to_string()), "{rendered}");
    assert!(!failed_item(&rendered, "rust").contains('/'), "{rendered}");
}

/// An unbuilt cargo-workspace plugin binary
/// (`plugin::missing_workspace_binary_hint`: `command` under
/// `<root>/target/debug/`, `<root>/Cargo.toml` present, the binary absent),
/// walked by the real `bulk_index::run`. The stored error keeps the step and
/// the hint as two causes, the hint innermost, so the rendered failed item
/// shows the hint - what to build - and not the step.
///
/// Control: fold the hint into the step (`bail!("failed to spawn the {}
/// plugin's bulk index: {hint}")` in `walk_one_language_in`) - the stored error is one
/// line and the two-cause assertion fails; with that assertion removed, the
/// item reads "rust (failed to spawn the rust plugin's bulk index: the
/// plugin binary ...)" and the `starts_with`/"failed to spawn" assertions
/// fail. On Windows the item also needs `shorten_paths` to recognise a
/// drive-rooted path (pinned host-independently in
/// `mcp::instructions`' `shorten_paths_keeps_only_the_last_component_of_rooted_paths`).
#[test]
fn an_unbuilt_workspace_plugin_binary_renders_the_build_hint_not_the_step() {
    use crate::protocol::types::CURRENT_PROTOCOL_VERSION;

    let project = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
    let plugins = workspace.path().join("plugins");
    let binary = workspace.path().join("target").join("debug").join("g-mesh-plugin-rust");
    let rust = plugins.join("rust");
    std::fs::create_dir_all(&rust).unwrap();
    std::fs::write(
        rust.join("plugin.toml"),
        format!(
            "[plugin]\nlanguage = \"rust\"\nprotocol_version = {CURRENT_PROTOCOL_VERSION}\n\
             plugin_version = \"0.0.0-test\"\n\n[plugin.spawn]\ncommand = '{}'\n\n\
             [plugin.languages]\nextensions = [\".rs\"]\n",
            binary.display()
        ),
    )
    .unwrap();
    crate::test_plugin::install(&plugins, "alpha", &[".alpha-src"]);
    let discovered = discover(std::slice::from_ref(&plugins)).expect("the fixture plugins must discover");
    let store = IndexStore::new(empty_store());

    let summary = bulk_index::run(project.path(), &store, &discovered)
        .expect("one failed language must not fail the walk");

    let error = match summary.outcomes.get("rust") {
        Some(LanguageOutcome::Failed { error }) => error.clone(),
        other => panic!("rust must be Failed, got {other:?}"),
    };
    let hint = plugin::missing_workspace_binary_hint(&binary)
        .expect("the fixture must be the unbuilt-workspace-binary shape");
    assert!(hint.contains("cargo build --workspace"), "{hint}");
    assert_eq!(
        error.lines().collect::<Vec<_>>(),
        ["failed to spawn the rust plugin's bulk index", hint.as_str()],
        "the step and the hint are separate causes, the hint innermost: {error}"
    );

    let rendered = rendered_with_rust_failed(&error);
    let item = failed_item(&rendered, "rust");

    // The hint names the spelling cargo builds on this platform
    // (`g-mesh-plugin-rust.exe` on Windows).
    let named = crate::plugin_manifest::exe_suffixed(&binary, std::env::consts::EXE_SUFFIX)
        .unwrap_or_else(|| binary.clone());
    let name = named.file_name().unwrap().to_str().unwrap();
    assert!(item.starts_with("rust (Run `cargo build --workspace` in "), "{rendered}");
    // The cap may cut the tail after the binary's name, never the command.
    assert!(item.contains(&format!("the plugin binary {name}")), "{rendered}");
    assert!(!item.contains("failed to spawn"), "{rendered}");
    assert!(!rendered.contains(&workspace.path().display().to_string()), "{rendered}");

    // The Windows spelling of the same hint, on any host: the `.exe` name
    // still leaves the whole build command inside the cause's byte cap.
    let windows = plugin::missing_workspace_binary_hint_with_suffix(&binary, ".exe")
        .expect("the fixture must be the unbuilt-workspace-binary shape");
    let cause = cause_of(&windows);
    assert!(cause.starts_with("Run `cargo build --workspace` in "), "{cause}");
    assert!(cause.contains("the plugin binary g-mesh-plugin-rust.exe"), "{cause}");
}

/// The 100-byte cap on a failed language's cause never cuts the
/// build command out of the unbuilt-workspace-binary hint. Worst case on
/// any host: the longest bundled workspace plugin binary name, read from the
/// real manifests, in its Windows `.exe` spelling, under both profiles
/// (`--release` is the longer command) and both hint variants (a workspace
/// root with `Cargo.toml`, and none: "the repository root"). Every case is
/// long enough to be cut, so the assertions are about where the cut lands:
/// after the whole command.
///
/// Control: restore the old order in
/// `plugin::missing_workspace_binary_hint_with_suffix` (binary first, then
/// "Run `{build}` in ...") - the cut lands inside the command and the
/// `contains` assertion fails (already for the first case, debug with a
/// root: 71 bytes of path and wording leave no room for the command).
#[test]
fn the_cap_never_cuts_the_build_command_out_of_the_longest_windows_hint() {
    let found = real_plugins(&["go", "python", "rust", "typescript"]);
    // The workspace-built plugins: those whose command sits under a cargo
    // `target/<profile>/` directory, as `${G_MESH_BIN_DIR}` resolves.
    let (language, name) = found
        .manifests
        .values()
        .filter(|manifest| {
            let profile = manifest.command.parent().and_then(|dir| dir.file_name());
            profile.is_some_and(|profile| profile == "debug" || profile == "release")
        })
        .map(|manifest| {
            let name = manifest.command.file_name().unwrap().to_str().unwrap();
            let name = name.strip_suffix(std::env::consts::EXE_SUFFIX).unwrap_or(name);
            (manifest.language.clone(), name.to_string())
        })
        .max_by_key(|(_, name)| name.len())
        .expect("at least one bundled plugin is workspace-built");
    let others: Vec<&str> =
        ["go", "python", "rust", "typescript"].into_iter().filter(|other| *other != language).collect();

    for with_root in [true, false] {
        for (profile, cargo_build) in
            [("debug", "`cargo build --workspace`"), ("release", "`cargo build --workspace --release`")]
        {
            let workspace = tempfile::tempdir().unwrap();
            if with_root {
                std::fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").unwrap();
            }
            let binary = workspace.path().join("target").join(profile).join(&name);
            let hint = plugin::missing_workspace_binary_hint_with_suffix(&binary, ".exe")
                .expect("the fixture must be the unbuilt-workspace-binary shape");
            let error = format!("failed to spawn the {language} plugin's bulk index\n{hint}");

            let mut outcomes = vec![(language.as_str(), failed(&error))];
            outcomes.extend(others.iter().map(|other| (*other, indexed())));
            let rendered = build(&warm_real(&found, &others, outcomes));
            let item = failed_item(&rendered, &language);
            let cause = item
                .strip_prefix(&format!("{language} ("))
                .and_then(|rest| rest.strip_suffix(')'))
                .unwrap_or_else(|| panic!("malformed item: {item}"));

            let case = format!("{profile}, with_root={with_root}: {cause}");
            assert!(cause.ends_with("..."), "the case must reach the cap: {case}");
            assert!(cause.len() <= MAX_CAUSE_BYTES, "{case}");
            assert!(cause.starts_with(&format!("Run {cargo_build} in ")), "{case}");
            assert!(cause.contains(cargo_build), "the whole command survives the cap: {case}");
            if !with_root {
                assert!(cause.starts_with(&format!("Run {cargo_build} in the repository root: ")), "{case}");
            }
        }
    }
}
