# GM-541: test-suite audit for redundant and obsolete tests

Report slice of GM-541 (batch 4.3.0). It closes the task's three acceptance
criteria:

1. Every candidate test is listed with its reason and its evidence: the other
   test that covers the same behaviour, or a control showing that the test
   does not fail when the code is reverted.
2. Nothing has been deleted. This report is a list for the owner to approve;
   the deletions are a follow-up task (section 4).
3. Section 1 states how many tests and how much suite time the
   recommendations remove.

Sources, all from GM-541's slices:

- S1 (core unit tests), S2 (core integration tests), S3 (plugins and wire):
  the candidate tables. Read-only audits of the main checkout.
- S6: revert controls in a throwaway worktree off release-4.3.0 `d50bb07`.
- S7: per-test times from the batch-end full run
  (`scripts/test-local.sh --full`, ci profile, heavy tests included) on
  release-4.3.0 `c35186a`.
- Test names and the line numbers below were re-checked by grep at
  release-4.3.0 `887d3e1`. Every candidate still exists under the name given.

## 1. Summary

### Tests audited

| Crate | Tests audited | How |
|---|---|---|
| g-mesh (core/src, unit) | 1715 | scripted scans of every body, all names read, about 90 bodies read |
| g-mesh (core/tests, integration) | 297 | scripted scans of every body, about 55 bodies read |
| g-mesh-plugin-sdk | 287 (289 `#[test]` by grep; 2 not body-scanned) | as above, about 60 bodies read across S3's scope |
| g-mesh-plugin-typescript | 325 | as above |
| g-mesh-plugin-rust | 170 | as above |
| g-mesh-plugin-python | 162 | as above |
| g-mesh-wire | 33 | as above |
| **all** | **2989** | plugins/go (about 125 Go tests) not audited, see section 5 |

The scans were: an exact body match, a body match with literals replaced, a
difflib similarity of at least 0.90-0.95, tests with no assertion,
tautologies, legacy-style names, and sleeps, spawned processes or `#[ignore]`.
The tautology scan found nothing in any crate. Every hit of the no-assert
scan turned out to assert through a helper, or to be an extractor artefact.

### Candidates by recommendation

47 candidate tests (41 rows in the slice tables; S2#10, #11 and #12 cover
more than one test each).

| Crate | Candidates | delete | merge | move-to-unit | rewrite | keep |
|---|---|---|---|---|---|---|
| g-mesh | 32 | 24 | 3 | 1 | 4 | 0 |
| g-mesh-plugin-sdk | 1 | 0 | 1 | 0 | 0 | 0 |
| g-mesh-plugin-typescript | 2 | 2 | 0 | 0 | 0 | 0 |
| g-mesh-plugin-rust | 4 | 2 | 2 | 0 | 0 | 0 |
| g-mesh-plugin-python | 7 | 5 | 2 | 0 | 0 | 0 |
| g-mesh-wire | 1 | 0 | 1 | 0 | 0 | 0 |
| **all** | **47** | **33** | **9** | **1** | **4** | **0** |

No candidate became **keep**. S6 ran 10 controls (c01-c10) over the 10
candidate tests that had a revert to test, and none showed that a candidate protects
something its covering test does not (section 2). Owner decisions, renames
and the "considered and kept" lists of the slices are not candidates; they
are listed in sections 2.4 and 3.

- **merge**: the candidate's one unique assertion moves into the covering
  test, then the candidate is deleted.
- **move-to-unit**: the assertion moves into a unit test, then the
  integration test is deleted.
- **rewrite**: the 4 `#[ignore]`d tests of S2#10. They depend on owner
  decision 3.2 and remove nothing, because none of them runs today.

### Tests removed and seconds saved

delete + merge + move-to-unit remove **43 tests** and **110.2 s** of a
**5666.8 s** per-test sum (1.94%).

| Crate | Tests removed | delete (s) | merge (s) | move-to-unit (s) | Removable (s) | Crate per-test sum (s) | Share |
|---|---|---|---|---|---|---|---|
| g-mesh | 28 | 46.76 | 3.62 | 0.79 | 51.17 | 4302.5 | 1.19% |
| g-mesh-plugin-sdk | 1 | 0.00 | 0.58 | 0.00 | 0.58 | 443.5 | 0.13% |
| g-mesh-plugin-typescript | 2 | 5.01 | 0.00 | 0.00 | 5.01 | 196.8 | 2.54% |
| g-mesh-plugin-rust | 4 | 2.11 | 32.82 | 0.00 | 34.93 | 380.2 | 9.19% |
| g-mesh-plugin-python | 7 | 6.10 | 10.67 | 0.00 | 16.77 | 277.5 | 6.04% |
| g-mesh-wire | 1 | 0.00 | 1.75 | 0.00 | 1.75 | 66.4 | 2.63% |
| **all** | **43** | **59.98** | **49.43** | **0.79** | **110.20** | **5666.8** | **1.94%** |

How to read these numbers:

- **One run, at low load for this batch.** The load average was 7-92 at the
  start and 55 at the end (real 1850 s, user 1770 s, sys 517 s). Earlier in
  the batch the machine ran at load 400+, where process-heavy tests took 2-5x
  longer. Read every number as a floor, with no variance estimate.
- **Per-test time, not wall time.** nextest runs tests in parallel, so the
  sum counts CPU slots. The wall-clock saving is smaller, except where a
  candidate is on a section's critical path: S3#12 (rust conformance, 31.0 s)
  and S2#8 (query_time_staleness, 11.9 s).
- **Most of a trivial test's time is process start.** The median unit test
  in this run took 1.40-2.05 s per crate. Deleting a pure `assert_eq!` test
  saves about that median.
- **merge and move-to-unit are upper bounds.** The moved assertion still
  runs. For a merge into a test that already does the same setup, the saving
  is close to the whole time.
- **Local runs see less of it.** S2#7 is already in the `local` profile's
  heavy list (GM-548), and GM-552 (not yet merged; branch
  `chore/GM-552-heavy-list-update`) adds both rust conformance tests to that
  list, S3#12 included. A default `scripts/test-local.sh` run skips them, so
  those savings show up only in CI and in `--full` runs.

The 5 largest items make up 61.8 s, 56% of the total: S3#12 31.0 s, S2#8
11.9 s, S3#13 9.1 s, S2#7 5.8 s, S2#4 3.2 s.

## 2. Candidates

Evidence kinds:

- **control**: an S6 revert control. It fails the covering test and leaves
  the candidate passing, or it fails both together. This is the strongest
  evidence.
- **construction**: the test exercises only test code (a fixture or a
  test-file helper), so no revert of production code can reach it. The
  control is not applicable, and the claim follows from the code itself.
- **reading (weaker)**: the covering test is named from reading both bodies,
  and no control was run. A semantic difference the reader missed would not
  be caught.

Line numbers are at `887d3e1` (`fn` line). Seconds are from S7.

### 2.1 Core unit tests (core/src), from S1

| ID | Test | Category | Evidence | Kind | Rec. | s |
|---|---|---|---|---|---|---|
| S1#1 | `embedding/pipeline.rs::embedding_a_node_with_no_text_never_touches_the_model_or_the_database` (l.1010) | duplicate; the name promises more than the test checks | Same body as `a_node_with_neither_has_nothing_to_embed`: `assert_eq!(text_to_embed(None, None), None)`. It never calls `embed_node`, the model or the database. | reading (weaker) | delete | 1.99 |
| S1#2 | `cli/config_wizard.rs::writing_the_global_wizard_output_targets_the_global_path_not_the_project_one` (l.433) | duplicate; the name promises more than the test checks | Same call and same 2 asserts as `answering_the_global_prompts_writes_exactly_those_values`; only the number differs. No path is asserted. Its own comment says the path is asserted in `config::tests`. | reading (weaker) | delete | 1.17 |
| S1#3 | `cli/status/tests.rs::embedding_phase_reports_structural_ready_with_embeddings_in_progress` (l.683) | duplicate | Same input and expected string as a case of `every_phase_prints_an_accurate_index_line`. | reading (weaker) | delete | 1.70 |
| S1#4 | `cli/status/tests.rs::failed_phase_reports_the_last_build_failed_and_will_be_retried` (l.696) | duplicate | Case 6 of `every_phase_prints_an_accurate_index_line`. | reading (weaker) | delete | 1.88 |
| S1#5 | `graph/pagination.rs::locality_breaks_ties_after_resolved` (l.1145) | duplicate | Same `paginate_edges` call (g-mesh `find_definition ranked_page`). `locality_ranks_the_same_file_then_the_same_directory_then_elsewhere` asserts everything this test asserts. | reading (weaker) | delete | 1.49 |
| S1#6 | `graph/pagination.rs::resolved_true_sorts_before_resolved_false_at_equal_locality` (l.1118) | duplicate | `a_resolved_row_elsewhere_ranks_before_an_unresolved_row_in_the_anchors_own_file` asserts the stronger form, on the same path. The only extra here is `!has_more` on a 2-row page. | reading (weaker) | delete (optionally move `!has_more` into the covering test) | 1.49 |
| S1#7 | `daemon/manifest/tests.rs::the_real_bundled_plugin_directory_satisfies_read_manifest_directly` (l.940) | duplicate | `the_checked_in_manifests_declare_each_languages_reexport_rule` calls `read_manifest` on the same real directory and asserts more fields. | reading (weaker) | delete | 1.11 |
| S1#8 | `daemon/tests.rs::a_batch_routes_its_new_files_before_their_modified_importers_whatever_the_event_order` (l.452) | duplicate (subset); spawns a python test plugin | `a_batch_routes_deletions_then_creations_then_modifications_whatever_the_event_order` pins created-before-modified on the same path. The pure ordering is also pinned in `watcher/batch/tests.rs`. | reading (weaker) | delete | 2.17 |
| S1#9 | `storage/connection.rs::same_root_hashes_to_same_directory` (l.153) | duplicate | `project_dir`'s only callees are `projects_root` and `project_hash` (g-mesh `find_callees`). Determinism is pinned in `daemon/identity.rs`. | reading (weaker) | delete | 1.24 |
| S1#10 | `daemon/identity.rs::same_path_always_hashes_identically` (l.73) | duplicate | `trailing_slash_and_relative_forms_hash_identically_after_canonicalization` compares `project_hash` across spellings, so any non-determinism fails it too. | reading (weaker) | merge into `trailing_slash_and_relative_forms_hash_identically_after_canonicalization` | 0.67 |
| S1#11 | `embedding/text.rs::is_deterministic` (l.431) | assertion cannot fail, and duplicate | Control c01 (`is_tag_line` returns false) fails this test and `drops_jsdoc_tags_and_rest_fields` together. Repeating a pure function 16 times adds nothing. | control | delete | 1.25 |
| S1#12 | `embedding/pipeline.rs::the_fake_model_is_deterministic` (l.1591) | assertion cannot fail (tests a test fixture) | It asserts on the test-only `fake_vector`. A broken fixture would also fail the tests that use it. | construction | delete | 1.45 |
| S1#13 | `languages/tests.rs::a_catalogue_entry_holds_only_a_language_and_its_extensions` (l.106) | obsolete name, and duplicate | The entry now also has `exclude_dirs`. The non-empty asserts are covered by `the_catalogue_names_exactly_the_four_bundled_languages_in_order` and `every_catalogue_entry_matches_its_real_plugin_manifest_extensions`. Referenced by name in `docs/adr/0021-per-language-bulk-outcome.md`. | reading (weaker) | delete (and update the ADR reference) | 1.39 |
| S1#14 | `protocol/jsonrpc.rs::frame_arriving_in_pieces_over_a_pipe_is_reassembled` (l.280) | timing-based, covered by a deterministic unit test | `frame_split_across_reads_is_reassembled` forces 3-byte reads and asserts `reads > 1`. This test uses a thread, a pipe and sleeps, and does not assert that a partial read happened. | reading (weaker) | delete | 1.62 |

### 2.2 Core integration tests (core/tests), from S2

| ID | Test | Category | Evidence | Kind | Rec. | s |
|---|---|---|---|---|---|---|
| S2#1 | `last_used.rs::a_project_directory_with_no_index_reads_as_nothing_recorded` (l.230) | duplicate | The same call as the unit test `gc/last_used.rs::a_directory_without_an_index_reads_as_none`. It spawns nothing. | reading (weaker) | delete | 0.88 |
| S2#2 | `state_isolation.rs::the_state_directory_parser_reads_the_line_status_actually_prints` (l.92) | assertion cannot fail | Control c02 rewords the status line. This test still passes; its sibling `a_spawned_g_mesh_resolves_the_same_isolated_state_root` fails. | control | delete | 0.19 |
| S2#3 | `overload_declaration_storage.rs::a_freshly_built_index_reads_schema_version_14` (l.183) (was `..._13` in S2) | slow integration covered by a unit test | Control c03 (stamp a wrong schema version) fails this test and `storage::schema::tests::wipes_and_reindexes_on_version_mismatch` together. The version is stamped by the test's own `ensure_current` call, not by the walk. The only extra is the literal `"14"`. | control | move-to-unit (pin the literal in the schema tests), then delete | 0.79 |
| S2#4 | `cli_stop.rs::stopping_an_already_stopped_project_is_a_clean_no_op` (l.231) | duplicate; spawns a daemon | `stop_shuts_down_both_the_core_and_the_plugin` leaves exactly the state that `stopping_a_project_with_no_daemon_running_is_a_clean_no_op` starts from, and both assert the same no-op. No control was run in S6. | reading (weaker) | delete | 3.20 |
| S2#5 | `daemon_core.rs::daemon_opens_sqlite_watches_files_and_serves_the_mcp_tool_surface` (l.216) | obsolete behaviour, and duplicate | Control c04 (no `ProjectWatcher` in `ActivationCtx::walk`): it still passes, because `tools/list` never activates. The tool list is covered by `shim_bootstrap.rs` and `mcp_e2e.rs`. Its one unique check is that `index.db` exists after start. | control | merge (move the `index.db` assert), then delete | 1.53 |
| S2#6 | `daemon_plugin_discovery_failure.rs::a_single_discovered_language_with_no_conflict_starts_the_daemon_normally` (l.165) | assertion cannot fail for its stated purpose | Control c05 makes discovery ignore `G_MESH_PLUGIN_ROOTS_OVERRIDE`. This test still passes; the conflict test `two_plugins_claiming_the_same_extension_fails_daemon_startup_with_a_clear_error` fails. | control | delete | 0.74 |
| S2#7 | `serving_while_indexing.rs::a_first_call_is_answered_normally_when_nothing_holds_the_walk_open` (l.338) | duplicate | Its neighbour test (l.268) ends with the same `find_definition("connect")` assert after the walk. Also covered by `lazy_activation.rs::first_structural_call_blocks_and_answers_fully`. No control was run in S6. It is in the `local` heavy list (`.config/nextest.toml`) and in `docs/architecture/gm-548-faster-local-tests.md`. | reading (weaker) | delete (and remove the heavy-list entry) | 5.79 |
| S2#8 | `query_time_staleness.rs::a_file_edited_while_the_daemon_was_down_is_reindexed_before_the_next_query_answers` (l.159) | slow integration, duplicate | Control c06 (no `ensure_fresh` in `get_file_outline`) fails this test, `daemon_core.rs::a_restart_against_an_already_indexed_project_does_not_walk_it_again` and `first_query_after_walk.rs::a_file_edited_after_the_walk_is_still_reindexed` together. The logic is unit-tested in `watcher/staleness.rs`. | control | delete | 11.95 |
| S2#9 | `one_plugin_binary_missing.rs::a_failed_language_recorded_in_the_index_is_not_reindexed_after_a_restart` (l.401) | duplicate | Control c07 (drop `seed_failed_languages`) fails this test and `a_failed_language_is_not_reindexed_by_the_first_tool_call_after_a_restart` together. The only extra here: it asserts after `activated()`. | control | merge (add `activated(root).await` to the covering test), then delete | 1.43 |
| S2#10a | `embedding_generation_pipeline.rs::indexing_a_fixture_file_embeds_its_documented_symbols` (l.209, `#[ignore]`) | obsolete behaviour | Drives `bulk_index::run(.., Some(embedding), ..)`. Every production caller passes `None` (section 3.2). | reading (weaker) | rewrite onto `walk_then_backfill` (decision 3.2) | not run |
| S2#10b | `...::a_walk_that_embeds_anything_records_the_active_model_in_meta` (l.274, `#[ignore]`) | obsolete behaviour | as S2#10a | reading (weaker) | rewrite (decision 3.2) | not run |
| S2#10c | `...::the_file_node_has_no_doc_comment_or_signature_and_is_not_embedded` (l.291, `#[ignore]`) | obsolete behaviour | as S2#10a | reading (weaker) | rewrite (decision 3.2) | not run |
| S2#10d | `...::the_stored_vector_matches_embedding_the_doc_comment_and_signature_directly` (l.344, `#[ignore]`) | obsolete behaviour | as S2#10a | reading (weaker) | rewrite (decision 3.2) | not run |
| S2#10e | `...::a_disabled_pipeline_indexes_the_fixture_without_writing_any_vectors` (l.188) | obsolete behaviour | It runs a real plugin walk with `Some(disabled pipeline)`, a combination production never passes. | reading (weaker) | delete | 0.81 |
| S2#11a | `protocol_conformance.rs::container_node_fixture_is_rejected` (l.79) | duplicate | Same `check_bulk_output` path and messages as the unit test `protocol/conformance.rs::a_plugin_emitted_container_node_is_a_violation`; only the input source differs (fixture file vs inline). The fixtures stay; plugins/go also reads one of them. | reading (weaker) | delete (low value, low cost) | 0.50 |
| S2#11b | `protocol_conformance.rs::placeholder_with_no_target_fixture_is_rejected` (l.66) | duplicate | as S2#11a, against `placeholder_with_no_target_is_a_violation` | reading (weaker) | delete | 0.73 |
| S2#12a | `cli_clean.rs::clean_refuses_a_path_shaped_project_id` (l.191) | slow integration covered by a unit test | Control c08 (reword the refusals) fails this test and `cli::clean::tests::a_path_shaped_project_id_is_refused_rather_than_joined` together. The "stderr and non-zero exit" wiring is shown by `clean_refuses_while_a_daemon_is_still_serving_the_project`. | control | delete | 1.35 |
| S2#12b | `cli_clean.rs::clean_in_a_never_indexed_directory_asks_for_an_explicit_project_id` (l.155) | slow integration covered by a unit test | Control c08 fails this test and `cli::clean::tests::an_unrecognized_cwd_asks_for_an_explicit_id_instead_of_guessing` together. | control | delete | 0.69 |

### 2.3 Plugins and wire, from S3

Paths are relative to the crate directory.

| ID | Test | Category | Evidence | Kind | Rec. | s |
|---|---|---|---|---|---|---|
| S3#1 | `typescript/src/semantic.rs::the_manifest_version_matches_the_crates` (l.252) | duplicate | `typescript/tests/units.rs::the_manifest_version_matches_the_crates` makes the same assertion on the same file, with a clearer message. | reading (weaker) | delete | 2.02 |
| S3#2 | `rust/src/extractor/emit.rs::a_files_end_is_where_a_trailing_space_cannot_move_it` (l.412) | duplicate | `Positions::file_range` is a one-line delegate to the SDK's `CharColumns::file_range`, pinned by `sdk/tests/columns.rs::columns_file_range_is_unmoved_by_a_space_before_the_last_newline`. The plugin wiring is pinned by `rust/src/extractor/tests.rs::a_trailing_space_before_the_last_newline_changes_nothing`. Named in `docs/architecture/gm-527-file-end-line.md`. | reading (weaker) | delete (and update the note) | 0.57 |
| S3#3 | `python/src/extractor/emit.rs::a_files_end_is_where_a_trailing_space_cannot_move_it` (l.536) | duplicate | as S3#2; the wiring is pinned by `python/src/extractor/tests.rs::a_trailing_space_before_the_last_newline_changes_nothing` | reading (weaker) | delete | 0.81 |
| S3#4 | `python/src/semantic.rs::a_command_that_is_a_path_is_the_only_candidate` (l.448) | duplicate | Python's `candidates` is a bare `npm_candidates` delegate (g-mesh `find_callers npm_candidates`). SDK `lsp/resolve.rs::a_command_that_is_a_path_is_the_only_origin` asserts the full list. | reading (weaker) | delete | 1.34 |
| S3#5 | `typescript/src/semantic.rs::a_command_that_is_a_path_is_the_only_candidate` (l.197) | duplicate | as S3#4 | reading (weaker) | delete | 2.98 |
| S3#6 | `python/src/semantic.rs::on_windows_every_origin_is_tried_bare_then_with_each_script_extension` (l.488) | duplicate | SDK `a_bare_name_is_path_then_node_modules_then_npx` (with `.cmd`) and `the_npx_candidate_names_the_package_and_probes_the_probe_bin` cover it. The pyright argv is pinned by python's `a_bare_name_is_path_then_the_projects_node_modules_then_npx`. Named in `docs/architecture/gm-325-typescript-lsp-semantics.md`. | reading (weaker) | delete | 1.40 |
| S3#7 | `python/src/semantic.rs::the_npx_probe_differs_from_the_npx_server_only_in_the_bin_name` (l.535) | duplicate | SDK l.453 plus python `every_candidate_is_probed_through_the_cli_twin_and_never_the_server`. Named in the GM-325 note. | reading (weaker) | merge (assert the npx argv in the python test) | 1.54 |
| S3#8 | `python/src/semantic.rs::a_binary_that_exits_non_zero_is_not_a_pyright` (l.590) | duplicate | It calls the SDK's `lsp::probe` directly, with no python code on the path. SDK `a_probe_that_exits_non_zero_is_an_error_naming_its_stderr` and `a_probe_that_cannot_start_is_an_error` also check the error text. | reading (weaker) | delete | 1.39 |
| S3#9 | `rust/src/semantic.rs::a_binary_that_exits_non_zero_is_not_a_server` (l.217) | duplicate | as S3#8 | reading (weaker) | delete | 1.54 |
| S3#10 | `rust/src/semantic.rs::a_path_that_fails_is_reported_with_its_origin` (l.294) | duplicate path | The same `resolve(...)` call as `nothing_usable_names_the_remedy`, asserting a different substring of the same message. | reading (weaker) | merge into `nothing_usable_names_the_remedy` | 1.82 |
| S3#11 | `wire/src/lib.rs::a_v1_shaped_node_exported_instead_of_visibility_is_rejected_with_a_clear_error` (l.1174) | duplicate | `WireNode` has no `deny_unknown_fields` (re-checked: 0 hits), so `exported` is ignored and both tests fail on serde's missing `visibility`. | reading (weaker) | merge (keep the v1 input as a second case of `a_node_missing_visibility_is_rejected`, with its GM-275 comment) | 1.75 |
| S3#12 | `rust/tests/conformance.rs::the_plugin_passes_every_check_that_applies_to_it` (l.363) | slow duplicate (full rust-analyzer session) | `the_linked_index_answers_the_acceptance_criteria` runs the same session plus `.expect(EXPECT)` and calls the same `assert_conformant()`. Only `assert_report_shape` is unique. TypeScript has already merged its pair this way (`the_linked_index_answers_every_expectation_with_vtsls`). | reading (weaker) | merge (move `assert_report_shape` into the linked test) | 31.01 |
| S3#13 | `python/tests/conformance.rs::the_plugin_passes_every_check_that_applies_to_it` (l.407) | slow duplicate (full pyright session) | Byte-identical to the rust pair; same reasoning as S3#12. | reading (weaker) | merge (as S3#12) | 9.13 |
| S3#14 | `sdk/tests/lsp_bridge.rs::a_cross_file_field_read_the_server_confirms_survives_a_later_empty_pass` (l.2616) | duplicate | Control c09 (route `ReceiverField` to `continue`) fails this test along with its 2 field siblings and the bridge unit test, so the field arm is covered elsewhere. Control c10 (call and field) fails it with `a_cross_file_call_the_server_confirms_survives_a_later_empty_pass`. | control | merge (parametrise the call test over the site kind) or delete | 0.58 |
| S3#15 | `python/tests/conformance.rs::spelled_tries_the_bare_name_then_every_script_extension` (l.340) | assertion cannot fail through production code | `spelled` is a helper in the test file. If it broke, `pyright_langserver()` would panic and name every spelling it tried. | construction | delete | 1.16 |

### 2.4 Listed for completeness, not candidates

| Tests | Status | Why |
|---|---|---|
| `mcp/source.rs::an_old_index_whole_file_end_reads_as_the_whole_file`, `mcp/find_definition/tests.rs::a_whole_file_candidate_in_an_old_index_still_carries_its_source`, `...::a_whole_file_candidate_past_the_old_end_is_stale_and_unsourced` (3.38 s together) | keep until decision 3.1 | They pin a production clamp that still exists. |
| `embedding_generation_pipeline.rs::a_structural_walk_followed_by_backfill_embeds_the_same_symbols_as_the_old_inline_walk` (`#[ignore]`) | keep, rename | "the old inline walk" is the path decision 3.2 retires. |
| `rust/src/census.rs::open_site_census`, `python/src/census.rs::open_site_census`, `rust/tests/semantic_pass_measurement.rs::whole_project_pass_cost` (all `#[ignore]`) | keep until decision 3.4 | They are measurement tools, not redundant tests. |
| `sdk/tests/id_scheme.rs` (2 tests) | keep, fix the name and doc | They are golden vectors for id stability. The name and doc still refer to the removed Node plugin and say that plugins/go "is not on this branch". |
| The "considered and kept" lists in S1, S2 and S3 | keep | Same-looking tests that turned out to cover different paths or arms. The slice tables give the reason for each, mostly backed by a g-mesh `find_definition`/`find_callers` result. |

## 3. Owner decisions

Each decision was checked against release-4.3.0 `887d3e1`.

Owner answers (2026-10-10):

- The list: "Утвердить, задачей в бэклог (Recommended)". The removals
  are a separate backlog task (section 4).
- 3.1, the old-index clamp: "Удалить с 3 тестами (Recommended)".
- 3.2, the inline-embedding bulk path: "Убрать путь, переписать 4 теста
  (Recommended)".
- 3.3, the CI `--expect` loop: "Оставить как есть (Recommended)".
- 3.4, the census harnesses: no decision needed. The hooks are
  `#[cfg(test)]` only, so the harnesses stay.

### 3.1 The 3 tests pinning the pre-GM-527 old-index clamp

**Today.** Before GM-527, a whole-file node in the index ended one line past
the file, at `(lines, 0)`. `mcp::source::read_span_within`
(`core/src/mcp/source.rs` l.98) still has a branch for that exact end: it
reads it as the end of the last line. The 3 tests in section 2.4 pin this
branch. Both GM-527 (bundled plugins emit the new end) and the schema bump
to `"14"` (GM-509) land in 4.3.0. Since `meta.indexer_version` is core's
version plus a digest of every plugin's fingerprint, any index built before
4.3.0 is wiped on upgrade. After 4.3.0, the only way an old end can reach the
index is a third-party plugin that still emits it.
`docs/architecture/gm-527-file-end-line.md` says the clamp "could be removed
once old indexes no longer need to be read" and sets no date.

**The change.** Either (a) keep the clamp and its 3 tests, or (b) remove the
`if end == lines.len() && end_col == Some(0) ...` branch, its doc paragraph
in `read_span`, and the 3 tests.

**Example.** A third-party plugin that has not adopted GM-527 indexes
`a.py` (10 lines, ending in a newline), and its whole-file node ends at
`(10, 0)`. With (a), `find_definition` on the module returns all 10 lines as
the snippet. With (b), it returns the coordinates with no snippet, the same
answer as for any span past the end of the file.

**Consequence.** (a) costs 3 tests (3.4 s) and one special case in a hot
read path, which protects plugins that may not exist. (b) removes them, and
the degradation for a stale plugin is a missing snippet, not an error.
Suggested: (b) in the follow-up task, unless a third-party plugin is known
to emit the old end. It is a production change, so it needs its own revert
control: a fixture with an `(lines, 0)` end must return no snippet.

### 3.2 Retiring the inline-embedding bulk path

**Today.** `daemon::bulk_index::run` and `run_with_progress`
(`core/src/daemon/bulk_index.rs` l.142, l.153) take an
`embedding: Option<&EmbeddingPipeline>`, carried in `WalkContext.embedding`
and used in the batch commit (l.551-554) and in `finish_unit` (l.221). Every
production caller passes `None`: `cli::init` (l.253), `cli::reindex`
(l.114), and `ActivationCtx::walk` (`daemon/activation.rs` l.232, with the
comment "embedding is the backfill pass's job"). The only `Some` caller in
the repository is the test helper `Project::walk` in
`core/tests/embedding_generation_pipeline.rs` (l.100). It is used by the
5 tests of S2#10a-e (4 of them `#[ignore]`d).

**The change.** Remove the parameter from both functions, the
`WalkContext.embedding` field and the bulk-walk embedding branch. Rewrite
S2#10a-d onto `walk_then_backfill`, or drop any that would then duplicate
`cached_vectors_are_bit_identical_to_fresh_ones_and_rank_the_same`. Delete
S2#10e and rename the `..._as_the_old_inline_walk` test. Note one dependency:
the doc comment of a control in `daemon/workspace_reindex.rs` (l.1454)
describes "embed during the staging walk (`WalkContext { embedding: ..`".
That control would need a new description.

**Example.** Today `bulk_index::run(root, &conn, Some(&pipeline), &plugins)`
compiles and embeds inline, but no shipped code path calls it that way. After
the change, the only way to get vectors is walk then backfill, which is what
`g-mesh init` and the daemon already do.

**Consequence.** Pro: one embedding path, and the ignored tests then test
the path production takes. Con: it is a production refactor across
bulk_index and its 11 test callers (each drops a `None` argument), and the
4 rewritten tests still need the real weights (`#[ignore]`), so CI does not
check the rewrite. The alternative (keep the path, delete only S2#10e) leaves
4 ignored tests pinning a path nobody takes.

### 3.3 The CI `--expect` loop repeating the rust/python/typescript conformance runs

**Today.** The step "g-mesh plugins check (--expect, every plugin with a
conformance fixture)" is at `.github/workflows/ci.yml` l.851 (S3's l.709
has moved). It loops over `plugins/*/` and runs
`g-mesh plugins check <plugin> --fixture .../conformance/project --expect .../conformance/expect.toml`
for every plugin that has both files: go, python, rust and typescript. On
the same 4 runners, the nextest step already runs
`the_linked_index_answers_the_acceptance_criteria` (rust, python) and
`the_linked_index_answers_every_expectation_with_vtsls` (typescript) against
the same fixture and `expect.toml`, each as a full language-server session.
In the batch-end run these took 33.0 s (rust), 9.8 s (python) and 8.5 s
(typescript) locally. The CI loop repeats about that much per runner; this
is an estimate, since CI step times were not measured. GM-548 and GM-552
change only the `local` nextest profile. The `ci` profile still runs both
rust conformance tests, so this duplication in CI is unchanged.

The two runs are not identical:

- the nextest tests use a generated manifest plus the shipped
  `[plugin.semantic]` section, and also assert that no expectation was
  skipped;
- the CI loop is the only run of the shipped `plugin.toml` as written,
  including its own command resolution.

Go has no nextest conformance test, so it must stay in the loop.

**The change.** (b) Limit the loop to plugins without a nextest conformance
test, which today is go only (for example an explicit `for plugin in plugins/go`,
or a skip list naming the other three). The alternatives are (a) keep both,
or (c) keep the loop and drop the nextest semantic tests.

**Example.** A change to `plugins/rust/plugin.toml`'s `[plugin.semantic]`
command that breaks only the shipped manifest's command resolution: with
(a), the CI loop catches it. With (b), it is caught only if the core
manifest tests (`core/src/daemon/manifest/tests.rs`, handshake and manifest
tests on the real files) cover that field.

**Consequence.** (b) saves about 50 s of language-server sessions per
runner, on 4 runners (an estimate), and loses the only end-to-end run of the
3 shipped manifests as written. (a) keeps both, at that cost. S3 recommends
(b) if the shipped-manifest coverage of the core manifest tests is accepted
as enough. Since that coverage was not checked field by field, this
decision should include a look at whether those tests read
`[plugin.semantic].command`.

### 3.4 The 3 `#[ignore]` measurement harnesses and their census hooks

**Today.** `rust/src/census.rs::open_site_census` and
`python/src/census.rs::open_site_census` (GM-314, need `GM314_CORPUS`), and
`rust/tests/semantic_pass_measurement.rs::whole_project_pass_cost` (GM-319,
needs `GM319_CORPUS` and a real rust-analyzer). They have no assertions and
never run in CI. One correction to S3: the census module is declared
`#[cfg(test)] pub(crate) mod census;` in both plugins' `lib.rs`, and every
hook carries `#[cfg(test)]`, so **nothing is compiled into the plugin
binaries**. The hooks are still woven through production source: 36
`census::` references in `rust/src/extractor/bodies.rs`, 1 in rust
`decls.rs`, 23 in `python/src/extractor/bodies.rs` and 1 in python
`decls.rs`. The modules are 372 + 273 lines, plus 128 lines of harness. The
run commands are documented in `docs/architecture/multi-language-plugins.md`
(l.1375-1378, l.1446) and `docs/architecture/gm-488-l4-fields-and-chains.md`
(l.284).

**The change.** Either (a) keep them as tools, or (b) delete each harness
with its module, its hooks and the doc commands, and leave the GM-314/GM-319
results in the docs as history.

**Example.** A reader of `rust/src/extractor/bodies.rs` today steps over
about 36 `#[cfg(test)] crate::census::...` lines that do not affect what the
plugin emits. With (b), those lines are gone, and re-measuring the open-site
census later means restoring the harness from git history.

**Consequence.** (a) costs readability, not runtime or binary size. (b)
makes the extractor code shorter, but it is a production-source edit across
two extractors, and the measurement can then only be repeated from history.
These are not test redundancy, so the time saving is 0 either way. Suggested:
(a), unless the extractor bodies are being refactored anyway.

## 4. Proposed follow-up task (not created)

**Title.** Remove the redundant tests found by the GM-541 audit

**Scope.** The 43 removals of section 2 that the owner approves (33 deletes,
9 merges, 1 move-to-unit), plus any decisions from section 3 that are
accepted. It is a tests-only task, unless 3.1, 3.2 or 3.4 is accepted. Side
edits that go with it:

- remove the `.config/nextest.toml` `local` heavy-list entry for S2#7, and
  the GM-552 entry for S3#12 if GM-552 has merged by then.
  `the_local_profile_skips_exactly_its_heavy_list` catches a stale entry;
- update the doc references to deleted test names:
  `docs/adr/0021-per-language-bulk-outcome.md` (S1#13),
  `docs/architecture/gm-527-file-end-line.md` (S3#2),
  `docs/architecture/gm-325-typescript-lsp-semantics.md` (S3#6, S3#7). The
  `docs/results/gm-548-*` mentions are historical measurements and stay.

**Slicing sketch.**

1. **core-unit** (implement): S1's 13 deletes and 1 merge (S1#10).
   Targeted runs of the touched modules only.
2. **core-integration** (implement): S2's 11 deletes, 2 merges (S2#5,
   S2#9), the move-to-unit (S2#3), and the heavy-list and doc edits. If 3.2
   is accepted, the bulk_index refactor and the S2#10 rewrites go here, or
   in their own slice if it grows.
3. **plugins-wire** (implement): S3's 9 deletes and 6 merges.
   `assert_report_shape` goes into both linked conformance tests. A real
   rust-analyzer and pyright are needed for the targeted conformance runs.
   If 3.3 is accepted, the ci.yml edit goes here, and 3.4 too if accepted.
4. **verify**: one background script that builds the changed test binaries,
   runs the changed files' tests, and runs 6-8 controls in a throwaway
   worktree. Each control must fail a test that survives the change:
   - c07 against the merged S2#9 assertion;
   - c09 and c10 against the parametrised lsp_bridge test;
   - new controls for the merges that have none yet: S2#5 (`index.db`),
     S3#11 (v1 input), S3#12 (`assert_report_shape`), S3#10 and S1#10;
   - if 3.1 is accepted, the clamp-removal control.

   No full suite for a tests-only result; the crates' suites only if
   production code changed (3.1, 3.2 or 3.4).

## 5. Coverage limits

- **Judged by name and scan only**, with no body-level read. A semantic
  duplicate written in different words with a different fixture would be
  missed here:
  - core/src: `graph/symbol_links/tests.rs` (109), `mcp/find_definition/tests.rs`
    (88), `daemon/manifest/tests.rs` (59), `mcp/instructions/tests.rs` (57),
    `mcp/get_dependencies/tests.rs` (47);
  - core/tests: `release_packaging_scripts.rs` (31), most of
    `plugin_check.rs` (41), `plugin_install.rs` (14),
    `incremental_matches_full_reindex.rs` (8), `plugin_memory_limit.rs`,
    `orphaned_daemon.rs`, `wedged_daemon.rs`, `idle_lifecycle.rs`,
    `daemon_lifeline.rs`, `daemon_build_staleness.rs`,
    `plugin_build_staleness.rs`, `cli_status.rs`, the import and linking
    acceptance files (`import_resolution`, `dynamic_import_resolution`,
    `default_export_linking`, `reexport_*`, `generic_reference_resolution`,
    `python_later_binding`, `rust_python_qualified_paths`,
    `files_created_*`, `named_glob_shadowing`, `walk_follows_symlinks`),
    `cut_release_plugin_versions.rs`, `go_plugin_fingerprint.rs`;
  - plugins: `typescript/tests/project_model.rs` (141),
    `sdk/tests/lsp_bridge.rs` (87, beyond the flagged pairs),
    `rust/src/extractor/tests.rs` (84), `python/src/extractor/tests.rs`
    (63), `python/src/project/mod.rs` (28).
- **2 SDK tests** were not body-scanned (287 extracted vs 289 `#[test]` by
  grep).
- **plugins/go was not audited**: 14 `*_test.go` files, about 125 Go tests.
  CI checks Go through `go test ./...` and `plugins check`.
- **Controls cover 10 of 47 candidates.** 35 candidates rest on reading only (the
  "reading (weaker)" rows), and 2 on construction. S6 left out S2#4 and
  S2#7 by choice: S2#4's covering test is named, and the claim for S2#7 is
  structural.
- **Timing** comes from one run at low load (section 1); the CI-loop
  estimate in 3.3 is a local measurement, not a CI one.
