# g-mesh project instructions

## Code comment rule

A code comment keeps only two things:

1. **What the code does**, when that isn't obvious from the code itself.
2. **An invariant a change must not break.** Examples:
   - lock order: plugin lock before connection lock.
   - foreign keys are OFF in production, so a write for a deleted node
     leaves an orphan row (don't "fix" this locally — it's relied on
     elsewhere).
   - the `indexed_files` baseline is written only after a successful
     reindex.

Everything else goes somewhere other than the comment, never into it:

- **Change history** ("GM-394 did X, GM-396 changed it because...") goes to
  commit messages and `git blame`, not to a comment. A reader wants "why is
  this true", not "who touched this and when" — that's what history is for.
- **Decisions** (why this approach over another, what was considered and
  rejected) go to an ADR (`docs/adr/`), not to a comment. Link to the ADR
  at module level, or at the one place in the code where the decision is
  actually embodied — never per function. A decision that shaped ten
  functions gets one link, not ten.

**A comment that mixes these is split, not kept or dropped whole.** Keep
the sentence that states the present invariant, rewritten in the present
tense with no ticket id ("Unchanged by GM-294: X is checked before Y"
becomes "X is checked before Y"). Drop the history. If the "because" is a
design choice (why this and not an alternative), it goes to an ADR; if it
only says what breaks when the invariant is violated, it stays as part of
the invariant.

This rule **overrides "match the surrounding comment density."** A file
full of history comments is not a style to match; it's exactly what this
rule removes. Don't add a new history comment because the ones around it
are history comments.

**Cleanup is file by file, inside a refactor ticket** — never a bulk pass
across the codebase. A ticket that touches a file for another reason is a
reasonable place to also clean that file's comments; a ticket whose only
purpose is comment cleanup should still land as one file (or a small,
related group) at a time, not as a single sweeping diff.

## Reading large files

Read excerpts, not whole large files: `grep -n` to find the lines that
matter, then read with an offset/limit (or `sed -n`) around them. Do not
read a whole large file end to end when a targeted read answers the
question — a full read early gets carried through every later turn.

## Architecture decisions

See [`docs/adr/README.md`](docs/adr/README.md) for the ADR index —
existing design decisions (indexed from `docs/architecture/`) and new ones
going forward.
