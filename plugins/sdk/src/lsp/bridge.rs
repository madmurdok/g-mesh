//! The engine itself: open sites in, a semantic diff out, with a language
//! server in the middle and a budget around the whole thing.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use anyhow::Result;
use g_mesh_wire::{
    EdgeKind, FileChangeDiff, NodeKind, PlaceholderTarget, Position, Range, SourceTier, TargetKey,
    TargetScope, WireNode,
};
use serde_json::{json, Value};

use super::client::{LspClient, Poll};
use super::config::SemanticConfig;
use super::position::{file_uri, line_text, path_from_uri, without_verbatim_prefix, PositionEncoding};
use crate::graph::{EdgeSpec, FileGraphBuilder, OpenSite, OpenSiteKind, PlaceholderKind};
use crate::index::SdkIndex;
use crate::path::RelPath;
use crate::semantic::{SemanticAnswer, SemanticEngine};

/// Every number the bridge is bounded by.
///
/// # Decision 6: where these come from
///
/// The ceiling is core's own: a whole-project `semanticPass` is killed after
/// `max(20 minutes, 10s × file count)` (`core::daemon::plugin::
/// RoundTripTimeouts::semantic_pass_project_timeout`, whose doc comment has
/// the measurement behind both halves), and a per-file one after a flat 120
/// seconds. A bridge that runs to *that* limit does not report an incomplete
/// pass - it gets killed, its plugin is relaunched, and the answer it had
/// half-built is lost. So every budget here is deliberately inside core's,
/// with the margin spent on the work core's timer is also counting: the walk
/// and extraction the SDK does before the engine is even asked
/// (`run`'s `hydrate`), and writing the response.
///
/// - **`request` (10s).** One position question against a server that has
///   finished indexing, which is milliseconds of real work; ten seconds is
///   the same budget core grants a whole *file*, so one stuck site can cost
///   at most what one file was allowed. It exists for the site that makes a
///   server pathological, not for the normal case.
/// - **`max_sites` (1,000,000).** A ceiling on how long a question *list* may
///   get, and - since GM-319 - nothing else. It used to be justified as a
///   stand-in for the clock ("at eight questions in flight and even 100ms of
///   server work each, 20,000 sites is about four minutes"), which was right
///   arithmetic about a number that the first real repositories exceeded.
///   GM-314 counted what the plugins actually emit: Django 87,832 questions,
///   tokio 27,750, **g-mesh itself 21,995** - three of four corpora over the
///   old 20,000, so their passes were cut short, reported incomplete, and
///   `language_state.semanticPassAt` was never set for them. A ceiling that a
///   190-file repository walks through is not protecting against "an order of
///   magnitude larger than anything measured".
///
///   What bounds a pass is the deadline below, and the reason the count does
///   not need to help is `request`: no single question can hang past ten
///   seconds, so the work between here and the deadline is bounded whatever
///   the list's length. And unlike a count, the deadline *scales with the
///   project* - `per_file × files` - which is what a large repository needs
///   and what a constant can never express. Measured (GM-319, by driving a
///   real rust-analyzer over g-mesh's own 190 files rather than assuming):
///   20,000 questions in 182.5s cold at eight in flight, which is **73ms per
///   request**, and 16-19ms on warm runs. 73 is what is sized against here,
///   because sizing against the best case is how the previous ceiling was set.
///   At 73ms, `per_file` (8s) buys about 877 questions per file, against the
///   densest corpus measured at 116 (g-mesh; tokio is 35, Django 30). So the
///   clock has roughly 7.6× headroom over the worst real density *at every
///   project size*, and the ceiling's job is only to stop a pathological input
///   from materialising an unbounded `Vec`.
///
///   One million is where that job is done and no further: 11.4× Django's
///   list, 45× g-mesh's, and about 230MB - 144 bytes of `Question` each, which
///   `the_site_ceiling_bounds_what_one_question_list_can_cost` pins, plus
///   ~100 bytes of the strings it holds (2,210,074 bytes across g-mesh's
///   21,995 sites, measured). Paid only by a project that genuinely has a
///   million open sites, and whose index is already holding every one of them
///   when the list is built. It still *moves* the cliff rather than removing
///   it (a ~8,600-file repository at g-mesh's density would reach it), and
///   that is deliberate: a pass cut short reports itself
///   incomplete, which is the same honest, retryable answer a dead server or
///   a spent deadline gets. See `docs/architecture/multi-language-plugins.md`,
///   "Implementation notes (GM-319)", for why resuming was weighed and not
///   built.
/// - **`concurrency` (8).** Language servers answer requests concurrently, and
///   a single outstanding question spends most of a pass waiting for a
///   round trip rather than for an answer. Eight is enough to keep a warm
///   server busy and small enough that it does not multiply the peak memory
///   `[plugin] memoryLimitMb` is watching - the server does this work
///   simultaneously, and it is the server's memory that the limit samples.
/// - **`project_floor` (15 minutes) and `per_file` (8s × files).** Core's own
///   shape - a floor, or a per-file budget, whichever is larger - at three
///   quarters and four fifths of its numbers, so the bridge gives up and
///   reports an incomplete pass before core's timer gives up on the bridge.
/// - **`single_file` (90s)** against core's flat 120s, for the same reason.
/// - **`readiness` (10 minutes) and `settle` (2s).** See [`LspBridge`]'s doc
///   on readiness. The readiness wait is *inside* the pass budget too, so a
///   server that never loads costs a pass rather than a plugin.
///
/// None of these is configuration. They are this type's fields so a test can
/// drive a real timer with small values instead of faking the clock, which is
/// the same escape hatch `RoundTripTimeouts` documents for core's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// How long one `definition`/`implementation` request may take.
    pub request: Duration,
    /// How long one pass's question list may get. A ceiling on the list, not
    /// a budget for the work - see this type's doc.
    pub max_sites: usize,
    /// How many requests may be outstanding at once.
    pub concurrency: usize,
    /// The floor for a whole-project pass.
    pub project_floor: Duration,
    /// How much whole-project budget each file in scope buys, above the floor.
    pub per_file: Duration,
    /// The whole budget for a pass over an explicit list of files.
    pub single_file: Duration,
    /// How long to wait for a server to finish indexing before giving up on
    /// the pass.
    pub readiness: Duration,
    /// How long to wait for a server to report *any* progress before deciding
    /// it is one that never does.
    pub settle: Duration,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(10),
            max_sites: 1_000_000,
            concurrency: 8,
            project_floor: Duration::from_secs(15 * 60),
            per_file: Duration::from_secs(8),
            single_file: Duration::from_secs(90),
            readiness: Duration::from_secs(10 * 60),
            settle: Duration::from_secs(2),
        }
    }
}

/// How many times one bridge will start a server over its own lifetime.
///
/// A server that crashes mid-pass is not restarted *within* that pass - the
/// pass is already incomplete, and retrying inside a budget that is already
/// spent trades a partial answer for none. The next pass starts a fresh one,
/// because a crash is usually about one question rather than about the
/// server, and refusing to try again would turn a transient fault into a
/// permanently structural-only language until the daemon restarts.
///
/// It is bounded because the other shape exists too: a server that crashes on
/// *every* pass (an incompatible toolchain, a corrupt cache) would otherwise
/// spawn a process per pass forever, which on a per-file pass is a process
/// per file someone saves.
const MAX_SERVER_STARTS: u32 = 4;

/// A [`SemanticEngine`](crate::SemanticEngine) that answers a structural
/// pass's open sites by asking a language server.
///
/// # What it asks, and for what
///
/// - **Every open site** except [`OpenSiteKind::Implementation`] gets
///   `textDocument/definition` at the site's own position. The location that
///   comes back is mapped through [`SdkIndex::node_at`] to the node the
///   structural tier already emitted for that declaration, and the answer is
///   recorded the way every cross-file answer in this design is recorded: a
///   `qualifiedName`-keyed placeholder in the *asking* file, plus one edge
///   onto it with `source: semantic` and the engine label from the manifest.
///   Not a direct edge onto the declaration's node id - this process cannot
///   know that core has that file indexed, and an edge onto an id nothing
///   declares is a dangling row (the same reasoning `plugins/go/semantic.go`
///   records for its own answers).
/// - **Nodes whose `nativeKind` the manifest lists** in
///   `implementation_kinds` get `textDocument/implementation` at their name,
///   and every location that comes back becomes a `SUPERTYPE_OF` edge from
///   the implementing declaration to a placeholder addressing the anchor.
///   That is the direction `find_implementations` walks.
///
/// # An implementation answer names a *site*, and a site is not a declaration
///
/// GM-289 assumed the location an `implementation` answer carries is the
/// implementing declaration, so that [`SdkIndex::node_at`] turns it straight
/// into the edge's `from`. That holds for a language whose implementations
/// are declarations - Go's `implementation` on an interface points at the
/// concrete type - and it is false for every language that implements
/// through a construct of its own. rust-analyzer answers `implementation` on
/// a trait with one `LocationLink` per `impl` block, whose
/// `targetSelectionRange` is the implementing type's name *inside the impl
/// header*: for `impl Shape for Square` it is the `Square` on the `impl`
/// line, not the `struct Square` six lines earlier. No plugin emits a node
/// for an `impl` block - it declares nothing of its own - so `node_at` finds
/// only the enclosing `File`, which is never an answer's target, and the
/// sweep produced nothing at all.
///
/// So an implementation location that does not land on a declaration this
/// index knows gets **one more question**: `textDocument/definition` at that
/// very position, which is the server's own way of being asked "what is
/// this name". Measured on rust-analyzer: `implementation` on `Loud` returns
/// `shapes.rs:81:14` and `beta/src/main.rs:30:14`, both `impl` headers, and
/// `definition` at those two answers `shapes.rs:33:11` (`struct Circle`) and
/// `beta/src/main.rs:28:11` (`struct Megaphone`) - the declarations the edges
/// have to start at, one of them in another crate of the workspace.
///
/// The second hop is asked **only when the first answer did not resolve**, so
/// a server that already points at a declaration pays nothing, and it is
/// asked only for a location in a file this index holds - a definition
/// outside the project cannot become an edge whatever it says. It never
/// spawns a third: an `Implementor` answer is read as a declaration or
/// dropped.
///
/// **An `Implementation` open site is still deliberately not asked**, and the
/// sweep above is why it does not need to be. The site that exists today
/// (`plugins/rust`'s `impl Trait for T` where `T` is declared in another
/// file) records the position of `T`, the *subtype*, while the edge it wants
/// runs from `T` to the trait - and the site carries no position for the
/// trait at all, so `definition` there answers "where is T", the wrong end,
/// and `implementation` there answers "what implements T", a different
/// question. Neither reconstructs the missing edge. Asking the trait instead
/// reconstructs all of it, including the shapes no open site is recorded for
/// at all: `impl Trait for T` where the *trait* came through a glob import
/// gets no edge and no site from `plugins/rust`, and the sweep finds it
/// anyway. The bridge therefore counts these sites and says so rather than
/// inventing an answer. See `docs/architecture/multi-language-plugins.md`'s
/// "Implementation notes (GM-290)" for the argument in full.
///
/// # Readiness (decision 4)
///
/// A cold server answers `definition` with nothing while it indexes, and
/// recording that as "no target" would write the absence of every cross-file
/// edge in the project into an index that then calls itself complete. The
/// design doc states the rule - "the bridge waits for the server's progress
/// end (`$/progress` for indexing) before asking, within the pass timeout. An
/// empty answer before readiness is never recorded as 'no target'" - and
/// `$/progress` is optional and server-specific, so "waits for progress end"
/// needs a definition both for the servers that send none and for the ones
/// that send *several*.
///
/// **The rule is one quiet period, not one token** (GM-290): the server is
/// ready when it has had nothing in flight for [`Budgets::settle`]
/// continuously. If that has not happened within [`Budgets::readiness`] (or
/// the pass budget, whichever comes first), the pass is **incomplete** and
/// asks nothing - it does not record "no target" for a single site, which is
/// the whole point.
///
/// That single rule subsumes the two GM-289 wrote, and it replaces the first
/// of them because measurement showed it wrong. GM-289's rule 1 was "once a
/// progress has begun, the server is ready when all of them have ended", and
/// a real rust-analyzer reports its startup as a *sequence* of tokens with
/// gaps between them - `Fetching`, then `Building CrateGraph`, then `Roots
/// Scanned`, then `Building compile-time-deps`, `Loading proc-macros` and
/// `cachePriming`, each one begun after the previous ended. Traced against
/// the Rust plugin's own conformance fixture: the active set first emptied
/// 5.83s in, in a 0.28s gap, and the last token ended at 14.21s. A bridge
/// that believed the first gap asked every one of its questions ten seconds
/// early and was answered `null` to all of them - measured, not feared, in
/// the first run of that trace. A quiet period cannot be fooled by a gap
/// shorter than itself, and it still answers the no-progress case the way
/// GM-289's rule 2 did: a server that says nothing is quiet from birth, so it
/// is ready exactly [`Budgets::settle`] after the client starts.
///
/// The settle is paid **once per server**, not once per pass. After a server
/// has been quiet for a full settle it has shown which shape it is, and a
/// later pass waits only for whatever is in flight now - otherwise the
/// per-file pass that follows every edit would spend two of its ninety
/// seconds proving a point that was already settled.
///
/// **And a manifest may say the shape instead of waiting to be shown it**
/// (GM-310). `[plugin.semantic] readiness = "on-demand"` -
/// [`ServerReadiness::OnDemand`](super::config::ServerReadiness::OnDemand) -
/// starts the client with that latch already set, so the very first pass
/// waits only for whatever is in flight now. It is not a shorter settle:
/// [`Budgets::settle`] keeps its value and both of its other jobs, because
/// those jobs want the opposite number. Traced, pyright answers a cross-file
/// `definition` correctly 41-146ms *before* its own `$/progress` token
/// begins, and that token begins 1.26-1.33s after `didOpen` - so the number
/// that would make its start-up cheap is a number too small to keep GM-309's
/// post-edit deferral honest on the same server. One shape key and one
/// unchanged duration, rather than one duration asked to be two things.
///
/// What keeps the claim safe when it is *wrong* is everything this section
/// already describes, still running. `sync_documents` marks the client edited
/// after every `didOpen`, so when the first question of the first pass goes
/// out the client has been quiet for milliseconds; `run_pass` defers every
/// empty answer that arrives while the client has not been quiet for a full
/// settle, and re-asks it only after a *continuous* settle of quiet - which a
/// server that is genuinely indexing cannot hand out. An `on-demand` manifest
/// in front of a rust-analyzer therefore produces the same edges it produces
/// today, at the same moment, having spent one deferral per question instead
/// of one wait per pass. `tests/lsp_bridge.rs`'s
/// `an_indexing_server_is_not_believed_early_even_when_the_manifest_says_on_demand`
/// is that case, and it is the test that fails if any of the above is removed.
///
/// Readiness is not only a startup condition: a server may begin indexing
/// again mid-pass (it usually does, after `didChange`). An empty answer that
/// arrives before the server is quiet again is therefore re-asked once, after
/// the quiet period returns, and only the second empty answer is believed.
///
/// **That rule only fires once the client has noticed the server is busy**
/// (GM-309). `LspClient::settle` latches once per server, so from a server's
/// second pass on `wait_ready` asks only "is anything in flight right now" -
/// which a client whose server settled passes ago answers "yes, quiet" the
/// instant a `didChange` is sent, because nothing has happened yet to make it
/// otherwise. A server does not begin reporting progress for an edit the
/// instant it receives one - measured for pyright at ~0.6s after `didOpen`
/// (the design doc's "Readiness, measured, and deliberately not changed") -
/// and an empty answer inside that gap was, before GM-309, indistinguishable
/// from a real "no target": `run_pass`'s own deferral test reads
/// `client.quiet_for(budgets.settle)`, and a quiet period that has already
/// run for minutes clears it trivially. `LspClient::mark_edited`, called from
/// `sync_documents` right after each `didOpen`/`didChange`, is the fix: it
/// resets how long the client has been quiet *for the purposes of that
/// judgement* whenever the client was not already known to be busy, so the
/// deferral rule above covers the gap before progress begins and not only
/// the progress itself. It does not resurrect a per-pass settle - `wait_ready`
/// still asks only `quiet_for(Duration::ZERO)` once settled, and answers that
/// question truthfully however recently `mark_edited` ran - it only changes
/// what a *specific* empty answer, arriving in the narrow window right after
/// an edit, is measured against. `tests/lsp_bridge.rs`'s
/// `a_didchange_race_is_not_recorded_as_no_target` forces the window with a
/// server scripted to answer nothing truthful until a real delay after
/// `didChange` has passed, and fails without this reset: the early `null`
/// is recorded as final in under two milliseconds, retracting the very edge
/// pass one had just found.
///
/// # Retraction (decision 5)
///
/// Two rules, and a third that is deliberately absent.
///
/// - **The bridge retracts its own stale answers.** Every edge it emits is
///   remembered against the file whose questions produced it; a later pass
///   over that same file retracts whatever it does not produce again - a
///   renamed method, a call that is no longer there. Only files this pass
///   actually finished are judged, because "did not produce it again" and
///   "never got round to asking" are the same absence otherwise.
/// - **It retracts a syntactic edge an answer contradicts,** when the
///   structural tier said which edge that is. [`OpenSite::replaces`] carries
///   the id: the shape that needs it is a use site the structural tier *did*
///   emit an edge for and guessed wrong about (`plugins/go/semantic.go`'s
///   `placeholderCall`, a `CALLS` that turns out to be a conversion), and the
///   rule is the Go engine's exactly - retract only when the semantic answer
///   lands somewhere else, never when it confirms.
/// - **It never guesses which edge a site belonged to.** An open site carries
///   `from_id` and `edge_kind`, and that pair does not name an edge: one
///   function calling two methods of the same name through different
///   receivers produces two sites with identical `(from_id, edge_kind)` and a
///   structural edge that belongs to neither. Retracting on that basis would
///   delete correct edges to fix ones that were never wrong, which is why
///   `replaces` exists instead.
pub struct LspBridge {
    language: String,
    root: PathBuf,
    /// The project root with symlinks resolved. A server reports absolute
    /// paths, and on macOS `$TMPDIR` is `/var/folders/…`, a symlink to
    /// `/private/var/folders/…`, which is the spelling everything downstream
    /// of the server uses. Both are kept and both are tried, the same way
    /// `plugins/go/semantic.go` keeps both for `go list`'s output.
    ///
    /// Windows has the same divergence for a different reason - a root given
    /// in 8.3 short form (`C:\Users\RUNNER~1\…`, which is what `%TEMP%` is on
    /// a GitHub Actions runner), a junction, a `subst` drive - and there
    /// `canonicalize` answers in the extended-length spelling, which
    /// [`without_verbatim_prefix`] removes before the path is stored. Kept
    /// verbatim it would match nothing: `strip_prefix` reads
    /// `Prefix::VerbatimDisk` and `Prefix::Disk` as different prefixes, so
    /// this second root - the whole point of which is to catch what the first
    /// one misses - would be dead weight on that platform.
    real_root: PathBuf,
    config: SemanticConfig,
    budgets: Budgets,
    client: Option<LspClient>,
    /// Set when the server binary is not there at all - the design's "semantic
    /// engine missing" mode. Permanent for this process: retrying a binary
    /// that does not exist once per pass would log the same line forever.
    unavailable: bool,
    starts: u32,
    /// Why the last attempt to start the server failed - the reason a pass
    /// with no server reports.
    start_failure: Option<String>,
    /// Documents the server has been told about, with the version it last saw
    /// and a hash of the text it was sent. The hash rather than the text: the
    /// SDK's index already holds every file's source, and a second copy of a
    /// whole project's text to answer "has this changed" is a lot of memory
    /// for one bit.
    opened: BTreeMap<RelPath, OpenDocument>,
    /// The edge ids this bridge has emitted, keyed by the file whose questions
    /// produced them - see this type's doc on retraction.
    emitted: BTreeMap<RelPath, Vec<String>>,
}

#[derive(Debug, Clone, Copy)]
struct OpenDocument {
    version: i64,
    text_hash: u64,
}

impl LspBridge {
    /// A bridge for the server `config` names, over `root`.
    ///
    /// Constructing one starts nothing: the server is spawned by the first
    /// pass that has a question to ask, which is a stronger promise than the
    /// lazy-engine contract requires (that one only forbids starting before
    /// the first `semanticPass`) and costs nothing to keep - a pass over
    /// files with no open sites has no reason to load a compiler.
    pub fn new(language: &str, root: &Path, config: SemanticConfig) -> Self {
        Self::with_budgets(language, root, config, Budgets::default())
    }

    /// [`LspBridge::new`] with budgets a test can make small - see
    /// [`Budgets`].
    pub fn with_budgets(language: &str, root: &Path, config: SemanticConfig, budgets: Budgets) -> Self {
        let real_root = std::fs::canonicalize(root)
            .map(|resolved| without_verbatim_prefix(&resolved))
            .unwrap_or_else(|_| root.to_path_buf());
        Self {
            language: language.to_string(),
            root: root.to_path_buf(),
            real_root,
            config,
            budgets,
            client: None,
            unavailable: false,
            starts: 0,
            start_failure: None,
            opened: BTreeMap::new(),
            emitted: BTreeMap::new(),
        }
    }

    /// The whole budget for one pass - see [`Budgets`].
    fn pass_budget(&self, whole_project: bool, files: usize) -> Duration {
        if !whole_project {
            return self.budgets.single_file;
        }
        let files = u32::try_from(files).unwrap_or(u32::MAX);
        self.budgets.project_floor.max(self.budgets.per_file.saturating_mul(files))
    }

    /// Starts the server if it is not running, or reports why there will not
    /// be one.
    fn ensure_client(&mut self, deadline: Instant) -> Option<&mut LspClient> {
        if self.client.as_mut().is_some_and(LspClient::gone) {
            eprintln!("[{}] the language server is gone - starting a new one", self.language);
            self.client = None;
        }
        if self.client.is_none() {
            if self.unavailable {
                return None;
            }
            if self.starts >= MAX_SERVER_STARTS {
                self.start_failure = Some(format!(
                    "the language server was started {MAX_SERVER_STARTS} times and is not started again \
                     in this process"
                ));
                return None;
            }
            self.starts += 1;
            match LspClient::start(&self.language, &self.config, &self.root, deadline) {
                Ok(client) => self.client = Some(client),
                Err(err) => {
                    self.start_failure = Some(format!(
                        "the language server {} could not be started: {err:#}",
                        self.config.command.display()
                    ));
                    if missing_binary(&err) {
                        self.unavailable = true;
                        eprintln!(
                            "[{}] the language server {} could not be started ({err:#}) - this \
                             language's semantic tier is off for the rest of this process's life; \
                             the structural graph is unaffected",
                            self.language,
                            self.config.command.display()
                        );
                    } else {
                        eprintln!(
                            "[{}] the language server did not come up ({err:#}) - this pass is \
                             incomplete and the next one will try again",
                            self.language
                        );
                    }
                    return None;
                }
            }
        }
        self.client.as_mut()
    }

    /// Sends `didOpen`/`didChange` so the server reads the same bytes the
    /// structural pass did.
    ///
    /// The text comes from [`SdkIndex`], never from the filesystem, and that
    /// is the point: the index holds the text every position in this pass was
    /// computed against, while the file on disk may already have moved on.
    /// Sending the server a newer file would put the questions and the answers
    /// in different coordinate systems - the failure being ruled out here is a
    /// definition mapped onto whatever declaration has since drifted into that
    /// line.
    ///
    /// Only files this pass will actually *ask about*, which on a whole-project
    /// pass is materially fewer than the files in scope. An open document is
    /// text the server holds in memory on the plugin's behalf, and the plugin
    /// is what `[plugin] memoryLimitMb` charges for it; a file nothing is asked
    /// about gains nothing by being open, because the server reads the ones it
    /// merely has to *resolve into* off disk itself.
    ///
    /// Nothing is ever closed again. A `didClose` after each pass would free
    /// that text and make the next pass re-send every byte of it, and the
    /// per-file passes that follow every edit are exactly where that would be
    /// paid over and over.
    fn sync_documents(
        client: &mut LspClient,
        opened: &mut BTreeMap<RelPath, OpenDocument>,
        root: &Path,
        index: &SdkIndex,
        scope: &BTreeSet<RelPath>,
        language_id: &str,
    ) {
        for path in scope {
            let Some(entry) = index.entry(path) else { continue };
            let hash = hash_of(&entry.source);
            let uri = file_uri(&path.to_absolute(root));
            match opened.get(path).copied() {
                Some(document) if document.text_hash == hash => {}
                Some(document) => {
                    let version = document.version + 1;
                    let _ = client.notify(
                        "textDocument/didChange",
                        json!({
                            "textDocument": { "uri": uri, "version": version },
                            // One full-text change, never an incremental one:
                            // this side has the whole file and no diff, and
                            // `TextDocumentSyncKind.Full` is what every server
                            // supports.
                            "contentChanges": [{ "text": entry.source }],
                        }),
                    );
                    // GM-309: the server has not necessarily reacted to this
                    // edit yet - see `LspClient::mark_edited`.
                    client.mark_edited();
                    opened.insert(path.clone(), OpenDocument { version, text_hash: hash });
                }
                None => {
                    let _ = client.notify(
                        "textDocument/didOpen",
                        json!({
                            "textDocument": {
                                "uri": uri,
                                "languageId": language_id,
                                "version": 1,
                                "text": entry.source,
                            }
                        }),
                    );
                    // Same reasoning as `didChange` above: a file this server
                    // has never seen is at least as likely to start it
                    // reanalysing as an edit to one it already had open.
                    client.mark_edited();
                    opened.insert(path.clone(), OpenDocument { version: 1, text_hash: hash });
                }
            }
        }
    }

    /// Waits until the server is ready to be believed - see this type's doc
    /// on readiness.
    ///
    /// [`LspClient::settle`] is what turns the quiet period from a per-pass
    /// cost into a per-server one.
    fn wait_ready(
        client: &mut LspClient,
        budgets: &Budgets,
        deadline: Instant,
        language: &str,
    ) -> Result<(), String> {
        let started = Instant::now();
        let until = deadline.min(started + budgets.readiness);
        loop {
            client.drain();
            if client.settle(budgets.settle) {
                return Ok(());
            }
            let now = Instant::now();
            if now >= until {
                let waited = now.saturating_duration_since(started);
                eprintln!(
                    "[{language}] the language server was still indexing after {waited:?} - this pass asks \
                     nothing rather than recording its empty answers as real"
                );
                return Err(format!("the language server was still indexing after {waited:?}"));
            }
            match client.poll(Duration::from_millis(25)) {
                Poll::Closed => return Err("the language server exited before it became ready".to_string()),
                _ => continue,
            }
        }
    }
}

/// Whether this is the "there is no such binary" failure rather than a server
/// that started and then misbehaved - the difference between degrading
/// permanently and trying again next pass.
fn missing_binary(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(io.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied)
        })
    })
}

/// A cheap content fingerprint for "has this file changed since the server was
/// told about it". Process-local and never persisted, so the standard hasher
/// (whose output is not stable across builds) is exactly the right tool.
fn hash_of(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

// --- questions --------------------------------------------------------------

/// One thing to ask the server, and what to do with the answer.
#[derive(Debug, Clone)]
struct Question {
    /// The file the question is asked *in* - the one whose document the
    /// position refers to, and the one this answer's edges are remembered
    /// against.
    file: RelPath,
    position: Position,
    ask: Ask,
}

#[derive(Debug, Clone)]
enum Ask {
    /// An open site: where does this name come from.
    Definition(OpenSite),
    /// A declaration other declarations may implement: who implements it.
    Implementation { anchor: String },
    /// The second hop of an implementation answer: what declaration is at the
    /// site the server pointed at - see [`LspBridge`]'s doc on why a site is
    /// not a declaration.
    ///
    /// `for_file` is the *anchor's* file, not this question's, because that is
    /// where the whole sweep's answers are remembered for retraction: a later
    /// pass over the trait re-derives its implementors, and a pass over some
    /// implementor's file has no idea the sweep ever happened.
    Implementor { anchor: String, for_file: RelPath },
}

impl Question {
    fn method(&self) -> &'static str {
        match self.ask {
            Ask::Definition(_) | Ask::Implementor { .. } => "textDocument/definition",
            Ask::Implementation { .. } => "textDocument/implementation",
        }
    }
}

/// Everything this pass will ask, plus what it refused to ask and why.
struct Questions {
    asking: Vec<Question>,
    /// Open sites of a kind this bridge cannot answer - see [`LspBridge`]'s
    /// doc. Counted for the log, and deliberately *not* a reason to call the
    /// pass incomplete: they are outside what this engine answers by design,
    /// and a pass that stays permanently "incomplete" for a reason no retry
    /// can change would keep a language owed a pass forever.
    unanswerable: usize,
    /// Whether [`Budgets::max_sites`] cut the list short - which, per that
    /// field's doc, now means the project is past a ceiling no measured
    /// corpus comes near rather than merely large.
    truncated: bool,
}

/// Builds the question list for `scope`, in scope order, stopping at the last
/// **whole file** that fits under [`Budgets::max_sites`].
///
/// The file boundary is load-bearing and was not here before GM-319. A cut
/// taken mid-file leaves that file with some of its questions asked and the
/// rest silently dropped - and `run_pass` then reports it in `covered`,
/// because every question it *did* hold was sent and answered. `retract_stale`
/// reads `covered` as "this pass is the whole truth about that file now" and
/// withdraws every edge an earlier pass emitted for a site this one never
/// reached. Correct edges deleted to make room for questions nobody asked, in
/// other words, which is the one direction `LspBridge`'s retraction rules
/// exist to forbid. Cutting between files makes the hazard unreachable rather
/// than unlikely: a file is either asked about completely or not named at all,
/// and a file not named is not covered, so its earlier answers stand.
///
/// One file may exceed the ceiling on its own - a generated file, a giant
/// table - and is taken anyway when it is the first with anything to ask.
/// The alternative is a pass that asks nothing about it forever, and the
/// deadline still bounds the work either way; an overrun of one file is the
/// smaller of the two failures, and the only one that cannot cost an edge.
/// A `max_sites` of zero is the exception to the exception, and stays a way
/// to ask nothing at all.
///
/// `unanswerable` counts only the files that were admitted, for the same
/// reason: a file the ceiling never reached has not had its sites *skipped*,
/// and reporting them in that log line would describe work this pass declined
/// to consider as work it looked at and turned down.
fn questions(index: &SdkIndex, scope: &[RelPath], config: &SemanticConfig, budgets: &Budgets) -> Questions {
    let mut asking: Vec<Question> = Vec::new();
    let mut unanswerable = 0usize;
    let mut truncated = false;
    for path in scope {
        let Some(entry) = index.entry(path) else { continue };
        let mut for_file = Vec::new();
        let mut skipped = 0usize;
        for site in &entry.graph.open_sites {
            if site.kind == OpenSiteKind::Implementation {
                skipped += 1;
                continue;
            }
            for_file.push(Question {
                file: path.clone(),
                position: site.position,
                ask: Ask::Definition(site.clone()),
            });
        }
        if !config.implementation_kinds.is_empty() {
            for node in &entry.graph.nodes {
                let implementable = node
                    .native_kind
                    .as_deref()
                    .is_some_and(|kind| config.implementation_kinds.iter().any(|want| want == kind));
                if !implementable {
                    continue;
                }
                for_file.push(Question {
                    file: path.clone(),
                    position: name_position(&entry.source, node),
                    ask: Ask::Implementation { anchor: node.id.clone() },
                });
            }
        }
        // Room for all of it, or nothing asked yet and a ceiling that is not
        // simply zero - the one case a file is admitted over the ceiling.
        let fits = asking.len() + for_file.len() <= budgets.max_sites;
        if !fits && !(asking.is_empty() && budgets.max_sites > 0) {
            truncated = true;
            break;
        }
        unanswerable += skipped;
        asking.append(&mut for_file);
    }
    Questions { asking, unanswerable, truncated }
}

/// Where a declaration's *name* starts, for a request that has to land on an
/// identifier.
///
/// A node's range starts at the whole declaration - `pub trait Foo` starts at
/// `pub`, `export class Foo` at `export` - and a server asked at that position
/// resolves the token there, which is a keyword and has no definition or
/// implementations. So the request is aimed at the first occurrence of the
/// node's own name inside its range.
///
/// A heuristic, and a language-agnostic one: it uses only the node's `name`
/// and its own text. It can be fooled by a declaration whose name also appears
/// earlier inside its own range (an attribute or annotation that repeats it),
/// and falls back to the range's start when the name is not found there at
/// all, which is the behaviour there would have been without it.
fn name_position(source: &str, node: &WireNode) -> Position {
    if node.name.is_empty() {
        return node.range.start;
    }
    let last = node.range.end.line.max(node.range.start.line);
    for line in node.range.start.line..=last {
        let text = line_text(source, line);
        let from = if line == node.range.start.line { node.range.start.col as usize } else { 0 };
        let chars: Vec<char> = text.chars().collect();
        if from > chars.len() {
            continue;
        }
        let haystack: String = chars[from..].iter().collect();
        if let Some(byte_at) = haystack.find(&node.name) {
            let col = from + haystack[..byte_at].chars().count();
            return Position { line, col: col as u32 };
        }
    }
    node.range.start
}

// --- the pass ---------------------------------------------------------------

/// Accumulates one pass's answer.
struct Answers {
    language: String,
    engine: String,
    builders: BTreeMap<RelPath, FileGraphBuilder>,
    /// Placeholders this pass will emit, by the address they wait on - so that
    /// two sites resolving to one declaration produce one node rather than one
    /// id written twice. The same rule `plugins/rust`'s emitter applies to its
    /// own output, and for the same reason: `apply_diff` upserts by id either
    /// way, but a stream that says one thing twice has stopped describing the
    /// file.
    ///
    /// Collected here rather than added to a builder on the spot, because the
    /// row a shared placeholder carries must not depend on which of its sites
    /// was answered first - see [`Placeholder`].
    placeholders: BTreeMap<Address, Placeholder>,
    edges: HashSet<String>,
    by_file: BTreeMap<RelPath, Vec<String>>,
    retract: BTreeSet<String>,
}

/// The node one address gets, and the use site whose `name` and `range` it
/// shows.
///
/// # Why the site is chosen rather than taken
///
/// A placeholder's identity is the address it waits on: its id is derived from
/// the file and the rendered target, and nothing else (`ids::node_id`). Its
/// `name` and `range`, though, describe a *use site*, and an address reached
/// from several sites has several of those - `SearchMode::Count` and
/// `SearchMode::CountMatches` in ripgrep's `crates/core/flags/hiargs.rs` both
/// address the enum `SearchMode`, so one node has to speak for both.
///
/// Recording whichever arrived first made that choice out of LSP answer
/// arrival order, which is not a property of the file. Three indexings of one
/// unchanged ripgrep checkout by one binary agreed on every node id and every
/// edge, and disagreed on 50, 54 and 63 placeholder *rows* between the pairs -
/// every one of them in `range`, six of them in `name` as well. Nothing
/// downstream was wrong, since the linker reads the target and never the row,
/// but the index had stopped being a function of the source: any node-level
/// comparison over a Rust corpus was reading that noise as a difference.
///
/// So the row is made a function of the address instead. `name` is the
/// resolved *declaration's* own name rather than the text at a use site -
/// `SearchMode`, which every site addressing it is a use of, and which
/// `qualifiedName` already says. `OpenSite::name` was never meant for this: it
/// is documented as "an LSP bridge uses it only for diagnostics", and
/// [`record_implementor`] already passes the declaration's name. The `range`
/// is the *earliest* of the sites, which is the convention the structural tier
/// arrives at by walking a file in source order.
struct Placeholder {
    id: String,
    target: PlaceholderTarget,
    name: String,
    range: Range,
}

/// A witness's rank: earliest position wins, and the name settles the tie that
/// two sites at one position would otherwise leave to arrival order again.
fn witness_rank<'a>(name: &'a str, range: &Range) -> (u32, u32, u32, u32, &'a str) {
    (range.start.line, range.start.col, range.end.line, range.end.col, name)
}

/// What a use site covers: the name written at it, starting where the site
/// starts. `written` is the source text, which is not always the name the
/// placeholder ends up carrying - see [`Placeholder`].
fn site_range(at: Position, written: &str) -> Range {
    Range { start: at, end: Position { line: at.line, col: at.col + written.chars().count() as u32 } }
}

/// Everything a placeholder's identity is derived from, in a form that can key
/// a map: its file, and the target it waits on (scope and key, each with the
/// discriminant that keeps a file scope from colliding with a container of the
/// same spelling).
type Address = (RelPath, bool, String, bool, String);

fn address_key(file: &RelPath, target: &PlaceholderTarget) -> Address {
    let (container, scope) = match &target.scope {
        TargetScope::File(path) => (false, path.clone()),
        TargetScope::Container(container) => (true, container.clone()),
    };
    let (qualified, key) = match &target.key {
        TargetKey::Name(name) => (false, name.clone()),
        TargetKey::QualifiedName(qualified) => (true, qualified.clone()),
    };
    (file.clone(), container, scope, qualified, key)
}

impl Answers {
    fn new(language: &str, engine: &str) -> Self {
        Self {
            language: language.to_string(),
            engine: engine.to_string(),
            builders: BTreeMap::new(),
            placeholders: BTreeMap::new(),
            edges: HashSet::new(),
            by_file: BTreeMap::new(),
            retract: BTreeSet::new(),
        }
    }

    fn builder(&mut self, file: &RelPath) -> &mut FileGraphBuilder {
        let (language, engine) = (self.language.clone(), self.engine.clone());
        self.builders.entry(file.clone()).or_insert_with(|| FileGraphBuilder::new(&language, &engine, file))
    }

    /// One answer: the placeholder that addresses `target`, and the edge onto
    /// it.
    ///
    /// `in_file` is the file the edge *starts* in, which is where the
    /// placeholder goes too - a placeholder is a node of the file that is
    /// waiting, and `fromFile` is what core's visibility check reads. `for_file`
    /// is the file whose question produced this, which is what the answer is
    /// remembered against for retraction; the two differ only for an
    /// implementation answer, whose edge starts at a declaration in some other
    /// file entirely.
    ///
    /// `name` and `at` describe the use site this answer came from; which of
    /// an address's sites the node ends up showing is [`Placeholder`]'s rule,
    /// not this call's. The node itself is added in [`Answers::finish`], once
    /// every site has had its say - so the id is derived here rather than
    /// returned by the builder.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        for_file: &RelPath,
        in_file: &RelPath,
        from_id: &str,
        kind: EdgeKind,
        name: &str,
        at: Range,
        target: PlaceholderTarget,
    ) -> String {
        let key = address_key(in_file, &target);
        let placeholder = match self.placeholders.get_mut(&key) {
            Some(held) => {
                if witness_rank(name, &at) < witness_rank(&held.name, &held.range) {
                    held.name = name.to_string();
                    held.range = at;
                }
                held.id.clone()
            }
            None => {
                let id = crate::graph::placeholder_id(in_file, PlaceholderKind::PendingSymbol, &target);
                self.placeholders
                    .insert(key, Placeholder { id: id.clone(), target, name: name.to_string(), range: at });
                id
            }
        };

        let id = crate::ids::edge_id(from_id, kind, &placeholder, None);
        if !self.edges.insert(id.clone()) {
            return id;
        }
        let engine = self.engine.clone();
        self.builder(in_file).add_edge(EdgeSpec {
            from_id: from_id.to_string(),
            to_id: placeholder,
            kind,
            // Every edge here lands on a placeholder, so nothing is confirmed
            // until core links it: `resolved` describes what the edge points
            // at, never who produced it.
            resolved: false,
            to_declaration: None,
            source: SourceTier::Semantic,
            engine,
        });
        self.by_file.entry(for_file.clone()).or_default().push(id.clone());
        id
    }

    fn finish(mut self) -> (FileChangeDiff, BTreeMap<RelPath, Vec<String>>) {
        let mut diff = FileChangeDiff::default();
        // The nodes, now that every site addressing each one has been seen. In
        // address order, which is the file's own order and not the order the
        // server happened to answer in - the same reason the row itself is
        // chosen rather than taken (see `Placeholder`).
        for ((file, ..), held) in std::mem::take(&mut self.placeholders) {
            let id = self.builder(&file).add_placeholder(
                PlaceholderKind::PendingSymbol,
                held.name,
                held.target,
                held.range,
            );
            debug_assert_eq!(id, held.id, "a placeholder's id must not depend on its row");
        }
        for (_, builder) in std::mem::take(&mut self.builders) {
            let graph = builder.finish();
            diff.upsert_nodes.extend(graph.nodes);
            diff.upsert_edges.extend(graph.edges);
        }
        // An edge this pass re-emitted is not stale, whatever an earlier pass
        // recorded about it: retracting and upserting one id in one diff is a
        // delete followed by an insert of the same row, which is at best a
        // waste and at worst a foreign-key fault for anything pointing at it.
        diff.delete_edge_ids = self.retract.into_iter().filter(|id| !self.edges.contains(id)).collect();
        (diff, self.by_file)
    }
}

/// The address of a declaration, as a placeholder target.
///
/// A `qualifiedName` key rather than a `name` key, always: a semantic tier
/// knows *which* declaration it means, and `name` would re-introduce the
/// ambiguity it just resolved (`Close` exists on a dozen types;
/// `Server.Close` on one). The scope is the declaration's container when it
/// has one and its file otherwise - core's linker looks candidates up by
/// `(language, container)` or by `filePath`, and a language with no containers
/// only ever has the second.
fn address_of(node: &WireNode, from_container: Option<String>) -> PlaceholderTarget {
    PlaceholderTarget {
        scope: match &node.container {
            Some(container) => TargetScope::Container(container.clone()),
            None => TargetScope::File(node.file_path.clone()),
        },
        key: TargetKey::QualifiedName(node.qualified_name.clone()),
        from_container,
    }
}

/// Whether a node is one an answer may point at.
///
/// A placeholder is not a declaration - it is another file's unanswered
/// question, and addressing one would chain a guess onto a guess. A `File`
/// node is not one either: a definition that lands only inside it (nothing
/// smaller contains the position) says "somewhere in this file", which is
/// weaker than the `IMPORTS` edge that already exists for it.
fn is_addressable(node: &WireNode) -> bool {
    if node.kind == NodeKind::File {
        return false;
    }
    !matches!(
        node.native_kind.as_deref(),
        Some("pending_symbol") | Some("reexport") | Some("resolved_module") | Some("external_module")
    )
}

/// A location a server answered with, in the server's own coordinates.
struct ServerLocation {
    uri: String,
    line: u32,
    column: u32,
}

/// The locations in a `definition`/`implementation` result.
///
/// Three shapes are legal and all three appear in the wild: a single
/// `Location`, an array of them, and - because this client advertises
/// `linkSupport` - an array of `LocationLink`s, whose `targetSelectionRange`
/// is the one that points at the declaration's name rather than at its whole
/// body.
fn locations(result: &Value) -> Vec<ServerLocation> {
    fn one(value: &Value, into: &mut Vec<ServerLocation>) {
        let link = value.get("targetUri").and_then(Value::as_str);
        let (uri, range) = match link {
            Some(uri) => (uri, value.get("targetSelectionRange").or_else(|| value.get("targetRange"))),
            None => match value.get("uri").and_then(Value::as_str) {
                Some(uri) => (uri, value.get("range")),
                None => return,
            },
        };
        let Some(start) = range.and_then(|range| range.get("start")) else { return };
        let (Some(line), Some(column)) =
            (start.get("line").and_then(Value::as_u64), start.get("character").and_then(Value::as_u64))
        else {
            return;
        };
        into.push(ServerLocation { uri: uri.to_string(), line: line as u32, column: column as u32 });
    }

    let mut out = Vec::new();
    match result {
        Value::Array(values) => values.iter().for_each(|value| one(value, &mut out)),
        Value::Object(_) => one(result, &mut out),
        _ => {}
    }
    out
}

/// The file a location is in, when this index holds it at all.
///
/// Separate from [`node_at`] because the two "no" answers are different
/// questions: a location in a file nothing indexed can never become an edge,
/// while a location in a file this index *does* hold, at a position no
/// declaration covers, is exactly the case the second hop was added for.
fn file_at(index: &SdkIndex, roots: [&Path; 2], location: &ServerLocation) -> Option<RelPath> {
    let absolute = path_from_uri(&location.uri)?;
    let relative = roots.iter().find_map(|root| absolute.strip_prefix(root).ok())?;
    let path = RelPath::new(relative.to_string_lossy().replace('\\', "/"));
    index.entry(&path).is_some().then_some(path)
}

/// A server location's position in the wire's own column units.
fn wire_position(
    index: &SdkIndex,
    path: &RelPath,
    encoding: PositionEncoding,
    location: &ServerLocation,
) -> Position {
    let source = index.source(path).unwrap_or_default();
    let column = encoding.to_wire_column(line_text(source, location.line), location.column);
    Position { line: location.line, col: column }
}

/// The node a location points at, together with the file it is in.
fn node_at<'i>(
    index: &'i SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    location: &ServerLocation,
) -> Option<(RelPath, &'i WireNode)> {
    let path = file_at(index, roots, location)?;
    let at = wire_position(index, &path, encoding, location);
    let node = index.node_at(&path, at)?;
    is_addressable(node).then_some((path, node))
}

/// How long a server whose pipe broke or whose stdout closed is given to
/// finish exiting before the pass stops waiting to learn how it ended.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// The reason a pass gives when its server died under it.
fn exited(status: Option<ExitStatus>) -> String {
    match status {
        Some(status) => format!("the language server exited during the pass ({status})"),
        None => "the language server exited during the pass".to_string(),
    }
}

/// Runs the question list against a live server.
///
/// Written as a free function rather than a method so that the client can be
/// borrowed mutably for the whole pass while the index, the budgets and the
/// accumulator are borrowed beside it - the alternative is a `&mut self`
/// method that re-borrows `self.client` on every line.
#[allow(clippy::too_many_arguments)]
fn run_pass(
    client: &mut LspClient,
    language: &str,
    engine: &str,
    roots: [&Path; 2],
    index: &SdkIndex,
    asking: Vec<Question>,
    budgets: &Budgets,
    deadline: Instant,
) -> (Answers, BTreeSet<RelPath>, Option<String>) {
    let mut answers = Answers::new(language, engine);
    let mut queue: Vec<Question> = asking.into_iter().rev().collect();
    // Questions whose empty answer arrived while the server was indexing. They
    // are *not* put straight back on the queue: re-asking a busy server
    // immediately gets the same empty answer, and doing that in a loop is a
    // spin rather than a retry. They wait until the server says it has
    // finished and are asked once more then - see `LspBridge`'s readiness
    // rules. `re_asked` is what keeps "once more" from being "forever".
    let mut deferred: Vec<Question> = Vec::new();
    let mut re_asked: HashSet<(RelPath, u32, u32)> = HashSet::new();
    let mut in_flight: BTreeMap<i64, (Question, Instant)> = BTreeMap::new();
    let mut failed_files: BTreeSet<RelPath> = BTreeSet::new();
    let mut touched_files: BTreeSet<RelPath> = BTreeSet::new();
    // The first reason the pass fell short, if it did: `None` is a complete
    // pass.
    let mut failure: Option<String> = None;
    let mut fail = |reason: String| {
        failure.get_or_insert(reason);
    };

    loop {
        // A deferred question goes back on the queue only once the server has
        // been quiet for a whole settle, not the instant its progress set
        // empties - the same rule, and for the same measured reason, as
        // readiness itself (see `LspBridge`'s doc). A re-ask sent into the gap
        // between two of rust-analyzer's startup phases is answered `null`
        // again, and that second empty answer is the one this bridge believes.
        if !deferred.is_empty() && client.quiet_for(budgets.settle) {
            queue.append(&mut deferred);
        }
        // Fill the pipeline.
        while in_flight.len() < budgets.concurrency.max(1) {
            let Some(question) = queue.pop() else { break };
            touched_files.insert(question.file.clone());
            let position = {
                let source = index.source(&question.file).unwrap_or_default();
                let line = line_text(source, question.position.line);
                client.encoding().from_wire_column(line, question.position.col)
            };
            let params = json!({
                "textDocument": { "uri": file_uri(&question.file.to_absolute(roots[0])) },
                "position": { "line": question.position.line, "character": position },
            });
            match client.request(question.method(), params) {
                Ok(id) => {
                    in_flight.insert(id, (question, Instant::now()));
                }
                Err(err) => {
                    failed_files.insert(question.file.clone());
                    // A pipe broken by a server that has died is its death,
                    // not a failure to write.
                    match client.exit_status(EXIT_GRACE) {
                        Some(status) => {
                            eprintln!("[{language}] the language server exited during the pass ({status})");
                            fail(exited(Some(status)));
                        }
                        None => {
                            eprintln!("[{language}] could not ask the language server ({err:#})");
                            fail(format!("could not ask the language server: {err:#}"));
                        }
                    }
                    queue.clear();
                    break;
                }
            }
        }
        if in_flight.is_empty() && deferred.is_empty() {
            break;
        }

        let now = Instant::now();
        if now >= deadline {
            eprintln!(
                "[{language}] the semantic pass ran out of its budget with {} question(s) \
                 outstanding and {} unasked",
                in_flight.len(),
                queue.len() + deferred.len()
            );
            fail(format!(
                "the pass ran out of its budget with {} question(s) outstanding and {} unasked",
                in_flight.len(),
                queue.len() + deferred.len()
            ));
            for (id, (question, _)) in std::mem::take(&mut in_flight) {
                client.cancel(id);
                failed_files.insert(question.file);
            }
            for question in queue.drain(..).chain(deferred.drain(..)) {
                failed_files.insert(question.file);
            }
            break;
        }

        if in_flight.is_empty() {
            // Only deferred questions left: nothing to time out, and nothing
            // to do but wait for the server to stop indexing.
            if matches!(client.poll(Duration::from_millis(25)), Poll::Closed) {
                for question in deferred.drain(..) {
                    failed_files.insert(question.file);
                }
                fail(exited(client.exit_status(EXIT_GRACE)));
                break;
            }
            continue;
        }

        // Wake up for whichever comes first: an answer, the oldest request's
        // own timeout, or the end of the pass.
        let oldest = in_flight.values().map(|(_, sent)| *sent).min().unwrap_or(now);
        let wake = (oldest + budgets.request).min(deadline);
        let wait = wake.saturating_duration_since(now).max(Duration::from_millis(1));

        match client.poll(wait) {
            Poll::Answered { id, result } => {
                let Some((question, _)) = in_flight.remove(&id) else { continue };
                // An empty answer while the server is indexing is not an
                // answer: re-ask it once, after the indexing ends.
                let empty = locations(&result).is_empty();
                let key = (question.file.clone(), question.position.line, question.position.col);
                if empty && !client.quiet_for(budgets.settle) && re_asked.insert(key) {
                    deferred.push(question);
                    continue;
                }
                let again = record_answer(&mut answers, index, roots, client.encoding(), &question, &result);
                // The second hop of an implementation answer. Pushed onto the
                // front of the queue rather than the back so that a sweep's
                // follow-ups are asked while the questions that produced them
                // are still the server's warm working set.
                queue.extend(again);
            }
            Poll::Failed { id, message } => {
                let Some((question, _)) = in_flight.remove(&id) else { continue };
                // A server that refuses one question has not answered it, so
                // the file it was in is not covered - but the pass goes on:
                // one bad position is not a reason to drop the other nine
                // thousand answers.
                eprintln!("[{language}] the server refused a question about {} ({message})", question.file);
                fail(format!("the language server refused a question about {} ({message})", question.file));
                failed_files.insert(question.file);
            }
            Poll::Closed => {
                eprintln!(
                    "[{language}] the language server exited during the pass - keeping the {} \
                     answer(s) it did give and reporting the pass incomplete",
                    answers.edges.len()
                );
                for (question, _) in std::mem::take(&mut in_flight).into_values() {
                    failed_files.insert(question.file);
                }
                for question in queue.drain(..) {
                    failed_files.insert(question.file);
                }
                fail(exited(client.exit_status(EXIT_GRACE)));
                break;
            }
            Poll::Noise => continue,
            Poll::Idle => {
                // Whatever woke us: retire every request that is past its own
                // timeout. A question with no answer is not a question
                // answered "nothing".
                let now = Instant::now();
                let expired: Vec<i64> = in_flight
                    .iter()
                    .filter(|(_, (_, sent))| now.duration_since(*sent) >= budgets.request)
                    .map(|(id, _)| *id)
                    .collect();
                for id in expired {
                    let Some((question, _)) = in_flight.remove(&id) else { continue };
                    client.cancel(id);
                    eprintln!(
                        "[{language}] the server did not answer a question about {} within {:?}",
                        question.file, budgets.request
                    );
                    fail(format!(
                        "the language server did not answer a question about {} within {:?}",
                        question.file, budgets.request
                    ));
                    failed_files.insert(question.file);
                }
            }
        }
    }

    let covered = touched_files.difference(&failed_files).cloned().collect();
    (answers, covered, failure)
}

/// Turns one server answer into whatever it is evidence for, and into
/// whatever it still has to be asked.
///
/// The returned questions are the second hop of an implementation answer and
/// nothing else - see [`LspBridge`]'s doc. They are always empty for every
/// other kind, which is what makes the recursion one hop deep by
/// construction rather than by a counter.
#[must_use]
fn record_answer(
    answers: &mut Answers,
    index: &SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    question: &Question,
    result: &Value,
) -> Vec<Question> {
    let found = locations(result);
    match &question.ask {
        Ask::Definition(site) => {
            // Several locations are the normal answer for a `cfg`-gated or
            // overloaded declaration. They are only usable when they agree:
            // core's linker refuses an ambiguous address by design, and
            // picking one here would be this bridge making the guess the
            // linker declines to make.
            let mut target: Option<(RelPath, &WireNode)> = None;
            for location in &found {
                let Some((path, node)) = node_at(index, roots, encoding, location) else { continue };
                match &target {
                    Some((_, chosen)) if chosen.id != node.id => return Vec::new(),
                    Some(_) => {}
                    None => target = Some((path, node)),
                }
            }
            let Some((_, node)) = target else { return Vec::new() };
            // The placeholder is named after the declaration it waits on, not
            // after the text at this site: `SearchMode::Count` and
            // `SearchMode::CountMatches` are two spellings of one address, and
            // a node that has to speak for both can only honestly carry the
            // name they have in common. The site still supplies the *range* -
            // as wide as what is written there - and which site that is comes
            // from `Placeholder`, not from which answer arrived first.
            let edge = answers.record(
                &question.file,
                &question.file,
                &site.from_id,
                site.edge_kind,
                &node.name,
                site_range(site.position, &site.name),
                address_of(node, site.from_container.clone()),
            );
            // The contradiction rule: an answer that lands somewhere else
            // retracts the structural edge the site said it replaces, and an
            // answer that confirms it retracts nothing - see [`LspBridge`].
            if let Some(replaced) = &site.replaces {
                if replaced != &edge {
                    answers.retract.insert(replaced.clone());
                }
            }
            Vec::new()
        }
        Ask::Implementation { anchor } => {
            let mut again = Vec::new();
            for location in &found {
                if record_implementor(answers, index, roots, encoding, anchor, &question.file, location) {
                    continue;
                }
                // The location is in a file this index holds, at a position no
                // declaration of it covers: an `impl` header, or whatever else
                // the language spells an implementation with. Ask the server
                // what is written there - see [`LspBridge`]'s doc.
                let Some(path) = file_at(index, roots, location) else { continue };
                again.push(Question {
                    position: wire_position(index, &path, encoding, location),
                    file: path,
                    ask: Ask::Implementor { anchor: anchor.clone(), for_file: question.file.clone() },
                });
            }
            again
        }
        Ask::Implementor { anchor, for_file } => {
            for location in &found {
                record_implementor(answers, index, roots, encoding, anchor, for_file, location);
            }
            Vec::new()
        }
    }
}

/// Records one implementor of `anchor`, if `location` is a declaration this
/// index holds. `false` means it is not one - which is a question for the
/// caller, not an error.
///
/// `for_file` is the file whose question produced this, and it is the
/// anchor's, never the implementor's: see [`Ask::Implementor`].
///
/// # Only a `Type` implements anything (GM-361)
///
/// [`SdkIndex::node_at`] answers with the **smallest node containing** the
/// position, which is not the same as "the declaration written there". An
/// `impl` header inside an inline module is contained by that module's own
/// node and by nothing smaller, so the sweep recorded the *module* as an
/// implementor: measured on ripgrep,
/// `find_implementations("Sink")` carried a row
/// `sink::sinks @ crates/searcher/src/sink.rs:516`, which is
/// `pub mod sinks { … }`, alongside the three types declared inside it.
///
/// A module implements nothing, in any language this bridge serves, so a
/// node that is not a [`NodeKind::Type`] is refused here. Refused, not
/// dropped: `false` is exactly the signal that sends an
/// [`Ask::Implementation`] answer to its second hop, which asks the server
/// what is written at that position and gets the implementing declaration -
/// the same path an `impl` header at a file's top level already took, where
/// `node_at` found only the `File` node.
fn record_implementor(
    answers: &mut Answers,
    index: &SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    anchor: &str,
    for_file: &RelPath,
    location: &ServerLocation,
) -> bool {
    let Some((_, anchor_node)) = index.node(anchor) else { return false };
    let anchor_node = anchor_node.clone();
    let Some((path, node)) = node_at(index, roots, encoding, location) else { return false };
    if node.id == anchor_node.id {
        // A server that lists the trait's own declaration among its
        // implementations, which some do - and, for the second hop, the
        // `Trait` half of `impl Trait for T` if a server ever points there.
        return true;
    }
    if node.kind != NodeKind::Type {
        // A container that merely encloses the position, not the declaration
        // written at it - see this function's own doc.
        return false;
    }
    let (from_id, at, container) = (node.id.clone(), node.range.start, node.container.clone());
    answers.record(
        for_file,
        &path,
        &from_id,
        // The direction `find_implementations` walks: subtype -> supertype.
        // The question was "who implements this", so the edge starts at each
        // answer and points at the anchor.
        EdgeKind::SupertypeOf,
        &anchor_node.name,
        site_range(at, &anchor_node.name),
        address_of(&anchor_node, container),
    );
    true
}

impl SemanticEngine for LspBridge {
    fn answer(&mut self, files: &[RelPath], index: &SdkIndex) -> Result<SemanticAnswer> {
        let whole_project = files.is_empty();
        let scope: Vec<RelPath> = if whole_project {
            index.paths()
        } else {
            files.iter().filter(|path| index.entry(path).is_some()).cloned().collect()
        };
        if scope.is_empty() {
            return Ok(SemanticAnswer::complete(FileChangeDiff::default()));
        }

        let started = Instant::now();
        let deadline = started + self.pass_budget(whole_project, scope.len());
        let plan = questions(index, &scope, &self.config, &self.budgets);
        if plan.unanswerable > 0 {
            eprintln!(
                "[{}] {} open site(s) of a kind this bridge does not answer were skipped",
                self.language, plan.unanswerable
            );
        }
        if plan.asking.is_empty() {
            if plan.truncated {
                // An empty list the ceiling produced, not one the scope did:
                // `max_sites` is zero, so not one file was admitted. Nothing
                // was asked, so nothing is covered - and in particular nothing
                // may be retracted, which is what this branch used to do to
                // every file in scope on the strength of a list it had refused
                // to build (GM-319). An unasked site is not a site that went
                // away.
                return Ok(SemanticAnswer::incomplete_because(
                    FileChangeDiff::default(),
                    "the open-site ceiling (max_sites) admitted no site",
                ));
            }
            // Nothing to ask means nothing to start a compiler for. The files
            // in scope are still *covered*, so an earlier pass's answers about
            // sites that have since gone away are retracted.
            let mut answers = Answers::new(&self.language, &self.config.engine);
            let covered: BTreeSet<RelPath> = scope.iter().cloned().collect();
            retract_stale(&mut answers, &self.emitted, &covered, &BTreeMap::new());
            for file in &covered {
                self.emitted.insert(file.clone(), Vec::new());
            }
            let (diff, _) = answers.finish();
            return Ok(SemanticAnswer::complete(diff));
        }

        let language = self.language.clone();
        let engine = self.config.engine.clone();
        let budgets = self.budgets;
        let root = self.root.clone();
        let real_root = self.real_root.clone();
        let mut opened = std::mem::take(&mut self.opened);
        let asked_about: BTreeSet<RelPath> =
            plan.asking.iter().map(|question| question.file.clone()).collect();
        let outcome = {
            // The client is borrowed for the whole pass, so everything else
            // this needs was cloned or moved out of `self` above.
            match self.ensure_client(deadline) {
                None => Err(None),
                Some(client) => {
                    client.drain();
                    Self::sync_documents(client, &mut opened, &root, index, &asked_about, &language);
                    match Self::wait_ready(client, &budgets, deadline, &language) {
                        Ok(()) => Ok(run_pass(
                            client,
                            &language,
                            &engine,
                            [&root, &real_root],
                            index,
                            plan.asking,
                            &budgets,
                            deadline,
                        )),
                        Err(reason) => Err(Some(reason)),
                    }
                }
            }
        };
        self.opened = opened;

        let (mut answers, covered, failure) = match outcome {
            Ok(outcome) => outcome,
            // No server, or one that never became ready: no answers, and -
            // crucially - nothing recorded as "no target", which is what the
            // readiness rule exists to prevent.
            Err(reason) => {
                let reason = reason
                    .or_else(|| self.start_failure.clone())
                    .unwrap_or_else(|| "the language server is not running".to_string());
                return Ok(SemanticAnswer::incomplete_because(FileChangeDiff::default(), reason));
            }
        };

        let produced = answers.by_file.clone();
        retract_stale(&mut answers, &self.emitted, &covered, &produced);
        let (diff, emitted) = answers.finish();
        // A file this pass finished gets a fresh baseline: what it produced is
        // now the whole truth about that file's semantic edges. A file it did
        // *not* finish keeps its old ids as well as the new ones - the old ones
        // may still describe live code that this pass simply never got to, and
        // forgetting them here would mean nothing ever retracts them.
        for file in &covered {
            self.emitted.insert(file.clone(), emitted.get(file).cloned().unwrap_or_default());
        }
        for (file, ids) in emitted {
            if covered.contains(&file) {
                continue;
            }
            let baseline = self.emitted.entry(file).or_default();
            for id in ids {
                if !baseline.contains(&id) {
                    baseline.push(id);
                }
            }
        }

        eprintln!(
            "[{}] semantic pass: {} file(s), {} node(s)/{} edge(s) upserted, {} edge(s) retracted, \
             in {:?}{}",
            self.language,
            scope.len(),
            diff.upsert_nodes.len(),
            diff.upsert_edges.len(),
            diff.delete_edge_ids.len(),
            started.elapsed(),
            if failure.is_none() && !plan.truncated { "" } else { " (incomplete)" }
        );
        let failure = failure.or_else(|| {
            plan.truncated.then(|| "the open-site ceiling (max_sites) left sites unasked".to_string())
        });
        Ok(SemanticAnswer { diff, complete: failure.is_none(), reason: failure })
    }
}

/// Retracts what an earlier pass emitted for a file this pass finished and did
/// not emit again - see [`LspBridge`]'s doc on retraction.
fn retract_stale(
    answers: &mut Answers,
    remembered: &BTreeMap<RelPath, Vec<String>>,
    covered: &BTreeSet<RelPath>,
    now: &BTreeMap<RelPath, Vec<String>>,
) {
    for file in covered {
        let Some(previous) = remembered.get(file) else { continue };
        let still: HashSet<&String> = now.get(file).map(|ids| ids.iter().collect()).unwrap_or_default();
        for id in previous {
            if !still.contains(id) {
                answers.retract.insert(id.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::NodeSpec;
    use g_mesh_wire::Visibility;

    fn node(name: &str, start: (u32, u32), end: (u32, u32)) -> WireNode {
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("a.toy"));
        let range = Range {
            start: Position { line: start.0, col: start.1 },
            end: Position { line: end.0, col: end.1 },
        };
        builder.add_node(NodeSpec::new(NodeKind::Type, name, name, range).native_kind("trait"));
        builder.finish().nodes.remove(0)
    }

    /// **GM-319.** The ceiling has to clear the question list every corpus
    /// anyone has actually counted, because a corpus over it is a project
    /// whose whole-project pass can never be recorded as finished - the diff
    /// is committed, `core::watcher::apply::apply_semantic_pass` fails the
    /// pass, `language_state.semanticPassAt` stays unset, the language's
    /// receiver-call gap is listed in every session's MCP instructions, and
    /// the whole pass is repeated on every daemon start.
    ///
    /// The numbers are GM-314's census, re-counted for g-mesh by GM-319. They
    /// are written down here rather than left in a document because this is
    /// the assertion that would have caught the defect: at the previous
    /// 20,000 it fails on three of the four.
    #[test]
    fn the_site_ceiling_clears_every_corpus_that_has_been_counted() {
        // corpus, questions the plugins build for it (GM-314 / GM-319).
        let measured = [
            ("django/django", 87_832usize),
            ("tokio-rs/tokio", 27_750),
            ("g-mesh itself", 21_995),
            ("pallets/flask", 1_825),
        ];
        let ceiling = Budgets::default().max_sites;
        for (corpus, questions) in measured {
            assert!(
                ceiling >= questions,
                "{corpus} builds {questions} questions and the ceiling is {ceiling}: its pass \
                 would be cut short and never recorded as complete"
            );
        }
        let largest = measured.iter().map(|(_, n)| *n).max().unwrap();
        assert!(
            ceiling >= largest * 4,
            "and with room to spare - the largest corpus measured is {largest}, and a ceiling \
             sized to exactly what has been seen is one the next repository walks through"
        );
    }

    /// What the ceiling costs if a project ever reaches it, so the figure this
    /// type's doc quotes is checked rather than asserted.
    ///
    /// The list is a second copy of open sites the index already holds, so
    /// the number that matters is the marginal one; it is bounded here at the
    /// order of magnitude, not to the byte, because `Question`'s layout is
    /// the compiler's business and a test that pinned it exactly would fail
    /// on a field reordering that costs nothing.
    #[test]
    fn the_site_ceiling_bounds_what_one_question_list_can_cost() {
        let per_question = std::mem::size_of::<Question>();
        let worst = Budgets::default().max_sites.saturating_mul(per_question);
        assert!(
            worst <= 512 * 1024 * 1024,
            "a full question list is {} bytes of `Question` ({per_question} each) - past the \
             point where the ceiling is protecting anything",
            worst
        );
    }

    /// A request has to land on the identifier, not on the keyword the
    /// declaration's range starts at - see [`name_position`].
    #[test]
    fn a_declarations_question_is_aimed_at_its_name() {
        let source = "pub trait Greeter {\n    fn hello(&self);\n}\n";
        let at = name_position(source, &node("Greeter", (0, 0), (2, 1)));
        assert_eq!(at, Position { line: 0, col: 10 });
    }

    /// And on a line where a character is not a byte, the column is counted in
    /// characters, like every other column on this wire.
    #[test]
    fn a_name_position_counts_characters_not_bytes() {
        let source = "// héllo 🦀\npub trait Grüßer {}\n";
        let at = name_position(source, &node("Grüßer", (1, 0), (1, 19)));
        assert_eq!(at, Position { line: 1, col: 10 }, "`pub trait ` is ten characters");
    }

    #[test]
    fn a_name_that_is_not_in_its_own_range_falls_back_to_the_range_start() {
        let source = "pub trait Greeter {}\n";
        let at = name_position(source, &node("Missing", (0, 0), (0, 20)));
        assert_eq!(at, Position { line: 0, col: 0 });
    }

    #[test]
    fn all_three_location_shapes_are_read() {
        let single = json!({ "uri": "file:///p/a.toy", "range": { "start": { "line": 3, "character": 7 } } });
        let array = json!([single.clone(), single.clone()]);
        let links = json!([{
            "targetUri": "file:///p/a.toy",
            "targetRange": { "start": { "line": 1, "character": 0 } },
            "targetSelectionRange": { "start": { "line": 3, "character": 7 } },
        }]);
        for (label, value) in [("single", &single), ("array", &array), ("links", &links)] {
            let found = locations(value);
            assert!(!found.is_empty(), "{label}");
            assert_eq!(found[0].uri, "file:///p/a.toy", "{label}");
            assert_eq!((found[0].line, found[0].column), (3, 7), "{label}");
        }
        assert!(locations(&Value::Null).is_empty(), "no definition is an empty list, not a panic");
        assert!(locations(&json!([])).is_empty());
    }

    /// A `LocationLink`'s selection range is the one that points at the name;
    /// `targetRange` covers the whole declaration and is only the fallback.
    #[test]
    fn a_location_link_prefers_the_selection_range() {
        let links = json!([{
            "targetUri": "file:///p/a.toy",
            "targetRange": { "start": { "line": 1, "character": 0 } },
        }]);
        let found = locations(&links);
        assert_eq!((found[0].line, found[0].column), (1, 0));
    }

    #[test]
    fn a_placeholder_or_a_file_node_is_never_an_answers_target() {
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("a.toy"));
        let range = Range { start: Position { line: 0, col: 0 }, end: Position { line: 0, col: 1 } };
        builder.file_node(range);
        builder.add_placeholder(
            PlaceholderKind::PendingSymbol,
            "x",
            PlaceholderTarget {
                scope: TargetScope::File("b.toy".into()),
                key: TargetKey::Name("x".into()),
                from_container: None,
            },
            range,
        );
        builder.add_external_module("some-package", range);
        builder.add_node(NodeSpec::new(NodeKind::Function, "f", "f", range).public());
        let graph = builder.finish();
        assert!(!is_addressable(&graph.nodes[0]), "the File node");
        assert!(!is_addressable(&graph.nodes[1]), "a pending symbol");
        assert!(!is_addressable(&graph.nodes[2]), "an external module");
        assert!(is_addressable(&graph.nodes[3]), "a real declaration");
    }

    /// The address a semantic answer carries: exact by `qualifiedName`, scoped
    /// to the declaration's container when it has one.
    #[test]
    fn an_address_is_qualified_and_container_scoped_when_it_can_be() {
        let mut node = node("Greeter", (0, 0), (0, 1));
        node.container = Some("krate::greet".to_string());
        node.qualified_name = "greet::Greeter".to_string();
        node.file_path = "src/greet.rs".to_string();
        node.visibility = Visibility::Public;

        let target = address_of(&node, Some("krate::app".to_string()));
        assert_eq!(target.scope, TargetScope::Container("krate::greet".to_string()));
        assert_eq!(target.key, TargetKey::QualifiedName("greet::Greeter".to_string()));
        assert_eq!(target.from_container.as_deref(), Some("krate::app"));

        node.container = None;
        let target = address_of(&node, None);
        assert_eq!(target.scope, TargetScope::File("src/greet.rs".to_string()));
    }

    /// **GM-378.** Two sites of one file addressing one declaration are one
    /// placeholder, and the row it carries must be the same whichever of them
    /// the server answered first.
    ///
    /// The instance is ripgrep's, reduced: `SearchMode::Count` at line 140 and
    /// `SearchMode::CountMatches` at line 144 of `crates/core/flags/hiargs.rs`
    /// both resolve to the enum `SearchMode`, so both land on node id
    /// `69f3fc74…`. Recording whichever arrived first put a different `name`
    /// and a different `range` on that one row from run to run, over an
    /// unchanged checkout indexed by an unchanged binary.
    ///
    /// The two arms here are the same two answers in the two arrival orders,
    /// which is exactly the difference between two such runs: with the witness
    /// rule removed, the two arms produce the two rows the two indexings did,
    /// and this test fails on them. Which *name* the surviving row carries is
    /// the sibling test below.
    #[test]
    fn one_address_answered_from_two_sites_carries_the_same_row_in_either_order() {
        let file = RelPath::new("crates/core/flags/hiargs.rs");
        let mut declaration = node("SearchMode", (30, 9), (40, 1));
        declaration.container = Some("rg::flags::lowargs".to_string());
        declaration.qualified_name = "flags::lowargs::SearchMode".to_string();
        declaration.file_path = "crates/core/flags/lowargs.rs".to_string();

        // The two sites, in source order.
        let sites = [
            (Position { line: 140, col: 28 }, "SearchMode::Count"),
            (Position { line: 144, col: 28 }, "SearchMode::CountMatches"),
        ];
        let mut rows = Vec::new();
        for order in [[0usize, 1], [1, 0]] {
            let mut answers = Answers::new("rust", "rust-analyzer");
            for i in order {
                let (at, written) = sites[i];
                answers.record(
                    &file,
                    &file,
                    "from",
                    EdgeKind::References,
                    &declaration.name,
                    site_range(at, written),
                    address_of(&declaration, Some("rg::flags::hiargs".to_string())),
                );
            }
            let (diff, _) = answers.finish();
            assert_eq!(diff.upsert_nodes.len(), 1, "two sites, one address, one node");
            rows.push(diff.upsert_nodes.into_iter().next().unwrap());
        }

        assert_eq!(
            rows[0].id, "69f3fc743f4eade2dc01f07e99f541f0",
            "the reduction has to be the real instance, not something like it: this is the id \
             the two rows collided on in an index of ripgrep"
        );

        assert_eq!(rows[0], rows[1], "the row a shared placeholder carries depends on answer order");
        assert_eq!(
            rows[0].name, "SearchMode",
            "the node speaks for both sites, so it carries the name they have in common - not \
             whichever spelling was answered first"
        );
        assert_eq!(
            rows[0].range.start,
            Position { line: 140, col: 28 },
            "and the earliest of its sites, the way a source-order walk would have picked"
        );
    }

    /// **GM-378.** And the name that one row carries is the declaration's,
    /// not the text at the site that happened to reach it.
    ///
    /// The other half of the same instance, taken through `record_answer` so
    /// that what is under test is the argument the caller chooses.
    /// `OpenSite::name` is documented as something "an LSP bridge uses only
    /// for diagnostics"; passing it here made the node claim to be
    /// `SearchMode::Count` while its `qualifiedName` said `…::SearchMode`.
    #[test]
    fn a_placeholder_is_named_after_the_declaration_it_waits_on() {
        let root = Path::new("/p");
        let declared = RelPath::new("lowargs.rs");
        let mut builder = FileGraphBuilder::new("rust", "tree-sitter", &declared);
        let range = Range { start: Position { line: 30, col: 9 }, end: Position { line: 40, col: 1 } };
        let mut spec = NodeSpec::new(NodeKind::Type, "SearchMode", "flags::lowargs::SearchMode", range)
            .native_kind("enum")
            .public();
        spec.container = Some("rg::flags::lowargs".to_string());
        builder.add_node(spec);
        let mut index = SdkIndex::new();
        index.insert(declared.clone(), "pub enum SearchMode {}\n".repeat(41), builder.finish());

        let asking = RelPath::new("hiargs.rs");
        let question = Question {
            file: asking.clone(),
            position: Position { line: 140, col: 28 },
            ask: Ask::Definition(OpenSite {
                from_id: "caller".to_string(),
                position: Position { line: 140, col: 28 },
                name: "SearchMode::Count".to_string(),
                kind: OpenSiteKind::Reference,
                edge_kind: EdgeKind::References,
                from_container: Some("rg::flags::hiargs".to_string()),
                replaces: None,
            }),
        };
        let found = json!({
            "uri": "file:///p/lowargs.rs",
            "range": { "start": { "line": 30, "character": 9 } },
        });

        let mut answers = Answers::new("rust", "rust-analyzer");
        let again =
            record_answer(&mut answers, &index, [root, root], PositionEncoding::Utf16, &question, &found);
        assert!(again.is_empty(), "a definition answer asks nothing further");
        let (diff, _) = answers.finish();
        assert_eq!(diff.upsert_nodes.len(), 1);
        assert_eq!(
            diff.upsert_nodes[0].name, "SearchMode",
            "the placeholder waits on the enum, whatever the site that reached it was spelled"
        );
        assert_eq!(
            diff.upsert_nodes[0].qualified_name, "rg::flags::lowargs::flags::lowargs::SearchMode",
            "and its name agrees with the address its id is derived from"
        );
        assert_eq!(
            diff.upsert_nodes[0].range,
            site_range(Position { line: 140, col: 28 }, "SearchMode::Count"),
            "the range is still the site's own extent, as wide as what is written there"
        );
    }

    #[test]
    fn a_whole_project_budget_scales_with_the_files_in_scope_and_a_per_file_one_does_not() {
        let bridge = LspBridge::new("toy", Path::new("/p"), SemanticConfig::new("toy-server"));
        assert_eq!(bridge.pass_budget(true, 10), Duration::from_secs(15 * 60), "the floor");
        assert_eq!(bridge.pass_budget(true, 1_000), Duration::from_secs(8_000), "8s a file");
        assert_eq!(bridge.pass_budget(false, 1_000), Duration::from_secs(90));
    }

    /// The bridge must stay inside core's own timer - see [`Budgets`].
    #[test]
    fn every_budget_is_inside_the_one_core_allows() {
        let budgets = Budgets::default();
        let bridge = LspBridge::new("toy", Path::new("/p"), SemanticConfig::new("toy-server"));
        // core: max(20min, 10s * files) for a whole project, flat 120s per file.
        for files in [0usize, 1, 100, 10_000] {
            let core = Duration::from_secs(20 * 60).max(Duration::from_secs(10) * files as u32);
            assert!(bridge.pass_budget(true, files) < core, "{files} files");
        }
        assert!(bridge.pass_budget(false, 1) < Duration::from_secs(120));
        assert!(budgets.readiness <= budgets.project_floor);
    }
}
