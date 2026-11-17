use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::paths;

/// Deterministic identity for a project root, shared by every call site
/// that needs to agree on it (SQLite storage location, daemon socket/pid
/// file location) - a single function so they can never diverge.
pub fn project_hash(root: &Path) -> Result<String> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", root.display()))?;
    Ok(hash_of_canonical(&canonical))
}

/// The hash of an already-canonical path: sha256 of its lossy string form,
/// first 8 bytes, lowercase hex. Changing it orphans every existing index.
pub(crate) fn hash_of_canonical(canonical: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The canonical project root a state directory belongs to, written beside
/// the `daemon.pid`/`daemon.build` files already there.
///
/// [`project_hash`] is one-way and everything inside the index is stored
/// relative to the root (`indexed_files.filePath` is
/// `src/thing.ts`, never an absolute path), so without this file a state
/// directory cannot say which project it is for. `cli::clean`'s `orphaned`
/// target is the reason that matters: state whose project has been deleted is
/// what actually accumulates, and it cannot be recognized without knowing
/// where the project was.
pub const ROOT_FILE: &str = "project.root";

/// The device id (`st_dev`) of the filesystem the project root was on when
/// [`record_project_root`] last ran, beside [`ROOT_FILE`]. Unix only.
///
/// It is what lets `cli::clean::root_state` tell a deleted directory from an
/// unmounted volume: a deleted root leaves its parent on the root's own
/// device, while an unmounted volume leaves a parent on another one.
pub const DEVICE_FILE: &str = "project.dev";

/// Records `root` as the project `state_dir` holds the state for.
///
/// Idempotent - rewritten every time the state directory is opened, which is
/// also how a directory created before this file existed acquires one.
///
/// The path is canonicalized here rather than trusted from the caller, so
/// that what is written is the same form [`project_hash`] hashed - two
/// spellings of one root (a relative path, a symlink, a trailing slash) must
/// not produce a state directory that disagrees with itself about where its
/// project is.
///
/// On Unix the root's device id is also written, to [`DEVICE_FILE`]. It is a
/// sidecar file and not a second line of `project.root` on purpose: an older
/// g-mesh reads the whole of `project.root` as the path, so a second line
/// would turn the path into something whose parent exists and let that
/// binary delete a live project.
pub fn record_project_root(state_dir: &Path, root: &Path) -> Result<()> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", root.display()))?;
    let path = state_dir.join(ROOT_FILE);
    fs::write(&path, canonical.to_string_lossy().as_bytes())
        .with_context(|| format!("failed to record the project root at {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = fs::metadata(&canonical)
            .with_context(|| format!("failed to stat project root {}", canonical.display()))?
            .dev();
        let dev_path = state_dir.join(DEVICE_FILE);
        fs::write(&dev_path, dev.to_string().as_bytes())
            .with_context(|| format!("failed to record the project device at {}", dev_path.display()))?;
    }
    Ok(())
}

/// The device id [`record_project_root`] wrote, or `None` if there is none
/// (a directory recorded before [`DEVICE_FILE`] existed, a non-Unix platform,
/// an unreadable or unparsable file). `None` is "cannot be judged", the same
/// posture as [`read_project_root`].
pub fn read_root_device(state_dir: &Path) -> Option<u64> {
    fs::read_to_string(state_dir.join(DEVICE_FILE)).ok()?.trim().parse().ok()
}

/// The root [`record_project_root`] wrote, or `None` if this state directory
/// has none.
///
/// `None` is not an error: every directory indexed before `ROOT_FILE` existed
/// is in exactly that state, and callers are expected to treat it as "cannot
/// be judged" rather than to guess. An unreadable or empty file reads the
/// same way, for the same reason - a half-written root is not a fact worth
/// acting on when the action is a delete.
pub fn read_project_root(state_dir: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(state_dir.join(ROOT_FILE)).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// `~/.g-mesh/projects/` (or `$G_MESH_HOME/projects/`), the one directory
/// every project's state lives under. Public because `cli::clean` enumerates
/// it rather than deriving a single project's path from a root the way
/// everything else here does.
pub fn projects_root() -> Result<PathBuf> {
    Ok(projects_root_under(&paths::g_mesh_home()?))
}

/// [`projects_root`] under the g-mesh home `home`.
pub(crate) fn projects_root_under(home: &Path) -> PathBuf {
    home.join(PROJECTS_DIR)
}

/// `~/.g-mesh/projects/<hash>/` for the given (canonicalized) project root.
/// Uses the same hash as the daemon's own socket/pid file location
/// ([`project_hash`]) so the two can never disagree.
pub fn project_dir(root: &Path) -> Result<PathBuf> {
    project_dir_under(&paths::g_mesh_home()?, root)
}

/// [`project_dir`] under the g-mesh home `home` rather than the one the
/// environment names, so a test can point it at a temp dir without writing
/// `G_MESH_HOME`, which every other thread in the process reads.
pub(crate) fn project_dir_under(home: &Path, root: &Path) -> Result<PathBuf> {
    Ok(projects_root_under(home).join(project_hash(root)?))
}

/// The project's state directory, created if absent and recording which
/// project root it belongs to.
///
/// The one place a state directory comes into existence, so that no path can
/// create one without also leaving `project.root` in it - which is what
/// `cli::clean orphaned` reads to tell state whose project was deleted from
/// state whose project is merely idle. `identity::record_project_root` is
/// idempotent, so an existing directory acquires the file the next time
/// anything opens it.
///
/// That claim was false until GM-255. `daemon::run` and
/// `shim::acquire_bootstrap_lock` both created the directory with a bare
/// `fs::create_dir_all` and no root file, and the shim is the *earliest* of
/// the three - so a project whose first contact was a shim got a state
/// directory with no identity. Nothing recovers it later: `project_hash` is
/// one-way, so `clean orphaned` can only class such a directory `Legacy` and
/// leave it alone. Measured on this machine's own test home, 707 of 775
/// directories were unsweepable for that reason.
///
/// So if a fourth call site ever needs a project directory, it goes through
/// here. A bare `create_dir_all` on this path is not a shortcut, it is a
/// directory that can never be cleaned up.
pub fn ensure_project_dir(root: &Path) -> Result<PathBuf> {
    ensure_project_dir_under(&paths::g_mesh_home()?, root)
}

/// [`ensure_project_dir`] under the g-mesh home `home` (see
/// [`project_dir_under`]).
pub(crate) fn ensure_project_dir_under(home: &Path, root: &Path) -> Result<PathBuf> {
    let dir = project_dir_under(home, root)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create project directory {}", dir.display()))?;
    record_project_root(&dir, root)?;
    Ok(dir)
}

/// The temp-then-rename write behind [`write_pid_file`], also used for the phase file.
pub(crate) fn write_state_file_atomic(path: &Path, contents: &str, what: &str) {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if let Err(err) = fs::write(&temporary, contents) {
        crate::log_role!("failed to write {what} {}: {err}", temporary.display());
        return;
    }
    if let Err(err) = fs::rename(&temporary, path) {
        crate::log_role!("failed to put {what} {} in place: {err}", path.display());
        let _ = fs::remove_file(&temporary);
    }
}

// ---- File names. Every state file's name is spelled here and nowhere else in the crate. ----

/// The directory under the g-mesh home that holds every project's state directory.
pub const PROJECTS_DIR: &str = "projects";
/// The project's SQLite index.
pub const INDEX_DB_FILE: &str = "index.db";
/// The per-project config (in a state directory) and the global one (in the home).
pub const CONFIG_FILE: &str = "config.toml";
/// The daemon's stderr, appended to by every daemon a shim spawns for the project.
pub const DAEMON_LOG_FILE: &str = "daemon.log";
/// Unix only: on Windows the endpoint is a pipe name (see `ipc::windows`).
#[cfg(unix)]
pub const SOCKET_FILE: &str = "daemon.sock";
/// The live daemon's pid, next to its socket.
pub const PID_FILE: &str = "daemon.pid";
/// The build the live daemon started from (see `daemon::build_stamp`).
pub const BUILD_STAMP_FILE: &str = "daemon.build";
/// Held by a shim while it bootstraps a daemon (`shim::connect_or_bootstrap`).
pub const BOOTSTRAP_LOCK_FILE: &str = "bootstrap.lock";
/// Held by a running daemon for its whole lifetime: its holder owns the
/// project's socket. Not the bootstrap lock: the shim holds that one while
/// spawning the daemon, so sharing it would deadlock.
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";
/// Where the lock's holder records that it has begun serving. Beside the lock,
/// not inside it: on Windows a second handle cannot read a locked file. Only
/// the holder writes it: after taking the lock, and the next taker clears it.
pub const DAEMON_SERVING_FILE: &str = "daemon.serving";
/// Written by `g-mesh reindex` and `g-mesh init` for as long as they rebuild
/// the project's index: their pid, start time and command, one per line.
pub const REBUILD_MARKER_FILE: &str = "reindex.pid";
/// The cold-start phase, one lowercased `indexing_status::Phase` word. Written
/// on every transition, removed on the way out, so outside readers tell "never
/// indexed" from "building".
pub const PHASE_FILE: &str = "index.phase";
/// The progress counters as JSON, written atomically and throttled by
/// `indexing_status::IndexingStatus`. A killed daemon leaves it behind, so a
/// reader must check its `pid` against the live daemon before trusting it.
pub const PROGRESS_FILE: &str = "index.progress";
/// How the daemon resolved its rerank (`embedding::rerank::OutcomeRecord`).
/// A killed daemon leaves it behind, so a reader checks its `pid` against the
/// live daemon.
pub const RERANK_FILE: &str = "rerank.outcome";
/// The temp file a seed is copied into before its rename to [`INDEX_DB_FILE`].
pub const SEED_TMP_FILE: &str = "index.db.seed-tmp";
/// Under the home: the shim's orphan sweep's clock (mtime) and lock.
pub const ORPHAN_SWEEP_STAMP: &str = "orphan-sweep.stamp";
const PLUGIN_PREFIX: &str = "plugin-";
const PID_SUFFIX: &str = ".pid";
const SUSPENDED_SUFFIX: &str = ".suspended";
const STAGING_PREFIX: &str = "staging-";
const STAGING_SUFFIX: &str = ".db";

/// What a probe answers when the file it probes cannot be opened or locked for
/// a reason other than "not there". Named at every call site: the callers want
/// opposite answers on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnUnreadable {
    /// Unreadable reads as held.
    Held,
    /// Unreadable reads as free.
    Free,
}

/// One project's state directory (`<home>/projects/<project_hash>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir(PathBuf);

impl AsRef<Path> for StateDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// A CLI rebuild's marker, as written: who, since when, and which command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildMarker {
    pub pid: u32,
    pub started_unix_secs: u64,
    pub command: String,
}

impl StateDir {
    /// The state directory of the project at `root` (not created).
    pub fn of(root: &Path) -> Result<Self> {
        project_dir(root).map(Self)
    }

    /// [`StateDir::of`] under the g-mesh home `home`.
    pub(crate) fn of_under(home: &Path, root: &Path) -> Result<Self> {
        project_dir_under(home, root).map(Self)
    }

    /// The state directory of the project at `root`, created and recording its
    /// root (see [`ensure_project_dir`]).
    pub fn ensure(root: &Path) -> Result<Self> {
        ensure_project_dir(root).map(Self)
    }

    /// [`StateDir::ensure`] under the g-mesh home `home`.
    pub(crate) fn ensure_under(home: &Path, root: &Path) -> Result<Self> {
        ensure_project_dir_under(home, root).map(Self)
    }

    /// A directory already known (listing `projects/`), whose root may be gone.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The final component (the project hash): the Windows pipe name.
    pub fn id(&self) -> Option<&str> {
        self.0.file_name()?.to_str()
    }

    pub fn index_db(&self) -> PathBuf {
        self.0.join(INDEX_DB_FILE)
    }

    pub fn config_file(&self) -> PathBuf {
        self.0.join(CONFIG_FILE)
    }

    pub fn daemon_log_file(&self) -> PathBuf {
        self.0.join(DAEMON_LOG_FILE)
    }

    #[cfg(unix)]
    pub fn socket_file(&self) -> PathBuf {
        self.0.join(SOCKET_FILE)
    }

    pub fn pid_file(&self) -> PathBuf {
        self.0.join(PID_FILE)
    }

    /// `plugin-<language>.pid`: one pid file per language.
    pub fn plugin_pid_file(&self, language: &str) -> PathBuf {
        self.0.join(format!("{PLUGIN_PREFIX}{language}{PID_SUFFIX}"))
    }

    /// `plugin-<language>.suspended`: the memory-limit suspension marker, named
    /// like that language's pid file.
    pub fn plugin_suspended_marker(&self, language: &str) -> PathBuf {
        self.0.join(format!("{PLUGIN_PREFIX}{language}{SUSPENDED_SUFFIX}"))
    }

    pub fn build_stamp_file(&self) -> PathBuf {
        self.0.join(BUILD_STAMP_FILE)
    }

    pub fn bootstrap_lock_file(&self) -> PathBuf {
        self.0.join(BOOTSTRAP_LOCK_FILE)
    }

    pub fn daemon_lock_file(&self) -> PathBuf {
        self.0.join(DAEMON_LOCK_FILE)
    }

    pub fn rebuild_marker_file(&self) -> PathBuf {
        self.0.join(REBUILD_MARKER_FILE)
    }

    pub fn phase_file(&self) -> PathBuf {
        self.0.join(PHASE_FILE)
    }

    pub fn progress_file(&self) -> PathBuf {
        self.0.join(PROGRESS_FILE)
    }

    pub fn rerank_outcome_file(&self) -> PathBuf {
        self.0.join(RERANK_FILE)
    }

    pub fn seed_tmp_file(&self) -> PathBuf {
        self.0.join(SEED_TMP_FILE)
    }

    /// Where `language`'s staging index lives (`staging-<language>.db`).
    pub fn staging_db(&self, language: &str) -> PathBuf {
        self.0.join(format!("{STAGING_PREFIX}{language}{STAGING_SUFFIX}"))
    }

    /// Whether a file name in a state directory is a staging index.
    pub fn is_staging_db(file_name: &str) -> bool {
        file_name.starts_with(STAGING_PREFIX) && file_name.ends_with(STAGING_SUFFIX)
    }

    fn serving_file(&self) -> PathBuf {
        self.0.join(DAEMON_SERVING_FILE)
    }

    /// Every `<prefix><language><suffix>` file present, paired with its language,
    /// sorted by language. Empty for a directory that does not exist or cannot
    /// be listed.
    fn plugin_files(&self, suffix: &str) -> Vec<(String, PathBuf)> {
        let mut files = Vec::new();
        let Ok(entries) = fs::read_dir(&self.0) else { return files };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(language) = name.strip_prefix(PLUGIN_PREFIX).and_then(|n| n.strip_suffix(suffix)) {
                files.push((language.to_string(), entry.path()));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    /// Every `plugin-<language>.pid` file present, paired with the language its
    /// name encodes, sorted. For tooling with no running daemon to ask: empty,
    /// not an error, for a directory that does not exist or cannot be listed
    /// ("unreadable means nothing recorded").
    pub fn plugin_pid_files(&self) -> Vec<(String, PathBuf)> {
        self.plugin_files(PID_SUFFIX)
    }

    /// Every `plugin-<language>.suspended` marker present, paired with its
    /// language and the reason written into it, sorted. Independent of
    /// [`StateDir::plugin_pid_files`]: a suspended language has no pid file. A
    /// marker that exists but cannot be read reports `"(unreadable)"`: it is
    /// still evidence something suspended that language.
    pub fn suspended_markers(&self) -> Vec<(String, String)> {
        self.plugin_files(SUSPENDED_SUFFIX)
            .into_iter()
            .map(|(language, path)| {
                let reason = fs::read_to_string(path)
                    .map(|contents| contents.trim().to_string())
                    .unwrap_or_else(|_| "(unreadable)".to_string());
                (language, reason)
            })
            .collect()
    }

    /// Removes every suspension marker. Called while nothing serves the project
    /// (daemon start under the singleton lock, `stop`). Best-effort: a marker
    /// left behind is stale, and the next call clears it.
    pub fn clear_suspension_markers(&self) {
        for (_, path) in self.plugin_files(SUSPENDED_SUFFIX) {
            let _ = fs::remove_file(path);
        }
    }

    /// The root [`record_project_root`] wrote here (see [`read_project_root`]).
    pub fn project_root(&self) -> Option<PathBuf> {
        read_project_root(&self.0)
    }

    /// The device id [`record_project_root`] wrote here (see [`read_root_device`]).
    pub fn root_device(&self) -> Option<u64> {
        read_root_device(&self.0)
    }

    /// Whether anything holds the daemon lock, probed by taking and dropping it.
    /// A missing lock file is not held. `on_unreadable` answers a lock file
    /// that cannot be opened, and (`Held` only) one whose lock cannot be
    /// probed; with `Free` a probe error is returned. `Held` never errs.
    pub fn daemon_lock_held(&self, on_unreadable: OnUnreadable) -> Result<bool> {
        let path = self.daemon_lock_file();
        let file = match File::options().write(true).truncate(false).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Ok(on_unreadable == OnUnreadable::Held),
        };
        match file.try_lock() {
            // Held by nobody: taken here for an instant and released by this drop.
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(err)) => match on_unreadable {
                OnUnreadable::Held => Ok(true),
                OnUnreadable::Free => Err(err).with_context(|| format!("failed to probe {}", path.display())),
            },
        }
    }

    /// The pid the lock's holder recorded once serving. `None` for a missing,
    /// empty, unparseable or newline-less record: all mean "nothing recorded".
    pub fn serving_owner(&self) -> Option<u32> {
        let recorded = fs::read_to_string(self.serving_file()).ok()?;
        recorded.strip_suffix('\n')?.trim().parse().ok()
    }

    /// Records that the lock's holder (`pid`) is now serving. Called after the
    /// socket is bound, never before: a holder with no pid recorded is starting
    /// up and must be left alone; one whose recorded pid is alive while nothing
    /// answers is wedged and can be evicted. The record is newline-terminated and
    /// [`StateDir::serving_owner`] refuses one without it: a reader may signal
    /// the pid it reads, so a partial record must read as nothing. Best-effort:
    /// a failure reads as `Starting`, which is left alone.
    pub(crate) fn record_serving_owner(&self, pid: u32) {
        write_pid_file(&self.serving_file(), pid);
    }

    /// Removes the serving record, so a fresh lock holder does not inherit its
    /// predecessor's claim. Called under the lock, which keeps "only the holder
    /// writes this" true.
    pub(crate) fn clear_serving_owner(&self) {
        let path = self.serving_file();
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => crate::log_role!("failed to clear {}: {err}", path.display()),
        }
    }

    /// The phase word a running daemon published; `None` when no daemon is
    /// running (it removes the file on exit) or none ever ran.
    pub fn read_phase(&self) -> Option<String> {
        fs::read_to_string(self.phase_file()).ok().map(|contents| contents.trim().to_string())
    }

    /// The rebuild marker as written, without judging whether it is live.
    /// `None` for a missing file or one without a pid and a start time.
    pub fn rebuild_marker(&self) -> Option<RebuildMarker> {
        let contents = fs::read_to_string(self.rebuild_marker_file()).ok()?;
        let mut lines = contents.lines();
        let (Some(Ok(pid)), Some(Ok(started_unix_secs))) =
            (lines.next().map(str::parse::<u32>), lines.next().map(str::parse::<u64>))
        else {
            return None;
        };
        let command = lines.next().unwrap_or("reindex").to_string();
        Some(RebuildMarker { pid, started_unix_secs, command })
    }

    /// Records that this process is rebuilding this index on behalf of
    /// `g-mesh <command>`.
    pub(crate) fn write_rebuild_marker(&self, command: &str) -> Result<()> {
        let path = self.rebuild_marker_file();
        let started =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        fs::write(&path, format!("{}\n{started}\n{command}\n", std::process::id()))
            .with_context(|| format!("failed to write the rebuild marker {}", path.display()))
    }

    /// Removes the rebuild marker a crashed `g-mesh reindex`/`init` left behind.
    /// Only a normal daemon calls this, after taking the singleton lock; never
    /// the lock acquisition itself, which the rebuild also goes through.
    pub(crate) fn clear_stale_rebuild_marker(&self) {
        let path = self.rebuild_marker_file();
        match fs::remove_file(&path) {
            Ok(()) => crate::log_role!("removed a stale rebuild marker {}", path.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => crate::log_role!("failed to remove {}: {err}", path.display()),
        }
    }
}

/// Reads a pid from a pid file; `None` for "nothing recorded". See
/// [`read_pid_file_result`] where "could not tell" must differ from that.
pub fn read_pid_file(path: &Path) -> Option<u32> {
    read_pid_file_result(path).ok().flatten()
}

/// Like [`read_pid_file`], but an unreadable file is an error: a caller deciding
/// a deletion must not read "could not tell" as "safe". A present but
/// unparseable file stays `Ok(None)`, which callers depend on.
pub fn read_pid_file_result(path: &Path) -> std::io::Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents.trim().parse().ok()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Records a pid atomically (a per-process temporary renamed into place), so a
/// reader never sees a truncated file, which reads as "no daemon". Best-effort:
/// every reader handles "nothing recorded".
pub fn write_pid_file(path: &Path, pid: u32) {
    write_state_file_atomic(path, &format!("{pid}\n"), "pid file");
}

/// The global config file, directly under the g-mesh home.
pub fn global_config_file(home: &Path) -> PathBuf {
    home.join(CONFIG_FILE)
}

/// The orphan sweep's stamp file, directly under the g-mesh home.
pub fn orphan_sweep_stamp(home: &Path) -> PathBuf {
    home.join(ORPHAN_SWEEP_STAMP)
}

/// Creates `state_dir`'s daemon lock file and makes it unopenable (`chmod
/// 000`). `false` when it still opens (running as root bypasses permissions):
/// the caller then has nothing to test and says so.
#[cfg(all(test, unix))]
#[allow(clippy::disallowed_macros)]
pub(crate) fn make_daemon_lock_unopenable(state_dir: &StateDir) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let path = state_dir.daemon_lock_file();
    File::create(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    if File::options().write(true).open(&path).is_ok() {
        eprintln!("note: {} still opens after chmod 000 (running as root?); skipped", path.display());
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_slash_and_relative_forms_hash_identically_after_canonicalization() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let canonical_hash = project_hash(root).unwrap();
        assert_eq!(canonical_hash, project_hash(root).unwrap(), "the same path hashes identically");

        let with_trailing_slash = root.join("");
        assert_eq!(canonical_hash, project_hash(&with_trailing_slash).unwrap());

        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let via_relative_detour = sub.join("..");
        assert_eq!(canonical_hash, project_hash(&via_relative_detour).unwrap());
    }

    #[test]
    fn a_recorded_root_reads_back_as_the_canonical_path() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();

        record_project_root(state.path(), project.path()).unwrap();

        assert_eq!(read_project_root(state.path()), Some(project.path().canonicalize().unwrap()));
    }

    /// The spellings that [`project_hash`] deliberately collapses have to
    /// collapse here too, or a directory's recorded root would not match the
    /// hash naming it.
    #[test]
    fn a_relative_spelling_of_the_root_is_recorded_canonically() {
        let project = tempfile::tempdir().unwrap();
        let sub = project.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let state = tempfile::tempdir().unwrap();

        record_project_root(state.path(), &sub.join("..")).unwrap();

        assert_eq!(read_project_root(state.path()), Some(project.path().canonicalize().unwrap()));
    }

    /// Every state directory created before this file existed - which is all
    /// of them, on any machine that has run g-mesh before this release.
    #[test]
    fn a_directory_with_no_recorded_root_reads_as_none_rather_than_failing() {
        let state = tempfile::tempdir().unwrap();

        assert_eq!(read_project_root(state.path()), None);
    }

    /// An empty or whitespace-only file is a write that did not finish. It
    /// must read as "unknown", not as the root `""`, which would resolve to
    /// something and be deleted.
    #[test]
    fn a_blank_recorded_root_reads_as_none() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join(ROOT_FILE), "  \n").unwrap();

        assert_eq!(read_project_root(state.path()), None);
    }

    #[test]
    fn distinct_paths_do_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hashes = std::collections::HashSet::new();
        for i in 0..200 {
            let dir = tmp.path().join(format!("project-{i}"));
            std::fs::create_dir(&dir).unwrap();
            let hash = project_hash(&dir).unwrap();
            assert!(hashes.insert(hash), "hash collision at project-{i}");
        }
    }

    /// Test 6 (GM-565): recording a root records the device it is on, a
    /// later record replaces a stale value, and a missing or unparsable file
    /// reads as `None` rather than failing.
    #[cfg(unix)]
    #[test]
    fn recording_a_root_records_its_device() {
        use std::os::unix::fs::MetadataExt;
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let real = std::fs::metadata(project.path()).unwrap().dev();

        assert_eq!(read_root_device(state.path()), None, "nothing recorded yet");

        record_project_root(state.path(), project.path()).unwrap();
        assert_eq!(read_root_device(state.path()), Some(real));

        std::fs::write(state.path().join(DEVICE_FILE), real.wrapping_add(1).to_string()).unwrap();
        record_project_root(state.path(), project.path()).unwrap();
        assert_eq!(read_root_device(state.path()), Some(real), "a stale device was not replaced");

        std::fs::write(state.path().join(DEVICE_FILE), "not a number").unwrap();
        assert_eq!(read_root_device(state.path()), None);
    }

    /// The hash names every project's state directory: a changed value
    /// orphans every existing index. The literal is
    /// `sha256(b"/srv/project").digest()[:8].hex()`.
    #[test]
    fn a_project_hash_never_changes() {
        assert_eq!(hash_of_canonical(Path::new("/srv/project")), "4f3f91f6b0b47e3b");

        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(project_hash(tmp.path()).unwrap(), hash_of_canonical(&tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn an_atomic_state_write_replaces_the_file_whole_and_leaves_no_temp_file() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("daemon.phase");
        std::fs::write(&path, "an older and longer previous content").unwrap();

        write_state_file_atomic(&path, "ready", "phase file");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ready");
        let names: Vec<String> = std::fs::read_dir(state.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["daemon.phase"], "the temp file must be renamed into place, not left beside it");
    }

    // -----------------------------------------------------------------
    // `daemon_lock_held`: each `OnUnreadable` arm
    // -----------------------------------------------------------------

    const ARMS: [OnUnreadable; 2] = [OnUnreadable::Free, OnUnreadable::Held];

    /// No lock file means no daemon, whichever way unreadable reads.
    ///
    /// Control: fold `NotFound` into the unreadable arm: `Held` reads a
    /// missing file as held.
    #[test]
    fn a_missing_daemon_lock_is_not_held_under_either_arm() {
        let dir = tempfile::tempdir().unwrap();
        for arm in ARMS {
            assert!(!StateDir::at(dir.path()).daemon_lock_held(arm).unwrap(), "{arm:?}");
        }
    }

    /// A lock file nobody holds is free and one held on another open file
    /// description is held, under both arms. Each probe gets its own
    /// directory: a probe's own instant lock, inherited by a child another
    /// test thread forks, must not read as held by the next probe.
    #[test]
    fn a_free_daemon_lock_is_free_and_a_held_one_held_under_either_arm() {
        for arm in ARMS {
            let free = tempfile::tempdir().unwrap();
            File::create(StateDir::at(free.path()).daemon_lock_file()).unwrap();
            assert!(!StateDir::at(free.path()).daemon_lock_held(arm).unwrap(), "free, {arm:?}");

            let held = tempfile::tempdir().unwrap();
            let holder = File::create(StateDir::at(held.path()).daemon_lock_file()).unwrap();
            holder.try_lock().unwrap();
            assert!(StateDir::at(held.path()).daemon_lock_held(arm).unwrap(), "held, {arm:?}");
        }
    }

    /// The polarity itself: a lock file that exists but cannot be opened
    /// reads as free under `Free` and as held under `Held`, and neither errs.
    ///
    /// Control: answer the open failure with `!=` instead of `==`: both arms
    /// read the opposite way.
    #[cfg(unix)]
    #[test]
    fn an_unopenable_daemon_lock_reads_as_free_under_free_and_held_under_held() {
        let dir = tempfile::tempdir().unwrap();
        let sd = StateDir::at(dir.path());
        if !make_daemon_lock_unopenable(&sd) {
            return;
        }
        assert!(!sd.daemon_lock_held(OnUnreadable::Free).unwrap(), "Free: an unopenable lock is not held");
        assert!(sd.daemon_lock_held(OnUnreadable::Held).unwrap(), "Held: an unopenable lock is held");
    }

    #[test]
    fn a_staging_db_name_round_trips_and_the_id_is_the_final_component() {
        let dir = tempfile::tempdir().unwrap();
        let sd = StateDir::at(dir.path().join("abc123"));
        let staging = sd.staging_db("rust");
        assert_eq!(staging.parent(), Some(sd.path()));
        assert!(StateDir::is_staging_db(staging.file_name().unwrap().to_str().unwrap()));
        for other in [INDEX_DB_FILE, SEED_TMP_FILE, PID_FILE, "staging-rust.db-journal"] {
            assert!(!StateDir::is_staging_db(other), "{other}");
        }
        assert_eq!(sd.id(), Some("abc123"));
    }

    // -----------------------------------------------------------------
    // Every state file name has one home: this file
    // -----------------------------------------------------------------

    /// Literals spelled outside this file that are not state file names.
    /// `(path under core/src, literal as spelled)`.
    const NOT_STATE_FILES: &[(&str, &str)] = &[
        // A JSON key of `g-mesh status --json`, not the directory.
        ("cli/status/json.rs", "\"projects\""),
    ];

    /// Whether `rel` (under `core/src`, `/`-separated) is a test-only file.
    fn is_test_file(rel: &str) -> bool {
        rel.ends_with("/tests.rs")
            || rel == "tests.rs"
            || rel.ends_with("_tests.rs")
            || rel.split('/').any(|part| part == "tests")
    }

    /// `source` up to its inline `#[cfg(..test..)] mod tests {`, without
    /// `//` comment lines.
    fn production_lines(source: &str) -> Vec<(usize, &str)> {
        let lines: Vec<&str> = source.lines().collect();
        let mut kept = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let next = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
            if trimmed.starts_with("#[cfg(")
                && trimmed.contains("test")
                && (next == "mod tests {" || next == "pub(crate) mod tests {")
            {
                break;
            }
            if !trimmed.starts_with("//") {
                kept.push((i + 1, *line));
            }
        }
        kept
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// Every section 4 name (GM-706 design) is spelled only in this file in
    /// non-test code: a state file renamed here is renamed everywhere.
    ///
    /// Control: spell `"daemon.pid"` in `daemon/mod.rs` again.
    #[test]
    fn every_state_file_name_is_spelled_only_in_state_dir_rs() {
        let mut needles: Vec<String> = [
            PROJECTS_DIR,
            ROOT_FILE,
            DEVICE_FILE,
            INDEX_DB_FILE,
            CONFIG_FILE,
            DAEMON_LOG_FILE,
            PID_FILE,
            BUILD_STAMP_FILE,
            BOOTSTRAP_LOCK_FILE,
            DAEMON_LOCK_FILE,
            DAEMON_SERVING_FILE,
            REBUILD_MARKER_FILE,
            PHASE_FILE,
            PROGRESS_FILE,
            RERANK_FILE,
            SEED_TMP_FILE,
            ORPHAN_SWEEP_STAMP,
        ]
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect();
        #[cfg(unix)]
        needles.push(format!("\"{SOCKET_FILE}\""));
        needles.push(format!("\"{PLUGIN_PREFIX}"));
        needles.push(format!("\"{STAGING_PREFIX}"));
        needles.push(format!("{SUSPENDED_SUFFIX}\""));

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        let mut offenders = Vec::new();
        for file in files {
            let rel = file.strip_prefix(&src).unwrap().to_string_lossy().replace('\\', "/");
            if rel == "paths/state_dir.rs" || is_test_file(&rel) {
                continue;
            }
            let source = fs::read_to_string(&file).unwrap();
            for (number, line) in production_lines(&source) {
                for needle in &needles {
                    if line.contains(needle.as_str())
                        && !NOT_STATE_FILES.contains(&(rel.as_str(), needle.as_str()))
                    {
                        offenders.push(format!("{rel}:{number}: {needle}"));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "state file names spelled outside paths/state_dir.rs:\n{}",
            offenders.join("\n")
        );
    }
}
