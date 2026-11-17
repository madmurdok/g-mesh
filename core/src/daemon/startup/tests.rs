//! [`StartupPlan::decide`]: the plan for every shape of [`IndexFacts`] the
//! design names (`docs/design/GM-707-watcher-ingest-startup-phases.md`,
//! section 5). Pure: no index, no daemon; each case compares the whole plan.

use super::*;

fn unwalked(had_generation: bool, reinitialized: bool) -> IndexFacts {
    IndexFacts { had_generation, reinitialized, walk: WalkFacts::Unwalked }
}

fn walked(semantic_pass_completed: bool, retry_languages: &[&str], seeded: bool) -> IndexFacts {
    IndexFacts {
        had_generation: true,
        reinitialized: false,
        walk: WalkFacts::Walked {
            semantic_pass_completed,
            retry_languages: retry_languages.iter().map(|language| language.to_string()).collect(),
            seeded,
        },
    }
}

/// An unwalked index's plan: only the cold cause differs between cases 1-3.
fn cold_plan(cause: ColdCause) -> StartupPlan {
    StartupPlan {
        initial: InitialPhase::Unindexed(cause),
        needs_walk: true,
        needs_semantic_pass_retry: false,
        retry_languages: Vec::new(),
        seeded: false,
        watcher: WatcherPlan::StartedByActivation,
    }
}

/// Case 1: no generation was ever stored, so the cause is `Fresh` whatever
/// `ensure_current` reported.
#[test]
fn an_index_with_no_generation_is_unindexed_because_fresh_and_activation_starts_the_watcher() {
    for reinitialized in [false, true] {
        assert_eq!(
            StartupPlan::decide(&unwalked(false, reinitialized)),
            cold_plan(ColdCause::Fresh),
            "reinitialized = {reinitialized}"
        );
    }
}

/// Case 2.
#[test]
fn a_stored_generation_that_was_reinitialized_is_unindexed_because_discarded() {
    assert_eq!(StartupPlan::decide(&unwalked(true, true)), cold_plan(ColdCause::Discarded));
}

/// Case 3.
#[test]
fn a_current_generation_whose_walk_never_finished_is_unindexed_because_incomplete() {
    assert_eq!(StartupPlan::decide(&unwalked(true, false)), cold_plan(ColdCause::Incomplete));
}

/// Case 4.
#[test]
fn a_walked_index_that_owes_nothing_is_structural_and_its_consumer_starts_now() {
    assert_eq!(
        StartupPlan::decide(&walked(true, &[], false)),
        StartupPlan {
            initial: InitialPhase::Structural,
            needs_walk: false,
            needs_semantic_pass_retry: false,
            retry_languages: Vec::new(),
            seeded: false,
            watcher: WatcherPlan::ConsumerNow,
        }
    );
}

/// Case 5: the seeded consumer starts now and opens the gate itself.
#[test]
fn an_unreconciled_seed_that_owes_no_retry_is_walking_because_seeded_and_its_consumer_starts_now() {
    assert_eq!(
        StartupPlan::decide(&walked(true, &[], true)),
        StartupPlan {
            initial: InitialPhase::Walking(ColdCause::Seeded),
            needs_walk: false,
            needs_semantic_pass_retry: false,
            retry_languages: Vec::new(),
            seeded: true,
            watcher: WatcherPlan::ConsumerNow,
        }
    );
}

/// Case 6: no incremental pass may run before the semantic-pass retry.
#[test]
fn a_walked_index_owing_its_semantic_pass_parks_the_watcher_for_activation() {
    assert_eq!(
        StartupPlan::decide(&walked(false, &[], false)),
        StartupPlan {
            initial: InitialPhase::Structural,
            needs_walk: false,
            needs_semantic_pass_retry: true,
            retry_languages: Vec::new(),
            seeded: false,
            watcher: WatcherPlan::ParkedForActivation,
        }
    );
}

/// Case 7: no edit of a failed language routes before its retry.
#[test]
fn a_walked_index_owing_a_failed_languages_retry_parks_the_watcher_for_activation() {
    assert_eq!(
        StartupPlan::decide(&walked(true, &["go"], false)),
        StartupPlan {
            initial: InitialPhase::Structural,
            needs_walk: false,
            needs_semantic_pass_retry: false,
            retry_languages: vec!["go".to_string()],
            seeded: false,
            watcher: WatcherPlan::ParkedForActivation,
        }
    );
}

/// Case 8: a retry owed by a seed parks the watcher; the gate goes to
/// activation, which starts the seeded consumer after the retry.
#[test]
fn an_unreconciled_seed_owing_its_semantic_pass_is_walking_and_parks_the_watcher_for_activation() {
    assert_eq!(
        StartupPlan::decide(&walked(false, &[], true)),
        StartupPlan {
            initial: InitialPhase::Walking(ColdCause::Seeded),
            needs_walk: false,
            needs_semantic_pass_retry: true,
            retry_languages: Vec::new(),
            seeded: true,
            watcher: WatcherPlan::ParkedForActivation,
        }
    );
}

/// Both retries owed at once still park the watcher, and the languages pass
/// through in the order they were read.
#[test]
fn both_retries_owed_park_the_watcher_and_keep_the_languages_in_order() {
    let plan = StartupPlan::decide(&walked(false, &["python", "go"], false));
    assert_eq!(plan.watcher, WatcherPlan::ParkedForActivation);
    assert!(plan.needs_semantic_pass_retry);
    assert_eq!(plan.retry_languages, vec!["python".to_string(), "go".to_string()]);
}

/// A walked index's plan never depends on the cold cause: the generation
/// facts only name why an unwalked index is cold.
#[test]
fn a_walked_index_ignores_the_generation_facts() {
    let walk = walked(true, &[], false).walk;
    let expected = StartupPlan::decide(&walked(true, &[], false));
    for (had_generation, reinitialized) in [(false, false), (false, true), (true, true), (true, false)] {
        let facts = IndexFacts { had_generation, reinitialized, walk: walk.clone() };
        assert_eq!(StartupPlan::decide(&facts), expected, "{facts:?}");
    }
}
