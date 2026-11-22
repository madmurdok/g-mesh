//! The reranker under each variant's settings: which variant and directory
//! it loads, at which beta, what it logs and what it records, for a variant
//! that stands in for an unpublished one, for the measurement setup that
//! loads an unpublished one, and for the fallback of a missing default.

use std::sync::{Arc, Mutex};

use super::*;

/// Logits `[1, 0]` for any two texts: with cosines [`COSINES`], beta 40
/// orders `[1, 0]` and beta 5 orders `[0, 1]`.
struct Fixed;

impl Scorer for Fixed {
    fn score(&self, _query: &str, texts: &[String]) -> Result<Vec<f32>> {
        assert_eq!(texts.len(), 2);
        Ok(vec![1.0, 0.0])
    }

    fn count_tokens(&self, _text: &str) -> usize {
        1
    }
}

const COSINES: [f64; 2] = [0.0, 0.1];
const ORDER_AT_L6_BETA: [usize; 2] = [1, 0];
const ORDER_AT_BETA_5: [usize; 2] = [0, 1];

/// What a reranker's loader was asked for, and every line it logged.
#[derive(Default)]
struct Seen {
    loads: Mutex<Vec<(PathBuf, RerankVariant)>>,
    lines: Mutex<Vec<String>>,
}

/// A reranker over `settings` whose loader answers with [`Fixed`] when
/// `loads`, else an error.
fn reranker(settings: RerankSettings, loads: bool) -> (Reranker, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let settings = Mutex::new(Some(settings));
    let (load_seen, log_seen) = (Arc::clone(&seen), Arc::clone(&seen));
    let reranker = Reranker::with_variant_loader(
        move || settings.lock().unwrap().take().expect("settings are taken once"),
        move |dir, variant| {
            load_seen.loads.lock().unwrap().push((dir.to_path_buf(), variant));
            if loads {
                Ok(Box::new(Fixed) as Box<dyn Scorer>)
            } else {
                bail!("the loader refused")
            }
        },
        move |line| log_seen.lines.lock().unwrap().push(line.to_string()),
    );
    (reranker, seen)
}

/// The order the loaded reranker gives two texts under [`COSINES`].
fn order_of(reranker: &Reranker) -> Vec<usize> {
    let scorer = reranker.scorer().expect("loaded");
    reranker.order(scorer, "q", &["a".to_string(), "b".to_string()], &COSINES).expect("an order")
}

fn settings(variant: RerankVariant, dir: &Path) -> RerankSettings {
    RerankSettings {
        enabled: true,
        variant,
        model_dir: Ok(dir.to_path_buf()),
        fallback_dir: None,
        beta_override: None,
        unpublished: None,
    }
}

/// A directory holding `variant`'s two files at their pinned sizes.
fn dir_with_sizes_of(variant: RerankVariant) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let spec = variant.spec();
    for (name, size) in [(ONNX_FILE_NAME, spec.onnx_size), (TOKENIZER_FILE_NAME, spec.tokenizer_size)] {
        File::create(dir.path().join(name)).unwrap().set_len(size).unwrap();
    }
    dir
}

/// The fixture's two orders tell beta 40 from beta 5 apart.
#[test]
fn the_fixed_scores_order_differently_at_the_two_betas() {
    assert_eq!(L6.beta, 40.0);
    assert_eq!(blended_order(&[1.0, 0.0], &COSINES, L6.beta).unwrap(), ORDER_AT_L6_BETA);
    assert_eq!(blended_order(&[1.0, 0.0], &COSINES, 5.0).unwrap(), ORDER_AT_BETA_5);
}

/// `l6` standing in for an unpublished `qat`: loaded from its directory at
/// its own beta, one line saying so, and the record names `qat`.
///
/// *Control:* drop the `if let Some(asked) = unpublished` log arm from
/// `Reranker::outcome` -> no line.
#[test]
fn a_stand_in_for_an_unpublished_variant_logs_one_line_and_ranks_at_its_own_beta() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let record_path = state.path().join("rerank.outcome");
    let (reranker, seen) = reranker(
        RerankSettings { unpublished: Some(RerankVariant::Qat), ..settings(RerankVariant::L6, dir.path()) },
        true,
    );
    reranker.record_outcome_to(record_path.clone());

    assert_eq!(order_of(&reranker), ORDER_AT_L6_BETA);
    assert_eq!(*seen.loads.lock().unwrap(), [(dir.path().to_path_buf(), RerankVariant::L6)]);
    let lines = seen.lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains(&format!(
            "rerank model qat is not published yet - search_code ranks with l6 from {} at l6's own beta",
            dir.path().display()
        )),
        "{lines:?}"
    );
    let record = OutcomeRecord::read(&record_path).unwrap();
    assert_eq!(
        (record.outcome, record.variant, record.fallback, record.unpublished),
        (RerankOutcome::Loaded, Some(RerankVariant::L6), false, Some(RerankVariant::Qat))
    );
}

/// The measurement setup: `qat` itself, from the given directory, at the
/// given beta, with no stand-in line.
#[test]
fn the_measurement_setup_loads_qat_at_the_given_beta() {
    let dir = tempfile::tempdir().unwrap();
    let (reranker, seen) = reranker(
        RerankSettings { beta_override: Some(5.0), ..settings(RerankVariant::Qat, dir.path()) },
        true,
    );
    assert_eq!(order_of(&reranker), ORDER_AT_BETA_5);
    assert_eq!(*seen.loads.lock().unwrap(), [(dir.path().to_path_buf(), RerankVariant::Qat)]);
    assert!(seen.lines.lock().unwrap().is_empty(), "{:?}", seen.lines.lock().unwrap());
}

/// An unpublished variant that fails to load is not sent to `g-mesh model
/// fetch`, which cannot add it; a published one is.
///
/// *Control:* make the advice in `Reranker::outcome` always "Run g-mesh
/// model fetch to enable it." -> the `qat` line names the fetch.
#[test]
fn an_unpublished_variant_that_fails_to_load_is_not_sent_to_model_fetch() {
    let empty = tempfile::tempdir().unwrap();
    let (qat, seen) = reranker(
        RerankSettings { beta_override: Some(5.0), ..settings(RerankVariant::Qat, empty.path()) },
        false,
    );
    assert_eq!(qat.take_unavailable_hint(), Some(RerankOutcome::Absent));
    let lines = seen.lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("It is not published yet, so g-mesh model fetch cannot add it."), "{lines:?}");
    assert!(!lines[0].contains("Run g-mesh model fetch"), "{lines:?}");

    let (l6, seen) = reranker(settings(RerankVariant::L6, empty.path()), false);
    assert_eq!(l6.take_unavailable_hint(), Some(RerankOutcome::Absent));
    let lines = seen.lines.lock().unwrap().clone();
    assert!(lines[0].contains("Run g-mesh model fetch to enable it."), "{lines:?}");
}

/// Once `qat` is published, a failed `qat` load is sent to `g-mesh model
/// fetch` like `l6`'s.
#[test]
fn a_published_qat_that_fails_to_load_is_sent_to_model_fetch() {
    let empty = tempfile::tempdir().unwrap();
    pretend_published(|| {
        let (qat, seen) = reranker(
            RerankSettings { beta_override: Some(5.0), ..settings(RerankVariant::Qat, empty.path()) },
            false,
        );
        assert_eq!(qat.take_unavailable_hint(), Some(RerankOutcome::Absent));
        let lines = seen.lines.lock().unwrap().clone();
        assert!(lines[0].contains("Run g-mesh model fetch to enable it."), "{lines:?}");
    });
    assert!(!RerankVariant::Qat.published(), "the pretence ends with its body");
}

/// The fallback for a missing default ranks at its own beta: a
/// `G_MESH_RERANK_BETA` set for the selected variant does not reach it.
///
/// *Control:* return `self.beta_override` from the fallback arm of
/// `RerankSettings::resolve_fallback` -> the order is beta 5's.
#[test]
fn the_fallback_for_a_missing_default_ranks_at_its_own_beta() {
    let missing = tempfile::tempdir().unwrap();
    let l6_dir = dir_with_sizes_of(RerankVariant::L6);
    let (reranker, seen) = reranker(
        RerankSettings {
            fallback_dir: Some(l6_dir.path().to_path_buf()),
            beta_override: Some(5.0),
            ..settings(RerankVariant::Qat, missing.path())
        },
        true,
    );
    assert_eq!(order_of(&reranker), ORDER_AT_L6_BETA);
    assert_eq!(*seen.loads.lock().unwrap(), [(l6_dir.path().to_path_buf(), RerankVariant::L6)]);
    let lines = seen.lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("rerank model qat is not fetched - search_code ranks with l6"), "{lines:?}");
}

/// `l6` selected with a beta set ranks at that beta: the rule scopes the
/// beta to the variant loaded, not away from `l6`.
#[test]
fn a_beta_set_for_the_loaded_l6_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let (reranker, _) = reranker(
        RerankSettings { beta_override: Some(5.0), ..settings(RerankVariant::L6, dir.path()) },
        true,
    );
    assert_eq!(order_of(&reranker), ORDER_AT_BETA_5);
}

/// A switched-off rerank records no variant and so no unpublished one;
/// `unpublished` is left out of a record that has none, and a record
/// without it reads.
///
/// *Control:* write `unpublished` instead of `variant.and(unpublished)` in
/// `Reranker::outcome` -> the switched-off record names `qat`.
#[test]
fn the_record_names_an_unpublished_variant_only_beside_a_loaded_one() {
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("rerank.outcome");
    let (off, _) = reranker(
        RerankSettings {
            enabled: false,
            unpublished: Some(RerankVariant::Qat),
            ..settings(RerankVariant::L6, Path::new("/stub"))
        },
        true,
    );
    off.record_outcome_to(path.clone());
    assert!(off.scorer().is_none());
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(!written.contains("unpublished"), "{written}");
    let record = OutcomeRecord::read(&path).unwrap();
    assert_eq!((record.outcome, record.variant, record.unpublished), (RerankOutcome::Off, None, None));

    std::fs::write(&path, r#"{"pid":7,"outcome":"loaded","dir":"/m","variant":"l6"}"#).unwrap();
    assert_eq!(OutcomeRecord::read(&path).map(|record| record.unpublished), Some(None));
    std::fs::write(&path, r#"{"pid":7,"outcome":"loaded","dir":"/m","variant":"l6","unpublished":"qat"}"#)
        .unwrap();
    assert_eq!(OutcomeRecord::read(&path).and_then(|record| record.unpublished), Some(RerankVariant::Qat));
}
