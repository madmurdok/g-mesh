//! What `g-mesh plugins check` prints, as data first and text second.
//!
//! # Sections, not one flat list
//!
//! A [`Report`] is a list of [`Section`]s, each a list of [`CheckResult`]s,
//! rather than a single list of checks, because the kit is not finished
//! growing: GM-277 adds expectations (`--expect <expect.toml>` - "these are
//! the callers of `Server.Close`", answered by the same query code the MCP
//! tools use against the linked index this kit already builds). Those are a
//! different *kind* of result from the contract checks here - one per
//! expectation the fixture's author wrote, not one per rule every plugin is
//! held to - so they belong in a section of their own, appended by GM-277
//! without reshaping anything already here. The flag itself is deliberately
//! not declared until then: a flag that parses and does nothing would read
//! as "expectations passed".
//!
//! # A line format tests and humans can both read
//!
//! Every result renders as one line that starts with its outcome and its
//! check id - `FAIL  stream-order` - followed by indented detail lines. The
//! first two tokens are the stable part: the integration tests
//! (`core/tests/plugin_check.rs`) assert on exactly those, which is also what
//! a CI log grep would. The detail wording is free to improve.
//!
//! # Findings are capped per check
//!
//! A plugin with one systematic mistake (every edge resolved the wrong way)
//! would otherwise print one line per edge of the whole fixture. The first
//! [`MAX_FINDINGS_SHOWN`] are enough to name the offending ids and lines; the
//! rest are counted, never silently dropped.

use std::fmt::Write as _;
use std::path::PathBuf;

/// How many findings one check prints before summarizing the rest as a count
/// - see this module's doc comment.
const MAX_FINDINGS_SHOWN: usize = 12;

/// One check's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    /// Every finding names the offending ids and where they came from (which
    /// bulk run and NDJSON line, or which control-plane step).
    Fail(Vec<String>),
    /// The check did not run, and the reason says why: not applicable to this
    /// plugin's declared capabilities, not reached because an earlier step of
    /// the session failed, or not instrumented. Never counted as a pass - see
    /// `checks`' doc comment on the semantic-engine marker for why "no
    /// evidence either way" must not read as "conformant".
    Skip(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Stable, dotted identifier (`id-stability.whitespace-edit`) - what the
    /// README documents and what tests match on.
    pub id: &'static str,
    pub outcome: Outcome,
    /// Non-failing observations attached to this check - today only the
    /// legacy-v1 wire-field warning on `shape` (see `checks::shape`).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: &'static str,
    pub results: Vec<CheckResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub language: String,
    pub plugin_dir: PathBuf,
    pub fixture: PathBuf,
    /// Facts about this run a reader needs to interpret a failure - which
    /// file was edited and how, which timeouts applied.
    pub notes: Vec<String>,
    pub sections: Vec<Section>,
}

impl Report {
    /// Every result across every section, in report order.
    pub fn results(&self) -> impl Iterator<Item = &CheckResult> {
        self.sections.iter().flat_map(|section| section.results.iter())
    }

    /// Whether any check failed. Skips and warnings do not fail a run.
    pub fn failed(&self) -> bool {
        self.results().any(|result| matches!(result.outcome, Outcome::Fail(_)))
    }

    /// The failing checks' ids, in report order.
    pub fn failed_ids(&self) -> Vec<&'static str> {
        self.results().filter(|r| matches!(r.outcome, Outcome::Fail(_))).map(|r| r.id).collect()
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ =
            writeln!(out, "g-mesh plugins check: {} plugin at {}", self.language, self.plugin_dir.display());
        let _ = writeln!(
            out,
            "fixture: {} (run against a scratch copy; the fixture itself is never modified)",
            self.fixture.display()
        );
        for note in &self.notes {
            let _ = writeln!(out, "{note}");
        }

        let (mut passed, mut failed, mut skipped) = (0, 0, 0);
        for section in &self.sections {
            let _ = writeln!(out, "\n{}:", section.title);
            let width = section.results.iter().map(|r| r.id.len()).max().unwrap_or(0);
            for result in &section.results {
                match &result.outcome {
                    Outcome::Pass => {
                        passed += 1;
                        let _ = writeln!(out, "  PASS  {}", result.id);
                    }
                    Outcome::Skip(reason) => {
                        skipped += 1;
                        let _ = writeln!(out, "  SKIP  {:width$}  {reason}", result.id);
                    }
                    Outcome::Fail(findings) => {
                        failed += 1;
                        let _ = writeln!(out, "  FAIL  {}", result.id);
                        for finding in findings.iter().take(MAX_FINDINGS_SHOWN) {
                            let _ = writeln!(out, "          - {finding}");
                        }
                        if findings.len() > MAX_FINDINGS_SHOWN {
                            let _ = writeln!(
                                out,
                                "          ... and {} more finding(s)",
                                findings.len() - MAX_FINDINGS_SHOWN
                            );
                        }
                    }
                }
                for warning in &result.warnings {
                    let _ = writeln!(out, "  WARN  {:width$}  {warning}", result.id);
                }
            }
        }

        let verdict = if failed > 0 { "FAIL" } else { "PASS" };
        let _ = writeln!(out, "\nresult: {verdict} ({failed} failed, {passed} passed, {skipped} skipped)");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(results: Vec<CheckResult>) -> Report {
        Report {
            language: "fake".to_string(),
            plugin_dir: PathBuf::from("/plugins/fake"),
            fixture: PathBuf::from("/fixture"),
            notes: vec!["edit target: a.fk".to_string()],
            sections: vec![Section { title: "checks", results }],
        }
    }

    fn result(id: &'static str, outcome: Outcome) -> CheckResult {
        CheckResult { id, outcome, warnings: Vec::new() }
    }

    #[test]
    fn a_report_with_only_passes_and_skips_has_not_failed() {
        let report = report(vec![
            result("shape", Outcome::Pass),
            result("capabilities.semantic-engine-lazy", Outcome::Skip("not instrumented".to_string())),
        ]);
        assert!(!report.failed());
        let text = report.render();
        assert!(text.contains("  PASS  shape\n"), "{text}");
        assert!(text.contains("  SKIP  capabilities.semantic-engine-lazy  not instrumented"), "{text}");
        assert!(text.contains("result: PASS (0 failed, 1 passed, 1 skipped)"), "{text}");
    }

    #[test]
    fn a_failing_check_prints_its_findings_and_caps_the_rest_as_a_count() {
        let findings = (0..MAX_FINDINGS_SHOWN + 3).map(|i| format!("finding {i}")).collect();
        let report =
            report(vec![result("stream-order", Outcome::Fail(findings)), result("shape", Outcome::Pass)]);
        assert!(report.failed());
        assert_eq!(report.failed_ids(), vec!["stream-order"]);
        let text = report.render();
        assert!(text.contains("  FAIL  stream-order\n          - finding 0\n"), "{text}");
        assert!(!text.contains(&format!("finding {MAX_FINDINGS_SHOWN}")), "{text}");
        assert!(text.contains("... and 3 more finding(s)"), "{text}");
        assert!(text.contains("result: FAIL (1 failed, 1 passed, 0 skipped)"), "{text}");
    }

    #[test]
    fn warnings_render_without_failing_the_run() {
        let mut shape = result("shape", Outcome::Pass);
        shape.warnings.push("legacy v1 fields".to_string());
        let report = report(vec![shape]);
        assert!(!report.failed());
        assert!(report.render().contains("  WARN  shape  legacy v1 fields"));
    }
}
