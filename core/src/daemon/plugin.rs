//! Spawns a language plugin as a child process from its manifest, performs
//! its handshake, and gives the rest of the daemon a way to route
//! `FileChanged` requests to it and apply the diff it answers with - the
//! missing link between the daemon (Rust) and a plugin process (for the
//! bundled one: `node` plus a compiled script in a checkout, a self-contained
//! executable that carries its own runtime in a release archive - see
//! `launch_command_for`; for any other, whatever a discovered manifest's
//! `command`/`args` name).
//!
//! [`PluginProcess`]/[`PluginState`] here are generic over any one plugin;
//! discovery and per-language routing live in `daemon::manifest`
//! (`~/.g-mesh/plugins/<language>/plugin.toml` + the bundled root) and
//! `daemon::registry::PluginRegistry`, which spawns one `PluginProcess` per
//! discovered language, lazily, the first time anything needs it. This
//! module keeps a narrower "the bundled JS/TS plugin specifically" surface
//! too - [`bundled_manifest`], [`plugin_entry_path`], [`PLUGIN_PATH_ENV`],
//! [`bundled_fingerprint`] - for the handful of callers that genuinely mean
//! that one plugin rather than whatever the registry has discovered:
//! `daemon::build_stamp` compares one *running daemon's* JS/TS build against
//! another's (a question about this install, not about what filled any one
//! project's index), and a couple of tests want a bare manifest without a
//! `plugin.toml` fixture. See each item's own doc comment for why it is not
//! dead code.
//!
//! # Crash detection and lazy relaunch
//!
//! An *unexpected* plugin exit - a panic, an OOM kill, anything that is not
//! the plugin choosing to stop - is a different problem from the deliberate
//! idle-sleep this daemon may grow later (see task 38): there, the daemon
//! decides to let the plugin go and knows exactly when it will need it back;
//! here, the plugin is just gone and the daemon finds out the hard way, mid
//! request. Leaving that silently broken until someone notices indexing has
//! stopped and restarts the daemon by hand is the failure mode this module
//! avoids: [`PluginProcess::apply_file_change`] treats a dead process as
//! recoverable, not fatal - it spawns a fresh one and replays whatever file
//! paths were still pending against it, transparently to the caller besides
//! the added latency of doing so.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::daemon::manifest::{Capabilities, PluginManifest, WorkspaceConfig};
use crate::embedding::EmbeddingPipeline;
use crate::protocol::handshake;
use crate::protocol::jsonrpc::{is_timeout, write_message};
use crate::protocol::types::{
    ControlEnvelope, ControlMessage, RequestId, CURRENT_PROTOCOL_VERSION, JSONRPC_VERSION,
};
use crate::watcher::apply::{apply_file_change as apply_file_change_diff, apply_semantic_pass};
use crate::watcher::staleness::{self, StalenessOutcome};

/// Overrides where the plugin's compiled entry point lives. Real installs
/// never need this - the default already resolves to the bundled plugin -
/// but it lets the integration test suite point at a build without
/// depending on the daemon binary's own install location.
pub const PLUGIN_PATH_ENV: &str = "G_MESH_JS_TS_PLUGIN_PATH";

/// How often [`PluginProcess::shutdown`] checks whether the plugin has taken
/// the hint and exited.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long a relaunch gives the process it replaced to exit on its closed
/// stdin before killing it. Only a live process ever waits this out - one
/// that crashed, or that a timeout already killed, is gone or going - and
/// nothing is blocked meanwhile, since the replacement is serving before the
/// wait starts.
const RELAUNCH_GRACE: Duration = Duration::from_secs(2);

/// Per-method budgets for one control-plane round trip - see
/// `docs/architecture/multi-language-plugins.md`'s "Semantic engine hangs or
/// is slow" failure mode, and `watcher::apply::round_trip`/
/// `protocol::jsonrpc::read_message_with_timeout` for the mechanism that
/// enforces them. Three separate fields, not one shared timeout, because each
/// covers a different amount of plugin work - and, as of this task's review
/// round, two very different evidence bars: [`file_changed`](Self::file_changed)
/// and [`semantic_pass_file`](Self::semantic_pass_file) below are reasoned
/// from first principles, not measured; [`semantic_pass_project`]
/// (Self::semantic_pass_project) is measured, because a flat guess there was
/// tried first and turned out wrong (see its own section).
///
/// - **`file_changed`**: one file's structural (tree-sitter) reparse plus its
///   incremental diff - no type-checking, no cross-file work. **Not
///   measured** - no per-file timing exists anywhere in this repo's indexes
///   or bench output. The reasoning: even a large single file is
///   milliseconds of parsing, so [`DEFAULT_FILE_CHANGED_TIMEOUT`] (30s)
///   leaves roughly two orders of magnitude of slack for OS scheduling noise
///   and a loaded CI box before calling the plugin wedged rather than merely
///   slow. Treat this as a guess with a wide margin, not a validated budget.
/// - **`semantic_pass_file`**: the per-file semantic upgrade that rides on
///   the very same reparse (`watcher::apply::apply_file_change`'s own doc
///   comment). **Not measured either** - same gap: the bench indexes this
///   task's `semantic_pass_project` numbers came from only record the
///   *whole-project* pass's timestamps (`meta.bulkIndexedAt` /
///   `meta.semanticPassAt`), never a per-file one. The reasoning: real
///   compiler work against an already-warm tsserver, scoped to the handful of
///   unresolved sites one file could plausibly have introduced, matching the
///   per-request budget `docs/architecture/multi-language-plugins.md`'s
///   `LspBridge` section already calls for ("Budgets: per-request
///   timeout..."). [`DEFAULT_SEMANTIC_PASS_FILE_TIMEOUT`] (120s) is longer
///   than the structural budget because it is real type-checking, not
///   parsing, but still bounded to one file's worth of it - again, a guess
///   with margin, not a validated number.
/// - **`semantic_pass_project`**: the whole-project pass
///   (`daemon::semantic::run_with_registry`/`run_once`), which type-checks
///   across every file in the project at once, cold. This one *is* measured,
///   and the first version of this comment was wrong to say no measurement
///   existed - it undersold what `meta.bulkIndexedAt` -> `meta.semanticPassAt`
///   deltas across a project's own index already record. Source: those
///   deltas, read across every g-mesh-bench index under `~/.g-mesh/projects`
///   as of 2026-09-15:
///
///   | corpus | n | min | p50 | p90 | max |
///   |---|---|---|---|---|---|
///   | excalidraw (658 `File` nodes) | 47 | 56s | 90s | ~163-174s (method-dependent) | **1475s (24.6min)**, under heavy contention from parallel bench runs sharing the machine; other tail samples 778s, 261s, 254s |
///   | task-tracker-mcp (49 `File` nodes) | 93 | 0s | 1s | 2s | 9s |
///
///   The one number that matters most: a real, healthy pass on a
///   medium-sized repo (excalidraw, 658 files) *already exceeded 20 minutes
///   once*, even before scaling to a larger corpus. A flat 20-minute timeout
///   would have killed that exact pass, relaunched the plugin, and had the
///   next attempt likely die the same way - a project that never finishes
///   its semantic layer. [`DEFAULT_SEMANTIC_PASS_PROJECT_TIMEOUT`] therefore
///   is not the timeout used directly; it is the **floor** a per-project
///   scaled budget is clamped to, via [`RoundTripTimeouts::
///   semantic_pass_project_timeout`]: `max(floor, file_count *
///   `[`SEMANTIC_PASS_PER_FILE_BUDGET`]`)`.
///     - **Floor: 20 minutes**, kept exactly at its pre-review value - it is
///       what small/empty projects (few or zero `File` nodes) still get, and
///       nothing in the measurement above argues for changing it.
///     - **Per-file budget: 10 seconds.** excalidraw's worst observed rate
///       under contention is 1475s / 658 files ~= 2.24s/file; 10s/file is
///       ~4.5x that. Its p50 rate is 90s / 658 files ~= 0.137s/file; 10s/file
///       is ~73x that. Both margins are deliberately generous: this budget
///       has to cover a repo this daemon has never actually measured, not
///       just replay the corpora above.
///
///   Task-tracker-mcp's own numbers (max 9s, 49 files) are there to show the
///   floor is not accidentally starving a small project - 49 * 10s = 490s is
///   already far below the 20-minute floor, so the floor is what it gets, and
///   9s of real work disappears into that floor with room to spare.
///
///   File-count scaling is computed **per language**
///   (`daemon::semantic::indexed_file_count`, narrowed by GM-270's
///   per-language semantic scheduler to `WHERE kind = 'File' AND language =
///   ?` over `nodes.language`, GM-264's column): each language's own
///   whole-project pass gets a budget sized off its own files, never
///   inflated by an unrelated language's files sitting in the same project,
///   and never starved by them either. Before GM-270 there was no
///   per-language `File`-node accounting to narrow this against, so a
///   multi-language project's single (bundled-plugin-only) pass was sized
///   off every discovered language's files combined - strictly more
///   generous than a single language needed, never less, which is why that
///   was safe to ship ahead of the narrowing rather than a correctness bug
///   in its own right.
///
/// Constants, not configuration (see the task this type was added for): a
/// project's `config.toml` has no `[plugin.timeouts]` section, and none of
/// these numbers are meant to be a per-project tuning knob. The only thing
/// that overrides them is the matching `*_TIMEOUT_ENV` variable
/// [`RoundTripTimeouts::from_env`] reads - the same "a test can drive the
/// real timer instead of faking the subsystem around it" escape hatch
/// `daemon::lifecycle::PLUGIN_IDLE_ENV` already documents, and for the same
/// reason: nobody wants a unit test to actually wait 20 minutes (or longer,
/// once file-count scaling is in play) to prove a timeout fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundTripTimeouts {
    pub file_changed: Duration,
    pub semantic_pass_file: Duration,
    /// The **floor** a real whole-project semanticPass timeout is clamped
    /// to, not the timeout itself. See
    /// [`RoundTripTimeouts::semantic_pass_project_timeout`] for the actual
    /// per-project value, and this type's own doc comment for the
    /// measurement behind both the floor and the per-file budget it is
    /// combined with.
    pub semantic_pass_project: Duration,
}

/// See [`RoundTripTimeouts`]'s doc comment for the evidence behind this
/// number.
pub const DEFAULT_FILE_CHANGED_TIMEOUT: Duration = Duration::from_secs(30);
/// See [`RoundTripTimeouts`]'s doc comment for the evidence behind this
/// number.
pub const DEFAULT_SEMANTIC_PASS_FILE_TIMEOUT: Duration = Duration::from_secs(120);
/// See [`RoundTripTimeouts`]'s doc comment for the evidence behind this
/// number. The floor for [`RoundTripTimeouts::semantic_pass_project_timeout`],
/// not a timeout used as-is - see that method.
pub const DEFAULT_SEMANTIC_PASS_PROJECT_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// How much whole-project semanticPass budget one indexed `File` node buys -
/// see [`RoundTripTimeouts`]'s doc comment ("Per-file budget: 10 seconds")
/// for the measurement this is derived from.
pub const SEMANTIC_PASS_PER_FILE_BUDGET: Duration = Duration::from_secs(10);

/// Test-only override for [`RoundTripTimeouts::file_changed`] - real installs
/// never set it. See [`RoundTripTimeouts`]'s doc comment.
pub const FILE_CHANGED_TIMEOUT_ENV: &str = "G_MESH_FILE_CHANGED_TIMEOUT_MS";
/// Test-only override for [`RoundTripTimeouts::semantic_pass_file`].
pub const SEMANTIC_PASS_FILE_TIMEOUT_ENV: &str = "G_MESH_SEMANTIC_PASS_FILE_TIMEOUT_MS";
/// Test-only override for [`RoundTripTimeouts::semantic_pass_project`] - the
/// floor, not the scaled timeout (there is no override for the per-file
/// budget; tests that need a short whole-project timeout pass a small
/// `file_count` too, since the floor is what dominates at small counts).
pub const SEMANTIC_PASS_PROJECT_TIMEOUT_ENV: &str = "G_MESH_SEMANTIC_PASS_PROJECT_TIMEOUT_MS";

impl Default for RoundTripTimeouts {
    fn default() -> Self {
        Self {
            file_changed: DEFAULT_FILE_CHANGED_TIMEOUT,
            semantic_pass_file: DEFAULT_SEMANTIC_PASS_FILE_TIMEOUT,
            semantic_pass_project: DEFAULT_SEMANTIC_PASS_PROJECT_TIMEOUT,
        }
    }
}

impl RoundTripTimeouts {
    /// The production defaults, unless a test-only env override names
    /// something else - see this type's own doc comment.
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            file_changed: parse_round_trip_timeout(
                std::env::var(FILE_CHANGED_TIMEOUT_ENV).ok().as_deref(),
                default.file_changed,
                FILE_CHANGED_TIMEOUT_ENV,
            ),
            semantic_pass_file: parse_round_trip_timeout(
                std::env::var(SEMANTIC_PASS_FILE_TIMEOUT_ENV).ok().as_deref(),
                default.semantic_pass_file,
                SEMANTIC_PASS_FILE_TIMEOUT_ENV,
            ),
            semantic_pass_project: parse_round_trip_timeout(
                std::env::var(SEMANTIC_PASS_PROJECT_TIMEOUT_ENV).ok().as_deref(),
                default.semantic_pass_project,
                SEMANTIC_PASS_PROJECT_TIMEOUT_ENV,
            ),
        }
    }

    /// The timeout to actually give a whole-project semanticPass over a
    /// project with `file_count` indexed `File` nodes: `self
    /// .semantic_pass_project` as a floor, and `file_count *
    /// `[`SEMANTIC_PASS_PER_FILE_BUDGET`]` as a per-project budget, whichever
    /// is larger - see [`RoundTripTimeouts`]'s doc comment for the
    /// measurement behind both numbers.
    ///
    /// `file_count` is clamped to `u32::MAX` before multiplying (a project
    /// with that many files is not one this daemon has ever seen, and
    /// overflowing the multiplication into a wildly wrong, possibly tiny,
    /// timeout would be a worse failure than merely capping the input).
    pub fn semantic_pass_project_timeout(&self, file_count: usize) -> Duration {
        let file_count = u32::try_from(file_count).unwrap_or(u32::MAX);
        let scaled = SEMANTIC_PASS_PER_FILE_BUDGET.saturating_mul(file_count);
        self.semantic_pass_project.max(scaled)
    }
}

/// `default` for an absent or unparseable override, and - unlike
/// `daemon::lifecycle::parse_timeout`'s idle timers - also for an explicit
/// `0`: an idle timer can mean "never" at zero, but a round-trip timeout of
/// zero would fail every request, which is not a meaningful "off" state for
/// this type to offer.
fn parse_round_trip_timeout(raw: Option<&str>, default: Duration, name: &str) -> Duration {
    let Some(raw) = raw else { return default };
    match raw.trim().parse::<u64>() {
        Ok(0) => default,
        Ok(millis) => Duration::from_millis(millis),
        Err(_) => {
            eprintln!(
                "g-mesh daemon: ignoring {name}={raw:?} - not a whole number of milliseconds; \
                 using the default {default:?}"
            );
            default
        }
    }
}

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

/// Directory names skipped while fingerprinting *every* plugin, regardless of
/// what its own manifest says - see `docs/architecture/plugin-modularity.md`'s
/// Data Model section ("Built-in baseline ignore"). A manifest's own
/// `fingerprint_ignore` extends this list; it never replaces it.
const BASELINE_FINGERPRINT_IGNORE: &[&str] =
    &[".git", "node_modules", "__pycache__", ".venv", "venv", ".pytest_cache"];

/// File name of the bundled plugin's single-executable build, as
/// `scripts/bundle-plugin.sh` stages it into a release archive. The two must
/// agree: this is how a binary that was compiled somewhere else entirely finds
/// the plugin sitting next to it.
const BUNDLED_PLUGIN_EXE: &str =
    if cfg!(windows) { "g-mesh-plugin-typescript.exe" } else { "g-mesh-plugin-typescript" };

/// Where this install's bundled JS/TS plugin is, in precedence order:
///
/// 1. [`PLUGIN_PATH_ENV`], for the test suite and anyone pointing a binary at
///    a plugin build of their own.
/// 2. The single-executable plugin a release archive unpacks beside the core
///    binary (`<exe dir>/plugins/typescript/g-mesh-plugin-typescript`). Probed
///    for existence rather than assumed, because it is exactly what a checkout
///    does not have.
/// 3. The compiled entry point in this repo's own tree, baked in at compile
///    time - `core/` and `plugins/typescript/` are sibling directories here.
///
/// The order matters in only one direction: an installed layout has no
/// `CARGO_MANIFEST_DIR` to resolve, and a checkout has no executable-adjacent
/// `plugins/`, so on any real machine at most one of (2) and (3) exists. Where
/// both somehow do, the artifact that shipped with the running binary wins -
/// see `daemon::manifest::bundled_roots`, which orders its roots the same way
/// for the same reason.
pub(crate) fn plugin_entry_path() -> PathBuf {
    if let Ok(over) = std::env::var(PLUGIN_PATH_ENV) {
        return PathBuf::from(over);
    }
    if let Some(installed) = installed_plugin_executable() {
        return installed;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript/dist/src/index.js")
}

/// The bundled plugin executable in an installed layout, if this really is
/// one. `None` in a checkout, which is the case that falls through to the
/// compile-time path above.
fn installed_plugin_executable() -> Option<PathBuf> {
    let candidate =
        crate::daemon::manifest::installed_bundled_root()?.join(BUNDLED_LANGUAGE).join(BUNDLED_PLUGIN_EXE);
    candidate.is_file().then_some(candidate)
}

/// How to launch whatever [`plugin_entry_path`] resolved to, as a
/// `(command, args)` pair.
///
/// A script needs an interpreter and an executable must not have one, and the
/// difference is decided by the entry's own extension rather than by which
/// branch above produced it - which is what keeps [`PLUGIN_PATH_ENV`] working
/// for both. Pointing it at a `dist/src/index.js` (what every test that sets
/// it does) still spawns `node`; pointing it at a single-executable build
/// spawns that build directly, with no Node.js needed on the machine at all.
///
/// `node` stays a bare command so `std::process::Command` looks it up on
/// `$PATH` at spawn time, matching how `daemon::manifest` resolves a bare
/// `command` in a `plugin.toml`.
fn launch_command_for(entry: &Path) -> (PathBuf, Vec<String>) {
    let is_script = entry
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "js" | "cjs" | "mjs"));

    if is_script {
        (PathBuf::from("node"), vec![entry.to_string_lossy().into_owned()])
    } else {
        (entry.to_path_buf(), Vec::new())
    }
}

/// The bundled plugin's wire identifier - `Handshake.language`
/// (`plugins/typescript/src/index.ts`), and since task 155 renamed
/// `plugins/js-ts/` to `plugins/typescript/`, its manifest's `language` and
/// its directory name too: the bundled plugin is discovered through the
/// exact same "directory name is the manifest's language" rule as any other
/// plugin, no special case retained (see
/// `docs/architecture/plugin-modularity.md`'s Options Considered #1).
///
/// A constant rather than a literal at each use site because it names two
/// genuinely different things that must never drift apart: the language
/// [`bundled_manifest`] hands out below, and the pid-file name
/// `daemon::plugin_pid_path` still resolves for the test suite and the few
/// other callers that predate per-language pid files and only ever meant
/// "the bundled JS/TS plugin's pid" (see that function's doc comment).
/// Production code that has to be genuinely multi-language-aware
/// (`cli::status`, `cli::stop`, `cli::clean`) never references this constant -
/// it lists every `plugin-<language>.pid` file `PluginRegistry` writes
/// instead of assuming this one.
pub const BUNDLED_LANGUAGE: &str = "typescript";

/// A [`PluginManifest`] describing the bundled JS/TS plugin as this install
/// would spawn it - [`plugin_entry_path`]'s resolved entry point plus whatever
/// `launch_command_for` says runs it, honoring the same [`PLUGIN_PATH_ENV`]
/// override - with nothing read from an actual `plugin.toml`.
///
/// Still used after `daemon::run` moved to `daemon::registry::PluginRegistry`
/// (task 155) and after the index's generation string stopped being keyed off
/// this one bundled-plugin view (task 163 - see
/// `daemon::registry::indexer_version`): [`bundled_fingerprint`] below is what
/// `daemon::build_stamp` compares one *running daemon's* JS/TS plugin against
/// another's, which is a different question from "what filled this index" and
/// is deliberately still asked of the bundled plugin alone; and a few tests
/// (`plugin_crash_recovery.rs`) still want a bare [`PluginManifest`] for the
/// bundled plugin without going through a `plugin.toml` fixture.
pub fn bundled_manifest() -> PluginManifest {
    let entry = plugin_entry_path();
    let manifest_dir = entry.parent().map(Path::to_path_buf).unwrap_or_default();
    let (command, args) = launch_command_for(&entry);
    PluginManifest {
        language: BUNDLED_LANGUAGE.to_string(),
        protocol_version: CURRENT_PROTOCOL_VERSION,
        plugin_version: String::new(),
        command,
        args,
        extensions: Vec::new(),
        fingerprint_ignore: Vec::new(),
        manifest_dir,
        // The bundled plugin's real capabilities/workspace live in
        // `plugins/typescript/plugin.toml`, not here - this helper predates
        // both fields and exists only for tests that need a bare manifest,
        // so the conservative defaults are the right stand-in rather than
        // duplicating the real manifest's values.
        capabilities: Capabilities::default(),
        workspace: WorkspaceConfig::default(),
    }
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
/// A plugin's dependency/VCS junk - `node_modules`, `.git`, and the rest of
/// [`BASELINE_FINGERPRINT_IGNORE`] (plus anything a manifest's own
/// `fingerprint_ignore` adds) - is walked but skipped by name, not hashed:
/// those directories are large, are not what a plugin's own build emits, and
/// walking them on every shim start would turn a sub-millisecond check into a
/// directory crawl. A dependency upgrade that changes extraction is therefore
/// still a manual [`CURRENT_INDEXER_VERSION`](crate::storage::schema::CURRENT_INDEXER_VERSION)
/// bump, which is exactly what that constant remains for - the two halves are
/// complementary, not redundant.
pub fn fingerprint(manifest: &PluginManifest) -> String {
    digest_of_plugin_build(&manifest.manifest_dir, &manifest.fingerprint_ignore).unwrap_or_else(|err| {
        eprintln!(
            "g-mesh: could not fingerprint the {} plugin at {}: {err:#} - \
                 a change to its extraction logic will not be noticed",
            manifest.language,
            manifest.manifest_dir.display()
        );
        FINGERPRINT_UNAVAILABLE.to_string()
    })
}

/// [`fingerprint`] for [`bundled_manifest`], memoized for the process's
/// lifetime - what `build_stamp::of_running_process` needs, and since task 163
/// its only caller.
///
/// It is deliberately *not* what stamps the index any more: that is
/// `daemon::registry::indexer_version`, a digest over every *discovered*
/// plugin's fingerprint, because with N plugins an index is only as current as
/// the least current of the builds that filled it. The two questions differ in
/// what they are for. A build stamp compares one running daemon against
/// another so a shim can decide whether to retire the incumbent, and the
/// bundled JS/TS plugin is the part of a daemon's build that
/// [`PLUGIN_PATH_ENV`] can redirect out from under it; the index's generation
/// is about content that is already stored.
///
/// [`fingerprint`] itself does not cache - `daemon::registry` computes one
/// fingerprint per discovered plugin, and owns that concern for its own set of
/// manifests rather than leaving a process-wide `OnceLock` to answer for all
/// of them.
///
/// Computed once per process: the shim asks for it on every call it makes,
/// and the answer cannot change under a running process in any way that
/// would matter (the plugin a daemon already spawned is the one it keeps).
pub fn bundled_fingerprint() -> &'static str {
    static FINGERPRINT: OnceLock<String> = OnceLock::new();
    FINGERPRINT.get_or_init(|| fingerprint(&bundled_manifest()))
}

/// Digests every regular file under `dir`, skipping any subdirectory whose
/// name is in [`BASELINE_FINGERPRINT_IGNORE`] or `ignore`, in a stable order.
///
/// The whole directory rather than just an entry point's siblings: a plugin's
/// own build output can be laid out however it likes, and the file most
/// likely to change what the graph looks like is not necessarily the entry
/// file. Each file's path and length go into the digest alongside its bytes,
/// so moving code between two files cannot leave the concatenation unchanged.
fn digest_of_plugin_build(dir: &Path, ignore: &[String]) -> Result<String> {
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

/// Finishes `hasher` the one way this daemon renders a digest: lowercase hex,
/// cut to [`FINGERPRINT_HEX_CHARS`].
///
/// Shared with `daemon::registry::indexer_version`, which hashes every
/// discovered plugin's fingerprint into a single digest of its own. Two
/// digests that mean different things may as well look alike - both are read
/// by humans comparing a build stamp or a `meta.indexer_version` at a glance -
/// and a second, subtly different truncation convention invented next door is
/// exactly the kind of drift a shared helper costs nothing to rule out.
pub(crate) fn truncated_hex(hasher: Sha256) -> String {
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
/// in [`BASELINE_FINGERPRINT_IGNORE`] or `ignore`. Recursive rather than one
/// flat `read_dir` so a plugin laid out in subdirectories does not silently
/// fall outside the fingerprint - a blind spot in this function is a wrong
/// answer served later, which is the exact failure it exists to prevent.
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

struct PluginIo {
    reader: BufReader<ChildStdout>,
    writer: ChildStdin,
}

/// The live child process plus its handshake-verified pipes, as one unit -
/// everything a relaunch has to replace together so a caller never observes
/// a `child` and an `io` that belong to two different processes.
struct PluginState {
    // Kept alive so the child is not dropped (and its pipes closed) while
    // still in use; only its pid is ever read, never its exit status, except
    // by `PluginProcess::process_has_exited`'s non-blocking crash check. A
    // daemon that is killed outright still needs nothing from it: the OS
    // closes the daemon's end of the child's stdin, which the plugin already
    // treats as its cue to exit (see index.ts's stdin "end" handler). What
    // [`PluginProcess::shutdown`] adds is the *deliberate* ending of a plugin
    // whose core carries on - a sleep on the idle timeout - where nothing
    // closes those pipes unless this process says so.
    child: Child,
    io: PluginIo,
}

/// Catches a node-launched plugin's missing entry point before it is ever
/// spawned, instead of letting `Command::spawn` succeed on `node` itself
/// (genuinely on `$PATH`) only for node to exit before the handshake once it
/// can't find the script. That latter shape - `spawn()` succeeds,
/// `handshake::perform` then reports "plugin closed its stdout before
/// sending a handshake" - is exactly what an unbuilt bundled JS/TS plugin
/// looks like (`core/build.rs` downgrades a failed `npm run build` to a
/// warning, so `dist/src/index.js` can genuinely not exist here), and that
/// message names the symptom, never the missing build. Checked here, not by
/// making `handshake::perform` itself smarter, because only the caller
/// knows whether this was even a node script to begin with - a non-node
/// plugin (the Go binary, a third-party SDK plugin) that fails its
/// handshake for some other reason must keep getting that other reason.
///
/// Not limited to the bundled TypeScript plugin by name: the remedy named
/// below (`npm ci && npm run build`, run in the entry's own npm package) is
/// equally correct for any node-based plugin discovered from a
/// `plugin.toml`, found by walking up from the entry for the nearest
/// `package.json` rather than assuming `plugins/typescript`'s exact layout.
///
/// `pub(crate)` rather than private: `daemon::manifest`'s own
/// `the_bundled_plugins_handshake_reports_the_version_its_manifest_declares`
/// test spawns the bundled plugin directly (it has to - it is checking the
/// handshake against a manifest read from disk, not through this module's
/// `PluginManifest`) and hits the exact same unbuilt-plugin failure; reusing
/// this check there keeps both call sites naming the same cause and the
/// same fix instead of the generic message drifting back in on one side.
pub(crate) fn missing_node_entry_hint(command: &Path, args: &[String]) -> Option<String> {
    let is_node = command.file_stem().and_then(|stem| stem.to_str()).is_some_and(|stem| stem == "node");
    let entry = Path::new(args.first()?);
    if !is_node || entry.is_file() {
        return None;
    }
    let package_dir = entry.ancestors().find(|dir| dir.join("package.json").is_file());
    Some(match package_dir {
        Some(dir) => format!(
            "the plugin's entry point {} does not exist - it has not been built yet. Run `npm ci && npm run \
             build` in {}",
            entry.display(),
            dir.display()
        ),
        None => format!(
            "the plugin's entry point {} does not exist - it has not been built yet (run `npm ci && npm run \
             build` in its package directory)",
            entry.display()
        ),
    })
}

/// Catches a cargo-workspace-built plugin's missing binary before it is ever
/// spawned - the same shape of problem [`missing_node_entry_hint`] catches
/// for a node-launched one, just on the other side of the check. There the
/// *launcher* (`node`) genuinely exists and the *entry* named in `args` is
/// what's missing; here there is no separate launcher - `command` itself is
/// the plugin binary - so it is `command`, not an argument, that has to be
/// tested for existence. `Command::spawn` on a missing `command` fails with a
/// bare `No such file or directory (os error 2)`, naming neither cargo nor
/// the fact that this is a build output at all, which is exactly the symptom
/// traced while verifying GM-301: a `cargo test -p g-mesh` run that never
/// built sibling workspace members failed the daemon's cold-start walk with
/// that raw OS error, four tests deep before the daemon log (not the test
/// failure itself) named the real cause.
///
/// Triggered by the path shape both bundled cargo-workspace plugins declare
/// today (`plugins/python/plugin.toml`, `plugins/rust/plugin.toml`:
/// `../../target/debug/g-mesh-plugin-<language>`, resolved by
/// `daemon::manifest::resolve_path_entry` against the manifest's own
/// directory) rather than by name, so a future third plugin that joins the
/// cargo workspace the same way is covered without this function learning
/// its name - the same "generic over the mechanism, not the specific
/// plugin" choice `missing_node_entry_hint`'s own doc comment makes for node.
/// A binary built by some other means that merely happens to live under a
/// directory named `target/debug` or `target/release` would false-positive
/// on this check; nothing in this codebase does that today, and the
/// generic spawn-failure message below is still correct if one ever did.
///
/// Names `cargo build --workspace` specifically, not a per-crate `cargo
/// build -p <name>`: the daemon's cold-start walk (`daemon::bulk_index::run`)
/// spawns every *discovered* plugin unconditionally, regardless of which
/// languages the project being indexed actually contains (see that module's
/// own doc comment), so a checkout missing even one workspace-built plugin's
/// binary needs the whole-workspace build to get unstuck, not just the one
/// crate the failure happened to name.
///
/// # Which spelling the message names
///
/// `command` reaching this function already went through
/// `manifest::resolve_exe_suffix` once, at manifest-read time - so on a
/// platform with a non-empty [`std::env::consts::EXE_SUFFIX`] (Windows),
/// `command` is extensionless *here* only when neither the unsuffixed nor
/// the suffixed spelling exists (if the suffixed one did, resolution would
/// already have switched to it). Reporting `command` as written in that case
/// would name a spelling cargo never produces on that platform at all -
/// wrong in a different way than the bare `No such file or directory` this
/// function exists to replace, not an improvement over it. So this
/// re-derives the suffixed spelling via [`manifest::exe_suffixed`] (the same
/// pure decision `resolve_exe_suffix` itself uses) purely for the message,
/// independent of whether that file exists - it's the name a real
/// `cargo build --workspace` on this platform would actually produce, and
/// therefore the one a spawn attempt is actually missing.
pub(crate) fn missing_workspace_binary_hint(command: &Path) -> Option<String> {
    missing_workspace_binary_hint_with_suffix(command, std::env::consts::EXE_SUFFIX)
}

/// [`missing_workspace_binary_hint`]'s logic with the platform suffix taken
/// as a parameter rather than read from [`std::env::consts::EXE_SUFFIX`]
/// internally, so a test can exercise the Windows arm (`".exe"`) from any
/// host.
fn missing_workspace_binary_hint_with_suffix(command: &Path, suffix: &str) -> Option<String> {
    if command.is_file() {
        return None;
    }
    let profile_dir = command.parent()?;
    let profile = profile_dir.file_name().and_then(|name| name.to_str())?;
    if profile != "debug" && profile != "release" {
        return None;
    }
    let target_dir = profile_dir.parent()?;
    if target_dir.file_name().and_then(|name| name.to_str()) != Some("target") {
        return None;
    }
    let workspace_root = target_dir.parent().filter(|root| root.join("Cargo.toml").is_file());

    let named =
        crate::daemon::manifest::exe_suffixed(command, suffix).unwrap_or_else(|| command.to_path_buf());

    Some(match workspace_root {
        Some(root) => format!(
            "the plugin binary {} does not exist - it has not been built yet. Run `cargo build --workspace` in {}",
            named.display(),
            root.display()
        ),
        None => format!(
            "the plugin binary {} does not exist - it has not been built yet (run `cargo build --workspace` in \
             the repository root)",
            named.display()
        ),
    })
}

/// Every "is this plugin's binary simply missing" check this module knows,
/// tried in turn - the one entry point both spawn sites
/// ([`PluginState::spawn`] and `daemon::bulk_index::walk_one_language`) call,
/// so a third such check joins both call sites by joining this function
/// rather than by becoming a third thing each spawn site has to remember to
/// call. [`missing_node_entry_hint`] and [`missing_workspace_binary_hint`]
/// never both match the same manifest - the former requires `command` itself
/// to be `node` (found on `$PATH`, so never "missing"), the latter requires
/// `command` itself to be the thing that's missing - so trying both costs
/// nothing on the manifest that matches neither.
pub(crate) fn missing_plugin_binary_hint(command: &Path, args: &[String]) -> Option<String> {
    missing_node_entry_hint(command, args).or_else(|| missing_workspace_binary_hint(command))
}

impl PluginState {
    /// Spawns `manifest`'s plugin for `project_root` and reads its handshake
    /// off stdout, hard-failing - matching `handshake::verify`'s "a protocol
    /// mismatch is a hard load failure" philosophy - if it doesn't check
    /// out, or if the live handshake's language disagrees with what the
    /// manifest declared. Shared by the first spawn (`PluginProcess::spawn`)
    /// and every crash relaunch (`PluginProcess::relaunch`): both need
    /// exactly the same startup sequence.
    fn spawn(project_root: &Path, manifest: &PluginManifest) -> Result<Self> {
        if let Some(hint) = missing_plugin_binary_hint(&manifest.command, &manifest.args) {
            bail!(hint);
        }

        let mut child = Command::new(&manifest.command)
            .args(&manifest.args)
            .arg(project_root)
            // The manifest core read, so the plugin reads the same one - see
            // `manifest::MANIFEST_PATH_ENV`. Without it an SDK plugin looks
            // beside its own binary, which is right in an installed layout and
            // wrong in a checkout, where core would send a `semanticPass` to a
            // plugin that never found the `[plugin.semantic]` section
            // declaring the server to answer it with.
            .env(crate::daemon::manifest::MANIFEST_PATH_ENV, manifest.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Plugin logs are diagnostic-only today - nothing consumes them
            // programmatically - so forwarding to the daemon's own stderr
            // is simplest; it still shows up wherever the daemon's stderr
            // goes (or /dev/null in tests that don't care).
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| {
                format!("failed to spawn {} plugin ({})", manifest.language, manifest.command.display())
            })?;

        let stdout = child.stdout.take().context("plugin child process has no stdout")?;
        let stdin = child.stdin.take().context("plugin child process has no stdin")?;

        let mut reader = BufReader::new(stdout);
        let handshake = handshake::perform(&mut reader).context("plugin handshake failed")?;

        // No precedent before N plugins existed to compare against - see
        // `docs/architecture/plugin-modularity.md`'s Interfaces section.
        // Same "protocol is code, a mismatch is a hard load failure"
        // philosophy `handshake::verify` already applies to protocol version
        // mismatches, just for the manifest's declared language instead.
        if handshake.language != manifest.language {
            bail!(
                "plugin at {} declares language \"{}\" in its manifest but its \
                 handshake reports \"{}\" - refusing to load",
                manifest.manifest_dir.display(),
                manifest.language,
                handshake.language,
            );
        }

        Ok(Self { child, io: PluginIo { reader, writer: stdin } })
    }

    /// Ends this process and reaps it: closes its pipes (the plugin's own cue
    /// to exit), waits up to `grace` for it to go, then kills it. Shared by
    /// [`PluginProcess::shutdown`] and [`PluginProcess::relaunch`] - both have
    /// to leave neither a running tsserver nor a zombie behind. On a process
    /// that has already exited it returns straight away.
    ///
    /// The relaunch half is new with GM-294. A relaunch used to replace a
    /// process that was always already dead (a crash, or a timeout's own
    /// kill), and simply dropped its `Child` - which never waits, so even
    /// that left a zombie until the daemon exited. Since a failed apply now
    /// relaunches a *live* plugin on purpose (see
    /// [`PluginProcess::apply_file_change`]), dropping would leave a whole
    /// running plugin and its tsserver behind.
    fn end(self, grace: Duration) -> Result<()> {
        // Destructured rather than dropped field by field: dropping `io`
        // closes both pipes, and closing the write half of the plugin's stdin
        // is precisely the "please exit" signal.
        let PluginState { mut child, io } = self;
        drop(io);

        let deadline = Instant::now() + grace;
        loop {
            match child.try_wait().context("failed to check whether the plugin had exited")? {
                Some(_) => return Ok(()),
                None if Instant::now() >= deadline => break,
                None => std::thread::sleep(EXIT_POLL_INTERVAL),
            }
        }

        child.kill().context("failed to signal a plugin that ignored its closed stdin")?;
        child.wait().context("failed to reap the plugin process")?;
        Ok(())
    }
}

/// A live handle on the spawned JS/TS plugin process. `Mutex`-wrapped so it
/// can be shared across the connection-serving threads and the watcher
/// thread the same way `daemon::run` already shares its `Connection` - a
/// full actor/async rewrite is more than this ticket needs.
pub struct PluginProcess {
    /// Kept so a crash relaunch can spawn a replacement for exactly the same
    /// project without any caller having to remember and pass it back in.
    project_root: PathBuf,
    /// Kept for the same reason as `project_root`: a crash relaunch
    /// (`Self::relaunch`) needs the exact command/language this process was
    /// spawned with, and no caller re-supplies it on that path.
    manifest: PluginManifest,
    /// Where this plugin's pid is recorded - `daemon::registry
    /// ::PluginRegistry::pid_file_for`'s per-language path for a registry-
    /// owned supervisor, or whatever legacy/test path a caller that predates
    /// the registry still passes. [`Self::relaunch`] rewrites *this* file,
    /// not a hardcoded one: before task 155 it always wrote the single
    /// legacy `plugin.pid` regardless of which language actually crashed,
    /// which two supervisors sharing that one file would have stepped on
    /// each other's records over - the exact hazard `PluginRegistry`'s own
    /// module doc calls out as "not deferred" until this was fixed.
    pid_file: PathBuf,
    state: Mutex<PluginState>,
    next_id: AtomicI64,
    /// File paths handed to [`Self::apply_file_change`] whose diff has not
    /// yet been confirmed committed. Ordinarily holds at most the one file
    /// currently in flight, dropped the instant its diff commits; a plugin
    /// that dies mid round-trip leaves it here instead, which is exactly the
    /// "pending dirty-file queue" a crash relaunch replays before returning.
    pending: Mutex<Vec<String>>,
    /// Resolved once at construction (from [`RoundTripTimeouts::from_env`])
    /// and reused across every relaunch - a timeout budget is a property of
    /// this `PluginProcess`, not of whichever child process happens to be
    /// running behind it right now.
    timeouts: RoundTripTimeouts,
}

impl PluginProcess {
    /// Spawns `manifest`'s plugin for `project_root` - see
    /// [`PluginState::spawn`]. `pid_file` is where [`Self::relaunch`] records
    /// a crash-recovery respawn's fresh pid; the *first* pid (this call's own)
    /// is the caller's job to write, matching every existing caller
    /// (`PluginSupervisor::start`/`replay_pending`/`ensure_fresh`), which
    /// already write it themselves right after a successful spawn.
    pub fn spawn(project_root: &Path, manifest: &PluginManifest, pid_file: PathBuf) -> Result<Self> {
        let state = PluginState::spawn(project_root, manifest)?;
        Ok(Self {
            project_root: project_root.to_path_buf(),
            manifest: manifest.clone(),
            pid_file,
            state: Mutex::new(state),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(Vec::new()),
            timeouts: RoundTripTimeouts::from_env(),
        })
    }

    /// Replaces this process's round-trip budget after construction - the
    /// seam GM-355 needed and [`FILE_CHANGED_TIMEOUT_ENV`] could not give.
    ///
    /// That env override is read once, in `spawn` above, and the resulting
    /// budget then belongs to this `PluginProcess` for the rest of its life,
    /// across every relaunch. That is right for production, where one budget
    /// describes one plugin. It is wrong for a test whose *subject* is a
    /// timeout, because such a test needs two different budgets in sequence:
    /// a short one for the round trip that must time out, and an ordinary one
    /// for the round trips that must succeed afterwards. Sharing the short
    /// one between them is what made
    /// `a_timed_out_file_change_relaunches_the_plugin_and_replays_the_dirty_file_without_blocking_another_language`
    /// a statement about the machine: its replay, a perfectly healthy round
    /// trip against the relaunched process, was racing the 150ms the *stall*
    /// had been given. Measured, that replay takes 2.0ms on an idle machine
    /// and still 2.2ms under a 20x CPU oversubscription - and crosses 150ms,
    /// times out and fails the test, at 50x.
    ///
    /// Test-only, because there is no production reason to change a budget
    /// mid-life and a setter nobody calls would be dead code. What stays real
    /// in the test that uses it is everything that matters: the timeout it is
    /// judged on still elapses for real, against a plugin that genuinely
    /// never answers.
    #[cfg(test)]
    pub(crate) fn set_round_trip_timeouts(&mut self, timeouts: RoundTripTimeouts) {
        self.timeouts = timeouts;
    }

    /// The plugin process's pid, so the daemon can record it for tooling that
    /// has to reason about the plugin from outside this process. Reflects
    /// whichever process is current, so it changes across a crash relaunch -
    /// `relaunch` keeps the on-disk pid file in step with it for the same
    /// reason.
    pub fn pid(&self) -> u32 {
        self.state.lock().unwrap().child.id()
    }

    /// Ends this plugin process and waits for it to be gone, consuming the
    /// handle - what `daemon::lifecycle` calls when the plugin has been idle
    /// long enough to sleep, and again on the core's own way out.
    ///
    /// Closing the pipes is the whole shutdown on the ordinary path: the
    /// plugin exits on its stdin's `end` event (index.ts), which is the same
    /// mechanism that makes it die with a core that was killed. The signal is
    /// only insurance against a plugin that does not notice - a hung reparse,
    /// a future handler that swallows the event - because a "sleeping" plugin
    /// still holding its half gigabyte of tsserver would be the exact cost
    /// this timeout exists to avoid.
    ///
    /// Reaping matters as much as ending: the daemon is this process's parent
    /// for its whole run, so an unwaited child would sit as a zombie until the
    /// core exits, and `is_process_alive` (which `cli::status` and the tests
    /// ask) cannot tell a zombie from a running process.
    pub fn shutdown(self, grace: Duration) -> Result<()> {
        // `state`'s Mutex is unwrapped via `into_inner` - `self` is owned
        // here, so there is no contention left to guard against, only the
        // poisoning case `.unwrap()` already treats as fatal everywhere else
        // in this module. See `PluginState::end` for the ending itself.
        let Self { state, .. } = self;
        state.into_inner().unwrap().end(grace)
    }

    /// Sends a `FileChanged` request for `file_path` to the plugin and
    /// applies its diff response to `conn`. The plugin's stdin/stdout pair
    /// is locked for each round trip's duration, so concurrent callers (e.g.
    /// a future reindex path alongside the watcher thread) queue rather than
    /// interleave their requests on the wire.
    ///
    /// If the plugin process has exited unexpectedly, this transparently
    /// spawns a fresh one and replays every file path still pending -
    /// including `file_path` itself - against it before returning, rather
    /// than surfacing the crash to the caller. See this module's doc comment
    /// for why that distinction (crash vs. a deliberate stop) matters.
    ///
    /// A round trip that instead *times out* (`self.timeouts` - see
    /// [`RoundTripTimeouts`]) is deliberately **not** treated the same way as
    /// an ordinary crash, even though `send_one`'s own `on_timeout` already
    /// killed the process by the time control gets back here, which makes
    /// `process_has_exited()` true either way. The difference is what is
    /// safe to do next: an ordinary crash means the dead process never got a
    /// chance to act on anything still pending, so blindly replaying it
    /// against the fresh one is lossless. A timeout carries no such
    /// guarantee - the plugin may have been mid-write on the very request
    /// that just timed out - so resending it blind here risks a duplicate
    /// side effect, and could turn one wedged plugin into a loop of
    /// full-timeout waits if the replacement is no more responsive than the
    /// original (imagine a plugin that is slow, not dead: every relaunch
    /// would just time out again). So on a timeout this still relaunches
    /// (`self.relaunch`, the same "existing crash-recovery path" an ordinary
    /// crash uses), but leaves `file_path` sitting in the pending queue and
    /// returns the timeout error rather than calling `replay_pending` -
    /// `daemon::lifecycle::PluginSupervisor::file_changed` is what notices
    /// that error is a timeout and requeues the file onto its own dirty
    /// queue, to be replayed by whatever next touches this language, exactly
    /// like a file that arrived while the plugin was asleep.
    ///
    /// Any other failure - the plugin is alive and answered, but its diff
    /// could not be committed (a storage error, a link pass that failed, an
    /// answer that does not parse) - is neither a crash nor a timeout, and is
    /// returned as it is, with the file dropped from the pending queue. It is
    /// *not* replayed: the plugin updates its cached copy of a file when it
    /// answers, not when core commits, so asking the same live process again
    /// gets an empty diff back and the failure turns into an `Ok(())` that
    /// nothing ever logs. That is exactly how a foreign-key refusal on every
    /// second edit went unseen (GM-292) - this method replayed after *any*
    /// send error, and `process_has_exited` merely decided whether to
    /// relaunch first. The caller reports what this returns - see
    /// `daemon::lifecycle::PluginSupervisor::file_changed`'s non-timeout
    /// branch.
    ///
    /// Returning the error is not enough on its own, because that cache is
    /// still ahead of the index: the file's *next* reparse - a later watcher
    /// event, or `ensure_fresh` on the next query - would get the same empty
    /// diff, and `ensure_fresh` would then record the new content hash over a
    /// graph that never took the edit. So a non-crash failure also relaunches
    /// the plugin deliberately (GM-293 on 2.12.1, GM-294 here). A fresh
    /// process has no cache, so its first reparse of the file is a full
    /// extraction, and the index converges as soon as whatever refused the
    /// write stops doing so. The price is a warm tsserver thrown away, which
    /// is acceptable only because this path should now be rare: the one
    /// failure known to hit it routinely, enforced foreign keys, is gone
    /// (`storage::connection::open`). If it ever becomes common, that is a
    /// bug to fix at its cause, not a relaunch to make cheaper. The relaunch
    /// is best-effort - a spawn failure is logged, and the apply error is
    /// still what the caller gets.
    ///
    /// "The plugin is alive" means both that the current process has not
    /// exited *and* that it is the process this request was sent to. If
    /// another caller relaunched in between, the process this request died
    /// on is gone and the current one never saw the file - that is the crash
    /// path's replay, not this one.
    ///
    /// `semantic_suspended` (task GM-274) is ANDed with this plugin's own
    /// `manifest.capabilities.semantic_pass` at every round trip this makes
    /// (directly, and via [`Self::replay_pending`] on the crash-recovery
    /// path below) - a language whose supervisor found its process tree over
    /// `memoryLimitMb` never receives a `semanticPass` request through this
    /// method, whether or not its manifest otherwise declares the capability.
    /// The caller (`daemon::lifecycle::PluginSupervisor::file_changed`/
    /// `replay_pending`) is what actually knows whether this language is
    /// suspended - a fact that outlives any one `PluginProcess`, since a
    /// suspended language's plugin still gets a *fresh* `PluginProcess` on
    /// its next wake (see that supervisor's own doc comment) - so it is
    /// threaded in here rather than read off `self`.
    pub fn apply_file_change(
        &self,
        conn: &Mutex<Connection>,
        file_path: impl Into<String>,
        embedding: &EmbeddingPipeline,
        semantic_suspended: bool,
    ) -> Result<()> {
        let file_path = file_path.into();
        self.enqueue_pending(&file_path);

        let (sent_to, sent) = self.send_one(conn, &file_path, embedding, semantic_suspended);
        if let Err(first_err) = sent {
            // A crash shows up here as a failed write or read on the plugin's
            // pipes. Confirm the process is really gone before replacing a
            // merely-slow process's live handle out from under it -
            // `process_has_exited` is a non-blocking (if briefly polled)
            // check for exactly that. A timeout has already forced this to be
            // true (`on_timeout` killed the process before this line runs),
            // but the check is still correct and still cheap to make
            // unconditionally.
            let exited = self.process_has_exited();

            if is_timeout(&first_err) {
                // See this function's own doc comment: a timeout is not safe
                // to replay inline. `file_path` (and anything else already
                // queued) stays in `self.pending` for whenever the next
                // ordinary call retries it - which is exactly what happens,
                // since a fresh `apply_file_change` for the same path just
                // re-enqueues (a no-op, already there) and sends it again.
                // Unchanged by GM-294: checked before the non-crash branch
                // below so a timeout can never be mistaken for one.
                if exited {
                    self.relaunch(&format!("the process exited unexpectedly ({first_err:#})"))
                        .context("failed to relaunch the JS/TS plugin after it exited unexpectedly")?;
                }
                return Err(first_err);
            }

            // Another thread may already have won the relaunch race by the
            // time we check - then the current process is alive, but it is
            // not the one this request died on, and replaying against it is
            // still the recovery (`replay_pending` always sends against
            // whatever is current).
            let relaunched_elsewhere = self.pid() != sent_to;
            if !exited && !relaunched_elsewhere {
                // Not a crash, so nothing a replay can fix - see this
                // method's doc. Dropped from the queue rather than left in
                // it: a later crash's replay would otherwise stop at this
                // entry first and fail every recovery behind it.
                self.remove_pending(&file_path);
                let err = first_err.context(format!("failed to apply the plugin's diff for {file_path}"));
                // Relaunched to discard a cache that is now ahead of the
                // index, not replayed - the error below is still what the
                // caller gets, whether or not the relaunch works.
                if let Err(relaunch_err) = self.relaunch(&format!(
                    "its change to {file_path} could not be applied ({err:#}), so its cached copy of \
                     that file is ahead of the index - a fresh process re-extracts it in full"
                )) {
                    eprintln!(
                        "g-mesh daemon: could not relaunch the {} plugin after a failed apply \
                         ({relaunch_err:#}) - {file_path} may stay stale until the plugin restarts",
                        self.manifest.language
                    );
                }
                return Err(err);
            }

            if exited {
                self.relaunch(&format!(
                    "the process exited unexpectedly ({first_err:#}) - replaying pending file changes"
                ))
                .context("failed to relaunch the JS/TS plugin after it exited unexpectedly")?;
            }

            return self.replay_pending(conn, embedding, semantic_suspended).with_context(|| {
                format!(
                    "JS/TS plugin process exited unexpectedly and could not be recovered while applying a change to {file_path}"
                )
            });
        }

        // The ordinary, no-crash case: `send_one` above already delivered
        // this file, so it is done, not still pending. `replay_pending`
        // never runs this call, so nothing else would otherwise drop it -
        // leaving it here would grow the queue forever and make every
        // future crash replay the project's entire change history.
        self.remove_pending(&file_path);
        Ok(())
    }

    /// Adds `file_path` to the pending queue unless it is already there -
    /// repeated crashes must not grow the queue without bound, and there is
    /// nothing to gain from sending the same path to the plugin twice.
    fn enqueue_pending(&self, file_path: &str) {
        let mut pending = self.pending.lock().unwrap();
        if !pending.iter().any(|f| f == file_path) {
            pending.push(file_path.to_string());
        }
    }

    /// Drops `file_path` from the pending queue - its diff has committed, so
    /// there is nothing left to replay it for.
    fn remove_pending(&self, file_path: &str) {
        self.pending.lock().unwrap().retain(|f| f != file_path);
    }

    /// Sends every file still queued to the plugin, in the order they were
    /// queued, dropping each once its diff commits. Only reached from the
    /// crash-recovery path in [`Self::apply_file_change`]: it starts from
    /// whatever the dead process left pending (which always includes the
    /// file that triggered this replay, still queued behind whatever an
    /// earlier crash may have left too) and picks up exactly where it left
    /// off.
    fn replay_pending(
        &self,
        conn: &Mutex<Connection>,
        embedding: &EmbeddingPipeline,
        semantic_suspended: bool,
    ) -> Result<()> {
        loop {
            let next = { self.pending.lock().unwrap().first().cloned() };
            let Some(file_path) = next else { return Ok(()) };
            self.send_one(conn, &file_path, embedding, semantic_suspended).1?;
            self.remove_pending(&file_path);
        }
    }

    /// Synchronous per-file staleness check plus reindex-if-needed, per
    /// `watcher::staleness::ensure_fresh` - see
    /// `daemon::lifecycle::PluginSupervisor::ensure_fresh`'s doc for why this
    /// exists and what gap it closes.
    ///
    /// The mtime/hash comparison (`watcher::staleness::is_stale`) runs
    /// without this process's `state` lock at all - the overwhelmingly common
    /// case (nothing changed) must not queue behind a live reparse it has
    /// nothing to do with, matching the whole point of `watcher::staleness`'s
    /// two-tier design. Only a real mismatch takes the lock, for exactly the
    /// one round trip a live watcher event would also pay for.
    ///
    /// Unlike [`Self::apply_file_change`], this does not go through the
    /// pending-queue crash-recovery path: a plugin that has crashed since the
    /// last round trip surfaces as an ordinary `Err` here, which the MCP
    /// layer logs and treats as best-effort (see `mcp::GMeshMcpServer::
    /// ensure_file_fresh`) rather than something worth relaunching a process
    /// over on a mere freshness check.
    ///
    /// A *timeout* still gets the process relaunched, though (best-effort -
    /// a relaunch failure is logged, not propagated over the timeout error
    /// itself): unlike an ordinary crash, this call already knows for certain
    /// the process is dead (its own `on_timeout` just killed it), so leaving
    /// it dead until some unrelated request happens to notice would strand
    /// every other query-time freshness check behind it for no reason. This
    /// query itself is not retried, matching [`Self::apply_file_change`]'s
    /// own reasoning for why a timed-out request must not be resent blind.
    ///
    /// The other exception is a reindex that a *live* plugin answered and the
    /// index refused (GM-293, ported by GM-294), for the same reason
    /// [`Self::apply_file_change`] relaunches on it - and here it matters
    /// more. The plugin has already cached the refused text, so the next
    /// query's retry would get an empty diff back, succeed, and record the
    /// new content hash as this file's baseline over a graph that never took
    /// the edit: stale data marked fresh, surviving a restart. The baseline
    /// is not written for the failed attempt (`watcher::staleness::
    /// ensure_fresh` records it only after a successful reindex); the
    /// relaunch is what makes the retry a full extraction instead of that
    /// empty diff. The error is still returned, for the MCP layer to log.
    /// Only a [`staleness::ReindexFailed`] that is not a timeout, from the
    /// very process that was asked and is still running, qualifies - a file
    /// that could not be read, or a baseline that could not be written,
    /// leaves the plugin's cache no further ahead than the index; a timeout
    /// already took the relaunch above; and a crashed plugin keeps the
    /// no-relaunch behaviour this doc starts with.
    ///
    /// `semantic_suspended` gates the semantic half exactly like
    /// [`Self::apply_file_change`]'s own parameter of the same name - see
    /// that method's doc comment.
    pub fn ensure_fresh(
        &self,
        conn: &Mutex<Connection>,
        file_path: &str,
        embedding: &EmbeddingPipeline,
        semantic_suspended: bool,
    ) -> Result<StalenessOutcome> {
        {
            let guard = conn.lock().unwrap();
            if !staleness::is_stale(&guard, &self.project_root, file_path)? {
                return Ok(StalenessOutcome::AlreadyFresh);
            }
        }

        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let (result, asked) = {
            let mut state = self.state.lock().unwrap();
            let asked = state.child.id();
            let PluginState { child, io: PluginIo { reader, writer } } = &mut *state;
            let mut on_timeout = self.kill_on_timeout(child);
            // `conn` is handed through as the `Mutex` it is, not pre-locked -
            // GM-396: `staleness::ensure_fresh`/`apply_file_change` take it
            // only for as long as each of their own steps needs it, so a
            // reindex's embedding step never holds it for the round trip's
            // whole duration.
            let result = staleness::ensure_fresh(
                reader,
                writer,
                conn,
                &self.project_root,
                file_path,
                id,
                embedding,
                self.timeouts.file_changed,
                self.timeouts.semantic_pass_file,
                self.manifest.capabilities.semantic_pass && !semantic_suspended,
                &mut on_timeout,
            );
            (result, asked)
        };
        self.relaunch_after_timeout_if_needed(&result);

        let Err(err) = &result else { return result };
        let refused_by_the_index = err.downcast_ref::<staleness::ReindexFailed>().is_some()
            && !is_timeout(err)
            && self.pid() == asked
            && !self.process_has_exited();
        if refused_by_the_index {
            if let Err(relaunch_err) = self.relaunch(&format!(
                "its query-time reindex of {file_path} could not be applied ({err:#}), so its cached \
                 copy of that file is ahead of the index - a fresh process re-extracts it in full"
            )) {
                eprintln!(
                    "g-mesh daemon: could not relaunch the {} plugin after a failed query-time \
                     reindex ({relaunch_err:#}) - {file_path} may stay stale until the plugin restarts",
                    self.manifest.language
                );
            }
        }
        result
    }

    /// Asks the plugin's semantic layer to upgrade what the structural pass
    /// could only guess at, and commits its answer - see
    /// `watcher::apply::apply_semantic_pass`.
    ///
    /// This is the *whole-project* entry point, used once the cold-start
    /// bulk walk is done (`daemon::run`); the per-file pass that follows an
    /// incremental reparse needs no call of its own, because
    /// `apply_file_change` already sends it on the same locked round trip.
    ///
    /// Unlike [`Self::apply_file_change`], a dead plugin surfaces here as an
    /// ordinary `Err` rather than a relaunch: there is nothing pending to
    /// replay (a semantic pass owns no file the index is missing), and the
    /// caller treats a missing upgrade as best-effort.
    ///
    /// A *timeout* is the one case that still relaunches - see
    /// [`Self::ensure_fresh`]'s doc comment for the identical reasoning: this
    /// call already knows the process is dead (`on_timeout` just killed it),
    /// so leaving it dead would strand every future request against this
    /// language, not just this one pass. `daemon::semantic`'s callers already
    /// treat any `Err` from this function as "the pass did not complete,
    /// `semanticPassAt` stays unset, try again later" - a relaunch changes
    /// nothing about that contract, it just means "later" has a live process
    /// to try against instead of a dead one.
    ///
    /// `file_count` is the number of indexed `File` nodes the caller already
    /// knows about (`daemon::semantic::indexed_file_count`), used to scale
    /// this one request's timeout via [`RoundTripTimeouts::
    /// semantic_pass_project_timeout`] - see that method and
    /// [`RoundTripTimeouts`]'s own doc comment for why a flat timeout here is
    /// wrong for anything past a small project.
    pub fn semantic_pass(
        &self,
        conn: &Mutex<Connection>,
        file_paths: Vec<String>,
        file_count: usize,
        embedding: &EmbeddingPipeline,
    ) -> Result<()> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let timeout = self.timeouts.semantic_pass_project_timeout(file_count);
        let result = {
            let mut state = self.state.lock().unwrap();
            let PluginState { child, io: PluginIo { reader, writer } } = &mut *state;
            let mut on_timeout = self.kill_on_timeout(child);
            // See `Self::ensure_fresh`'s identical comment: `conn` is handed
            // through as the `Mutex` it is (GM-396).
            apply_semantic_pass(reader, writer, conn, file_paths, id, embedding, timeout, &mut on_timeout)
        };
        self.relaunch_after_timeout_if_needed(&result);
        result
    }

    /// Sends the `workspaceChanged` notification for `file_path` - GM-263's
    /// wire addition, finally used by GM-272's per-language reindex (see
    /// `docs/architecture/multi-language-plugins.md`'s Interfaces section on
    /// `ControlMessage::WorkspaceChanged`). A **notification**, not a
    /// request: `id: None`, no response frame is read, and the caller does
    /// not block on this plugin acting on it before moving on to the reindex
    /// that actually repopulates the graph - the design doc is explicit that
    /// core "follows it with the per-language reindex", not "waits for an
    /// acknowledgement".
    ///
    /// Only ever reached for a plugin whose own manifest declared at least
    /// one `[plugin.workspace] watch_files` pattern (`daemon::registry
    /// ::PluginRegistry::workspace_language_matches` is what decided this
    /// language was even in play), so a plugin that predates this message -
    /// the bundled TS one, whose `watch_files` is empty - never receives it
    /// in the first place; see `daemon::workspace_reindex`'s module doc for
    /// why that is enough and no capability/version check is layered on top.
    /// A plugin that *did* declare `watch_files` but genuinely does not
    /// recognize this method (not possible for anything in this repo today,
    /// but a hand-written third-party manifest could) is expected to ignore
    /// an unrecognized notification the same way the bundled TS plugin's own
    /// `handleEnvelope` already does for any method it does not match in its
    /// `switch` - falling through to "no `id`, so nothing is written back" -
    /// which is the ordinary, safe behaviour for an unknown JSON-RPC
    /// notification, not a special case this wire message has to plan
    /// around.
    ///
    /// Best-effort from the caller's point of view: a write failure here
    /// (the pipe is gone, the process just died) is returned so the caller
    /// can log it, but it must never abort the reindex that follows - the
    /// notification is an optimization (the plugin drops a cache it would
    /// otherwise have to notice is stale on its own), not a precondition for
    /// correctness, since the reindex rebuilds the plugin's on-disk-derived
    /// state from scratch regardless of whether this arrived.
    pub fn notify_workspace_changed(&self, file_path: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let envelope = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            message: ControlMessage::WorkspaceChanged { file_path: file_path.to_string() },
        };
        write_message(&mut state.io.writer, &envelope)
            .context("failed to send the workspaceChanged notification")
    }

    /// Shared tail of [`Self::ensure_fresh`]/[`Self::semantic_pass`]: neither
    /// goes through [`Self::apply_file_change`]'s pending-queue replay, but
    /// both still owe the plugin a relaunch once they know for certain it is
    /// dead - which a timeout (unlike an ordinary crash) tells them for
    /// certain, since their own `on_timeout` is what killed it. Called after
    /// `self.state`'s lock has already been released (both callers scope
    /// their round trip in a block for exactly this), because [`Self::relaunch`]
    /// takes that same lock itself.
    ///
    /// Best-effort: a relaunch failure is logged and swallowed here rather
    /// than replacing the caller's own error, because the caller's error (the
    /// timeout) is the one its own contract already knows how to report;
    /// losing that in favor of a secondary "and then the relaunch also
    /// failed" would tell the caller less, not more.
    fn relaunch_after_timeout_if_needed<T>(&self, result: &Result<T>) {
        let Err(err) = result else { return };
        if !is_timeout(err) {
            return;
        }
        if let Err(relaunch_err) = self.relaunch(&format!("the process exited unexpectedly ({err:#})")) {
            eprintln!(
                "g-mesh daemon: failed to relaunch the {} plugin after a control-plane timeout: {relaunch_err:#}",
                self.manifest.language
            );
        }
    }

    /// One `fileChanged` round trip against whichever process is current,
    /// returning that process's pid alongside the result. The pid is read
    /// under the same `state` lock the round trip holds, so it names the
    /// process that actually got the request - which is what
    /// [`Self::apply_file_change`] compares against to tell "this plugin
    /// refused, and is still here" from "another caller relaunched it in
    /// between". Read before taking the lock instead, a relaunch landing in
    /// that gap would make a live plugin's storage refusal look like a crash
    /// someone else had already recovered from, and replay it into exactly
    /// the empty diff GM-292 hid behind.
    fn send_one(
        &self,
        conn: &Mutex<Connection>,
        file_path: &str,
        embedding: &EmbeddingPipeline,
        semantic_suspended: bool,
    ) -> (u32, Result<()>) {
        // A per-process atomic counter is all `apply_file_change_diff`'s doc
        // comment asks for - it only needs an id unique enough to catch a
        // response answering the wrong request, not a globally unique one.
        // Left untouched across a relaunch: the fresh process has never seen
        // any of these ids either, so there is nothing to collide with.
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let mut state = self.state.lock().unwrap();
        let sent_to = state.child.id();
        // Split into disjoint field borrows up front - borrowing `child` and
        // the reader/writer as three separate `&mut` borrows doesn't
        // typecheck through the `MutexGuard`'s `DerefMut` otherwise, and
        // `on_timeout` below needs `child` independently of the read.
        let PluginState { child, io: PluginIo { reader, writer } } = &mut *state;
        let mut on_timeout = self.kill_on_timeout(child);
        // See `Self::ensure_fresh`'s identical comment: `conn` is handed
        // through as the `Mutex` it is (GM-396).
        let result = apply_file_change_diff(
            reader,
            writer,
            conn,
            file_path,
            id,
            embedding,
            self.timeouts.file_changed,
            self.timeouts.semantic_pass_file,
            self.manifest.capabilities.semantic_pass && !semantic_suspended,
            &mut on_timeout,
        );
        (sent_to, result)
    }

    /// The `on_timeout` this process gives every round trip: kill the child
    /// behind `child` - see `protocol::jsonrpc::read_message_with_timeout`'s
    /// doc comment for why killing it is what actually unblocks the read
    /// that timed out (closing its stdout), and this module's own doc
    /// comment for why an unexpected exit - which this deliberately causes -
    /// is already a case [`Self::apply_file_change`]/[`Self::semantic_pass`]/
    /// [`Self::ensure_fresh`] know how to recover from.
    ///
    /// Takes `child` as a parameter rather than reading `self.state` itself
    /// because every caller already holds `self.state`'s lock for the
    /// round trip this guards - calling back into `self.state.lock()` from
    /// inside the closure would deadlock on the very same, non-reentrant
    /// `Mutex`.
    fn kill_on_timeout<'a>(&'a self, child: &'a mut Child) -> impl FnMut() + 'a {
        move || {
            eprintln!(
                "g-mesh daemon: the {} plugin did not answer a control-plane request in time - \
                 killing it so a fresh process can take over",
                self.manifest.language
            );
            let _ = child.kill();
        }
    }

    /// Whether the process backing the *current* state has exited.
    ///
    /// Polls `try_wait` for up to half a second rather than checking exactly
    /// once: a killed process's pipes close - which is what makes the write
    /// in [`Self::send_one`] fail in the first place - a moment *before* the
    /// kernel finishes tearing it down far enough for `try_wait` to see it
    /// as exited, so a single check made immediately after that failed write
    /// can race a gap that is real, if usually tiny. A plugin that is merely
    /// slow to answer (not dead) still costs nothing extra: `try_wait`
    /// itself never blocks, and the very first call already covers the
    /// overwhelmingly common case where the exit is already visible.
    fn process_has_exited(&self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if matches!(self.state.lock().unwrap().child.try_wait(), Ok(Some(_))) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Replaces the current process - a crashed one, one a timeout killed, or
    /// a live one whose cache has to be discarded - with a freshly spawned
    /// one, handshake and all. `why` is logged, not propagated - a relaunch
    /// that itself fails to spawn is the caller's problem (via the `Result`
    /// this returns), but one that succeeds should read as "recovered from
    /// X", not silently swallow what X was.
    ///
    /// `why` is the whole reason, worded by the caller: a crash and a
    /// deliberate relaunch over a failed apply (GM-294) are different events
    /// and must not read the same in the log.
    ///
    /// `self.pid_file` - this process's own, not a hardcoded shared path - is
    /// rewritten too - left alone, it would keep naming a process that no
    /// longer exists, or worse, one a recycled pid now belongs to. Before
    /// task 155 this always wrote the single legacy `plugin.pid`, which was
    /// harmless while there was only ever one plugin but would have let two
    /// languages' relaunches overwrite each other's pid file once there was
    /// more than one.
    ///
    /// The process being replaced is ended and reaped after the swap
    /// ([`PluginState::end`]) - near-instant for one that already crashed or
    /// was killed, and what keeps a relaunched *live* plugin from leaving its
    /// tsserver running or its pid a zombie.
    fn relaunch(&self, why: &str) -> Result<()> {
        eprintln!("g-mesh daemon: relaunching the {} plugin: {why}", self.manifest.language);
        let fresh = PluginState::spawn(&self.project_root, &self.manifest)?;
        let pid = fresh.child.id();
        let replaced = std::mem::replace(&mut *self.state.lock().unwrap(), fresh);
        super::write_pid_file(&self.pid_file, pid);
        // Outside the state lock: the fresh process is already serving, and
        // waiting out the old one's grace period must not hold up a request.
        if let Err(err) = replaced.end(RELAUNCH_GRACE) {
            eprintln!(
                "g-mesh daemon: the replaced {} plugin process did not shut down cleanly: {err:#}",
                self.manifest.language
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let hint =
            missing_workspace_binary_hint(&binary).expect("a missing target/debug binary must get a hint");
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
        assert!(hint.contains("cargo build --workspace"), "{hint}");
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
    /// message is left to name it, the same way `missing_node_entry_hint`
    /// declines a non-node command.
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

        assert!(hint.contains("g-mesh-plugin-python.exe"), "{hint}");
        assert!(!hint.contains("g-mesh-plugin-python does not exist"), "{hint}");
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

    /// [`missing_plugin_binary_hint`] tries the node-entry check first and
    /// falls back to the workspace-binary check - both spawn sites
    /// (`PluginState::spawn`, `daemon::bulk_index::walk_one_language`) call
    /// only this, so it has to actually dispatch to the workspace check for a
    /// non-node manifest, not just the node one every existing caller already
    /// exercised.
    #[test]
    fn missing_plugin_binary_hint_dispatches_to_the_workspace_check_for_a_non_node_command() {
        let workspace = tempfile::tempdir().unwrap();
        let binary = workspace.path().join("target").join("debug").join("g-mesh-plugin-python");
        let hint =
            missing_plugin_binary_hint(&binary, &[]).expect("a missing workspace binary must get a hint");
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

    /// `npm run build` rewrites every file on every invocation. A rebuild
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

        fs::write(dir.path().join("node_modules/pkg/index.js"), "module.exports = { changed: true };")
            .unwrap();
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

    /// The bundled plugin's own build is readable from the running test
    /// binary - `core/build.rs` has just built the plugin it points at - so
    /// every build stamp this process publishes names a real fingerprint
    /// rather than degrading to [`FINGERPRINT_UNAVAILABLE`].
    ///
    /// (What the *index* is stamped with is no longer this value - see
    /// `daemon::registry::indexer_version` and its own tests.)
    #[test]
    fn the_bundled_plugins_build_is_fingerprintable_from_the_test_binary() {
        // Same unbuilt-plugin check the actual spawn path makes (see
        // `missing_node_entry_hint`'s doc comment) - without it, this test's
        // own assertion below fails as "unavailable" != "unavailable"
        // (`assert_ne!` printing both sides identically, since both are the
        // same degraded constant), which names nothing about why.
        let bundled_manifest = bundled_manifest();
        if let Some(hint) = missing_node_entry_hint(&bundled_manifest.command, &bundled_manifest.args) {
            panic!("{hint}");
        }

        let bundled = bundled_fingerprint();

        assert_ne!(
            bundled, FINGERPRINT_UNAVAILABLE,
            "the test binary's own plugin build must be readable - `cargo test` builds it"
        );
        assert_eq!(bundled.len(), FINGERPRINT_HEX_CHARS);
        assert!(bundled.chars().all(|c| c.is_ascii_hexdigit()), "{bundled} must be hex");
    }

    /// A compiled `.js` entry point needs `node` in front of it, and the
    /// argument order has to be exactly what a shell would have written -
    /// this is the shape every existing install and every test that sets
    /// [`PLUGIN_PATH_ENV`] depends on.
    #[test]
    fn a_javascript_entry_point_is_launched_through_node() {
        let entry = Path::new("/somewhere/plugins/typescript/dist/src/index.js");

        let (command, args) = launch_command_for(entry);

        assert_eq!(command, PathBuf::from("node"), "a script needs an interpreter");
        assert_eq!(args, vec![entry.to_string_lossy().into_owned()]);
    }

    /// The single-executable build (`scripts/bundle-plugin.sh`) carries its own
    /// runtime, so naming an interpreter would both be wrong and reintroduce
    /// the Node.js dependency the whole bundle exists to remove.
    #[test]
    fn a_self_contained_plugin_executable_is_launched_directly() {
        let entry = Path::new("/opt/g-mesh/plugins/typescript/g-mesh-plugin-typescript");

        let (command, args) = launch_command_for(entry);

        assert_eq!(command, entry, "the executable is its own command");
        assert!(args.is_empty(), "nothing is prepended to a native executable's argv");
    }

    /// Windows names the same artifact with an extension, which must not be
    /// mistaken for a script.
    #[test]
    fn a_windows_plugin_executable_is_launched_directly_too() {
        let (command, args) =
            launch_command_for(Path::new(r"C:\g-mesh\plugins\typescript\g-mesh-plugin-typescript.exe"));

        assert!(args.is_empty(), "{command:?} took interpreter arguments it should not have");
        assert_ne!(command, PathBuf::from("node"));
    }

    /// The dev checkout keeps the behavior it has always had: nothing about
    /// bundling a release may change how `cargo test` and a working tree spawn
    /// the plugin. (Test binaries live in the workspace's
    /// `target/<profile>/deps/`, where no `plugins/` directory exists, so
    /// resolution falls through to the compile-time path.)
    #[test]
    fn a_checkout_still_resolves_to_the_compiled_javascript_entry_point() {
        let manifest = bundled_manifest();

        assert_eq!(manifest.command, PathBuf::from("node"));
        assert_eq!(manifest.args.len(), 1);
        assert!(
            manifest.args[0].ends_with("index.js"),
            "expected a compiled JS entry point, got {}",
            manifest.args[0]
        );
    }

    /// The check `docs/architecture/plugin-modularity.md`'s Interfaces
    /// section adds right after `handshake::perform` succeeds: a manifest
    /// whose declared `language` disagrees with what the live plugin's
    /// handshake actually reports is a hard-fail, naming both values - the
    /// bundled JS/TS plugin's handshake reports `"typescript"` (see
    /// `protocol::types`'s handshake test), so declaring anything else in
    /// the manifest must be refused.
    #[test]
    fn spawning_a_manifest_whose_language_disagrees_with_the_live_handshake_hard_fails_naming_both() {
        let manifest = PluginManifest { language: "python".to_string(), ..bundled_manifest() };
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
    fn index_enforcing_foreign_keys() -> Mutex<Connection> {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::storage::schema::apply(&conn).unwrap();
        Mutex::new(conn)
    }

    fn greet_end_line(conn: &Mutex<Connection>) -> i64 {
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
            PluginProcess::spawn(project.path(), &bundled_manifest(), project.path().join("plugin.pid"))
                .expect("failed to spawn the JS/TS plugin");
        let embedding = EmbeddingPipeline::disabled();
        plugin
            .apply_file_change(&conn, "lib.ts", &embedding, true)
            .expect("a cold cache sends only upserts, which nothing can refuse");
        let pid = plugin.pid();

        fs::write(&file, GREET_GROWN).unwrap();
        let err = match plugin.apply_file_change(&conn, "lib.ts", &embedding, true) {
            Ok(()) => panic!("a diff the index refused must not be reported as applied"),
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
        let baseline = |conn: &Mutex<Connection>| -> (i64, String) {
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
            PluginProcess::spawn(project.path(), &bundled_manifest(), project.path().join("plugin.pid"))
                .expect("failed to spawn the JS/TS plugin");
        let embedding = EmbeddingPipeline::disabled();
        assert_eq!(
            plugin.ensure_fresh(&conn, "lib.ts", &embedding, true).unwrap(),
            StalenessOutcome::ReindexedNoPriorRecord,
            "a never-indexed file is a cold-cache reparse, which nothing can refuse"
        );
        let before = baseline(&conn);
        let pid = plugin.pid();

        std::thread::sleep(Duration::from_millis(10));
        fs::write(&file, GREET_GROWN).unwrap();
        let err = match plugin.ensure_fresh(&conn, "lib.ts", &embedding, true) {
            Ok(outcome) => panic!("a reindex the index refused must not be reported as {outcome:?}"),
            Err(err) => format!("{err:#}"),
        };
        assert!(err.contains("FOREIGN KEY"), "the storage error itself must reach the caller: {err}");
        assert_eq!(baseline(&conn), before, "a refused reindex must not advance the baseline");

        // Whatever refused the write stops refusing it.
        conn.lock().unwrap().pragma_update(None, "foreign_keys", "OFF").unwrap();
        assert_eq!(
            plugin.ensure_fresh(&conn, "lib.ts", &embedding, true).unwrap(),
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
}
