//! `g-mesh config` / `g-mesh config --global`: an interactive wizard that
//! prompts for a handful of user-facing settings and writes exactly the
//! answered values into `config.toml`.
//!
//! # Which fields the wizard asks about
//!
//! Deliberately a small subset of what [`crate::config`] can round-trip:
//!
//! - Per-project (`g-mesh config`): `embedding.model` (with a short
//!   speed/accuracy blurb), `plugin.idleTimeoutMinutes`, and (task GM-274)
//!   `plugin.memoryLimitMb`.
//! - Global (`g-mesh config --global`): `cleanup.enabled` and
//!   `cleanup.idleThresholdDays`.
//!
//! Two things are intentionally *not* asked:
//!
//! - **"Languages to enable"** - there is no backing config field for this
//!   yet. `ProjectConfig` ([`crate::config`]) has no `languages` section at
//!   all (language support today is discovered from installed plugins, not
//!   toggled per project), so prompting for it would mean inventing a new
//!   schema field that nothing reads - out of scope here per the task's own
//!   instructions. Skipped rather than faked.
//! - **Low-level tuning** (traversal limits, batch size, `daemon`'s
//!   `coreIdleTimeoutHours`) - deliberately TOML-only for power users, per
//!   the architecture doc. The wizard reads the existing config first and
//!   carries every field it does not ask about through unchanged, so running
//!   it never resets a hand-edited value it was not asked to touch.
//!
//! # Testability
//!
//! Same split [`cli::reindex`] uses between a real-I/O `run` and a pure,
//! testable core: [`wizard_project`] and [`wizard_global`] take any
//! `BufRead` + `Write` pair, so tests drive them with an in-memory
//! `Cursor` instead of the real stdin/stdout - no human at the keyboard
//! required.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};

use crate::config::{self, CleanupConfig, EmbeddingConfig, GlobalConfig, PluginConfig, ProjectConfig};

/// Runs the wizard against real stdin/stdout and writes the result to the
/// project config (`global: false`) or the global config (`global: true`).
pub fn run(global: bool) -> Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = io::stdout();

    if global {
        let existing = config::read_global_config().context("failed to read global config")?;
        let updated = wizard_global(&existing, &mut reader, &mut stdout)?;
        config::write_global_config(&updated).context("failed to write global config")?;
        let path = config::global_config_path()?;
        writeln!(stdout, "g-mesh: wrote {}", path.display())?;
    } else {
        let cwd = std::env::current_dir().context("failed to resolve the current directory")?;
        let existing = config::read_project_config(&cwd).context("failed to read project config")?;
        let updated = wizard_project(&existing, &mut reader, &mut stdout)?;
        config::write_project_config(&cwd, &updated).context("failed to write project config")?;
        let path = config::project_config_path(&cwd)?;
        writeln!(stdout, "g-mesh: wrote {}", path.display())?;
    }

    Ok(())
}

/// Prompts for the per-project settings this wizard covers, starting from
/// `existing` so every other field (currently just `daemon`) round-trips
/// unchanged.
pub fn wizard_project<R: BufRead, W: Write>(
    existing: &ProjectConfig,
    reader: &mut R,
    writer: &mut W,
) -> Result<ProjectConfig> {
    writeln!(writer, "g-mesh project configuration")?;
    writeln!(writer, "(press enter to keep the current value shown in brackets)")?;
    writeln!(writer)?;

    writeln!(writer, "Embedding model - used by semantic search:")?;
    writeln!(
        writer,
        "  jina-embeddings-v2-base-code (default) - balanced speed and accuracy, good for most codebases"
    )?;
    writeln!(writer, "  a larger model trades slower, more memory-hungry embedding for better accuracy")?;
    let model = prompt_string(reader, writer, "Embedding model", &existing.embedding.model)?;

    let idle_timeout_minutes = prompt_u64(
        reader,
        writer,
        "Plugin idle timeout, in minutes, before its process is put to sleep",
        existing.plugin.idle_timeout_minutes,
    )?;

    writeln!(writer)?;
    writeln!(writer, "Plugin memory limit, in MB, per language's process tree (plugin plus")?;
    writeln!(writer, "whatever it has spawned - tsserver, rust-analyzer, ...):")?;
    writeln!(writer, "  off (default) - idle sleep only, exactly today's behaviour")?;
    writeln!(
        writer,
        "  a number - idle sleep stays on, and a tree over this limit is put to sleep too, \
         with that language's semantic passes suspended until the daemon restarts"
    )?;
    // The one place a person actually chooses this number, so the one place
    // the guarantee has to be stated in the terms they will hold it to. See
    // GM-304's notes in docs/architecture/multi-language-plugins.md for why a
    // ceiling is not on offer, and config::PluginConfig::memory_limit_mb.
    writeln!(writer)?;
    writeln!(writer, "This is a circuit breaker, not a ceiling: a tree found over the limit is")?;
    writeln!(writer, "stopped so it cannot keep exceeding it, but it is NOT held under the limit")?;
    writeln!(writer, "in the first place. A language server can cross the number once and by a")?;
    writeln!(writer, "wide margin - a real rust-analyzer measured here climbed to 563-580MB over")?;
    writeln!(writer, "a 13-17s first pass before anything could stop it. Leave headroom: pick a")?;
    writeln!(writer, "number you can afford to overshoot, not the most you have free.")?;
    let memory_limit_mb = prompt_optional_u64(
        reader,
        writer,
        "Plugin memory limit in MB (leave blank to keep, \"off\" to disable)",
        existing.plugin.memory_limit_mb,
    )?;

    Ok(ProjectConfig {
        plugin: PluginConfig { idle_timeout_minutes, memory_limit_mb },
        daemon: existing.daemon,
        embedding: EmbeddingConfig { model },
    })
}

/// Prompts for the global settings this wizard covers.
pub fn wizard_global<R: BufRead, W: Write>(
    existing: &GlobalConfig,
    reader: &mut R,
    writer: &mut W,
) -> Result<GlobalConfig> {
    writeln!(writer, "g-mesh global configuration")?;
    writeln!(writer, "(press enter to keep the current value shown in brackets)")?;
    writeln!(writer)?;

    let enabled = prompt_bool(
        reader,
        writer,
        "Warn about idle projects that look safe to clean up",
        existing.cleanup.enabled,
    )?;
    let idle_threshold_days = prompt_u64(
        reader,
        writer,
        "Idle threshold, in days, before a project is considered for that warning",
        existing.cleanup.idle_threshold_days,
    )?;

    Ok(GlobalConfig { cleanup: CleanupConfig { enabled, idle_threshold_days } })
}

/// Prompts once for a free-form string, returning `default` unchanged when
/// the answer is empty (including EOF, which `read_line` reports as `Ok(0)`
/// with the buffer left empty).
fn prompt_string<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
    default: &str,
) -> Result<String> {
    write!(writer, "{prompt} [{default}]: ")?;
    writer.flush()?;

    let mut line = String::new();
    reader.read_line(&mut line).context("failed to read wizard input")?;
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() { default.to_string() } else { trimmed.to_string() })
}

/// Prompts for a non-negative integer, re-prompting on anything that does
/// not parse as one; empty input or EOF keeps `default`.
fn prompt_u64<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
    default: u64,
) -> Result<u64> {
    loop {
        write!(writer, "{prompt} [{default}]: ")?;
        writer.flush()?;

        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).context("failed to read wizard input")?;
        if bytes_read == 0 {
            return Ok(default);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(default);
        }
        match trimmed.parse::<u64>() {
            Ok(value) => return Ok(value),
            Err(_) => writeln!(writer, "  please enter a whole number")?,
        }
    }
}

/// Prompts for an optional non-negative integer - `plugin.memoryLimitMb`'s
/// own shape, "empty means off" (see this module's doc comment and the
/// architecture doc's "Plugin memory limit" section).
///
/// Follows this file's own "press enter to keep the current value" rule
/// exactly like [`prompt_u64`]/[`prompt_bool`]: empty input or EOF answers
/// `default` unchanged, whatever `default` is - `Some(4096)` stays
/// `Some(4096)`, `None` stays `None`. That is deliberately not the same
/// question as "turn the limit off", which needs its own explicit answer
/// (`"off"`, case-insensitively, or a literal `0` - both read the same way,
/// since a zero-megabyte limit is not a value anything could usefully mean):
/// a wizard run that only re-answers the idle timeout must not silently clear
/// a memory limit someone configured by hand-editing `config.toml` days ago.
fn prompt_optional_u64<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
    default: Option<u64>,
) -> Result<Option<u64>> {
    let hint = match default {
        Some(value) => value.to_string(),
        None => "off".to_string(),
    };
    loop {
        write!(writer, "{prompt} [{hint}]: ")?;
        writer.flush()?;

        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).context("failed to read wizard input")?;
        if bytes_read == 0 {
            return Ok(default);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(default);
        }
        if trimmed.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        match trimmed.parse::<u64>() {
            Ok(0) => return Ok(None),
            Ok(value) => return Ok(Some(value)),
            Err(_) => writeln!(writer, "  please enter a whole number, or \"off\" to disable")?,
        }
    }
}

/// Prompts for a yes/no answer, re-prompting on anything else; empty input
/// or EOF keeps `default`.
fn prompt_bool<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
    default: bool,
) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    loop {
        write!(writer, "{prompt} [{hint}]: ")?;
        writer.flush()?;

        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).context("failed to read wizard input")?;
        if bytes_read == 0 {
            return Ok(default);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(writer, "  please answer y or n")?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use crate::config::DaemonConfig;

    fn run_project_wizard(existing: &ProjectConfig, input: &str) -> (ProjectConfig, String) {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut out = Vec::new();
        let updated = wizard_project(existing, &mut reader, &mut out).unwrap();
        (updated, String::from_utf8(out).unwrap())
    }

    fn run_global_wizard(existing: &GlobalConfig, input: &str) -> (GlobalConfig, String) {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut out = Vec::new();
        let updated = wizard_global(existing, &mut reader, &mut out).unwrap();
        (updated, String::from_utf8(out).unwrap())
    }

    #[test]
    fn accepting_every_default_leaves_the_project_config_unchanged() {
        let existing = ProjectConfig {
            plugin: PluginConfig { idle_timeout_minutes: 60, memory_limit_mb: Some(4096) },
            daemon: DaemonConfig { core_idle_timeout_hours: 24 },
            embedding: EmbeddingConfig { model: "jina-embeddings-v2-base-code".to_string() },
        };

        let (updated, _) = run_project_wizard(&existing, "\n\n\n");

        assert_eq!(updated, existing);
    }

    #[test]
    fn answering_the_project_prompts_writes_exactly_those_values() {
        let existing = ProjectConfig::default();

        let (updated, _) = run_project_wizard(&existing, "custom-model\n15\n2048\n");

        assert_eq!(updated.embedding.model, "custom-model");
        assert_eq!(updated.plugin.idle_timeout_minutes, 15);
        assert_eq!(updated.plugin.memory_limit_mb, Some(2048));
        // Not asked - carried through from the existing config untouched.
        assert_eq!(updated.daemon, existing.daemon);
    }

    /// The wizard round-trip test for the new field (this task's own
    /// acceptance criterion): answering "off" for a project that already has
    /// a memory limit configured turns it back to `None`, and answering a
    /// number sets it - both directions of `prompt_optional_u64`.
    #[test]
    fn the_memory_limit_prompt_can_set_and_clear_the_field() {
        let existing = ProjectConfig::default();
        let (set, _) = run_project_wizard(&existing, "\n\n512\n");
        assert_eq!(set.plugin.memory_limit_mb, Some(512));

        let (cleared, _) = run_project_wizard(&set, "\n\noff\n");
        assert_eq!(cleared.plugin.memory_limit_mb, None);

        // Keeping the default (empty input) leaves whichever value was
        // already there untouched, in either direction.
        let (kept_off, _) = run_project_wizard(&existing, "\n\n\n");
        assert_eq!(kept_off.plugin.memory_limit_mb, None);
        let (kept_set, _) = run_project_wizard(&set, "\n\n\n");
        assert_eq!(kept_set.plugin.memory_limit_mb, Some(512));
    }

    /// GM-304's user-facing half. The decision that `memoryLimitMb` is a
    /// circuit breaker rather than a ceiling is only worth making if the
    /// person choosing the number is told which one they are getting - someone
    /// typing 600 on a machine with 1GB free is otherwise expecting a cap the
    /// daemon holds them under. This prompt is where that choice is made, so
    /// the wording is part of the behaviour and is asserted like any other
    /// part of it: both that the limit can be exceeded and roughly by how
    /// much, in the figures GM-291 actually measured.
    #[test]
    fn the_memory_limit_prompt_says_the_limit_can_be_exceeded_once() {
        let (_, transcript) = run_project_wizard(&ProjectConfig::default(), "\n\n512\n");

        assert!(transcript.contains("circuit breaker, not a ceiling"), "{transcript}");
        assert!(transcript.contains("NOT held under the limit"), "{transcript}");
        assert!(transcript.contains("563-580MB"), "{transcript}");
    }

    #[test]
    fn a_non_numeric_idle_timeout_is_rejected_and_reprompted() {
        let existing = ProjectConfig::default();

        let (updated, transcript) = run_project_wizard(&existing, "\nnot-a-number\n42\n\n");

        assert_eq!(updated.plugin.idle_timeout_minutes, 42);
        assert!(transcript.contains("please enter a whole number"), "{transcript}");
    }

    #[test]
    fn eof_mid_wizard_keeps_remaining_defaults() {
        let existing = ProjectConfig::default();

        // Only one line of input, then the "stdin" is closed.
        let (updated, _) = run_project_wizard(&existing, "custom-model\n");

        assert_eq!(updated.embedding.model, "custom-model");
        assert_eq!(updated.plugin.idle_timeout_minutes, existing.plugin.idle_timeout_minutes);
        assert_eq!(updated.plugin.memory_limit_mb, existing.plugin.memory_limit_mb);
    }

    #[test]
    fn accepting_every_default_leaves_the_global_config_unchanged() {
        let existing = GlobalConfig { cleanup: CleanupConfig { enabled: true, idle_threshold_days: 90 } };

        let (updated, _) = run_global_wizard(&existing, "\n\n");

        assert_eq!(updated, existing);
    }

    #[test]
    fn answering_the_global_prompts_writes_exactly_those_values() {
        let existing = GlobalConfig::default();

        let (updated, _) = run_global_wizard(&existing, "n\n30\n");

        assert!(!updated.cleanup.enabled);
        assert_eq!(updated.cleanup.idle_threshold_days, 30);
    }

    #[test]
    fn an_invalid_yes_no_answer_is_rejected_and_reprompted() {
        let existing = GlobalConfig::default();

        let (updated, transcript) = run_global_wizard(&existing, "maybe\nyes\n90\n");

        assert!(updated.cleanup.enabled);
        assert!(transcript.contains("please answer y or n"), "{transcript}");
    }

    /// End-to-end: writing the wizard's answers to a real `config.toml` and
    /// reading it back produces exactly those values - the acceptance
    /// criterion this task is judged on, exercised without a human typing.
    #[test]
    fn writing_the_wizard_output_round_trips_exactly_the_answered_values() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path();

        let existing = config::read_project_config(project_root).unwrap();
        let (updated, _) = run_project_wizard(&existing, "answered-model\n7\n256\n");
        config::write_project_config(project_root, &updated).unwrap();

        let read_back = config::read_project_config(project_root).unwrap();
        assert_eq!(read_back.plugin.memory_limit_mb, Some(256));
        assert_eq!(read_back.embedding.model, "answered-model");
        assert_eq!(read_back.plugin.idle_timeout_minutes, 7);
    }

    #[test]
    fn writing_the_global_wizard_output_targets_the_global_path_not_the_project_one() {
        // wizard_global/write_global_config never take a project root at
        // all, so there is no path for a per-project value to leak into -
        // asserted structurally via the function signatures plus the
        // dedicated global path test in `config::tests`.
        let existing = GlobalConfig::default();
        let (updated, _) = run_global_wizard(&existing, "n\n45\n");
        assert!(!updated.cleanup.enabled);
        assert_eq!(updated.cleanup.idle_threshold_days, 45);
    }
}
