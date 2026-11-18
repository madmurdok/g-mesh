//! The engine itself: open sites in, a semantic diff out, with a language
//! server in the middle and a budget around the whole thing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use super::client::{LspClient, Poll};
use super::config::SemanticConfig;
use super::position::{file_uri, without_verbatim_prefix};
use super::profile::ServerProfile;
use g_mesh_plugin_sdk::RelPath;
use g_mesh_plugin_sdk::SdkIndex;

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
///   plugin sets ([`ServerProfile::warm_up`], through
///   [`LspBridge::with_profile`]), because how long a server takes to load a
///   project is a fact about that server.
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

/// A [`SemanticEngine`](g_mesh_plugin_sdk::SemanticEngine) that answers a structural
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
/// **A server that says when it has finished loading is asked to** (GM-550).
/// rust-analyzer 1.97 under load paused 2.04s and 2.47s between `Fetching`
/// and `Building CrateGraph` - longer than the settle - and the bridge called
/// it ready inside the gap. [`ServerProfile::quiescent_status`] advertises
/// rust-analyzer's `experimental.serverStatusNotification` capability; the
/// server then reports `quiescent: false` from its first progress until the
/// last start-up phase (cache priming included) has ended, and `true` within
/// milliseconds of that. The client counts `quiescent: false` as work in
/// flight, so no gap between phases reads as quiet, and the first settle
/// needs only "quiescent and nothing in flight" - see [`LspClient::settle`]
/// for the two cases that keep the quiet period (a server that sends no
/// status, and the settle after a `workspaceChanged`) and for the bound on a
/// server that never reports `true`.
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
    /// What the plugin's code declares about this server - see
    /// [`LspBridge::with_profile`]. Its `warm_up` is applied to `budgets`
    /// when set, and read from there.
    profile: ServerProfile,
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
    /// keyed by their file - see [`pass::trim_untyped_calls`]. Only what lets a later
    /// pass put a name back when its answers stop arriving.
    trimmed: BTreeMap<RelPath, BTreeSet<String>>,
    /// Core's deadline for the pass about to be answered, when core sent one
    /// (`budgetMs`, GM-521) - see [`LspBridge::pass_deadline`].
    core_deadline: Option<Instant>,
    /// Files passes were sent whose change on disk the server has not been
    /// told about yet: a pass that starts no server (nothing to ask) leaves
    /// them for the next one that does. Only filled when
    /// [`ServerProfile::client_watches_files`] is set.
    unforwarded: BTreeSet<RelPath>,
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

    /// Applies `profile`: its `warm_up`, when set, replaces
    /// [`Budgets::warm_up`]; its other fields are read where the bridge
    /// starts the server and syncs documents.
    #[must_use]
    pub fn with_profile(mut self, profile: ServerProfile) -> Self {
        if profile.warm_up.is_some() {
            self.budgets.warm_up = profile.warm_up;
        }
        self.profile = profile;
        self
    }

    /// The profile this bridge was given, [`ServerProfile::DEFAULT`] if none.
    pub fn profile(&self) -> &ServerProfile {
        &self.profile
    }

    /// Sends `method` with `params` to the running server as a request whose
    /// answer nobody waits for: a pass's poll loop drops an answer to an id
    /// it did not ask. For a plugin that must tell its server something the
    /// bridge has no verb for, such as rust-analyzer's
    /// `rust-analyzer/reloadWorkspace`. Starts nothing: with no server
    /// running it sends nothing and answers `Ok(false)`, since a server
    /// started later reads the project fresh anyway.
    pub fn request(&mut self, method: &str, params: Value) -> Result<bool> {
        match self.client.as_mut() {
            Some(client) => client.request(method, params).map(|_| true),
            None => Ok(false),
        }
    }

    /// [`LspBridge::new`] with budgets a test can make small - see
    /// [`Budgets`].
    pub fn with_budgets(language: &str, root: &Path, config: SemanticConfig, budgets: Budgets) -> Self {
        let real_root = std::fs::canonicalize(root)
            .map(|resolved| without_verbatim_prefix(&resolved))
            .unwrap_or_else(|_| root.to_path_buf());
        Self {
            language: language.to_string(),
            profile: ServerProfile::DEFAULT,
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
            unforwarded: BTreeSet::new(),
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
            g_mesh_plugin_sdk::log_line!(
                "[{}] the language server is gone - starting a new one",
                self.language
            );
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
            match LspClient::start(
                &self.language,
                &self.config,
                &self.root,
                deadline,
                self.profile.quiescent_status,
                self.profile.client_watches_files,
            ) {
                Ok(client) => self.client = Some(client),
                Err(err) => {
                    self.start_failure = Some(format!(
                        "the language server {} could not be started: {err:#}",
                        self.config.command.display()
                    ));
                    if missing_binary(&err) {
                        self.unavailable = true;
                        g_mesh_plugin_sdk::log_line!(
                            "[{}] the language server {} could not be started ({err:#}) - this \
                             language's semantic tier is off for the rest of this process's life; \
                             the structural graph is unaffected",
                            self.language,
                            self.config.command.display()
                        );
                    } else {
                        g_mesh_plugin_sdk::log_line!(
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
    /// Plus, when [`ServerProfile::client_watches_files`] is set, every indexed
    /// file a pass was sent (an edited one), asked about or not - see
    /// [`LspBridge::forward_watched`] (GM-624). Those stay open too, so what
    /// `memoryLimitMb` is charged for grows with the files edited during the
    /// session, not only with the files asked about.
    ///
    /// Nothing is closed again, except a file deleted from disk - see
    /// [`LspBridge::forward_watched`]. A `didClose` after each pass would free
    /// that text and make the next pass re-send every byte of it, and the
    /// per-file passes that follow every edit are exactly where that would be
    /// paid over and over. It would also undo the GM-624 fix: a closed file
    /// goes back to the server reading it from disk, asynchronously.
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

    /// Tells the server about every file in `pending` - see
    /// [`ServerProfile::client_watches_files`] - and returns the ones to hand to
    /// [`LspBridge::sync_documents`] instead of announcing them.
    ///
    /// A file the server holds open is not announced: its text is the
    /// client's, so it is re-synced. Neither is a file still on disk that
    /// the index holds (GM-624): it is opened with the index text. A
    /// `workspace/didChangeWatchedFiles` has no acknowledgement - the server
    /// reads the file whenever it gets to it, so the next request may still
    /// see the old text - and what it reads is the disk, which may already
    /// have moved past the text this pass's positions were computed against.
    /// A `didOpen` is applied before the next request and carries exactly
    /// that text. Only deleted files and files the index does not hold
    /// (a manifest, say) are still announced. Created and changed files are
    /// both `Changed`: the server reads the file either way and tells the
    /// two apart by what it had.
    ///
    /// A deleted file the server holds open is closed first (`didClose`),
    /// dropped from `opened`, and then announced `Deleted` like any other
    /// deleted file. Re-syncing it instead would send nothing - the index no
    /// longer holds it, so [`LspBridge::sync_documents`] skips it - and the
    /// server would go on resolving into the deleted file's last text (a
    /// `mod util;` still finding the removed `util.rs`). Whether a file is
    /// deleted is read off disk when the batch is forwarded, so one deleted
    /// and recreated before then is simply an edit.
    fn forward_watched(
        client: &mut LspClient,
        pending: &mut BTreeSet<RelPath>,
        opened: &mut BTreeMap<RelPath, OpenDocument>,
        root: &Path,
        index: &SdkIndex,
    ) -> BTreeSet<RelPath> {
        const CHANGED: u32 = 2;
        const DELETED: u32 = 3;
        let mut resync = BTreeSet::new();
        let mut changes = Vec::new();
        for path in std::mem::take(pending) {
            let absolute = path.to_absolute(root);
            let exists = absolute.exists();
            if exists && (opened.contains_key(&path) || index.entry(&path).is_some()) {
                resync.insert(path);
                continue;
            }
            if !exists && opened.remove(&path).is_some() {
                let _ = client.notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": file_uri(&absolute) } }),
                );
            }
            let kind = if exists { CHANGED } else { DELETED };
            changes.push(json!({ "uri": file_uri(&absolute), "type": kind }));
        }
        if !changes.is_empty() {
            let _ = client.notify("workspace/didChangeWatchedFiles", json!({ "changes": changes }));
            // Same reasoning as a `didChange`: the server reanalyses.
            client.mark_edited();
        }
        resync
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
                g_mesh_plugin_sdk::log_line!(
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

/// The `languageId` `path` is opened under: the first of `language_ids`
/// whose extension the path ends with, else `language`.
fn language_id<'a>(path: &RelPath, language: &'a str, language_ids: &[(&str, &'a str)]) -> &'a str {
    language_ids
        .iter()
        .find(|(extension, _)| path.as_str().ends_with(extension))
        .map_or(language, |(_, id)| id)
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

mod answers;
mod hover;
mod landing;
mod pass;
mod questions;

#[cfg(test)]
mod tests;
