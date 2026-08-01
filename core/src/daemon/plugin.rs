//! Spawns the bundled JS/TS language plugin as a child process, performs its
//! handshake, and gives the rest of the daemon a way to route `FileChanged`
//! requests to it and apply the diff it answers with - the missing link
//! between the daemon (Rust) and the plugin (Node.js) process.
//!
//! Only the one bundled JS/TS plugin is spawned here, unconditionally. The
//! general `~/.g-mesh/plugins/<language>/` discovery/manifest scheme
//! documented in the v1 architecture doc is deliberately not built: this MVP
//! release bundles exactly one plugin, so there is nothing to discover.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::protocol::handshake;
use crate::protocol::types::RequestId;
use crate::storage::schema::CURRENT_INDEXER_VERSION;
use crate::watcher::apply::apply_file_change as apply_file_change_diff;

/// Overrides where the plugin's compiled entry point lives. Real installs
/// never need this - the default already resolves to the bundled plugin -
/// but it lets the integration test suite point at a build without
/// depending on the daemon binary's own install location.
pub const PLUGIN_PATH_ENV: &str = "G_MESH_JS_TS_PLUGIN_PATH";

/// How much of the digest [`fingerprint`] keeps. 64 bits is far more than
/// enough to tell two builds of one plugin apart, and short enough that a
/// human reading a build stamp file can compare two of them at a glance.
const FINGERPRINT_HEX_CHARS: usize = 16;

/// What [`fingerprint`] answers when it cannot read the plugin's build at
/// all. Deliberately a fixed string rather than a random or timestamped one:
/// two processes that both fail to look compare *equal*, which degrades to
/// the behavior there was before this existed instead of making every start
/// look like a change. It cannot collide with a real answer, which is hex.
pub const FINGERPRINT_UNAVAILABLE: &str = "unavailable";

/// Shared with `daemon::bulk_index`, which spawns the same entry point in
/// its one-shot mode - both must honor the same override.
pub(crate) fn plugin_entry_path() -> PathBuf {
    if let Ok(over) = std::env::var(PLUGIN_PATH_ENV) {
        return PathBuf::from(over);
    }
    // `core/` and `plugins/js-ts/` are sibling directories in this repo, and
    // there is no distribution pipeline yet (see release notes' backlog) -
    // resolving relative to this crate's own source tree, baked in at
    // compile time, is the pragmatic MVP answer.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/js-ts/dist/src/index.js")
}

/// Identifies the plugin *logic* this process would run, as a short hex
/// digest of its compiled output.
///
/// # Why this exists
///
/// The whole graph is computed by the plugin, but until task 116 nothing in
/// g-mesh could tell that the plugin had changed. The two staleness checks
/// that existed both looked somewhere else: `daemon::build_stamp` at the core
/// executable, and `storage::schema::CURRENT_INDEXER_VERSION` at a constant
/// somebody has to remember to bump. Task 115 rewrote how the extractor
/// resolves same-file edges and - correctly following every rule that was
/// written down, none of which is enforced by anything - did not bump that
/// constant. Every existing index went on serving the previous extractor's
/// output, with a current schema, a current core binary, and no symptom other
/// than wrong answers.
///
/// So this is the plugin's half of "which pipeline produced what is in the
/// index", derived the way `build_stamp`'s docs argue the core's half should
/// be: from the artifact itself, so it needs no discipline to maintain and
/// cannot silently agree when it should not.
///
/// # Content, not mtime
///
/// `build_stamp` compares the core executable's mtime because it only needs an
/// *ordering* ("is that daemon behind me?"). This one has to answer a
/// different question - "would that build produce a different graph?" - where
/// mtime is both too eager and unordered: `npm run build` rewrites every file
/// in `dist/` on every invocation, and a re-emitted but byte-identical bundle
/// must not cost a project a full re-walk. A digest over the bytes changes
/// exactly when the logic does.
///
/// # What it does not cover
///
/// The plugin's *dependencies* - the tree-sitter grammars in `node_modules` -
/// are not hashed: they are large, they are not part of what `npm run build`
/// emits, and walking them on every shim start would turn a sub-millisecond
/// check into a directory crawl. A grammar upgrade that changes extraction is
/// therefore still a manual [`CURRENT_INDEXER_VERSION`] bump, which is exactly
/// what that constant remains for - the two halves are complementary, not
/// redundant.
///
/// Computed once per process: the shim asks for it on every call it makes, and
/// the answer cannot change under a running process in any way that would
/// matter (the plugin a daemon already spawned is the one it keeps).
pub fn fingerprint() -> &'static str {
    static FINGERPRINT: OnceLock<String> = OnceLock::new();
    FINGERPRINT.get_or_init(|| {
        let entry = plugin_entry_path();
        digest_of_plugin_build(&entry).unwrap_or_else(|err| {
            eprintln!(
                "g-mesh: could not fingerprint the JS/TS plugin at {}: {err:#} - \
                 a change to its extraction logic will not be noticed",
                entry.display()
            );
            FINGERPRINT_UNAVAILABLE.to_string()
        })
    })
}

/// The generation string an index is stamped with, and the thing
/// `storage::schema::ensure_current` compares: core's hand-maintained
/// pipeline generation and the plugin build that filled the index, joined.
///
/// Both halves have to be in it. The constant alone misses every plugin-side
/// change (the failure task 116 fixes); the fingerprint alone would miss every
/// change in `graph::imports` / `graph::symbol_links`, which run in core and
/// leave the plugin's bytes untouched.
pub fn indexer_version() -> String {
    format!("{CURRENT_INDEXER_VERSION}+{}", fingerprint())
}

/// Digests every compiled file the plugin ships, in a stable order.
///
/// The whole emitted tree rather than just the entry point: `dist/src` is one
/// `tsc` output split across modules, and the extractor - the part most likely
/// to change what the graph looks like - is not the entry file. Each file's
/// path and length go into the digest alongside its bytes, so moving code
/// between two files cannot leave the concatenation unchanged.
fn digest_of_plugin_build(entry: &Path) -> Result<String> {
    let dir = entry
        .parent()
        .with_context(|| format!("{} has no parent directory", entry.display()))?;

    let mut files = Vec::new();
    collect_emitted_files(dir, dir, &mut files)?;
    if files.is_empty() {
        bail!("no compiled plugin files found under {}", dir.display());
    }
    // Directory iteration order is whatever the filesystem feels like, and a
    // fingerprint that depends on it would differ between two identical
    // checkouts.
    files.sort();

    let mut hasher = Sha256::new();
    for (relative, path) in &files {
        let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }

    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(FINGERPRINT_HEX_CHARS)
        .collect())
}

/// Gathers every `.js` file under `dir` as a pair of its path relative to
/// `root` and its full path. Recursive rather than one flat `read_dir` so a
/// future plugin laid out in subdirectories does not silently fall outside
/// the fingerprint - a blind spot in this function is a wrong answer served
/// later, which is the exact failure it exists to prevent.
fn collect_emitted_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("failed to list {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read an entry of {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", path.display()))?;
        if file_type.is_dir() {
            collect_emitted_files(root, &path, out)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        out.push((relative, path));
    }
    Ok(())
}

struct PluginIo {
    reader: BufReader<ChildStdout>,
    writer: ChildStdin,
}

/// A live handle on the spawned JS/TS plugin process. `Mutex`-wrapped so it
/// can be shared across the connection-serving threads and the watcher
/// thread the same way `daemon::run` already shares its `Connection` - a
/// full actor/async rewrite is more than this ticket needs.
pub struct PluginProcess {
    // Kept alive so the child is not dropped (and its pipes closed) while
    // still in use; only its pid is ever read, never its exit status.
    // Killing it explicitly on daemon shutdown is unnecessary: the OS closes
    // the daemon's end of the child's stdin when the daemon process exits,
    // which the plugin already treats as its cue to exit (see index.ts's
    // stdin "end" handler).
    child: Child,
    io: Mutex<PluginIo>,
    next_id: AtomicI64,
}

impl PluginProcess {
    /// Spawns the plugin for `project_root`, reads its handshake off stdout,
    /// and hard-fails - matching `handshake::verify`'s "a protocol mismatch
    /// is a hard load failure" philosophy - if it doesn't check out.
    pub fn spawn(project_root: &Path) -> Result<Self> {
        let entry = plugin_entry_path();
        let mut child = Command::new("node")
            .arg(&entry)
            .arg(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Plugin logs are diagnostic-only today - nothing consumes them
            // programmatically - so forwarding to the daemon's own stderr
            // is simplest; it still shows up wherever the daemon's stderr
            // goes (or /dev/null in tests that don't care).
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn JS/TS plugin at {}", entry.display()))?;

        let stdout = child
            .stdout
            .take()
            .context("plugin child process has no stdout")?;
        let stdin = child
            .stdin
            .take()
            .context("plugin child process has no stdin")?;

        let mut reader = BufReader::new(stdout);
        handshake::perform(&mut reader).context("JS/TS plugin handshake failed")?;

        Ok(Self {
            child,
            io: Mutex::new(PluginIo { reader, writer: stdin }),
            next_id: AtomicI64::new(1),
        })
    }

    /// The plugin process's pid, so the daemon can record it for tooling that
    /// has to reason about the plugin from outside this process.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Sends a `FileChanged` request for `file_path` to the plugin and
    /// applies its diff response to `conn`. The plugin's stdin/stdout pair
    /// is locked for the round trip's duration, so concurrent callers (e.g.
    /// a future reindex path alongside the watcher thread) queue rather than
    /// interleave their requests on the wire.
    pub fn apply_file_change(&self, conn: &Mutex<Connection>, file_path: impl Into<String>) -> Result<()> {
        // A per-process atomic counter is all `apply_file_change`'s doc
        // comment asks for - it only needs an id unique enough to catch a
        // response answering the wrong request, not a globally unique one.
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let mut io = self.io.lock().unwrap();
        // Split into disjoint field borrows up front - borrowing `io.reader`
        // and `io.writer` directly as two separate `&mut` arguments doesn't
        // typecheck through the `MutexGuard`'s `DerefMut`.
        let PluginIo { reader, writer } = &mut *io;
        let mut conn = conn.lock().unwrap();
        apply_file_change_diff(reader, writer, &mut conn, file_path, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a throwaway `dist/`-shaped directory and returns its entry
    /// point, so the digest can be exercised without a real plugin build.
    fn emitted(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
        for (relative, contents) in files {
            let path = dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
        }
        dir.join("index.js")
    }

    #[test]
    fn the_same_build_fingerprints_the_same_way_twice() {
        let dir = tempfile::tempdir().unwrap();
        let entry = emitted(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);

        let first = digest_of_plugin_build(&entry).unwrap();
        assert_eq!(digest_of_plugin_build(&entry).unwrap(), first);
        assert_eq!(first.len(), FINGERPRINT_HEX_CHARS);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first} must be hex");
    }

    /// The case task 116 is about: nothing but the extractor's own compiled
    /// logic changed, and that has to be visible.
    #[test]
    fn changing_one_emitted_file_changes_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let entry = emitted(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);
        let before = digest_of_plugin_build(&entry).unwrap();

        fs::write(dir.path().join("extract.js"), "parse(); resolveLexically();").unwrap();

        assert_ne!(digest_of_plugin_build(&entry).unwrap(), before);
    }

    /// `npm run build` rewrites every file on every invocation. A rebuild
    /// that changed nothing must not cost a project a full re-walk, which is
    /// why the digest is over content and not over mtimes.
    #[test]
    fn re_emitting_identical_bytes_leaves_the_fingerprint_alone() {
        let dir = tempfile::tempdir().unwrap();
        let entry = emitted(dir.path(), &[("index.js", "run();"), ("extract.js", "parse();")]);
        let before = digest_of_plugin_build(&entry).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(dir.path().join("extract.js"), "parse();").unwrap();

        assert_eq!(digest_of_plugin_build(&entry).unwrap(), before);
    }

    /// Moving a line from one module to another leaves the concatenated
    /// bytes identical - the path and length mixed in are what keep the two
    /// builds apart.
    #[test]
    fn moving_code_between_two_files_still_changes_the_fingerprint() {
        let one = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let before = digest_of_plugin_build(&emitted(
            one.path(),
            &[("index.js", "ab"), ("extract.js", "c")],
        ))
        .unwrap();
        let after = digest_of_plugin_build(&emitted(
            other.path(),
            &[("index.js", "a"), ("extract.js", "bc")],
        ))
        .unwrap();

        assert_ne!(after, before);
    }

    #[test]
    fn a_file_in_a_subdirectory_is_part_of_the_build_too() {
        let dir = tempfile::tempdir().unwrap();
        let entry = emitted(dir.path(), &[("index.js", "run();"), ("lang/ts.js", "grammar();")]);
        let before = digest_of_plugin_build(&entry).unwrap();

        fs::write(dir.path().join("lang/ts.js"), "grammar(2);").unwrap();

        assert_ne!(digest_of_plugin_build(&entry).unwrap(), before);
    }

    /// An absent or unbuilt plugin is not a fingerprint of zero files - that
    /// would compare equal to every other unbuilt tree and read as "nothing
    /// changed".
    #[test]
    fn an_unbuilt_plugin_has_no_fingerprint_at_all() {
        let dir = tempfile::tempdir().unwrap();
        assert!(digest_of_plugin_build(&dir.path().join("index.js")).is_err());

        fs::write(dir.path().join("README.md"), "not a build").unwrap();
        assert!(digest_of_plugin_build(&dir.path().join("index.js")).is_err());
    }

    /// The two halves of the generation string are both there, and the one
    /// the running test binary computes is a real one - `core/build.rs` has
    /// just built the plugin it points at.
    #[test]
    fn the_recorded_generation_names_the_core_pipeline_and_the_plugin_build() {
        let version = indexer_version();
        let (core, plugin) = version.split_once('+').expect("both halves must be present");

        assert_eq!(core, CURRENT_INDEXER_VERSION);
        assert_eq!(plugin, fingerprint());
        assert_ne!(
            plugin, FINGERPRINT_UNAVAILABLE,
            "the test binary's own plugin build must be readable - `cargo test` builds it"
        );
    }
}
