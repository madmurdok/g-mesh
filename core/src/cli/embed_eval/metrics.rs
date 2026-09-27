//! The eval's metrics (D5-D7 of `docs/architecture/embedding-eval.md`), as
//! pure functions over per-query outcomes. Nothing here reads a file or runs
//! a model, so every number the report prints can be unit-tested.
//!
//! Conventions the report relies on:
//! - "pooled" is the mean over languages of each language's mean, so every
//!   language weighs the same whatever its query count;
//! - bounds are one-sided 95%: `lower` is the 5th percentile and `upper` the
//!   95th of a percentile bootstrap stratified by language;
//! - a false alarm is conditional on the right answer being ranked first:
//!   its denominator is the positives whose top hit is expected.

use std::collections::BTreeMap;

use super::rng::Rng;

/// Rankings keep this many hits per query; a first expected hit below it
/// scores 0 in MRR.
pub const KEPT_HITS: usize = 100;

/// The false-alarm rate a floor is fitted to hold (`similarity.rs`).
pub const MAX_FALSE_ALARM: f64 = 0.03;

/// One query's outcome under one arm.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub query_id: String,
    pub corpus: String,
    pub language: String,
    /// `false` for an absent-answer query.
    pub positive: bool,
    /// A mechanical name query (D6): fit half only, never in recall or MRR.
    pub mechanical: bool,
    /// The query shares an identifier sub-token with an expected name.
    pub overlap: bool,
    pub held_out: bool,
    /// 1-based rank of the first expected node among the kept hits.
    pub first_expected_rank: Option<usize>,
    pub top_score: Option<f64>,
    pub top_language: Option<String>,
}

impl Outcome {
    pub fn hit_at(&self, k: usize) -> f64 {
        match self.first_expected_rank {
            Some(rank) if rank <= k => 1.0,
            _ => 0.0,
        }
    }

    pub fn reciprocal_rank(&self) -> f64 {
        match self.first_expected_rank {
            Some(rank) if rank <= KEPT_HITS => 1.0 / rank as f64,
            _ => 0.0,
        }
    }

    fn top_is_expected(&self) -> bool {
        self.first_expected_rank == Some(1)
    }

    /// Whether the top hit clears its own language's floor. `None` when
    /// there is no top hit or no floor for its language.
    fn top_clears_floor(&self, floors: &Floors) -> Option<bool> {
        let score = self.top_score?;
        let floor = floors.get(self.top_language.as_deref()?)?;
        Some(score >= *floor)
    }
}

pub type Floors = BTreeMap<String, f64>;

/// Per-language groups of a per-query value.
pub type Groups = BTreeMap<String, Vec<f64>>;

/// Mean over groups of each group's mean; `None` when no group has values.
pub fn pooled_mean(groups: &Groups) -> Option<f64> {
    let means: Vec<f64> = groups
        .values()
        .filter(|values| !values.is_empty())
        .map(|values| values.iter().sum::<f64>() / values.len() as f64)
        .collect();
    (!means.is_empty()).then(|| means.iter().sum::<f64>() / means.len() as f64)
}

/// Groups `value(o)` by language over the outcomes `keep` selects.
pub fn group_by_language<'a>(
    outcomes: impl IntoIterator<Item = &'a Outcome>,
    keep: impl Fn(&Outcome) -> bool,
    value: impl Fn(&Outcome) -> f64,
) -> Groups {
    let mut groups = Groups::new();
    for o in outcomes {
        if keep(o) {
            groups.entry(o.language.clone()).or_default().push(value(o));
        }
    }
    groups
}

/// The authored positives recall@k and MRR are computed over.
pub fn is_scored_positive(o: &Outcome) -> bool {
    o.positive && !o.mechanical
}

/// Largest floor at which at most `max_rate` of `scores` fall strictly below
/// it, rounded down to two decimals. `scores` are the top scores of queries
/// whose top hit is right; `None` when there are none.
///
/// With the scores sorted ascending and `k = floor(max_rate * n)`, a floor of
/// `s[k]` leaves exactly the `k` smallest (or fewer, on ties) below it, and
/// any higher floor adds `s[k]` itself.
pub fn fit_floor(scores: &[f64], max_rate: f64) -> Option<f64> {
    if scores.is_empty() {
        return None;
    }
    let mut sorted = scores.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let k = ((max_rate * n as f64) + 1e-9).floor() as usize;
    let raw = sorted[k.min(n - 1)];
    Some(round_down_2(raw))
}

fn round_down_2(x: f64) -> f64 {
    ((x * 100.0) + 1e-9).floor() / 100.0
}

/// D6: per language, the floor fitted on the fit half - authored fit-half
/// positives plus every mechanical positive - over the queries whose top
/// hit is expected.
pub fn fit_floors(outcomes: &[Outcome]) -> Floors {
    let mut scores: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for o in outcomes {
        let in_fit = o.mechanical || !o.held_out;
        if o.positive && in_fit && o.top_is_expected() {
            if let Some(score) = o.top_score {
                scores.entry(o.language.clone()).or_default().push(score);
            }
        }
    }
    scores
        .into_iter()
        .filter_map(|(language, s)| fit_floor(&s, MAX_FALSE_ALARM).map(|f| (language, f)))
        .collect()
}

/// A rate as its parts, so the report can print "3/97".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    pub events: usize,
    pub total: usize,
}

impl Rate {
    pub fn value(&self) -> Option<f64> {
        (self.total > 0).then(|| self.events as f64 / self.total as f64)
    }
}

/// D5 false alarm, held-out authored positives of `language` (all languages
/// when `None`): the top hit is right but below its floor.
pub fn false_alarm(outcomes: &[Outcome], floors: &Floors, language: Option<&str>) -> Rate {
    let mut rate = Rate { events: 0, total: 0 };
    for o in outcomes {
        if !(is_scored_positive(o) && o.held_out && o.top_is_expected()) {
            continue;
        }
        if language.is_some_and(|l| l != o.language) {
            continue;
        }
        if let Some(clears) = o.top_clears_floor(floors) {
            rate.total += 1;
            if !clears {
                rate.events += 1;
            }
        }
    }
    rate
}

/// Which held-out authored queries a confident-wrong rate counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Population {
    Positives,
    Absent,
    Combined,
}

/// Per-query confident-wrong indicator, `None` for a query outside the
/// held-out authored set or with no top hit to judge.
pub fn confident_wrong(o: &Outcome, floors: &Floors) -> Option<f64> {
    if o.mechanical || !o.held_out {
        return None;
    }
    let clears = o.top_clears_floor(floors)?;
    let wrong = if o.positive { clears && !o.top_is_expected() } else { clears };
    Some(if wrong { 1.0 } else { 0.0 })
}

pub fn confident_wrong_rate(outcomes: &[Outcome], floors: &Floors, population: Population) -> Rate {
    let mut rate = Rate { events: 0, total: 0 };
    for o in outcomes {
        let included = match population {
            Population::Positives => o.positive,
            Population::Absent => !o.positive,
            Population::Combined => true,
        };
        if !included {
            continue;
        }
        if let Some(v) = confident_wrong(o, floors) {
            rate.total += 1;
            if v > 0.0 {
                rate.events += 1;
            }
        }
    }
    rate
}

/// D7: recall@k a ranking with no information reaches on average, the mean
/// over queries of `min(1, k * |E| / N)`.
pub fn chance_recall(expected_sizes: &[usize], candidate_count: usize, k: usize) -> f64 {
    if expected_sizes.is_empty() || candidate_count == 0 {
        return 0.0;
    }
    let total: f64 = expected_sizes.iter().map(|&e| ((k * e) as f64 / candidate_count as f64).min(1.0)).sum();
    total / expected_sizes.len() as f64
}

/// A point estimate with one-sided 95% bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bound {
    pub point: f64,
    pub lower: f64,
    pub upper: f64,
}

/// Percentile bootstrap of the pooled mean, resampling queries with
/// replacement within each language group.
pub fn bootstrap(groups: &Groups, resamples: usize, seed: u64) -> Option<Bound> {
    let point = pooled_mean(groups)?;
    let groups: Vec<&Vec<f64>> = groups.values().filter(|v| !v.is_empty()).collect();
    let mut rng = Rng::new(seed);
    let mut stats = Vec::with_capacity(resamples);
    for _ in 0..resamples {
        let mut sum_of_means = 0.0;
        for values in &groups {
            let n = values.len();
            let mut total = 0.0;
            for _ in 0..n {
                total += values[rng.below(n)];
            }
            sum_of_means += total / n as f64;
        }
        stats.push(sum_of_means / groups.len() as f64);
    }
    stats.sort_by(f64::total_cmp);
    if stats.is_empty() {
        return Some(Bound { point, lower: point, upper: point });
    }
    let tail = (0.05 * stats.len() as f64).floor() as usize;
    Some(Bound { point, lower: stats[tail], upper: stats[stats.len() - 1 - tail] })
}

/// Per-query `candidate - reference` of `value`, grouped by language, over
/// the queries `keep` selects in both arms. Fails when the two arms did not
/// score the same query set, which would make the comparison unpaired.
pub fn paired_deltas(
    reference: &[Outcome],
    candidate: &[Outcome],
    keep: impl Fn(&Outcome) -> bool,
    value: impl Fn(&Outcome) -> Option<f64>,
) -> anyhow::Result<Groups> {
    let by_id: BTreeMap<&str, &Outcome> = candidate.iter().map(|o| (o.query_id.as_str(), o)).collect();
    if by_id.len() != reference.len() {
        anyhow::bail!("the two arms scored different query sets ({} vs {})", reference.len(), by_id.len());
    }
    let mut groups = Groups::new();
    for r in reference {
        let c = by_id
            .get(r.query_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("query {} is missing from the candidate arm", r.query_id))?;
        if !keep(r) {
            continue;
        }
        if let (Some(rv), Some(cv)) = (value(r), value(c)) {
            groups.entry(r.language.clone()).or_default().push(cv - rv);
        }
    }
    Ok(groups)
}

/// D5: share of paired authored positives where exactly one arm hits at `k`.
pub fn discordance(reference: &[Outcome], candidate: &[Outcome], k: usize) -> Option<f64> {
    let by_id: BTreeMap<&str, &Outcome> = candidate.iter().map(|o| (o.query_id.as_str(), o)).collect();
    let mut total = 0usize;
    let mut discordant = 0usize;
    for r in reference.iter().filter(|o| is_scored_positive(o)) {
        if let Some(c) = by_id.get(r.query_id.as_str()) {
            total += 1;
            if r.hit_at(k) != c.hit_at(k) {
                discordant += 1;
            }
        }
    }
    (total > 0).then(|| discordant as f64 / total as f64)
}

/// D7: whether a broken arm sits "clearly lower" than the reference on
/// pooled recall@10.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrokenArmCheck {
    pub gap_points: f64,
    pub ratio: f64,
    pub bounds_separate: bool,
    pub passes: bool,
}

pub const BROKEN_ARM_MIN_GAP: f64 = 0.20;
pub const BROKEN_ARM_MAX_RATIO: f64 = 0.25;

pub fn broken_arm_check(reference: Bound, broken: Bound) -> BrokenArmCheck {
    let gap = reference.point - broken.point;
    let ratio = if reference.point > 0.0 { broken.point / reference.point } else { f64::INFINITY };
    let bounds_separate = reference.lower > broken.upper;
    BrokenArmCheck {
        gap_points: gap * 100.0,
        ratio,
        bounds_separate,
        passes: gap >= BROKEN_ARM_MIN_GAP - 1e-12 && ratio <= BROKEN_ARM_MAX_RATIO + 1e-12 && bounds_separate,
    }
}

/// D7: the random arm's recall@10 must stay within 3x chance plus 2 points,
/// or the harness leaks order (ties, say) and the run is void.
pub fn random_arm_is_at_chance(random_recall: f64, chance: f64) -> bool {
    random_recall <= 3.0 * chance + 0.02 + 1e-12
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(id: &str, language: &str, rank: Option<usize>, top: Option<f64>) -> Outcome {
        Outcome {
            query_id: id.to_string(),
            corpus: "c".to_string(),
            language: language.to_string(),
            positive: true,
            mechanical: false,
            overlap: false,
            held_out: false,
            first_expected_rank: rank,
            top_score: top,
            top_language: top.map(|_| language.to_string()),
        }
    }

    /// Control: changing `hit_at`'s `rank <= k` to `rank < k` fails the
    /// rank-10 case; counting `None` as a hit fails the miss.
    #[test]
    fn recall_at_k_counts_a_hit_at_rank_k_and_not_below_it() {
        assert_eq!(outcome("a", "go", Some(10), None).hit_at(10), 1.0);
        assert_eq!(outcome("a", "go", Some(11), None).hit_at(10), 0.0);
        assert_eq!(outcome("a", "go", None, None).hit_at(10), 0.0);
        assert_eq!(outcome("a", "go", Some(1), None).hit_at(1), 1.0);
    }

    /// Control: dropping the `KEPT_HITS` cut, or returning 1/(rank+1), fails.
    #[test]
    fn reciprocal_rank_is_one_over_the_first_expected_rank() {
        assert_eq!(outcome("a", "go", Some(1), None).reciprocal_rank(), 1.0);
        assert_eq!(outcome("a", "go", Some(4), None).reciprocal_rank(), 0.25);
        assert_eq!(outcome("a", "go", Some(101), None).reciprocal_rank(), 0.0);
        assert_eq!(outcome("a", "go", None, None).reciprocal_rank(), 0.0);
    }

    /// Languages weigh the same: go has 1 query at 1.0, rust 3 at 0.0; the
    /// pooled mean is 0.5, not the per-query 0.25. Control: replacing
    /// `pooled_mean` with a flat mean over all values fails.
    #[test]
    fn the_pooled_mean_weighs_languages_equally() {
        let mut groups = Groups::new();
        groups.insert("go".into(), vec![1.0]);
        groups.insert("rust".into(), vec![0.0, 0.0, 0.0]);
        assert_eq!(pooled_mean(&groups), Some(0.5));
        assert_eq!(pooled_mean(&Groups::new()), None);
    }

    /// 100 scores 0.01..1.00: 3% may fall below, so the floor is the 4th
    /// smallest, 0.04. Control: `k` computed as `ceil`, or taking `s[k-1]`,
    /// or rounding to nearest instead of down, each fails one assertion.
    #[test]
    fn the_floor_is_the_largest_value_keeping_false_alarms_at_three_percent() {
        let scores: Vec<f64> = (1..=100).map(|i| i as f64 / 100.0).collect();
        assert_eq!(fit_floor(&scores, 0.03), Some(0.04));

        // 0.5948 must round down to 0.59, not to 0.60.
        let mut scores = vec![0.5948; 10];
        scores.extend(vec![0.9; 90]);
        assert_eq!(fit_floor(&scores, 0.03), Some(0.59));

        // Fewer than 34 scores: 3% of n is below one, so none may fall
        // below and the floor is the minimum.
        assert_eq!(fit_floor(&[0.7, 0.62, 0.8], 0.03), Some(0.62));
        assert_eq!(fit_floor(&[], 0.03), None);
    }

    /// At the fitted floor the fit set's own false-alarm rate is within 3%.
    #[test]
    fn a_fitted_floor_holds_its_promise_on_the_fit_set() {
        let mut rng = Rng::new(5);
        let scores: Vec<f64> = (0..357).map(|_| 0.4 + 0.5 * rng.next_f64()).collect();
        let floor = fit_floor(&scores, 0.03).unwrap();
        let below = scores.iter().filter(|&&s| s < floor).count();
        assert!(below as f64 <= 0.03 * scores.len() as f64, "{below} of {}", scores.len());
    }

    /// Only fit-half and mechanical positives whose top hit is right feed the
    /// floor. Control: fitting on held-out rows too, or on rows ranked
    /// second, moves the go floor off 0.80.
    #[test]
    fn floors_are_fitted_on_the_fit_half_only() {
        let mut fit = outcome("f", "go", Some(1), Some(0.80));
        fit.held_out = false;
        let mut held = outcome("h", "go", Some(1), Some(0.10));
        held.held_out = true;
        let second = outcome("s", "go", Some(2), Some(0.05));
        let mut mech = outcome("m", "go", Some(1), Some(0.90));
        mech.mechanical = true;
        mech.held_out = true; // mechanical rows are fit whatever their hash says
        let floors = fit_floors(&[fit, held, second, mech]);
        assert_eq!(floors.get("go"), Some(&0.80));
    }

    /// Control: counting rows whose top hit is wrong in the denominator, or
    /// testing `>` instead of `<` against the floor, fails.
    #[test]
    fn false_alarm_is_conditional_on_the_right_answer_ranked_first() {
        let floors: Floors = [("go".to_string(), 0.5)].into();
        let mut rows = vec![
            outcome("a", "go", Some(1), Some(0.4)), // alarm
            outcome("b", "go", Some(1), Some(0.6)), // fine
            outcome("c", "go", Some(2), Some(0.3)), // not ranked first: not counted
            outcome("d", "go", Some(1), Some(0.5)), // at the floor: fine
        ];
        for r in &mut rows {
            r.held_out = true;
        }
        let rate = false_alarm(&rows, &floors, None);
        assert_eq!(rate, Rate { events: 1, total: 3 });
        assert_eq!(false_alarm(&rows, &floors, Some("rust")), Rate { events: 0, total: 0 });
    }

    /// A positive is confidently wrong when a wrong top hit clears the
    /// floor; an absent query when anything clears it. Control: treating
    /// absent like positives (requiring a wrong top hit) fails the absent
    /// case, since an absent query has no expected rank.
    #[test]
    fn confident_wrong_by_population() {
        let floors: Floors = [("go".to_string(), 0.5)].into();
        let mut wrong = outcome("w", "go", Some(3), Some(0.7));
        let mut right = outcome("r", "go", Some(1), Some(0.9));
        let mut quiet = outcome("q", "go", Some(4), Some(0.3));
        let mut absent_loud = outcome("a", "go", None, Some(0.6));
        absent_loud.positive = false;
        let mut absent_quiet = outcome("b", "go", None, Some(0.2));
        absent_quiet.positive = false;
        let mut fit_row = outcome("f", "go", Some(3), Some(0.9));
        for r in [&mut wrong, &mut right, &mut quiet, &mut absent_loud, &mut absent_quiet] {
            r.held_out = true;
        }
        fit_row.held_out = false;
        let rows = vec![wrong, right, quiet, absent_loud, absent_quiet, fit_row];
        assert_eq!(confident_wrong_rate(&rows, &floors, Population::Positives), Rate { events: 1, total: 3 });
        assert_eq!(confident_wrong_rate(&rows, &floors, Population::Absent), Rate { events: 1, total: 2 });
        assert_eq!(confident_wrong_rate(&rows, &floors, Population::Combined), Rate { events: 2, total: 5 });
    }

    /// Control: dropping the `min(1, ..)` cap fails the second case.
    #[test]
    fn chance_recall_is_k_times_expected_over_candidates_capped_at_one() {
        assert!((chance_recall(&[1, 2], 1000, 10) - 0.015).abs() < 1e-12);
        assert_eq!(chance_recall(&[3], 20, 10), 1.0);
        assert_eq!(chance_recall(&[], 20, 10), 0.0);
    }

    /// A constant sample has zero-width bounds; a noisy one brackets its
    /// point, and the same seed reproduces the bounds exactly. Control:
    /// swapping `lower`/`upper` or resampling across languages (pooling all
    /// values into one group) fails.
    #[test]
    fn bootstrap_bounds_bracket_the_pooled_point_and_are_reproducible() {
        let mut constant = Groups::new();
        constant.insert("go".into(), vec![0.3; 50]);
        let b = bootstrap(&constant, 500, 1).unwrap();
        // Summing fifty 0.3s is not exactly 15.0, so compare within rounding.
        for v in [b.point, b.lower, b.upper] {
            assert!((v - 0.3).abs() < 1e-12, "{b:?}");
        }

        let mut rng = Rng::new(2);
        let mut noisy = Groups::new();
        noisy.insert("go".into(), (0..100).map(|_| if rng.next_f64() < 0.6 { 1.0 } else { 0.0 }).collect());
        noisy.insert("rust".into(), vec![0.0; 10]);
        let b = bootstrap(&noisy, 2000, 9).unwrap();
        assert!(b.lower < b.point && b.point < b.upper, "{b:?}");
        // rust is constant at 0, so every resample's pooled mean is half the
        // go resample's mean: bounds stay within [0, 0.5].
        assert!(b.lower >= 0.0 && b.upper <= 0.5, "{b:?}");
        assert_eq!(bootstrap(&noisy, 2000, 9).unwrap(), b);
    }

    /// Control: computing `reference - candidate` flips the sign; skipping
    /// the query-set check lets an unpaired comparison through.
    #[test]
    fn paired_deltas_are_candidate_minus_reference_and_require_the_same_queries() {
        let reference = vec![outcome("a", "go", Some(1), None), outcome("b", "go", None, None)];
        let candidate = vec![outcome("b", "go", Some(2), None), outcome("a", "go", Some(30), None)];
        let deltas =
            paired_deltas(&reference, &candidate, is_scored_positive, |o| Some(o.hit_at(10))).unwrap();
        let mut got = deltas["go"].clone();
        got.sort_by(f64::total_cmp);
        assert_eq!(got, vec![-1.0, 1.0]);

        let short = vec![outcome("a", "go", Some(1), None)];
        assert!(paired_deltas(&reference, &short, is_scored_positive, |o| Some(o.hit_at(10))).is_err());
    }

    #[test]
    fn discordance_counts_queries_where_exactly_one_arm_hits() {
        let reference = vec![
            outcome("a", "go", Some(1), None),
            outcome("b", "go", None, None),
            outcome("c", "go", Some(3), None),
            outcome("d", "go", None, None),
        ];
        let candidate = vec![
            outcome("a", "go", None, None),
            outcome("b", "go", Some(2), None),
            outcome("c", "go", Some(5), None),
            outcome("d", "go", None, None),
        ];
        assert_eq!(discordance(&reference, &candidate, 10), Some(0.5));
    }

    /// Each of the three D7 conditions can fail on its own. Control:
    /// dropping any one condition from `passes` lets its case through.
    #[test]
    fn a_broken_arm_must_be_lower_by_gap_ratio_and_bounds() {
        let reference = Bound { point: 0.60, lower: 0.55, upper: 0.65 };
        let clearly_lower = Bound { point: 0.05, lower: 0.03, upper: 0.08 };
        assert!(broken_arm_check(reference, clearly_lower).passes);

        let small_gap = Bound { point: 0.45, lower: 0.40, upper: 0.50 };
        assert!(!broken_arm_check(reference, small_gap).passes);

        let low_reference = Bound { point: 0.30, lower: 0.26, upper: 0.34 };
        let ratio_too_high = Bound { point: 0.09, lower: 0.07, upper: 0.10 };
        let check = broken_arm_check(low_reference, ratio_too_high);
        assert!(check.gap_points >= 20.0 && check.ratio > 0.25 && !check.passes, "{check:?}");

        let overlapping = Bound { point: 0.10, lower: 0.02, upper: 0.56 };
        let check = broken_arm_check(reference, overlapping);
        assert!(!check.bounds_separate && !check.passes, "{check:?}");
    }

    #[test]
    fn the_random_arm_may_reach_three_times_chance_plus_two_points() {
        assert!(random_arm_is_at_chance(0.05, 0.01));
        assert!(!random_arm_is_at_chance(0.06, 0.01));
    }
}
