//! D9 of `docs/architecture/embedding-eval.md`, applied mechanically: the
//! quality gates, the cost gates, and the verdict they add up to. Deltas are
//! fractions (0.05 = 5 points); bounds are one-sided 95%.

use std::collections::BTreeMap;

use super::config::Role;
use super::metrics::Bound;

#[derive(Debug, Clone, PartialEq)]
pub struct Gate {
    pub id: &'static str,
    pub passed: bool,
    pub detail: String,
}

/// What the quality gates read, candidate against reference.
#[derive(Debug, Clone)]
pub struct QualityEvidence {
    pub recall10_delta: Bound,
    pub mrr_delta: Bound,
    /// Point estimate per language.
    pub recall10_delta_by_language: BTreeMap<String, f64>,
    /// Held-out, positives and absent combined.
    pub confident_wrong_delta: Bound,
    /// Held-out false alarm, each arm at its own floors, paired over the
    /// positives both arms rank right first; pooled like Q4.
    pub false_alarm_delta: Bound,
    /// Point estimate per language, reported beside Q5 and not gated.
    pub false_alarm_delta_by_language: BTreeMap<String, f64>,
    /// The candidate's own held-out false-alarm rate at its own floors,
    /// reported and not gated (D6 fits it to 3% on the fit half only).
    pub false_alarm_by_language: BTreeMap<String, Option<f64>>,
}

/// D11's measurements for one variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Costs {
    pub pass_seconds: f64,
    pub max_rss_bytes: f64,
    pub model_bytes: f64,
    pub query_latency_ms: f64,
}

const EPS: f64 = 1e-12;

fn pts(x: f64) -> String {
    format!("{:+.1} pts", x * 100.0)
}

pub fn quality_gates(role: Role, e: &QualityEvidence) -> Vec<Gate> {
    let mut gates = Vec::new();
    match role {
        Role::Quality => {
            gates.push(Gate {
                id: "Q1",
                passed: e.recall10_delta.lower > 0.0,
                detail: format!("recall@10 lower bound {} must be > 0", pts(e.recall10_delta.lower)),
            });
            gates.push(Gate {
                id: "Q2",
                passed: e.mrr_delta.lower > 0.0,
                detail: format!("MRR lower bound {:+.3} must be > 0", e.mrr_delta.lower),
            });
        }
        _ => {
            gates.push(Gate {
                id: "Q1",
                passed: e.recall10_delta.point >= -0.02 - EPS && e.recall10_delta.lower >= -0.05 - EPS,
                detail: format!(
                    "recall@10 delta {} (>= -2.0), lower {} (>= -5.0)",
                    pts(e.recall10_delta.point),
                    pts(e.recall10_delta.lower)
                ),
            });
            gates.push(Gate {
                id: "Q2",
                passed: e.mrr_delta.point >= -0.02 - EPS && e.mrr_delta.lower >= -0.05 - EPS,
                detail: format!(
                    "MRR delta {:+.3} (>= -0.02), lower {:+.3} (>= -0.05)",
                    e.mrr_delta.point, e.mrr_delta.lower
                ),
            });
        }
    }
    let worst = e.recall10_delta_by_language.iter().min_by(|a, b| a.1.total_cmp(b.1));
    gates.push(Gate {
        id: "Q3",
        passed: !e.recall10_delta_by_language.is_empty()
            && e.recall10_delta_by_language.values().all(|&d| d >= -0.10 - EPS),
        detail: match worst {
            Some((language, d)) => format!("worst language {language} {} (>= -10 each)", pts(*d)),
            None => "no per-language deltas".to_string(),
        },
    });
    gates.push(Gate {
        id: "Q4",
        passed: e.confident_wrong_delta.point <= EPS && e.confident_wrong_delta.upper <= 0.05 + EPS,
        detail: format!(
            "confident-wrong delta {} (<= 0), upper {} (<= +5)",
            pts(e.confident_wrong_delta.point),
            pts(e.confident_wrong_delta.upper)
        ),
    });
    let or_none = |v: Vec<String>| if v.is_empty() { "none".to_string() } else { v.join(", ") };
    let deltas = e.false_alarm_delta_by_language.iter().map(|(l, d)| format!("{l} {}", pts(*d))).collect();
    let rates = e
        .false_alarm_by_language
        .iter()
        .map(|(l, r)| match r {
            Some(r) => format!("{l} {:.1}%", r * 100.0),
            None => format!("{l} -"),
        })
        .collect();
    gates.push(Gate {
        id: "Q5",
        passed: e.false_alarm_delta.point <= EPS && e.false_alarm_delta.upper <= 0.05 + EPS,
        detail: format!(
            "false-alarm delta {} (<= 0), upper {} (<= +5); delta by language {}; own rate {}",
            pts(e.false_alarm_delta.point),
            pts(e.false_alarm_delta.upper),
            or_none(deltas),
            or_none(rates)
        ),
    });
    gates
}

pub fn cost_gates(role: Role, candidate: &Costs, reference: &Costs) -> Vec<Gate> {
    let ratio = |c: f64, r: f64| if r > 0.0 { c / r } else { f64::INFINITY };
    let pass = ratio(candidate.pass_seconds, reference.pass_seconds);
    let rss = ratio(candidate.max_rss_bytes, reference.max_rss_bytes);
    let size = ratio(candidate.model_bytes, reference.model_bytes);
    let latency = ratio(candidate.query_latency_ms, reference.query_latency_ms);
    match role {
        Role::Quality => vec![Gate {
            id: "C-quality",
            passed: pass <= 1.5 + EPS && rss <= 1.2 + EPS && candidate.model_bytes < 1e9,
            detail: format!(
                "pass {pass:.2}x (<= 1.5), RSS {rss:.2}x (<= 1.2), size {:.0} MB (< 1000)",
                candidate.model_bytes / 1e6
            ),
        }],
        _ => vec![
            Gate {
                id: "C-win",
                passed: pass <= 0.60 + EPS || size <= 0.50 + EPS || rss <= 0.70 + EPS,
                detail: format!("pass {pass:.2}x (<= 0.60) or size {size:.2}x (<= 0.50) or RSS {rss:.2}x (<= 0.70)"),
            },
            Gate {
                id: "C-no-worse",
                passed: [pass, rss, size, latency].iter().all(|&x| x <= 1.10 + EPS),
                detail: format!(
                    "pass {pass:.2}x, RSS {rss:.2}x, size {size:.2}x, query latency {latency:.2}x (each <= 1.10)"
                ),
            },
        ],
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Every gate passed: eligible for the switch (still subject to D10).
    Pass,
    /// A gate failed: keep the reference.
    Fail(Vec<&'static str>),
    /// Only Q1's bound failed, in the pre-registered second-stage window
    /// [-7, -5): the one allowed extension of the query set applies.
    SecondStage,
    /// No verdict: validity failed, or the costs were not measured.
    Undecided(String),
}

pub fn decide(
    role: Role,
    valid: Result<(), String>,
    recall10_delta: Bound,
    quality: &[Gate],
    cost: Option<&[Gate]>,
) -> Verdict {
    if let Err(reason) = valid {
        return Verdict::Undecided(reason);
    }
    let failed: Vec<&'static str> =
        quality.iter().chain(cost.unwrap_or(&[])).filter(|g| !g.passed).map(|g| g.id).collect();
    let second_stage_window = recall10_delta.lower >= -0.07 - EPS && recall10_delta.lower < -0.05 - EPS;
    if role == Role::Cost && failed == ["Q1"] && recall10_delta.point >= -0.02 - EPS && second_stage_window {
        return Verdict::SecondStage;
    }
    if !failed.is_empty() {
        return Verdict::Fail(failed);
    }
    match cost {
        Some(_) => Verdict::Pass,
        None => Verdict::Undecided("quality gates pass; costs (D11) not measured".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(point: f64, lower: f64, upper: f64) -> Bound {
        Bound { point, lower, upper }
    }

    fn evidence() -> QualityEvidence {
        QualityEvidence {
            recall10_delta: bound(-0.01, -0.04, 0.02),
            mrr_delta: bound(-0.01, -0.03, 0.01),
            recall10_delta_by_language: [("go".to_string(), -0.05), ("rust".to_string(), 0.02)].into(),
            confident_wrong_delta: bound(-0.01, -0.03, 0.02),
            false_alarm_delta: bound(0.0, -0.02, 0.02),
            false_alarm_delta_by_language: [("go".to_string(), 0.0), ("rust".to_string(), 0.0)].into(),
            false_alarm_by_language: [("go".to_string(), Some(0.02)), ("rust".to_string(), Some(0.0))].into(),
        }
    }

    fn passed(gates: &[Gate], id: &str) -> bool {
        gates.iter().find(|g| g.id == id).unwrap().passed
    }

    /// Each boundary sits exactly on its D9 threshold and passes; a hair
    /// beyond it fails. The comparisons carry an `EPS` tolerance, so a value
    /// of exactly -0.02 passes under `>` too; the second block therefore
    /// sits each value on the computed threshold itself (`-0.02 - EPS`,
    /// `EPS`, ...), where only the inclusive comparison passes. Control:
    /// turning any Q1-Q5 `>=` into `>` (or `<=` into `<`) fails that block;
    /// flipping the sign of a threshold fails one of the others.
    #[test]
    fn cost_candidate_quality_gates_hold_at_their_thresholds() {
        let mut on_eps = evidence();
        on_eps.recall10_delta = bound(-0.02 - EPS, -0.05 - EPS, 0.0);
        on_eps.mrr_delta = bound(-0.02 - EPS, -0.05 - EPS, 0.0);
        on_eps.recall10_delta_by_language.insert("python".into(), -0.10 - EPS);
        on_eps.confident_wrong_delta = bound(EPS, -0.02, 0.05 + EPS);
        on_eps.false_alarm_delta = bound(EPS, -0.02, 0.05 + EPS);
        let gates = quality_gates(Role::Cost, &on_eps);
        assert!(gates.iter().all(|g| g.passed), "{gates:?}");

        let mut e = evidence();
        e.recall10_delta = bound(-0.02, -0.05, 0.0);
        e.mrr_delta = bound(-0.02, -0.05, 0.0);
        e.recall10_delta_by_language.insert("python".into(), -0.10);
        e.confident_wrong_delta = bound(0.0, -0.02, 0.05);
        e.false_alarm_delta = bound(0.0, -0.02, 0.05);
        let gates = quality_gates(Role::Cost, &e);
        assert!(gates.iter().all(|g| g.passed), "{gates:?}");

        e.recall10_delta = bound(-0.021, -0.04, 0.0);
        assert!(!passed(&quality_gates(Role::Cost, &e), "Q1"));
        e.recall10_delta = bound(-0.01, -0.051, 0.0);
        assert!(!passed(&quality_gates(Role::Cost, &e), "Q1"));
        e.recall10_delta_by_language.insert("python".into(), -0.101);
        assert!(!passed(&quality_gates(Role::Cost, &e), "Q3"));
        e.confident_wrong_delta = bound(0.001, -0.02, 0.03);
        assert!(!passed(&quality_gates(Role::Cost, &e), "Q4"));
        e.confident_wrong_delta = bound(0.0, -0.02, 0.051);
        assert!(!passed(&quality_gates(Role::Cost, &e), "Q4"));
        e.false_alarm_delta = bound(0.001, -0.02, 0.03);
        assert!(!passed(&quality_gates(Role::Cost, &e), "Q5"));
        e.false_alarm_delta = bound(0.0, -0.02, 0.051);
        assert!(!passed(&quality_gates(Role::Cost, &e), "Q5"));
        e.false_alarm_delta = bound(f64::NAN, f64::NAN, f64::NAN);
        assert!(!passed(&quality_gates(Role::Cost, &e), "Q5"));
    }

    /// Q5 is relative to the reference: 20 held-out go positives ranked
    /// right first, 3 of them below the floor (15% false alarm, jina's
    /// order of magnitude). A candidate equal to R passes; one with 4 more
    /// alarms (+20 points) fails. Control: restoring the absolute check
    /// (`false_alarm_by_language` all <= 3%) fails the equal candidate.
    #[test]
    fn q5_false_alarm_is_judged_against_the_reference_not_against_3_percent() {
        use super::super::metrics::{self, Floors, Outcome};
        let arm = |alarms: usize| -> Vec<Outcome> {
            (0..20)
                .map(|i| Outcome {
                    query_id: format!("q{i}"),
                    corpus: "c".to_string(),
                    language: "go".to_string(),
                    positive: true,
                    mechanical: false,
                    overlap: false,
                    held_out: true,
                    first_expected_rank: Some(1),
                    top_score: Some(if i < alarms { 0.40 } else { 0.70 }),
                    top_language: Some("go".to_string()),
                })
                .collect()
        };
        let floors: Floors = [("go".to_string(), 0.50)].into();
        let reference = arm(3);
        let q5 = |candidate: &[Outcome]| {
            let groups = metrics::paired_at_own_floors(
                &reference,
                &floors,
                candidate,
                &floors,
                metrics::false_alarm_indicator,
            );
            let mut e = evidence();
            e.false_alarm_delta = metrics::bootstrap(&groups, 2000, 398).unwrap();
            e.false_alarm_by_language =
                [("go".to_string(), metrics::false_alarm(candidate, &floors, Some("go")).value())].into();
            quality_gates(Role::Cost, &e).into_iter().find(|g| g.id == "Q5").unwrap()
        };

        let equal = arm(3);
        assert_eq!(metrics::false_alarm(&equal, &floors, None).value(), Some(0.15));
        let gate = q5(&equal);
        assert!(gate.passed, "{gate:?}");

        let worse = arm(7);
        let gate = q5(&worse);
        assert!(!gate.passed, "{gate:?}");
    }

    /// A quality candidate must be better, not merely not worse. Control:
    /// applying the cost rule to the quality role passes this evidence.
    #[test]
    fn a_quality_candidate_needs_positive_lower_bounds() {
        let e = evidence();
        let gates = quality_gates(Role::Quality, &e);
        assert!(!passed(&gates, "Q1") && !passed(&gates, "Q2"));
        let mut better = e.clone();
        better.recall10_delta = bound(0.06, 0.01, 0.10);
        better.mrr_delta = bound(0.05, 0.01, 0.09);
        assert!(quality_gates(Role::Quality, &better).iter().all(|g| g.passed));
    }

    #[test]
    fn a_cost_candidate_must_win_somewhere_and_lose_nowhere() {
        let reference =
            Costs { pass_seconds: 600.0, max_rss_bytes: 1.5e9, model_bytes: 6.4e8, query_latency_ms: 20.0 };
        let small =
            Costs { pass_seconds: 200.0, max_rss_bytes: 4e8, model_bytes: 1.3e8, query_latency_ms: 5.0 };
        assert!(cost_gates(Role::Cost, &small, &reference).iter().all(|g| g.passed));

        let no_win =
            Costs { pass_seconds: 500.0, max_rss_bytes: 1.4e9, model_bytes: 6e8, query_latency_ms: 20.0 };
        assert!(!passed(&cost_gates(Role::Cost, &no_win, &reference), "C-win"));

        let slow_queries = Costs { query_latency_ms: 23.0, ..small };
        assert!(!passed(&cost_gates(Role::Cost, &slow_queries, &reference), "C-no-worse"));

        let big =
            Costs { pass_seconds: 800.0, max_rss_bytes: 1.7e9, model_bytes: 1.2e9, query_latency_ms: 30.0 };
        assert!(!passed(&cost_gates(Role::Quality, &big, &reference), "C-quality"));
    }

    /// Control: dropping the window check sends a -9 point bound to the
    /// second stage; dropping the "only Q1" check sends a Q3 failure there.
    #[test]
    fn the_second_stage_is_only_for_a_q1_bound_in_its_window() {
        let q1_only = vec![
            Gate { id: "Q1", passed: false, detail: String::new() },
            Gate { id: "Q2", passed: true, detail: String::new() },
        ];
        let in_window = bound(-0.01, -0.06, 0.03);
        assert_eq!(decide(Role::Cost, Ok(()), in_window, &q1_only, None), Verdict::SecondStage);
        let below_window = bound(-0.01, -0.09, 0.03);
        assert_eq!(decide(Role::Cost, Ok(()), below_window, &q1_only, None), Verdict::Fail(vec!["Q1"]));
        let mut with_q3 = q1_only.clone();
        with_q3.push(Gate { id: "Q3", passed: false, detail: String::new() });
        assert_eq!(decide(Role::Cost, Ok(()), in_window, &with_q3, None), Verdict::Fail(vec!["Q1", "Q3"]));
    }

    #[test]
    fn validity_failure_and_missing_costs_give_no_verdict() {
        let ok = vec![Gate { id: "Q1", passed: true, detail: String::new() }];
        let b = bound(0.0, -0.01, 0.01);
        assert!(matches!(decide(Role::Cost, Err("broken arms".into()), b, &ok, None), Verdict::Undecided(_)));
        assert!(matches!(decide(Role::Cost, Ok(()), b, &ok, None), Verdict::Undecided(_)));
        let cost = vec![Gate { id: "C-win", passed: true, detail: String::new() }];
        assert_eq!(decide(Role::Cost, Ok(()), b, &ok, Some(&cost)), Verdict::Pass);
    }
}
