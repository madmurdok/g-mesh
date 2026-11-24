//! The index's generation string: core's pipeline generation joined with a
//! digest of every discovered plugin's build. Reads only files under each
//! manifest's `manifest_dir`; it spawns no plugin.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use super::{DiscoveredPlugins, PluginManifest};
use crate::storage::schema::CURRENT_INDEXER_VERSION;

/// How much of the digest [`fingerprint`] keeps. 64 bits is far more than
/// enough to tell two builds of one plugin apart, and short enough that a
/// human reading a build stamp file can compare two of them at a glance.
pub(crate) const FINGERPRINT_HEX_CHARS: usize = 16;

/// What [`fingerprint`] answers when it cannot read the plugin's build at
/// all. A fixed string rather than a random or timestamped one: two processes
/// that both fail to look compare *equal* instead of making every start look
/// like a change. It cannot collide with a real answer, which is hex.
pub const FINGERPRINT_UNAVAILABLE: &str = "unavailable";

/// Directory names skipped while fingerprinting *every* plugin, regardless of
/// what its own manifest says - see `docs/architecture/plugin-modularity.md`'s
/// Data Model section ("Built-in baseline ignore"). A manifest's own
/// `fingerprint_ignore` extends this list; it never replaces it.
const BASELINE_FINGERPRINT_IGNORE: &[&str] =
    &[".git", "node_modules", "__pycache__", ".venv", "venv", ".pytest_cache"];

/// The generation string an index is stamped with, and the thing
/// `storage::schema::ensure_current` compares: core's hand-maintained pipeline
/// generation ([`CURRENT_INDEXER_VERSION`]) and the plugin *builds* that
/// filled the index, joined.
///
/// Both halves have to be in it. The constant alone misses every plugin-side
/// change; the plugin half alone would miss every change in `link::imports` /
/// `link::symbol_links`, which run in core and leave every plugin's bytes
/// untouched.
///
/// # Every discovered plugin, not just the one that was spawned
///
/// An index is a single artifact filled by whichever plugins had something to
/// say about the project, so it is only as current as the *least* current of
/// the builds behind it: a Python plugin rebuilt with a different extractor
/// invalidates the same `index.db` a JS/TS rebuild does.
///
/// Discovered rather than *active*: which supervisors happen to have been
/// spawned is a property of what the daemon has been asked so far, not of
/// what filled the index, and reading it would make the answer change under a
/// running daemon.
///
/// Takes `&DiscoveredPlugins` rather than a registry: `daemon::run` checks the
/// index before it builds a registry, and `cli::init`/`cli::reindex` never
/// build one.
pub fn indexer_version(discovered: &DiscoveredPlugins) -> String {
    format!("{CURRENT_INDEXER_VERSION}+{}", plugins_digest(discovered))
}

/// One digest over every discovered plugin's `(language, fingerprint)` pair.
///
/// Re-hashed rather than concatenated so the result stays one fixed-width
/// value however many plugins are installed.
///
/// Sorted by language first. `DiscoveredPlugins::manifests` is a `HashMap`, so
/// its iteration order varies between processes; hashing in that order would
/// make two daemons of the same install disagree about the index they share
/// and wipe each other's work. The language is hashed alongside its
/// fingerprint (and both are NUL-delimited) so that renaming a plugin, or two
/// languages swapping builds, cannot leave the concatenation unchanged.
pub fn plugins_digest(discovered: &DiscoveredPlugins) -> String {
    let mut fingerprinted: Vec<(&str, String)> = discovered
        .manifests
        .iter()
        .map(|(language, manifest)| (language.as_str(), fingerprint(manifest)))
        .collect();
    fingerprinted.sort_unstable_by_key(|(one, _)| *one);

    let mut hasher = Sha256::new();
    for (language, fingerprint) in &fingerprinted {
        hasher.update(language.as_bytes());
        hasher.update([0]);
        hasher.update(fingerprint.as_bytes());
        hasher.update([0]);
    }
    truncated_hex(hasher)
}

/// [`indexer_version`] for a discovery that found nothing - the generation
/// string a *test* stamps a fixture index with when it needs `meta` to exist
/// and nothing will ever compare that stamp against a live daemon's.
///
/// Over an empty [`DiscoveredPlugins`]: a real discovery would make every
/// fixture walk and hash the installed plugins' whole build for a value none
/// of them reads back.
#[cfg(test)]
pub(crate) fn fixture_indexer_version() -> String {
    indexer_version(&DiscoveredPlugins::default())
}

/// Identifies the plugin *logic* a process would run, as a short hex digest
/// of its build output: the plugin's half of "which pipeline produced what is
/// in the index", derived from the artifact itself so it needs no discipline
/// to maintain.
///
/// # Content, not mtime
///
/// The question is "would that build produce a different graph?", where
/// mtime is both too eager and unordered: a rebuild can rewrite a file with
/// identical bytes, and a re-emitted but byte-identical build must not cost a
/// project a full re-walk. A digest over the bytes changes exactly when the
/// logic does.
///
/// # What it does not cover
///
/// A plugin's dependency/VCS junk - `node_modules`, `.git`, and the rest of
/// [`BASELINE_FINGERPRINT_IGNORE`] (plus anything a manifest's own
/// `fingerprint_ignore` adds) - is walked but skipped by name, not hashed. A
/// dependency upgrade that changes extraction is therefore still a manual
/// [`CURRENT_INDEXER_VERSION`] bump.
pub fn fingerprint(manifest: &PluginManifest) -> String {
    digest_of_plugin_build(&manifest.manifest_dir, &manifest.fingerprint_ignore).unwrap_or_else(|err| {
        crate::log_line!(
            "g-mesh: could not fingerprint the {} plugin at {}: {err:#} - \
                 a change to its extraction logic will not be noticed",
            manifest.language,
            manifest.manifest_dir.display()
        );
        FINGERPRINT_UNAVAILABLE.to_string()
    })
}

/// Digests every regular file under `dir`, skipping any subdirectory whose
/// name is in [`BASELINE_FINGERPRINT_IGNORE`] or `ignore`, in a stable order.
///
/// Each file's path and length go into the digest alongside its bytes, so
/// moving code between two files cannot leave the concatenation unchanged.
pub(crate) fn digest_of_plugin_build(dir: &Path, ignore: &[String]) -> Result<String> {
    let mut files = Vec::new();
    collect_fingerprinted_files(dir, dir, ignore, &mut files)?;
    if files.is_empty() {
        bail!("no plugin files found under {}", dir.display());
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

    Ok(truncated_hex(hasher))
}

/// Finishes `hasher` the one way a digest is rendered here: lowercase hex,
/// cut to [`FINGERPRINT_HEX_CHARS`]. Shared by the per-plugin fingerprint and
/// [`plugins_digest`], so both read alike.
fn truncated_hex(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(FINGERPRINT_HEX_CHARS)
        .collect()
}

/// Gathers every regular file under `dir` as a pair of its path relative to
/// `root` and its full path, recursing into subdirectories except those named
/// in [`BASELINE_FINGERPRINT_IGNORE`] or `ignore`. A plugin laid out in
/// subdirectories must not fall outside the fingerprint.
fn collect_fingerprinted_files(
    root: &Path,
    dir: &Path,
    ignore: &[String],
    out: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("failed to list {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read an entry of {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry.file_type().with_context(|| format!("failed to stat {}", path.display()))?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if BASELINE_FINGERPRINT_IGNORE.contains(&name.as_ref())
                || ignore.iter().any(|ignored| ignored == name.as_ref())
            {
                continue;
            }
            collect_fingerprinted_files(root, &path, ignore, out)?;
            continue;
        }
        if !file_type.is_file() {
            // Symlinks, sockets, etc. - not a regular file this plugin's
            // build emitted.
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
        out.push((relative, path));
    }
    Ok(())
}
