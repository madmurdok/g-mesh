//! The similarity floor, and the one shape `search_code` has ever had that
//! means *no* (GM-381).
//!
//! # The defect this closes
//!
//! `search_code` is similarity-ranked and always returns rows. "Nothing in
//! this index matches" and "here are twenty ranked guesses" were the same
//! response: same fields, same row count, same confident ordering. There was
//! no shape the tool could produce that a caller could branch on to mean
//! *no*.
//!
//! That matters more here than it would on another tool, because the shipped
//! guidance (`cli::agent_instructions`, README's "Reducing self-verification
//! cost") tells an agent to reach for `search_code` **first** on a "find the
//! function that does X" prompt, and that advice is earned - measured, reps
//! that called it first converged in 8-11 turns against 15 for the one that
//! grep-guessed. Advice to go first is only safe if going wrong is legible.
//!
//! # What this says, and what it refuses to say
//!
//! [`NoMatch`] carries a reason and a sentence, and **no number**. The score
//! that produced the verdict is already in the page - every row carries its
//! own `score` - so the caller can check the verdict against its own inputs
//! without this block repeating one of them. What the block deliberately does
//! *not* publish is the floor itself. A constant on the wire is a constant
//! callers re-implement against, and then disagree with the server about the
//! day a re-run of the calibration moves it; `super::provenance`'s rule
//! ("never manufacture a number a caller will act on") is the neighbouring
//! case, and this is the same instinct applied to a number that is real but
//! is not the caller's to act on.
//!
//! # The rows stay, and that is the load-bearing choice
//!
//! Five shapes were weighed: a floor that drops rows below it, a margin
//! between the first and second hit, a `resolvedBy`-style label, an outright
//! refusal like `find_definition::import_only_refusal`, and this one - keep
//! every row, add a response-level verdict. Two arguments decided it.
//!
//! **Recall cannot regress, by construction.** A shape that drops rows has to
//! be defended with a count of the right answers it would now refuse. This one
//! refuses none. Measured over the benchmark corpora's own 34 oracle symbols,
//! of which 28 have a correct hit somewhere in the top ten, a *row-dropping*
//! floor would have lost 2 of them at 0.60 (`LaserTrails`, correct hit 0.472
//! at rank 7, and `LineStep`, 0.584 at rank 4) and 8 at 0.65, six of those
//! ranked **first**. Under this shape every one of them is still on the page,
//! and only the page-level verdict changes.
//!
//! **A floor measured on five repositories can be wrong on a sixth, and the
//! failure has to stay visible.** A verdict that silently deletes rows makes a
//! miscalibrated floor invisible - the tool just gets worse on that repository
//! and nothing says why. A verdict printed beside the rows it judged can be
//! read against them: a caller who sees `noMatch` above a row scoring 0.58 can
//! see the disagreement, and so can whoever re-runs the calibration. This
//! codebase's standing rule is that a missing edge beats a wrong one, and that
//! rule is about *asserting* something false; a ranked row asserts nothing,
//! so it is not the thing the rule is protecting against.
//!
//! The margin (`top1 - top2`) lost on measurement rather than on taste, which
//! is worth recording because it is the intuitive choice. Separating "the
//! right answer is on this page" from "it is not in this index at all", on
//! the name-query arm (about 150 positives and 150 negatives per language):
//!
//! | feature | go | python | rust | typescript |
//! |---|---|---|---|---|
//! | top score (this floor) | 0.997 | 0.993 | 0.966 | 0.977 |
//! | `top1 - mean(rest)` | 0.960 | 0.945 | 0.894 | 0.920 |
//! | `top1 - top2` | 0.842 | 0.825 | 0.728 | 0.850 |
//!
//! (area under the ROC curve; 0.5 is a coin). The absolute score wins in every
//! language, and the gap widens on free-text queries, where the margin falls
//! to 0.43-0.71 - at or below chance. The reason is structural: a *good* page
//! often has two good hits (an interface and its impl, an overload pair), so a
//! small margin is as much a property of a right answer as of a wrong one.
//!
//! # Two reasons, and shape is checked before score
//!
//! [`NoMatchReason::QueryIsAPathOrPackage`] fires first because the score
//! cannot catch it. Only declarations' doc comments and signatures are
//! embedded, so a specifier has nothing to match and its similarity is
//! computed against unrelated text: `@excalidraw/element` scores **0.699**
//! against excalidraw - above every floor below - and **0.566** against
//! task-tracker-mcp, an index where that package does not exist at all. Both
//! numbers reproduced here exactly, on freshly built indexes, from
//! `g-mesh-bench`'s `docs/results/v0.21.0-semantic-threshold-calibration.md`.
//! A score that high on an index the query has nothing to do with is proof
//! that the number describes the query's shape, not the corpus.
//!
//! [`find_definition::is_module_specifier`](super::find_definition) makes the
//! same check and deliberately makes it differently: its input is a symbol
//! name, so `contains('/')` is enough. This tool's input is free text, where
//! `"serialize/deserialize the config"` is an ordinary query - hence the
//! whitespace guard here. Measured across every arm of the sweep,
//! [`is_specifier_query`] fires on 35 of the 70 junk queries (package
//! specifiers, paths and invented identifiers) and on **0 of 2,275** name,
//! short-phrase, sentence and cross-corpus queries.
//!
//! # A name is told *no*; prose is told *low*
//!
//! The same below-floor first page gets [`NoMatch`] when the query is a name
//! and [`low_similarity`]'s sentence when it is prose ([`is_prose_query`]),
//! never both. The prose sentence asks for one confirming read of the top
//! row and must not vouch for it: a caller that accepts it unread loses the
//! floor's protection. Decision and measurements:
//! `docs/adr/0010-search-code-low-similarity-on-prose.md`.
//!
//! # Per language, because doc-comment density is not a constant
//!
//! Embeddings sit in one space; the text fed into them does not. The four
//! bundled languages differ in how much of a declaration is prose: ripgrep
//! 40.2% of embedded declarations carry a doc comment, gin 34.0%,
//! py-requests 30.7%, task-tracker-mcp 33.1%, excalidraw **20.2%**. What was
//! measured, rather than reasoned from that, is the outcome: a right answer's
//! score has a median of 0.908 in gin and 0.762 across the two TypeScript
//! corpora, and the two distributions' low tails sit 0.10 apart. Which of
//! prose density, index size and naming style produces that is not separated
//! here - only that the languages land in different places, so one constant
//! cannot be right for all four.
//!
//! It is not right for all four. The shipped 0.60 was measured on TypeScript,
//! and TypeScript is the language it fits worst, because it was calibrated on
//! symbol-name queries alone. Over all four query shapes measured here, and
//! counting a false alarm as the verdict firing on a page whose right answer
//! was ranked **first**:
//!
//! | language | this floor | false alarm | caught | at a global 0.60 |
//! |---|---|---|---|---|
//! | go | 0.59 | 1.2% | 90.5% | 1.9% / 91.5% |
//! | python | 0.57 | 0.0% | 81.9% | 1.8% / 87.2% |
//! | rust | 0.55 | 1.3% | 74.8% | 3.1% / 85.8% |
//! | typescript | 0.50 | 2.4% | 70.9% | **8.1%** / 92.5% |
//!
//! Those are the fp32 model's floors and rates. g-mesh ships the int8
//! weights with their own fitted floors, go / python / rust / typescript
//! **0.57 / 0.57 / 0.55 / 0.53** (`docs/adr/0011-embedding-model-int8.md`,
//! fit in `docs/results/gm-398-model-comparison.md`); [`floor`] holds them.
//! The argument above for one floor per language holds for both.
//!
//! Averaging these into one number would cost TypeScript a false "nothing
//! matched" on one search in twelve - which is precisely the "a guarantee
//! measured on TypeScript is a guarantee about TypeScript" failure
//! `super::provenance` was written about, arriving from the other direction.
//!
//! # Why not `provenance`, which is the tempting place to put this
//!
//! `super::provenance` already carries a response-level disclosure and it
//! would have been cheap to add a third variant to it. It answers a different
//! question. `provenance` says *which tier produced this answer* - a property
//! of the index and the installed plugins, identical for every query until
//! something is installed or a pass completes. This says *how much this
//! particular match is worth* - a property of one query against one index,
//! different on the next call. A caller taught that `provenance` means "your
//! installation is degraded" would read a low-similarity page as a missing
//! `rust-analyzer`, and a caller taught the reverse would read a missing
//! engine as a bad query. Both fields get less readable for being one field.
//!
//! `provenance`'s own module doc also draws the line explicitly, in its rule
//! 1: `search_code` "answers exactly as well without" the semantic tier and
//! "must stay silent". Reusing the field would have required breaking a rule
//! that module states about this tool by name. What is reused is the
//! *discipline*: a conditional disclosure, absent in the healthy case, so
//! that seeing it means something.
//!
//! # What this does not fix
//!
//! A floor addresses "nothing matched". It does not address the other half of
//! the calibration's finding: a closely related declaration, confidently
//! scored, that is not the one asked for - `AppState` returning
//! `createAppState` at 0.845. No threshold can separate those, because the
//! wrong answer scores like a right one. That case is what
//! `find_definition`'s semantic rung labels `resolvedBy: semanticNeighbours`
//! for, and what `search_code`'s tool description means by "ranked by
//! similarity". It is named here so the next reader does not mistake this
//! module for a fix for it.
//!
//! # How these numbers were reached, and what they are worth
//!
//! 2,345 `search_code` calls, across five corpora at their pinned benchmark
//! revisions (excalidraw and task-tracker-mcp for TypeScript, gin for Go,
//! ripgrep for Rust, py-requests for Python), each indexed from scratch by
//! this workspace's own build. Ground truth is mechanical rather than
//! hand-picked: positives are declarations sampled from each index by a fixed
//! seed and queried by their own name (n=150 per corpus), plus the corpora's
//! own task prompts at three lengths; negatives are real identifiers sampled
//! from the *other* corpora and proven absent from this one (n=150 per
//! corpus), plus each corpus's prompts asked of a different language's index.
//! Every query was assigned to a fit or a held-out half by a hash of its own
//! text. The floor is the largest value at which the fit half's false-alarm
//! rate stays at or below 3%, rounded **down** to two decimals - down, because
//! a lower floor fires less often, and silence is the status quo rather than a
//! new failure.
//!
//! Two limits belong next to the numbers rather than in a footnote. First,
//! the held-out half reproduces the false-alarm rate closely for go
//! (2.4% -> 0.0%), python (0.0% -> 0.0%) and rust (1.4% -> 1.2%), and less
//! closely for typescript (1.6% -> 3.2%); on the looser any-rank definition
//! TypeScript degrades further (2.2% -> 6.4%), and that is the one number in
//! this module that does not hold up out of sample. Second, the free-text arm
//! is thin outside TypeScript - 5 to 8 positive queries per language against
//! 143 to 149 name queries - so these floors are *measured* on symbol-name
//! queries and only *checked* on free text. They are therefore set low enough
//! that free text does not suffer, which is why the catch rates are 71-90%
//! rather than the 86-93% a name-only calibration would have reported.

use serde::Serialize;

use super::search_code::SearchResult;

/// Why this page is not a match.
///
/// A closed set, and ordered as [`verdict`] tries them: shape first, because
/// a specifier's score is not evidence at all, and only then the score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum NoMatchReason {
    /// The query is a path or a package specifier rather than a description,
    /// so its score is a property of its shape - see this module's doc.
    QueryIsAPathOrPackage,
    /// Every row scored below the floor for its own language.
    BelowSimilarityFloor,
}

/// The response-level verdict on a `search_code` page.
///
/// Response-level rather than per-row, for the reason
/// `find_callers_callees::ExcludedReferences` and `all_unresolved` are: it is
/// a property of the call, and a property of the call repeated once per row is
/// both wrong in shape and paid for per row. Present only when the answer is
/// *no* - a field that appeared on every response would be a permanent
/// footnote rather than a signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NoMatch {
    pub(super) reason: NoMatchReason,
    /// Said in the response rather than left to the guidance, because the
    /// guidance is installed per project and this verdict has to be readable
    /// by a caller that never saw it - the same reason
    /// `find_definition::by_semantic_neighbours` carries its own
    /// `explanation` on the weakest rung of that ladder.
    pub(super) explanation: &'static str,
}

const BELOW_FLOOR_EXPLANATION: &str =
    "Nothing here reached the similarity floor for its language, so read this page as 'no \
     declaration in this index matches' rather than as candidates. The rows are still listed, \
     and their `score` column is what this verdict was computed from - but they are the nearest \
     vectors, not matches. Fall back to a structural tool or to grep rather than rewording the \
     query.";

/// [`low_similarity`]'s sentence: the soft counterpart of
/// [`BELOW_FLOOR_EXPLANATION`] for a prose query. It asks for one check and
/// must not vouch for the row (no "plausibly matches", no "stop").
const LOW_SIMILARITY_EXPLANATION: &str =
    "Every row scored low for its language, so none is a confident match. The top row may still \
     be right: check it with one read, and if it is not, fall back to a structural tool or grep \
     rather than rewording the query.";

const SPECIFIER_EXPLANATION: &str =
    "This query is a path or a package specifier, not a description. Only declarations' doc \
     comments and signatures are embedded, so a specifier has nothing to match and the scores \
     below are a property of the query's shape rather than evidence about this project. Use \
     get_dependencies for an import path, or find_definition for a file name.";

/// The floor for a language whose plugin was never calibrated.
///
/// The lowest of the four measured, not their mean: an unmeasured language
/// could sit lower than any of them, and a floor that is too low fires less
/// often - which leaves the caller exactly where it was before this module
/// existed, rather than somewhere new and wrong. The same "assumed to do the
/// least" default `mcp::instructions::present_languages` applies to a
/// language whose manifest has gone missing.
const DEFAULT_FLOOR: f64 = 0.53;

/// The similarity floor for `language`, measured per language because
/// doc-comment density is not a constant - see this module's doc comment for
/// the full table, the sample, and the held-out check.
///
/// Matched on the manifest's own `language` id, spelled exactly as
/// `nodes.language` and `daemon::manifest` spell it, for the reason
/// `provenance::Provenance::language` gives: an agent cross-referencing the
/// two must never meet two spellings of one language.
pub(crate) fn floor(language: &str) -> f64 {
    // The int8 model's fitted floors (ADR 0011). Held-out false alarm at
    // each, go / python / rust / typescript: 29.4 / 10.0 / 14.3 / 7.7%.
    match language {
        "go" => 0.57,
        "python" => 0.57,
        "rust" => 0.55,
        // The lowest of the four: DEFAULT_FLOOR must equal it.
        "typescript" => 0.53,
        _ => DEFAULT_FLOOR,
    }
}

/// Whether `query` is a specifier rather than a description of behaviour.
///
/// Three conditions, and the whitespace one is the whole difference from
/// `find_definition::is_module_specifier`: that predicate judges a symbol
/// name, where a `/` can only be a path separator, while this one judges free
/// text, where `"serialize/deserialize the config"` is a perfectly ordinary
/// thing to search for. Measured over the whole sweep this fires on 35 of the
/// 70 junk queries - every package specifier and path, none of the invented
/// identifiers, which the floor catches instead - and on 0 of 2,275 real
/// ones.
pub(super) fn is_specifier_query(query: &str) -> bool {
    let query = query.trim();
    !query.is_empty() && !is_prose_query(query) && (query.starts_with('@') || query.contains('/'))
}

/// Whether `query` is prose rather than a name: whitespace *inside* it, after
/// trimming, so `"  readFile  "` is still a name and `"read file"` is prose.
/// `parse_config` is a name and `parse config` is prose.
///
/// This is exactly the rule the floor eval scores
/// (`eval/embedding/shipped_floor_rates.py`:
/// `any(c.isspace() for c in text.strip())`), and it is the whitespace half of
/// [`is_specifier_query`], so the two cannot drift apart: a specifier is never
/// prose, which keeps [`verdict`] and [`low_similarity`] disjoint.
pub(super) fn is_prose_query(query: &str) -> bool {
    query.trim().chars().any(char::is_whitespace)
}

/// A first page with rows, every one of them below its own language's floor.
///
/// Each row against its own language's floor, not the page's best row
/// against one of them: in a polyglot repository a page can mix languages,
/// and a Go hit at 0.58 and a TypeScript hit at 0.58 are not worth the same.
/// On the single-language repositories this was measured over the two rules
/// coincide, which is why the distinction is stated by what it guarantees
/// rather than by a measurement that could not separate them.
fn below_floor(cursor: Option<&str>, results: &[SearchResult]) -> bool {
    cursor.is_none() && !results.is_empty() && results.iter().all(|hit| hit.score < floor(&hit.language))
}

/// The verdict for one `search_code` page, or `None` when there is nothing
/// to say.
///
/// Four ways to get `None`, and each is a deliberate silence:
///
/// - **`cursor` is `Some`.** Only a first page is judged. A continuation's
///   rows are by construction the ones the first page already outranked, so
///   "nothing here cleared the floor" would be true of most continuations and
///   would say nothing about the query. The caller that paged has already
///   read the verdict on page one. The rule lives here, with the rest of the
///   verdict's meaning, rather than as a condition at the call site where it
///   could not be tested against its own control.
/// - **The page is empty** and the query is not a specifier. `results: []` is
///   already a shape that means no, and it means a *different* no (nothing
///   embedded, or nothing indexed yet), which is the conflation
///   `super::provenance` spends its own doc comment refusing. Annotating it
///   would make two causes look like one. A specifier still gets its verdict
///   here, because that one is a fact about the query rather than about what
///   the index happened to return.
/// - **Some row cleared its language's floor.** The healthy case, and the
///   common one, which is what keeps the field worth reading when it appears.
/// - **The query is prose.** A below-floor prose page gets
///   [`low_similarity`] instead; only a name query is told *no*.
pub(super) fn verdict(query: &str, cursor: Option<&str>, results: &[SearchResult]) -> Option<NoMatch> {
    if cursor.is_some() {
        return None;
    }
    if is_specifier_query(query) {
        return Some(NoMatch {
            reason: NoMatchReason::QueryIsAPathOrPackage,
            explanation: SPECIFIER_EXPLANATION,
        });
    }
    (below_floor(cursor, results) && !is_prose_query(query)).then_some(NoMatch {
        reason: NoMatchReason::BelowSimilarityFloor,
        explanation: BELOW_FLOOR_EXPLANATION,
    })
}

/// The soft counterpart of [`verdict`]'s floor verdict, for a prose query:
/// the same page (first page, rows, every row below its language's floor),
/// but a sentence saying the top row may still be right instead of a *no* -
/// see this module's "A name is told *no*" section.
///
/// `None` on a name query, where [`verdict`] speaks instead, and on every
/// page [`verdict`] would stay silent about for the same reasons.
pub(super) fn low_similarity(
    query: &str,
    cursor: Option<&str>,
    results: &[SearchResult],
) -> Option<&'static str> {
    (below_floor(cursor, results) && is_prose_query(query)).then_some(LOW_SIMILARITY_EXPLANATION)
}

/// [`verdict`] for a page ranked while the embedding pass is still owed.
/// "Nothing close" is not known while vectors are missing, so the floor's
/// verdict is withheld; the one about the query's shape still stands.
pub(super) fn partial_verdict(
    query: &str,
    cursor: Option<&str>,
    results: &[SearchResult],
) -> Option<NoMatch> {
    verdict(query, cursor, results).filter(|no_match| no_match.reason == NoMatchReason::QueryIsAPathOrPackage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(score: f64, language: &str) -> SearchResult {
        SearchResult::for_test(score, language)
    }

    /// The control for every "fires" test below: the *only* thing that
    /// changes is the score, and the verdict has to vanish for it. A block
    /// present in both arms would be a permanent footnote, not a signal.
    #[test]
    fn a_page_whose_best_row_clears_its_floor_says_nothing() {
        let page = [hit(0.54, "typescript"), hit(0.30, "typescript")];
        for query in ["parses a config file", "parseConfigFile"] {
            assert_eq!(verdict(query, None, &page), None, "{query}");
            assert_eq!(low_similarity(query, None, &page), None, "{query}");
        }
    }

    #[test]
    fn a_page_whose_every_row_is_below_its_floor_is_a_no() {
        let page = [hit(0.49, "typescript"), hit(0.30, "typescript")];

        let verdict = verdict("parseConfigFile", None, &page).expect("this page is not a match");

        assert_eq!(verdict.reason, NoMatchReason::BelowSimilarityFloor);
        assert_eq!(verdict.explanation, BELOW_FLOOR_EXPLANATION, "a name keeps today's wording");
        assert_eq!(low_similarity("parseConfigFile", None, &page), None, "a name is never told 'low'");
    }

    /// The same below-floor page asked in prose is not a *no*. It
    /// gets the soft sentence and no verdict; the name-query test above is
    /// its control, differing only in the query's whitespace.
    #[test]
    fn a_prose_query_below_its_floor_is_low_similarity_not_a_no() {
        let page = [hit(0.49, "typescript"), hit(0.30, "typescript")];

        assert_eq!(verdict("parses a config file", None, &page), None);
        assert_eq!(low_similarity("parses a config file", None, &page), Some(LOW_SIMILARITY_EXPLANATION));
        assert_eq!(
            low_similarity("parses a config file", Some("c"), &page),
            None,
            "a continuation is not judged"
        );
        assert_eq!(
            low_similarity("parses a config file", None, &[]),
            None,
            "an empty page speaks for itself"
        );
    }

    /// The shape test's edges: whitespace *inside* the trimmed query is prose,
    /// surrounding whitespace is not, and an underscore is not whitespace.
    #[test]
    fn prose_means_whitespace_inside_the_trimmed_query() {
        for name in ["readFile", "  readFile  ", "\treadFile\n", "parse_config", "", "   "] {
            assert!(!is_prose_query(name), "{name:?} is a name");
        }
        for prose in ["read file", "  read file  ", "read\tfile", "parse config"] {
            assert!(is_prose_query(prose), "{prose:?} is prose");
        }
        let page = [hit(0.10, "rust")];
        assert!(verdict("  readFile  ", None, &page).is_some(), "a padded name keeps the hard verdict");
        assert_eq!(low_similarity("  readFile  ", None, &page), None);
        assert!(low_similarity("  read file  ", None, &page).is_some());
    }

    /// The sentence may not vouch for the row it cannot vouch for, may not
    /// leak the floor, and is cheaper than the verdict it replaces.
    #[test]
    fn the_low_similarity_sentence_asks_for_a_check_and_does_not_vouch() {
        for forbidden in ["plausibly", "stop", "0.5", "0.4"] {
            assert!(!LOW_SIMILARITY_EXPLANATION.contains(forbidden), "{forbidden}");
        }
        assert!(LOW_SIMILARITY_EXPLANATION.contains("check it with one read"));
        assert!(LOW_SIMILARITY_EXPLANATION.len() < BELOW_FLOOR_EXPLANATION.len());
    }

    /// The per-language table is the point, not decoration: one score, four
    /// languages, two verdicts. 0.54 clears TypeScript's 0.53 and misses
    /// Rust's 0.55, Python's 0.57 and Go's 0.57.
    #[test]
    fn one_score_is_a_match_in_one_language_and_not_in_another() {
        assert_eq!(verdict("readFile", None, &[hit(0.54, "typescript")]), None);
        for language in ["rust", "python", "go"] {
            assert!(
                verdict("readFile", None, &[hit(0.54, language)]).is_some(),
                "0.54 must be below {language}'s floor"
            );
        }
    }

    /// The shipped floors are the int8 model's fitted ones (ADR 0011), the
    /// floors its confirmatory gates and agent-level check ran at.
    ///
    /// *Control:* restore the fp32 floors (go 0.59, typescript 0.50, default
    /// 0.50) and this fails.
    #[test]
    fn the_shipped_floors_are_the_int8_models() {
        let shipped: Vec<f64> = ["go", "python", "rust", "typescript"].iter().map(|l| floor(l)).collect();
        assert_eq!(shipped, vec![0.57, 0.57, 0.55, 0.53]);
        assert_eq!(floor("kotlin"), 0.53);
    }

    /// A language nothing has calibrated falls to the lowest measured floor,
    /// so it under-fires rather than inventing a threshold for it.
    #[test]
    fn an_unmeasured_language_uses_the_lowest_measured_floor() {
        assert_eq!(floor("kotlin"), DEFAULT_FLOOR);
        assert_eq!(DEFAULT_FLOOR, floor("typescript"), "the default must be the lowest measured floor");
        for language in ["go", "python", "rust"] {
            assert!(floor(language) > DEFAULT_FLOOR, "{language} must sit above the default");
        }
    }

    /// In a polyglot page each row is judged by its own language. The Go row
    /// at 0.54 is below Go's 0.57 while the TypeScript row at 0.54 is above
    /// TypeScript's 0.53, so the page is a match - and it would not be under
    /// a single global floor taken from either language.
    #[test]
    fn a_mixed_language_page_judges_each_row_by_its_own_floor() {
        assert_eq!(verdict("readFile", None, &[hit(0.54, "go"), hit(0.54, "typescript")]), None);
        assert!(verdict("readFile", None, &[hit(0.54, "go"), hit(0.49, "typescript")]).is_some());
    }

    /// The measured case the floor cannot catch: `@excalidraw/element` scores
    /// 0.699 against excalidraw, above every floor in the table, and 0.566
    /// against an index where that package does not exist at all. Shape has
    /// to decide it, and shape is checked first - the score here is high
    /// enough that only the guard firing first can explain the verdict.
    #[test]
    fn a_specifier_is_a_no_at_a_score_no_floor_would_refuse() {
        let page = [hit(0.699, "typescript")];

        let verdict = verdict("@excalidraw/element", None, &page).expect("a specifier is never a match");

        assert_eq!(verdict.reason, NoMatchReason::QueryIsAPathOrPackage);
    }

    /// Free text is this tool's input, so the guard may not fire on a `/`
    /// that a person wrote inside a sentence. This is the whole reason the
    /// predicate is not `find_definition::is_module_specifier`.
    #[test]
    fn a_slash_inside_a_phrase_is_not_a_specifier() {
        assert!(!is_specifier_query("serialize/deserialize the config"));
        assert!(!is_specifier_query("reads a file"));
        assert!(!is_specifier_query("mutateElement"));
        assert!(is_specifier_query("@excalidraw/element"));
        assert!(is_specifier_query("packages/excalidraw/index.tsx"));
        assert!(is_specifier_query("  src/db/connection.ts  "));
    }

    /// An empty page is already a shape that means no, and it means a
    /// different one - see [`verdict`]'s doc comment.
    #[test]
    fn an_empty_page_is_left_to_speak_for_itself() {
        assert_eq!(verdict("reads a file", None, &[]), None);
    }

    /// The wire spelling, pinned here rather than only where it is built:
    /// `camelCase` key, `camelCase` enum value, two fields and no floor.
    /// Publishing the constant is what this module's doc comment refuses.
    #[test]
    fn the_serialized_shape_is_a_reason_and_a_sentence_and_no_number() {
        let json = serde_json::to_string(&NoMatch {
            reason: NoMatchReason::BelowSimilarityFloor,
            explanation: "…",
        })
        .unwrap();

        assert_eq!(json, r#"{"reason":"belowSimilarityFloor","explanation":"…"}"#);
        assert!(!BELOW_FLOOR_EXPLANATION.contains("0.5"), "the floor must not leak through the prose");
    }

    /// The same below-floor page, judged complete and judged partial: only
    /// the partial judgement is silent. A specifier keeps its verdict either
    /// way.
    #[test]
    fn a_partial_page_withholds_the_floor_verdict_but_not_the_specifier_one() {
        let page = [hit(0.10, "typescript")];
        assert!(verdict("readFile", None, &page).is_some(), "the control: complete, this page is a no");
        assert_eq!(partial_verdict("readFile", None, &page), None);
        assert_eq!(
            partial_verdict("@excalidraw/element", None, &page).map(|v| v.reason),
            Some(NoMatchReason::QueryIsAPathOrPackage)
        );
    }
}
