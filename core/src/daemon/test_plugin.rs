//! A stand-in language plugin, for unit tests that need a *second* language
//! to exist.
//!
//! Every existing test that spawns a plugin spawns the real bundled JS/TS one
//! (`core/tests/plugin_crash_recovery.rs`, `daemon::plugin`'s own tests, via
//! `daemon::plugin::bundled_manifest`), because until now there was exactly
//! one plugin to spawn. `daemon::registry`'s whole subject is what happens
//! with *more* than one - routing between them, spawning them independently,
//! keeping one language's crash away from another's - and none of that can be
//! tested against a single plugin, however real.
//!
//! So this module writes a plugin directory that is real in every way
//! `daemon::manifest`, `daemon::plugin` and `daemon::lifecycle` care about -
//! a `plugin.toml` that `read_manifest`/`discover` parse like any other, and
//! a process that speaks the actual wire protocol (`Content-Length` framed
//! JSON-RPC: a handshake first, then one `FileChangeResponse` per request).
//! That process is `g-mesh-fake-plugin`, a test-only binary of
//! `plugins/sdk` (`plugins/sdk/fake/main.rs`), which the manifest names as
//! `${G_MESH_BIN_DIR}/g-mesh-fake-plugin`; it exists only after
//! `cargo build --workspace`. It answers every request with an *empty* diff, which is the
//! point: these tests are about which process gets asked, not about what a
//! parser makes of a file.
//!
//! It also answers a one-shot `--bulk-index` invocation
//! (`daemon::bulk_index`'s own spawn shape - see that module's tests), the
//! same way the real bundled plugin does: a fixed, deterministic NDJSON
//! stream of two nodes and the edge between them, named after this fake
//! plugin's own language so a test summing two languages' output can tell
//! whose contribution is whose. Each node `<language>-nN` also carries a
//! signature and a doc comment when the project root holds a
//! `.<language>-nN.sig` / `.<language>-nN.doc` file, read at walk time, so an
//! embedding test can give nodes text and change it between walks.
//! [`set_bulk_stream`] replaces the whole stream, and can make the walk exit
//! non-zero after it, for tests that need a different graph per walk.
//!
//! Each variant's behaviour is chosen by `fake-plugin.json` in the plugin
//! directory, read once when the process starts; the binary's module doc
//! lists every file it reads and writes there.
//!
//! # Counting spawns
//!
//! Each fake process appends its own pid to `spawns.log` in its plugin
//! directory before it does anything else, so a test can ask *how many
//! processes this manifest has ever produced* ([`spawns`]) rather than
//! inferring it from a pid that happens to look the same. That is what makes
//! "the second file of the same language reuses the first supervisor" and
//! "waking a sleeping supervisor re-spawns the plugin its own manifest names"
//! into assertions about processes rather than about return values.
//!
//! # Counting round trips
//!
//! Same idea, one level down: every framed request that carries an id (a
//! real round trip, as opposed to the handshake or a notification) is
//! appended to `requests.log` in the same directory, as `"<method>
//! <filePath>"`, before it is answered ([`requests`]). Task 129 is what
//! first needed this - "a burst of rapid saves to the same file costs one
//! plugin round trip, not one per save" is a claim about how many times the
//! plugin was actually asked, and `spawns.log` alone cannot distinguish a
//! debounced burst from a single lucky one that never crashed the process it
//! was already talking to.
//!
//! [`file_changed_requests`] narrows that log to just the `fileChanged`
//! entries - what `PluginSupervisor::file_changed`/`apply_file_change`
//! actually sends per incremental reparse - excluding the `semanticPass`
//! request `apply_file_change` also always sends on the very same round trip
//! (`watcher::apply::apply_file_change`'s own doc comment): that one is a
//! fixed 1:1 side effect of a `fileChanged` request, not a second thing a
//! debounce test is checking, so counting both together would double every
//! number for a reason unrelated to what changed.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::protocol::types::CURRENT_PROTOCOL_VERSION;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;

/// The file each fake plugin process appends its pid to on startup.
const SPAWN_LOG: &str = "spawns.log";

/// The fake plugin's options file - see [`write_options`].
const OPTIONS_FILE: &str = "fake-plugin.json";

/// The file a *gated* plugin waits for before it announces its handshake -
/// see [`install_gated`] and [`open_handshake_gate`].
const HANDSHAKE_GATE: &str = "handshake.allow";

/// The file each fake plugin process appends one line to per answered
/// request that carried an id - see this module's "Counting round trips" doc.
const REQUEST_LOG: &str = "requests.log";

/// The file each fake plugin process appends one line to per **notification**
/// it received - a framed message with no `id`, answered with nothing per
/// JSON-RPC 2.0. Kept apart from [`REQUEST_LOG`] rather than folded into it:
/// every existing caller of [`requests`]/[`file_changed_requests`] already
/// assumes each line there is a real id-carrying round trip (GM-270's
/// `only_the_semantic_pass_capable_language_receives_the_request` among
/// them), and a notification is a different kind of thing on the wire - it
/// gets no response frame at all, unlike every entry `REQUEST_LOG` holds.
/// GM-272 is the first fixture that needs to observe one:
/// `ControlMessage::WorkspaceChanged` (GM-263) is sent as a notification, and
/// its whole test-visible existence is "did the plugin see it happen", which
/// only [`notifications`] can answer.
const NOTIFICATION_LOG: &str = "notifications.log";

/// The file, in the directory *above* each plugin directory, that every fake
/// plugin under it appends `"<language> <method>"` to for every framed message
/// it receives, request or notification. One file shared by all of them, so a
/// test can assert the order in which core sent messages to different
/// plugins ([`frames`]).
const FRAME_LOG: &str = "frames.log";

/// A plugin directory holding this file answers every complete
/// `semanticPass` with its contents as the diff, instead of an empty one. See
/// [`set_semantic_pass_answer`].
const SEMANTIC_ANSWER: &str = "semantic-pass.json";

/// A plugin directory holding this file sets its fields on every
/// `semanticPass` answer. See [`set_semantic_pass_fields`].
const SEMANTIC_FIELDS: &str = "semantic-pass-fields.json";

/// The file each fake plugin process appends every `semanticPass` request's
/// `filePaths` to, as a JSON array. See [`semantic_passes`].
const SEMANTIC_PASS_LOG: &str = "semantic-passes.log";

/// Makes the plugin in this directory hold every `semanticPass` answer until
/// [`SEMANTIC_PASS_GATE_OPEN`] exists - see [`gate_semantic_pass`].
const SEMANTIC_PASS_GATED: &str = "semantic-pass.gated";

/// Lets a [`SEMANTIC_PASS_GATED`] plugin answer its `semanticPass` requests -
/// see [`open_semantic_pass_gate`].
const SEMANTIC_PASS_GATE_OPEN: &str = "semantic-pass.allow";

/// Writes a discoverable plugin directory named `language` under `root`,
/// claiming `extensions`, and returns the directory it created.
///
/// Laid out exactly as a real one is (`<root>/<language>/plugin.toml`), so
/// `daemon::manifest::discover(&[root])` picks it up with no test-only path
/// through discovery.
///
/// No `[plugin.capabilities]` table at all - the same "says nothing" shape a
/// hand-written manifest predating GM-270 has - so `Capabilities::default`
/// reads it as `semantic_pass = false`: this fixture's plugin is never sent a
/// `semanticPass` request. Use [`install_semantic_pass_capable`] for a
/// language GM-270's per-language scheduler should ask for a pass.
pub(crate) fn install(root: &Path, language: &str, extensions: &[&str]) -> PathBuf {
    install_inner(root, language, extensions, false, false, false, false, &[], &[])
}

/// [`install`], but the manifest also carries a `[plugin.workspace]` table
/// with `watch_files`/`exclude_dirs` - GM-272's fixture for a language whose
/// `daemon::registry::PluginRegistry::workspace_language_matches`/
/// `route_settled_path` must route a settled path to a per-language reindex
/// (`watch_files`) or refuse to route one at all (`exclude_dirs`), instead of
/// the ordinary extension routing every other `install*` fixture here
/// exercises. Each entry in `watch_files` is written into the manifest
/// exactly as given - an exact name (`"go.mod"`) or a genuine glob
/// (`"*.csproj"`) both compile the same way through `daemon::manifest::
/// read_manifest`'s `Glob::new`, so this fixture needs no separate "glob"
/// variant to test glob matching specifically.
pub(crate) fn install_with_workspace(
    root: &Path,
    language: &str,
    extensions: &[&str],
    watch_files: &[&str],
    exclude_dirs: &[&str],
) -> PathBuf {
    install_inner(root, language, extensions, false, false, false, false, watch_files, exclude_dirs)
}

/// [`install_with_workspace`], but also `[plugin.capabilities] semantic_pass
/// = true` (like [`install_semantic_pass_capable`]) - GM-272's fixture for
/// checking that a workspace reindex's semantic phase
/// (`daemon::workspace_reindex::run`) is asked for, and its
/// `language_state.semanticPassAt` recorded, exactly for the one language
/// being reindexed.
pub(crate) fn install_with_workspace_semantic_pass_capable(
    root: &Path,
    language: &str,
    extensions: &[&str],
    watch_files: &[&str],
    exclude_dirs: &[&str],
) -> PathBuf {
    install_inner(root, language, extensions, false, false, true, false, watch_files, exclude_dirs)
}

/// [`install`], but the manifest declares `[plugin.capabilities]
/// semantic_pass = true` - GM-270's fixture for a language whose plugin
/// `daemon::semantic::run_with_registry`/`run_once` must ask for a
/// whole-project pass, and whose per-file `fileChanged` round trip
/// (`watcher::apply::apply_file_change`) must be followed by a `semanticPass`
/// request too.
///
/// The fake plugin's own wire behaviour is unchanged either way - it answers
/// *any* id-carrying request with an empty `{}` diff, `fileChanged` and
/// `semanticPass` alike (see this module's own doc comment on
/// [`requests`]/[`file_changed_requests`]) - so this capability changes only
/// whether core *sends* the `semanticPass` request at all, which is exactly
/// what GM-270's tests (`requests.log` assertions) are about. Installing two
/// languages with different capabilities - one via this function, one via
/// [`install`] - is what lets a test prove only the capable one ever receives
/// it.
pub(crate) fn install_semantic_pass_capable(root: &Path, language: &str, extensions: &[&str]) -> PathBuf {
    install_inner(root, language, extensions, false, false, true, false, &[], &[])
}

/// [`install`], but the plugin does not answer its handshake until
/// [`open_handshake_gate`] is called - a spawn that is *held open* for as long
/// as a test wants to look at what the rest of the daemon does meanwhile
/// (`daemon::registry`'s task-164 tests).
///
/// A gate rather than a sleep, deliberately. What those tests are about is a
/// spawn that is in flight *right now*, and a real one is a process launch
/// plus whatever the plugin does before it can speak - hundreds of
/// milliseconds, but a different number on every machine and a wildly
/// different one under a loaded `cargo test`, where a bare process start
/// can take over a second. Any test that raced a fixed delay
/// would be asserting about this machine's scheduler as much as about the
/// daemon. A gate removes wall-clock time from the question entirely: the
/// spawn stays in flight until the test says otherwise, so "while a spawn is
/// in progress" is a state the test *holds*, not one it hopes to catch.
///
/// The gate sits in front of the handshake frame specifically, because that is
/// what `PluginProcess::spawn` blocks on. The process itself still starts, and
/// still records its pid in `spawns.log` first, so a test can tell "the spawn
/// is in flight" from "it has not started yet" by observation ([`spawns`]).
///
/// Every test that installs one of these **must** open its gate, on every path
/// including a failing assertion: a spawning thread that is never let go never
/// joins.
pub(crate) fn install_gated(root: &Path, language: &str, extensions: &[&str]) -> PathBuf {
    install_inner(root, language, extensions, true, false, false, false, &[], &[])
}

/// Lets the plugin(s) installed in `plugin_dir` finish their handshake - see
/// [`install_gated`]. Idempotent, and safe to call on a plugin that was never
/// gated in the first place.
pub(crate) fn open_handshake_gate(plugin_dir: &Path) {
    fs::write(plugin_dir.join(HANDSHAKE_GATE), "go\n")
        .expect("failed to open the fake plugin's handshake gate");
}

/// Makes the plugin in `plugin_dir` receive (and log) every later
/// `semanticPass` request but hold its answer until [`open_semantic_pass_gate`]
/// is called: a pass that is in flight for exactly as long as a test wants,
/// with no wall-clock race - the same reasoning as [`install_gated`], one step
/// later. Unlike [`install_stalling`], the answer does arrive once the gate
/// opens. Read per request, so it takes effect without a respawn.
///
/// Every test that gates a pass **must** open the gate on every path,
/// including a failing assertion: the thread waiting on the pass never
/// returns otherwise.
pub(crate) fn gate_semantic_pass(plugin_dir: &Path) {
    fs::write(plugin_dir.join(SEMANTIC_PASS_GATED), "gated\n")
        .expect("failed to gate the fake plugin's semantic pass");
}

/// Lets a plugin gated by [`gate_semantic_pass`] answer its `semanticPass`
/// requests. Idempotent.
pub(crate) fn open_semantic_pass_gate(plugin_dir: &Path) {
    fs::write(plugin_dir.join(SEMANTIC_PASS_GATE_OPEN), "go\n")
        .expect("failed to open the fake plugin's semantic pass gate");
}

/// [`install`], but the plugin completes its handshake normally and then
/// never answers the *first* framed request it ever sees for this plugin
/// directory - it still parses that request and logs it to `requests.log`
/// ([`requests`]), so a test can confirm the request really was received, but
/// it deliberately never writes the response frame back. Every request after
/// that first one - including the first one a crash-recovery relaunch's fresh
/// process sees - is answered normally, exactly like [`install`].
///
/// This is task GM-271's fixture: a language whose plugin is up, has shaken
/// hands, and then hangs on exactly one request - the "semantic engine hangs
/// or is slow" failure mode `docs/architecture/multi-language-plugins.md`'s
/// Failure Modes section describes, and exactly the shape nothing but a
/// per-request timeout can recover from (a debounce, a retry, a bigger read
/// buffer - none of them help when the peer is simply never going to write
/// anything for *that* request). "Exactly one request, ever" rather than
/// "every request forever" is deliberate: it is what lets a test observe the
/// *whole* recovery cycle - timeout, relaunch, and a subsequent replay that
/// actually succeeds - rather than an unbounded retry loop that never
/// converges. The "already stalled once" fact is persisted to
/// `stalled-once.marker` in this plugin's own directory, not held in the process's
/// memory, specifically so it survives exactly the event this fixture exists
/// to provoke: a crash-recovery relaunch, which is a brand new process
/// with no memory of what its predecessor already did.
///
/// The stalled process itself stays alive and keeps its stdin open (unlike a
/// crashed plugin, whose pipes are already closed) - so a test using this
/// fixture is specifically exercising the *timeout* path, not the
/// pre-existing "process exited" crash-recovery path
/// `plugin_crash_recovery.rs` already covers. It still exits cleanly if core
/// closes its stdin (as every fixture here does), and
/// it dies immediately if killed - which is exactly what
/// `daemon::plugin::PluginProcess`'s `on_timeout` does once a request against
/// it runs past its budget.
pub(crate) fn install_stalling(root: &Path, language: &str, extensions: &[&str]) -> PathBuf {
    install_inner(root, language, extensions, false, true, false, false, &[], &[])
}

/// [`install_semantic_pass_capable`], but the fake plugin process also
/// allocates and holds onto a large buffer right after its handshake - task
/// GM-274's fixture for `[plugin] memoryLimitMb`: a real child process
/// whose resident memory a test can actually put a low configured limit
/// under, exercised by `daemon::lifecycle::PluginSupervisor::check_memory_limit`'s
/// own sampling (`daemon::memory::process_tree_rss_mb`), not by a mock.
///
/// `semantic_pass` capable (unlike plain [`install`]) so a test using this
/// fixture can also prove the *other* half of GM-274: once this language is
/// suspended, no `semanticPass` request reaches it, even though its manifest
/// says it would otherwise receive one - the discriminating condition every
/// other capability test in this module already relies on
/// ([`install_semantic_pass_capable`]'s own doc comment).
///
/// 200MB is comfortably above the fake plugin's idle RSS (under 2MB) and
/// comfortably below anything that would make this fixture slow or flaky to allocate - the
/// point is a real, measurable spike a low test-only `memoryLimitMb` (well
/// under 200MB, well over the idle baseline) can reliably catch, not a
/// pathological one.
pub(crate) fn install_memory_hungry(root: &Path, language: &str, extensions: &[&str]) -> PathBuf {
    install_inner(root, language, extensions, false, false, true, true, &[], &[])
}

/// [`install_semantic_pass_capable`], but the plugin answers the first
/// `semanticPass` this directory ever receives with an empty diff marked
/// `incomplete`, carrying `reason` as its `incompleteReason` - what an SDK
/// plugin sends when its language server errored or never answered - or no
/// `incompleteReason` at all when `reason` is `None`, as a plugin that
/// predates the field does. Every later request is answered normally, so a
/// retry completes.
pub(crate) fn install_incomplete_once(
    root: &Path,
    language: &str,
    extensions: &[&str],
    reason: Option<&str>,
) -> PathBuf {
    let dir = install_inner(root, language, extensions, false, false, true, false, &[], &[]);
    answer_first_semantic_pass_incomplete(&dir, language, reason);
    dir
}

/// Rewrites the options of `language`'s installed plugin directory so that
/// its next process answers the first `semanticPass` as
/// [`install_incomplete_once`] describes (and is neither gated, stalling nor
/// memory-hungry), keeping the manifest it was installed with.
pub(crate) fn answer_first_semantic_pass_incomplete(dir: &Path, language: &str, reason: Option<&str>) {
    assert_eq!(
        dir.file_name().and_then(|name| name.to_str()),
        Some(language),
        "not {language}'s plugin directory"
    );
    write_options(dir, &Options { incomplete_once: true, incomplete_reason: reason, ..Options::default() });
}

#[allow(clippy::too_many_arguments)]
fn install_inner(
    root: &Path,
    language: &str,
    extensions: &[&str],
    gated: bool,
    stalling: bool,
    semantic_pass: bool,
    memory_hungry: bool,
    watch_files: &[&str],
    exclude_dirs: &[&str],
) -> PathBuf {
    let dir = root.join(language);
    fs::create_dir_all(&dir).expect("failed to create the fake plugin's directory");
    write_options(&dir, &Options { gated, stalling, memory_hungry, ..Options::default() });
    fs::write(
        dir.join("plugin.toml"),
        manifest(language, extensions, semantic_pass, watch_files, exclude_dirs),
    )
    .expect("failed to write the fake plugin's manifest");
    dir
}

/// Adds `semantic_sweep = true` to a semantic-pass-capable plugin's
/// `[plugin.capabilities]`. Takes effect at the next `discover`.
pub(crate) fn declare_semantic_sweep(plugin_dir: &Path) {
    let path = plugin_dir.join("plugin.toml");
    let manifest = fs::read_to_string(&path).expect("failed to read the fake plugin's manifest");
    assert!(manifest.contains("semantic_pass = true\n"), "only a semantic-pass-capable manifest sweeps");
    let swept = manifest.replace("semantic_pass = true\n", "semantic_pass = true\nsemantic_sweep = true\n");
    fs::write(&path, swept).expect("failed to write the fake plugin's manifest");
}

/// Makes every later complete `semanticPass` to the plugin in `plugin_dir`
/// answer with `diff` (a `FileChangeDiff` as JSON), or, with `None`, with an
/// empty diff again. Read per request, so it takes effect without a respawn.
pub(crate) fn set_semantic_pass_answer(plugin_dir: &Path, diff: Option<&str>) {
    let path = plugin_dir.join(SEMANTIC_ANSWER);
    match diff {
        Some(diff) => fs::write(&path, diff).expect("failed to write the fake plugin's semantic answer"),
        None => {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Makes every later `semanticPass` answer of the plugin in `plugin_dir`
/// (bar an [`install_incomplete_once`] one) carry `fields` (a JSON object of
/// response fields, e.g. `{"incomplete": true, "unfinishedFiles": [...]}`), or,
/// with `None`, none again. Read per request, so it takes effect without a
/// respawn.
pub(crate) fn set_semantic_pass_fields(plugin_dir: &Path, fields: Option<&str>) {
    let path = plugin_dir.join(SEMANTIC_FIELDS);
    match fields {
        Some(fields) => fs::write(&path, fields).expect("failed to write the fake plugin's answer fields"),
        None => {
            let _ = fs::remove_file(&path);
        }
    }
}

/// The `filePaths` of every `semanticPass` this plugin directory's
/// process(es) have ever been sent, oldest first, as JSON arrays (`[]` for a
/// whole-project pass). Empty before the first one.
pub(crate) fn semantic_passes(plugin_dir: &Path) -> Vec<String> {
    let Ok(log) = fs::read_to_string(plugin_dir.join(SEMANTIC_PASS_LOG)) else { return Vec::new() };
    log.lines().map(str::to_string).collect()
}

/// Every pid this plugin directory has ever been spawned as, oldest first.
/// Empty (rather than a panic) before the first spawn - "never spawned" is a
/// perfectly ordinary thing for a test to assert.
pub(crate) fn spawns(plugin_dir: &Path) -> Vec<u32> {
    let Ok(log) = fs::read_to_string(plugin_dir.join(SPAWN_LOG)) else { return Vec::new() };
    log.lines().filter_map(|line| line.trim().parse().ok()).collect()
}

/// Every id-carrying request this plugin directory's process(es) have ever
/// answered, oldest first, across every spawn, as `"<method> <filePath>"`
/// (`filePath` empty for a request with no such field, e.g. `status`).
/// Empty (rather than a panic) before the first one, same as [`spawns`].
pub(crate) fn requests(plugin_dir: &Path) -> Vec<String> {
    let Ok(log) = fs::read_to_string(plugin_dir.join(REQUEST_LOG)) else { return Vec::new() };
    log.lines().map(str::to_string).collect()
}

/// Every **notification** (a framed message with no `id`) this plugin
/// directory's process(es) have ever received, oldest first, across every
/// spawn, as `"<method> <filePath>"` (`filesCreated` as
/// `"filesCreated <path>,<path>"`) - see [`NOTIFICATION_LOG`]'s own doc
/// comment for why this is a separate log from [`requests`] rather than the
/// same one. Empty before the first one, same as [`requests`]/[`spawns`].
pub(crate) fn notifications(plugin_dir: &Path) -> Vec<String> {
    let Ok(log) = fs::read_to_string(plugin_dir.join(NOTIFICATION_LOG)) else { return Vec::new() };
    log.lines().map(str::to_string).collect()
}

/// Every framed message any fake plugin installed under `plugins_root` has
/// received, oldest first, as `"<language> <method>"` - see [`FRAME_LOG`].
pub(crate) fn frames(plugins_root: &Path) -> Vec<String> {
    let Ok(log) = fs::read_to_string(plugins_root.join(FRAME_LOG)) else { return Vec::new() };
    log.lines().map(str::to_string).collect()
}

/// Adds `semantic_prepare = true` to a semantic-pass-capable plugin's
/// `[plugin.capabilities]`. Takes effect at the next `discover`.
pub(crate) fn declare_semantic_prepare(plugin_dir: &Path) {
    let path = plugin_dir.join("plugin.toml");
    let manifest = fs::read_to_string(&path).expect("failed to read the fake plugin's manifest");
    assert!(manifest.contains("semantic_pass = true\n"), "only a semantic-pass-capable manifest prepares");
    let prepared =
        manifest.replace("semantic_pass = true\n", "semantic_pass = true\nsemantic_prepare = true\n");
    fs::write(&path, prepared).expect("failed to write the fake plugin's manifest");
}

/// Adds `files_created = true` to any fake plugin's `[plugin.capabilities]`,
/// adding the table if the manifest has none. Takes effect at the next
/// `discover`.
pub(crate) fn declare_files_created(plugin_dir: &Path) {
    let path = plugin_dir.join("plugin.toml");
    let manifest = fs::read_to_string(&path).expect("failed to read the fake plugin's manifest");
    let table = "[plugin.capabilities]\n";
    let declared = if manifest.contains(table) {
        manifest.replace(table, &format!("{table}files_created = true\n"))
    } else {
        format!("{manifest}\n{table}files_created = true\n")
    };
    fs::write(&path, declared).expect("failed to write the fake plugin's manifest");
}

/// Just the `fileChanged` requests among [`requests`], as the file path each
/// one named - i.e. one entry per real `PluginSupervisor::file_changed` ->
/// `apply_file_change` round trip, the granularity task 129's debounce test
/// cares about. Deliberately excludes the `semanticPass` request
/// `apply_file_change` also always sends on the same round trip
/// (`watcher::apply::apply_file_change`'s own doc): that one is a fixed,
/// pre-existing 1:1 side effect of *this* one, not a second thing debouncing
/// could coalesce away, and counting it in would double every number below
/// for a reason that has nothing to do with what this test is checking.
pub(crate) fn file_changed_requests(plugin_dir: &Path) -> Vec<String> {
    requests(plugin_dir)
        .into_iter()
        .filter_map(|line| line.strip_prefix("fileChanged ").map(str::to_string))
        .collect()
}

/// Makes every later bulk walk of `language` over `project` emit `lines`
/// (NDJSON, one item each) instead of its fixed stream, then exit with
/// `exit_code`.
pub(crate) fn set_bulk_stream(project: &Path, language: &str, lines: &[String], exit_code: i32) {
    let mut stream = lines.join("\n");
    stream.push('\n');
    fs::write(project.join(format!(".{language}-bulk.ndjson")), stream)
        .expect("failed to write the bulk stream");
    fs::write(project.join(format!(".{language}-bulk.exit")), exit_code.to_string())
        .expect("failed to write the bulk exit status");
}

/// A fresh in-memory index for the (empty) diffs a fake plugin's round trips
/// commit. Shared by every caller of this module, because none of them cares
/// what is in it - only that the commit path a real file change takes is the
/// one being exercised.
pub(crate) fn empty_index() -> IndexStore {
    let conn = Connection::open_in_memory().expect("failed to open an in-memory index");
    conn.pragma_update(None, "foreign_keys", "ON").expect("failed to enable foreign keys");
    schema::apply(&conn).expect("failed to apply the schema");
    IndexStore::new(conn)
}

/// `semantic_pass` controls whether the manifest carries a
/// `[plugin.capabilities] semantic_pass = true` table at all - see
/// [`install`]/[`install_semantic_pass_capable`]'s own doc comments. `false`
/// omits the table entirely rather than writing `semantic_pass = false`
/// explicitly, matching what a real hand-written manifest predating GM-270
/// looks like - both read the same way through `Capabilities::default`, but
/// the omitted-table shape is the one this fixture is standing in for.
fn manifest(
    language: &str,
    extensions: &[&str],
    semantic_pass: bool,
    watch_files: &[&str],
    exclude_dirs: &[&str],
) -> String {
    let extensions = extensions.iter().map(|ext| format!("\"{ext}\"")).collect::<Vec<_>>().join(", ");
    let capabilities = if semantic_pass { "\n[plugin.capabilities]\nsemantic_pass = true\n" } else { "" };
    // Omitted entirely when both are empty, matching `daemon::manifest`'s own
    // "an absent [plugin.workspace] table defaults to all three fields empty"
    // convention - the ordinary fixture (every `install*` call before
    // GM-272) still parses to exactly the conservative default it always
    // did.
    let workspace = if watch_files.is_empty() && exclude_dirs.is_empty() {
        String::new()
    } else {
        let watch = watch_files.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(", ");
        let exclude = exclude_dirs.iter().map(|e| format!("\"{e}\"")).collect::<Vec<_>>().join(", ");
        format!("\n[plugin.workspace]\nwatch_files = [{watch}]\nexclude_dirs = [{exclude}]\n")
    };
    format!(
        r#"
[plugin]
language = "{language}"
protocol_version = {CURRENT_PROTOCOL_VERSION}
plugin_version = "0.0.0-test"

[plugin.spawn]
command = "${{G_MESH_BIN_DIR}}/{FAKE_PLUGIN_BIN}"
args = ["--language", "{language}", "--dir", "./"]

[plugin.languages]
extensions = [{extensions}]
{capabilities}{workspace}"#
    )
}

/// Breaks the plugin in `plugin_dir` for every later spawn: the process
/// starts, says nothing for `after_ms` (long enough for other callers to pile
/// in behind its spawn), then exits 1 without a handshake - the failure
/// `PluginProcess::spawn` reports.
pub(crate) fn never_handshake(plugin_dir: &Path, after_ms: u64) {
    write_options(
        plugin_dir,
        &Options { exit_without_handshake_after_ms: Some(after_ms), ..Options::default() },
    );
}

/// Points the manifest in `plugin_dir` at a fake binary that was never built,
/// so its spawn fails the way a never-built workspace plugin's does; returns
/// the missing path. Takes effect at the next `discover`.
pub(crate) fn point_at_a_missing_binary(plugin_dir: &Path) -> PathBuf {
    let path = plugin_dir.join("plugin.toml");
    let manifest = fs::read_to_string(&path).expect("failed to read the fake plugin's manifest");
    let command = format!("command = \"${{G_MESH_BIN_DIR}}/{FAKE_PLUGIN_BIN}\"");
    assert!(manifest.contains(&command), "not a fake plugin manifest: {manifest}");
    let missing = format!("{FAKE_PLUGIN_BIN}-never-built");
    fs::write(&path, manifest.replace(&command, &format!("command = \"${{G_MESH_BIN_DIR}}/{missing}\"")))
        .expect("failed to write the fake plugin's manifest");
    crate::daemon::manifest::current_bin_dir().expect("the test binary's directory is unknown").join(missing)
}

/// The file whose bytes stand for the plugin's build in its directory's
/// fingerprint (`daemon::plugin::fingerprint` hashes the whole directory):
/// rewriting it with different bytes is a rebuild that changed something.
pub(crate) fn build_artifact(plugin_dir: &Path) -> PathBuf {
    plugin_dir.join(OPTIONS_FILE)
}

/// The binary every manifest written here names - see this module's doc.
const FAKE_PLUGIN_BIN: &str = "g-mesh-fake-plugin";

/// What one fake plugin process does beyond answering - mirrors the options
/// file `plugins/sdk/fake/main.rs` reads at start.
#[derive(Default)]
struct Options<'a> {
    gated: bool,
    stalling: bool,
    memory_hungry: bool,
    incomplete_once: bool,
    incomplete_reason: Option<&'a str>,
    exit_without_handshake_after_ms: Option<u64>,
}

/// Writes `options` where the fake plugin in `dir` reads them, failing with
/// the build command when the binary itself has not been built.
fn write_options(dir: &Path, options: &Options<'_>) {
    let bin_dir = crate::daemon::manifest::current_bin_dir().expect("the test binary's directory is unknown");
    let binary = bin_dir.join(format!("{FAKE_PLUGIN_BIN}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.is_file(),
        "{} does not exist - run `cargo build --workspace` before core's tests",
        binary.display()
    );
    let json = serde_json::json!({
        "gated": options.gated,
        "stalling": options.stalling,
        "memoryHungry": options.memory_hungry,
        "incompleteOnce": options.incomplete_once,
        "incompleteReason": options.incomplete_reason,
        "exitWithoutHandshakeAfterMs": options.exit_without_handshake_after_ms,
    });
    fs::write(dir.join(OPTIONS_FILE), json.to_string()).expect("failed to write the fake plugin's options");
}
