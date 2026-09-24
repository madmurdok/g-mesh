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
/// called on the first `semanticPass` and never before, which is the whole
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
/// they are the semantic tier's, and core has no field for them.
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
        project: None,
        index: SdkIndex::new(),
        engine: LazyEngine::new(&spec.language, semantic),
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
    /// `None` while the project model could not be built. Unlike the bulk
    /// walk, this is not fatal: a plugin that stops answering `fileChanged`
    /// leaves its language's whole index frozen at whatever it was, which is
    /// worse than answering from a model that is missing. Every request
    /// retries the load, so a `Cargo.toml` fixed in the editor heals the
    /// plugin without a restart.
    project: Option<E::Project>,
    index: SdkIndex,
    engine: LazyEngine,
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
            "semanticPass" => {
                // Test-only (GM-397): parks the pass before any engine
                // starts, blocking this thread as a long pass would.
                hold_point("semantic", &self.spec.language);
                let files: Vec<RelPath> = params
                    .and_then(|params| params.get("filePaths"))
                    .and_then(|paths| paths.as_array())
                    .map(|paths| paths.iter().filter_map(|path| path.as_str()).map(RelPath::new).collect())
                    .unwrap_or_default();
                self.hydrate(&files);
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
                self.load_project();
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

    /// Makes sure the index holds what the semantic pass is about to be asked
    /// about.
    ///
    /// The control-plane process never runs the bulk walk - that is a
    /// separate, one-shot process - so its index holds only the files it has
    /// been sent a `fileChanged` for. That is enough for a *per-file* pass and
    /// not nearly enough for the whole-project one core sends after the cold
    /// walk, where the plugin would otherwise answer about the two files
    /// someone happened to edit. So the files in scope are extracted here,
    /// once, before the engine is asked. (The JS/TS plugin solves the same
    /// problem by having its semantic pass walk the project itself.)
    ///
    /// Gated on a semantic tier existing, and reached only from a
    /// `semanticPass`: a plugin with no engine, and a plugin doing structural
    /// work, never pay for this. The extractions it adds are ordinary cache
    /// entries - a later `fileChanged` for one of these files diffs against
    /// it, which is correct and saves that file a re-extraction.
    fn hydrate(&mut self, files: &[RelPath]) {
        if !self.engine.configured() {
            return;
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
            let Some(project) = self.project.as_ref() else { return };
            if let Some(graph) = extract_caught(self.extractor, project, &path, &source, &self.spec.language)
            {
                self.index.insert(path, source, graph);
            }
        }
    }

    /// Reparses one file against what this process last saw of it.
    fn file_changed(&mut self, path: &RelPath) -> FileChangeDiff {
        if !self.claims(path) {
            eprintln!("[{}] ignoring {path}: this plugin does not claim its extension", self.spec.language);
            return FileChangeDiff::default();
        }

        let Some(source) = read_source(path, &self.root, &self.spec.language) else {
            // Gone, or unreadable: everything this plugin had for the file is
            // deleted. Forgetting it too means a re-creation is treated as a
            // first sighting rather than diffed against a stale baseline.
            let diff = diff_file(self.index.graph(path), &FileGraph::default());
            self.index.remove(path);
            return diff;
        };

        // Editors save unchanged buffers often enough for this to be worth a
        // string comparison: identical text cannot produce a different graph
        // from a pure extractor, so there is nothing to parse and nothing to
        // say.
        if self.index.source(path) == Some(source.as_str()) {
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

        let diff = diff_file(self.index.graph(path), &graph);
        self.index.insert(path.clone(), source, graph);
        diff
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
    serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": answer.diff,
        "incomplete": whole_project && !answer.complete,
    })
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
}
