use super::*;

#[test]
fn identical_sets_pass_regardless_of_order() {
    let a: BTreeSet<String> = ["a", "b"].into_iter().map(String::from).collect();
    let b: BTreeSet<String> = ["b", "a"].into_iter().map(String::from).collect();
    assert_eq!(set_diff_outcome(&a, &b), Outcome::Pass);
}

/// The diff test's core assertion, in miniature: one missing, one extra
/// entry, both named in the findings.
#[test]
fn a_mismatched_set_names_both_the_missing_and_the_extra_entry() {
    let expected: BTreeSet<String> = ["src/a.ts:f", "src/b.ts:g"].into_iter().map(String::from).collect();
    let actual: BTreeSet<String> = ["src/a.ts:f", "src/c.ts:h"].into_iter().map(String::from).collect();
    let Outcome::Fail(findings) = set_diff_outcome(&expected, &actual) else {
        panic!("expected a failure");
    };
    let text = findings.join("\n");
    assert!(text.contains("missing (expected, not found): src/b.ts:g"), "{text}");
    assert!(text.contains("extra (found, not expected): src/c.ts:h"), "{text}");
    assert!(text.contains("expected: {src/a.ts:f, src/b.ts:g}"), "{text}");
    assert!(text.contains("actual:   {src/a.ts:f, src/c.ts:h}"), "{text}");
}

#[test]
fn a_real_result_page_is_not_mistaken_for_a_candidate_page() {
    let page = serde_json::json!({
        "anchor": {"id": "n1", "qualifiedName": "f", "kind": "Function", "filePath": "a.ts", "startLine": 1},
        "results": [],
        "hasMore": false,
        "nextCursor": null,
        "allUnresolved": false,
    });
    assert!(!is_candidate_page(&page));
}

/// The regression this module actually hit end to end: `find_definition`'s
/// own direct, unambiguous answer (`DefinitionNode`) echoes a top-level
/// `resolvedBy` too - "Name"/"QualifiedName"/"Id" - but has no top-level
/// `results` array, unlike a real candidate page. Checking `resolvedBy`
/// alone misclassified this shape as a candidate page and refused every
/// unambiguous `[[definition]]` expectation.
#[test]
fn a_find_definition_direct_answer_is_not_mistaken_for_a_candidate_page() {
    let node = serde_json::json!({
        "id": "n1",
        "kind": "Function",
        "name": "format",
        "qualifiedName": "format",
        "filePath": "src/overload.ts",
        "startLine": 1,
        "startCol": 0,
        "endLine": 3,
        "endCol": 1,
        "resolvedBy": "qualifiedName",
    });
    assert!(!is_candidate_page(&node));
}

#[test]
fn a_candidate_page_is_recognized_by_its_top_level_resolved_by() {
    let page = serde_json::json!({
        "ambiguous": true,
        "resolvedBy": "nameAmbiguous",
        "results": [{"id": "n1", "qualifiedName": "f", "filePath": "a.ts", "kind": "Function"}],
        "hasMore": false,
        "nextCursor": null,
    });
    assert!(is_candidate_page(&page));
}

#[test]
fn pick_candidate_refuses_without_a_file_even_with_one_candidate() {
    let page = serde_json::json!({
        "resolvedBy": "nameAmbiguous",
        "results": [{"id": "n1", "qualifiedName": "f", "filePath": "a.ts", "kind": "Function"}],
    });
    assert!(pick_candidate(&page, None).is_err());
}

#[test]
fn pick_candidate_narrows_by_file_to_exactly_one() {
    let page = serde_json::json!({
        "resolvedBy": "nameAmbiguous",
        "results": [
            {"id": "n1", "qualifiedName": "f", "filePath": "a.ts", "kind": "Function"},
            {"id": "n2", "qualifiedName": "f", "filePath": "b.ts", "kind": "Function"},
        ],
    });
    let picked = pick_candidate(&page, Some("b.ts")).unwrap();
    assert_eq!(picked.get("id").and_then(Value::as_str), Some("n2"));
}

#[test]
fn pick_candidate_refuses_a_file_matching_more_than_one_candidate() {
    let page = serde_json::json!({
        "resolvedBy": "nameAmbiguous",
        "results": [
            {"id": "n1", "qualifiedName": "f", "filePath": "a.ts", "kind": "Function"},
            {"id": "n2", "qualifiedName": "f", "filePath": "a.ts", "kind": "Function"},
        ],
    });
    assert!(pick_candidate(&page, Some("a.ts")).is_err());
}

#[test]
fn rows_to_set_blanks_the_qualified_name_of_a_file_level_row() {
    let value = serde_json::json!({
        "results": [
            {"filePath": "a.ts", "qualifiedName": "f", "resolved": true},
            {"filePath": "b.ts", "resolved": true},
        ],
    });
    let set: BTreeSet<String> = rows_to_list(&value).into_iter().collect();
    assert!(set.contains("a.ts:f"), "{set:?}");
    assert!(set.contains("b.ts:"), "{set:?}");
}

/// Decision 11's row half, which is the whole of how a row's `resolved`
/// bit becomes assertable: a confirmed row is spelled exactly as it was
/// before GM-386, so every already-written fixture entry keeps passing
/// and thereby asserts the healthy case; an unconfirmed one takes the
/// marker and no longer matches the row a fixture already names. A row
/// with no `resolved` field at all is read as unconfirmed rather than
/// assumed healthy.
#[test]
fn an_unconfirmed_row_is_marked_and_a_confirmed_one_is_spelled_as_before() {
    let value = serde_json::json!({
        "results": [
            {"filePath": "a.rs", "qualifiedName": "f", "resolved": true},
            {"filePath": "b.rs", "qualifiedName": "g", "resolved": false},
            {"filePath": "c.rs", "qualifiedName": "h"},
        ],
    });
    let rows = rows_to_list(&value);
    assert_eq!(rows, vec!["a.rs:f", "unresolved:b.rs:g", "unresolved:c.rs:h"], "{rows:?}");
}

/// Decision 11's response half. `false` is the only shape that passes:
/// `true` is the page built entirely from edges the linker could not
/// confirm, and a missing field is a result page that stopped declaring
/// a marker every one of them declares non-optional.
#[test]
fn all_unresolved_is_asserted_in_every_one_of_its_three_states() {
    assert!(all_unresolved_finding(&serde_json::json!({"allUnresolved": false})).is_none());

    let flagged = all_unresolved_finding(&serde_json::json!({"allUnresolved": true}))
        .expect("a wholly unconfirmed page must fail the entry");
    assert!(flagged.contains("allUnresolved: true"), "{flagged}");

    let missing = all_unresolved_finding(&serde_json::json!({"results": []}))
        .expect("a page with no marker at all must fail the entry");
    assert!(missing.contains("no allUnresolved field"), "{missing}");
}

/// Decision 12, both ways round - the property that makes every
/// already-written entry assert something without being edited is the
/// second arm: an entry that says nothing requires the response to carry
/// no tally.
#[test]
fn the_files_tally_is_an_assertion_in_both_of_its_states() {
    let with_tally = serde_json::json!({
        "results": [],
        "files": [{"path": "src/main.ts", "refs": 2}, {"path": "src/math.ts", "refs": 1}],
    });
    let without = serde_json::json!({"results": []});
    let expected = ["src/main.ts:2".to_string(), "src/math.ts:1".to_string()];

    assert!(files_finding(&with_tally, Some(&expected)).is_none());
    assert!(files_finding(&without, None).is_none());

    let unexpected = files_finding(&with_tally, None).expect("an unasserted tally must fail the entry");
    assert!(unexpected.contains("expected no files tally"), "{unexpected}");
    assert!(unexpected.contains("src/main.ts:2"), "{unexpected}");

    let absent = files_finding(&without, Some(&expected)).expect("a missing tally must fail the entry");
    assert!(absent.contains("no files tally at all"), "{absent}");

    // The count is the part the set of rows cannot say - GM-386's whole
    // reason for asserting the tally rather than only its presence.
    let wrong_count = files_finding(&with_tally, Some(&["src/main.ts:3".to_string()]))
        .expect("a count that moved must fail the entry");
    assert!(wrong_count.contains("files tally mismatch"), "{wrong_count}");
}

/// Decision 13, both ways round, plus the two shapes that must never be
/// compared as if whole: a count that moved while the tally did not, and
/// a tally the response itself says was cut.
#[test]
fn excluded_references_is_an_assertion_in_both_of_its_states() {
    let disclosed = serde_json::json!({
        "results": [],
        "excludedReferences": {
            "count": 2,
            "files": [{"path": "a.rs", "refs": 1}, {"path": "b.rs", "refs": 1}],
            "hint": "...",
        },
    });
    let silent = serde_json::json!({"results": []});
    let expected = ExcludedExpectation { count: 2, files: vec!["a.rs:1".to_string(), "b.rs:1".to_string()] };

    assert!(excluded_references_finding(&disclosed, Some(&expected)).is_none());
    assert!(excluded_references_finding(&silent, None).is_none());

    let unexpected =
        excluded_references_finding(&disclosed, None).expect("an unasserted disclosure must fail the entry");
    assert!(unexpected.contains("expected no excludedReferences block"), "{unexpected}");

    // The arm that catches a plugin dropping a usage shape it used to
    // emit a REFERENCES edge for: the caller set does not move, so
    // nothing else in this file would notice.
    let vanished = excluded_references_finding(&silent, Some(&expected))
        .expect("a disclosure that stopped being made must fail the entry");
    assert!(vanished.contains("carries no excludedReferences block"), "{vanished}");

    let undercount = ExcludedExpectation { count: 3, files: expected.files.clone() };
    let wrong = excluded_references_finding(&disclosed, Some(&undercount))
        .expect("a count that disagrees must fail the entry");
    assert!(wrong.contains("expected count = 3"), "{wrong}");
    assert!(wrong.contains("actual count = 2"), "{wrong}");

    let mut truncated = disclosed.clone();
    truncated["excludedReferences"]["filesTruncated"] = serde_json::json!(true);
    let cut = excluded_references_finding(&truncated, Some(&expected))
        .expect("a cut tally must never be compared as if whole");
    assert!(cut.contains("filesTruncated"), "{cut}");
}

/// Decisions 12 and 13, the parsing half: both keys are optional, and
/// `deny_unknown_fields` reaches inside the inline table too - so a
/// misspelled sub-key is a parse error rather than an assertion that
/// silently checks less than it looks like it does.
#[test]
fn the_new_keys_parse_and_a_typo_inside_them_is_a_hard_error() {
    let file: ExpectFile = toml::from_str(
        "[[callers]]\nsymbol = \"f\"\nexpect = []\n\n\
         [[callers]]\nsymbol = \"g\"\nexpect = []\nfiles = [\"a.rs:2\"]\n\
         excluded_references = { count = 1, files = [\"b.rs:1\"] }\n",
    )
    .unwrap();
    assert_eq!(file.callers[0].files, None);
    assert!(file.callers[0].excluded_references.is_none());
    assert_eq!(file.callers[1].files.as_deref(), Some(["a.rs:2".to_string()].as_slice()));
    let block = file.callers[1].excluded_references.as_ref().expect("the block parses");
    assert_eq!(block.count, 1);
    assert_eq!(block.files, vec!["b.rs:1".to_string()]);

    for (source, needle) in [
        (
            "[[callers]]\nsymbol = \"f\"\nexpect = []\nexcluded_references = { count = 1, \
             file = [\"b.rs:1\"] }\n",
            "file",
        ),
        // `count` is required: a block that only listed files would
        // assert the capped half and not the exact one.
        ("[[callers]]\nsymbol = \"f\"\nexpect = []\nexcluded_references = { files = [] }\n", "count"),
    ] {
        let err = toml::from_str::<ExpectFile>(source).unwrap_err().to_string();
        assert!(err.contains(needle), "expected {needle:?} to be rejected, got: {err}");
    }
}

#[test]
fn import_rows_to_set_prefixes_a_targetless_row_with_container() {
    let value = serde_json::json!({
        "results": [
            {"filePath": "a.ts", "kind": "File"},
            {"qualifiedName": "react", "kind": "Module"},
        ],
    });
    let set: BTreeSet<String> = import_rows(&value).into_iter().collect();
    assert!(set.contains("a.ts"), "{set:?}");
    assert!(set.contains("container:react"), "{set:?}");
}

/// Decision 7's parsing half: `[[importers]]` is its own list, `via_module`
/// is optional, and `deny_unknown_fields` keeps it out of `[[imports]]` -
/// the property that made a separate list worth having over a `direction`
/// key.
#[test]
fn importers_parse_with_and_without_via_module_and_imports_reject_it() {
    let file: ExpectFile = toml::from_str(
        "[[importers]]\nfile = \"a.ts\"\nexpect = [\"b.ts\"]\n\n\
         [[importers]]\nfile = \"pkg/helpers.py\"\nvia_module = \"pkg.helpers\"\nexpect = []\n",
    )
    .unwrap();
    assert_eq!(file.importers.len(), 2);
    assert_eq!(file.importers[0].via_module, None);
    assert_eq!(file.importers[1].via_module.as_deref(), Some("pkg.helpers"));
    assert_eq!(file.importers[0].tier, Tier::Structural);

    let err = toml::from_str::<ExpectFile>("[[imports]]\nfile = \"a.ts\"\nvia_module = \"a\"\nexpect = []\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("via_module") || err.contains("unknown field"), "{err}");
}

/// Decision 7: `via_module` is checked both ways round. The case that
/// matters is the third - a response with no `resolvedFrom` against an
/// entry that named a module is GM-356's own defect, and it must not read
/// as "not checked".
#[test]
fn via_module_is_an_assertion_in_both_of_its_states() {
    let substituted = serde_json::json!({
        "results": [],
        "resolvedFrom": {"requested": "pkg/helpers.py", "qualifiedName": "pkg.helpers"},
    });
    let literal = serde_json::json!({"results": []});

    assert!(resolved_from_finding(&substituted, Some("pkg.helpers")).is_none());
    assert!(resolved_from_finding(&literal, None).is_none());

    let missing = resolved_from_finding(&literal, Some("pkg.helpers")).expect("must flag it");
    assert!(missing.contains("no resolvedFrom"), "{missing}");

    let unexpected = resolved_from_finding(&substituted, None).expect("must flag it");
    assert!(unexpected.contains("expected no substitution"), "{unexpected}");

    let wrong = resolved_from_finding(&substituted, Some("pkg.other")).expect("must flag it");
    assert!(wrong.contains("pkg.helpers"), "{wrong}");
}

/// Decision 8: the duplicate GM-361's de-duplication removed, which a
/// `BTreeSet` cannot see. Two rows of one implementor is one answer twice.
#[test]
fn a_repeated_row_is_named_with_its_count() {
    let rows: Vec<String> =
        ["a.rs:A", "a.rs:A", "b.rs:B", "a.rs:C", "a.rs:C", "a.rs:C"].into_iter().map(String::from).collect();
    let finding = duplicate_row_finding(&rows).expect("must flag the repeats");
    assert!(finding.contains("a.rs:A (x2)"), "{finding}");
    assert!(finding.contains("a.rs:C (x3)"), "{finding}");
    assert!(!finding.contains("b.rs:B"), "{finding}");

    assert!(duplicate_row_finding(&["a.rs:A".to_string(), "b.rs:B".to_string()]).is_none());
    assert!(duplicate_row_finding(&[]).is_none());
}

/// Decision 8's reporting rule: a duplicate fails the entry *even when the
/// set matches*, and says the set matched - otherwise the failure reads as
/// a set mismatch and sends a reader hunting for one that is not there.
#[test]
fn a_finding_against_a_matching_set_fails_and_says_the_set_matched() {
    let expected: BTreeSet<String> = ["a.rs:A"].into_iter().map(String::from).collect();
    let actual = expected.clone();
    let Outcome::Fail(findings) = outcome_with(vec!["dup".to_string()], &expected, &actual) else {
        panic!("a finding must fail the entry");
    };
    let text = findings.join("\n");
    assert!(text.contains("dup"), "{text}");
    assert!(text.contains("the set itself matched: {a.rs:A}"), "{text}");

    assert_eq!(outcome_with(Vec::new(), &expected, &actual), Outcome::Pass);
}

/// Decision 4: a page reporting `hasMore: true` even at the maximum
/// limit must fail rather than be silently compared as if complete.
#[test]
fn a_page_reporting_has_more_is_never_treated_as_complete() {
    let page = serde_json::json!({"results": [], "hasMore": true});
    let finding = page_truncation_finding(&page, "callers").expect("must flag an incomplete page");
    assert!(finding.contains("hasMore: true"), "{finding}");
    assert!(finding.contains(&pagination::MAX_PAGE_SIZE.to_string()), "{finding}");
}

#[test]
fn a_complete_page_is_not_flagged_as_truncated() {
    let page = serde_json::json!({"results": [], "hasMore": false});
    assert!(page_truncation_finding(&page, "callers").is_none());
}

#[test]
fn a_truncated_dependency_walk_is_never_treated_as_complete() {
    let walk = serde_json::json!({"results": [], "truncated": true, "truncatedBy": "maxFanout"});
    let finding = walk_truncation_finding(&walk).expect("must flag a truncated walk");
    assert!(finding.contains("maxFanout"), "{finding}");
}

#[test]
fn an_untruncated_dependency_walk_is_not_flagged() {
    let walk = serde_json::json!({"results": [], "truncated": false});
    assert!(walk_truncation_finding(&walk).is_none());
}

/// Decision 6, the parsing half: `tier` defaults to `Structural` when
/// absent, and a `[[callers]]` entry that does spell `tier = "semantic"`
/// parses to `Tier::Semantic` - the two states `skip_or_eval` branches
/// on.
#[test]
fn tier_defaults_to_structural_and_parses_semantic_when_given() {
    let file: ExpectFile = toml::from_str(
        "[[callers]]\nsymbol = \"a\"\nexpect = []\n\n\
         [[callers]]\nsymbol = \"b\"\nexpect = []\ntier = \"semantic\"\n",
    )
    .unwrap();
    assert_eq!(file.callers[0].tier, Tier::Structural);
    assert_eq!(file.callers[1].tier, Tier::Semantic);
}

/// Decision 6: with `skip_semantic = true`, a `Semantic`-tier entry never
/// calls `eval` at all (it would panic if it did) and reports `Skip`
/// under the same id `eval_symbol_expectation` would have used; a
/// `Structural` one is unaffected.
#[test]
fn skip_or_eval_skips_only_the_semantic_tier_entries() {
    let skipped = skip_or_eval(true, "callers", 2, Tier::Semantic, || panic!("must not run"));
    assert_eq!(skipped.id, "expectations.callers[2]");
    assert!(matches!(skipped.outcome, Outcome::Skip(_)), "{:?}", skipped.outcome);

    let ran = skip_or_eval(true, "callers", 0, Tier::Structural, || CheckResult {
        id: "expectations.callers[0]".into(),
        outcome: Outcome::Pass,
        warnings: Vec::new(),
    });
    assert_eq!(ran.outcome, Outcome::Pass);

    let ran_without_the_flag = skip_or_eval(false, "callers", 2, Tier::Semantic, || CheckResult {
        id: "expectations.callers[2]".into(),
        outcome: Outcome::Pass,
        warnings: Vec::new(),
    });
    assert_eq!(ran_without_the_flag.outcome, Outcome::Pass);
}

/// Decision 9's root cause, pinned at the exact place it was lost:
/// `tool_json` collapsed a tool-level refusal and a protocol-level failure
/// into one `Err(String)`, so nothing downstream could tell "this name is
/// not a declaration" from "the call broke" - which is why decision 2's
/// zero-element branch was unreachable. [`tool_outcome`] keeps them apart.
#[test]
fn a_tool_level_refusal_is_a_refusal_and_a_protocol_error_is_not() {
    let refusal = CallToolResult::error(vec![ContentBlock::text("g-mesh: no symbol named 'x' found")]);
    let Ok(ToolOutcome::Refusal(text)) = tool_outcome(Ok(refusal)) else {
        panic!("is_error: true must be a Refusal, not an Err");
    };
    assert_eq!(text, "g-mesh: no symbol named 'x' found");

    let answer = CallToolResult::success(vec![ContentBlock::text("{\"results\":[]}")]);
    let Ok(ToolOutcome::Answer(value)) = tool_outcome(Ok(answer)) else {
        panic!("a successful result must parse as an Answer");
    };
    assert!(value.get("results").is_some(), "{value}");

    // The arm an entry must never accept: the handler never produced a
    // result at all.
    let protocol: Result<CallToolResult, ErrorData> =
        Err(ErrorData::internal_error("g-mesh: the index is gone".to_string(), None));
    assert!(tool_outcome(protocol).is_err(), "a protocol-level failure must stay an Err");
}

/// The four categories that compare sets must be unaffected by the split:
/// a refusal is still exactly the `Err(text)` they have always reported.
#[test]
fn tool_json_still_reports_a_refusal_as_a_plain_error_string() {
    let refusal = CallToolResult::error(vec![ContentBlock::text("g-mesh: nope")]);
    assert_eq!(tool_json(tool_outcome(Ok(refusal))), Err("g-mesh: nope".to_string()));
}

/// Decision 9's text assertion: every phrase must be present, each missing
/// one is named, and the refusal itself is quoted so a fixture can be
/// fixed against what was said rather than against a guess.
#[test]
fn a_refusal_must_carry_every_phrase_the_entry_named() {
    let text = "g-mesh: nothing named 'strings' is declared in this project. It names something \
                this project imports: 'strings' (1) - 1 import record(s), which have no definition \
                site here. For what a file imports, or what imports it, use get_dependencies.";
    let phrases = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    assert!(
        refusal_text_findings(text, &phrases(&["is declared in this project", "get_dependencies"])).is_none()
    );

    let findings = refusal_text_findings(text, &phrases(&["is declared in this project", "ghost"]))
        .expect("a missing phrase must fail the entry");
    let joined = findings.join("\n");
    assert!(joined.contains("missing 1 required phrase(s): \"ghost\""), "{joined}");
    assert!(joined.contains("use get_dependencies"), "the refusal itself must be quoted: {joined}");
}

/// The degenerate shape `deny_unknown_fields` cannot catch: `contains` is
/// present and well-typed but empty, which would make the entry accept any
/// refusal at all - including one about a parameter mistake. Decision 9
/// fails it instead.
#[test]
fn an_empty_contains_never_passes_vacuously() {
    let findings = refusal_text_findings("g-mesh: anything at all", &[])
        .expect("an empty `contains` must fail rather than accept everything");
    assert!(findings.join("\n").contains("`contains` is empty"), "{findings:?}");
}

/// Decision 9's `[[refusal]]` parses with its three required keys, rejects
/// a `file` companion (there is nothing to disambiguate), and its
/// `contains` cannot appear on a `[[callers]]` entry - the property that
/// made a separate struct worth having, exactly as decision 7 argued for
/// `via_module`.
#[test]
fn refusal_entries_parse_and_their_keys_stay_out_of_the_other_categories() {
    let file: ExpectFile = toml::from_str(
        "[[refusal]]\ntool = \"definition\"\nsymbol = \"strings\"\ncontains = [\"imports\"]\n\n\
         [[refusal]]\ntool = \"references\"\nsymbol = \"ghost\"\ncontains = [\"no symbol named\"]\n\
         tier = \"semantic\"\n",
    )
    .unwrap();
    assert_eq!(file.refusal.len(), 2);
    assert_eq!(file.refusal[0].tool, RefusedTool::Definition);
    assert_eq!(file.refusal[0].tier, Tier::Structural);
    assert_eq!(file.refusal[1].tool, RefusedTool::References);
    assert_eq!(file.refusal[1].tier, Tier::Semantic);

    for (source, needle) in [
        ("[[refusal]]\ntool = \"definition\"\nsymbol = \"x\"\ncontains = []\nfile = \"a.rs\"\n", "file"),
        ("[[callers]]\nsymbol = \"x\"\nexpect = []\ncontains = [\"y\"]\n", "contains"),
        ("[[refusal]]\ntool = \"imports\"\nsymbol = \"x\"\ncontains = [\"y\"]\n", "imports"),
    ] {
        let err = toml::from_str::<ExpectFile>(source).unwrap_err().to_string();
        assert!(err.contains(needle), "expected {needle:?} to be rejected, got: {err}");
    }

    // `contains` is required, not defaulted - an entry that forgot it is a
    // parse error rather than an entry that accepts any refusal.
    let err = toml::from_str::<ExpectFile>("[[refusal]]\ntool = \"callers\"\nsymbol = \"x\"\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("contains"), "{err}");
}

/// The line a `[[refusal]]` prints when it was answered instead, on both
/// shapes of answer decision 3 case 1 describes: the four edge-walking
/// tools wrap the node in `anchor`, `find_definition` returns it bare.
#[test]
fn an_answered_refusal_names_what_it_resolved_to_on_either_shape() {
    let walk = serde_json::json!({
        "anchor": {"id": "n1", "qualifiedName": "helper", "filePath": "a.fk", "kind": "Function"},
        "results": [],
    });
    assert_eq!(resolved_anchor_line(&walk), "a.fk:helper");

    let definition = serde_json::json!({"qualifiedName": "helper", "filePath": "a.fk"});
    assert_eq!(resolved_anchor_line(&definition), "a.fk:helper");
}

#[test]
fn an_unparsable_expect_file_reports_a_readable_parse_error() {
    let dir = std::env::temp_dir().join(format!("g-mesh-expectations-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("expect.toml");
    std::fs::write(&path, "[[callers]]\nsymbol = \"f\"\nexpect = [\"a.ts:f\"]\nbogus_key = true\n").unwrap();
    let err = parse(&path).unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("bogus_key") || text.contains("unknown field"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
