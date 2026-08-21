//! Reads the source a node's coordinates point at, so a caller that asked
//! "where is this defined" does not have to spend a whole round trip finding
//! out what is there.
//!
//! # Why this is worth the payload
//!
//! Measured on g-mesh-bench, 2026-08-20: with call arguments recorded, 5 of 6
//! `Read`/`Grep` calls that followed a g-mesh call opened a file that call had
//! just named. The agent asked *where* and then spent a turn asking *what*.
//!
//! The arithmetic is lopsided. A turn costs 18,000-22,000 tokens at g-mesh's
//! prompt prefix, because a stateless CLI agent re-sends the whole
//! conversation every time. A declaration of [`MAX_LINES`] lines is a few
//! hundred. Even re-read on ten subsequent turns, the snippet stays an order
//! of magnitude below the single round trip it removes - which is the bound
//! [`MAX_LINES`] is chosen against, not "how much source is nice to have".
//!
//! This reverses an earlier task filed on the belief that payload was the main
//! cost. The per-turn measurement corrected it: payload is roughly an order
//! below a round trip, so the right direction is to spend payload to buy
//! turns.
//!
//! # Why it reads the file rather than storing the text
//!
//! The index stores coordinates, not source. Storing snippets would double
//! the write path's output, and would go stale exactly when it matters - a
//! file edited since the last walk. Reading at answer time means a snippet is
//! either current or absent, never confidently wrong.

use std::fs;
use std::path::Path;

use serde::Serialize;

/// The most lines a snippet carries before it is cut.
///
/// Set from the round-trip arithmetic in this module's doc comment rather
/// than from taste: 80 lines is roughly 800 tokens, so even re-read across
/// ten later turns it stays well under the 18,000-22,000 a single avoided
/// round trip costs. Declarations longer than this are the ones where the
/// caller most likely wants the file anyway.
pub(super) const MAX_LINES: usize = 80;

/// A hard second bound, for the minified bundle or generated file where 80
/// lines is not 800 tokens but 80,000. Lines are cut, never split mid-line:
/// half a line of source is a thing a reader has to reconstruct, and the
/// truncation marker already says the rest is missing.
pub(super) const MAX_CHARS: usize = 6_000;

/// The declaration's own text, with an explicit account of anything left out.
///
/// `omitted_lines` is `None` for a complete snippet rather than `Some(0)`, so
/// the field's presence alone answers "is this all of it" - a silent cut is
/// the one outcome worth ruling out, since a caller reading a truncated body
/// as complete draws conclusions from code that is not there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Snippet {
    /// 1-based, as an editor would show it - unlike the `startLine` beside it
    /// in the response, which is the index's own 0-based coordinate. Both are
    /// present because they answer different questions and silently changing
    /// the existing one would break every caller doing arithmetic on it.
    pub(super) first_line: i64,
    pub(super) text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) omitted_lines: Option<usize>,
}

/// The text between `start_line` and `end_line` inclusive, both 0-based as
/// stored in the index (tree-sitter's own convention; `semanticPass.ts` adds
/// one when it hands a position to the TypeScript server, which is the other
/// end of the same fact).
///
/// `None` rather than an error for every way this can fail - the file is gone,
/// unreadable, not valid UTF-8, or shorter than the index believes because it
/// was edited since the last walk. A definition's coordinates are still a
/// correct, useful answer without its text, so a missing snippet degrades the
/// response instead of failing the call.
pub(super) fn read_span(
    project_root: &Path,
    file_path: &str,
    start_line: i64,
    end_line: i64,
) -> Option<Snippet> {
    if start_line < 0 || end_line < start_line {
        return None;
    }

    let contents = fs::read_to_string(project_root.join(file_path)).ok()?;
    let lines: Vec<&str> = contents.lines().collect();

    let start = usize::try_from(start_line).ok()?;
    let end = usize::try_from(end_line).ok()?;
    // A span past the end of the file means the index is describing a version
    // of this file that is no longer on disk. Answering with whatever happens
    // to be at those lines now would be worse than answering with nothing.
    if start >= lines.len() || end >= lines.len() {
        return None;
    }

    let span = &lines[start..=end];
    let kept = bounded(span);

    Some(Snippet {
        first_line: start_line + 1,
        text: span[..kept].join("\n"),
        omitted_lines: (kept < span.len()).then(|| span.len() - kept),
    })
}

/// How many leading lines of `span` fit inside both caps.
///
/// Always at least one line when the span has one, even if that single line
/// alone busts [`MAX_CHARS`]: a snippet cut to nothing, marked as truncated,
/// carries the cost of the field and none of its value. The line-length cap
/// exists for the generated file where *every* line is enormous, and one such
/// line is still a bounded amount of text.
fn bounded(span: &[&str]) -> usize {
    let mut chars = 0;
    for (n, line) in span.iter().take(MAX_LINES).enumerate() {
        // +1 for the newline join, which counts toward what the caller pays.
        chars += line.chars().count() + 1;
        if chars > MAX_CHARS {
            return n.max(1);
        }
    }
    span.len().min(MAX_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(contents: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("failed to create a temp project");
        std::fs::write(dir.path().join("a.ts"), contents).expect("failed to write the fixture");
        (dir, "a.ts".to_string())
    }

    #[test]
    fn a_span_is_read_inclusively_and_reported_one_based() {
        let (dir, file) = project("zero\none\ntwo\nthree\n");

        let snippet = read_span(dir.path(), &file, 1, 2).expect("the span is inside the file");

        assert_eq!(snippet.text, "one\ntwo", "0-based and inclusive at both ends");
        assert_eq!(snippet.first_line, 2, "reported as an editor would show it");
        assert_eq!(snippet.omitted_lines, None);
    }

    #[test]
    fn a_single_line_declaration_is_that_line() {
        let (dir, file) = project("zero\none\ntwo\n");

        let snippet = read_span(dir.path(), &file, 0, 0).expect("the span is inside the file");

        assert_eq!(snippet.text, "zero");
        assert_eq!(snippet.first_line, 1);
    }

    /// The cut has to be visible. A caller that reads a truncated body as a
    /// complete one draws conclusions from code that is not there, which is a
    /// worse failure than having no snippet at all.
    #[test]
    fn a_long_declaration_is_cut_and_says_by_how_much() {
        let body: String = (0..MAX_LINES + 30).map(|n| format!("line {n}\n")).collect();
        let (dir, file) = project(&body);

        let snippet =
            read_span(dir.path(), &file, 0, (MAX_LINES + 29) as i64).expect("the span is inside the file");

        assert_eq!(snippet.text.lines().count(), MAX_LINES);
        assert_eq!(snippet.omitted_lines, Some(30));
    }

    #[test]
    fn a_few_enormous_lines_are_cut_by_the_char_cap_not_the_line_cap() {
        let huge = "x".repeat(MAX_CHARS / 2);
        let (dir, file) = project(&format!("{huge}\n{huge}\n{huge}\n"));

        let snippet = read_span(dir.path(), &file, 0, 2).expect("the span is inside the file");

        assert!(snippet.text.lines().count() < 3, "the char cap must bite before the line cap");
        assert_eq!(snippet.omitted_lines, Some(3 - snippet.text.lines().count()));
    }

    /// One line over the cap still beats zero lines plus a truncation notice.
    #[test]
    fn one_line_longer_than_the_whole_budget_is_still_returned() {
        let huge = "x".repeat(MAX_CHARS * 2);
        let (dir, file) = project(&format!("{huge}\nnext\n"));

        let snippet = read_span(dir.path(), &file, 0, 1).expect("the span is inside the file");

        assert_eq!(snippet.text, huge);
        assert_eq!(snippet.omitted_lines, Some(1));
    }

    /// The index outliving the file it describes is the ordinary case after an
    /// edit, not an exceptional one - so it degrades the answer rather than
    /// failing the call, and never returns whatever now sits at those lines.
    #[test]
    fn a_span_past_the_end_of_the_file_reads_as_absent_rather_than_wrong() {
        let (dir, file) = project("one\ntwo\n");

        assert_eq!(read_span(dir.path(), &file, 0, 9), None, "the file shrank since it was indexed");
        assert_eq!(read_span(dir.path(), &file, 5, 6), None);
    }

    #[test]
    fn a_missing_file_reads_as_absent() {
        let dir = tempfile::tempdir().expect("failed to create a temp project");

        assert_eq!(read_span(dir.path(), "gone.ts", 0, 1), None);
    }

    #[test]
    fn a_nonsensical_span_reads_as_absent() {
        let (dir, file) = project("one\ntwo\n");

        assert_eq!(read_span(dir.path(), &file, -1, 1), None);
        assert_eq!(read_span(dir.path(), &file, 1, 0), None, "end before start");
    }
}
