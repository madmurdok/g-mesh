//! `g-mesh status --json`: the whole [`Report`] as one JSON object.
//!
//! Built by explicit mapping, not `#[derive(Serialize)]`, so the JSON names
//! are a contract of their own: renaming a Rust field or variant cannot
//! change them. Keys are camelCase (as the wire protocol), state values
//! snake_case (as the `language_outcome.outcome` column). `formatVersion`
//! changes only when a key is removed or renamed; adding keys does not.

use serde_json::{json, Value};

use super::{live_progress, BuildState, CoreState, LanguageRow, LanguageSection, Mode, PluginState, Report};
use crate::languages::LanguageOutcome;

/// The `formatVersion` this build writes.
pub const FORMAT_VERSION: u32 = 1;

/// The report as `g-mesh status --json` prints it.
pub fn to_json(report: &Report) -> Value {
    let daemon_alive = !matches!(report.core, CoreState::NotRunning);
    json!({
        "formatVersion": FORMAT_VERSION,
        "mode": match report.mode {
            Mode::Light => "light",
            Mode::Full => "full",
        },
        "projectRoot": report.project_root.display().to_string(),
        "projectId": report.project_id,
        "stateDir": report.state_dir.display().to_string(),
        "daemon": daemon(report.core, report.build),
        "plugins": report.plugins.iter().map(|plugin| {
            let (state, pid) = match plugin.state {
                PluginState::Active { pid } => ("active", pid),
                PluginState::Orphaned { pid } => ("orphaned", pid),
            };
            json!({ "language": plugin.language, "state": state, "pid": pid })
        }).collect::<Vec<_>>(),
        "suspended": report.suspended_languages.iter().map(|suspended| {
            json!({ "language": suspended.language, "reason": suspended.reason })
        }).collect::<Vec<_>>(),
        "lastUsed": report.last_used.as_ref().map(|last_used| json!({
            "timestamp": last_used.timestamp,
            "idleSeconds": last_used.idle.as_secs(),
        })),
        "index": if report.front.is_some() { Value::Null } else { index(report, daemon_alive) },
        "front": report.front.map(|front| json!({
            "projects": front.projects,
            "truncated": front.truncated,
        })),
        "languages": languages(report),
    })
}

fn daemon(core: CoreState, build: BuildState) -> Value {
    let (state, pid) = match core {
        CoreState::Running { pid } => ("running", Some(pid)),
        CoreState::NotAccepting { pid } => ("not_accepting", Some(pid)),
        CoreState::Wedged { pid } => ("wedged", Some(pid)),
        CoreState::NotRunning => ("not_running", None),
    };
    let build = match build {
        BuildState::NotRunning => None,
        BuildState::Current => Some("current"),
        BuildState::Outdated => Some("outdated"),
        BuildState::PluginChanged => Some("plugin_changed"),
        BuildState::Unknown => Some("unknown"),
    };
    json!({ "state": state, "pid": pid, "build": build })
}

/// The `index` object. A phase word or progress file whose daemon is not
/// running is a leftover, so both read `null` then, as in `render`.
fn index(report: &Report, daemon_alive: bool) -> Value {
    let index = &report.index;
    let phase = if daemon_alive { report.phase.as_deref() } else { None };
    let progress = live_progress(report).map(|progress| {
        json!({
            "phase": progress.phase,
            "walk": {
                "languagesDone": progress.walk.languages_done,
                "languagesTotal": progress.walk.languages_total,
                "currentLanguage": progress.walk.current_language,
                "items": progress.walk.items,
            },
            "semantic": {
                "languagesDone": progress.semantic.languages_done,
                "languagesTotal": progress.semantic.languages_total,
                "currentLanguage": progress.semantic.current_language,
            },
            "embeddings": {
                "done": progress.embeddings.done,
                "total": progress.embeddings.total,
            },
        })
    });
    json!({
        "phase": phase,
        "bulkIndexed": index.bulk_indexed,
        "progress": progress,
        "semanticPass": {
            "completed": index.semantic_pass_completed,
            "owed": index.semantic_pass_owed,
            "failures": index.semantic_pass_failures.iter().map(|(language, reason)| {
                json!({ "language": language, "reason": reason })
            }).collect::<Vec<_>>(),
            "pending": index.semantic_pending.iter().map(|(language, since, files)| {
                json!({ "language": language, "since": since, "files": files })
            }).collect::<Vec<_>>(),
            "pendingReindex": index.pending_reindex.iter().map(|(language, trigger)| {
                json!({ "language": language, "trigger": trigger })
            }).collect::<Vec<_>>(),
            "leftovers": index.semantic_leftovers.iter().map(|leftover| json!({
                "language": leftover.language,
                "residualFiles": leftover.residual_files,
                "neverAnswered": leftover.never_answered,
            })).collect::<Vec<_>>(),
        },
        "syntaxErrorFiles": index.syntax_error_files,
        "coverage": index.coverage.map(|coverage| json!({
            "discovered": coverage.discovered,
            "indexed": coverage.indexed,
            "dirty": coverage.dirty,
        })),
    })
}

fn languages(report: &Report) -> Value {
    let languages = &report.languages;
    let state = match &languages.section {
        LanguageSection::NoIndex => "no_index",
        LanguageSection::Front => "front",
        LanguageSection::PredatesOutcomes { .. } => "predates_outcomes",
        LanguageSection::WalkInProgress => "walk_in_progress",
        LanguageSection::NoneRecorded => "none_recorded",
        LanguageSection::Recorded(_) => "recorded",
    };
    let mut value = json!({
        "state": state,
        "pluginDiscoveryError": languages.installed.as_ref().err(),
        "outcomes": languages.rows().iter().map(outcome).collect::<Vec<_>>(),
    });
    if let LanguageSection::PredatesOutcomes { schema_version } = &languages.section {
        value["schemaVersion"] = json!(schema_version);
    }
    value
}

/// One `outcomes` entry. The keys depend on `outcome`: `files` for
/// `indexed` and `plugin_absent`, `installCommand` for `plugin_absent`
/// (`null` once the plugin is installed, or for a language outside the
/// catalogue), `error` and `causes` for `failed`.
fn outcome(row: &LanguageRow<'_>) -> Value {
    let mut value = json!({
        "language": row.language,
        "pluginVersion": row.plugin_version,
    });
    match row.outcome {
        None => {
            value["outcome"] = json!("not_in_last_walk");
        }
        Some(LanguageOutcome::Indexed { files }) => {
            value["outcome"] = json!("indexed");
            value["files"] = json!(files);
        }
        Some(LanguageOutcome::PluginAbsent { files }) => {
            value["outcome"] = json!("plugin_absent");
            value["files"] = json!(files);
            let install = crate::languages::entry(row.language)
                .filter(|_| row.plugin_version.is_none())
                .map(|entry| entry.install_command());
            value["installCommand"] = json!(install);
        }
        Some(LanguageOutcome::Failed { error }) => {
            value["outcome"] = json!("failed");
            value["error"] = json!(crate::languages::error_on_one_line(error));
            value["causes"] =
                json!(error.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>());
        }
    }
    value
}
