# GM-527: a whole-file node's end is one past the last line

Status: design (GM-527/S1). No production code changed by this note.

## The problem

On py-requests, `find_definition` for `__version__` returns an ambiguous page.
One candidate is the `__version__.py` `Module` node, stored as
`(0,0)-(14,0)` in a 14-line file whose last byte is `\n`.
`core/src/mcp/source.rs` `read_span_within` refuses any span with
`end >= lines.len()` as stale. `lines.len()` is 14, so the candidate gets no
`source`, and the page says `AMBIGUOUS_PARTLY_SOURCED` (GM-352).

## Cause: a deliberate convention

The off-by-one is not a tree-sitter accident in one plugin. Every plugin gives
the whole-file range the same end:
`(number of '\n', number of chars after the last '\n')`.
For a file that ends in `\n`, that is `(lineCount, 0)`: a position just past
the last line, where an editor's cursor sits after the final newline. It is
"one past" only for a reader that turns a range into a list of whole lines.

The convention exists for the plugin-check `id-stability.whitespace-edit`
check (`core/src/cli/plugin_check/session.rs` `whitespace_edit`). The check
inserts a space before the file's last newline and requires an empty diff. An
end of `(newlines, tail)` does not move under that edit. An end of
`(lastLine, lengthOfLastLine)` would move by one column.

### Per plugin

Checked in code and in the live index of the main checkout (sqlite on
`~/.g-mesh/projects/959ade85d9a343b1/index.db`; script output in
`SCRATCH/gm527-nodes.txt`). The index compares each stored end with the text
end recomputed from the file on disk.

| Plugin | Node(s) with the whole-file range | Site | Ends in `\n` | No final `\n` |
|---|---|---|---|---|
| SDK (shared) | contract for `File` | `plugins/sdk/src/columns.rs` `Columns::file_range` (61); doc on `FileGraphBuilder::file_node` (`plugins/sdk/src/graph.rs` ~455-470) | `(n, 0)` = `lines().count()`; pinned by `plugins/sdk/tests/columns.rs` `columns_file_range_of_a_terminated_file_ends_on_the_empty_last_line` | `(n, tail)`, `n = lines().count()-1`; pinned by `columns_file_range_ends_at_newlines_and_last_line_chars` |
| TypeScript | `File` only | `plugins/typescript/src/extractor/mod.rs:94` -> `FileModel::new` (`model.rs:253`) | same | same |
| Python | `File` **and** the file's own `Module` (`native_kind` `module` or `package`) | `plugins/python/src/extractor/emit.rs` `Positions::file_range` (130), used by `emit.rs:209` (File) and `decls.rs` `Declarer::announce` (158-177, Module) | index: 46 File, 43 module, 2 package nodes, all `(n,0)` with `n == lines().count()` | `(n, tail)`, within the file |
| Rust | `File` only. `mod x;` / `mod x {}` Modules use the item's AST range (`decls.rs` `mod_item`, `positions().range(item)`) | `plugins/rust/src/extractor/emit.rs` `Positions::file_range` (95), used at 158 | index: 340 File nodes, all `(n,0)` = line count; 5 Module nodes at (0,0) are one-line `mod` items, inside the file | `(n, tail)` |
| Go | `File` only. Go `Module` nodes are import or external placeholders (`extract.go:482`, `semantic.go:1006`, `uses.go:773`); no package node has a file range | `plugins/go/extract.go` `computeFileNode` (942) / `textEndPosition` (976) | index: 41 File nodes, all `(n,0)` = line count | `(n, tail)`, pinned by `extract_test.go` |

Summary:
- The shape is shared by every plugin's `File` node.
- Python is the only plugin that also gives a *declaration* (the `Module`)
  that range, which makes it a `find_definition` candidate.
- A file without a final newline is never affected: its end line is the last
  real line.
- No index row exceeds `(lineCount, 0)`.
- The TypeScript rows could not be checked in the live index, because the TS
  plugin failed to start there (the `select_project` guidance said so).
  Its end comes from the same SDK function as the measured rows.

## Readers of `endLine`

| Reader | Found by | What it does with the end | Affected? |
|---|---|---|---|
| `mcp::source::read_span` | g-mesh `find_callers(read_span_within)` | wrapper | yes, the site of the fix |
| `mcp::find_definition::DefinitionNode::with_source` (94) | g-mesh `find_callers(read_span)` | source of a resolved answer | yes: a resolved Python Module gets no source today either |
| `mcp::find_definition::CandidatePage::ambiguous` (220) | g-mesh `find_callers(read_span_within)` | candidate sources on an ambiguous page | yes, the reported case |
| 5 tests in `source.rs` | g-mesh `find_callers(read_span)` | unit tests | signature change only |
| `graph::queries::find_by_position` (705-720) | grep `endLine` in core | `(endLine > l OR (endLine = l AND endCol >= c))` | no: inclusive containment, `(n,0)` is correct |
| `graph::containers::defining_containers` (675-690) | grep | `m.endLine >= f.endLine` compares a Python Module with its File | no under (a); under (b) and (c) only if both nodes change together |
| `cli::plugin_check::session::whitespace_edit` (689) and `checks::whitespace_edit` (665) | brief + grep | compares stored ranges before and after the edit | no under (a); breaks under (b) and (c) unless the check changes |
| `cli::plugin_check::session::declaration_edit` (629-655) | grep | breaks the line at the *first non-File declaration's* end line. For Python that is often the Module, so `at == bytes.len()` and the edit lands at EOF | no under (a). Under (c) the Python edit moves from EOF to before the last line (stronger, but a behaviour change) |
| `mcp::get_file_outline` (38, 73) | grep | passes `endLine` through | no under (a) |
| `watcher::apply` (632, 664), storage | grep | stores the wire range as is | no under (a) |
| `cli::embed_eval::churn` (170-240) | grep | eval tool; inserts a line at a file's `end_line` | no under (a) |
| conformance `expect.toml` (go, python, rust, typescript) and `plugin_check/expectations.rs` | grep `line\|range\|end` | no range or end-line expectations | no |

The g-mesh calls ran against project `g-mesh`, because the worktree is not
in the project list and this code is unchanged on the branch. Both
`find_callers` calls came back complete (`hasMore: false`,
`allUnresolved: false`). The remaining reads are SQL strings and struct
fields, which g-mesh does not track, so grep found them.

## Options

### (a) Core readers treat `(lineCount, 0)` as the file's end and clamp (recommended)

`read_span_within` takes `end_col` as well. It accepts
`end_line == lines.len()` exactly when all three hold:
- `end_col == 0`;
- the file ends in `\n`;
- `start_line < lines.len()`.

It then reads up to the last line, `lines.len() - 1`. Everything else keeps
today's checks.

- Benefits:
  - One function and its two callers change; no plugin changes, and no
    whitespace-check change.
  - It works on indexes that already exist, with no reindex. Under (b) or (c),
    stored rows keep `(n,0)` until each file is re-walked.
  - The convention stays the documented SDK contract.
- Risks:
  - `endLine` in responses still reads `14` for a 14-line file. That is
    correct as a position, but an agent that runs `sed -n 1,15p` from it
    asks for one line too many, which is harmless.
  - `DefinitionCandidate` needs an `end_col` it does not serialize today.
    It is added as a `#[serde(skip)]` field, so the response shape does not
    change.

### (b) Core normalizes the end at storage or in responses

The end is rewritten as `(n,0)` -> `(n-1, lengthOfLine(n-1))`.
- At storage (`watcher::apply`), the plugin-check whitespace check reads the
  stored ranges, and the new end moves under its edit. The check fails for
  every plugin.
- In responses, core has to read the file to learn the last line's length.
  That is the same file read (a) already does, plus a second meaning for
  `endCol`.
- Benefit: `endLine` in a response is always a real line.
- Risks: storage and responses disagree, or the whitespace check breaks.
  Several output sites change (find_definition, outline, search).

### (c) Change the plugin convention and the whitespace check

All five sites move to `(lastLine, lengthOfLastLine)`, and the whitespace
check stops comparing the File node's (and Python Module's) end column.
- Benefit: endLine is the last real line everywhere, at the source.
- Risks:
  - Four plugins in three languages, the SDK contract and its tests change,
    plus the plugin-check rule that exists to keep this end still.
  - Existing indexes keep `(n,0)` until every file is re-walked, so (a)'s
    reader fix is still needed in the meantime.
  - It changes `declaration_edit`'s Python target, and an external plugin
    author's contract.

## Recommendation

Option (a), with the strict rule (`end_col == 0` and a final `\n`).

How it meets AC2 ("endLine never exceeds the file's line count"):
- Read literally, it already holds: no stored `endLine` is greater than the
  line count (measured above). The largest is equal, at column 0 after a
  final newline.
- What the AC means is "such a span is readable", and (a) makes it readable.
- To remove the ambiguity, the proposed AC2 wording for the owner is:

> After the fix, an `endLine` is at most the file's line count, and equal only
> as `(lineCount, 0)` in a file ending in a newline, the position after the
> last newline, which every source reader treats as the end of the last line.
> A Module candidate gets source on an ambiguous page (requests
> `__version__`).

### Stale spans stay refused

A file can only fall short of a stored span if it was shortened. Each case:
- **Stored end beyond the line count** (`end_line > lines.len()`): refused,
  as today.
- **`end_line == lines.len()` with `end_col > 0`**: refused. A non-zero
  column cannot exist past the final newline.
- **`end_line == lines.len()` in a file with no final newline**: refused.
  That line does not exist.
- **Accepted only**: `(lines.len(), 0)` with a final `\n`. That is exactly the
  current file's own end, so the span is "the whole current file from
  `start`". For a whole-file node this is the right text even if the file
  changed. A non-file node that happens to end there today would be served
  the trailing lines; with `end_col == 0` this is not a shape any plugin emits
  for a declaration (the index shows none).
- **`start_line >= lines.len()`**: refused, as today. This includes an empty
  file, which has nothing to show.

## Edit map

Change:
- `core/src/mcp/source.rs`:
  - `read_span` (79-88): add an `end_col: i64` parameter and pass it through.
  - `read_span_within` (91-124): add `end_col`, and add the clamp before the
    stale check. Rewrite the stale comment (106-108) to name the one accepted
    end.
  - Doc comment (70-78): state the `(lineCount, 0)` rule.
- `core/src/mcp/find_definition.rs`:
  - `DefinitionNode::with_source` (94-98): pass `self.end_col`.
  - `struct DefinitionCandidate` (127-152): add
    `#[serde(skip)] end_col: Option<i64>`.
  - Constructors: `From<SearchResult>` (162-172) sets `None`; the
    file-name-rung constructor (~1168-1178) sets `Some(n.end_col)`.
  - Candidate SQL (~416-418): add `n.endCol AS endCol`. `map_row` (423-440)
    reads it.
  - `CandidatePage::ambiguous` (220-255): pass the column. Drop "a Python
    `Module` whose `endLine` is one past ..." from its doc (215-219), and keep
    "a file edited since the walk".
- `core/src/mcp/find_definition/tests.rs`:
  - `runs_one_past_the_end` and
    `a_page_where_one_candidates_span_ends_past_the_file_says_only_some_are_sourced`
    (1908-1932) assert today's refusal, so they flip under the fix.
  - Rewrite them: the `(0,2)`-in-`m.rs` candidate is now sourced, and a
    separate genuinely stale candidate (end line 5 in a 2-line file) keeps the
    partly-sourced case.

Read for context, do not change:
- `plugins/sdk/src/columns.rs` `file_range`;
- `plugins/python/src/extractor/decls.rs` `announce`;
- `core/src/cli/plugin_check/session.rs` `whitespace_edit`;
- `core/src/graph/containers.rs` `defining_containers`.

Optional, docs only: add one sentence to `FileGraphBuilder::file_node`'s doc
saying that core reads `(n, 0)` as the end of the last line.

## Test plan (behaviours, one control each)

1. **The file's end is readable.**
   - Test: `read_span_within` on `"a\nb\n"` with `(0, 2, end_col 0)` returns
     `"a\nb"`, `first_line 1`, no omission.
   - Control: remove the clamp; the result is `None`.
2. **Shortened file, end beyond the line count.**
   - Test: the existing `a_span_past_the_end_of_the_file_reads_as_absent_rather_than_wrong`,
     plus `(0, 3, 0)` on `"a\nb\n"` returns `None`.
   - Control: make the clamp unconditional (`end.min(len-1)`); the test fails.
3. **Non-zero column at the line count.**
   - Test: `(0, 2, end_col 1)` on `"a\nb\n"` returns `None`.
   - Control: drop the `end_col == 0` condition.
4. **No final newline.**
   - Test: `(0, 2, 0)` on `"a\nb"` returns `None`, while `(0, 1, 1)` returns
     `"a\nb"`.
   - Control: drop the "ends with `\n`" condition.
5. **The start is still checked.**
   - Test: `(2, 2, 0)` on `"a\nb\n"` returns `None`, and the empty file with
     `(0, 0, 0)` returns `None`.
   - Control: clamp `start` as well.
6. **Ambiguous page, Module-shaped candidate (AC2).**
   - Test: the rewritten `runs_one_past_the_end` page with candidates
     `a`, `b`, `m (0..2, col 0)` carries source for all three, and the
     explanation is `AMBIGUOUS_SOURCED`.
   - Control: revert `read_span_within`; it gives `["a","b"]` and
     `PARTLY_SOURCED`.
7. **Partly-sourced still holds for a stale candidate.**
   - Test: a page where one candidate ends at line 5 of a 2-line file is
     `AMBIGUOUS_PARTLY_SOURCED`, and that candidate has no source.
   - Control: the unconditional clamp from 2.
8. **A resolved answer to a whole-file Module carries `source`.**
   - Test: `find_definition` by id on a node `(0,0)-(n,0)` returns `source`.
   - Control: pass `0`→`1` as `end_col` in `with_source`, or revert, and the
     source is absent.

The tests use no processes, threads or timers, so no 5x repetition rule
applies.

## Must confirm (verify slice)

- On py-requests, an ambiguous `find_definition("__version__")` page carries
  source for the `__version__.py` Module candidate, and the explanation is
  not `AMBIGUOUS_PARTLY_SOURCED` (unless another candidate is stale). This is
  a live-index check, not a unit test.
- `plugin-check` still passes `id-stability.whitespace-edit` for every
  plugin. No plugin code changes, so this should hold trivially.
- The response JSON of an ambiguous page has no new `endCol` key.

## Questions for the owner

### Q1. Where to fix it: the reader (a), the stored value (b), or the plugins (c)?

- **Today:** for a file ending in a newline, g-mesh records a whole file as
  ending at `(lineCount, 0)`. That is the spot just after the final newline,
  like a cursor at the very end in an editor. Python also gives this range to
  the file's `Module` symbol. When `find_definition` wants to show that
  symbol's text, the source reader treats line `lineCount` as "past the end,
  the file must have shrunk" and shows nothing.
- **Example:** `requests/__version__.py` has 14 lines and ends with `\n`.
  Its Module is stored as `0:0-14:0`.

| Option | Stored range | `endLine` in the answer | Source shown? | Also changes |
|---|---|---|---|---|
| today | `0:0-14:0` | 14 | no | - |
| (a) reader clamps | `0:0-14:0` | 14 | yes, lines 1-14 | 2 core functions |
| (b) core rewrites | `0:0-13:N` (N = length of line 14) | 13 | yes | the plugin-check whitespace rule, or storage/response disagreement |
| (c) plugins change | `0:0-13:N` | 13 | yes, but only after a reindex | 4 plugins + SDK contract + plugin-check rule; old indexes still need (a) |

- **Consequence:** (a) is the smallest change and also fixes indexes that
  already exist. (b) and (c) make `endLine` always a real line, at the cost of
  rewriting a rule that is there on purpose.
- **Recommended:** (a).

### Q2. AC2 wording

- **The issue:** "endLine never exceeds the file's line count" is already true
  as written, because 14 does not exceed 14. So it does not say what the task
  wants.
- **Under (a):** the stored and returned value stays 14, and the fix is that
  it is now read as "end of line 14".
- **Proposed wording:** "endLine is at most the file's line count, and equal
  only as `(lineCount, 0)` after a final newline, which every source reader
  treats as the end of the last line. A Module candidate gets source on an
  ambiguous page (requests `__version__`)."
- **If you want the literal reading** ("endLine is a real line, at most
  lineCount-1"), that means (c), and Q1 changes with it.

### Q3. How strict should the reader be?

- **Strict (recommended):** accept `endLine == lineCount` only when
  `endCol == 0` and the file ends in a newline.
  - Example on `"a\nb\n"`: `(0..2, col 0)` is served; `(0..2, col 1)` is
    refused as stale.
  - Cost: the column has to travel to the reader, as a hidden field on the
    candidate.
- **Loose:** accept `endLine == lineCount` whenever the file ends in a
  newline, whatever the column.
  - Example: both spans above are served.
  - Cost: one fewer field. A symbol from a longer, older version of the file
    whose end now happens to equal the line count would be shown the file's
    last lines instead of nothing.
- **Consequence:** strict keeps the stale guard as tight as it is today, at
  the price of about 10 extra lines.
