# GM-527: a whole-file node's end is one past the last line

Status: design, revision 2 (GM-527/S5). It replaces revision 1, which
recommended a reader-only fix. No production code is changed by this note.

## Decisions taken

The owner chose:
- **Q1:** "(c) Меняются плагины" (the plugins change).
- **Q3:** "Строго: только колонка 0 (Recommended)" (strict: column 0 only).
- **Follow-up:** "(c) + строгий читатель для старых индексов (Recommended)"
  ((c) plus a strict reader for old indexes).

Amended AC2:
- In a newly built index, a File or Module `endLine` is at most
  `lineCount - 1`, the last real line.
- The plugins and the SDK change the whole-file convention. The plugin-check
  whitespace-edit check and the conformance expectations change with them.
- Old indexes hold `(lineCount, 0)` after a final newline. Core's
  `read_span_within` accepts exactly that end as the end of the last line,
  and refuses every other past-the-end span.
- A Module candidate gets source on an ambiguous page (requests
  `__version__`) in both a new and an old index.

## The problem and its cause

On py-requests, the `__version__.py` Module is stored as `(0,0)-(14,0)` in a
14-line file that ends in `\n`. `core/src/mcp/source.rs` `read_span_within`
refuses `end >= lines.len()` as stale, so the candidate gets no source.

Every plugin today ends the whole-file range at
`(number of '\n', length of the text after the last one)`. That is
`(lineCount, 0)` for a file ending in `\n`. The convention exists because the
plugin-check whitespace-edit check needs an end that a space inserted before
the last newline cannot move (see the next section). Python is the only plugin
that also gives this range to a declaration, the Module. A live index of the
main checkout confirms it (`SCRATCH/gm527-nodes.txt`):
- 41 Go, 46 Python and 340 Rust File nodes end at `(lineCount,0)`;
- so do 45 Python Module nodes (43 `module`, 2 `package`);
- no row exceeds `(lineCount,0)`.

## What the whitespace-edit check catches, and the new end

`id-stability.whitespace-edit` works like this:
- `session.rs` `whitespace_edit` (689) inserts one space before the file's
  last newline.
- The plugin's `fileChanged` answer must be an empty diff
  (`checks.rs` `whitespace_edit`, 665-706: any upsert or delete is a finding).
- It catches a plugin whose ids or ranges depend on something an edit that
  changes no declaration still moves. Examples: a range taken from byte
  offsets or the text length, an id derived from a position, or a node
  re-emitted on every change.
- The core test `a_range_moved_by_whitespace_fails_the_whitespace_edit_check`
  (`core/tests/plugin_check.rs:393`) pins it. It uses the fake defect
  `whitespace-moves-range`, whose File end column is the text length.

That edit was chosen because it moves nothing a correct plugin reports. The
new convention has to keep that property, or the check loses its strength.
Two ends meet the AC:

- **(A) Content end (recommended).** The end of the text with trailing
  whitespace removed.
  - Rule: `content = text.trim_end_matches([' ', '\t', '\r', '\n', '\x0b', '\x0c'])`,
    and `end = (number of '\n' in content, length of content after its last '\n')`.
  - That is today's formula applied to `content` instead of `text`.
  - The inserted space is always either trailing whitespace (trimmed away) or
    on an earlier line than the final one (changes neither the newline count
    of `content` nor its last line). So the end does not move.
  - The check's code stays as it is, at full strength.
- **(B) End of the last line.** `(lines().count()-1, length of that line)`.
  - The inserted space lands on that line in every file ending in `\n`, so
    the File (and Python Module) end column grows by one.
  - The check must then accept exactly that upsert. That makes it blind to
    the very defect its fake pins: the text length also grows by exactly one.

The rest of this note assumes (A). Questions for the owner, Q1, asks to
confirm it.

### Exact rule per case, under (A)

Columns keep each plugin's present unit. The SDK, Python and Rust count chars;
Go counts bytes, as `textEndPosition` does now.

| Input | Today | (A) | `lines().count()-1` |
|---|---|---|---|
| `""` (empty) | `(0,0)` | `(0,0)` | -1, the one exception: an empty file has no line |
| `"\n \n"` (whitespace only) | `(2,0)` | `(0,0)` | 1 |
| `"a\nbc"` (no final newline) | `(1,2)` | `(1,2)`, unchanged | 1 |
| `"a\nbc\n"` (final newline) | `(2,0)` | `(1,2)` | 1 |
| `"a\nbc\n\n"` (ends in `"\n\n"`) | `(3,0)` | `(1,2)` | 2 |
| `"a\r\nbc\r\n"` (CRLF) | `(2,0)` | `(1,2)` | 1 |
| `"a\nbc  \n"` (trailing spaces) | `(2,0)` | `(1,2)` | 1 |

So `endLine <= lineCount-1` holds for every non-empty file.

### Sites (all five implementations plus the fake)

| Site | Used by | Change |
|---|---|---|
| `plugins/sdk/src/columns.rs` `CharColumns::file_range` (57-70) | TS via `plugins/typescript/src/extractor/mod.rs:94` -> `FileModel::new` (`model.rs:253`), File only | apply rule (A) |
| `plugins/python/src/extractor/emit.rs` `Positions::file_range` (120-140) | File (`emit.rs:209`) and Module (`decls.rs` `Declarer::announce`, 158-177) | apply rule (A); File and Module stay identical |
| `plugins/rust/src/extractor/emit.rs` `Positions::file_range` (86-104) | File (`emit.rs:158`). `mod` items use their AST range | apply rule (A) |
| `plugins/go/extract.go` `textEndPosition` (960-981) | `computeFileNode` (942). Go has no file-ranged Module or package node | apply rule (A) in bytes (`bytes.TrimRight(content, " \t\r\n\v\f")`) |
| `plugins/sdk/toy/main.rs` (94-104) | the SDK's reference toy plugin, run by `plugins/sdk/tests/toy_conformance.rs` | apply rule (A) |
| `plugins/sdk/fake/toy.rs` `extract` (157-166) | core's plugin-check fake. Its conformant branch must follow (A), and its `whitespace-moves-range` branch stays the text length | conformant branch: rule (A) |

How the sites were found:
- **g-mesh `find_callers(FileGraphBuilder::file_node)`**: tests and fixtures
  only (graph.rs, diff.rs, index.rs, `lsp/bridge.rs` tests,
  `tests/lsp_bridge.rs`, all with hand-written ranges). There are no
  production callers, and these tests are unaffected.
- **g-mesh `find_callers(Columns::file_range)`**: resolved by meaning only
  (the type is `CharColumns`), so grep listed the `file_range`,
  `textEndPosition` and `file_node(` sites.
- The toy and the fake are not in the g-mesh index; grep found them.

## Conformance expectations and tests that change

Counted with grep, after g-mesh showed no production callers beyond the
listed sites.

- **Conformance `expect.toml` (go, python, rust, typescript): 0 change.** None
  of the 4 files states a range or an end line (grep for
  `line|range|end|start =` keys). `plugin_check/expectations.rs` has no range
  field either.
- **The `ALL_CHECKS` lists: 0 change.** These are in
  `plugins/{python,rust,typescript}/tests/conformance.rs`,
  `plugins/sdk/tests/toy_conformance.rs` and `core/tests/plugin_check.rs`.
  No check id is added; the file-end assertion goes inside the existing
  whitespace-edit check (Q2).
- **Plugin tests whose expected values change: 5 assertions in 3 files.**
  - `plugins/sdk/tests/columns.rs`:
    - `columns_file_range_of_a_terminated_file_ends_on_the_empty_last_line`
      (62-64): `(2,0)` becomes `(1,2)`, and the test is renamed.
    - `columns_carriage_return_is_an_ordinary_character` (72-76): its
      `file_range` assertion becomes `(1,2)`, because the trailing `\r` is
      now trimmed. Its `at` assertion stays.
  - `plugins/go/extract_test.go`:
    - `TestTextEndPositionCountsNewlinesAndTrailingBytes`: 2 rows change,
      `"package main\n"` to `(0,12)` and `"a\nb\nc\n"` to `(2,1)`.
    - `TestComputeFileNodeMatchesTheDesignDocsWorkedExample`: `(10,0)`
      becomes `(8,10)`. The last content is `// trailer` on line 8.
- **Unchanged, and still pinning the whitespace property:**
  - `a_files_end_is_where_a_trailing_space_cannot_move_it` in
    `python/.../emit.rs` (509) and `rust/.../emit.rs` (404);
  - `a_trailing_space_before_the_last_newline_changes_nothing` in Python
    `tests.rs:846` and Rust `tests.rs:1525`;
  - `core/tests/plugin_check.rs:393`.
- **Unchanged wire fixtures:** `core/tests/fixtures/valid.ndjson` and
  `valid_v2.ndjson` carry File ends of `(10, …)`. They are protocol examples
  with no file on disk, and core does not check the convention, so they stay.
- **Core test that flips:** `runs_one_past_the_end` and
  `a_page_where_one_candidates_span_ends_past_the_file_says_only_some_are_sourced`
  (`core/src/mcp/find_definition/tests.rs` 1908-1932) assert today's refusal.
  They are rewritten (behaviour 8 below).
- **Doc comments that state the old rule:**
  - `columns.rs:57-60`;
  - `graph.rs:465-469`;
  - Python `emit.rs:120-129`;
  - Rust `emit.rs:87-94`;
  - `go/extract.go:960-975`;
  - `toy/main.rs:96-99`;
  - `fake/toy.rs:162-163`;
  - `plugin_check/session.rs:672-676`, the "(lines, 0) … (8, 0)" bullet,
    which is already wrong for TS today.

## The strict old-index reader in core

`read_span_within` gains `end_col`. It accepts `end_line == lines.len()` only
when all three hold:
- `end_col == 0`;
- the file ends in `\n`;
- `start_line < lines.len()`.

It then reads up to the last line. Every other span keeps today's checks, so
these are still refused as stale:
- `end_line > lines.len()` (the file was shortened);
- `(lines.len(), c>0)`;
- `(lines.len(), 0)` with no final newline;
- `start_line >= lines.len()`.

The one span it accepts is the current file's own old-convention end. For a
whole-file node, "the whole current file" is the right text even if the file
changed.

With the new convention, File and Module ends are real lines. They go through
the ordinary path, and the clamp is reached only by old-index rows. The
clamp could be removed once old indexes no longer need to be read; this note
does not set a date for that.

## What else reads a File end (Q5 of the brief)

Nothing in core compares a File end with the file's length:
- **Staleness** (`watcher/staleness.rs`) is mtime- and hash-based, and reads
  no range.
- **The incremental diff** (`plugins/sdk/src/diff.rs`) compares node records
  by id and value.
- **`watcher::apply`** stores the wire range as is.

These readers do change behaviour, each acceptably:

| Reader | Found by | Effect of (A) |
|---|---|---|
| `mcp::source::read_span` / `read_span_within` | g-mesh `find_callers(read_span_within)`, `find_callers(read_span)` | new-index File and Module spans are ordinary. Old ones go through the strict clamp |
| `find_definition::DefinitionNode::with_source`, `CandidatePage::ambiguous` | same two calls | source is served (the AC) |
| `graph::queries::find_by_position` via `find_definition::by_position` | g-mesh `find_callers(find_by_position)` | a cursor on trailing blank lines (or after the last content) no longer lands on the File node. The answer becomes "no symbol found at …" instead of the File node, which today comes without source anyway |
| `graph::containers::defining_containers` (684), `m.endLine >= f.endLine` for a Python Module against its File | grep | both come from one function, so they stay equal |
| `plugin_check::session::declaration_edit` (629-655), which edits at the first non-File declaration's end line | grep | for Python that is often the Module. Today the edit lands at EOF; under (A) it lands before the last content line, a real edit. The check compares against a fresh bulk walk, so it stays valid |
| `sdk::diff` | grep | appending blank lines or trailing whitespace no longer re-upserts the File node. Adding content does |
| `get_file_outline`, `embed_eval::churn`, storage | grep | pass-through only, no effect |

## Edit map

### Plugins (new convention)

- `plugins/sdk/src/columns.rs`:
  - `CharColumns::file_range` (57-70): apply rule (A), and update its doc.
- `plugins/sdk/src/graph.rs`:
  - `FileGraphBuilder::file_node` doc (460-470): state rule (A) and why
    (the whitespace-edit check).
- `plugins/python/src/extractor/emit.rs`:
  - `Positions::file_range` (120-140): rule (A).
  - Tests block (~509): add the case table.
- `plugins/rust/src/extractor/emit.rs`:
  - `Positions::file_range` (86-104): rule (A).
  - Tests (~404): the case table.
- `plugins/go/extract.go`:
  - `textEndPosition` (960-981): rule (A) in bytes, and update its doc.
- `plugins/go/extract_test.go` (5-62): the table rows and `(8,10)`.
- `plugins/sdk/toy/main.rs` (94-104): rule (A).
- `plugins/sdk/fake/toy.rs`:
  - `extract` (157-166): the conformant branch follows rule (A).
  - Add a defect `file-end-past-last-line` that keeps the old `(lineCount,0)`
    end, and list it in the module doc (~40).
- `plugins/sdk/tests/columns.rs` (56-76): the 2 changed assertions, plus
  new rows for whitespace-only, `"\n\n"` and trailing spaces.

### plugin-check

- `core/src/cli/plugin_check/checks.rs`:
  - `whitespace_edit` (665-706): before the diff loop, take the target file's
    File node from `run.bulk[0]` and its `original` bytes. Add a finding when
    the File's `end.line` differs from the content-end line of `original`.
  - Compare the line only: core does not know a plugin's column unit, and
    the line is what the AC constrains.
  - Update the module doc (38-46).
- `core/src/cli/plugin_check/session.rs`:
  - `whitespace_edit` doc (660-688): replace the TS `(lines, 0)` bullet
    with rule (A).
- `core/tests/plugin_check.rs`:
  - Add `a_file_end_past_the_last_line_fails_the_whitespace_edit_check`
    next to 393, using the new fake defect.

### Core reader (old indexes)

- `core/src/mcp/source.rs`:
  - `read_span` (79-88) and `read_span_within` (91-124): add `end_col` and
    the strict clamp. Doc (70-78) and stale comment (106-108).
- `core/src/mcp/find_definition.rs`:
  - `with_source` (94-98): pass `end_col`.
  - `DefinitionCandidate` (127-152): add
    `#[serde(skip)] end_col: Option<i64>`.
  - Constructors: `From<SearchResult>` (162-172) sets `None`; the
    file-name rung (~1168-1178) sets `Some(n.end_col)`.
  - Candidate SQL (~416-418): add `n.endCol AS endCol`. `map_row`
    (423-440) reads it.
  - `CandidatePage::ambiguous` (220-255): pass it, and drop the
    Python-Module sentence from its doc (215-219).
- `core/src/mcp/find_definition/tests.rs`: rewrite 1908-1932 (behaviour 8).

### Read for context only

- `plugins/python/src/extractor/decls.rs` `announce`;
- `core/src/graph/containers.rs` `defining_containers`;
- `core/src/cli/plugin_check/session.rs` `declaration_edit`.

## Test plan (8 behaviours, one control each)

1. **SDK/TS rule (A).**
   - Test: the `columns.rs` table covers empty, whitespace-only, no final
     newline, final newline, `"\n\n"`, CRLF and trailing spaces.
   - Control: restore the old `CharColumns::file_range` body; the final-`\n`
     rows fail.
2. **Python File and Module.**
   - Test: on `"x = 1\n\n"`, both the File and the Module end at `(0,5)` and
     are equal. Also the table in `emit.rs`.
   - Control: restore the old Python `file_range`.
3. **Rust File.**
   - Test: the table in Rust `emit.rs`.
   - Control: restore the old Rust `file_range`.
4. **Go File.**
   - Test: the `textEndPosition` table and `computeFileNode` `(8,10)`.
   - Control: restore the old `textEndPosition`.
5. **plugin-check enforces the line.**
   - Test: the fake defect `file-end-past-last-line` makes only
     `id-stability.whitespace-edit` fail, with a finding naming
     `"a.fk#file"`. The existing `whitespace-moves-range` test still fails
     only that check.
   - Control: remove the new finding from `checks::whitespace_edit`; the new
     test's `assert_only_failure` fails.
6. **Old-index end is read.**
   - Test: `read_span_within` on `"a\nb\n"`, `(0, 2, end_col 0)`, returns
     `"a\nb"`.
   - Control: remove the clamp, which gives `None`.
7. **Every other past-the-end span is refused.**
   - Test: on `"a\nb\n"`, `(0,3,0)`, `(0,2,1)` and `(2,2,0)` are all `None`,
     and so is `(0,2,0)` on `"a\nb"`. The existing
     `a_span_past_the_end_of_the_file_reads_as_absent_rather_than_wrong`
     is kept.
   - Control: make the clamp unconditional (`end.min(len-1)`); the refusals
     fail.
8. **Ambiguous page, requests shape, old and new index.**
   - Test: candidates `a`, `b`, `old (0..2, col 0)` and `new (0..1, col 9)`
     in `"run = 1\nother = 2\n"`. All four carry source, and the explanation
     is `AMBIGUOUS_SOURCED`. A second page with a stale candidate (end line 5
     in a 2-line file) stays `AMBIGUOUS_PARTLY_SOURCED`.
   - Control: revert `read_span_within`; `old` loses its source and the
     explanation becomes `PARTLY_SOURCED`.

Existing tests that must keep passing unchanged: the whitespace tests listed
under "Unchanged" above, and every plugin's `tests/conformance.rs`, which runs
plugin-check end to end against the real plugin. No test involves processes,
threads or timers beyond plugin-check's existing ones.

## Must confirm (verify slice)

- On py-requests, after a fresh index, `__version__.py`'s Module ends at
  `(13, …)`, and the ambiguous `find_definition("__version__")` page carries
  its source.
- The same query against an index built before the fix (kept aside first)
  also carries the source.
- Every plugin passes `g-mesh plugins check`, including the new file-end
  finding.
- The ambiguous page's JSON has no new `endCol` key.

## Questions for the owner

### Q1. Where exactly should a file's range end?

- **Today:** a file ending in a newline is recorded as ending just after that
  newline, at "line 14, column 0" of a 14-line file (lines counted from 0, so
  that line does not exist). You chose to make it end on a real line.
- **Two ways to pick that line:**
  - **(A) Content end (recommended).** The end is where the file's last
    non-blank character is. Trailing spaces, tabs and blank lines are left
    out.
  - **(B) End of the last line.** The end is the end of the file's final
    line, including trailing spaces, and including a trailing blank line when
    the file ends in `"\n\n"`.
- **Example:** `"x = 1  \n\n"` (one line of code, two trailing spaces, an
  extra blank line):
  - (A) ends at line 0, column 5;
  - (B) ends at line 1, column 0;
  - today it ends at line 2, column 0.
- **Consequence:** a compliance check in plugin-check adds a space at the end
  of the file and requires that nothing the plugin reports changes. It exists
  to catch plugins whose ranges or ids shift for no real reason.
  - Under (A) the end does not move, so the check keeps its full strength and
    its code does not change.
  - Under (B) the end moves by one column. The check would have to excuse
    exactly that change, and would then miss the defect it is tested with (a
    range computed from the text length, which also moves by one).
  - Side effect of (A): asking `find_definition` for "what is at this
    cursor" on a trailing blank line answers "nothing here" instead of the
    whole file.

### Q2. Should plugin-check itself enforce the new end?

- **Today:** nothing checks the convention. Each plugin just follows a doc
  comment, which is how all four ended up on the old one.
- **Options:**
  - **Inside the whitespace-edit check, line only (recommended).** That check
    already has the edited file's bytes. It adds one finding when the File
    node's end line is not the file's last non-blank line. Columns are not
    compared, because core does not know whether a plugin counts columns in
    characters, bytes or UTF-16 units.
    - Example: a plugin still sending `(14,0)` for a 14-line file fails
      `id-stability.whitespace-edit` with "File ends on line 14, the file's
      content ends on line 13".
  - **A new check id** (`range.file-end`). Clearer in the report, but it adds
    an entry to the check list that 5 test files assert in full.
  - **No enforcement.** Only the plugins' own unit tests pin it. An external
    plugin can still send the old end, and its Module (if it has one) loses
    source again.
- **Consequence:** with enforcement, every plugin's end-to-end conformance
  test guards the AC, not only its unit tests.
