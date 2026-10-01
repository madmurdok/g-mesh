//! Auto-installs the cross-tool project-instruction files that
//! `g-mesh init --agent <tool>...` writes, instead of asking a person to
//! hand-copy the snippet README.md documents.
//!
//! # Design: one shared file plus small bridges, not one file per tool
//!
//! `AGENTS.md` is the real 2026 cross-tool convention - Cursor, Windsurf,
//! GitHub Copilot, OpenAI Codex CLI, Kimi Code CLI, Aider and others all read
//! it natively, so writing it once already covers most of that list with
//! nothing tool-specific. Only two tools need anything extra, because they
//! read a differently-named file instead of `AGENTS.md`: Claude Code reads
//! `CLAUDE.md`, and Gemini CLI defaults to `GEMINI.md`. Both happen to
//! support the same `@path` import syntax, so the "extra" work for either is
//! one line - `@AGENTS.md` as the file's first line - rather than a second
//! copy of the whole snippet that could quietly drift from the first. That
//! is why [`apply`] always ensures `AGENTS.md` exists whenever any target was
//! named (every bridge depends on it) and only writes a bridge file for the
//! specific tool(s) actually requested.
//!
//! # Idempotence
//!
//! Both [`ensure_agents_md`] and [`ensure_bridge_file`] are safe to run
//! repeatedly, and safe to run against a file a person has already started
//! editing by hand:
//!
//! - [`ensure_agents_md`] wraps its injected block in
//!   `<!-- g-mesh:agents-md:begin -->` / `<!-- g-mesh:agents-md:end -->`
//!   marker comments. A second run replaces exactly the span from the begin
//!   marker through the end marker with the current snippet, so an upgraded
//!   g-mesh refreshes an installed block, hand-written content outside the
//!   markers is never touched, and a second `init` never appends a second
//!   copy. Edits *inside* the markers are overwritten: edit outside them. A
//!   file whose markers do not delimit exactly one block (a begin marker with
//!   no end marker after it, or two begin markers) is refused rather than
//!   guessed at, since a wrong guess about where the block ends deletes user
//!   text.
//! - [`ensure_bridge_file`] checks whether the file's first line already
//!   reads `@AGENTS.md`. No marker is needed there because the only thing
//!   this function ever writes is that one line, prepended once.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::cli::AgentTarget;

const BEGIN_MARKER: &str = "<!-- g-mesh:agents-md:begin -->";
const END_MARKER: &str = "<!-- g-mesh:agents-md:end -->";
const BRIDGE_LINE: &str = "@AGENTS.md";

/// The canonical cross-tool project-instruction snippet.
///
/// The only source of this text. README.md's "Reducing self-verification
/// cost" section carries a copy that `readme_mirrors_the_snippet` pins byte
/// for byte; g-mesh-bench's `GMESH_CONFIGURED_CLAUDE_MD` is pinned by its own
/// drift guard; an installed `AGENTS.md` block is refreshed by re-running
/// `g-mesh init --agent ...` (see [`ensure_agents_md`]).
pub const AGENTS_MD_SNIPPET: &str = r#"# Code search (TypeScript/JavaScript, Rust, Python, Go projects)

- Prefer g-mesh (`mcp__g-mesh__*`) for cross-file impact analysis, ambiguous naming (same symbol name declared in different scopes/files), and call-graph/multi-hop questions (callers, implementations, transitive dependencies) — grep can't resolve these reliably and has real unbounded cost (many round-trips, occasionally very expensive) when it tries. For simple, unambiguous single-symbol lookups, grep/`Explore`/manual reading is often just as fast and cheaper — g-mesh's tool schema adds fixed overhead per turn that doesn't pay for itself on easy questions (measured: g-mesh costs *more* tokens than grep on simple lookups, both isolated and in a long session — see `g-mesh-bench/docs/results/v0.2.0-session-economy-findings.md`). Fall back to grep when g-mesh returns no result, errors, or the target isn't something it tracks (non-code files, config, CSS, etc.).
- No manual indexing command exists or is needed. The g-mesh daemon bootstraps and indexes a project automatically on its first tool call in that project's directory. On first use in a new project, just issue any g-mesh call (e.g. `get_file_outline` on a source file) to trigger indexing, then proceed.
- When the g-mesh server covers a folder of several projects, call `select_project` first. In Claude Code its tools may be deferred: load them (ToolSearch) before the first call.
- Trust a complete answer from the structural tools (`find_*`, `get_dependencies`). A response says when it is not complete or not exact (`hasMore`, `truncated`, `allUnresolved`, a `resolved: false` row, a `resolvedBy` other than `id`/`qualifiedName`/`name`/`qualifiedNameSuffix`), and its `hint`/`explanation` says what to do next. Absent those, do not re-check it with grep or Read: that re-verification is the most expensive habit these tools have.
- `find_definition` returns the declaration's source in `source.text`: do not Read the file after it unless `source.omittedLines` says it was cut.
- The index serves the checkout it was built on. In a `git worktree` on another branch, trust g-mesh for code the branch has not changed and read the changed files directly.
- When delegating, put this section in the subagent's brief: a subagent does not inherit it, and it may need to load the g-mesh tools too. grep is still right there for one known symbol or for non-code.
"#;

/// Upper bound on [`AGENTS_MD_SNIPPET`]'s size in bytes, the counterpart of
/// `mcp::instructions::INSTRUCTIONS_BYTE_CEILING` for the text a project's
/// `AGENTS.md`/`CLAUDE.md` carries. That file is re-read on every turn and has
/// no transport truncation, so this bound is a cost decision: growing past it
/// is a behaviour change that needs a measurement, not a quiet edit.
pub const AGENTS_MD_SNIPPET_BYTE_CEILING: usize = 2560;

/// What [`ensure_agents_md`] did to `AGENTS.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentsMdWrite {
    /// The file did not exist and was created with the block.
    Created,
    /// The file existed without the block, which was appended to it.
    Appended,
    /// The file held an older block, which was replaced with the current one.
    Refreshed,
    /// The file already held the current block, or nothing was requested.
    #[default]
    Unchanged,
}

impl AgentsMdWrite {
    /// Whether the file's bytes changed.
    pub fn wrote(self) -> bool {
        self != AgentsMdWrite::Unchanged
    }
}

/// What [`apply`] actually wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    /// What happened to `AGENTS.md`.
    pub agents_md_written: AgentsMdWrite,
    /// Whether `CLAUDE.md` was created or given the bridge line. `false`
    /// (with `AgentTarget::Claude` requested) means it already bridged.
    pub claude_md_written: bool,
    /// Whether `GEMINI.md` was created or given the bridge line. `false`
    /// (with `AgentTarget::Gemini` requested) means it already bridged.
    pub gemini_md_written: bool,
}

/// Ensures `project_root/AGENTS.md` contains the current [`AGENTS_MD_SNIPPET`].
///
/// - No file: it is created with the snippet wrapped in the
///   `g-mesh:agents-md` marker comments.
/// - A file without the begin marker: the marker-wrapped block is appended
///   after a blank-line separator; existing content is never overwritten.
/// - A file with exactly one begin marker and an end marker after it: the
///   span from the begin marker through the end marker is replaced with the
///   current block, leaving everything before and after it byte-identical.
///   If the span already equals the current block, nothing is written.
/// - Any other arrangement of markers is an error naming the file and the
///   fix, and the file is left untouched.
pub fn ensure_agents_md(project_root: &Path) -> Result<AgentsMdWrite> {
    let path = project_root.join("AGENTS.md");
    let block = format!("{BEGIN_MARKER}\n{AGENTS_MD_SNIPPET}{END_MARKER}");

    let existing = match fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };

    let (new_contents, write) = match existing {
        None => (format!("{block}\n"), AgentsMdWrite::Created),
        Some(contents) => match contents.match_indices(BEGIN_MARKER).count() {
            0 => {
                let mut contents = contents;
                if !contents.ends_with('\n') {
                    contents.push('\n');
                }
                contents.push('\n');
                contents.push_str(&block);
                contents.push('\n');
                (contents, AgentsMdWrite::Appended)
            }
            1 => {
                let begin = contents.find(BEGIN_MARKER).expect("counted one begin marker");
                let Some(end_offset) = contents[begin..].find(END_MARKER) else {
                    bail!(
                        "{} has a `{BEGIN_MARKER}` line with no `{END_MARKER}` after it, so the \
                         g-mesh block's extent is unknown; add the end marker where the block \
                         ends, or delete the begin marker to have a fresh block appended",
                        path.display()
                    );
                };
                let end = begin + end_offset + END_MARKER.len();
                if contents[begin..end] == block {
                    return Ok(AgentsMdWrite::Unchanged);
                }
                let refreshed = format!("{}{block}{}", &contents[..begin], &contents[end..]);
                (refreshed, AgentsMdWrite::Refreshed)
            }
            _ => bail!(
                "{} has more than one `{BEGIN_MARKER}` line, so which g-mesh block to refresh is \
                 ambiguous; delete all but one begin/end marker pair",
                path.display()
            ),
        },
    };

    fs::write(&path, new_contents).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(write)
}

/// Ensures `project_root/<filename>` bridges to `AGENTS.md` via Claude
/// Code's and Gemini CLI's shared `@path` import syntax: a file whose first
/// line is `@AGENTS.md` imports the whole file, so neither tool needs its own
/// copy of the snippet.
///
/// If the file does not exist, it is created with just the bridge line. If
/// it exists and its first line is not already the bridge line, the bridge
/// line is prepended (with a blank-line separator) and the rest of the file
/// is preserved untouched below it. If the first line already is the bridge
/// line, this is a no-op.
///
/// Returns `true` if the file was created or given the bridge line, `false`
/// if it already had it.
pub fn ensure_bridge_file(project_root: &Path, filename: &str) -> Result<bool> {
    let path = project_root.join(filename);
    let existing = fs::read_to_string(&path).ok();

    if let Some(contents) = &existing {
        if contents.lines().next() == Some(BRIDGE_LINE) {
            return Ok(false);
        }
    }

    let new_contents = match existing {
        None => format!("{BRIDGE_LINE}\n"),
        Some(contents) => format!("{BRIDGE_LINE}\n\n{contents}"),
    };

    fs::write(&path, new_contents).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

/// Ensures every project-instruction file `agents` names exists.
///
/// An empty `agents` slice does nothing and returns immediately - `init`
/// without `--agent` must behave exactly as it always has. Otherwise
/// `AGENTS.md` is ensured once regardless of which specific targets were
/// given, since every bridge file depends on it existing; then a bridge file
/// is ensured for each of `AgentTarget::Claude` / `AgentTarget::Gemini`
/// present in `agents`. `AgentTarget::AgentsMd` needs nothing further beyond
/// the `AGENTS.md` write already done.
pub fn apply(project_root: &Path, agents: &[AgentTarget]) -> Result<Outcome> {
    if agents.is_empty() {
        return Ok(Outcome::default());
    }

    let agents_md_written = ensure_agents_md(project_root)?;
    let mut outcome = Outcome { agents_md_written, ..Outcome::default() };

    for agent in agents {
        match agent {
            AgentTarget::AgentsMd => {}
            AgentTarget::Claude => {
                outcome.claude_md_written = ensure_bridge_file(project_root, "CLAUDE.md")?;
            }
            AgentTarget::Gemini => {
                outcome.gemini_md_written = ensure_bridge_file(project_root, "GEMINI.md")?;
            }
        }
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        tempfile::tempdir().expect("failed to create a temp project root")
    }

    #[test]
    fn ensure_agents_md_creates_a_marker_wrapped_file_when_absent() {
        let project = project();

        let written = ensure_agents_md(project.path()).unwrap();

        assert_eq!(written, AgentsMdWrite::Created);
        let contents = fs::read_to_string(project.path().join("AGENTS.md")).unwrap();
        assert_eq!(contents, format!("{BEGIN_MARKER}\n{AGENTS_MD_SNIPPET}{END_MARKER}\n"));
    }

    #[test]
    fn ensure_agents_md_is_a_noop_once_the_marker_is_present() {
        let project = project();
        assert_eq!(ensure_agents_md(project.path()).unwrap(), AgentsMdWrite::Created);

        let written_again = ensure_agents_md(project.path()).unwrap();

        assert_eq!(written_again, AgentsMdWrite::Unchanged, "a second run must not report a write");
        let contents = fs::read_to_string(project.path().join("AGENTS.md")).unwrap();
        assert_eq!(contents.matches(BEGIN_MARKER).count(), 1, "the block must not be duplicated");
    }

    #[test]
    fn ensure_agents_md_appends_after_pre_existing_content_without_touching_it() {
        let project = project();
        let path = project.path().join("AGENTS.md");
        fs::write(&path, "# My project\n\nSome hand-written notes.\n").unwrap();

        let written = ensure_agents_md(project.path()).unwrap();

        assert_eq!(written, AgentsMdWrite::Appended);
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("# My project\n\nSome hand-written notes.\n"), "{contents}");
        assert!(contents.contains(BEGIN_MARKER), "{contents}");
    }

    #[test]
    fn ensure_bridge_file_creates_just_the_bridge_line_when_absent() {
        let project = project();

        let written = ensure_bridge_file(project.path(), "CLAUDE.md").unwrap();

        assert!(written);
        let contents = fs::read_to_string(project.path().join("CLAUDE.md")).unwrap();
        assert_eq!(contents, "@AGENTS.md\n");
    }

    #[test]
    fn ensure_bridge_file_is_a_noop_when_already_bridging() {
        let project = project();
        let path = project.path().join("CLAUDE.md");
        fs::write(&path, "@AGENTS.md\n\nSome other instructions.\n").unwrap();

        let written = ensure_bridge_file(project.path(), "CLAUDE.md").unwrap();

        assert!(!written);
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "@AGENTS.md\n\nSome other instructions.\n", "unchanged content is untouched");
    }

    #[test]
    fn ensure_bridge_file_prepends_the_bridge_line_and_preserves_existing_content() {
        let project = project();
        let path = project.path().join("CLAUDE.md");
        fs::write(&path, "# Existing instructions\n\nDo not break the build.\n").unwrap();

        let written = ensure_bridge_file(project.path(), "CLAUDE.md").unwrap();

        assert!(written);
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("@AGENTS.md\n\n"), "{contents}");
        assert!(contents.contains("# Existing instructions\n\nDo not break the build.\n"), "{contents}");
    }

    #[test]
    fn apply_with_no_targets_does_nothing() {
        let project = project();

        let outcome = apply(project.path(), &[]).unwrap();

        assert_eq!(outcome, Outcome::default());
        assert!(!project.path().join("AGENTS.md").exists());
    }

    #[test]
    fn apply_with_agents_md_only_writes_agents_md_and_no_bridge_files() {
        let project = project();

        let outcome = apply(project.path(), &[AgentTarget::AgentsMd]).unwrap();

        assert_eq!(outcome.agents_md_written, AgentsMdWrite::Created);
        assert!(!outcome.claude_md_written);
        assert!(!outcome.gemini_md_written);
        assert!(project.path().join("AGENTS.md").exists());
        assert!(!project.path().join("CLAUDE.md").exists());
        assert!(!project.path().join("GEMINI.md").exists());
    }

    #[test]
    fn apply_with_claude_and_gemini_writes_agents_md_once_plus_both_bridges() {
        let project = project();

        let outcome = apply(project.path(), &[AgentTarget::Claude, AgentTarget::Gemini]).unwrap();

        assert_eq!(outcome.agents_md_written, AgentsMdWrite::Created);
        assert!(outcome.claude_md_written);
        assert!(outcome.gemini_md_written);
        assert!(project.path().join("AGENTS.md").exists());

        let claude = fs::read_to_string(project.path().join("CLAUDE.md")).unwrap();
        assert!(claude.starts_with("@AGENTS.md"), "{claude}");
        let gemini = fs::read_to_string(project.path().join("GEMINI.md")).unwrap();
        assert!(gemini.starts_with("@AGENTS.md"), "{gemini}");
    }

    #[test]
    fn apply_run_twice_is_fully_idempotent() {
        let project = project();
        apply(project.path(), &[AgentTarget::Claude, AgentTarget::Gemini]).unwrap();

        let outcome = apply(project.path(), &[AgentTarget::Claude, AgentTarget::Gemini]).unwrap();

        assert_eq!(outcome.agents_md_written, AgentsMdWrite::Unchanged);
        assert!(!outcome.claude_md_written);
        assert!(!outcome.gemini_md_written);
    }

    #[test]
    fn ensure_agents_md_refreshes_an_old_block_and_preserves_text_around_it() {
        let project = project();
        let path = project.path().join("AGENTS.md");
        let before = "# My project\n\nNotes above the block.\n\n";
        let after = "\n\n## Mine\n\nNotes below the block.\n";
        let old_block = format!("{BEGIN_MARKER}\n# Code search (old)\n\n- an outdated bullet\n{END_MARKER}");
        fs::write(&path, format!("{before}{old_block}{after}")).unwrap();

        let written = ensure_agents_md(project.path()).unwrap();

        assert_eq!(written, AgentsMdWrite::Refreshed);
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, format!("{before}{BEGIN_MARKER}\n{AGENTS_MD_SNIPPET}{END_MARKER}{after}"));
        assert_eq!(ensure_agents_md(project.path()).unwrap(), AgentsMdWrite::Unchanged);
    }

    #[test]
    fn ensure_agents_md_refuses_a_begin_marker_without_an_end_marker() {
        let project = project();
        let path = project.path().join("AGENTS.md");
        let original = format!("# Mine\n\n{BEGIN_MARKER}\n# Code search\n\nmy own notes after it\n");
        fs::write(&path, &original).unwrap();

        let err = ensure_agents_md(project.path()).unwrap_err().to_string();

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains(END_MARKER), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), original, "a refused file must stay untouched");
    }

    #[test]
    fn ensure_agents_md_refuses_an_end_marker_only_before_the_begin_marker() {
        let project = project();
        let path = project.path().join("AGENTS.md");
        let original = format!("{END_MARKER}\n# Mine\n{BEGIN_MARKER}\n# Code search\n");
        fs::write(&path, &original).unwrap();

        assert!(ensure_agents_md(project.path()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn ensure_agents_md_refuses_two_begin_markers() {
        let project = project();
        let path = project.path().join("AGENTS.md");
        let block = format!("{BEGIN_MARKER}\n{AGENTS_MD_SNIPPET}{END_MARKER}\n");
        let original = format!("{block}\n{block}");
        fs::write(&path, &original).unwrap();

        let err = ensure_agents_md(project.path()).unwrap_err().to_string();

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("more than one"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn agents_md_snippet_fits_its_ceiling() {
        assert!(
            AGENTS_MD_SNIPPET.len() <= AGENTS_MD_SNIPPET_BYTE_CEILING,
            "AGENTS_MD_SNIPPET is {} bytes, over its {AGENTS_MD_SNIPPET_BYTE_CEILING}-byte ceiling",
            AGENTS_MD_SNIPPET.len()
        );
    }

    /// README.md carries a copy for people who paste it into a global
    /// `CLAUDE.md` instead of running `init`; it must be this exact text.
    #[test]
    fn readme_mirrors_the_snippet() {
        let readme = include_str!("../../../README.md");
        let heading = readme
            .find("### Reducing self-verification cost")
            .expect("README.md lost its \"Reducing self-verification cost\" heading");
        let fence = "```markdown\n";
        let start = heading
            + readme[heading..].find(fence).expect("no ```markdown block after the heading")
            + fence.len();
        let len = readme[start..].find("\n```\n").expect("the ```markdown block is not closed") + 1;

        assert_eq!(
            &readme[start..start + len],
            AGENTS_MD_SNIPPET,
            "README.md's snippet block drifted from AGENTS_MD_SNIPPET"
        );
    }

    /// Every bundled language plugin is named in the snippet's heading, so a
    /// new plugin cannot ship with guidance that says its projects are out of
    /// scope.
    #[test]
    fn snippet_heading_names_every_bundled_plugin_language() {
        let heading = AGENTS_MD_SNIPPET.lines().next().unwrap().to_lowercase();
        let words: Vec<&str> =
            heading.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
        let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins");

        let mut languages = Vec::new();
        for entry in fs::read_dir(&plugins).unwrap() {
            let manifest = entry.unwrap().path().join("plugin.toml");
            if !manifest.exists() {
                continue;
            }
            let parsed: toml::Value = toml::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
            let language = parsed["plugin"]["language"].as_str().unwrap().to_lowercase();
            assert!(
                words.contains(&language.as_str()),
                "{} declares language {language:?}, which the snippet heading {heading:?} does not name",
                manifest.display()
            );
            languages.push(language);
        }
        assert!(languages.len() >= 4, "found only {languages:?} under {}", plugins.display());
    }
}
