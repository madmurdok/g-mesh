//! The engine itself: open sites in, a semantic diff out, with a language
//! server in the middle and a budget around the whole thing.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use anyhow::Result;
use g_mesh_wire::{
    EdgeKind, FileChangeDiff, NodeKind, PlaceholderTarget, Position, Range, SourceTier, TargetKey,
    TargetScope, WireDeclaration, WireEdge, WireNode,
};
use serde_json::{json, Value};

use super::client::{LspClient, Poll, CONTENT_MODIFIED};
use super::config::{OverloadDisambiguation, SemanticConfig};
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
///   These three apply only when core sent no `budgetMs` (a core older than
///   GM-521); when it did, the pass plans to three quarters of core's own
///   deadline instead - see [`LspBridge::pass_deadline`].
/// - **`readiness` (10 minutes) and `settle` (2s).** See [`LspBridge`]'s doc
///   on readiness. The readiness wait is *inside* the pass budget too, so a
///   server that never loads costs a pass rather than a plugin.
/// - **`warm_up` (off).** A longer budget for a server's *first* question,
///   for a server that holds every answer until it has loaded the project the
///   question's file belongs to and reports no progress while it does - so
///   readiness cannot see the load, and the first questions meet it instead.
///   GM-325 measured vtsls with one tsserver (`useSyntaxServer = "never"`):
///   12.8-33 s of loading on excalidraw, during which every request in flight
///   timed out at `request`, so every cold pass was incomplete and re-run on
///   the next daemon start. While it is owed, `run_pass` keeps **one**
///   question in flight, under this budget instead of `request`; the first
///   answer, refusal or timeout spends it, and the pipeline fills to
///   `concurrency` under `request` as usual. One question rather than eight
///   under the long budget because a server that is loading answers none of
///   them sooner, and a server that never answers then costs one warm-up
///   rather than eight. Spent once per *server process*
///   (`LspClient::warmed_up`): a restarted server is cold again and owes a
///   new one; a pass on a warm server never pays it. A warm-up that times out
///   fails its question exactly as `request` would - the pass is incomplete
///   and the file unfinished - and is not re-armed, so a server that never answers
///   costs one long wait, not one per question or per pass. It sits *inside*
///   the pass deadline like everything else, after `readiness`: a deferred
///   empty answer still waits for `settle`, and the warm-up question's own
///   empty answer counts as an answer (the server is responsive). `None` - the
///   default - is exactly the behaviour before it existed. The one value a
///   plugin sets ([`LspBridge::warm_up`]), because how long a server takes to
///   load a project is a fact about that server.
///
/// None of these is configuration, `warm_up` aside. They are this type's fields so a test can
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
    /// How long a server's first question may take, if longer than
    /// `request`; see this type's doc. `None` gives the first question
    /// `request` like every other.
    pub warm_up: Option<Duration>,
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
            warm_up: None,
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
///   `textDocument/definition` at the site's own position (an
///   [`OpenSiteKind::OverloadCall`] only when its target is overloaded, and
///   perhaps a `hover` hop - see "Overload binding" below, and a re-export
///   hop only while unsettled - see "Re-export hops"). A typed
///   [`OpenSiteKind::ReceiverField`] site (`x.f` with `replaces`) follows the
///   same retraction rules as a typed [`OpenSiteKind::ReceiverCall`]. The
///   location that
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
/// # Overload binding
///
/// An [`OpenSiteKind::OverloadCall`] site **refines** a structural `CALLS`
/// edge that is right about the function and silent about which of its
/// overloads the call binds. The answer never moves the call: it either names
/// one of the target's `declarations` by ordinal, or leaves the structural
/// edge exactly as it was. The rule and its reasons are
/// `docs/adr/0024-semantic-tier-refines-by-binding-a-declaration.md`.
///
/// - **Filter.** A site is asked only when its `replaces` edge lands on a
///   node with `declarations`, or on a placeholder whose key names one
///   anywhere in the index. Every other site's structural edge is already the
///   whole answer, and is not counted unanswerable.
/// - **Step 1, containment.** `definition` at the call; every location is
///   mapped to the tightest declaration range that contains it
///   ([`declaration_at`]), falling back to [`node_at`]. Locations on two
///   nodes, or on a node the structural edge does not land on, bind nothing.
/// - **Step 2.** One ordinal binds (a server that answers with the bound
///   overload: tsserver).
/// - **Step 3, hover.** Several ordinals bind only when the manifest sets
///   `overload_disambiguation = "hover"`: `hover` at the call and at each
///   bodiless candidate's name (cached per pass), normalised, compared equal
///   or equal once the candidate's first parameter is dropped (a bound
///   receiver). Exactly one match binds (pyright, whose `definition` returns
///   the whole set whatever the call binds). Zero or several bind nothing:
///   the hover path fails closed.
/// - **Step 4.** A declaration with a body is never bound in a set that has
///   bodiless ones.
///
/// A bound site records a `CALLS` edge onto the usual placeholder, with
/// `toDeclaration` set and the ordinal hashed into its id, so two overloads
/// called from one caller are two edges. The structural edge is settled **all
/// or nothing**: retracted only when every `OverloadCall` site naming it in a
/// finished file bound; otherwise re-sent (R1) and every bound edge onto it
/// dropped. R2 does not apply (the answer always lands on the structural
/// target), and R3 never drops a bound edge. Bound edges are remembered like
/// any other, so a deleted call's binding is retracted by the next pass.
///
/// An untyped receiver call (`ReceiverCall`, no `replaces`) whose answer lands
/// on an overload set goes through steps 1-4 too, and records its edge with
/// the ordinal when one binds and without it when none does.
///
/// # Re-export hops
///
/// A [`OpenSiteKind::Reference`] site with `replaces` names a structural edge
/// onto a `pending_symbol` placeholder addressed by file and bare name - a use
/// that hops through a re-export or a default import. It is asked only while
/// that file is in the index and declares nothing of that name itself, and
/// not at all when an `OverloadCall` at the same call was kept as an overload
/// question. Its answer goes through the ordinary `definition` rules: one that
/// lands elsewhere contradicts the structural edge, an empty or ambiguous one
/// upholds it (R1). The design is
/// `docs/architecture/gm-325-typescript-lsp-semantics.md`, section 4.3.
///
/// A [`OpenSiteKind::ReceiverField`] site with `replaces` is not a hop: it is
/// always asked, like a typed `ReceiverCall`
/// (`docs/architecture/gm-497-field-reference-sites.md`).
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
/// **Except after a project-model change** (GM-433). A `workspaceChanged`
/// ([`SemanticEngine::workspace_changed`]) unlatches the settle, so the next
/// pass - the whole-project one core runs after the reindex - waits for a
/// full quiet period again. Measured on rust-analyzer after a version bump
/// (GM-429 finding 1): a latched client returned ~13ms after "Building
/// compile-time-deps" ended, while the server spent 3.4-4.5s more rebuilding
/// its crate graph, and every question asked into that window was answered
/// empty (and re-asked) or `ContentModified` (and, before GM-433, counted as
/// a refusal that left the pass incomplete). A per-file `didChange` does not
/// unlatch it; that stays [`LspClient::mark_edited`]'s narrower job. Nor
/// does it for an on-demand server - see [`LspClient::unsettle`].
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
///   lands somewhere else, never when it confirms. Three rules complete it
///   (`docs/architecture/gm-489-structural-semantic-duplicate.md`, §3):
///   - **R1, restore unless contradicted.** In a file the pass finished,
///     every structural edge a site names is re-sent unchanged, `source`
///     still syntactic, unless every answer about it contradicted it. An
///     empty, ambiguous or unresolved answer is not a contradiction. A
///     re-sent edge is never remembered as this bridge's own, so neither its
///     stale-answer retraction nor core's semantic sweep can delete it.
///   - **R2, agreement adds nothing.** An answer agrees when it lands on the
///     structural edge's own target, or would get the structural edge's own
///     id (a placeholder with the same address), or core's linker moved the
///     structural edge onto exactly the answered declaration
///     ([`SdkIndex::linked_target`], from the pass's `linkedEdges`). The last
///     covers a placeholder addressed at a re-export: its address names the
///     re-exporting container, never the declaring one, so only core's link
///     result shows that the two agree. It records no edge. The two
///     ids differ whenever the structural edge points at a declaration of the
///     same file, so recording one would leave two rows for one call each
///     time an edit re-sends the structural edge.
///   - **R3, coverage.** A semantic edge from the same node, of the same
///     kind, onto the declaration a re-sent structural edge lands on - or
///     with that edge's very id - is dropped, and an earlier pass's copy of
///     it is retracted.
/// - **It never guesses which edge a site belonged to.** An open site carries
///   `from_id` and `edge_kind`, and that pair does not name an edge: one
///   function calling two methods of the same name through different
///   receivers produces two sites with identical `(from_id, edge_kind)` and a
///   structural edge that belongs to neither. Retracting on that basis would
///   delete correct edges to fix ones that were never wrong, which is why
///   `replaces` exists instead.
pub struct LspBridge {
    language: String,
    /// `(extension, languageId)` pairs for a `didOpen` - see
    /// [`LspBridge::language_ids`]. A file no pair matches is opened as
    /// `language`.
    language_ids: &'static [(&'static str, &'static str)],
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
    /// The nodes this bridge last re-sent with a shortened `untypedCalls`,
    /// keyed by their file - see [`trim_untyped_calls`]. Only what lets a later
    /// pass put a name back when its answers stop arriving.
    trimmed: BTreeMap<RelPath, BTreeSet<String>>,
    /// Core's deadline for the pass about to be answered, when core sent one
    /// (`budgetMs`, GM-521) - see [`LspBridge::pass_deadline`].
    core_deadline: Option<Instant>,
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
    /// pass that has a question to ask, or by [`SemanticEngine::prepare`]
    /// when core says a whole-project pass is owed - never for structural
    /// work, which is what the lazy-engine contract forbids.
    pub fn new(language: &str, root: &Path, config: SemanticConfig) -> Self {
        Self::with_budgets(language, root, config, Budgets::default())
    }

    /// Opens each file whose name ends with one of the `(extension,
    /// languageId)` pairs' extensions under that `languageId`, and every other
    /// file under the bridge's language.
    ///
    /// A server may parse a document by its `languageId` rather than by its
    /// file name: tsserver reads `typescriptreact` as TSX, and a `.tsx` file
    /// opened as `typescript` is parsed without JSX.
    pub fn language_ids(mut self, by_extension: &'static [(&'static str, &'static str)]) -> Self {
        self.language_ids = by_extension;
        self
    }

    /// Gives each server's first question `budget` instead of
    /// [`Budgets::request`] - see [`Budgets`]'s `warm_up`. For a server that
    /// answers nothing while it loads a project and reports no progress for
    /// the load.
    #[must_use]
    pub fn warm_up(mut self, budget: Duration) -> Self {
        self.budgets.warm_up = Some(budget);
        self
    }

    /// [`LspBridge::new`] with budgets a test can make small - see
    /// [`Budgets`].
    pub fn with_budgets(language: &str, root: &Path, config: SemanticConfig, budgets: Budgets) -> Self {
        let real_root = std::fs::canonicalize(root)
            .map(|resolved| without_verbatim_prefix(&resolved))
            .unwrap_or_else(|_| root.to_path_buf());
        Self {
            language: language.to_string(),
            language_ids: &[],
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
            trimmed: BTreeMap::new(),
            core_deadline: None,
        }
    }

    /// When the pass that starts at `started` must be done.
    ///
    /// Core sent its own deadline (`budgetMs`, GM-521): the pass plans to
    /// three quarters of the time left until it, and the last quarter is the
    /// margin for building, writing and sending the answer. Three quarters is
    /// the ratio [`Budgets`]' own numbers keep to core's defaults (15 of 20
    /// minutes, 90 of 120 seconds), so with core's default timeouts a pass
    /// gets about what it got before - but now also for a residual pass,
    /// whose many files arrive as a per-file-shaped list that only core's
    /// budget tells apart from a per-file pass, and under an overridden core
    /// timeout. Reckoned from `started` rather than from the request, so time
    /// already spent before the engine was asked (hydration) is not planned
    /// twice; it can only make the plan shorter, never past core's deadline.
    ///
    /// No deadline from core (an older core): [`Self::pass_budget`], today's
    /// rules.
    fn pass_deadline(&self, started: Instant, whole_project: bool, files: usize) -> Instant {
        match self.core_deadline {
            Some(core) => {
                let remaining = core.saturating_duration_since(started);
                started + (remaining - remaining / 4)
            }
            None => started + self.pass_budget(whole_project, files),
        }
    }

    /// The whole budget for one pass when core sent none - see [`Budgets`].
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
            crate::log_line!("[{}] the language server is gone - starting a new one", self.language);
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
                        crate::log_line!(
                            "[{}] the language server {} could not be started ({err:#}) - this \
                             language's semantic tier is off for the rest of this process's life; \
                             the structural graph is unaffected",
                            self.language,
                            self.config.command.display()
                        );
                    } else {
                        crate::log_line!(
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
        language: &str,
        language_ids: &[(&str, &str)],
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
                                "languageId": language_id(path, language, language_ids),
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
                crate::log_line!(
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
    /// An [`OpenSiteKind::OverloadCall`] site: which declarations of the
    /// call's overload set does the server say it reaches - see
    /// [`LspBridge`]'s doc on overload binding.
    Overload(OpenSite),
    /// The second hop of an overload answer the server did not narrow to one
    /// declaration: `hover` at the call, or at one candidate declaration's
    /// name. `pending` indexes [`Answers::pending`], which joins the hops.
    ///
    /// `for_file` is the *call's* file. A candidate's hover is asked in the
    /// file that declares it, but it is part of the call's answer: it is that
    /// file whose pass falls short when the hover does, and a declaring file
    /// is not covered just because a call elsewhere asked about one of its
    /// names.
    OverloadHover { pending: usize, at: Hovered, for_file: RelPath },
}

/// Where an [`Ask::OverloadHover`] is asked.
#[derive(Debug, Clone)]
enum Hovered {
    /// At the call - what the server says the call binds.
    Call,
    /// At the name of declaration `ordinal` of `node` - what that overload
    /// looks like through the same printer.
    Candidate { node: String, ordinal: u32 },
}

impl Question {
    fn method(&self) -> &'static str {
        match self.ask {
            Ask::Definition(_) | Ask::Implementor { .. } | Ask::Overload(_) => "textDocument/definition",
            Ask::Implementation { .. } => "textDocument/implementation",
            Ask::OverloadHover { .. } => "textDocument/hover",
        }
    }

    /// The file this question's outcome is accounted to: the one that is not
    /// covered when it goes unanswered, and that is covered when it is. Its
    /// own document, except for a candidate hover - see
    /// [`Ask::OverloadHover`].
    fn accounted_to(&self) -> &RelPath {
        match &self.ask {
            Ask::OverloadHover { for_file, .. } => for_file,
            _ => &self.file,
        }
    }

    /// Whether `result` says nothing - the answer a busy server gives, which
    /// `run_pass` re-asks once after the server settles.
    fn is_empty_answer(&self, result: &Value) -> bool {
        match self.ask {
            Ask::OverloadHover { .. } => hover_text(result).is_none(),
            _ => locations(result).is_empty(),
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
    // Built on the first `OverloadCall` site, once per pass.
    let mut overloaded: Option<Overloaded> = None;
    for path in scope {
        let Some(entry) = index.entry(path) else { continue };
        let mut for_file = Vec::new();
        let mut skipped = 0usize;
        let mut kept_overloads: HashSet<(String, u32, u32)> = HashSet::new();
        for site in &entry.graph.open_sites {
            let ask = match site.kind {
                OpenSiteKind::Implementation => {
                    skipped += 1;
                    continue;
                }
                OpenSiteKind::OverloadCall => {
                    // Only a call whose structural target really is an
                    // overload set is worth a question: a plugin records the
                    // site for every call it cannot rule out. A dropped site
                    // is not unanswerable - its structural edge is the whole
                    // answer - and neither is one without the `replaces` this
                    // kind requires, which has nothing to refine.
                    let Some(replaced) = &site.replaces else { continue };
                    let overloaded = overloaded.get_or_insert_with(|| Overloaded::of(index));
                    if !overloaded.targets(&entry.graph, replaced) {
                        continue;
                    }
                    kept_overloads.insert(site_key(site));
                    Ask::Overload(site.clone())
                }
                OpenSiteKind::Reference if site.replaces.is_some() => {
                    // A hop through a re-export or a default import: asked
                    // only while its placeholder is one the linker cannot
                    // settle on its own. Like a dropped overload call, a
                    // site not asked is not unanswerable - its structural
                    // edge stands as the answer.
                    let replaced = site.replaces.as_deref().unwrap_or_default();
                    if !unsettled_hop(index, &entry.graph, replaced) {
                        continue;
                    }
                    Ask::Definition(site.clone())
                }
                OpenSiteKind::ReceiverCall | OpenSiteKind::ReceiverField | OpenSiteKind::Reference => {
                    Ask::Definition(site.clone())
                }
            };
            for_file.push(Question { file: path.clone(), position: site.position, ask });
        }
        // One question per call: a hop site at a call already kept as an
        // overload question is dropped, because the overload binding is the
        // stronger answer and two answers for one edge would leave two rows.
        // After the loop, so the order sites were recorded in does not matter.
        if !kept_overloads.is_empty() {
            for_file.retain(|question| match &question.ask {
                Ask::Definition(site) if site.kind == OpenSiteKind::Reference && site.replaces.is_some() => {
                    !kept_overloads.contains(&site_key(site))
                }
                _ => true,
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

/// The `languageId` `path` is opened under: the first of `language_ids`
/// whose extension the path ends with, else `language`.
fn language_id<'a>(path: &RelPath, language: &'a str, language_ids: &[(&str, &'a str)]) -> &'a str {
    language_ids
        .iter()
        .find(|(extension, _)| path.as_str().ends_with(extension))
        .map_or(language, |(_, id)| id)
}

/// Where a site is, as [`questions`] matches an overload call with a hop
/// site at the same call: the enclosing node and the name's position.
fn site_key(site: &OpenSite) -> (String, u32, u32) {
    (site.from_id.clone(), site.position.line, site.position.col)
}

/// Whether structural edge `replaced` of `graph` is a hop the linker cannot
/// settle, so a [`OpenSiteKind::Reference`] site naming it is worth a
/// question: the edge lands on a `pending_symbol` placeholder addressed by
/// file `f` and bare name `n`, `f` is in the index, and `f` itself declares
/// nothing named `n`. A file that does declare `n` is where the linker lands
/// the edge already; what is left is `n` re-exported from elsewhere, or
/// `default`.
fn unsettled_hop(index: &SdkIndex, graph: &crate::graph::FileGraph, replaced: &str) -> bool {
    let Some(edge) = graph.edges.iter().find(|edge| edge.id == replaced) else { return false };
    let Some(placeholder) = graph.nodes.iter().find(|node| node.id == edge.to_id) else { return false };
    if placeholder.native_kind.as_deref() != Some(PlaceholderKind::PendingSymbol.native_kind()) {
        return false;
    }
    let Some(target) = &placeholder.target else { return false };
    let (TargetScope::File(file), TargetKey::Name(name)) = (&target.scope, &target.key) else { return false };
    let Some(declaring) = index.entry(&RelPath::new(file)) else { return false };
    !declaring.graph.nodes.iter().any(|node| node.name == *name && is_addressable(node))
}

/// The declarations an [`OpenSiteKind::OverloadCall`] site may be bound to,
/// as the filter in [`questions`] reads them: every node of the index that
/// carries `declarations`, by id, and by the two keys a placeholder can wait
/// on.
struct Overloaded {
    ids: HashSet<String>,
    names: HashSet<String>,
    qualified: HashSet<String>,
}

impl Overloaded {
    fn of(index: &SdkIndex) -> Self {
        let mut overloaded = Self { ids: HashSet::new(), names: HashSet::new(), qualified: HashSet::new() };
        for (_, entry) in index.files() {
            for node in entry.graph.nodes.iter().filter(|node| node.declarations.is_some()) {
                overloaded.ids.insert(node.id.clone());
                overloaded.names.insert(node.name.clone());
                overloaded.qualified.insert(node.qualified_name.clone());
            }
        }
        overloaded
    }

    /// Whether structural edge `replaced` of `graph` lands on an overload
    /// set: a node that carries `declarations`, or a placeholder whose key
    /// names one somewhere in the index.
    fn targets(&self, graph: &crate::graph::FileGraph, replaced: &str) -> bool {
        let Some(edge) = graph.edges.iter().find(|edge| edge.id == replaced) else { return false };
        if self.ids.contains(&edge.to_id) {
            return true;
        }
        graph
            .nodes
            .iter()
            .find(|node| node.id == edge.to_id)
            .and_then(|node| node.target.as_ref())
            .is_some_and(|target| match &target.key {
                TargetKey::Name(name) => self.names.contains(name),
                TargetKey::QualifiedName(qualified) => self.qualified.contains(qualified),
            })
    }
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

/// Where `name` is written inside one of an overload set's declarations, for
/// a `hover` that has to land on it.
///
/// [`name_position`]'s search with one difference: the match must be a whole
/// identifier. A declaration's range includes its decorators, and a function
/// called `load` must not be found inside `@overload`. `None` when the name is
/// not there, which leaves that candidate without a hover and so unmatched.
fn declaration_name_position(source: &str, name: &str, declaration: &WireDeclaration) -> Option<Position> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    if name.is_empty() {
        return None;
    }
    let width = name.chars().count();
    for line in declaration.start_line..=declaration.end_line.max(declaration.start_line) {
        let chars: Vec<char> = line_text(source, line).chars().collect();
        let from = if line == declaration.start_line { declaration.start_col as usize } else { 0 };
        let to = if line == declaration.end_line {
            (declaration.end_col as usize).min(chars.len())
        } else {
            chars.len()
        };
        let mut col = from;
        while col + width <= to {
            let here: String = chars[col..col + width].iter().collect();
            let before = col.checked_sub(1).map(|at| chars[at]);
            let after = chars.get(col + width).copied();
            if here == name && !before.is_some_and(word) && !after.is_some_and(word) {
                return Some(Position { line, col: col as u32 });
            }
            col += 1;
        }
    }
    None
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
    /// Every edge id this pass recorded, kept or not - what makes a second
    /// site with the same edge a no-op.
    edges: HashSet<String>,
    /// The recorded edges, in the order they were recorded. Held rather than
    /// added to a builder on the spot, so that [`Answers::settle`] can drop
    /// the ones a re-sent structural edge already covers (R3) before
    /// they reach the diff or `by_file`.
    recorded: Vec<Recorded>,
    by_file: BTreeMap<RelPath, Vec<String>>,
    retract: BTreeSet<String>,
    /// Structural edges (`OpenSite::replaces`) an answer agreed with, and the
    /// `(from, kind, declaration)` each agreeing answer landed on (R2).
    /// An agreeing answer records nothing; this is what R3 uses to know
    /// which declaration a structural edge onto a placeholder stands for.
    confirmed: BTreeMap<String, Vec<(String, EdgeKind, String)>>,
    /// Structural edges at least one answer contradicted - it landed on
    /// another declaration.
    contradicted: BTreeSet<String>,
    /// Structural edges at least one answer did *not* contradict: it agreed,
    /// or it was empty, ambiguous or outside the index. One such answer is
    /// enough to keep the edge (R1).
    upheld: BTreeSet<String>,
    /// The structural edges this pass re-sends unchanged (R1), set by
    /// [`Answers::settle`].
    resend: Vec<WireEdge>,
    /// How many untyped receiver-call sites (`ReceiverCall`, no `replaces`)
    /// this pass got an answer for, by `(from_id, name)` - see
    /// [`untyped_call_answered`] and [`trim_untyped_calls`] (GM-486).
    untyped_answered: HashMap<(String, String), usize>,
    /// How this server's overload answers are narrowed - the manifest's
    /// `overload_disambiguation`.
    disambiguation: OverloadDisambiguation,
    /// Bound [`OpenSiteKind::OverloadCall`] sites, by the structural edge
    /// they refine: the site's position, and the bound edge it recorded. A
    /// site of that edge that is missing here was left unbound, or never
    /// answered - and either keeps the structural edge (all or nothing, see
    /// [`Answers::settle`]).
    overload_bound: BTreeMap<String, BTreeMap<(u32, u32), String>>,
    /// Overload answers waiting on their `hover` hop, by
    /// [`Ask::OverloadHover::pending`]. `None` once concluded.
    pending: Vec<Option<PendingOverload>>,
    /// Each candidate declaration's normalised hover, by `(node, ordinal)`,
    /// for the pass: a set called from fifty places is hovered once. An empty
    /// string is a hover that said nothing.
    hovers: HashMap<(String, u32), String>,
}

/// An overload answer whose candidates only `hover` can tell apart - the
/// join of its [`Ask::OverloadHover`] questions.
struct PendingOverload {
    landing: Landing,
    candidates: Vec<u32>,
    /// The call's normalised hover, once answered.
    call: Option<String>,
    /// Hover questions still unanswered.
    waiting: usize,
}

/// Where an overload answer landed, and the site it answers: everything
/// [`Answers::conclude`] needs to record the outcome.
struct Landing {
    /// The call's file.
    file: RelPath,
    site: OpenSite,
    node_id: String,
    node_name: String,
    address: PlaceholderTarget,
}

/// One semantic edge this pass recorded, waiting for [`Answers::finish`].
struct Recorded {
    id: String,
    /// The file the edge starts in, and so the builder it goes into.
    in_file: RelPath,
    from_id: String,
    kind: EdgeKind,
    /// The placeholder the edge lands on.
    to_id: String,
    /// The declaration the placeholder waits on - what R3 compares a re-sent
    /// structural edge's target with.
    declaration: String,
    /// The ordinal of the declaration the call binds, for a bound overload
    /// call - written to `toDeclaration`, and part of the edge's id.
    to_declaration: Option<u32>,
}

/// The placeholder id and the edge id an answer from `from_id` onto `target`
/// gets in `in_file`. One formula for [`Answers::record`] and for the
/// agreement test in [`record_answer`], so the two cannot drift apart.
///
/// `to_declaration` is part of the edge id and not of the placeholder's: two
/// calls of two overloads from one caller are two edges onto one placeholder.
fn answer_ids(
    in_file: &RelPath,
    from_id: &str,
    kind: EdgeKind,
    target: &PlaceholderTarget,
    to_declaration: Option<u32>,
) -> (String, String) {
    let placeholder = crate::graph::placeholder_id(in_file, PlaceholderKind::PendingSymbol, target);
    let edge = crate::ids::edge_id(from_id, kind, &placeholder, to_declaration);
    (placeholder, edge)
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
            recorded: Vec::new(),
            by_file: BTreeMap::new(),
            retract: BTreeSet::new(),
            confirmed: BTreeMap::new(),
            contradicted: BTreeSet::new(),
            upheld: BTreeSet::new(),
            resend: Vec::new(),
            untyped_answered: HashMap::new(),
            disambiguation: OverloadDisambiguation::default(),
            overload_bound: BTreeMap::new(),
            pending: Vec::new(),
            hovers: HashMap::new(),
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
    /// returned by the builder. The edge waits there too, so that
    /// [`Answers::settle`] can still drop it (R3).
    ///
    /// `declaration` is the id of the node the answer landed on, which the
    /// placeholder addresses; `to_declaration` the ordinal of the one of its
    /// declarations a bound overload call binds.
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
        declaration: &str,
        to_declaration: Option<u32>,
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
                let (id, _) = answer_ids(in_file, from_id, kind, &target, to_declaration);
                self.placeholders
                    .insert(key, Placeholder { id: id.clone(), target, name: name.to_string(), range: at });
                id
            }
        };

        let id = crate::ids::edge_id(from_id, kind, &placeholder, to_declaration);
        if !self.edges.insert(id.clone()) {
            return id;
        }
        self.recorded.push(Recorded {
            id: id.clone(),
            in_file: in_file.clone(),
            from_id: from_id.to_string(),
            kind,
            to_id: placeholder,
            declaration: declaration.to_string(),
            to_declaration,
        });
        self.by_file.entry(for_file.clone()).or_default().push(id.clone());
        id
    }

    /// Settles what this pass says about the structural edges its sites named
    /// in `replaces`, for the files it finished - see [`LspBridge`]'s doc on
    /// retraction. Called once, after the last answer and before
    /// `by_file` is read for retraction.
    ///
    /// - **R1, restore unless contradicted.** Every structural edge a site in
    ///   a finished file names is re-sent unchanged, as the index holds it,
    ///   unless every answer about it contradicted it. It never enters
    ///   `by_file`, so neither `retract_stale` nor core's semantic sweep can
    ///   reach it.
    /// - **R3, coverage.** A recorded semantic edge whose id is a re-sent
    ///   edge's, or that goes from the same node, of the same kind, onto the
    ///   declaration a re-sent edge already lands on, is dropped: from the
    ///   diff and from `by_file`, so an earlier pass's copy of it is
    ///   retracted.
    ///
    /// - **Overload binding, all or nothing per edge.** A structural edge
    ///   that [`OpenSiteKind::OverloadCall`] sites refine is retracted only
    ///   when *every* such site of it in a finished file bound an ordinal;
    ///   its bound edges are kept. Otherwise it is re-sent by R1 and every
    ///   bound edge refining it is dropped, as is every bound edge whose file
    ///   was not finished. Exempt from R3: an unbound edge never covers a
    ///   bound one.
    ///
    /// A file the pass did not finish is not touched: its sites' answers may
    /// be missing, and "no answer" is not "the structural edge stands" until
    /// the question was actually put.
    fn settle(&mut self, index: &SdkIndex, finished: &BTreeSet<RelPath>) {
        let bound = self.settle_overloads(index, finished);
        let mut seen: HashSet<String> = HashSet::new();
        for file in finished {
            let Some(graph) = index.graph(file) else { continue };
            for site in &graph.open_sites {
                let Some(replaced) = &site.replaces else { continue };
                if bound.contains(replaced) {
                    continue;
                }
                if self.contradicted.contains(replaced) && !self.upheld.contains(replaced) {
                    continue;
                }
                if !seen.insert(replaced.clone()) {
                    continue;
                }
                if let Some(edge) = graph.edges.iter().find(|edge| &edge.id == replaced) {
                    self.resend.push(edge.clone());
                }
            }
        }
        if self.resend.is_empty() {
            return;
        }

        // What the re-sent edges cover: their own ids, and the declarations
        // they land on - their own target, or whatever an agreeing answer
        // said a placeholder target stands for.
        let ids: HashSet<&str> = self.resend.iter().map(|edge| edge.id.as_str()).collect();
        let mut lands: HashMap<(&str, &str), Vec<EdgeKind>> = HashMap::new();
        for edge in &self.resend {
            lands.entry((edge.from_id.as_str(), edge.to_id.as_str())).or_default().push(edge.kind);
            for (from, kind, declaration) in self.confirmed.get(&edge.id).into_iter().flatten() {
                lands.entry((from.as_str(), declaration.as_str())).or_default().push(*kind);
            }
        }
        // A bound overload edge says more than any structural edge onto the
        // same function: it is never covered.
        let covered = |edge: &Recorded| {
            edge.to_declaration.is_none()
                && (ids.contains(edge.id.as_str())
                    || lands
                        .get(&(edge.from_id.as_str(), edge.declaration.as_str()))
                        .is_some_and(|kinds| kinds.contains(&edge.kind)))
        };
        let dropped: HashSet<String> =
            self.recorded.iter().filter(|edge| covered(edge)).map(|edge| edge.id.clone()).collect();
        if dropped.is_empty() {
            return;
        }
        self.recorded.retain(|edge| !dropped.contains(&edge.id));
        for ids in self.by_file.values_mut() {
            ids.retain(|id| !dropped.contains(id));
        }
    }

    /// The overload half of [`Answers::settle`]: retracts every structural
    /// edge all of whose [`OpenSiteKind::OverloadCall`] sites in `finished`
    /// files bound an ordinal, drops every other bound edge, and returns the
    /// retracted edges - which R1 must then not re-send.
    fn settle_overloads(&mut self, index: &SdkIndex, finished: &BTreeSet<RelPath>) -> BTreeSet<String> {
        let mut sites: BTreeMap<&str, Vec<(u32, u32)>> = BTreeMap::new();
        for file in finished {
            let Some(graph) = index.graph(file) else { continue };
            for site in graph.open_sites.iter().filter(|site| site.kind == OpenSiteKind::OverloadCall) {
                let Some(replaced) = &site.replaces else { continue };
                sites.entry(replaced.as_str()).or_default().push((site.position.line, site.position.col));
            }
        }
        let bound: BTreeSet<String> = sites
            .into_iter()
            .filter(|(replaced, positions)| {
                self.overload_bound
                    .get(*replaced)
                    .is_some_and(|by_site| positions.iter().all(|position| by_site.contains_key(position)))
            })
            .map(|(replaced, _)| replaced.to_string())
            .collect();
        // One edge id can serve two structural edges (two calls of one
        // overload from one caller): kept if either is wholly bound.
        let keep: HashSet<&String> = bound
            .iter()
            .filter_map(|replaced| self.overload_bound.get(replaced))
            .flat_map(|by| by.values())
            .collect();
        let dropped: HashSet<String> = self
            .overload_bound
            .iter()
            .filter(|(replaced, _)| !bound.contains(*replaced))
            .flat_map(|(_, by_site)| by_site.values())
            .filter(|id| !keep.contains(id))
            .cloned()
            .collect();
        if !dropped.is_empty() {
            self.recorded.retain(|edge| !dropped.contains(&edge.id));
            for ids in self.by_file.values_mut() {
                ids.retain(|id| !dropped.contains(id));
            }
        }
        self.retract.extend(bound.iter().cloned());
        bound
    }

    /// Records what one overload answer concluded: a bound edge for a bound
    /// [`OpenSiteKind::OverloadCall`] (booked for [`Answers::settle`]),
    /// nothing for an unbound one (its structural edge stands), and for a
    /// receiver call the edge it would have had anyway, with the ordinal when
    /// there is one.
    fn conclude(&mut self, landing: Landing, ordinal: Option<u32>) {
        let site = &landing.site;
        if site.kind == OpenSiteKind::OverloadCall && ordinal.is_none() {
            return;
        }
        let id = self.record(
            &landing.file,
            &landing.file,
            &site.from_id,
            site.edge_kind,
            &landing.node_name,
            site_range(site.position, &site.name),
            landing.address.clone(),
            &landing.node_id,
            ordinal,
        );
        if site.kind == OpenSiteKind::OverloadCall {
            if let Some(replaced) = &site.replaces {
                self.overload_bound
                    .entry(replaced.clone())
                    .or_default()
                    .insert((site.position.line, site.position.col), id);
            }
        }
    }

    /// Steps 2-4 of [`LspBridge`]'s overload binding for an answer that
    /// landed on `node`'s declarations `ordinals`: binds, leaves unbound, or
    /// returns the `hover` questions that will decide.
    fn bind_overload(
        &mut self,
        index: &SdkIndex,
        landing: Landing,
        node: &WireNode,
        node_file: &RelPath,
        ordinals: &BTreeSet<u32>,
    ) -> Vec<Question> {
        let candidates = match choose_overload(node, ordinals, self.disambiguation) {
            Choice::Bound(ordinal) => {
                self.conclude(landing, Some(ordinal));
                return Vec::new();
            }
            Choice::Unbound => {
                self.conclude(landing, None);
                return Vec::new();
            }
            Choice::Hover(candidates) => candidates,
        };
        let pending = self.pending.len();
        let mut again = vec![Question {
            file: landing.file.clone(),
            position: landing.site.position,
            ask: Ask::OverloadHover { pending, at: Hovered::Call, for_file: landing.file.clone() },
        }];
        let source = index.source(node_file).unwrap_or_default();
        for &ordinal in &candidates {
            if self.hovers.contains_key(&(node.id.clone(), ordinal)) {
                continue;
            }
            let declaration =
                node.declarations.iter().flatten().find(|declaration| declaration.ordinal == ordinal);
            let Some(position) = declaration
                .and_then(|declaration| declaration_name_position(source, &node.name, declaration))
            else {
                // Nowhere to hover: the candidate can never match. Settled now
                // so it is not asked again for the next call.
                self.hovers.insert((node.id.clone(), ordinal), String::new());
                continue;
            };
            again.push(Question {
                file: node_file.clone(),
                position,
                ask: Ask::OverloadHover {
                    pending,
                    at: Hovered::Candidate { node: node.id.clone(), ordinal },
                    for_file: landing.file.clone(),
                },
            });
        }
        self.pending.push(Some(PendingOverload { landing, candidates, call: None, waiting: again.len() }));
        again
    }

    /// One `hover` answer of a pending overload; concludes it when it was the
    /// last one outstanding.
    fn hovered(&mut self, pending: usize, at: &Hovered, text: Option<String>) {
        let text = text.unwrap_or_default();
        let Some(Some(waiting)) = self.pending.get_mut(pending) else { return };
        match at {
            Hovered::Call => waiting.call = Some(text),
            Hovered::Candidate { node, ordinal } => {
                self.hovers.insert((node.clone(), *ordinal), text);
            }
        }
        waiting.waiting = waiting.waiting.saturating_sub(1);
        if waiting.waiting > 0 {
            return;
        }
        let Some(done) = self.pending[pending].take() else { return };
        let call = done.call.unwrap_or_default();
        let matching: Vec<u32> = done
            .candidates
            .iter()
            .copied()
            .filter(|ordinal| {
                self.hovers
                    .get(&(done.landing.node_id.clone(), *ordinal))
                    .is_some_and(|declared| hover_matches(declared, &call))
            })
            .collect();
        let ordinal = match matching.as_slice() {
            [one] => Some(*one),
            _ => None,
        };
        self.conclude(done.landing, ordinal);
    }

    fn finish(mut self) -> (FileChangeDiff, BTreeMap<RelPath, Vec<String>>) {
        let mut diff = FileChangeDiff::default();
        // The edges that survived `settle`, in the order they were recorded,
        // and only the placeholders one of them still lands on.
        let recorded = std::mem::take(&mut self.recorded);
        let used: HashSet<&str> = recorded.iter().map(|edge| edge.to_id.as_str()).collect();
        let kept: HashSet<String> = recorded.iter().map(|edge| edge.id.clone()).collect();
        for edge in &recorded {
            let engine = self.engine.clone();
            self.builder(&edge.in_file).add_edge(EdgeSpec {
                from_id: edge.from_id.clone(),
                to_id: edge.to_id.clone(),
                kind: edge.kind,
                // Every edge here lands on a placeholder, so nothing is
                // confirmed until core links it: `resolved` describes what the
                // edge points at, never who produced it.
                resolved: false,
                to_declaration: edge.to_declaration,
                source: SourceTier::Semantic,
                engine,
            });
        }
        // The nodes, now that every site addressing each one has been seen. In
        // address order, which is the file's own order and not the order the
        // server happened to answer in - the same reason the row itself is
        // chosen rather than taken (see `Placeholder`).
        for ((file, ..), held) in std::mem::take(&mut self.placeholders) {
            if !used.contains(held.id.as_str()) {
                continue;
            }
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
        // The structural edges R1 restores, exactly as the index holds them:
        // `source` stays syntactic, so core's semantic sweep never reads them
        // as this tier's.
        let resent: HashSet<String> = self.resend.iter().map(|edge| edge.id.clone()).collect();
        diff.upsert_edges.extend(std::mem::take(&mut self.resend));
        // An edge this pass re-emitted is not stale, whatever an earlier pass
        // recorded about it: retracting and upserting one id in one diff is a
        // delete followed by an insert of the same row, which is at best a
        // waste and at worst a foreign-key fault for anything pointing at it.
        // The same holds for a structural edge one site contradicted and
        // another upheld: it is re-sent, so it is not retracted.
        diff.delete_edge_ids =
            self.retract.into_iter().filter(|id| !kept.contains(id) && !resent.contains(id)).collect();
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
        key_path: node.qualified_path.clone(),
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

/// The overload declaration a location points into: the tightest
/// `declarations` range of any node of its file that contains it, with that
/// node and the declaration's ordinal.
///
/// Not [`node_at`]: a node's own range is not its overload set's extent.
/// Python's node is the first `@overload` stub, so a server pointing at the
/// second one lands, by node range, on the enclosing class or file; and
/// TypeScript's node is the implementation.
fn declaration_at<'i>(
    index: &'i SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    location: &ServerLocation,
) -> Option<(RelPath, &'i WireNode, u32)> {
    let path = file_at(index, roots, location)?;
    let at = wire_position(index, &path, encoding, location);
    let at = (at.line, at.col);
    let graph = index.graph(&path)?;
    let mut best: Option<((u32, u32), &WireNode, u32)> = None;
    for node in graph.nodes.iter().filter(|node| is_addressable(node)) {
        for declaration in node.declarations.iter().flatten() {
            let (start, end) = (
                (declaration.start_line, declaration.start_col),
                (declaration.end_line, declaration.end_col),
            );
            if !(start <= at && at <= end) {
                continue;
            }
            let lines = end.0.saturating_sub(start.0);
            let span = (lines, if lines == 0 { end.1.saturating_sub(start.1) } else { end.1 });
            let tighter = best.as_ref().is_none_or(|(held, held_node, held_ordinal)| {
                (span, &node.id, declaration.ordinal) < (*held, &held_node.id, *held_ordinal)
            });
            if tighter {
                best = Some((span, node, declaration.ordinal));
            }
        }
    }
    best.map(|(_, node, ordinal)| (path, node, ordinal))
}

/// Step 1 of overload binding: the one node every location of `found` lands
/// on, by declaration containment first and node containment otherwise, with
/// the ordinals of its declarations they land in. `None` when nothing lands
/// in the index, or two locations land on different nodes (the `agree` rule
/// of a plain `definition` answer).
fn overload_landing<'i>(
    index: &'i SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    found: &[ServerLocation],
) -> Option<(RelPath, &'i WireNode, BTreeSet<u32>)> {
    let mut landed: Option<(RelPath, &WireNode, BTreeSet<u32>)> = None;
    for location in found {
        let (path, node, ordinal) = match declaration_at(index, roots, encoding, location) {
            Some((path, node, ordinal)) => (path, node, Some(ordinal)),
            None => match node_at(index, roots, encoding, location) {
                Some((path, node)) => (path, node, None),
                None => continue,
            },
        };
        match &mut landed {
            Some((_, held, _)) if held.id != node.id => return None,
            Some((_, _, ordinals)) => ordinals.extend(ordinal),
            None => landed = Some((path, node, ordinal.into_iter().collect())),
        }
    }
    landed
}

/// What steps 2-4 of overload binding make of the ordinals an answer landed
/// in.
enum Choice {
    Bound(u32),
    Unbound,
    /// Ask `hover` at the call and at each of these.
    Hover(Vec<u32>),
}

/// Steps 2-4: exactly one bindable ordinal binds; several go to `hover` when
/// the manifest allows it and stay unbound otherwise. A declaration with a
/// body is never bindable in a set that has bodiless ones: no call binds an
/// implementation, so a server pointing there is answering "the function",
/// not "the overload".
fn choose_overload(
    node: &WireNode,
    ordinals: &BTreeSet<u32>,
    disambiguation: OverloadDisambiguation,
) -> Choice {
    let Some(declarations) = &node.declarations else { return Choice::Unbound };
    let stubs = declarations.iter().any(|declaration| !declaration.has_body);
    let bindable = |ordinal: u32| {
        declarations
            .iter()
            .find(|declaration| declaration.ordinal == ordinal)
            .is_some_and(|declaration| !(stubs && declaration.has_body))
    };
    match ordinals.len() {
        1 => {
            let ordinal = *ordinals.iter().next().expect("one ordinal");
            if bindable(ordinal) {
                Choice::Bound(ordinal)
            } else {
                Choice::Unbound
            }
        }
        0 => Choice::Unbound,
        _ if disambiguation == OverloadDisambiguation::Hover => {
            let candidates: Vec<u32> =
                ordinals.iter().copied().filter(|ordinal| bindable(*ordinal)).collect();
            if candidates.is_empty() {
                Choice::Unbound
            } else {
                Choice::Hover(candidates)
            }
        }
        _ => Choice::Unbound,
    }
}

/// Whether an overload answer agrees with the structural edge its site
/// refines: the edge lands on `node`, or on a placeholder that waits on it -
/// one whose id the answer itself would get, or whose key names `node`.
/// Anything else would move the call to another target, which refining
/// never does.
fn refines(index: &SdkIndex, file: &RelPath, site: &OpenSite, replaced: &str, node: &WireNode) -> bool {
    let Some(graph) = index.graph(file) else { return false };
    let Some(edge) = graph.edges.iter().find(|edge| edge.id == replaced) else { return false };
    if edge.to_id == node.id {
        return true;
    }
    let address = address_of(node, site.from_container.clone());
    let (_, prospective) = answer_ids(file, &site.from_id, site.edge_kind, &address, None);
    if prospective == replaced {
        return true;
    }
    graph.nodes.iter().find(|held| held.id == edge.to_id).and_then(|held| held.target.as_ref()).is_some_and(
        |target| match &target.key {
            TargetKey::Name(name) => name == &node.name,
            TargetKey::QualifiedName(qualified) => qualified == &node.qualified_name,
        },
    )
}

/// The signature a `hover` result shows, normalised for comparison; `None`
/// when it shows nothing.
///
/// `contents` may be a string, a `MarkupContent`, a `MarkedString` object, or
/// an array of either. The signature is the first fenced code block when
/// there is one, and the first paragraph otherwise: what follows is
/// documentation, which a call and a declaration need not share.
fn hover_text(result: &Value) -> Option<String> {
    fn collect(value: &Value, into: &mut Vec<String>) {
        match value {
            Value::String(text) => into.push(text.clone()),
            Value::Array(values) => values.iter().for_each(|value| collect(value, into)),
            Value::Object(object) => {
                if let Some(text) = object.get("value").and_then(Value::as_str) {
                    into.push(text.to_string());
                }
            }
            _ => {}
        }
    }
    let mut parts = Vec::new();
    collect(result.get("contents")?, &mut parts);
    let text = parts.join("\n");
    let signature = match text.find("```") {
        Some(fence) => {
            let body = &text[fence + 3..];
            let body = body.find('\n').map_or("", |newline| &body[newline + 1..]);
            body.find("```").map_or(body, |end| &body[..end])
        }
        None => text.trim().split("\n\n").next().unwrap_or_default(),
    };
    let normalised = normalise_hover(signature);
    (!normalised.is_empty()).then_some(normalised)
}

/// Whitespace collapsed to single spaces, and none just inside brackets or
/// before a comma - so a signature a printer wraps over several lines equals
/// the same signature on one.
fn normalise_hover(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(collapsed.len());
    let chars: Vec<char> = collapsed.chars().collect();
    for (at, &c) in chars.iter().enumerate() {
        if c == ' ' {
            let before = out.chars().last();
            let after = chars.get(at + 1).copied();
            if matches!(before, Some('(') | Some('[')) || matches!(after, Some(')') | Some(']') | Some(',')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Step 3's match: a candidate declaration's hover equals the call's, or does
/// once its first parameter is dropped (a method called through a bound
/// receiver). Two empty hovers never match.
fn hover_matches(declared: &str, call: &str) -> bool {
    if declared.is_empty() || call.is_empty() {
        return false;
    }
    declared == call || without_first_parameter(declared).is_some_and(|dropped| dropped == call)
}

/// `text` with the first parameter of its first parameter list removed - the
/// first `(` that directly follows an identifier, so a `(method)` prefix is
/// not taken for one. `None` when there is no list or it is empty.
fn without_first_parameter(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let open = (1..chars.len())
        .find(|&at| chars[at] == '(' && (chars[at - 1].is_alphanumeric() || chars[at - 1] == '_'))?;
    let mut depth = 0usize;
    let mut comma = None;
    let mut close = None;
    for (at, &c) in chars.iter().enumerate().skip(open + 1) {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth > 0 => depth -= 1,
            ')' => {
                close = Some(at);
                break;
            }
            ',' if depth == 0 && comma.is_none() => comma = Some(at),
            _ => {}
        }
    }
    let close = close?;
    if close == open + 1 {
        return None;
    }
    let rest_from = comma.map_or(close, |comma| comma + 1);
    let head: String = chars[..=open].iter().collect();
    let rest: String = chars[rest_from..].iter().collect();
    Some(format!("{head}{}", rest.trim_start()))
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
    disambiguation: OverloadDisambiguation,
) -> (Answers, BTreeSet<RelPath>, Option<String>) {
    let mut answers = Answers::new(language, engine);
    answers.disambiguation = disambiguation;
    let mut queue: Vec<Question> = asking.into_iter().rev().collect();
    // Questions whose empty answer arrived while the server was indexing. They
    // are *not* put straight back on the queue: re-asking a busy server
    // immediately gets the same empty answer, and doing that in a loop is a
    // spin rather than a retry. They wait until the server says it has
    // finished and are asked once more then - see `LspBridge`'s readiness
    // rules. `re_asked` is what keeps "once more" from being "forever".
    let mut deferred: Vec<Question> = Vec::new();
    let mut re_asked: HashSet<(RelPath, u32, u32, &'static str)> = HashSet::new();
    let mut in_flight: BTreeMap<i64, (Question, Instant)> = BTreeMap::new();
    let mut failed_files: BTreeSet<RelPath> = BTreeSet::new();
    let mut touched_files: BTreeSet<RelPath> = BTreeSet::new();
    // The first reason the pass fell short, if it did: `None` is a complete
    // pass.
    let mut failure: Option<String> = None;
    let mut fail = |reason: String| {
        failure.get_or_insert(reason);
    };
    // Never shorter than `request`: a warm-up is a longer budget or none.
    let warm_up = budgets.warm_up.map(|warm_up| warm_up.max(budgets.request));
    let mut warm_up_logged = false;

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
        // Fill the pipeline - to one question while the server's warm-up is
        // owed (see `Budgets::warm_up`), to `concurrency` after.
        let warming = warm_up.is_some() && !client.warmed_up();
        let width = if warming { 1 } else { budgets.concurrency.max(1) };
        let request_budget = match warm_up {
            Some(warm_up) if warming => warm_up,
            _ => budgets.request,
        };
        if warming && !warm_up_logged && !queue.is_empty() {
            warm_up_logged = true;
            crate::log_line!(
                "[{language}] the server has not answered yet - its first question may take up to {request_budget:?} \
                 (warm-up), the rest {:?}",
                budgets.request
            );
        }
        while in_flight.len() < width {
            let Some(question) = queue.pop() else { break };
            touched_files.insert(question.accounted_to().clone());
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
                    failed_files.insert(question.accounted_to().clone());
                    // A pipe broken by a server that has died is its death,
                    // not a failure to write.
                    match client.exit_status(EXIT_GRACE) {
                        Some(status) => {
                            crate::log_line!(
                                "[{language}] the language server exited during the pass ({status})"
                            );
                            fail(exited(Some(status)));
                        }
                        None => {
                            crate::log_line!("[{language}] could not ask the language server ({err:#})");
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
            crate::log_line!(
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
                failed_files.insert(question.accounted_to().clone());
            }
            for question in queue.drain(..).chain(deferred.drain(..)) {
                failed_files.insert(question.accounted_to().clone());
            }
            break;
        }

        if in_flight.is_empty() {
            // Only deferred questions left: nothing to time out, and nothing
            // to do but wait for the server to stop indexing.
            if matches!(client.poll(Duration::from_millis(25)), Poll::Closed) {
                for question in deferred.drain(..) {
                    failed_files.insert(question.accounted_to().clone());
                }
                fail(exited(client.exit_status(EXIT_GRACE)));
                break;
            }
            continue;
        }

        // Wake up for whichever comes first: an answer, the oldest request's
        // own timeout, or the end of the pass.
        let oldest = in_flight.values().map(|(_, sent)| *sent).min().unwrap_or(now);
        let wake = (oldest + request_budget).min(deadline);
        let wait = wake.saturating_duration_since(now).max(Duration::from_millis(1));

        match client.poll(wait) {
            Poll::Answered { id, result } => {
                let Some((question, _)) = in_flight.remove(&id) else { continue };
                // Any answer, empty or not, is a server that is answering.
                client.mark_warmed_up();
                // An empty answer while the server is indexing is not an
                // answer: re-ask it once, after the indexing ends.
                let empty = question.is_empty_answer(&result);
                let key =
                    (question.file.clone(), question.position.line, question.position.col, question.method());
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
            Poll::Failed { id, code, message } => {
                let Some((question, _)) = in_flight.remove(&id) else { continue };
                client.mark_warmed_up();
                // `ContentModified` is not a refusal: the server's state moved
                // under the question (rust-analyzer switching crate graphs,
                // GM-433), so it is asked again once the server is quiet -
                // the same deferral, under the same once-only `re_asked`
                // rule, as an empty answer from a busy server. A second
                // `ContentModified` for the same site falls through to the
                // refusal below, so "again" cannot become "forever".
                if code == Some(CONTENT_MODIFIED) {
                    let key = (
                        question.file.clone(),
                        question.position.line,
                        question.position.col,
                        question.method(),
                    );
                    if re_asked.insert(key) {
                        deferred.push(question);
                        continue;
                    }
                }
                // A server that refuses one question has not answered it, so
                // the file it was in is not covered - but the pass goes on:
                // one bad position is not a reason to drop the other nine
                // thousand answers.
                crate::log_line!(
                    "[{language}] the server refused a question about {} ({message})",
                    question.file
                );
                fail(format!("the language server refused a question about {} ({message})", question.file));
                failed_files.insert(question.accounted_to().clone());
            }
            Poll::Closed => {
                crate::log_line!(
                    "[{language}] the language server exited during the pass - keeping the {} \
                     answer(s) it did give and reporting the pass incomplete",
                    answers.edges.len()
                );
                for (question, _) in std::mem::take(&mut in_flight).into_values() {
                    failed_files.insert(question.accounted_to().clone());
                }
                for question in queue.drain(..) {
                    failed_files.insert(question.accounted_to().clone());
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
                    .filter(|(_, (_, sent))| now.duration_since(*sent) >= request_budget)
                    .map(|(id, _)| *id)
                    .collect();
                if !expired.is_empty() {
                    // A timed-out warm-up is spent too: it is not re-armed,
                    // so a server that never answers costs one long wait.
                    client.mark_warmed_up();
                }
                for id in expired {
                    let Some((question, _)) = in_flight.remove(&id) else { continue };
                    client.cancel(id);
                    crate::log_line!(
                        "[{language}] the server did not answer a question about {} within {:?}{}",
                        question.file,
                        request_budget,
                        if warming { " (its warm-up budget)" } else { "" }
                    );
                    fail(format!(
                        "the language server did not answer a question about {} within {:?}",
                        question.file, request_budget
                    ));
                    failed_files.insert(question.accounted_to().clone());
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
/// The returned questions are the second hop of an implementation answer, or
/// the `hover` hop of an overload answer, and nothing else - see
/// [`LspBridge`]'s doc. Neither second hop returns any, which is what makes
/// the recursion one hop deep by construction rather than by a counter.
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
            // An untyped receiver call onto an overload set takes the
            // overload path: the same edge, plus the ordinal when one can be
            // bound. A call that names a structural edge keeps the rules
            // below, which an ordinal would not survive (R2 records nothing).
            if site.kind == OpenSiteKind::ReceiverCall && site.replaces.is_none() {
                if let Some((path, node, ordinals)) = overload_landing(index, roots, encoding, &found) {
                    if node.declarations.is_some() {
                        *answers
                            .untyped_answered
                            .entry((site.from_id.clone(), site.name.clone()))
                            .or_default() += 1;
                        let landing = Landing {
                            file: question.file.clone(),
                            site: site.clone(),
                            node_id: node.id.clone(),
                            node_name: node.name.clone(),
                            address: address_of(node, site.from_container.clone()),
                        };
                        return answers.bind_overload(index, landing, node, &path, &ordinals);
                    }
                }
            }
            // Several locations are the normal answer for a `cfg`-gated or
            // overloaded declaration. They are only usable when they agree:
            // core's linker refuses an ambiguous address by design, and
            // picking one here would be this bridge making the guess the
            // linker declines to make.
            let mut target: Option<(RelPath, &WireNode)> = None;
            let mut agree = true;
            for location in &found {
                let Some((path, node)) = node_at(index, roots, encoding, location) else { continue };
                match &target {
                    Some((_, chosen)) if chosen.id != node.id => {
                        agree = false;
                        break;
                    }
                    Some(_) => {}
                    None => target = Some((path, node)),
                }
            }
            if untyped_call_answered(index, roots, site, &found, agree && target.is_some()) {
                *answers.untyped_answered.entry((site.from_id.clone(), site.name.clone())).or_default() += 1;
            }
            let landed = if agree { target } else { None };
            let Some((_, node)) = landed else {
                // Empty, ambiguous, or nothing this index holds: no evidence
                // against the structural edge, which therefore stands (R1).
                if let Some(replaced) = &site.replaces {
                    answers.upheld.insert(replaced.clone());
                }
                return Vec::new();
            };
            let address = address_of(node, site.from_container.clone());
            if let Some(replaced) = &site.replaces {
                // R2, agreement adds nothing: the answer lands on the
                // structural edge's own target (`Bound::Here`), or would get
                // the structural edge's own id (`Bound::There`, one address),
                // or core linked the structural edge onto it (through a
                // re-export the edge's placeholder address names, which the
                // answered declaration's own address never matches).
                // Recording it anyway leaves two rows for one call once an
                // edit re-sends the structural edge - see [`LspBridge`]'s doc.
                let (_, prospective) =
                    answer_ids(&question.file, &site.from_id, site.edge_kind, &address, None);
                let lands_on_it = index
                    .graph(&question.file)
                    .and_then(|graph| graph.edges.iter().find(|edge| &edge.id == replaced))
                    .is_some_and(|edge| edge.to_id == node.id);
                let linked_on_it = index.linked_target(replaced) == Some(node.id.as_str());
                if lands_on_it || linked_on_it || &prospective == replaced {
                    answers.upheld.insert(replaced.clone());
                    answers.confirmed.entry(replaced.clone()).or_default().push((
                        site.from_id.clone(),
                        site.edge_kind,
                        node.id.clone(),
                    ));
                    return Vec::new();
                }
            }
            // The placeholder is named after the declaration it waits on, not
            // after the text at this site: `SearchMode::Count` and
            // `SearchMode::CountMatches` are two spellings of one address, and
            // a node that has to speak for both can only honestly carry the
            // name they have in common. The site still supplies the *range* -
            // as wide as what is written there - and which site that is comes
            // from `Placeholder`, not from which answer arrived first.
            answers.record(
                &question.file,
                &question.file,
                &site.from_id,
                site.edge_kind,
                &node.name,
                site_range(site.position, &site.name),
                address,
                &node.id,
                None,
            );
            // The contradiction rule: an answer that lands somewhere else
            // retracts the structural edge the site said it replaces - see
            // [`LspBridge`].
            if let Some(replaced) = &site.replaces {
                answers.contradicted.insert(replaced.clone());
                answers.retract.insert(replaced.clone());
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
        Ask::Overload(site) => {
            let Some(replaced) = &site.replaces else { return Vec::new() };
            // Nothing landed, the locations disagree, or they land somewhere
            // the structural edge does not: no binding, and the structural
            // edge stands. Refining never moves a call.
            let Some((path, node, ordinals)) = overload_landing(index, roots, encoding, &found) else {
                return Vec::new();
            };
            if !refines(index, &question.file, site, replaced, node) {
                return Vec::new();
            }
            let landing = Landing {
                file: question.file.clone(),
                site: site.clone(),
                node_id: node.id.clone(),
                node_name: node.name.clone(),
                address: address_of(node, site.from_container.clone()),
            };
            answers.bind_overload(index, landing, node, &path, &ordinals)
        }
        Ask::OverloadHover { pending, at, .. } => {
            answers.hovered(*pending, at, hover_text(result));
            Vec::new()
        }
    }
}

/// Whether one answer to `site` counts towards dropping its name from the
/// caller's `untypedCalls` (GM-486, D3 as extended after the noise
/// measurement).
///
/// Only an untyped receiver call is counted: a `ReceiverCall` site that
/// replaces no structural edge, which is exactly what
/// `FileGraphBuilder::finish` folds into `untypedCalls`. Such a call counts
/// as answered when the server found something and every place it found is
/// accounted for: either this pass recorded an edge for it (`recorded`), or
/// every location lies outside the files this index holds - a std or
/// dependency method, which can never become an edge and is the noise the
/// marker was measured to carry. An empty answer is not an answer. Neither is
/// one that lands in an indexed file without becoming an edge (targets that
/// disagree, or a position no addressable declaration covers): the call may
/// well be to a project method, and the marker is what says its edge is
/// missing.
fn untyped_call_answered(
    index: &SdkIndex,
    roots: [&Path; 2],
    site: &OpenSite,
    found: &[ServerLocation],
    recorded: bool,
) -> bool {
    if site.kind != OpenSiteKind::ReceiverCall || site.replaces.is_some() || found.is_empty() {
        return false;
    }
    recorded || found.iter().all(|location| file_at(index, roots, location).is_none())
}

/// The caller nodes whose `untypedCalls` this pass shortens or restores, ready
/// to upsert (GM-486).
///
/// A name leaves a node's list once every untyped receiver-call site of that
/// name in that node was answered ([`untyped_call_answered`]); a name with one
/// site left unanswered stays. The list each node starts from is the
/// structural one the SDK index holds, so a structural reparse that re-sends
/// the full list is undone by the next pass's answers, the same way edges are.
///
/// Only `files` are considered: the files this pass both asked about and
/// finished. A file the pass did not finish says nothing about its sites, so
/// its nodes are left as core holds them, as an earlier pass's edges are. In a
/// finished file a node is re-sent when its list got shorter, or when an
/// earlier pass shortened it (`trimmed`) and this one no longer does - so a
/// name whose answers stopped arriving comes back.
///
/// The node goes out whole, as the index holds it, with only `untypedCalls`
/// changed: core's upsert replaces every column and child row from the
/// record, so anything less would erase what the structural tier sent.
fn trim_untyped_calls(
    index: &SdkIndex,
    files: &BTreeSet<RelPath>,
    answered: &HashMap<(String, String), usize>,
    trimmed: &mut BTreeMap<RelPath, BTreeSet<String>>,
) -> Vec<WireNode> {
    let mut upserts = Vec::new();
    for file in files {
        let previously = trimmed.remove(file).unwrap_or_default();
        let Some(entry) = index.entry(file) else { continue };
        let mut sites: HashMap<(&str, &str), usize> = HashMap::new();
        for site in &entry.graph.open_sites {
            if site.kind == OpenSiteKind::ReceiverCall && site.replaces.is_none() {
                *sites.entry((site.from_id.as_str(), site.name.as_str())).or_default() += 1;
            }
        }
        let mut now = BTreeSet::new();
        for node in entry.graph.nodes.iter().filter(|node| !node.untyped_calls.is_empty()) {
            let kept: Vec<String> = node
                .untyped_calls
                .iter()
                .filter(|name| {
                    let total = sites.get(&(node.id.as_str(), name.as_str())).copied().unwrap_or(0);
                    let done = answered.get(&(node.id.clone(), (*name).clone())).copied().unwrap_or(0);
                    total == 0 || done < total
                })
                .cloned()
                .collect();
            if kept.len() < node.untyped_calls.len() {
                now.insert(node.id.clone());
                upserts.push(WireNode { untyped_calls: kept, ..node.clone() });
            } else if previously.contains(&node.id) {
                upserts.push(node.clone());
            }
        }
        if !now.is_empty() {
            trimmed.insert(file.clone(), now);
        }
    }
    upserts
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
        &anchor_node.id,
        None,
    );
    true
}

impl SemanticEngine for LspBridge {
    /// A project-model change makes the running server prove its readiness
    /// again: the next pass waits for a full settle (GM-433) - see
    /// [`LspClient::unsettle`] for why, and for why an on-demand server is
    /// left as it is. No server running means nothing to do: the next one
    /// starts unsettled anyway.
    fn workspace_changed(&mut self) {
        if let Some(client) = self.client.as_mut() {
            client.unsettle();
        }
    }

    /// A whole-project pass is owed: start the server now, so it indexes
    /// while core is busy elsewhere. Through [`LspBridge::ensure_client`], so
    /// the server is registered for `kill_live_servers`, counts against
    /// `MAX_SERVER_STARTS` and a missing binary turns the tier off exactly as
    /// a pass's start would. A running server is left as it is, settled or
    /// not. Readiness stays the pass's question: what the server sends
    /// meanwhile queues on the client's channel and is read by `wait_ready`.
    fn prepare(&mut self) {
        let deadline = Instant::now() + self.budgets.single_file;
        self.ensure_client(deadline);
    }

    fn set_pass_deadline(&mut self, deadline: Option<Instant>) {
        self.core_deadline = deadline;
    }

    fn answer(&mut self, files: &[RelPath], index: &SdkIndex) -> Result<SemanticAnswer> {
        let whole_project = files.is_empty();
        // What this pass was sent - core adds the files earlier passes did not
        // finish itself. A file no longer in the index was deleted or
        // failed to extract: there is nothing left to ask about it, so it is
        // not unfinished either.
        let scope: Vec<RelPath> = if whole_project {
            index.paths()
        } else {
            let scope: BTreeSet<RelPath> =
                files.iter().filter(|path| index.entry(path).is_some()).cloned().collect();
            scope.into_iter().collect()
        };
        if scope.is_empty() {
            return Ok(SemanticAnswer::complete(FileChangeDiff::default()).with_unfinished(BTreeSet::new()));
        }

        let started = Instant::now();
        let deadline = self.pass_deadline(started, whole_project, scope.len());
        let plan = questions(index, &scope, &self.config, &self.budgets);
        if plan.unanswerable > 0 {
            crate::log_line!(
                "[{}] {} open site(s) of a kind this bridge does not answer were skipped",
                self.language,
                plan.unanswerable
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
                )
                .with_unfinished(scope.into_iter().collect()));
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
            let (mut diff, _) = answers.finish();
            // No question means no untyped receiver call in these files, so
            // this only forgets what an earlier pass trimmed in them.
            diff.upsert_nodes.extend(trim_untyped_calls(index, &covered, &HashMap::new(), &mut self.trimmed));
            // Nothing left to ask is nothing unfinished.
            return Ok(SemanticAnswer::complete(diff).with_unfinished(BTreeSet::new()));
        }

        let language = self.language.clone();
        let language_ids = self.language_ids;
        let engine = self.config.engine.clone();
        let budgets = self.budgets;
        let disambiguation = self.config.overload_disambiguation;
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
                    Self::sync_documents(
                        client,
                        &mut opened,
                        &root,
                        index,
                        &asked_about,
                        &language,
                        language_ids,
                    );
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
                            disambiguation,
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
                // Not one file was finished: every one asked about is
                // unfinished. A file in scope with no question has nothing to
                // finish, and the ceiling did not cut any (`plan.truncated`
                // aside, which this answer is incomplete for anyway).
                return Ok(SemanticAnswer::incomplete_because(FileChangeDiff::default(), reason)
                    .with_unfinished(asked_about));
            }
        };

        // The files this pass asked about and finished. A file reached only by
        // an implementation sweep's second hop is covered without its own
        // sites having been asked, so it is not one of them.
        let finished: BTreeSet<RelPath> = covered.intersection(&asked_about).cloned().collect();
        // Restores structural edges and drops the semantic edges they cover,
        // before `by_file` is read: a dropped edge an earlier pass emitted is
        // then retracted below.
        answers.settle(index, &finished);
        let produced = answers.by_file.clone();
        retract_stale(&mut answers, &self.emitted, &covered, &produced);
        let untyped_answered = std::mem::take(&mut answers.untyped_answered);
        let (mut diff, emitted) = answers.finish();
        // Callers whose untyped receiver calls were all answered (GM-486), in
        // the files this pass finished.
        diff.upsert_nodes.extend(trim_untyped_calls(index, &finished, &untyped_answered, &mut self.trimmed));
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

        crate::log_line!(
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
        // What core is to ask again: every file asked about and not finished,
        // and - when the ceiling cut the list - every file in scope it left
        // unasked, since a file with no question is settled only when the
        // ceiling is not why it had none.
        let mut unfinished: BTreeSet<RelPath> = asked_about.difference(&finished).cloned().collect();
        if plan.truncated {
            unfinished.extend(scope.iter().filter(|file| !asked_about.contains(*file)).cloned());
        }
        Ok(SemanticAnswer {
            diff,
            complete: failure.is_none(),
            reason: failure,
            unfinished: Some(unfinished),
        })
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
                key_path: None,
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
        assert_eq!(target.key_path, None, "no path on the declaration, none on the key");

        let path = g_mesh_wire::QualifiedPath::root("greet").child("::", "Greeter");
        node.qualified_path = Some(path.clone());
        let target = address_of(&node, None);
        assert_eq!(target.key_path, Some(path));
        assert_eq!(target.check_key_path(), Ok(()));
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
                    &declaration.id,
                    None,
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

    /// With core's deadline, a pass plans to three quarters of the time left
    /// until it, whatever its shape: a many-file per-file-shaped pass under
    /// 120s plans 90s, not 8s a file. A deadline already past plans nothing.
    ///
    /// Control: ignore `core_deadline` in `pass_deadline`.
    #[test]
    fn with_cores_deadline_a_pass_plans_three_quarters_of_the_time_left() {
        let mut bridge = LspBridge::new("toy", Path::new("/p"), SemanticConfig::new("toy-server"));
        let started = Instant::now();

        bridge.set_pass_deadline(Some(started + Duration::from_secs(120)));
        assert_eq!(bridge.pass_deadline(started, false, 1_000), started + Duration::from_secs(90));
        assert_eq!(bridge.pass_deadline(started, true, 1_000), started + Duration::from_secs(90));

        bridge.set_pass_deadline(Some(started + Duration::from_secs(20 * 60)));
        assert_eq!(bridge.pass_deadline(started, true, 10), started + Duration::from_secs(15 * 60));

        let later = started + Duration::from_secs(10);
        bridge.set_pass_deadline(Some(started));
        assert_eq!(bridge.pass_deadline(later, true, 10), later, "a past deadline leaves no time to plan");
    }

    /// Without core's deadline - never sent, or cleared by a pass without
    /// one - the bridge's own budgets apply.
    #[test]
    fn without_cores_deadline_a_pass_plans_its_own_budget() {
        let mut bridge = LspBridge::new("toy", Path::new("/p"), SemanticConfig::new("toy-server"));
        let started = Instant::now();
        for (whole_project, files) in [(false, 1_000), (true, 10), (true, 1_000)] {
            assert_eq!(
                bridge.pass_deadline(started, whole_project, files),
                started + bridge.pass_budget(whole_project, files)
            );
        }

        bridge.set_pass_deadline(Some(started + Duration::from_secs(120)));
        bridge.set_pass_deadline(None);
        assert_eq!(bridge.pass_deadline(started, true, 1_000), started + Duration::from_secs(8_000));
    }

    // --- GM-486: untyped receiver calls the semantic tier answered ---------

    /// `a.toy` declares `add` and `sub`; `b.toy`'s `caller` calls `len`
    /// twice and `add` once through untyped receivers, `typed` through a
    /// receiver with a structural edge (`replaces`), and holds a plain
    /// reference `bare`. `caller` also carries every column a structural
    /// node can, so a re-sent copy that lost one is visible.
    fn untyped_index() -> (SdkIndex, String) {
        let range = |start: (u32, u32), end: (u32, u32)| Range {
            start: Position { line: start.0, col: start.1 },
            end: Position { line: end.0, col: end.1 },
        };

        let a = RelPath::new("a.toy");
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &a);
        builder.file_node(range((0, 0), (2, 0)));
        for (line, name) in [(0, "add"), (1, "sub")] {
            builder.add_node(
                NodeSpec::new(NodeKind::Function, name, name, range((line, 3), (line, 6)))
                    .native_kind("method")
                    .public(),
            );
        }
        let mut index = SdkIndex::new();
        index.insert(a, "fn add\nfn sub\n".to_string(), builder.finish());

        let b = RelPath::new("b.toy");
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &b);
        builder.record_untyped_receiver_calls();
        builder.file_node(range((0, 0), (6, 0)));
        let caller = builder.add_node(
            NodeSpec::new(NodeKind::Function, "caller", "m::caller", range((0, 0), (5, 1)))
                .native_kind("function")
                .in_container("m", None)
                .public(),
        );
        for (line, name, kind, replaces) in [
            (1, "len", OpenSiteKind::ReceiverCall, None),
            (2, "len", OpenSiteKind::ReceiverCall, None),
            (3, "add", OpenSiteKind::ReceiverCall, None),
            (4, "typed", OpenSiteKind::ReceiverCall, Some("e-typed".to_string())),
            (4, "bare", OpenSiteKind::Reference, None),
            // GM-497: an untyped field read is not a receiver call.
            (5, "fld", OpenSiteKind::ReceiverField, None),
        ] {
            builder.open_site(OpenSite {
                from_id: caller.clone(),
                position: Position { line, col: 4 },
                name: name.to_string(),
                kind,
                edge_kind: if kind == OpenSiteKind::ReceiverField {
                    EdgeKind::References
                } else {
                    EdgeKind::Calls
                },
                from_container: Some("m".to_string()),
                replaces,
            });
        }
        let mut graph = builder.finish();
        let node = graph.nodes.iter_mut().find(|node| node.id == caller).unwrap();
        // `fld` (a `ReceiverField`) is not folded in: control, count
        // `ReceiverField` in `fold_untyped_calls`.
        assert_eq!(node.untyped_calls, ["add", "len"], "the structural list the fixture starts from");
        node.signature = Some("fn caller()".to_string());
        node.doc_comment = Some("Calls things.".to_string());
        node.declarations = Some(vec![g_mesh_wire::WireDeclaration {
            ordinal: 0,
            start_line: 0,
            start_col: 0,
            end_line: 5,
            end_col: 1,
            signature: Some("fn caller()".to_string()),
            has_body: true,
        }]);
        let path = |first: &str| {
            g_mesh_wire::QualifiedPath(vec![
                g_mesh_wire::PathSegment { sep: None, name: first.to_string() },
                g_mesh_wire::PathSegment { sep: Some("::".to_string()), name: "caller".to_string() },
            ])
        };
        node.qualified_path = Some(path("m"));
        node.alias_paths = vec![path("alias")];
        node.target = Some(PlaceholderTarget {
            scope: TargetScope::Container("m".to_string()),
            key: TargetKey::QualifiedName("m::caller".to_string()),
            from_container: Some("m".to_string()),
            key_path: None,
        });
        index.insert(
            b,
            "fn caller() {\n  x.len();\n  y.len();\n  z.add();\n  t.typed();\n}\n".to_string(),
            graph,
        );
        (index, caller)
    }

    /// The open site of `caller` called `name` at `line`.
    fn site_of(index: &SdkIndex, name: &str, line: u32) -> OpenSite {
        let entry = index.entry(&RelPath::new("b.toy")).unwrap();
        entry
            .graph
            .open_sites
            .iter()
            .find(|site| site.name == name && site.position.line == line)
            .unwrap()
            .clone()
    }

    /// How many sites of `(caller, name)` one answer `found` counted as
    /// answered, and how many edges it recorded.
    fn answered_by(index: &SdkIndex, caller: &str, site: OpenSite, found: Value) -> (usize, usize) {
        let question = Question {
            file: RelPath::new("b.toy"),
            position: site.position,
            ask: Ask::Definition(site.clone()),
        };
        let mut answers = Answers::new("toy", "toy-lsp");
        let root = Path::new("/p");
        let _ = record_answer(&mut answers, index, [root, root], PositionEncoding::Utf16, &question, &found);
        let counted =
            answers.untyped_answered.get(&(caller.to_string(), site.name.clone())).copied().unwrap_or(0);
        let (diff, _) = answers.finish();
        (counted, diff.upsert_edges.len())
    }

    /// **GM-486, the counting rule.** An untyped site counts as answered when
    /// the answer is non-empty and either recorded an edge or lies wholly
    /// outside the index. An empty or null answer does not count, and neither
    /// does one that lands in an indexed file without an edge.
    ///
    /// Controls, each in `untyped_call_answered` / `record_answer`: drop the
    /// `found.is_empty()` check (the empty and null cases count); drop the
    /// `|| found.iter().all(..)` arm (the std case does not count); pass
    /// `false` for `recorded` (the edge case does not count); pass
    /// `target.is_some()` instead of `agree && target.is_some()` (the
    /// disagreeing case counts); test `node_at(..).is_none()` instead of
    /// `file_at(..).is_none()` (the no-node case counts); drop the
    /// `site.replaces.is_some()` or `site.kind != ReceiverCall` check (a
    /// typed call, a reference or a field read counts).
    #[test]
    fn an_untyped_call_is_answered_only_by_a_non_empty_answer_that_is_an_edge_or_outside_the_index() {
        let (index, caller) = untyped_index();
        let len = || site_of(&index, "len", 1);
        let outside = json!({ "uri": "file:///rustlib/core/src/slice.rs", "range": { "start": { "line": 9, "character": 4 } } });
        let at_add = json!({ "uri": "file:///p/a.toy", "range": { "start": { "line": 0, "character": 3 } } });
        let at_sub = json!({ "uri": "file:///p/a.toy", "range": { "start": { "line": 1, "character": 3 } } });
        // Inside `a.toy`, before `add`: only the file node covers it.
        let at_no_node =
            json!({ "uri": "file:///p/a.toy", "range": { "start": { "line": 0, "character": 0 } } });

        assert_eq!(answered_by(&index, &caller, len(), Value::Null), (0, 0), "a null answer is no answer");
        assert_eq!(answered_by(&index, &caller, len(), json!([])), (0, 0), "an empty answer is no answer");
        assert_eq!(answered_by(&index, &caller, len(), outside.clone()), (1, 0), "std, outside the index");
        assert_eq!(
            answered_by(&index, &caller, len(), json!([outside.clone(), outside.clone()])),
            (1, 0),
            "every location outside the index"
        );
        assert_eq!(
            answered_by(&index, &caller, len(), at_add.clone()),
            (1, 1),
            "an answer that became an edge"
        );
        assert_eq!(
            answered_by(&index, &caller, len(), at_no_node.clone()),
            (0, 0),
            "an indexed file with no node there may be a missed project call"
        );
        assert_eq!(
            answered_by(&index, &caller, len(), json!([at_add.clone(), at_sub])),
            (0, 0),
            "targets that disagree record no edge, so the name stays"
        );
        assert_eq!(
            answered_by(&index, &caller, len(), json!([outside.clone(), at_no_node])),
            (0, 0),
            "one location in the index without an edge is enough to keep the name"
        );
        assert_eq!(
            answered_by(&index, &caller, site_of(&index, "typed", 4), outside.clone()),
            (0, 0),
            "a call with a structural edge is not an untyped call"
        );
        assert_eq!(
            answered_by(&index, &caller, site_of(&index, "bare", 4), outside.clone()),
            (0, 0),
            "a reference is not a receiver call"
        );
        assert_eq!(
            answered_by(&index, &caller, site_of(&index, "fld", 5), outside),
            (0, 0),
            "a field read is not a receiver call (GM-497)"
        );
    }

    fn answered(counts: &[(&str, &str, usize)]) -> HashMap<(String, String), usize> {
        counts.iter().map(|(from, name, n)| ((from.to_string(), name.to_string()), *n)).collect()
    }

    fn b_toy() -> BTreeSet<RelPath> {
        BTreeSet::from([RelPath::new("b.toy")])
    }

    /// **GM-486.** A name leaves the list only when every untyped site of it
    /// in that caller was answered: one of two `len` sites is not enough.
    /// Only the listed files are considered.
    ///
    /// Control: in `trim_untyped_calls`, keep a name only when `done == 0`
    /// instead of `done < total` (`len` drops after one of its two sites).
    #[test]
    fn a_name_drops_only_when_every_one_of_its_sites_in_the_caller_was_answered() {
        let (index, caller) = untyped_index();
        let trim = |counts: &[(&str, &str, usize)], files: &BTreeSet<RelPath>| {
            trim_untyped_calls(&index, files, &answered(counts), &mut BTreeMap::new())
        };

        let one_len = trim(&[(&caller, "len", 1), (&caller, "add", 1)], &b_toy());
        assert_eq!(one_len.len(), 1, "{one_len:#?}");
        assert_eq!(one_len[0].id, caller);
        assert_eq!(one_len[0].untyped_calls, ["len"], "`add` is answered, one `len` site is still open");

        let both_len = trim(&[(&caller, "len", 2), (&caller, "add", 1)], &b_toy());
        assert_eq!(both_len[0].untyped_calls, Vec::<String>::new(), "every site answered");

        assert!(trim(&[(&caller, "len", 1)], &b_toy()).is_empty(), "nothing shorter, nothing re-sent");
        assert!(
            trim(&[(&caller, "add", 1)], &BTreeSet::from([RelPath::new("a.toy")])).is_empty(),
            "a file not listed is left as core holds it"
        );
    }

    /// **GM-486.** The re-sent caller is the structural node whole, with only
    /// `untypedCalls` shorter: core's upsert replaces every column and child
    /// row from the record, so anything less would erase what the structural
    /// tier sent (declarations, qualified and alias paths, target, doc,
    /// signature, container).
    ///
    /// Control: in `trim_untyped_calls`, build the upsert as
    /// `WireNode { untyped_calls: kept, declarations: None, alias_paths:
    /// Vec::new(), ..node.clone() }`.
    #[test]
    fn a_re_sent_caller_keeps_every_other_column() {
        let (index, caller) = untyped_index();
        let original = index
            .entry(&RelPath::new("b.toy"))
            .unwrap()
            .graph
            .nodes
            .iter()
            .find(|n| n.id == caller)
            .unwrap()
            .clone();

        let upserts =
            trim_untyped_calls(&index, &b_toy(), &answered(&[(&caller, "add", 1)]), &mut BTreeMap::new());
        assert_eq!(upserts, vec![WireNode { untyped_calls: vec!["len".to_string()], ..original }]);
    }

    /// **GM-486.** Every pass starts from the structural list the index
    /// holds: the same answers trim the same node again on the next pass
    /// (undoing a structural reparse that re-sent the full list in between),
    /// a name comes back once its answers stop, and a node that is neither
    /// trimmed nor was trimmed is not re-sent.
    ///
    /// Controls, in `trim_untyped_calls`: drop the `else if
    /// previously.contains(..)` branch (pass 3 re-sends nothing, so `add`
    /// never comes back); re-send a shortened node only when
    /// `!previously.contains(&node.id)` (pass 2 re-sends the full list).
    #[test]
    fn each_pass_trims_from_the_structural_list_and_a_name_comes_back_when_its_answers_stop() {
        let (index, caller) = untyped_index();
        let mut trimmed = BTreeMap::new();
        let mut pass = |counts: &[(&str, &str, usize)]| {
            let upserts = trim_untyped_calls(&index, &b_toy(), &answered(counts), &mut trimmed);
            upserts.into_iter().map(|node| (node.id, node.untyped_calls)).collect::<Vec<_>>()
        };
        let sent =
            |names: &[&str]| vec![(caller.clone(), names.iter().map(|n| n.to_string()).collect::<Vec<_>>())];

        assert_eq!(pass(&[(&caller, "add", 1)]), sent(&["len"]), "pass 1 trims `add`");
        assert_eq!(pass(&[(&caller, "add", 1)]), sent(&["len"]), "pass 2 trims it again, from the full list");
        assert_eq!(pass(&[]), sent(&["add", "len"]), "pass 3: no answer, the full list goes back");
        assert_eq!(pass(&[]), Vec::new(), "pass 4: nothing trimmed now or before, nothing re-sent");
    }

    // --- which open sites a pass asks about ---------------------------------

    fn line_range(line: u32) -> Range {
        Range { start: Position { line, col: 0 }, end: Position { line, col: 9 } }
    }

    /// What `m.toy`, the file a hop's placeholder names, declares under the
    /// name `n`.
    #[derive(Clone, Copy, Debug)]
    enum Declares {
        Nothing,
        Function,
        Placeholder,
    }

    fn declaring_file(index: &mut SdkIndex, declares: Declares) {
        let m = RelPath::new("m.toy");
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &m);
        builder.file_node(line_range(0));
        match declares {
            Declares::Nothing => {}
            Declares::Function => {
                builder.add_node(
                    NodeSpec::new(NodeKind::Function, "n", "n", line_range(0))
                        .native_kind("function")
                        .public(),
                );
            }
            Declares::Placeholder => {
                builder.add_placeholder(
                    PlaceholderKind::PendingSymbol,
                    "n",
                    file_name("o.toy", "n"),
                    line_range(0),
                );
            }
        }
        index.insert(m, "fn n\n".to_string(), builder.finish());
    }

    /// `o.toy`, declaring an overload set named `n`: a node with
    /// `declarations`, which is what makes an `OverloadCall` onto `n` worth
    /// asking.
    fn overload_file(index: &mut SdkIndex) {
        let o = RelPath::new("o.toy");
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &o);
        builder.file_node(line_range(0));
        let declaration = |ordinal| WireDeclaration {
            ordinal,
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 9,
            signature: None,
            has_body: false,
        };
        builder.add_node(
            NodeSpec::new(NodeKind::Function, "n", "n", line_range(0))
                .native_kind("function")
                .public()
                .declarations(vec![declaration(0), declaration(1)]),
        );
        index.insert(o, "fn n\n".to_string(), builder.finish());
    }

    fn file_name(file: &str, name: &str) -> PlaceholderTarget {
        PlaceholderTarget {
            scope: TargetScope::File(file.to_string()),
            key: TargetKey::Name(name.to_string()),
            from_container: None,
            key_path: None,
        }
    }

    /// What an open site of `u.toy` names in `replaces`.
    #[derive(Clone, Copy, Debug)]
    enum Replaces {
        TheEdge,
        AMissingEdge,
        Nothing,
    }

    /// `u.toy`: `f` with a `CALLS` edge onto a `kind` placeholder addressed by
    /// `target`, and one open site per `(kind, replaces, line)`, all at
    /// column 2 of `line`.
    fn using_file(
        index: &mut SdkIndex,
        kind: PlaceholderKind,
        target: PlaceholderTarget,
        sites: &[(OpenSiteKind, Replaces, u32)],
    ) {
        let u = RelPath::new("u.toy");
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &u);
        builder.file_node(Range { start: Position { line: 0, col: 0 }, end: Position { line: 4, col: 0 } });
        let f = builder.add_node(
            NodeSpec::new(NodeKind::Function, "f", "f", line_range(0)).native_kind("function").public(),
        );
        let placeholder = builder.add_placeholder(kind, "n", target, line_range(1));
        let edge = builder.placeholder_edge(EdgeKind::Calls, &f, &placeholder);
        for (site_kind, replaces, line) in sites {
            builder.open_site(OpenSite {
                from_id: f.clone(),
                position: Position { line: *line, col: 2 },
                name: "n".to_string(),
                kind: *site_kind,
                edge_kind: EdgeKind::Calls,
                from_container: None,
                replaces: match replaces {
                    Replaces::TheEdge => Some(edge.clone()),
                    Replaces::AMissingEdge => Some("e-missing".to_string()),
                    Replaces::Nothing => None,
                },
            });
        }
        index.insert(u, "fn f\n  n()\n  n()\n  n()\n".to_string(), builder.finish());
    }

    /// What one pass over `u.toy` asks: each question's kind (`OverloadCall`
    /// for an overload question) and line, sorted, and the unanswerable count.
    fn asked_about_u(index: &SdkIndex) -> (Vec<(OpenSiteKind, u32)>, usize) {
        let questions = questions(
            index,
            &[RelPath::new("u.toy")],
            &SemanticConfig::new("toy-server"),
            &Budgets::default(),
        );
        let mut asked: Vec<(OpenSiteKind, u32)> = questions
            .asking
            .iter()
            .map(|question| match &question.ask {
                Ask::Definition(site) => (site.kind, site.position.line),
                Ask::Overload(site) => (OpenSiteKind::OverloadCall, site.position.line),
                other => panic!("only open sites are asked here: {other:?}"),
            })
            .collect();
        asked.sort_by_key(|(kind, line)| (format!("{kind:?}"), *line));
        (asked, questions.unanswerable)
    }

    /// A hop - a `Reference` site whose `replaces` names an edge onto a
    /// `pending_symbol` placeholder addressed by file and bare name - is asked
    /// only while that file is indexed and declares nothing addressable of
    /// that name. Every other hop is left to its structural edge, and none of
    /// them counts as unanswerable.
    #[test]
    fn a_hop_is_asked_only_while_the_linker_cannot_settle_its_placeholder() {
        use PlaceholderKind::{PendingSymbol, Reexport};
        let qualified = PlaceholderTarget {
            scope: TargetScope::File("m.toy".to_string()),
            key: TargetKey::QualifiedName("n".to_string()),
            from_container: None,
            key_path: None,
        };
        let container = PlaceholderTarget {
            scope: TargetScope::Container("m".to_string()),
            key: TargetKey::Name("n".to_string()),
            from_container: None,
            key_path: None,
        };
        let cases = [
            (
                "m.toy declares no n",
                Declares::Nothing,
                PendingSymbol,
                file_name("m.toy", "n"),
                Replaces::TheEdge,
                1,
            ),
            (
                "m.toy's n is only a placeholder",
                Declares::Placeholder,
                PendingSymbol,
                file_name("m.toy", "n"),
                Replaces::TheEdge,
                1,
            ),
            (
                "m.toy declares n",
                Declares::Function,
                PendingSymbol,
                file_name("m.toy", "n"),
                Replaces::TheEdge,
                0,
            ),
            (
                "the file is not indexed",
                Declares::Nothing,
                PendingSymbol,
                file_name("gone.toy", "n"),
                Replaces::TheEdge,
                0,
            ),
            ("a container scope", Declares::Nothing, PendingSymbol, container, Replaces::TheEdge, 0),
            ("a qualified-name key", Declares::Nothing, PendingSymbol, qualified, Replaces::TheEdge, 0),
            (
                "a reexport placeholder",
                Declares::Nothing,
                Reexport,
                file_name("m.toy", "n"),
                Replaces::TheEdge,
                0,
            ),
            (
                "the edge is missing",
                Declares::Nothing,
                PendingSymbol,
                file_name("m.toy", "n"),
                Replaces::AMissingEdge,
                0,
            ),
        ];
        for (case, declares, kind, target, replaces, expected) in cases {
            let mut index = SdkIndex::new();
            declaring_file(&mut index, declares);
            using_file(&mut index, kind, target, &[(OpenSiteKind::Reference, replaces, 1)]);
            let (asked, unanswerable) = asked_about_u(&index);
            let want: Vec<(OpenSiteKind, u32)> =
                (0..expected).map(|_| (OpenSiteKind::Reference, 1)).collect();
            assert_eq!(asked, want, "{case}");
            assert_eq!(unanswerable, 0, "a hop not asked is not unanswerable: {case}");
        }
    }

    /// A `Reference` site with no `replaces`, a `ReceiverCall` and a
    /// `ReceiverField` (GM-497) are asked whatever the placeholder looks
    /// like: the hop rule is about a `Reference`'s `replaces`. Control for
    /// the `ReceiverField` row: route `ReceiverField` to `continue` in
    /// `questions`.
    #[test]
    fn a_reference_without_replaces_and_a_receiver_call_are_asked_as_before() {
        let mut index = SdkIndex::new();
        declaring_file(&mut index, Declares::Function);
        using_file(
            &mut index,
            PlaceholderKind::PendingSymbol,
            file_name("m.toy", "n"),
            &[
                (OpenSiteKind::Reference, Replaces::Nothing, 1),
                (OpenSiteKind::ReceiverCall, Replaces::Nothing, 2),
                (OpenSiteKind::ReceiverCall, Replaces::TheEdge, 3),
                (OpenSiteKind::ReceiverField, Replaces::TheEdge, 4),
            ],
        );
        let (asked, unanswerable) = asked_about_u(&index);
        assert_eq!(
            asked,
            vec![
                (OpenSiteKind::ReceiverCall, 2),
                (OpenSiteKind::ReceiverCall, 3),
                (OpenSiteKind::ReceiverField, 4),
                (OpenSiteKind::Reference, 1)
            ]
        );
        assert_eq!(unanswerable, 0);
    }

    /// **GM-497, items 10, 11 and 16.** Onto the placeholder a typed Rust field
    /// read replaces - a `Container` scope with a `QualifiedName` key - a
    /// `ReceiverField` site is asked with or without `replaces`, while a
    /// `Reference` hop with the same `replaces` is still left to its
    /// structural edge. Controls: route `ReceiverField` to `continue` in
    /// `questions` (lines 2 and 3 are not asked); give `ReceiverField` the
    /// hop arm (`ReceiverField if site.replaces.is_some()` beside
    /// `Reference`: line 2 is not asked); route a `Reference` with `replaces`
    /// to `Ask::Definition` unconditionally (line 1 is asked).
    #[test]
    fn a_receiver_field_is_asked_onto_the_placeholder_a_hop_is_not() {
        let mut index = SdkIndex::new();
        declaring_file(&mut index, Declares::Nothing);
        let field = PlaceholderTarget {
            scope: TargetScope::Container("m".to_string()),
            key: TargetKey::QualifiedName("T.n".to_string()),
            from_container: None,
            key_path: None,
        };
        using_file(
            &mut index,
            PlaceholderKind::PendingSymbol,
            field,
            &[
                (OpenSiteKind::Reference, Replaces::TheEdge, 1),
                (OpenSiteKind::ReceiverField, Replaces::TheEdge, 2),
                (OpenSiteKind::ReceiverField, Replaces::Nothing, 3),
            ],
        );
        let (asked, unanswerable) = asked_about_u(&index);
        assert_eq!(asked, vec![(OpenSiteKind::ReceiverField, 2), (OpenSiteKind::ReceiverField, 3)]);
        assert_eq!(unanswerable, 0);
    }

    /// A hop at the same call (enclosing node and position) as an overload
    /// question that was kept is dropped, whichever of the two sites was
    /// recorded first; a hop elsewhere stays.
    #[test]
    fn a_hop_at_a_kept_overload_call_is_dropped_in_either_order() {
        let hop = (OpenSiteKind::Reference, Replaces::TheEdge, 1);
        let overload = (OpenSiteKind::OverloadCall, Replaces::TheEdge, 1);
        let elsewhere = (OpenSiteKind::OverloadCall, Replaces::TheEdge, 2);
        for sites in [[hop, overload, elsewhere], [overload, hop, elsewhere], [elsewhere, overload, hop]] {
            let mut index = SdkIndex::new();
            declaring_file(&mut index, Declares::Nothing);
            overload_file(&mut index);
            using_file(&mut index, PlaceholderKind::PendingSymbol, file_name("m.toy", "n"), &sites);
            let (asked, unanswerable) = asked_about_u(&index);
            assert_eq!(
                asked,
                vec![(OpenSiteKind::OverloadCall, 1), (OpenSiteKind::OverloadCall, 2)],
                "{sites:?}"
            );
            assert_eq!(unanswerable, 0);
        }

        let mut index = SdkIndex::new();
        declaring_file(&mut index, Declares::Nothing);
        overload_file(&mut index);
        using_file(
            &mut index,
            PlaceholderKind::PendingSymbol,
            file_name("m.toy", "n"),
            &[(OpenSiteKind::OverloadCall, Replaces::TheEdge, 2), hop],
        );
        let (asked, _) = asked_about_u(&index);
        assert_eq!(
            asked,
            vec![(OpenSiteKind::OverloadCall, 2), (OpenSiteKind::Reference, 1)],
            "a hop at another call"
        );
    }

    /// When no overload set answers to the call's name, its `OverloadCall`
    /// site is filtered out and the hop at the same call is asked instead.
    #[test]
    fn a_hop_at_a_filtered_overload_call_is_asked() {
        for sites in [
            [
                (OpenSiteKind::Reference, Replaces::TheEdge, 1),
                (OpenSiteKind::OverloadCall, Replaces::TheEdge, 1),
            ],
            [
                (OpenSiteKind::OverloadCall, Replaces::TheEdge, 1),
                (OpenSiteKind::Reference, Replaces::TheEdge, 1),
            ],
        ] {
            let mut index = SdkIndex::new();
            declaring_file(&mut index, Declares::Nothing);
            using_file(&mut index, PlaceholderKind::PendingSymbol, file_name("m.toy", "n"), &sites);
            let (asked, unanswerable) = asked_about_u(&index);
            assert_eq!(asked, vec![(OpenSiteKind::Reference, 1)], "{sites:?}");
            assert_eq!(unanswerable, 0);
        }
    }

    /// `didOpen`'s `languageId` is the first pair whose extension the path
    /// ends with, else the bridge's language.
    #[test]
    fn a_document_opens_under_the_first_matching_language_id() {
        let ids = [(".tsx", "typescriptreact"), (".x.tsx", "never-reached"), (".js", "javascript")];
        let id = |path: &str| language_id(&RelPath::new(path), "typescript", &ids);
        assert_eq!(id("src/a.tsx"), "typescriptreact");
        assert_eq!(id("src/a.x.tsx"), "typescriptreact", "the first pair wins");
        assert_eq!(id("src/a.js"), "javascript");
        assert_eq!(id("src/a.ts"), "typescript");
        assert_eq!(language_id(&RelPath::new("src/a.tsx"), "typescript", &[]), "typescript");
    }
}
