// Included (with `include!`) by `core/build.rs` and by
// `core/tests/ts_build_stamp.rs`, so the test exercises exactly the decision
// the build script makes. Every path is spelled out in full rather than
// imported: an included file shares its includer's namespace, and a `use`
// here would collide with the includer's own.
//
// `core/build.rs` skips `npm run build` when a digest of the JS/TS plugin's
// build inputs equals the one recorded after the last successful build and
// the built entry point exists. Why a content digest and not an env guard:
// docs/results/gm-429-speedup-proposal.md, section 3.

/// The JS/TS plugin's build inputs, relative to `plugins/typescript`: both
/// what `core/build.rs` declares with `cargo:rerun-if-changed` and what
/// [`ts_inputs_digest`] hashes. One list, so the two cannot drift - a file
/// cargo watches but the digest ignored would leave a stale `dist/` behind a
/// matching stamp.
const TS_BUILD_INPUTS: &[&str] = &["src", "package.json", "tsconfig.json"];

/// Where the digest of the last successful build's inputs is recorded,
/// relative to `plugins/typescript`.
///
/// Must stay under a directory `daemon::plugin::fingerprint` skips
/// (`BASELINE_FINGERPRINT_IGNORE`): anywhere else in the plugin directory the
/// stamp would be part of the plugin's fingerprint, and adding or deleting it
/// would re-walk every index. `node_modules/` is also gitignored, and `npm ci`
/// replaces it, so a new `tsc` always rebuilds once.
const TS_BUILD_STAMP: &str = "node_modules/.g-mesh-ts-build-inputs";

/// The file whose absence means there is no build to keep, whatever the
/// stamp says: the entry point `daemon::plugin` spawns.
const TS_BUILD_ENTRY: &str = "dist/src/index.js";

/// A content digest of [`TS_BUILD_INPUTS`] under `plugin_dir`, as lowercase
/// hex. Each regular file contributes its path relative to `plugin_dir` with
/// `/` separators, its length and its bytes, in sorted path order - so the
/// digest ignores mtimes (an archive extraction or a restore that keeps old
/// mtimes still compares by content), is the same on every platform, and
/// changes when code moves between two files. A missing input contributes a
/// marker of its own, so deleting one changes the digest too.
fn ts_inputs_digest(plugin_dir: &std::path::Path) -> std::io::Result<String> {
    use sha2::Digest;

    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for input in TS_BUILD_INPUTS {
        let path = plugin_dir.join(input);
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => ts_collect_files(&path, input, &mut files)?,
            Ok(_) => files.push((input.to_string(), path)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => missing.push(input),
            Err(err) => return Err(err),
        }
    }
    files.sort();

    let mut hasher = sha2::Sha256::new();
    for input in missing {
        hasher.update(b"missing\0");
        hasher.update(input.as_bytes());
        hasher.update([0]);
    }
    for (relative, path) in &files {
        let bytes = std::fs::read(path)?;
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Every regular file under `dir`, recursively, as (`relative`-prefixed path
/// with `/` separators, absolute path). Symlinks are followed, as `tsc`
/// follows them.
fn ts_collect_files(
    dir: &std::path::Path,
    relative: &str,
    files: &mut Vec<(String, std::path::PathBuf)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_relative = format!("{relative}/{name}");
        let path = entry.path();
        if std::fs::metadata(&path)?.is_dir() {
            ts_collect_files(&path, &child_relative, files)?;
        } else {
            files.push((child_relative, path));
        }
    }
    Ok(())
}

/// Whether `npm run build` has to run: yes unless the built entry point
/// exists and the stamp records exactly the current inputs' digest. Any
/// doubt - no stamp, an unreadable input - means build, which is never wrong,
/// only slower.
fn ts_build_needed(plugin_dir: &std::path::Path) -> bool {
    if !plugin_dir.join(TS_BUILD_ENTRY).is_file() {
        return true;
    }
    let Ok(recorded) = std::fs::read_to_string(plugin_dir.join(TS_BUILD_STAMP)) else { return true };
    match ts_inputs_digest(plugin_dir) {
        Ok(current) => recorded.trim() != current,
        Err(_) => true,
    }
}

/// Removes the stamp before a build runs, so a build that fails part-way -
/// `tsc` emits even when it reports type errors - can never leave a stamp
/// vouching for a `dist/` it did not produce.
fn ts_forget_build(plugin_dir: &std::path::Path) {
    let _ = std::fs::remove_file(plugin_dir.join(TS_BUILD_STAMP));
}

/// Records the inputs' digest after a successful build. Digested *after* the
/// build, not before: `npm run build`'s `prebuild` step writes
/// `src/version.generated.ts`, one of the inputs, and the next run compares
/// against what is on disk then.
///
/// Records nothing when `node_modules/` is absent (a build that found `tsc`
/// somewhere else): creating that directory here would make the next failed
/// build's "run `npm ci`" diagnosis in `core/build.rs` miss its cue, and no
/// stamp only costs the next run a build.
fn ts_record_build(plugin_dir: &std::path::Path) -> std::io::Result<()> {
    let stamp = plugin_dir.join(TS_BUILD_STAMP);
    if !stamp.parent().is_some_and(std::path::Path::is_dir) {
        return Ok(());
    }
    let digest = ts_inputs_digest(plugin_dir)?;
    std::fs::write(stamp, digest)
}
