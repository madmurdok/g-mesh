//! The plugin process: its two modes, its framing, and everything it does
//! between core's questions and the extractor's answers.
//!
//! # Two modes, because a walk and a conversation have different shapes
//!
//! - **`--bulk-index <root>`**: one shot. Walk the project, stream every
//!   file's nodes and then its edges to stdout as NDJSON, exit. The whole
//!   stdout is a self-contained stream that ends at EOF, which is exactly
//!   what core's `NdjsonReader` consumes - no handshake ahead of it, no
//!   end-of-stream marker to agree on.
//! - **`<root>`**: long-lived. Announce the handshake, then answer framed
//!   JSON-RPC requests until stdin closes.
//!
//! Core spawns the same command for both (`daemon::bulk_index::walk_one_language`
//! and `daemon::plugin::PluginProcess::spawn`), so both live in one binary.
//!
//! # Nodes before edges, per file, always
//!
//! Core commits a bulk stream in batches and may cut a batch anywhere, so an
//! edge may only lean on what an earlier line already delivered - and an edge
//! may only name nodes of its own file, so "earlier" can be guaranteed
//! file-locally. [`FileGraph`] holds nodes and edges in separate vectors and
//! this module writes them in that order, so a plugin cannot get it wrong by
//! interleaving; the extractor's only obligation is to put the file's `File`
//! node first, which [`FileGraphBuilder`](crate::graph::FileGraphBuilder)
//! makes the natural thing to do.
//!
//! # A panic costs one file
//!
//! Every call into the extractor is wrapped in [`catch_unwind`]. An extractor
//! that panics on one pathological file would otherwise take the process down
//! mid-walk, and core would report the language's whole bulk index as failed -
//! costing the project every other file too. Caught, the panic costs that one
//! file: the walk skips it, and a `fileChanged` answers an empty diff and
//! keeps its previous baseline, so the next edit is diffed against the last
//! extraction that worked rather than against a hole.
//!
//! This is a backstop, not a feature. The default panic hook still prints the
//! panic and its location to stderr, which the daemon inherits, and a plugin
//! that panics is a plugin with a bug. What it is not is a plugin that can
//! silently delete a project's index.
//!
//! # A syntax error is not an error
//!
//! There is no way for [`Extractor::extract`] to report a parse failure,
//! deliberately. An error-tolerant parser returns most of a broken file's
//! graph, and that is worth keeping: the files someone is halfway through
//! editing are exactly the files they are about to ask questions about.
//! `hasSyntaxErrors` records it (see [`FileGraph::mark_syntax_errors`]) and
//! the graph is committed like any other.

use std::collections::HashSet;
use std::io::{self, BufReader, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use g_mesh_wire::{FileChangeDiff, Handshake, CURRENT_PROTOCOL_VERSION, JSONRPC_VERSION};

use crate::diff::diff_file;
use crate::framing::{read_frame, write_message};
use crate::graph::FileGraph;
use crate::hold::hold_point;
use crate::index::SdkIndex;
use crate::manifest::{PluginSpec, ResolvedSpec};
use crate::semantic::{LazyEngine, SemanticEngineFactory};
use crate::walk::walk_project;
use crate::Extractor;

use crate::path::RelPath;

/// Selects one-shot bulk-index mode. Must stay in sync with core's
/// `daemon::bulk_index::BULK_INDEX_FLAG`.
const BULK_INDEX_FLAG: &str = "--bulk-index";

/// Set to `1` by core on every bulk spawn: stdin is then a pipe core holds
/// open and never writes, and its EOF means core is gone (GM-397). Must stay
/// in sync with core's `daemon::bulk_index::BULK_STDIN_LIFELINE_ENV`. Without
/// it the bulk walk leaves stdin alone, so an older core or a hand run with
/// `< /dev/null` is not read as "exit before walking".
const BULK_STDIN_LIFELINE_ENV: &str = "G_MESH_BULK_STDIN_LIFELINE";

/// How long the control-plane reader gives the main loop to take the graceful
/// path after core closed stdin, before it ends the process itself - see
/// [`read_control_stream`]. Long enough for an idle loop's own shutdown, short
/// enough that a plugin busy on a request is gone well within seconds.
const LIFELINE_GRACE: Duration = Duration::from_secs(1);

/// Runs the plugin. Does not return: both modes end in [`std::process::exit`].
///
/// `spec` declares what the manifest would say, for the layouts where no
/// manifest can be found beside the binary - see [`PluginSpec`]. `semantic`
/// is the factory for the plugin's semantic tier, or `None` for a plugin that
/// has none (whose manifest must then say `semantic_pass = false`); it is
/// called on the first `semanticPass` (or `prepareSemanticPass`) and never
/// before, which is the whole
/// reason it is a factory - see `semantic`'s module doc.
///
/// # Arguments as core passes them
///
/// Core spawns `<command> <manifest args…> --bulk-index <root>` for a walk
/// and `<command> <manifest args…> <root>` for the control plane. The root is
/// therefore the argument after the flag in the first case and the *last*
/// argument in the second - `last`, not `first`, because a manifest may put
/// arguments of its own in front of it (the JS/TS plugin's
/// `args = ["dist/src/index.js"]`, which `node` consumes, is what makes this
/// invisible there). With no argument at all, the current directory: a bare
/// run from a project root is how a plugin is debugged by hand.
pub fn run<E: Extractor>(extractor: E, spec: PluginSpec, semantic: Option<SemanticEngineFactory>) -> ! {
    let resolved = ResolvedSpec::resolve(&spec);
    let args: Vec<String> = std::env::args().skip(1).collect();

    let code = match args.iter().position(|arg| arg == BULK_INDEX_FLAG) {
        Some(flag) => {
            let root = args.get(flag + 1).map(PathBuf::from).unwrap_or_else(current_dir);
            if std::env::var_os(BULK_STDIN_LIFELINE_ENV).is_some_and(|value| value == "1") {
                watch_bulk_lifeline(&resolved.language);
            }
            bulk_index(&extractor, &resolved, &root)
        }
        None => {
            let root = args.last().map(PathBuf::from).unwrap_or_else(current_dir);
            control_plane(&extractor, &resolved, semantic, &root)
        }
    };
    std::process::exit(code);
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// --- bulk index -------------------------------------------------------------

/// Starts the bulk walk's lifeline watcher (GM-397): a thread that reads and
/// discards stdin, and ends the process the moment it reaches EOF.
///
/// The walk itself never touches stdin, and a walk can go a long time without
/// writing - the project load, or any stretch that fits in the output buffer -
/// so without this a killed core goes unnoticed until the next write fails,
/// or never, if the walk finishes first. A read error counts as EOF: with the
/// variable set, core promised a pipe, and a stdin that cannot be read is not
/// one core is still holding.
///
/// Exits with 1, not 0: whatever core is left to read this stream, it did not
/// get a complete one.
fn watch_bulk_lifeline(language: &str) {
    let language = language.to_string();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        eprintln!("[{language}] core closed the bulk stream's lifeline - exiting");
        std::process::exit(1);
    });
}

/// Walks `root` and streams it. Returns the process exit code.
fn bulk_index<E: Extractor>(extractor: &E, spec: &ResolvedSpec, root: &Path) -> i32 {
    // A walk with no project model would emit a graph addressed against
    // nothing - imports resolved to the wrong files, containers missing - and
    // core would commit all of it. Failing loudly is the only honest answer:
    // `walk_one_language` turns a non-zero exit into a named failure for this
    // language and leaves the index as it was.
    let project = match extractor.load_project(root) {
        Ok(project) => project,
        Err(err) => {
            eprintln!("[{}] failed to load the project at {}: {err:#}", spec.language, root.display());
            return 1;
        }
    };

    // Test-only (GM-397): parks the walk after the load and before the
    // first write - the silent stretch in which a killed core goes unnoticed.
    hold_point("bulk", &spec.language);

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let mut files = 0usize;
    let (mut nodes, mut edges) = (0usize, 0usize);

    for path in walk_project(root, &spec.extensions, &spec.exclude_dirs) {
        let Some(source) = read_source(&path, root, &spec.language) else { continue };
        let Some(graph) = extract_caught(extractor, &project, &path, &source, &spec.language) else {
            continue;
        };
        if let Err(err) = write_graph(&mut out, &graph) {
            // A broken pipe means core stopped reading - it has failed this
            // walk already and nothing is served by finishing it.
            eprintln!("[{}] failed to write the bulk stream: {err}", spec.language);
            return 1;
        }
        files += 1;
        nodes += graph.nodes.len();
        edges += graph.edges.len();
    }

    if let Err(err) = out.flush() {
        eprintln!("[{}] failed to flush the bulk stream: {err}", spec.language);
        return 1;
    }
    eprintln!("[{}] bulk index complete: {files} file(s), {nodes} node(s), {edges} edge(s)", spec.language);
    0
}

/// One file's NDJSON: every node, then every edge. Open sites are not written -
/// they are the semantic tier's. An extractor that opted in
/// ([`FileGraphBuilder::record_untyped_receiver_calls`](crate::FileGraphBuilder::record_untyped_receiver_calls))
/// already folded its untyped receiver calls into their nodes'
/// `untypedCalls`, so those reach core on the node itself.
fn write_graph<W: Write>(out: &mut W, graph: &FileGraph) -> io::Result<()> {
    for node in &graph.nodes {
        writeln!(out, "{}", serde_json::to_string(node).expect("a WireNode always serializes"))?;
    }
    for edge in &graph.edges {
        writeln!(out, "{}", serde_json::to_string(edge).expect("a WireEdge always serializes"))?;
    }
    Ok(())
}

// --- control plane ----------------------------------------------------------

/// The long-lived mode. Returns the process exit code.
fn control_plane<E: Extractor>(
    extractor: &E,
    spec: &ResolvedSpec,
    semantic: Option<SemanticEngineFactory>,
    root: &Path,
) -> i32 {
    let handshake = Handshake {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        language: spec.language.clone(),
        plugin_version: spec.version.clone(),
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if let Err(err) = write_message(&mut out, &handshake) {
        eprintln!("[{}] failed to announce the handshake: {err}", spec.language);
        return 1;
    }

    let mut session = Session {
        extractor,
        spec,
        root: root.to_path_buf(),
        root_real: std::fs::canonicalize(root).ok(),
        project: None,
        index: SdkIndex::new(),
        engine: LazyEngine::new(&spec.language, semantic),
        project_hydrated: false,
    };
    session.load_project();

    let inbound = read_control_stream(&spec.language);
    loop {
        // A closed channel means the reader is gone without saying why,
        // which only a panic on its thread can do - the stream is as good as
        // closed.
        let next = inbound.recv().unwrap_or(Ok(None));
        match next {
            // Core ends a plugin by closing its stdin
            // (`daemon::plugin::shutdown`), so this is the whole shutdown
            // path. The engine's own child process, if it has one, goes with
            // this process - the line says whether there was one, because a
            // plugin that was only ever asked structural questions and still
            // reports an engine is the failure `capabilities.semantic-engine-lazy`
            // is about, seen from the plugin's own side.
            Ok(None) => {
                eprintln!(
                    "[{}] core closed the control stream - exiting (semantic engine started: {})",
                    spec.language,
                    session.engine.started()
                );
                return 0;
            }
            Ok(Some(body)) => {
                if let Err(err) = session.handle(&body, &mut out) {
                    eprintln!("[{}] failed to answer a control message: {err:#}", spec.language);
                    return 1;
                }
            }
            Err(err) => {
                // Framing errors desynchronize the byte stream - there is no
                // resuming from one, only guessing.
                eprintln!("[{}] the control stream is unreadable: {err:#}", spec.language);
                return 1;
            }
        }
    }
}

/// Reads the control stream on its own thread and hands each frame to the
/// main loop through a channel (GM-397).
///
/// Reading used to happen on the main loop itself, between requests - so a
/// plugin busy on a long `semanticPass` did not notice core's death until the
/// pass ended, up to core's 20-minute ceiling, with its language server
/// running alongside. Here stdin is read continuously, whatever the main loop
/// is doing:
///
/// - Frames and a framing error are passed on in order; the channel is
///   unbounded, so a busy main loop never stops this thread reading.
/// - On EOF the reader posts `Ok(None)`, which an idle main loop answers with
///   the graceful path it always took (`LspClient`'s shutdown on drop). If the
///   process is still alive [`LIFELINE_GRACE`] later, the main loop is stuck
///   in a request, so this thread kills the language servers itself -
///   `process::exit` skips `LspClient::drop` - and ends the process.
///
/// A framing error is passed on as well, and then the reader only drains
/// stdin to its EOF: the main loop returns on the error as soon as it takes
/// it, but it may be mid-request until then.
fn read_control_stream(language: &str) -> Receiver<anyhow::Result<Option<Vec<u8>>>> {
    let (sender, inbound) = mpsc::channel();
    let language = language.to_string();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(io::stdin().lock());
        loop {
            match read_frame(&mut reader) {
                Ok(Some(body)) => {
                    if sender.send(Ok(Some(body))).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = sender.send(Ok(None));
                    break;
                }
                Err(err) => {
                    // The stream is desynchronized and no further frame can
                    // be trusted, but the main loop may still be mid-request
                    // when it gets here: keep watching for EOF regardless.
                    let _ = sender.send(Err(err));
                    let _ = io::copy(&mut reader, &mut io::sink());
                    break;
                }
            }
        }
        std::thread::sleep(LIFELINE_GRACE);
        crate::lsp::kill_live_servers();
        eprintln!("[{language}] core closed the control stream mid-request - exiting");
        std::process::exit(1);
    });
    inbound
}

struct Session<'a, E: Extractor> {
    extractor: &'a E,
    spec: &'a ResolvedSpec,
    root: PathBuf,
    /// `root` resolved through every link, taken once when the session starts
    /// (a root moved or relinked under a running plugin is not followed).
    /// `None` when it could not be resolved, which turns off the event remap
    /// in [`Session::indexed_spelling`] and nothing else.
    root_real: Option<PathBuf>,
    /// `None` while the project model could not be built. Unlike the bulk
    /// walk, this is not fatal: a plugin that stops answering `fileChanged`
    /// leaves its language's whole index frozen at whatever it was, which is
    /// worse than answering from a model that is missing. Every request
    /// retries the load, so a `Cargo.toml` fixed in the editor heals the
    /// plugin without a restart.
    project: Option<E::Project>,
    index: SdkIndex,
    engine: LazyEngine,
    /// Whether this process has hydrated the whole project into `index`
    /// (GM-487). Done once, by the first `semanticPass` of any scope, and
    /// undone by `workspaceChanged`, which clears the index.
    project_hydrated: bool,
}

impl<E: Extractor> Session<'_, E> {
    fn load_project(&mut self) {
        match self.extractor.load_project(&self.root) {
            Ok(project) => self.project = Some(project),
            Err(err) => {
                self.project = None;
                eprintln!(
                    "[{}] failed to load the project at {}: {err:#} - answering from no project model \
                     until it loads",
                    self.spec.language,
                    self.root.display()
                );
            }
        }
    }

    /// Handles one frame and writes at most one response.
    fn handle<W: Write>(&mut self, body: &[u8], out: &mut W) -> anyhow::Result<()> {
        // Parsed as a loose value rather than straight into `ControlEnvelope`:
        // an envelope whose `method` this SDK does not know fails to
        // deserialize as a whole, id and all, and a request left unanswered
        // wedges core's stream until its timeout. This way an unknown method
        // still gets an acknowledgement.
        let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(body) else {
            eprintln!("[{}] ignoring a control message that is not JSON", self.spec.language);
            return Ok(());
        };
        let id = envelope.get("id").filter(|id| !id.is_null()).cloned();
        let method = envelope.get("method").and_then(|method| method.as_str()).unwrap_or_default();
        let params = envelope.get("params");

        match method {
            "fileChanged" => {
                let path = params
                    .and_then(|params| params.get("filePath"))
                    .and_then(|path| path.as_str())
                    .unwrap_or_default();
                let diff = self.file_changed(&RelPath::new(path));
                self.respond(out, id, diff)
            }
            "filesCreated" => {
                // A notification: every listed file was created in one
                // watcher batch, and each still gets its own `fileChanged`
                // after this. Only presence is applied here, nothing is
                // extracted, so there is no diff to answer with. A non-string
                // entry costs itself, not the list.
                let files: Vec<RelPath> = params
                    .and_then(|params| params.get("filePaths"))
                    .and_then(|paths| paths.as_array())
                    .map(|paths| paths.iter().filter_map(|path| path.as_str()).map(RelPath::new).collect())
                    .unwrap_or_default();
                self.files_created(&files);
                self.acknowledge(out, id)
            }
            "semanticPass" => {
                // Test-only (GM-397): parks the pass before any engine
                // starts, blocking this thread as a long pass would.
                hold_point("semantic", &self.spec.language);
                let files: Vec<RelPath> = params
                    .and_then(|params| params.get("filePaths"))
                    .and_then(|paths| paths.as_array())
                    .map(|paths| paths.iter().filter_map(|path| path.as_str()).map(RelPath::new).collect())
                    .unwrap_or_default();
                // A per-file pass needs every file its answers can land in,
                // not only its own (GM-487): an answer pointing into a file
                // the index does not hold is dropped as "outside the index".
                // So the first pass of this process hydrates the whole
                // project, whatever its scope; after that, a per-file pass
                // only adds what is new.
                if files.is_empty() || !self.project_hydrated {
                    self.project_hydrated = self.hydrate(&[]);
                }
                if !files.is_empty() {
                    self.hydrate(&files);
                }
                let root = self.root.clone();
                let answer = self.engine.answer(&files, &self.index, &root);
                self.respond_to_pass(out, id, files.is_empty(), answer)
            }
            "workspaceChanged" => {
                // A notification: core follows it with the per-language
                // reindex that repopulates the graph, so there is no diff to
                // answer with. What it means for the plugin is that every
                // cached extraction was made against a project model that no
                // longer applies.
                let file = params
                    .and_then(|params| params.get("filePath"))
                    .and_then(|path| path.as_str())
                    .unwrap_or_default();
                eprintln!(
                    "[{}] workspace changed ({file}) - reloading the project model",
                    self.spec.language
                );
                self.index.clear();
                self.project_hydrated = false;
                self.load_project();
                // And a running semantic engine must not trust its server's
                // earlier readiness for the pass that follows (GM-433).
                self.engine.workspace_changed();
                self.acknowledge(out, id)
            }
            "prepareSemanticPass" => {
                // A notification: a whole-project pass is owed and will
                // follow, so the engine may start now. Nothing is hydrated
                // here - the pass does that against the files it is asked.
                let root = self.root.clone();
                self.engine.prepare(&root);
                self.acknowledge(out, id)
            }
            // `reindex` is a no-op by design: a whole-project rebuild is an
            // unbounded stream, not one response frame, so core runs it as a
            // separate `--bulk-index` process. `status` is a liveness probe.
            other => {
                if !other.is_empty() && other != "reindex" && other != "status" {
                    eprintln!("[{}] ignoring unknown control method {other:?}", self.spec.language);
                }
                self.acknowledge(out, id)
            }
        }
    }

    /// Makes sure the index holds `files` - the whole project when `files` is
    /// empty - before the semantic pass is asked.
    ///
    /// The control-plane process never runs the bulk walk - that is a
    /// separate, one-shot process - so its index holds only the files it has
    /// been sent a `fileChanged` for. That is not enough for any pass: a
    /// per-file pass's answers point into other files, and an answer landing
    /// in a file the index does not hold is dropped as "outside the index"
    /// (GM-487); the whole-project pass core sends after the cold walk would
    /// otherwise answer about the two files someone happened to edit. So the
    /// caller hydrates the whole project once per process, and the files in
    /// scope on every pass. (The JS/TS plugin solves the same problem by
    /// having its semantic pass walk the project itself.)
    ///
    /// Gated on a semantic tier existing, and reached only from a
    /// `semanticPass`: a plugin with no engine, and a plugin doing structural
    /// work, never pay for this. The extractions it adds are *unreported*
    /// entries: core has not been told about them, so they are no diff
    /// baseline - the next `fileChanged` for one of these files answers a
    /// complete diff (see [`SdkIndex::baseline`]).
    ///
    /// Returns whether every file in scope was considered: `false` when there
    /// is no engine, or no project model to extract against.
    fn hydrate(&mut self, files: &[RelPath]) -> bool {
        if !self.engine.configured() {
            return false;
        }
        let scope: Vec<RelPath> = if files.is_empty() {
            walk_project(&self.root, &self.spec.extensions, &self.spec.exclude_dirs)
        } else {
            files.to_vec()
        };
        for path in scope {
            if self.index.entry(&path).is_some() || !self.claims(&path) {
                continue;
            }
            let Some(source) = read_source(&path, &self.root, &self.spec.language) else { continue };
            if self.project.is_none() {
                self.load_project();
            }
            self.presence_changed(&path, true);
            let Some(project) = self.project.as_ref() else { return false };
            if let Some(graph) = extract_caught(self.extractor, project, &path, &source, &self.spec.language)
            {
                self.index.insert_unreported(path, source, graph);
            }
        }
        true
    }

    /// Reparses one file against what this process last saw of it.
    fn file_changed(&mut self, path: &RelPath) -> FileChangeDiff {
        let remapped = self.indexed_spelling(path);
        let path = remapped.as_ref().unwrap_or(path);
        if !self.claims(path) {
            eprintln!("[{}] ignoring {path}: this plugin does not claim its extension", self.spec.language);
            return FileChangeDiff::default();
        }

        // Presence first, and before the unchanged-text short-circuit below,
        // so the model never extracts against a file set missing this event.
        // An unreadable file is applied as gone, as the diff treats it.
        let source = read_source(path, &self.root, &self.spec.language);
        self.presence_changed(path, source.is_some());
        let Some(source) = source else {
            // Gone, or unreadable: everything this plugin had for the file is
            // deleted. Forgetting it too means a re-creation is treated as a
            // first sighting rather than diffed against a stale baseline.
            let diff = diff_file(self.index.baseline(path), &FileGraph::default());
            self.index.remove(path);
            return diff;
        };

        // Editors save unchanged buffers often enough for this to be worth a
        // string comparison: identical text cannot produce a different graph
        // from a pure extractor, so there is nothing to parse and nothing to
        // say. Only against a reported entry: a hydrated one at the same text
        // is still news to core (GM-487).
        if self.index.entry(path).is_some_and(|entry| entry.reported && entry.source == source) {
            return FileChangeDiff::default();
        }

        if self.project.is_none() {
            self.load_project();
        }
        let Some(project) = self.project.as_ref() else {
            return FileChangeDiff::default();
        };

        let Some(graph) = extract_caught(self.extractor, project, path, &source, &self.spec.language) else {
            // The previous baseline is deliberately kept: it is the last
            // thing this plugin actually told core, so diffing the next edit
            // against it is correct. Replacing it with nothing would make the
            // next successful reparse re-send a whole file core already has.
            return FileChangeDiff::default();
        };

        let diff = diff_file(self.index.baseline(path), &graph);
        self.index.insert(path.clone(), source, graph);
        diff
    }

    /// The real spelling `path` is indexed under, when `path` reaches that
    /// file through an in-root link; `None` means handle `path` as spelled.
    ///
    /// The walk indexes a file reachable two ways under its real spelling
    /// (`docs/adr/0025-project-walk-follows-symlinks.md`), but the OS may
    /// report an edit under either spelling (inotify shares one watch per
    /// inode). Handling the link spelling as written would index the file a
    /// second time beside a baseline that never updates. Invariants:
    /// - remapped only when the index already holds the real spelling, so a
    ///   file the walk indexed under a link spelling (reachable only through
    ///   links) is still handled as that spelling;
    /// - a path resolving outside the root is never remapped: the walk
    ///   refuses such links, so nothing under that spelling is indexed;
    /// - a path that no longer resolves (deleted, dangling) is handled as
    ///   spelled, so its deletion reaches the entry it names.
    ///
    /// One `canonicalize` per event.
    fn indexed_spelling(&self, path: &RelPath) -> Option<RelPath> {
        self.real_spelling(path).filter(|real| self.index.entry(real).is_some())
    }

    /// `path`'s real in-root spelling when it differs from `path`; `None`
    /// when they are the same, when `path` does not resolve, when it resolves
    /// outside the root, or when the root itself could not be resolved. The
    /// one `canonicalize` behind [`Session::indexed_spelling`] and
    /// [`Session::files_created`].
    fn real_spelling(&self, path: &RelPath) -> Option<RelPath> {
        let root_real = self.root_real.as_ref()?;
        let real = std::fs::canonicalize(path.to_absolute(&self.root)).ok()?;
        let real = RelPath::relative_to(root_real, &real)?;
        (real != *path).then_some(real)
    }

    /// Applies presence for every file of one watcher batch's creations, so
    /// the `fileChanged`s that follow extract against a model holding all of
    /// them. Extracts nothing.
    ///
    /// Invariants:
    /// - a link spelling is remapped to its real spelling when the real one
    ///   is indexed **or listed in this same call**: the real one may be
    ///   created in this batch and so not indexed yet, and the walk lists only
    ///   the real spelling (`docs/adr/0025-project-walk-follows-symlinks.md`).
    ///   Otherwise the path is handled as spelled;
    /// - `present` is read from the disk now, not taken from the list: a file
    ///   may be gone again by the time this arrives, and its own
    ///   `fileChanged` follows and agrees;
    /// - one bad entry (unclaimed, outside the root, a panicking hook) costs
    ///   only itself;
    /// - with no project model, nothing is applied: the next load reads the
    ///   disk, which already holds every listed file.
    fn files_created(&mut self, files: &[RelPath]) {
        if self.project.is_none() {
            return;
        }
        let listed: HashSet<&RelPath> = files.iter().collect();
        let mut applied: HashSet<RelPath> = HashSet::new();
        for spelled in files {
            let path = self
                .real_spelling(spelled)
                .filter(|real| listed.contains(real) || self.index.entry(real).is_some())
                .unwrap_or_else(|| spelled.clone());
            if !self.claims(&path) || !applied.insert(path.clone()) {
                continue;
            }
            let present = is_readable_source(&path, &self.root);
            self.presence_changed(&path, present);
        }
    }

    /// [`Extractor::file_presence_changed`] for one path, when there is a
    /// project model to apply it to and the path is inside the root.
    fn presence_changed(&mut self, path: &RelPath, present: bool) {
        if !is_within_root(path) {
            return;
        }
        if let Some(project) = self.project.as_mut() {
            presence_caught(self.extractor, project, path, present, &self.spec.language);
        }
    }

    fn claims(&self, path: &RelPath) -> bool {
        path.extension().is_some_and(|extension| self.spec.extensions.contains(&extension))
    }

    /// Answers a `semanticPass` with its diff and, for a whole-project pass
    /// that did not finish, the `incomplete` flag core reads to leave
    /// `language_state.semanticPassAt` unset
    /// (`core::watcher::apply::apply_semantic_pass`).
    ///
    /// # Why only a whole-project pass carries it
    ///
    /// `semanticPassAt` is a one-shot completion flag for the *whole-project*
    /// pass, and nothing else reads `incomplete`. On a per-file pass - the one
    /// that follows every settled reparse - the only effect the flag could
    /// have is core logging a line per keystroke-save, which is precisely the
    /// noise the design's "log once" rule for a degraded engine exists to
    /// prevent (`docs/architecture/multi-language-plugins.md`, "Semantic
    /// engine missing"). A permanently missing language server would otherwise
    /// print one failure line into the daemon log for every file anyone
    /// touches, all of them saying the same thing the first one said.
    ///
    /// The engine still answers honestly in both cases - [`SemanticAnswer`]'s
    /// `complete` is about the pass, not about the wire - and this is the one
    /// place that decides the flag is worth sending.
    fn respond_to_pass<W: Write>(
        &self,
        out: &mut W,
        id: Option<serde_json::Value>,
        whole_project: bool,
        answer: crate::semantic::SemanticAnswer,
    ) -> anyhow::Result<()> {
        let Some(id) = id else { return Ok(()) };
        if whole_project && !answer.complete {
            eprintln!(
                "[{}] the whole-project semantic pass did not finish - reporting it incomplete so the \
                 index does not record a semantic pass it did not get",
                self.spec.language
            );
        }
        write_message(out, &pass_response(id, whole_project, answer))?;
        Ok(())
    }

    /// Answers a request with a diff. A notification (no id) gets nothing -
    /// but the work above still ran, so the cache stays current.
    fn respond<W: Write>(
        &self,
        out: &mut W,
        id: Option<serde_json::Value>,
        diff: FileChangeDiff,
    ) -> anyhow::Result<()> {
        let Some(id) = id else { return Ok(()) };
        write_message(out, &serde_json::json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": diff }))?;
        Ok(())
    }

    /// The answer to a method that produces no diff. Never sent for
    /// `fileChanged`/`semanticPass`: core deserializes those responses as a
    /// `FileChangeResponse` and this shape is not one.
    fn acknowledge<W: Write>(&self, out: &mut W, id: Option<serde_json::Value>) -> anyhow::Result<()> {
        let Some(id) = id else { return Ok(()) };
        write_message(
            out,
            &serde_json::json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": { "acknowledged": true } }),
        )?;
        Ok(())
    }
}

/// The frame a `semanticPass` is answered with - see
/// [`Session::respond_to_pass`] for why only a whole-project pass carries the
/// flag.
///
/// A free function so its shape can be asserted directly: what core reads is
/// this JSON, and the difference between a pass that is recorded as done and
/// one that is retried is one key in it.
fn pass_response(
    id: serde_json::Value,
    whole_project: bool,
    answer: crate::semantic::SemanticAnswer,
) -> serde_json::Value {
    let incomplete = whole_project && !answer.complete;
    let mut response = serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": answer.diff,
        "incomplete": incomplete,
    });
    if let (true, Some(reason)) = (incomplete, answer.reason) {
        response["incompleteReason"] = serde_json::Value::String(reason);
    }
    response
}

// --- shared helpers ---------------------------------------------------------

/// Reads one file as UTF-8, or reports why not and returns `None`.
///
/// A file that is not valid UTF-8 is not an error worth stopping for: it is a
/// binary file someone gave a source extension, and the rest of the project
/// is still worth indexing.
fn read_source(path: &RelPath, root: &Path, language: &str) -> Option<String> {
    match std::fs::read(path.to_absolute(root)) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(source) => Some(source),
            Err(_) => {
                eprintln!("[{language}] skipping {path}: not valid UTF-8");
                None
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => {
            eprintln!("[{language}] skipping {path}: {err}");
            None
        }
    }
}

/// Whether `path` would be read as a source by [`read_source`] - an existing
/// file that is valid UTF-8 - without its logging, so presence agrees with
/// what the file's own `fileChanged` will find.
fn is_readable_source(path: &RelPath, root: &Path) -> bool {
    std::fs::read(path.to_absolute(root)).is_ok_and(|bytes| std::str::from_utf8(&bytes).is_ok())
}

/// Whether `path` is relative and never climbs out with `..`: the
/// presence hook's contract excludes every other path.
fn is_within_root(path: &RelPath) -> bool {
    let path = Path::new(path.as_str());
    !path.has_root()
        && path.components().all(|component| {
            !matches!(component, std::path::Component::ParentDir | std::path::Component::Prefix(_))
        })
}

/// [`Extractor::file_presence_changed`], with a panic in it costing this
/// path's presence rather than the process or the rest of a batch.
///
/// `AssertUnwindSafe` although the hook mutates `project`: a panic may leave
/// the model half-updated for this path, which the hook's contract accepts
/// (it says not to panic); `workspaceChanged` rebuilds the model whole.
fn presence_caught<E: Extractor>(
    extractor: &E,
    project: &mut E::Project,
    path: &RelPath,
    present: bool,
    language: &str,
) {
    if catch_unwind(AssertUnwindSafe(|| extractor.file_presence_changed(project, path, present))).is_err() {
        eprintln!("[{language}] the presence hook panicked on {path} - its presence is not applied");
    }
}

/// [`Extractor::extract`], with a panic in it costing this file rather than
/// the process - see this module's doc.
///
/// `AssertUnwindSafe` because neither the extractor nor the project model is
/// mutated here: `extract` takes both by shared reference, so a panic cannot
/// leave either half-updated. What it *can* leave inconsistent is state an
/// extractor hides behind interior mutability, which is why
/// [`Extractor`]'s contract asks for purity in the first place.
fn extract_caught<E: Extractor>(
    extractor: &E,
    project: &E::Project,
    path: &RelPath,
    source: &str,
    language: &str,
) -> Option<FileGraph> {
    match catch_unwind(AssertUnwindSafe(|| extractor.extract(project, path, source))) {
        Ok(graph) => Some(graph),
        Err(_) => {
            // The default hook has already printed the panic and its
            // location; this says which file provoked it, which the panic
            // itself does not.
            eprintln!("[{language}] the extractor panicked on {path} - skipping this file");
            None
        }
    }
}

// --- framing ----------------------------------------------------------------
//
// LSP-style `Content-Length` framing, the same wire core's
// `protocol::jsonrpc` writes and reads, and the same wire a semantic tier
// speaks to a language server ([`crate::lsp`]). It lives in `framing` because
// this process speaks it in both directions - see that module's doc.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::SemanticAnswer;

    /// The one key that decides whether core records this language as having
    /// had its semantic pass - and the reason a per-file pass never carries it.
    #[test]
    fn only_a_whole_project_pass_reports_that_it_did_not_finish() {
        let id = serde_json::json!(7);
        let incomplete = SemanticAnswer::incomplete(FileChangeDiff::default());
        let complete = SemanticAnswer::complete(FileChangeDiff::default());

        let response = pass_response(id.clone(), true, incomplete.clone());
        assert_eq!(response["incomplete"], serde_json::json!(true));
        assert!(response.get("result").is_some(), "an incomplete pass still carries its diff");

        assert_eq!(pass_response(id.clone(), true, complete.clone())["incomplete"], serde_json::json!(false));
        assert_eq!(
            pass_response(id.clone(), false, incomplete)["incomplete"],
            serde_json::json!(false),
            "a per-file pass has no completion flag to protect"
        );
        assert_eq!(pass_response(id, false, complete)["incomplete"], serde_json::json!(false));
    }

    /// An incomplete whole-project pass carries the engine's reason for core
    /// to record; a per-file one does not, for the same reason it carries no
    /// flag.
    #[test]
    fn an_incomplete_whole_project_pass_says_why() {
        let id = serde_json::json!(7);
        let answer = SemanticAnswer::incomplete_because(FileChangeDiff::default(), "the server exited");

        let response = pass_response(id.clone(), true, answer.clone());
        assert_eq!(response["incompleteReason"], serde_json::json!("the server exited"));
        assert!(pass_response(id.clone(), false, answer).get("incompleteReason").is_none());
        let complete = SemanticAnswer::complete(FileChangeDiff::default());
        assert!(pass_response(id, true, complete).get("incompleteReason").is_none());
    }

    /// The diff an incomplete pass did manage travels with it - core commits
    /// it and declines to record the pass, which is the whole point of the
    /// field being beside `result` rather than replacing it.
    #[test]
    fn an_incomplete_pass_still_carries_what_it_resolved() {
        use crate::graph::{FileGraphBuilder, NodeSpec};
        use g_mesh_wire::{NodeKind, Position, Range};

        let path = RelPath::new("a.toy");
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &path);
        let range = Range { start: Position { line: 0, col: 0 }, end: Position { line: 0, col: 1 } };
        builder.add_node(NodeSpec::new(NodeKind::Function, "a", "a", range));
        let graph = builder.finish();

        let diff = FileChangeDiff { upsert_nodes: graph.nodes, ..Default::default() };
        let response = pass_response(serde_json::json!(1), true, SemanticAnswer::incomplete(diff));
        assert_eq!(response["incomplete"], serde_json::json!(true));
        assert_eq!(response["result"]["upsertNodes"].as_array().map(Vec::len), Some(1));
    }

    /// An extractor that panics on one file costs that file and nothing else -
    /// the walk keeps going, and the process is still standing afterwards.
    ///
    /// The panic hook is silenced for the duration, because the default one
    /// prints the panic to stderr and a *deliberately* panicking test would
    /// otherwise look like a failing one in the output.
    #[test]
    fn a_panicking_extractor_costs_its_file_and_not_the_process() {
        use crate::graph::{FileGraphBuilder, NodeSpec};
        use g_mesh_wire::{NodeKind, Position, Range};

        struct Explodes;

        impl crate::Extractor for Explodes {
            const LANGUAGE: &'static str = "boom";
            type Project = ();

            fn load_project(&self, _root: &Path) -> anyhow::Result<()> {
                Ok(())
            }

            fn extract(&self, _project: &(), path: &RelPath, _source: &str) -> FileGraph {
                assert_ne!(path.as_str(), "bad.boom", "deliberate panic for the test below");
                let mut builder = FileGraphBuilder::new("boom", "boom-parser", path);
                let range = Range { start: Position { line: 0, col: 0 }, end: Position { line: 0, col: 0 } };
                builder.file_node(range);
                builder.add_node(NodeSpec::new(NodeKind::Function, "ok", "ok", range).public());
                builder.finish()
            }
        }

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let exploded = extract_caught(&Explodes, &(), &RelPath::new("bad.boom"), "", "boom");
        std::panic::set_hook(previous);
        assert!(exploded.is_none(), "a panic must not be reported as a graph");

        // The same extractor, still usable, on the next file.
        let fine = extract_caught(&Explodes, &(), &RelPath::new("good.boom"), "", "boom")
            .expect("the file after the panicking one is extracted normally");
        assert_eq!(fine.nodes.len(), 2);
    }

    /// GM-433: a `workspaceChanged` frame is forwarded to a started semantic
    /// engine, so its server's earlier readiness is not trusted for the pass
    /// that follows - and the frame is still acknowledged.
    #[test]
    fn a_workspace_changed_frame_reaches_the_started_engine() {
        use crate::semantic::{SemanticEngine, SemanticEngineFactory};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct Nothing;

        impl crate::Extractor for Nothing {
            const LANGUAGE: &'static str = "toy";
            type Project = ();

            fn load_project(&self, _root: &Path) -> anyhow::Result<()> {
                Ok(())
            }

            fn extract(&self, _project: &(), path: &RelPath, _source: &str) -> FileGraph {
                crate::graph::FileGraphBuilder::new("toy", "toy-parser", path).finish()
            }
        }

        struct Watching {
            changes: Arc<AtomicUsize>,
        }

        impl SemanticEngine for Watching {
            fn answer(&mut self, _files: &[RelPath], _index: &SdkIndex) -> anyhow::Result<SemanticAnswer> {
                Ok(SemanticAnswer::complete(FileChangeDiff::default()))
            }

            fn workspace_changed(&mut self) {
                self.changes.fetch_add(1, Ordering::SeqCst);
            }
        }

        let changes = Arc::new(AtomicUsize::new(0));
        let changed = Arc::clone(&changes);
        let factory: SemanticEngineFactory = Box::new(move |_root| {
            Ok(Box::new(Watching { changes: Arc::clone(&changed) }) as Box<dyn SemanticEngine>)
        });
        let spec = ResolvedSpec::resolve_from(&PluginSpec::new("toy", "0.0.0", &[".toy"]), None);
        let root = PathBuf::from("/projects/toy");
        let mut session = Session {
            extractor: &Nothing,
            spec: &spec,
            root: root.clone(),
            root_real: None,
            project: None,
            index: SdkIndex::new(),
            engine: LazyEngine::new("toy", Some(factory)),
            project_hydrated: false,
        };
        // Only a started engine has a readiness to forget.
        session.engine.answer(&[], &SdkIndex::new(), &root);

        let frame = serde_json::json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": 3,
            "method": "workspaceChanged",
            "params": { "filePath": "Cargo.toml" },
        });
        let mut out = Vec::new();
        session.handle(frame.to_string().as_bytes(), &mut out).unwrap();

        assert_eq!(changes.load(Ordering::SeqCst), 1, "the started engine is told about the change");
        let written = String::from_utf8(out).unwrap();
        assert!(written.contains(r#""acknowledged":true"#), "{written}");
    }

    /// A `prepareSemanticPass` notification starts the engine and hands it
    /// the cue, before any `semanticPass` - and, being a notification, writes
    /// nothing back.
    ///
    /// Control: remove the `"prepareSemanticPass"` arm from
    /// `Session::handle` and the frame falls through to the unknown-method
    /// arm, starting nothing.
    #[test]
    fn a_prepare_frame_starts_the_engine_before_any_pass() {
        use crate::semantic::{SemanticEngine, SemanticEngineFactory};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct Nothing;

        impl crate::Extractor for Nothing {
            const LANGUAGE: &'static str = "toy";
            type Project = ();

            fn load_project(&self, _root: &Path) -> anyhow::Result<()> {
                Ok(())
            }

            fn extract(&self, _project: &(), path: &RelPath, _source: &str) -> FileGraph {
                crate::graph::FileGraphBuilder::new("toy", "toy-parser", path).finish()
            }
        }

        struct Preparing {
            prepared: Arc<AtomicUsize>,
        }

        impl SemanticEngine for Preparing {
            fn answer(&mut self, _files: &[RelPath], _index: &SdkIndex) -> anyhow::Result<SemanticAnswer> {
                Ok(SemanticAnswer::complete(FileChangeDiff::default()))
            }

            fn prepare(&mut self) {
                self.prepared.fetch_add(1, Ordering::SeqCst);
            }
        }

        let starts = Arc::new(AtomicUsize::new(0));
        let prepared = Arc::new(AtomicUsize::new(0));
        let (started, told) = (Arc::clone(&starts), Arc::clone(&prepared));
        let factory: SemanticEngineFactory = Box::new(move |_root| {
            started.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Preparing { prepared: Arc::clone(&told) }) as Box<dyn SemanticEngine>)
        });
        let spec = ResolvedSpec::resolve_from(&PluginSpec::new("toy", "0.0.0", &[".toy"]), None);
        let mut session = Session {
            extractor: &Nothing,
            spec: &spec,
            root: PathBuf::from("/projects/toy"),
            root_real: None,
            project: None,
            index: SdkIndex::new(),
            engine: LazyEngine::new("toy", Some(factory)),
            project_hydrated: false,
        };

        let frame = serde_json::json!({ "jsonrpc": JSONRPC_VERSION, "method": "prepareSemanticPass" });
        let mut out = Vec::new();
        session.handle(frame.to_string().as_bytes(), &mut out).unwrap();

        assert_eq!(starts.load(Ordering::SeqCst), 1, "the engine is started by the notification");
        assert_eq!(prepared.load(Ordering::SeqCst), 1, "and told to prepare");
        assert!(out.is_empty(), "a notification is answered with nothing");
    }

    #[test]
    fn a_files_nodes_are_written_before_its_edges() {
        use crate::graph::{FileGraphBuilder, NodeSpec};
        use g_mesh_wire::{NodeKind, Position, Range};

        let path = RelPath::new("a.toy");
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &path);
        let range = Range { start: Position { line: 0, col: 0 }, end: Position { line: 1, col: 0 } };
        let file = builder.file_node(range);
        let a = builder.add_node(NodeSpec::new(NodeKind::Function, "a", "a", range).public());
        builder.defines(&file, &a, true);

        let mut out = Vec::new();
        write_graph(&mut out, &builder.finish()).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
        assert_eq!(lines.len(), 4, "two nodes and two edges");
        assert!(lines[0].contains("\"kind\":\"File\""), "{:?}", lines[0]);
        assert!(lines[1].contains("\"kind\":\"Function\""), "{:?}", lines[1]);
        assert!(lines[2].contains("\"DEFINES\""), "{:?}", lines[2]);
        assert!(lines[3].contains("\"EXPORTS\""), "{:?}", lines[3]);
    }

    // --- GM-487: a per-file pass hydrates the whole project -----------------

    /// Declares one public function named after its file's stem, so every
    /// extraction has nodes a diff can carry.
    struct Declares;

    impl crate::Extractor for Declares {
        const LANGUAGE: &'static str = "toy";
        type Project = ();

        fn load_project(&self, _root: &Path) -> anyhow::Result<()> {
            Ok(())
        }

        fn extract(&self, _project: &(), path: &RelPath, source: &str) -> FileGraph {
            use crate::graph::{FileGraphBuilder, NodeSpec};
            use g_mesh_wire::{NodeKind, Position, Range};

            let mut builder = FileGraphBuilder::new("toy", "toy-parser", path);
            let range = Range { start: Position { line: 0, col: 0 }, end: Position { line: 1, col: 0 } };
            builder.file_node(range);
            let name = path.as_str().trim_end_matches(".toy").to_string();
            // The source's text is part of the signature, so two versions of
            // one file extract to different graphs.
            builder.add_node(
                NodeSpec::new(NodeKind::Function, name.clone(), name, range)
                    .signature(source.trim())
                    .public(),
            );
            builder.finish()
        }
    }

    /// An engine that records the paths its index held on each pass.
    struct Recording {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Vec<RelPath>>>>,
    }

    impl crate::semantic::SemanticEngine for Recording {
        fn answer(&mut self, _files: &[RelPath], index: &SdkIndex) -> anyhow::Result<SemanticAnswer> {
            self.seen.lock().unwrap().push(index.paths());
            Ok(SemanticAnswer::complete(FileChangeDiff::default()))
        }
    }

    /// A scratch project of `files` on disk, removed on drop.
    struct Project(PathBuf);

    impl Project {
        fn new(tag: &str, files: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir().join(format!("g-mesh-run-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for (path, text) in files {
                std::fs::write(dir.join(path), text).unwrap();
            }
            Project(dir)
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Sends one request to `session` and returns its `result`.
    fn request(
        session: &mut Session<'_, Declares>,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let frame =
            serde_json::json!({ "jsonrpc": JSONRPC_VERSION, "id": 1, "method": method, "params": params });
        let mut out = Vec::new();
        session.handle(frame.to_string().as_bytes(), &mut out).unwrap();
        let written = String::from_utf8(out).unwrap();
        let (_, body) = written.split_once("\r\n\r\n").expect("one framed response");
        let response: serde_json::Value = serde_json::from_str(body).unwrap();
        response["result"].clone()
    }

    /// A fresh control-plane session over `root`, with a [`Recording`] engine.
    fn fresh_session<'a>(
        spec: &'a ResolvedSpec,
        root: &Path,
        seen: &std::sync::Arc<std::sync::Mutex<Vec<Vec<RelPath>>>>,
    ) -> Session<'a, Declares> {
        let seen = std::sync::Arc::clone(seen);
        let factory: crate::semantic::SemanticEngineFactory = Box::new(move |_root| {
            Ok(Box::new(Recording { seen: std::sync::Arc::clone(&seen) })
                as Box<dyn crate::semantic::SemanticEngine>)
        });
        Session {
            extractor: &Declares,
            spec,
            root: root.to_path_buf(),
            root_real: std::fs::canonicalize(root).ok(),
            project: None,
            index: SdkIndex::new(),
            engine: LazyEngine::new("toy", Some(factory)),
            project_hydrated: false,
        }
    }

    fn toy_spec() -> ResolvedSpec {
        ResolvedSpec::resolve_from(&PluginSpec::new("toy", "0.0.0", &[".toy"]), None)
    }

    /// GM-487 Fix 1: the first per-file `semanticPass` of a process hands its
    /// engine an index holding the whole project, so an answer landing in
    /// another file has a node to land on.
    ///
    /// Control: drop the `self.hydrate(&[])` call in the `"semanticPass"` arm
    /// and the engine sees `a.toy` alone.
    #[test]
    fn a_per_file_pass_hands_its_engine_the_whole_project() {
        let project = Project::new("hydrate", &[("a.toy", "a v1\n"), ("b.toy", "b v1\n")]);
        let spec = toy_spec();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = fresh_session(&spec, &project.0, &seen);

        request(&mut session, "semanticPass", serde_json::json!({ "filePaths": ["a.toy"] }));

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one pass, one answer");
        assert_eq!(
            seen[0],
            vec![RelPath::new("a.toy"), RelPath::new("b.toy")],
            "the per-file pass's index holds every file of the project"
        );
    }

    /// GM-487 Fix 1's baseline hazard: a file a semantic pass hydrated from
    /// disk is news to core, so the `fileChanged` that follows - here at the
    /// very text that was hydrated - answers a complete diff carrying the
    /// file's nodes, not "nothing changed".
    ///
    /// Control: make `SdkIndex::insert_unreported` record `reported: true`
    /// (or restore the plain `source ==` short-cut in `file_changed`) and the
    /// diff is empty.
    #[test]
    fn a_file_changed_after_hydration_sends_a_complete_diff() {
        let project = Project::new("baseline", &[("a.toy", "a v1\n"), ("b.toy", "b v2\n")]);
        let spec = toy_spec();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = fresh_session(&spec, &project.0, &seen);

        request(&mut session, "semanticPass", serde_json::json!({ "filePaths": ["a.toy"] }));
        assert!(session.index.entry(&RelPath::new("b.toy")).is_some(), "the pass hydrated b.toy");

        let diff: FileChangeDiff = serde_json::from_value(request(
            &mut session,
            "fileChanged",
            serde_json::json!({ "filePath": "b.toy" }),
        ))
        .unwrap();
        assert!(diff.complete, "a hydrated entry is no baseline: {diff:#?}");
        assert!(
            diff.upsert_nodes.iter().any(|node| node.name == "b" && node.file_path == "b.toy"),
            "the diff carries b.toy's nodes: {diff:#?}"
        );

        // Reported now: the same text again is the ordinary short-cut.
        let again: FileChangeDiff = serde_json::from_value(request(
            &mut session,
            "fileChanged",
            serde_json::json!({ "filePath": "b.toy" }),
        ))
        .unwrap();
        assert!(crate::is_empty_diff(&again), "a reported entry is a baseline again: {again:#?}");
    }

    /// The same hazard on a deletion: a hydrated file removed from disk
    /// answers a complete (empty) diff, so core drops whatever it holds for
    /// the file rather than only the nodes this process hydrated.
    ///
    /// Control: diff the deletion against `self.index.graph(path)` instead of
    /// `self.index.baseline(path)` in `file_changed` and the diff is not
    /// `complete`.
    #[test]
    fn a_hydrated_file_deleted_from_disk_sends_a_complete_diff() {
        let project = Project::new("deleted", &[("a.toy", "a v1\n"), ("b.toy", "b v1\n")]);
        let spec = toy_spec();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = fresh_session(&spec, &project.0, &seen);

        request(&mut session, "semanticPass", serde_json::json!({ "filePaths": ["a.toy"] }));
        assert!(session.index.entry(&RelPath::new("b.toy")).is_some(), "the pass hydrated b.toy");
        std::fs::remove_file(project.0.join("b.toy")).unwrap();

        let diff: FileChangeDiff = serde_json::from_value(request(
            &mut session,
            "fileChanged",
            serde_json::json!({ "filePath": "b.toy" }),
        ))
        .unwrap();
        assert!(diff.complete, "a deletion of a hydrated file replaces all core holds for it: {diff:#?}");
        assert!(diff.upsert_nodes.is_empty(), "{diff:#?}");
        assert!(session.index.entry(&RelPath::new("b.toy")).is_none());
    }

    // `Session::indexed_spelling` remaps a `fileChanged` spelled through an
    // in-root link onto the real spelling the index holds
    // (docs/adr/0025-project-walk-follows-symlinks.md). B9 and S7-* name the
    // behaviours in docs/architecture/gm-349-sdk-walk-symlinks.md.

    /// `fileChanged` for `path`, answered.
    fn file_changed(session: &mut Session<'_, Declares>, path: &str) -> FileChangeDiff {
        serde_json::from_value(request(session, "fileChanged", serde_json::json!({ "filePath": path })))
            .unwrap()
    }

    /// The distinct `file_path`s of the nodes `diff` upserts, sorted.
    fn upserted_paths(diff: &FileChangeDiff) -> Vec<String> {
        let mut paths: Vec<String> = diff.upsert_nodes.iter().map(|node| node.file_path.clone()).collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// A [`Project`] holding `real/a.toy`, with `alias -> real`.
    #[cfg(unix)]
    fn aliased_project(tag: &str) -> Project {
        let project = Project::new(tag, &[]);
        std::fs::create_dir_all(project.0.join("real")).unwrap();
        std::fs::write(project.0.join("real/a.toy"), "a v1\n").unwrap();
        std::os::unix::fs::symlink("real", project.0.join("alias")).unwrap();
        project
    }

    /// B9 / S7-a: an edit reported under the link spelling of an indexed
    /// file updates the real spelling's entry and adds no second one; the
    /// same text again under the alias is the ordinary unchanged short-cut.
    ///
    /// Control: make `indexed_spelling` return `None` -> the diff's nodes
    /// are under `alias/a.toy`, the index gains that entry, and the resend
    /// is not empty.
    #[cfg(unix)]
    #[test]
    fn a_link_spelled_event_for_an_indexed_file_updates_its_real_spelling() {
        let project = aliased_project("remap");
        let spec = toy_spec();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = fresh_session(&spec, &project.0, &seen);
        file_changed(&mut session, "real/a.toy");
        std::fs::write(project.0.join("real/a.toy"), "a v2\n").unwrap();

        let diff = file_changed(&mut session, "alias/a.toy");
        assert_eq!(upserted_paths(&diff), vec!["real/a.toy"], "the edit is news, under the real spelling");
        assert_eq!(session.index.paths(), vec![RelPath::new("real/a.toy")], "no second entry");

        let again = file_changed(&mut session, "alias/a.toy");
        assert!(crate::is_empty_diff(&again), "unchanged text under the alias: {again:#?}");
    }

    /// S7-b: a file indexed only under a link spelling (its real spelling is
    /// not in the index) is handled as spelled.
    ///
    /// Control: drop the `self.index.entry(&real).is_some()` condition in
    /// `indexed_spelling` -> the nodes and the entry are under `real/a.toy`.
    #[cfg(unix)]
    #[test]
    fn a_link_spelled_event_for_a_file_not_indexed_by_its_real_spelling_is_handled_as_spelled() {
        let project = aliased_project("remap-alias-only");
        let spec = toy_spec();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = fresh_session(&spec, &project.0, &seen);

        let diff = file_changed(&mut session, "alias/a.toy");
        assert_eq!(upserted_paths(&diff), vec!["alias/a.toy"], "{diff:#?}");
        assert_eq!(session.index.paths(), vec![RelPath::new("alias/a.toy")]);
    }

    /// S7-c: a path through a link resolving outside the root is handled as
    /// spelled.
    ///
    /// Control: in `indexed_spelling`, fall back to the absolute real path
    /// when `RelPath::relative_to(root_real, ..)` fails, and drop the indexed
    /// check -> the event lands under an absolute spelling, not `ext/a.toy`.
    #[cfg(unix)]
    #[test]
    fn a_link_spelled_event_resolving_outside_the_root_is_handled_as_spelled() {
        let outside = Project::new("remap-outside-target", &[("a.toy", "a v1\n")]);
        let project = Project::new("remap-outside", &[]);
        std::os::unix::fs::symlink(&outside.0, project.0.join("ext")).unwrap();
        let spec = toy_spec();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = fresh_session(&spec, &project.0, &seen);

        let diff = file_changed(&mut session, "ext/a.toy");
        assert_eq!(upserted_paths(&diff), vec!["ext/a.toy"], "{diff:#?}");
        assert_eq!(session.index.paths(), vec![RelPath::new("ext/a.toy")]);
    }

    /// S7-d: a path that no longer resolves is handled as spelled, so the
    /// deletion reaches the entry it names - here the link spelling, with the
    /// real spelling indexed too.
    ///
    /// Control: make `indexed_spelling` resolve a missing file through its
    /// parent (`canonicalize(parent).join(file_name)`) -> the deletion
    /// removes `real/a.toy` and leaves `alias/a.toy`.
    #[cfg(unix)]
    #[test]
    fn a_deleted_path_is_handled_as_spelled() {
        let project = aliased_project("remap-deleted");
        let spec = toy_spec();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = fresh_session(&spec, &project.0, &seen);
        // Alias first: its real spelling is not indexed yet, so both are.
        file_changed(&mut session, "alias/a.toy");
        file_changed(&mut session, "real/a.toy");
        assert_eq!(session.index.paths(), vec![RelPath::new("alias/a.toy"), RelPath::new("real/a.toy")]);
        std::fs::remove_file(project.0.join("real/a.toy")).unwrap();

        let diff = file_changed(&mut session, "alias/a.toy");
        assert!(!diff.delete_node_ids.is_empty(), "a deletion: {diff:#?}");
        assert_eq!(session.index.paths(), vec![RelPath::new("real/a.toy")], "the alias entry went");
    }
}

#[cfg(test)]
mod presence_tests {
    //! GM-515: `filesCreated` and the presence hook
    //! ([`crate::Extractor::file_presence_changed`]). Numbers name the
    //! behaviours in docs/architecture/gm-515-batch-presence.md.

    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    /// An extractor whose project model is the set of `.toy` files that
    /// exist: built by `load_project` from the walk, kept by the hook. A
    /// line `import X` extracts to `X=resolved` in the file's signature when
    /// `X` is in the set, `X=unresolved` otherwise. Every hook call and
    /// every extraction is recorded; the hook panics on `panic.toy`, the
    /// extraction on a source containing `panic-extract`.
    #[derive(Default)]
    struct Presence {
        hooked: Mutex<Vec<(String, bool)>>,
        extracted: Mutex<Vec<String>>,
    }

    impl crate::Extractor for Presence {
        const LANGUAGE: &'static str = "toy";
        type Project = BTreeSet<String>;

        fn load_project(&self, root: &Path) -> anyhow::Result<BTreeSet<String>> {
            Ok(crate::walk::walk_project(root, &[".toy".to_string()], &[])
                .into_iter()
                .map(|path| path.as_str().to_string())
                .collect())
        }

        fn extract(&self, project: &BTreeSet<String>, path: &RelPath, source: &str) -> FileGraph {
            use crate::graph::{FileGraphBuilder, NodeSpec};
            use g_mesh_wire::{NodeKind, Position, Range};

            self.extracted.lock().unwrap().push(path.as_str().to_string());
            assert!(!source.contains("panic-extract"), "the test extractor panics on panic-extract");
            let imports: Vec<String> = source
                .lines()
                .filter_map(|line| line.strip_prefix("import "))
                .map(|target| {
                    let state = if project.contains(target.trim()) { "resolved" } else { "unresolved" };
                    format!("{}={state}", target.trim())
                })
                .collect();
            let mut builder = FileGraphBuilder::new("toy", "toy-parser", path);
            let range = Range { start: Position { line: 0, col: 0 }, end: Position { line: 1, col: 0 } };
            builder.file_node(range);
            let name = path.as_str().trim_end_matches(".toy").to_string();
            builder.add_node(
                NodeSpec::new(NodeKind::Function, name.clone(), name, range)
                    .signature(imports.join(" "))
                    .public(),
            );
            builder.finish()
        }

        fn file_presence_changed(&self, project: &mut BTreeSet<String>, path: &RelPath, present: bool) {
            self.hooked.lock().unwrap().push((path.as_str().to_string(), present));
            assert!(!path.as_str().ends_with("panic.toy"), "the test hook panics on panic.toy");
            if present {
                project.insert(path.as_str().to_string());
            } else {
                project.remove(path.as_str());
            }
        }
    }

    /// A scratch root, removed on drop.
    struct Root(PathBuf);

    impl Root {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("g-mesh-presence-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Root(dir)
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn toy_spec() -> ResolvedSpec {
        ResolvedSpec::resolve_from(&PluginSpec::new("toy", "0.0.0", &[".toy"]), None)
    }

    /// A session as `control_plane` starts one: project model loaded from
    /// the disk before the first frame.
    fn started<'a>(extractor: &'a Presence, spec: &'a ResolvedSpec, root: &Path) -> Session<'a, Presence> {
        let mut session = Session {
            extractor,
            spec,
            root: root.to_path_buf(),
            root_real: std::fs::canonicalize(root).ok(),
            project: None,
            index: SdkIndex::new(),
            engine: LazyEngine::new("toy", None),
            project_hydrated: false,
        };
        session.load_project();
        session
    }

    /// Hands `session` one frame and returns what it wrote, unframed.
    fn send(session: &mut Session<'_, Presence>, frame: serde_json::Value) -> Option<serde_json::Value> {
        let mut out = Vec::new();
        session.handle(frame.to_string().as_bytes(), &mut out).unwrap();
        if out.is_empty() {
            return None;
        }
        let written = String::from_utf8(out).unwrap();
        let (_, body) = written.split_once("\r\n\r\n").expect("one framed response");
        Some(serde_json::from_str(body).unwrap())
    }

    /// `filesCreated` for `paths`, as core sends it: a notification.
    fn files_created(session: &mut Session<'_, Presence>, paths: serde_json::Value) {
        let written = send(
            session,
            serde_json::json!({ "jsonrpc": JSONRPC_VERSION, "method": "filesCreated", "params": { "filePaths": paths } }),
        );
        assert_eq!(written, None, "a notification is answered with nothing");
    }

    /// `fileChanged` for `path`, answered.
    fn file_changed(session: &mut Session<'_, Presence>, path: &str) -> FileChangeDiff {
        let response = send(
            session,
            serde_json::json!({
                "jsonrpc": JSONRPC_VERSION, "id": 1, "method": "fileChanged", "params": { "filePath": path }
            }),
        )
        .expect("a request is answered");
        serde_json::from_value(response["result"].clone()).unwrap()
    }

    /// The signature of the node named `name` that `diff` upserts.
    fn signature_of(diff: &FileChangeDiff, name: &str) -> String {
        let node = diff.upsert_nodes.iter().find(|node| node.name == name).unwrap_or_else(|| {
            panic!("the diff upserts no node {name}: {diff:#?}");
        });
        node.signature.clone().unwrap_or_default()
    }

    fn hooked(extractor: &Presence) -> Vec<(String, bool)> {
        extractor.hooked.lock().unwrap().clone()
    }

    /// 9: a new importer of a new file, created in one batch, resolves in
    /// that batch: `filesCreated [a, b]` puts both in the model before `a`
    /// is extracted, and each file is extracted exactly once.
    ///
    /// Control: make the `"filesCreated"` arm only `acknowledge` -> `a`'s
    /// import is unresolved.
    #[test]
    fn a_same_batch_new_importer_of_a_new_file_resolves_in_that_batch() {
        let root = Root::new("same-batch");
        let (extractor, spec) = (Presence::default(), toy_spec());
        let mut session = started(&extractor, &spec, &root.0);
        root.write("a.toy", "import b.toy\n");
        root.write("b.toy", "\n");

        files_created(&mut session, serde_json::json!(["a.toy", "b.toy"]));
        let a = file_changed(&mut session, "a.toy");
        file_changed(&mut session, "b.toy");

        assert_eq!(signature_of(&a, "a"), "b.toy=resolved", "{a:#?}");
        assert_eq!(*extractor.extracted.lock().unwrap(), vec!["a.toy", "b.toy"], "each extracted once");
    }

    /// 10: the degradation 9 is measured against - the same batch with no
    /// `filesCreated` (a plugin core never announces to) leaves `a`'s import
    /// unresolved: `b` is not in the model when `a` is extracted.
    ///
    /// Control: none in production code; this pins the baseline that makes
    /// 9's control observable (if `fileChanged` read the disk, both would
    /// resolve and 9 would prove nothing).
    #[test]
    fn without_the_notification_the_same_batch_importer_stays_unresolved() {
        let root = Root::new("no-announce");
        let (extractor, spec) = (Presence::default(), toy_spec());
        let mut session = started(&extractor, &spec, &root.0);
        root.write("a.toy", "import b.toy\n");
        root.write("b.toy", "\n");

        let a = file_changed(&mut session, "a.toy");
        file_changed(&mut session, "b.toy");

        assert_eq!(signature_of(&a, "a"), "b.toy=unresolved", "{a:#?}");
    }

    /// 11: `fileChanged` applies presence before extracting, for a creation
    /// and for a deletion: after `fileChanged b` (created) the importer's
    /// next extraction resolves; after `fileChanged b` (deleted) it does not.
    ///
    /// Control: remove the `self.presence_changed(path, source.is_some())`
    /// call in `Session::file_changed` -> the importer stays unresolved
    /// after the creation.
    #[test]
    fn file_changed_applies_presence_before_extracting() {
        let root = Root::new("file-changed");
        root.write("a.toy", "import b.toy\n");
        let (extractor, spec) = (Presence::default(), toy_spec());
        let mut session = started(&extractor, &spec, &root.0);
        assert_eq!(signature_of(&file_changed(&mut session, "a.toy"), "a"), "b.toy=unresolved");

        root.write("b.toy", "\n");
        file_changed(&mut session, "b.toy");
        root.write("a.toy", "import b.toy\n# edited\n");
        let created = file_changed(&mut session, "a.toy");
        assert_eq!(signature_of(&created, "a"), "b.toy=resolved", "{created:#?}");

        std::fs::remove_file(root.0.join("b.toy")).unwrap();
        file_changed(&mut session, "b.toy");
        root.write("a.toy", "import b.toy\n");
        let deleted = file_changed(&mut session, "a.toy");
        assert_eq!(signature_of(&deleted, "a"), "b.toy=unresolved", "{deleted:#?}");
    }

    /// 11a: the hook runs before extracting the *same* file: a new file
    /// that imports itself, never announced by `filesCreated`, resolves the
    /// import on its own `fileChanged`.
    ///
    /// Control (C4a): move `self.presence_changed(path, source.is_some())` in
    /// `Session::file_changed` after `extract_caught` -> `c` is not in the
    /// model while it is extracted, so `c.toy=unresolved`.
    #[test]
    fn file_changed_applies_presence_before_extracting_the_same_file() {
        let root = Root::new("self-import");
        let (extractor, spec) = (Presence::default(), toy_spec());
        let mut session = started(&extractor, &spec, &root.0);
        root.write("c.toy", "import c.toy\n");

        let c = file_changed(&mut session, "c.toy");

        assert_eq!(signature_of(&c, "c"), "c.toy=resolved", "{c:#?}");
    }

    /// 11b (design D5, "before the unchanged-text short-circuit"): a
    /// `fileChanged` whose text matches the reported entry extracts nothing
    /// but still calls the hook.
    ///
    /// Control (C4a, or moving the hook below the short-circuit): the second
    /// `fileChanged` returns before the hook -> `b` is hooked once.
    #[test]
    fn an_unchanged_file_changed_still_applies_presence() {
        let root = Root::new("short-circuit");
        let (extractor, spec) = (Presence::default(), toy_spec());
        let mut session = started(&extractor, &spec, &root.0);
        root.write("b.toy", "\n");

        file_changed(&mut session, "b.toy");
        let unchanged = file_changed(&mut session, "b.toy");

        assert_eq!(unchanged, FileChangeDiff::default(), "unchanged text is short-circuited");
        assert_eq!(*extractor.extracted.lock().unwrap(), vec!["b.toy"], "extracted once");
        assert_eq!(hooked(&extractor), vec![("b.toy".to_string(), true), ("b.toy".to_string(), true)]);
    }

    /// 11c: a file whose extraction fails is still applied as present, so its
    /// importers resolve although it has no graph yet.
    ///
    /// Control (C4a): the failed extraction returns before the hook -> `b`
    /// is never hooked and `a.toy`'s import stays unresolved.
    #[test]
    fn a_file_whose_extraction_fails_is_still_applied_as_present() {
        let root = Root::new("extract-fails");
        let (extractor, spec) = (Presence::default(), toy_spec());
        let mut session = started(&extractor, &spec, &root.0);
        root.write("b.toy", "panic-extract\n");
        root.write("a.toy", "import b.toy\n");

        let b = file_changed(&mut session, "b.toy");
        let a = file_changed(&mut session, "a.toy");

        assert_eq!(b, FileChangeDiff::default(), "a failed extraction sends nothing");
        assert_eq!(hooked(&extractor)[0], ("b.toy".to_string(), true));
        assert_eq!(signature_of(&a, "a"), "b.toy=resolved", "{a:#?}");
    }

    /// 12: a listed path that is no longer on disk (or is not UTF-8, which
    /// its own `fileChanged` reads as gone) is applied as absent.
    ///
    /// Control: hardcode `present = true` in `Session::files_created` ->
    /// `b` and `c` are hooked present and `a`'s imports resolve.
    #[test]
    fn a_listed_path_gone_from_disk_is_applied_as_absent() {
        let root = Root::new("gone");
        let (extractor, spec) = (Presence::default(), toy_spec());
        let mut session = started(&extractor, &spec, &root.0);
        root.write("a.toy", "import b.toy\nimport c.toy\n");
        std::fs::write(root.0.join("c.toy"), [0xff, 0xfe, 0x00]).unwrap();

        files_created(&mut session, serde_json::json!(["a.toy", "b.toy", "c.toy"]));
        let a = file_changed(&mut session, "a.toy");

        assert_eq!(
            hooked(&extractor)[..3],
            [("a.toy".to_string(), true), ("b.toy".to_string(), false), ("c.toy".to_string(), false)]
        );
        assert_eq!(signature_of(&a, "a"), "b.toy=unresolved c.toy=unresolved", "{a:#?}");
    }

    /// 13: one bad entry costs only itself. A non-string, an unclaimed
    /// extension and a path whose hook panics come first; the two good paths
    /// after them are still applied, and the next request is answered.
    ///
    /// Control: `return` instead of `continue` on an unclaimed path in
    /// `Session::files_created`, or call the hook without `presence_caught`'s
    /// `catch_unwind` -> `b`/`c` are not in the model (or the test panics).
    #[test]
    fn one_bad_entry_does_not_cost_the_others() {
        let root = Root::new("bad-entry");
        let (extractor, spec) = (Presence::default(), toy_spec());
        let mut session = started(&extractor, &spec, &root.0);
        for path in ["x.rs", "panic.toy", "b.toy", "c.toy"] {
            root.write(path, "\n");
        }
        root.write("a.toy", "import b.toy\nimport c.toy\n");

        files_created(&mut session, serde_json::json!([5, "x.rs", "panic.toy", "b.toy", "c.toy"]));
        let a = file_changed(&mut session, "a.toy");

        let hooked: Vec<String> = hooked(&extractor).into_iter().map(|(path, _)| path).collect();
        assert_eq!(hooked[..3], ["panic.toy", "b.toy", "c.toy"], "the unclaimed path never reaches the hook");
        assert_eq!(signature_of(&a, "a"), "b.toy=resolved c.toy=resolved", "{a:#?}");
    }

    /// 14 (D6) and E5: a link spelling whose real spelling is listed in the
    /// same notification records only the real spelling, once, though the
    /// real one is not indexed yet (created in this batch).
    ///
    /// Control: skip the remap in `Session::files_created` (use `spelled`
    /// directly) -> the hook also sees `alias/b.toy`; drop the `applied`
    /// check -> it sees `real/b.toy` twice.
    #[cfg(unix)]
    #[test]
    fn a_link_spelling_listed_with_its_real_spelling_records_only_the_real_one() {
        let root = Root::new("link-listed");
        let (extractor, spec) = (Presence::default(), toy_spec());
        std::fs::create_dir_all(root.0.join("real")).unwrap();
        std::os::unix::fs::symlink("real", root.0.join("alias")).unwrap();
        let mut session = started(&extractor, &spec, &root.0);
        root.write("real/b.toy", "\n");

        files_created(&mut session, serde_json::json!(["alias/b.toy", "real/b.toy"]));

        assert_eq!(hooked(&extractor), vec![("real/b.toy".to_string(), true)]);
    }

    /// E7: a link spelling whose real spelling is neither indexed nor listed
    /// is handled as spelled, the walk's rule for a file reachable only
    /// through a link.
    ///
    /// Control: remap unconditionally (drop the `.filter(..)` on
    /// `real_spelling` in `Session::files_created`) -> the hook sees
    /// `real/b.toy`.
    #[cfg(unix)]
    #[test]
    fn a_link_spelling_whose_real_spelling_is_neither_indexed_nor_listed_is_handled_as_spelled() {
        let root = Root::new("link-unlisted");
        let (extractor, spec) = (Presence::default(), toy_spec());
        std::fs::create_dir_all(root.0.join("real")).unwrap();
        std::os::unix::fs::symlink("real", root.0.join("alias")).unwrap();
        let mut session = started(&extractor, &spec, &root.0);
        root.write("real/b.toy", "\n");
        root.write("c.toy", "\n");

        files_created(&mut session, serde_json::json!(["alias/b.toy", "c.toy"]));

        assert_eq!(hooked(&extractor), vec![("alias/b.toy".to_string(), true), ("c.toy".to_string(), true)]);
    }

    /// E4: a path outside the root (absolute, or climbing out with `..`)
    /// never reaches the hook.
    ///
    /// Control: drop the `is_within_root` check in `Session::presence_changed`
    /// -> the hook sees both.
    #[test]
    fn a_path_outside_the_root_never_reaches_the_hook() {
        let root = Root::new("outside");
        let (extractor, spec) = (Presence::default(), toy_spec());
        let mut session = started(&extractor, &spec, &root.0);
        root.write("b.toy", "\n");

        files_created(&mut session, serde_json::json!(["../x.toy", "/abs/x.toy", "b.toy"]));

        assert_eq!(hooked(&extractor), vec![("b.toy".to_string(), true)]);
    }

    /// 15: `filesCreated` with an id is acknowledged; as a notification
    /// nothing is written (asserted by [`files_created`] in every test above).
    ///
    /// Control: remove the `self.acknowledge(out, id)` call in the arm ->
    /// no response.
    #[test]
    fn files_created_with_an_id_is_acknowledged() {
        let root = Root::new("ack");
        let (extractor, spec) = (Presence::default(), toy_spec());
        let mut session = started(&extractor, &spec, &root.0);

        let response = send(
            &mut session,
            serde_json::json!({
                "jsonrpc": JSONRPC_VERSION, "id": 7, "method": "filesCreated", "params": { "filePaths": [] }
            }),
        )
        .expect("a request is answered");
        assert_eq!(response["id"], 7);
        assert_eq!(response["result"], serde_json::json!({ "acknowledged": true }));
    }
}
