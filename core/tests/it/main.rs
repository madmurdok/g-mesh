//! Core's integration tests that exercise the product: the library, the
//! `g-mesh` binary, its daemons, shims and plugins. One binary rather than one
//! per file, because each binary is a full link of the crate (GM-623).
//!
//! Placement rule (GM-623): a core integration test whose subject is a file
//! under `scripts/` (it runs that script, or parses its output) goes to
//! `core/tests/scripts/`; every other one goes to `core/tests/it/`. Either way
//! it is a module here, so a new file needs its own `mod <file>;` line below,
//! and `it/layout.rs` fails, naming the file, on one that has none or on a new
//! top-level `core/tests/*.rs` (which cargo would build as a binary of its own).
//!
//! Run one file's tests:
//! `cargo nextest run -p g-mesh --test it -E 'test(/^<file>::/)'`.

mod absent_language_field;
mod ambiguous_reexport_linking;
mod cli_clean;
mod cli_clean_sweeping;
mod cli_init;
mod cli_reindex;
mod cli_status;
mod cli_stop;
mod common;
mod config_reindex_go;
mod config_reindex_python;
mod config_reindex_rust;
mod config_reindex_typescript;
mod daemon_build_staleness;
mod daemon_core;
mod daemon_lifeline;
mod daemon_log_lines;
mod daemon_missing_plugin_binary;
mod daemon_plugin_bin_dir;
mod daemon_plugin_discovery_failure;
mod daemon_sigterm;
mod default_export_linking;
mod dynamic_import_resolution;
mod embedding_generation_pipeline;
mod embedding_peak_memory;
mod files_created_python;
mod files_created_typescript;
mod first_query_after_walk;
mod generic_reference_resolution;
mod gitignore_reevaluation;
mod go_plugin_fingerprint;
mod handshake_independent_of_indexing;
mod idle_lifecycle;
mod import_resolution;
mod incremental_embed_outside_lock;
mod incremental_matches_full_reindex;
mod index_wait_progress;
mod last_used;
mod layout;
mod lazy_activation;
mod log_line_atomic;
mod mcp_e2e;
mod module_tree_refresh_rust;
mod multi_project_front;
mod named_glob_shadowing;
mod namespace_import_after_init;
mod namespace_import_resolution;
mod nextest_wait_budgets;
mod offline_changes_reconcile;
mod one_plugin_binary_missing;
mod orphaned_daemon;
mod overload_call_binding;
mod overload_call_resolution;
mod overload_declaration_storage;
mod plugin_bridge;
mod plugin_build_staleness;
mod plugin_check;
mod plugin_crash_recovery;
mod plugin_install;
mod plugin_memory_limit;
mod plugins_die_with_daemon;
mod protocol_conformance;
mod python_later_binding;
mod reexport_linking;
mod reexport_member_linking;
mod repeated_edits_through_a_warm_plugin;
mod replay_progress;
mod rust_glob_trait_overrides;
mod rust_python_qualified_paths;
mod semantic_pass_trigger;
mod serving_while_indexing;
mod shim_bootstrap;
mod shim_handle_inheritance;
mod stale_index_invalidation;
mod state_isolation;
mod structural_does_not_wait_for_embeddings;
mod symlink_table_routing;
mod typescript_grammar_routing;
#[cfg(unix)] // every user of it is a `#![cfg(unix)]` file
mod typescript_registry;
mod typescript_workspace_reindex;
mod walk_follows_symlinks;
mod wedged_daemon;
mod worktree_daemon_lifecycle;
mod worktree_seed;
