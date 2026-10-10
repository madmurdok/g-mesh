use super::*;
use crate::daemon::manifest::{Capabilities, WorkspaceConfig};
use crate::protocol::types::CURRENT_PROTOCOL_VERSION;
use crate::storage::index_store::IndexStore;
use rusqlite::Connection;

/// GM-271 review round: the floor must win for a project too small for
/// the per-file budget to matter - see `RoundTripTimeouts`'s doc comment
/// ("Task-tracker-mcp's own numbers... show the floor is not
/// accidentally starving a small project"). 49 files (task-tracker-mcp's
/// own `File`-node count) * 10s/file = 490s, well under the 20-minute
/// (1200s) floor.
#[test]
fn semantic_pass_project_timeout_uses_the_floor_for_a_small_project() {
    let timeouts = RoundTripTimeouts::default();
    assert_eq!(timeouts.semantic_pass_project_timeout(49), timeouts.semantic_pass_project);
    assert_eq!(timeouts.semantic_pass_project_timeout(0), timeouts.semantic_pass_project);
}

/// The other half: a project large enough that the per-file budget
/// exceeds the floor must get the scaled value, not the flat one that
/// this task's review measured as too short for excalidraw's own 658
/// `File` nodes (a real pass there took as long as 1475s = 24.6min,
/// past the 20-minute floor).
#[test]
fn semantic_pass_project_timeout_scales_past_the_floor_for_a_large_project() {
    let timeouts = RoundTripTimeouts::default();
    let file_count = 658;
    let expected = SEMANTIC_PASS_PER_FILE_BUDGET * file_count;
    assert!(
        expected > timeouts.semantic_pass_project,
        "the fixture must actually exercise the scaled branch, not the floor"
    );
    assert_eq!(timeouts.semantic_pass_project_timeout(file_count as usize), expected);
}

/// GM-316: a missing `target/debug/g-mesh-plugin-*` binary - exactly the
/// shape `plugins/python/plugin.toml` and `plugins/rust/plugin.toml`
/// declare - must name the binary, say it was never built, and name
/// `cargo build --workspace`, not the bare `No such file or directory`
/// `Command::spawn` would otherwise report.
#[test]
fn missing_workspace_binary_hint_names_the_binary_and_the_build_command() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    let binary = workspace.path().join("target").join("debug").join("g-mesh-plugin-python");
    // Deliberately not created - this is the "never built" case.

    let hint = missing_workspace_binary_hint(&binary).expect("a missing target/debug binary must get a hint");
    assert!(hint.contains(&binary.display().to_string()), "{hint}");
    assert!(hint.contains("has not been built yet"), "{hint}");
    assert!(hint.contains("cargo build --workspace"), "{hint}");
    assert!(
        hint.contains(&workspace.path().display().to_string()),
        "the hint should name the workspace root: {hint}"
    );
}

/// The `release` profile is just as much a cargo build output as `debug`.
#[test]
fn missing_workspace_binary_hint_also_matches_the_release_profile() {
    let workspace = tempfile::tempdir().unwrap();
    let binary = workspace.path().join("target").join("release").join("g-mesh-plugin-rust");
    let hint =
        missing_workspace_binary_hint(&binary).expect("a missing target/release binary must get a hint");
    assert!(hint.contains("cargo build --workspace --release"), "{hint}");
}

/// GM-404: a debug build is told to run the plain build, never `--release`.
#[test]
fn missing_workspace_binary_hint_names_no_release_flag_for_the_debug_profile() {
    let workspace = tempfile::tempdir().unwrap();
    let binary = workspace.path().join("target").join("debug").join("g-mesh-plugin-rust");
    let hint = missing_workspace_binary_hint(&binary).expect("a missing target/debug binary must get a hint");
    assert!(hint.contains("`cargo build --workspace`"), "{hint}");
    assert!(!hint.contains("--release"), "{hint}");
}

/// A binary that exists gets no hint at all - the common case, and the
/// one that must stay cheap (no I/O beyond the one `is_file` check) on
/// every ordinary spawn.
#[test]
fn missing_workspace_binary_hint_is_none_once_the_binary_exists() {
    let workspace = tempfile::tempdir().unwrap();
    let dir = workspace.path().join("target").join("debug");
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("g-mesh-plugin-python");
    std::fs::write(&binary, b"").unwrap();
    assert_eq!(missing_workspace_binary_hint(&binary), None);
}

/// A path that is simply missing, but not under a cargo `target/<profile>`
/// directory, is not this check's business - the generic spawn-failure
/// message is left to name it.
#[test]
fn missing_workspace_binary_hint_ignores_a_missing_path_outside_target() {
    let workspace = tempfile::tempdir().unwrap();
    let binary = workspace.path().join("bin").join("g-mesh-plugin-go");
    assert_eq!(missing_workspace_binary_hint(&binary), None);
}

/// GM-335: on Windows, `manifest::resolve_exe_suffix` only ever switches
/// `command` to the `.exe` spelling once that file is confirmed to
/// exist - so a genuinely-missing binary reaches this function still
/// spelled without a suffix, exactly as `plugins/python/plugin.toml`
/// writes it. The hint must still name the `.exe` spelling, since that's
/// what a real `cargo build --workspace` on Windows actually produces
/// and therefore what a spawn attempt is actually missing - reporting
/// the unsuffixed spelling (what `command` is literally holding here)
/// would misname the missing file, not just fail to help with it. This
/// is the test that catches the message regressing back to `command`
/// unmodified - see this test module's own
/// `missing_workspace_binary_hint_names_the_binary_and_the_build_command`
/// for the non-Windows-suffix baseline this builds on, and this function's
/// own "Which spelling the message names" doc comment for the reasoning.
#[test]
fn missing_workspace_binary_hint_names_the_exe_suffixed_spelling_on_windows() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    // Exactly what `manifest::resolve_path_entry` leaves `command` as
    // when neither spelling exists: unsuffixed, per the manifest itself.
    let binary = workspace.path().join("target").join("debug").join("g-mesh-plugin-python");

    let hint = missing_workspace_binary_hint_with_suffix(&binary, ".exe")
        .expect("a missing target/debug binary must get a hint");

    // The binary the hint says is unbuilt is the `.exe` one, never the bare
    // name `command` holds (control: name `command` instead of the suffixed
    // spelling - both assertions fail, on any host, via the `_with_suffix`
    // entry point).
    let bare = binary.display().to_string();
    assert!(hint.contains(&format!("the plugin binary {bare}.exe has not been built yet")), "{hint}");
    assert!(!hint.contains(&format!("{bare} has not been built yet")), "{hint}");
}

/// GM-351: the hint's exact wording in both variants - the build command
/// first, so the 100-byte cap on a failed language's cause in the MCP
/// instructions can only cut into the path; then the workspace root (or
/// "the repository root" when no `Cargo.toml` marks one), the binary's path
/// and "has not been built yet", which the bulk-index and plugin-check
/// callers match on.
///
/// Controls: restore the old order (binary first); drop the root or the
/// binary's path or "has not been built yet" from either arm; swap the arms -
/// each fails one of the two `assert_eq!`s.
#[test]
fn missing_workspace_binary_hint_leads_with_the_build_command_in_both_variants() {
    let workspace = tempfile::tempdir().unwrap();
    let binary = workspace.path().join("target").join("release").join("g-mesh-plugin-rust");

    let rootless =
        missing_workspace_binary_hint_with_suffix(&binary, "").expect("a missing binary gets a hint");
    assert_eq!(
        rootless,
        format!(
            "Run `cargo build --workspace --release` in the repository root: \
             the plugin binary {} has not been built yet",
            binary.display()
        )
    );

    std::fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    let rooted =
        missing_workspace_binary_hint_with_suffix(&binary, "").expect("a missing binary gets a hint");
    assert_eq!(
        rooted,
        format!(
            "Run `cargo build --workspace --release` in {}: the plugin binary {} has not been built yet",
            workspace.path().display(),
            binary.display()
        )
    );
}

/// Once the `.exe` file actually exists, `command` itself would already
/// have been switched to it by `manifest::resolve_exe_suffix` before ever
/// reaching this function - so from this function's own point of view
/// (which only sees whatever `command` it was handed), an existing
/// suffixed binary reads as `command.is_file()` and produces no hint at
/// all, the same as the plain "binary exists" case.
#[test]
fn missing_workspace_binary_hint_is_none_when_the_command_already_carries_the_suffix() {
    let workspace = tempfile::tempdir().unwrap();
    let dir = workspace.path().join("target").join("debug");
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("g-mesh-plugin-python.exe");
    std::fs::write(&binary, b"").unwrap();

    assert_eq!(missing_workspace_binary_hint_with_suffix(&binary, ".exe"), None);
}

/// Both spawn sites (`PluginState::spawn`,
/// `daemon::bulk_index::walk_one_language`) call only
/// [`missing_plugin_binary_hint`], so it has to dispatch to the
/// workspace-binary check.
#[test]
fn missing_plugin_binary_hint_dispatches_to_the_workspace_check() {
    let workspace = tempfile::tempdir().unwrap();
    let binary = workspace.path().join("target").join("debug").join("g-mesh-plugin-python");
    let hint = missing_plugin_binary_hint(&binary).expect("a missing workspace binary must get a hint");
    assert!(hint.contains("cargo build --workspace"), "{hint}");
}

/// A pathological file count must not overflow the multiplication into a
/// wildly wrong (and, worse, possibly small) `Duration` - it clamps to
/// `u32::MAX` files instead, which still comfortably exceeds the floor.
#[test]
fn semantic_pass_project_timeout_does_not_overflow_on_an_absurd_file_count() {
    let timeouts = RoundTripTimeouts::default();
    let huge = timeouts.semantic_pass_project_timeout(usize::MAX);
    assert!(huge > timeouts.semantic_pass_project);
    assert_eq!(huge, SEMANTIC_PASS_PER_FILE_BUDGET.saturating_mul(u32::MAX));
}

/// Writes `files` (relative-path, contents pairs) under `dir`, creating
/// whatever subdirectories they need - the fixture every digest test in
/// this module builds on, whether or not it goes through a
/// [`PluginManifest`].
fn emit(dir: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
    }
}

/// A minimal fixture [`PluginManifest`] pointing at `dir`, with `ignore`
/// as its `fingerprint_ignore` - everything else is irrelevant to
/// [`fingerprint`], which only reads `manifest_dir` and
/// `fingerprint_ignore`.
fn manifest_for(dir: &Path, ignore: &[&str]) -> PluginManifest {
    PluginManifest {
        language: "fixture".to_string(),
        protocol_version: CURRENT_PROTOCOL_VERSION,
        plugin_version: String::new(),
        command: PathBuf::from("true"),
        args: Vec::new(),
        extensions: Vec::new(),
        fingerprint_ignore: ignore.iter().map(|s| s.to_string()).collect(),
        manifest_dir: dir.to_path_buf(),
        // Irrelevant to `fingerprint` - see this function's own doc
        // comment - so the conservative defaults are fine here too.
        capabilities: Capabilities::default(),
        workspace: WorkspaceConfig::default(),
        non_symbol_queries: Default::default(),
        symbol_query_prefixes: Default::default(),
        reexports: Default::default(),
    }
}

#[test]
fn the_same_build_fingerprints_the_same_way_twice() {
    let dir = tempfile::tempdir().unwrap();
    emit(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);

    let first = digest_of_plugin_build(dir.path(), &[]).unwrap();
    assert_eq!(digest_of_plugin_build(dir.path(), &[]).unwrap(), first);
    assert_eq!(first.len(), FINGERPRINT_HEX_CHARS);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first} must be hex");
}

/// The case task 116 is about: nothing but the extractor's own compiled
/// logic changed, and that has to be visible.
#[test]
fn changing_one_emitted_file_changes_the_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    emit(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);
    let before = digest_of_plugin_build(dir.path(), &[]).unwrap();

    fs::write(dir.path().join("extract.js"), "parse(); resolveLexically();").unwrap();

    assert_ne!(digest_of_plugin_build(dir.path(), &[]).unwrap(), before);
}

/// A rebuild can rewrite every file with identical bytes. A rebuild
/// that changed nothing must not cost a project a full re-walk, which is
/// why the digest is over content and not over mtimes.
#[test]
fn re_emitting_identical_bytes_leaves_the_fingerprint_alone() {
    let dir = tempfile::tempdir().unwrap();
    emit(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);
    let before = digest_of_plugin_build(dir.path(), &[]).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(10));
    fs::write(dir.path().join("extract.js"), "parse();").unwrap();

    assert_eq!(digest_of_plugin_build(dir.path(), &[]).unwrap(), before);
}

/// Moving a line from one module to another leaves the concatenated
/// bytes identical - the path and length mixed in are what keep the two
/// builds apart.
#[test]
fn moving_code_between_two_files_still_changes_the_fingerprint() {
    let one = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    emit(one.path(), &[("index.js", "ab"), ("extract.js", "c")]);
    emit(other.path(), &[("index.js", "a"), ("extract.js", "bc")]);

    let before = digest_of_plugin_build(one.path(), &[]).unwrap();
    let after = digest_of_plugin_build(other.path(), &[]).unwrap();

    assert_ne!(after, before);
}

#[test]
fn a_file_in_a_subdirectory_is_part_of_the_build_too() {
    let dir = tempfile::tempdir().unwrap();
    emit(dir.path(), &[("index.js", "run();"), ("lang/ts.js", "grammar();")]);
    let before = digest_of_plugin_build(dir.path(), &[]).unwrap();

    fs::write(dir.path().join("lang/ts.js"), "grammar(2);").unwrap();

    assert_ne!(digest_of_plugin_build(dir.path(), &[]).unwrap(), before);
}

/// An absent or unbuilt plugin is not a fingerprint of zero files - that
/// would compare equal to every other unbuilt tree and read as "nothing
/// changed".
#[test]
fn an_unbuilt_plugin_has_no_fingerprint_at_all() {
    let dir = tempfile::tempdir().unwrap();
    assert!(digest_of_plugin_build(dir.path(), &[]).is_err());
}

/// A change inside a directory on the built-in baseline ignore list (see
/// `docs/architecture/plugin-modularity.md`'s "Built-in baseline ignore")
/// must not move the fingerprint - the whole point of walking every file
/// by default is that known dependency/VCS junk is the one thing that
/// still has to be carved out by name.
#[test]
fn a_change_inside_a_baseline_ignored_directory_does_not_change_the_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    emit(dir.path(), &[("index.js", "run();")]);
    let before = digest_of_plugin_build(dir.path(), &[]).unwrap();

    emit(dir.path(), &[("node_modules/pkg/index.js", "module.exports = {};")]);
    assert_eq!(digest_of_plugin_build(dir.path(), &[]).unwrap(), before);

    fs::write(dir.path().join("node_modules/pkg/index.js"), "module.exports = { changed: true };").unwrap();
    assert_eq!(digest_of_plugin_build(dir.path(), &[]).unwrap(), before);
}

/// The same, but for a directory named in a manifest's own
/// `fingerprint_ignore` rather than the built-in baseline list -
/// `[plugin.fingerprint].ignore` extends the baseline, it does not
/// replace it.
#[test]
fn a_change_inside_a_manifest_declared_ignore_directory_does_not_change_the_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    emit(dir.path(), &[("index.js", "run();")]);
    let manifest = manifest_for(dir.path(), &["vendor"]);
    let before = fingerprint(&manifest);
    assert_ne!(before, FINGERPRINT_UNAVAILABLE);

    emit(dir.path(), &[("vendor/lib.js", "var x = 1;")]);
    assert_eq!(fingerprint(&manifest), before);

    fs::write(dir.path().join("vendor/lib.js"), "var x = 2;").unwrap();
    assert_eq!(fingerprint(&manifest), before);
}

/// The critical correctness property behind both ignore-list tests above:
/// ignoring specific directories must not accidentally ignore everything
/// else too.
#[test]
fn a_change_to_a_non_ignored_file_still_changes_the_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    emit(dir.path(), &[("index.js", "run();"), ("vendor/lib.js", "var x = 1;")]);
    let manifest = manifest_for(dir.path(), &["vendor"]);
    let before = fingerprint(&manifest);

    fs::write(dir.path().join("index.js"), "run(2);").unwrap();

    assert_ne!(fingerprint(&manifest), before);
}

/// The discovered plugins are readable from the running test binary, so
/// every build stamp this process publishes names a real fingerprint rather
/// than degrading to [`FINGERPRINT_UNAVAILABLE`].
#[test]
fn the_discovered_plugins_are_fingerprintable_from_the_test_binary() {
    let digest = discovered_fingerprint();

    assert_ne!(digest, FINGERPRINT_UNAVAILABLE, "the checkout's plugins must be discoverable");
    assert_eq!(digest.len(), FINGERPRINT_HEX_CHARS);
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()), "{digest} must be hex");
}

/// `plugins/typescript/plugin.toml` as discovery reads it, with its
/// capabilities reset to the defaults so a test does not depend on which
/// tiers the plugin turns on. `${G_MESH_BIN_DIR}` resolves to the test
/// binary's `target/<profile>/`.
fn typescript_manifest() -> PluginManifest {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript");
    let manifest = crate::daemon::manifest::read_manifest(&dir)
        .unwrap_or_else(|err| panic!("plugins/typescript/plugin.toml: {err:#}"));
    PluginManifest { capabilities: Capabilities::default(), ..manifest }
}

/// The check `docs/architecture/plugin-modularity.md`'s Interfaces
/// section adds right after `handshake::perform` succeeds: a manifest
/// whose declared `language` disagrees with what the live plugin's
/// handshake actually reports is a hard-fail, naming both values - the
/// TypeScript plugin's handshake reports `"typescript"` (see
/// `protocol::types`'s handshake test), so declaring anything else in
/// the manifest must be refused.
#[test]
fn spawning_a_manifest_whose_language_disagrees_with_the_live_handshake_hard_fails_naming_both() {
    let manifest = PluginManifest { language: "python".to_string(), ..typescript_manifest() };
    let project = tempfile::tempdir().unwrap();

    let err = match PluginProcess::spawn(project.path(), &manifest, project.path().join("plugin.pid")) {
        Ok(_) => panic!("spawning against a manifest declaring the wrong language must fail"),
        Err(err) => err,
    };

    let message = format!("{err:#}");
    assert!(message.contains("python"), "{message}");
    assert!(message.contains("typescript"), "{message}");
}

const GREET: &str = "export function greet(): string {\n  return \"hi\";\n}\n";
/// [`GREET`] with one more line in its body: `greet`'s range changes, so
/// a warm TS plugin reports it as a delete plus an upsert of the same id,
/// without re-sending the file's unchanged `DEFINES` edge into it.
const GREET_GROWN: &str = "export function greet(): string {\n  const a = 1;\n  return \"hi\";\n}\n";

/// An in-memory index that *enforces* foreign keys - the state the
/// daemon's connection was silently in before GM-293/GM-294, and the
/// most direct way to make a live plugin's perfectly ordinary diff be
/// refused by storage.
fn index_enforcing_foreign_keys() -> IndexStore {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    crate::storage::schema::apply(&conn).unwrap();
    IndexStore::new(conn)
}

fn greet_end_line(conn: &IndexStore) -> i64 {
    conn.lock()
        .unwrap()
        .query_row("SELECT endLine FROM nodes WHERE filePath = 'lib.ts' AND name = 'greet'", [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// GM-293, ported by GM-294. A diff the *storage* refuses, from a plugin
/// that is alive and well, is neither a crash nor a timeout - and
/// treating it as a crash was what hid GM-292 for as long as it lasted:
/// the "replay" asked that same live plugin again, its cache already held
/// the new text, so it answered with an empty diff and the failure came
/// back as `Ok(())`.
///
/// The refusal is produced the way production produced it: an index that
/// enforces foreign keys, then an edit through a warm plugin cache that
/// deletes and re-adds a symbol whose unchanged `DEFINES` edge is not
/// re-sent. The second half is what the deliberate relaunch is for: once
/// the index accepts writes again, the very next reparse of that file -
/// with no further edit on disk - must carry the edit in, rather than the
/// empty diff a plugin still caching the refused text would answer with.
///
/// `semantic_suspended = true` throughout: the refusal is in the
/// structural diff, and starting tsserver for a semantic pass would only
/// make this slower.
#[test]
fn a_storage_failure_behind_a_live_plugin_is_returned_and_the_relaunch_lets_the_next_apply_land() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("lib.ts");
    fs::write(&file, GREET).unwrap();
    let conn = index_enforcing_foreign_keys();

    let plugin =
        PluginProcess::spawn(project.path(), &typescript_manifest(), project.path().join("plugin.pid"))
            .expect("failed to spawn the JS/TS plugin");
    let embedding = EmbeddingPipeline::disabled();
    plugin
        .apply_file_change(&conn, "lib.ts", &embedding, true)
        .expect("a cold cache sends only upserts, which nothing can refuse");
    let pid = plugin.pid();

    fs::write(&file, GREET_GROWN).unwrap();
    let err = match plugin.apply_file_change(&conn, "lib.ts", &embedding, true) {
        Ok(_) => panic!("a diff the index refused must not be reported as applied"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(message.contains("FOREIGN KEY"), "the storage error itself must reach the caller: {message}");
    assert!(!is_timeout(&err), "and must not be mistaken for a timeout the supervisor would requeue");
    assert!(plugin.pending.lock().unwrap().is_empty(), "nothing is left queued for a later replay");
    assert_eq!(greet_end_line(&conn), 2, "nothing of the refused diff was committed");

    // Whatever refused the write stops refusing it.
    conn.lock().unwrap().pragma_update(None, "foreign_keys", "OFF").unwrap();
    plugin
        .apply_file_change(&conn, "lib.ts", &embedding, true)
        .expect("the same file's next reparse must apply once the index accepts writes");

    assert_eq!(
        greet_end_line(&conn),
        3,
        "the refused edit must reach the index on the next reparse - a plugin still caching it \
         would answer with an empty diff and leave `greet` ending on line 2"
    );
    assert_ne!(plugin.pid(), pid, "the plugin holding the refused text must have been relaunched");
    assert!(
        !crate::daemon::is_process_alive(pid),
        "the replaced plugin must be ended and reaped, not left running or as a zombie"
    );
}

/// The query-time twin of the test above, where getting it wrong is
/// worse: a retry that gets an empty diff also *records the baseline*,
/// marking the stale graph fresh. So besides the edit reaching the index
/// once writes are accepted again, the baseline must name what is on
/// disk - and must not have moved for the refused attempt.
#[test]
fn a_refused_query_time_reindex_relaunches_the_plugin_so_the_retry_applies_the_edit() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("lib.ts");
    fs::write(&file, GREET).unwrap();
    let conn = index_enforcing_foreign_keys();
    let baseline = |conn: &IndexStore| -> (i64, String) {
        conn.lock()
            .unwrap()
            .query_row(
                "SELECT mtimeMillis, contentHash FROM indexed_files WHERE filePath = 'lib.ts'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };
    let on_disk = |path: &Path| -> (i64, String) {
        let mtime = staleness::mtime_millis(&fs::metadata(path).unwrap()).unwrap();
        let hash = Sha256::digest(fs::read(path).unwrap()).iter().map(|b| format!("{b:02x}")).collect();
        (mtime, hash)
    };

    let plugin =
        PluginProcess::spawn(project.path(), &typescript_manifest(), project.path().join("plugin.pid"))
            .expect("failed to spawn the JS/TS plugin");
    let embedding = EmbeddingPipeline::disabled();
    assert_eq!(
        plugin.ensure_fresh(&conn, "lib.ts", &embedding, true).unwrap().0,
        StalenessOutcome::ReindexedNoPriorRecord,
        "a never-indexed file is a cold-cache reparse, which nothing can refuse"
    );
    let before = baseline(&conn);
    let pid = plugin.pid();

    std::thread::sleep(Duration::from_millis(10));
    fs::write(&file, GREET_GROWN).unwrap();
    let err = match plugin.ensure_fresh(&conn, "lib.ts", &embedding, true) {
        Ok((outcome, _)) => panic!("a reindex the index refused must not be reported as {outcome:?}"),
        Err(err) => format!("{err:#}"),
    };
    assert!(err.contains("FOREIGN KEY"), "the storage error itself must reach the caller: {err}");
    assert_eq!(baseline(&conn), before, "a refused reindex must not advance the baseline");

    // Whatever refused the write stops refusing it.
    conn.lock().unwrap().pragma_update(None, "foreign_keys", "OFF").unwrap();
    assert_eq!(
        plugin.ensure_fresh(&conn, "lib.ts", &embedding, true).unwrap().0,
        StalenessOutcome::ReindexedViaHashMismatch,
        "the file is still stale, so the next query must reindex it"
    );

    assert_eq!(
        greet_end_line(&conn),
        3,
        "the refused edit must reach the index on the retry - a plugin still caching it would \
         answer with an empty diff, leave `greet` ending on line 2, and have the baseline recorded anyway"
    );
    assert_eq!(baseline(&conn), on_disk(&file), "the baseline must now describe the file on disk");
    assert_ne!(plugin.pid(), pid, "the plugin holding the refused text must have been relaunched");
    assert!(
        !crate::daemon::is_process_alive(pid),
        "the replaced plugin must be ended and reaped, not left running or as a zombie"
    );
}
