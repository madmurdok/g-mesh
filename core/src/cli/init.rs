//! `g-mesh init`: bootstrap the current project's g-mesh state up front,
//! before any agent has asked it a question.
//!
//! Nothing here is required. The zero-config path already covers a project
//! that has never run this command: the first MCP call the shim proxies
//! bootstraps a detached daemon (`shim::connect_or_bootstrap`), and that
//! daemon's own cold start (`daemon::run`) creates the same
//! `~/.g-mesh/projects/<hash>/` directory, writes the same schema, and walks
//! the project the same way this command does. `init` exists for someone who
//! would rather pay that cost - a possibly-large bulk walk - on their own
//! schedule from a terminal, and see it finish, than have it land silently
//! inside their first real agent query.
//!
//! # Why this walks synchronously instead of bootstrapping a daemon
//!
//! The other shape this command could have taken is `shim::spawn_detached_daemon`:
//! start a daemon in the background and let its own cold-start walk (which
//! runs exactly this same [`bulk_index::run`]) happen off to the side. That
//! was rejected because it does not actually satisfy what `init` is for. The
//! whole point of pre-indexing is to know the index is ready *before* the
//! first agent query - a command that returns the instant a background
//! process has merely started gives no such guarantee, and would need
//! `status` polling to find out afterwards whether it is actually safe to go
//! ask something. Running the walk in the foreground, the same way
//! `cli::reindex` already does, means `init` finishing *is* the index being
//! ready: nothing left to poll, no daemon left running for `init` to justify
//! leaving behind. The next real MCP call still bootstraps its own daemon
//! exactly as it always has, and finds an index that is already current and
//! already fully walked - the same postcondition `cli::reindex`'s module
//! doc describes.
//!
//! # Why the walk is followed by a semantic pass
//!
//! Because "ready" has to mean the same thing here as it does anywhere else.
//! A bulk walk produces the graph tree-sitter can see, and `daemon::run`
//! follows its own walk with a whole-project semantic pass that produces the
//! rest - the call edges hidden behind `import * as ns`, the re-export chains
//! core's name-matching walk cannot finish, the overload a call really binds.
//! That daemon-side pass is reached only on a cold start, and the very record
//! this command writes (`meta.bulkIndexedAt`) is what tells every later daemon
//! it owes no cold start. So an `init`ed project used to keep a permanently
//! structural-only graph, and `find_callers` on a function reached only
//! through a namespace import answered nothing on an index that called itself
//! complete. `daemon::semantic` holds the rest of that argument.
//!
//! # Why a daemon already running is stopped first
//!
//! Same reasoning as `cli::reindex`: this command opens the project's SQLite
//! index directly and writes to it outside of any daemon's supervision. A
//! live daemon holds that same file open and may be mid-write off its own
//! file watcher, so it is stopped first via `cli::stop`, the same safe,
//! wait-for-genuinely-gone shutdown `reindex` relies on. A project someone is
//! running `init` against for the first time will not normally have one, but
//! `init` is also safe to run again later - see below - and by then one may
//! well exist.
//!
//! # Idempotence
//!
//! Running `init` twice must not repeat expensive or destructive work it
//! already did:
//!
//! - `config.toml` is written only if none exists yet, so a config someone
//!   has since hand-tuned is never silently reset to defaults by a second
//!   `init`.
//! - The index is rebuilt from scratch (`schema::ensure_current`, which
//!   creates the schema on a fresh project and leaves an already-current one
//!   untouched) but the bulk walk itself is skipped when
//!   `schema::bulk_index_completed` already says the project has been fully
//!   walked - the same fact `daemon::run`'s own cold start checks before
//!   deciding whether it owes a walk.
//! - The semantic pass rides on that same decision rather than on one of its
//!   own: it follows a walk this command actually ran, so a second `init`
//!   starts no type checker either. It is the more expensive of the two on a
//!   large project (one round trip per question the walk left open), and
//!   nothing about a project that has not changed makes its answers stale.
//!   A project whose index predates this behaviour therefore stays as it is
//!   until something rebuilds it - `g-mesh reindex`, which runs the pass for
//!   the same reason this does.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::cli::{agent_instructions, stop, AgentTarget};
use crate::config::{self, ProjectConfig};
use crate::daemon::bulk_index::{self, BulkIndexSummary};
use crate::daemon::{manifest, registry, semantic};
use crate::embedding::EmbeddingPipeline;
use crate::storage::{connection, schema};

/// What a single `init` actually did.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub project_id: String,
    pub state_dir: PathBuf,
    /// Whether a daemon was serving this project and had to be stopped
    /// before `init` could safely touch its index.
    pub daemon_was_running: bool,
    /// Whether `config.toml` was written. `false` means one already existed
    /// and was left alone.
    pub config_written: bool,
    /// `None` when the project was already fully walked and the bulk index
    /// was skipped rather than repeated.
    pub summary: Option<BulkIndexSummary>,
    /// Whether the type checker got to say what the structural walk could
    /// not - see `daemon::semantic` for why a walk on its own leaves an
    /// index that is complete only in the shapes tree-sitter can see. False
    /// whenever there was no walk to follow (an already-indexed project),
    /// when no JS/TS plugin is installed, and when the pass failed - which
    /// costs exactly the edges it was about and is reported on stderr.
    pub semantic_pass_ran: bool,
    /// The `--agent` targets that were requested, carried through so
    /// [`render`] knows which of `agent_instructions`'s fields to report on.
    /// Empty means `--agent` was not used at all, in which case `render`
    /// prints nothing extra - `init`'s behavior is unchanged when the flag is
    /// unused.
    pub agents: Vec<AgentTarget>,
    pub agent_instructions: agent_instructions::Outcome,
}

/// Bootstraps the project the current directory belongs to.
pub fn run(agents: &[AgentTarget]) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve the current directory")?;
    let outcome = init(&cwd, agents)?;
    print!("{}", render(&outcome, &cwd));
    Ok(())
}

/// Sets `project_root`'s g-mesh state up: its state directory, a default
/// `config.toml` if it does not already have one, a fully walked index, and,
/// when `agents` is non-empty, the project-instruction files named in it (see
/// [`agent_instructions::apply`]).
///
/// Split out from [`run`] so it can be exercised against a temporary project
/// root without taking over the process's real cwd.
pub fn init(project_root: &Path, agents: &[AgentTarget]) -> Result<Outcome> {
    let stop_outcome = stop::stop(project_root)?;

    let state_dir = connection::project_dir(project_root)
        .context("failed to resolve the project's state directory")?;
    let project_id =
        state_dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();

    let config_path = config::project_config_path(project_root)
        .context("failed to resolve the project's config path")?;
    let config_written = !config_path.exists();
    if config_written {
        config::write_project_config(project_root, &ProjectConfig::default())
            .context("failed to write the project's config.toml")?;
    }

    // Discovered up front rather than inside the walk below, because the
    // generation this index is stamped with is derived from it (see
    // `daemon::registry::indexer_version`) - and stamping it with anything
    // other than what `daemon::run` would compute means the very next daemon
    // start throws away the index this command just paid to build. Same
    // discovery `daemon::run`'s own cold start uses; the walk below takes this
    // same value rather than repeating the scan.
    let discovered = manifest::discover(&manifest::default_roots())
        .context("failed to discover language plugins")?;

    let conn = connection::open(project_root)
        .context("failed to open the project's SQLite index")?;
    schema::ensure_current(&conn, &registry::indexer_version(&discovered))
        .context("failed to check the index's schema and indexer versions")?;
    let already_indexed = schema::bulk_index_completed(&conn)
        .context("failed to check whether the project has already been indexed")?;

    let mut semantic_pass_ran = false;
    let summary = if already_indexed {
        None
    } else {
        let canonical_root = project_root.canonicalize().with_context(|| {
            format!("failed to canonicalize project root {}", project_root.display())
        })?;
        let conn = Arc::new(Mutex::new(conn));
        // Read back rather than assumed from `ProjectConfig::default()`: a
        // pre-existing config.toml (the `!config_written` case) may name a
        // different model, and this walk has to honor whatever is actually
        // configured, exactly like `daemon::run`'s own cold start.
        let project_config = config::read_project_config(project_root)
            .context("failed to read the project's config.toml")?;
        let embedding_pipeline = EmbeddingPipeline::load(&project_config.embedding);
        // Discovery's result, resolved above, handed straight in - see
        // `daemon::bulk_index::run`'s doc comment for why it takes that
        // directly rather than a `PluginRegistry`.
        let summary = bulk_index::run(&canonical_root, &conn, &embedding_pipeline, &discovered)
            .context("failed to build the project's initial index")?;
        schema::record_bulk_index(&conn.lock().unwrap())
            .context("failed to record that the project was indexed")?;
        // The walk is only the half of a complete index that tree-sitter can
        // see, and the record just written is what stops any later daemon
        // start from finishing the other half - see `daemon::semantic`. So
        // this command owes the pass it just made unreachable, and running it
        // here is also the only way `init`'s promise stays true: that the
        // index is *ready* when it returns, not merely walked.
        match semantic::run_once(&canonical_root, &state_dir, &conn, &discovered, &embedding_pipeline) {
            Ok(ran) => semantic_pass_ran = ran,
            Err(err) => eprintln!(
                "g-mesh: the semantic pass over the freshly built index failed ({err:#}) - \
                 its edges keep whatever the structural pass resolved"
            ),
        }
        Some(summary)
    };

    let agent_instructions_outcome = agent_instructions::apply(project_root, agents)
        .context("failed to set up agent instruction files")?;

    Ok(Outcome {
        project_id,
        state_dir,
        daemon_was_running: stop_outcome.stopped_anything(),
        config_written,
        summary,
        semantic_pass_ran,
        agents: agents.to_vec(),
        agent_instructions: agent_instructions_outcome,
    })
}

/// Renders an outcome as the text the command prints.
pub fn render(outcome: &Outcome, project_root: &Path) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "g-mesh: initialized {}", project_root.display());
    let _ = writeln!(out, "  project id: {}", outcome.project_id);
    let _ = writeln!(out, "  state dir:  {}", outcome.state_dir.display());
    if outcome.daemon_was_running {
        let _ = writeln!(
            out,
            "  daemon:     stopped so init could rebuild safely - the next tool call starts a fresh one"
        );
    }
    if outcome.config_written {
        let _ = writeln!(out, "  config:     wrote config.toml with defaults");
    } else {
        let _ = writeln!(out, "  config:     existing config.toml left untouched");
    }
    match &outcome.summary {
        Some(summary) => {
            let _ = writeln!(
                out,
                "  index:      {} nodes, {} edges ({} imports linked, {} symbols linked)",
                summary.nodes, summary.edges, summary.linked_imports, summary.linked_symbols,
            );
            if summary.skipped_lines > 0 {
                let _ = writeln!(
                    out,
                    "  warning:    {} unreadable line(s) were skipped - the index may be incomplete",
                    summary.skipped_lines
                );
            }
        }
        None => {
            let _ = writeln!(out, "  index:      already fully indexed - nothing to do");
        }
    }
    if outcome.semantic_pass_ran {
        let _ = writeln!(out, "  semantic:   pass complete over what the walk could not resolve");
    }
    if !outcome.agents.is_empty() {
        if outcome.agent_instructions.agents_md_written {
            let _ = writeln!(out, "  AGENTS.md:  wrote the g-mesh code-search snippet");
        } else {
            let _ = writeln!(out, "  AGENTS.md:  already had the g-mesh snippet - left untouched");
        }
        for agent in &outcome.agents {
            match agent {
                AgentTarget::AgentsMd => {}
                AgentTarget::Claude => {
                    if outcome.agent_instructions.claude_md_written {
                        let _ = writeln!(out, "  CLAUDE.md:  wrote the @AGENTS.md bridge line");
                    } else {
                        let _ =
                            writeln!(out, "  CLAUDE.md:  already bridges to AGENTS.md - left untouched");
                    }
                }
                AgentTarget::Gemini => {
                    if outcome.agent_instructions.gemini_md_written {
                        let _ = writeln!(out, "  GEMINI.md:  wrote the @AGENTS.md bridge line");
                    } else {
                        let _ =
                            writeln!(out, "  GEMINI.md:  already bridges to AGENTS.md - left untouched");
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_project_reports_a_written_config_and_a_freshly_built_index() {
        let outcome = Outcome {
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            daemon_was_running: false,
            config_written: true,
            summary: Some(BulkIndexSummary {
                nodes: 10,
                edges: 4,
                skipped_lines: 0,
                linked_imports: 2,
                linked_symbols: 1,
            }),
            semantic_pass_ran: true,
            agents: Vec::new(),
            agent_instructions: agent_instructions::Outcome::default(),
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("initialized /tmp/project"), "{rendered}");
        assert!(rendered.contains("wrote config.toml with defaults"), "{rendered}");
        assert!(!rendered.contains("stopped so init could rebuild"), "{rendered}");
        assert!(rendered.contains("10 nodes, 4 edges (2 imports linked, 1 symbols linked)"), "{rendered}");
    }

    #[test]
    fn a_project_already_fully_indexed_skips_the_walk_and_says_so() {
        let outcome = Outcome {
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            daemon_was_running: false,
            config_written: false,
            summary: None,
            semantic_pass_ran: false,
            agents: Vec::new(),
            agent_instructions: agent_instructions::Outcome::default(),
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("existing config.toml left untouched"), "{rendered}");
        assert!(rendered.contains("already fully indexed - nothing to do"), "{rendered}");
    }

    #[test]
    fn a_daemon_stopped_for_the_rebuild_is_called_out() {
        let outcome = Outcome {
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            daemon_was_running: true,
            config_written: false,
            summary: Some(BulkIndexSummary::default()),
            semantic_pass_ran: false,
            agents: Vec::new(),
            agent_instructions: agent_instructions::Outcome::default(),
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("stopped so init could rebuild safely"), "{rendered}");
    }

    /// `--agent` unused (empty list) must print nothing extra - `init`'s
    /// behavior is unchanged from before this feature existed.
    #[test]
    fn no_agents_requested_prints_nothing_about_agent_instructions() {
        let outcome = Outcome {
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            daemon_was_running: false,
            config_written: false,
            summary: None,
            semantic_pass_ran: false,
            agents: Vec::new(),
            agent_instructions: agent_instructions::Outcome::default(),
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(!rendered.contains("AGENTS.md"), "{rendered}");
        assert!(!rendered.contains("CLAUDE.md"), "{rendered}");
        assert!(!rendered.contains("GEMINI.md"), "{rendered}");
    }

    /// Requested agents whose files were freshly written are reported as
    /// written, one line per file actually touched.
    #[test]
    fn requested_agents_that_were_written_are_reported() {
        let outcome = Outcome {
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            daemon_was_running: false,
            config_written: false,
            summary: None,
            semantic_pass_ran: false,
            agents: vec![AgentTarget::Claude, AgentTarget::Gemini],
            agent_instructions: agent_instructions::Outcome {
                agents_md_written: true,
                claude_md_written: true,
                gemini_md_written: true,
            },
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("AGENTS.md:  wrote the g-mesh code-search snippet"), "{rendered}");
        assert!(rendered.contains("CLAUDE.md:  wrote the @AGENTS.md bridge line"), "{rendered}");
        assert!(rendered.contains("GEMINI.md:  wrote the @AGENTS.md bridge line"), "{rendered}");
    }

    /// A second `init --agent claude` against an already-set-up project
    /// reports that everything was already present, not a fresh write.
    #[test]
    fn requested_agents_already_present_are_reported_as_left_untouched() {
        let outcome = Outcome {
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            daemon_was_running: false,
            config_written: false,
            summary: None,
            semantic_pass_ran: false,
            agents: vec![AgentTarget::Claude],
            agent_instructions: agent_instructions::Outcome::default(),
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(
            rendered.contains("AGENTS.md:  already had the g-mesh snippet - left untouched"),
            "{rendered}"
        );
        assert!(
            rendered.contains("CLAUDE.md:  already bridges to AGENTS.md - left untouched"),
            "{rendered}"
        );
        assert!(!rendered.contains("GEMINI.md"), "Gemini was never requested: {rendered}");
    }
}
