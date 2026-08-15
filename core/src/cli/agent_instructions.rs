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
//!   marker comments. A second run checks for the marker's presence, not for
//!   the exact snippet text, so hand-written content elsewhere in the file is
//!   never touched and a second `init` never appends a second copy.
//! - [`ensure_bridge_file`] checks whether the file's first line already
//!   reads `@AGENTS.md`. No marker is needed there because the only thing
//!   this function ever writes is that one line, prepended once.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::cli::AgentTarget;

const BEGIN_MARKER: &str = "<!-- g-mesh:agents-md:begin -->";
const END_MARKER: &str = "<!-- g-mesh:agents-md:end -->";
const BRIDGE_LINE: &str = "@AGENTS.md";

/// The canonical cross-tool project-instruction snippet.
///
/// Copied from README.md's "Reducing self-verification cost" section and the
/// user's own `~/.claude/CLAUDE.md` "Code search" section, and mirrored in
/// g-mesh-bench's `GMESH_CONFIGURED_CLAUDE_MD` - all three must be kept in
/// sync by hand if this ever changes.
pub const AGENTS_MD_SNIPPET: &str = r#"# Code search (TypeScript/JavaScript projects)

- In TS/JS projects, prefer g-mesh (`mcp__g-mesh__*`) for cross-file impact analysis, ambiguous naming (same symbol name declared in different scopes/files), and call-graph/multi-hop questions (callers, implementations, transitive dependencies) — grep can't resolve these reliably and has real unbounded cost (many round-trips, occasionally very expensive) when it tries. For simple, unambiguous single-symbol lookups, grep/`Explore`/manual reading is often just as fast and cheaper — g-mesh's tool schema adds fixed overhead per turn that doesn't pay for itself on easy questions (measured: g-mesh costs *more* tokens than grep on simple lookups, both isolated and in a long session — see `g-mesh-bench/docs/results/v0.2.0-session-economy-findings.md`). Fall back to grep when g-mesh returns no result, errors, or the target isn't something it tracks (non-code files, config, CSS, etc.).
- No manual indexing command exists or is needed. The g-mesh daemon bootstraps and indexes a project automatically on its first tool call in that project's directory. On first use in a new project, just issue any g-mesh call (e.g. `get_file_outline` on a source file) to trigger indexing, then proceed.
- How to use the tools:
  - `get_file_outline(file_path)` — list a file's top-level symbols before reading it in full, or to find the right symbol name to query next.
  - `find_definition(symbol_name)` or `find_definition(file_path, position)` — resolve a symbol to its definition and get its `symbol_id`. Not required before the tools below — they accept `symbol_name` directly and skip this call when the name is likely unambiguous, saving a round-trip. Call `find_definition` first only when you already expect ambiguity or need the declaration site itself.
  - `find_references(symbol_name or symbol_id)` — every usage of a symbol across the project; use before renaming or removing something.
  - `find_callers(symbol_name or symbol_id)` / `find_callees(...)` — walk the call graph up or down from a function.
  - `find_implementations(symbol_name or symbol_id)` — concrete types implementing an interface/abstract class.
  - `get_dependencies(file_path, direction: Outgoing|Incoming)` — walk the import graph (what a file imports / what imports it); use for impact analysis before changing a shared module.
  - `search_code(query)` — free-text semantic search over doc comments and signatures, ranked by similarity. Default to this as your *first* move on a "find the function/bug that does X" prompt when no symbol name is given — not something to reach for only after Grep has already failed a few times. Measured: on a bug-hunt task with no named symbol, reps that called `search_code` first converged in 8-11 turns; the one rep that skipped it and grep-guessed regex patterns from turn 1 took 15 turns for the same final answer (g-mesh-bench, `ex-implement-mutateelement-elbow-zero-position`). Skip it only for a symbol whose name you already know — `find_definition`/`find_references` are cheaper and exact there. Needs the project's embedding model available; if it errors saying semantic search is unavailable, fall back to grep or the structural tools instead.
  - If a `symbol_name` turns out ambiguous, the result carries `ambiguous: true` with a ranked candidate list — re-query using a candidate's `id` as `symbol_id`, not its `qualifiedName` (the same qualifiedName can name more than one declaration).
- Typical flow: call `find_references`/`find_callers`/`find_callees`/`find_implementations` directly with `symbol_name` when it's likely unique; only call `find_definition` first if you expect ambiguity or need the declaration site itself. Use `get_file_outline` first if you don't already know the right symbol name.
- A `find_references`/`find_callers`/`find_callees`/`find_implementations` result is complete for the question it answers when: it was anchored by `symbol_id` or an unambiguous `symbol_name` (same guarantee either way), every row shows `resolved: true`, and the response has no `allUnresolved: true` flag — don't re-verify that with grep/Read. As of g-mesh 0.8.x, `resolved: false` is a narrow, accurate signal (only edges whose target is in another file g-mesh couldn't confirm — same-file edges are always `resolved: true`, matched against declarations actually in scope), not a blanket disclaimer, so still check: a row that shows `resolved: false` (check that row, not the whole list), a response with `allUnresolved: true` (the whole page is unconfirmed), or anything the result doesn't claim to cover at all — e.g. whether other, similarly-named symbols exist elsewhere, or a method call reached through a variable receiver (`x.foo()`, which produces no edge by design). Measured on real g-mesh-bench runs after the 0.8.x same-file-resolution fix: mean cost dropped ~38% and mean turns ~35% on the task this was tested on, with the remaining tool calls answering things g-mesh genuinely doesn't cover rather than re-checking it (see g-mesh's README "Reducing self-verification cost" section) — but grep/Read still earn their keep on the cases above, so don't suppress those.
- Resolving an ambiguous name (the bullet above on `ambiguous: true` candidates) to a specific `symbol_id` doesn't reopen the completeness question: a `find_references`/`find_callers`/`find_callees`/`find_implementations` page anchored by that `symbol_id` carries the exact same `resolved: true`/no-`allUnresolved` guarantee as an unambiguous `symbol_name` query. Once you've picked the right candidate, treat its result as final — don't grep/Read each returned call site file-by-file to reconfirm it's "really" that symbol and not the same-named other one, and don't run a second, broad text search across the repo to check for anything the query might have missed. Both duplicate work the tool has already resolved, the same way re-verifying a plain unambiguous result would.
- `find_callers`/`find_callees` only ever walk `CALLS` edges, and a `CALLS` edge only exists when the call site sits lexically inside a *named, tracked* function or method. A call written at a file's top level, or inside an anonymous/inline callback that isn't itself extracted as its own symbol (exactly the shape of `it("...", () => { requireTask(...) })` in a test file), gets a `REFERENCES` edge instead — which `find_callers` never sees, even on an otherwise complete, `resolved: true`, `hasMore: false` page. That's not a hole in its own guarantee (it's complete for `CALLS` edges specifically), but it's narrower than "every place this is called" when the prompt implies that — use `find_references` *instead of* `find_callers` whenever the task needs an exhaustive caller list (before a rename/removal, or anything that should include test files). Instead of, not as well as: for the same anchor `find_references` returns a strict superset of `find_callers`' rows (every `CALLS` edge, plus the `REFERENCES`/`SUPERTYPE_OF` ones), and each row's own `referenceKind` separates them inside that one page — asking both tools is two round-trips for one answer. A usage that sits outside any tracked symbol comes back as a whole-file row — `kind: File`, with no `qualifiedName`/`startLine`/`startCol` — because the graph has no smaller unit to point at there, not because the position went missing from an otherwise complete row. When the task asks which *files* are affected (a rename, an impact list), that row is already the answer at the granularity it claims: take it and move on, rather than grepping the file for the exact lines it deliberately doesn't carry.
- A `get_dependencies` result's completeness is signaled by `truncated`/`truncatedBy`, not a per-row `resolved` flag — there isn't one; a multi-hop path can't be summarized by one boolean the way a single edge can. `truncated: false` means the walk reached everything within its depth/fanout bounds — trust it fully, don't re-verify with grep. `truncated: true` needs a follow-up keyed off `truncatedBy`, not a blanket re-query: on `maxDepth`, re-call anchored on the returned `frontierNodes` to go further; on `maxFanout`, that one node had more imports/importers than the fanout cap, so re-query just that node with the single-hop tools' own pagination; on `explorationBudget`/`responseSize`, call again with the returned `resumeToken`. The default `max_depth` is only 2 (shallower than a single-hop tool's own completeness bar), so check `truncated` before treating one result as the whole *transitive* tree — but a depth bound limits only how far the walk goes, never how completely it walked the levels it did reach: `truncated: false` with an empty `frontierNodes` is the entire answer for the depth you asked for, and at `max_depth: 1` that is exactly the complete set of direct importers (`Incoming`) or direct imports (`Outgoing`).
- Which imports produce those rows is the other half of trusting one. A row is a *file*, not an import statement, and its edge comes from a parsed module specifier: `import ... from`, type-only `import type ...`, `export ... from`, and `import()`/`require()` whose specifier is a static string or folds to one. Type-only imports sit in the graph exactly like value imports, so an `Incoming` walk already answers "every file that imports this, both kinds" — measured on g-mesh-bench's `tt-deps-incoming-db-connection`, one `Incoming`, `max_depth: 1` call on `src/db/connection.ts` returned all 21 importing `src/` files (18 of them `import type`-only), exactly the task's ground-truth set, and the follow-up greps three separate runs ran to check it found nothing it had missed. So don't re-derive that list with a `from ["'].*<module path>` grep: it is the most expensive habit on this tool, a whole extra round-trip that reproduces an answer already in hand. What a row genuinely doesn't carry is which names the importing file binds, whether that particular import was type-only, and on what line — `IMPORTS` edges have no position in the schema. When the task needs that for some file, Read that one file; don't grep the tree for all of them. The only importer that can be missing is one whose specifier no static fold can compute (built from a runtime value, `process.env`, or another file's constant).
- `search_code` is similarity-ranked, not a resolved graph query — its top hit isn't automatically "the answer" the way a `find_definition` hit is. But once a hit's `qualifiedName`/`kind`/`filePath` plausibly match what the prompt describes, one targeted confirming read (the exact lines, or `get_file_outline`) is enough — check the doc comment/signature there, then stop. Don't keep re-issuing `search_code` with reworded queries hunting for a "better" match, and don't follow a confirmed hit with a broad grep sweep across the repo "just in case" — that's the same wasted re-verification the bullet above warns against for the structural tools, just dressed up as more searching instead of more reading.
- `find_implementations` only returns direct implementors/extenders by default — a class extending a class that implements the anchor interface won't show up in a `hasMore: false` page. For the whole hierarchy, re-call with `transitive: true` (walks the same edges transitively, up to a bounded depth, resumable via `resume_token`).
"#;

/// What [`apply`] actually wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    /// Whether `AGENTS.md` was created or appended to. `false` means the
    /// marker was already present and nothing changed.
    pub agents_md_written: bool,
    /// Whether `CLAUDE.md` was created or given the bridge line. `false`
    /// (with `AgentTarget::Claude` requested) means it already bridged.
    pub claude_md_written: bool,
    /// Whether `GEMINI.md` was created or given the bridge line. `false`
    /// (with `AgentTarget::Gemini` requested) means it already bridged.
    pub gemini_md_written: bool,
}

/// Ensures `project_root/AGENTS.md` contains [`AGENTS_MD_SNIPPET`].
///
/// If the file does not exist, it is created with the snippet wrapped in the
/// `g-mesh:agents-md` marker comments. If it exists but lacks the begin
/// marker, the marker-wrapped block is appended after a blank-line separator,
/// so existing content is never overwritten. If the begin marker is already
/// present, this is a no-op.
///
/// Returns `true` if the file was created or appended to, `false` if it was
/// already set up.
pub fn ensure_agents_md(project_root: &Path) -> Result<bool> {
    let path = project_root.join("AGENTS.md");
    let existing = fs::read_to_string(&path).ok();

    if let Some(contents) = &existing {
        if contents.contains(BEGIN_MARKER) {
            return Ok(false);
        }
    }

    let block = format!("{BEGIN_MARKER}\n{AGENTS_MD_SNIPPET}{END_MARKER}\n");
    let new_contents = match existing {
        None => block,
        Some(mut contents) => {
            if !contents.ends_with('\n') {
                contents.push('\n');
            }
            contents.push('\n');
            contents.push_str(&block);
            contents
        }
    };

    fs::write(&path, new_contents).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
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

        assert!(written);
        let contents = fs::read_to_string(project.path().join("AGENTS.md")).unwrap();
        assert!(contents.contains(BEGIN_MARKER), "{contents}");
        assert!(contents.contains(END_MARKER), "{contents}");
        assert!(contents.contains("Code search (TypeScript/JavaScript projects)"), "{contents}");
    }

    #[test]
    fn ensure_agents_md_is_a_noop_once_the_marker_is_present() {
        let project = project();
        assert!(ensure_agents_md(project.path()).unwrap());

        let written_again = ensure_agents_md(project.path()).unwrap();

        assert!(!written_again, "a second run must not report a write");
        let contents = fs::read_to_string(project.path().join("AGENTS.md")).unwrap();
        assert_eq!(contents.matches(BEGIN_MARKER).count(), 1, "the block must not be duplicated");
    }

    #[test]
    fn ensure_agents_md_appends_after_pre_existing_content_without_touching_it() {
        let project = project();
        let path = project.path().join("AGENTS.md");
        fs::write(&path, "# My project\n\nSome hand-written notes.\n").unwrap();

        let written = ensure_agents_md(project.path()).unwrap();

        assert!(written);
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

        assert!(outcome.agents_md_written);
        assert!(!outcome.claude_md_written);
        assert!(!outcome.gemini_md_written);
        assert!(project.path().join("AGENTS.md").exists());
        assert!(!project.path().join("CLAUDE.md").exists());
        assert!(!project.path().join("GEMINI.md").exists());
    }

    #[test]
    fn apply_with_claude_and_gemini_writes_agents_md_once_plus_both_bridges() {
        let project = project();

        let outcome =
            apply(project.path(), &[AgentTarget::Claude, AgentTarget::Gemini]).unwrap();

        assert!(outcome.agents_md_written);
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

        let outcome =
            apply(project.path(), &[AgentTarget::Claude, AgentTarget::Gemini]).unwrap();

        assert!(!outcome.agents_md_written);
        assert!(!outcome.claude_md_written);
        assert!(!outcome.gemini_md_written);
    }
}
