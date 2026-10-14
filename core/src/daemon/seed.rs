//! Seeding a linked git worktree's index from its main working tree's index
//! (GM-564, `docs/design/GM-564-seed-worktree-index.md`).
//!
//! The index stores only root-relative paths, so a fresh worktree's index is
//! almost the main checkout's. [`seed_if_eligible`] copies the main working
//! tree's `index.db` with SQLite's online backup into `index.db.seed-tmp`,
//! records where it came from (`seed_origin`), and renames it into place
//! before `connection::open` runs. Every failure means "no seed": the daemon
//! then cold-walks as it always did.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};

use crate::storage::{connection, schema, vectors};

/// The temp file a seed is copied into before its rename to `index.db`.
const SEED_TMP: &str = "index.db.seed-tmp";

/// Environment switch: `0` disables seeding, `1` enables it even while
/// [`SEEDED_RECONCILE_READY`] is false.
pub const SEED_ENV: &str = "G_MESH_SEED";

/// HOOK POINT (GM-564 D4/D5, next slice): whether the seeded first-start
/// reconcile exists. Until it does, a seeded index would go through the plain
/// reconcile, which keeps files ignored in the worktree but indexed in main,
/// may reindex whole languages on the fresh `.gitignore` mtimes, and answers
/// tool calls with main's content before reconciling. So seeding is off by
/// default and only `G_MESH_SEED=1` turns it on. The slice that adds the
/// seeded reconcile (driven by `schema::seed_pending`) flips this to `true`.
const SEEDED_RECONCILE_READY: bool = false;

/// Where a seeded index came from, for the start log line.
#[derive(Debug, Clone)]
pub struct SeedOrigin {
    pub source_root: PathBuf,
    pub copy_ms: u128,
}

/// Whether seeding is switched on, from `G_MESH_SEED` and
/// [`SEEDED_RECONCILE_READY`].
fn seed_enabled() -> bool {
    match std::env::var(SEED_ENV).ok().as_deref().map(str::trim) {
        Some("0") => false,
        Some("1") => true,
        _ => SEEDED_RECONCILE_READY,
    }
}

/// The main working tree's counterpart of `root` when `root` lies in a linked
/// git worktree (D1a), else `None`.
///
/// Walks up from `root` to the first `.git`. It must be a file whose
/// `gitdir:` names a directory containing `commondir`; the common dir must be
/// named `.git` and be the `.git` directory of its parent, which is the main
/// working tree. `root`'s path below its worktree top is applied to that tree.
/// Rejected: the main checkout itself (`.git` is a directory), a submodule
/// (`.git` file, no `commondir`), a bare repo's worktree (common dir not a
/// working tree's `.git`).
pub fn main_checkout(root: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let (top, dot_git, meta) = root.ancestors().find_map(|dir| {
        let candidate = dir.join(".git");
        fs::symlink_metadata(&candidate).ok().map(|meta| (dir.to_path_buf(), candidate, meta))
    })?;
    if !meta.is_file() {
        return None;
    }
    let contents = fs::read_to_string(&dot_git).ok()?;
    let gitdir = contents.lines().find_map(|line| line.strip_prefix("gitdir:"))?.trim();
    if gitdir.is_empty() {
        return None;
    }
    let gitdir = top.join(gitdir);
    let commondir = fs::read_to_string(gitdir.join("commondir")).ok()?;
    let commondir = commondir.trim();
    if commondir.is_empty() {
        return None;
    }
    let common = gitdir.join(commondir).canonicalize().ok()?;
    if common.file_name()? != ".git" || !common.is_dir() {
        return None;
    }
    let main_top = common.parent()?.to_path_buf();
    // The common dir must be its parent's `.git` directory and not a bare
    // repo cloned into a directory named `.git` (`core.bare` decides).
    let main_dot_git = fs::symlink_metadata(main_top.join(".git")).ok()?;
    if !main_dot_git.is_dir() || is_bare(&common) {
        return None;
    }
    if main_top == top {
        return None;
    }
    let below = root.strip_prefix(&top).ok()?;
    let main_root = main_top.join(below);
    main_root.is_dir().then_some(main_root)
}

/// Whether the git config in `common` says `bare = true` (in `[core]`).
fn is_bare(common: &Path) -> bool {
    let Ok(config) = fs::read_to_string(common.join("config")) else { return false };
    let mut in_core = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_core = line.trim_start_matches('[').trim_end_matches(']').trim().eq_ignore_ascii_case("core");
            continue;
        }
        if in_core {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("bare") {
                    return value.trim().eq_ignore_ascii_case("true");
                }
            }
        }
    }
    false
}

/// Deletes a seed temp file (and its SQLite side files) a killed seed left in
/// `state_dir`. Called at start, next to `remove_stale_staging`.
pub(crate) fn remove_stale_seed_tmp(state_dir: &Path) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let path = state_dir.join(format!("{SEED_TMP}{suffix}"));
        if let Err(err) = fs::remove_file(&path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                crate::log_line!(
                    "g-mesh daemon: could not remove the stale seed file {} ({err})",
                    path.display()
                );
            }
        }
    }
}

/// Seeds `state_dir/index.db` for the project at `root` from its main working
/// tree's index when every D2 condition holds; logs the reason and returns
/// `None` otherwise. Never fails the start: an error is a "no seed".
pub(crate) fn seed_if_eligible(root: &Path, state_dir: &Path, indexer_version: &str) -> Option<SeedOrigin> {
    if !seed_enabled() {
        return None;
    }
    if fs::symlink_metadata(state_dir.join("index.db")).is_ok() {
        return None;
    }
    let source_root = main_checkout(root)?;
    let source_db = match connection::project_dir(&source_root) {
        Ok(dir) => dir.join("index.db"),
        Err(err) => {
            crate::log_line!(
                "g-mesh daemon: not seeding - no state dir for {} ({err:#})",
                source_root.display()
            );
            return None;
        }
    };
    if fs::symlink_metadata(&source_db).is_err() {
        crate::log_line!("g-mesh daemon: not seeding - {} has no index", source_root.display());
        return None;
    }
    match seed_from(&source_db, state_dir, &source_root, indexer_version) {
        Ok(Some(origin)) => Some(origin),
        Ok(None) => None,
        Err(err) => {
            remove_stale_seed_tmp(state_dir);
            crate::log_line!("g-mesh daemon: not seeding from {} ({err:#})", source_root.display());
            None
        }
    }
}

/// Whether the index on `conn` is a complete one of the expected generation.
/// `Err` carries the reason it is not.
fn check_source(conn: &Connection, indexer_version: &str) -> Result<()> {
    let expected = (schema::CURRENT_SCHEMA_VERSION.to_string(), indexer_version.to_string());
    match schema::stored_generation(conn) {
        Some(generation) if generation == expected => {}
        _ => bail!("its generation differs from this daemon's"),
    }
    if !schema::bulk_index_completed(conn)? {
        bail!("it has not finished its walk");
    }
    Ok(())
}

fn seed_from(
    source_db: &Path,
    state_dir: &Path,
    source_root: &Path,
    indexer_version: &str,
) -> Result<Option<SeedOrigin>> {
    vectors::register_extension();
    let started = Instant::now();
    let source = Connection::open_with_flags(
        source_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("failed to open {} read-only", source_db.display()))?;
    source.busy_timeout(Duration::from_secs(5)).context("failed to set a busy timeout")?;
    // Checked before copying, so a mismatch never costs a copy.
    if let Err(reason) = check_source(&source, indexer_version) {
        crate::log_line!("g-mesh daemon: not seeding from {} - {reason:#}", source_root.display());
        return Ok(None);
    }

    let tmp = state_dir.join(SEED_TMP);
    remove_stale_seed_tmp(state_dir);
    {
        let mut dest =
            Connection::open(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;
        {
            // One step of every page: one read transaction, so a consistent
            // snapshot including WAL-only commits; an incremental backup
            // would restart on every write of the main daemon.
            let backup = Backup::new(&source, &mut dest).context("failed to start the backup")?;
            match backup.step(-1).context("failed to copy the index")? {
                StepResult::Done => {}
                other => bail!("the backup did not finish ({other:?})"),
            }
        }
        drop(source);
        dest.pragma_update(None, "foreign_keys", "OFF")
            .context("failed to disable foreign-key enforcement")?;
        // The copy's own snapshot is what counts: the main daemon may have
        // reset its index between the check above and the backup.
        if let Err(reason) = check_source(&dest, indexer_version) {
            drop(dest);
            remove_stale_seed_tmp(state_dir);
            crate::log_line!("g-mesh daemon: not seeding from {} - {reason:#}", source_root.display());
            return Ok(None);
        }
        schema::record_seed_origin(&dest, &source_root.to_string_lossy())?;
        dest.pragma_update(None, "wal_checkpoint", "TRUNCATE").context("failed to checkpoint the copy")?;
    }
    fs::File::open(&tmp)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("failed to sync {}", tmp.display()))?;
    // A crashed daemon's side files with no `index.db` would be applied to
    // the renamed copy.
    for side in ["index.db-wal", "index.db-shm", "index.db-journal"] {
        match fs::remove_file(state_dir.join(side)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("failed to remove a stale {side}")),
        }
    }
    remove_seed_side_files(state_dir);
    let db = state_dir.join("index.db");
    fs::rename(&tmp, &db).with_context(|| format!("failed to rename {} into place", tmp.display()))?;
    if let Ok(dir) = fs::File::open(state_dir) {
        let _ = dir.sync_all();
    }
    Ok(Some(SeedOrigin { source_root: source_root.to_path_buf(), copy_ms: started.elapsed().as_millis() }))
}

/// Removes the temp copy's own `-wal`/`-shm` once its connection is closed
/// and checkpointed, so nothing named after the temp file outlives it.
fn remove_seed_side_files(state_dir: &Path) {
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ = fs::remove_file(state_dir.join(format!("{SEED_TMP}{suffix}")));
    }
}
