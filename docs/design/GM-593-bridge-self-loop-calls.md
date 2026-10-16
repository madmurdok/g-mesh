# GM-593: the bridge's enclosing-node fallback makes wrong self-loop CALLS

Status: design (S1). Branch `fix/GM-593-bridge-self-loop-calls`, off
release-4.10.0 (d8fcc50). Follows GM-569 (ADR 0035), D1 option B, which was
left for this task.

Tags: [code] read in this branch; [run] measured on the GM-569/S8 "after"
indexes (71b4e88: GM-567 + GM-569), excalidraw 1acf66ed, ripgrep e89fff8,
which are still in the GM-569 session scratchpad (`gm569-s8/index-*-after.db`);
no reindex was needed.

## 0. Summary

A `CALLS` answer that lands on a `Function` is sent wherever in that function
it points. When the engine answers a declaration g-mesh has no node for (a
Rust nested `fn`, a TS local, a hook's destructured binding, a property of an
inline parameter type, a default import), `node_at` lands on the function
around it, which for such a call is the caller itself: a self-loop. The
corpora have 23 semantic `CALLS` self-loops; 17 are this fallback and 6 are
real recursion.

**Recommendation: option C. A `CALLS` landing on the caller itself is kept
only when it points at the caller's own name** (GM-569's `lands_on_name`). It
drops the 17 wrong edges, keeps the 6 recursive ones, and touches nothing
except self-landings. That is a one-condition change in `record_answer`.

## 1. What happens today

`record_answer`'s `Ask::Definition` branch (`plugins/sdk/src/lsp/bridge.rs`
2568-2736) runs every location through `node_at` (1997-2007), which returns
the smallest node whose range contains it (`SdkIndex::node_at`). GM-569 then
filters the landing (2613-2623):

```rust
agree && (site.edge_kind != EdgeKind::Calls
    || node.kind == NodeKind::Function
    || lands_on_name(index, path, wire_position(..), node))
```

A `Function` landing is never held to its name. [code]

**Example** (ripgrep `crates/cli/src/decompress.rs` 448-488):

```rust
fn try_resolve_binary<P: AsRef<Path>>(prog: P) -> Result<PathBuf, CommandError> {
    fn is_exe(path: &Path) -> bool { .. }   // no node: nested fns are not indexed
    ..
    if is_exe(&path) { .. }                 // rust-analyzer answers `is_exe` at 453
}
```

`node_at(453:7)` lands on `try_resolve_binary`. The site's `from_id` is
`try_resolve_binary` too, so `find_callers try_resolve_binary` lists itself.
The excalidraw case from the task: woff2-loader's `load` calls
`bindings({ wasmBinary: binary })`, where `bindings` is a default import, and
gets a `CALLS` edge onto `load`. [run]

### The 23 semantic CALLS self-loops [run]

Semantic `CALLS` with `fromId = toId`, resolved, on the GM-569/S8 "after"
indexes. To classify them, I grepped each caller's body for its own name and
for nested declarations:

| corpus | fallback (wrong) | recursion (right) |
|---|---|---|
| excalidraw | 8: `duplicateElements` (local arrow `insertBeforeOrAfterIndex`), `DarkModeToggle`, `ElementCanvasButton` (`props.onChange()`, inline param type), `ChartPreviewBtn`, `PlainTextPreviewBtn`, `RadioSelection`, `getErrorLineExtension`, woff2 `load` (default import `bindings`) | 0 |
| ripgrep | 9: nested `fn` in `try_resolve_binary` (`is_exe`), `default_decompression_commands` (`add`), `is_readable_stdin` (`imp` x3), `HiArgs::sort` (`attach_timestamps`), `Parser::parse_class` (`add_to_last_range`), `walk::path_equals` (`never_equal`), `PrinterPath::with_separator` (`replace_separator`), `PrinterPath::as_path` (`imp`), `trim_ascii_prefix` (`is_space`) | 6: `Error::{is_partial, is_io, io_error, into_io_error, depth}` and `<Error as Display>::fmt`, each `err.<same>()` on a boxed inner `Error` |

The task text says "4" for excalidraw, but that was the count before GM-567.
GM-567 added 4 more, so 8 are live now (GM-567/S9 named them). Syntactic
self-loops (15 and 9) are made by the structural tier, not the bridge, and are
out of scope.

Non-self fallback onto a `Function` would be a semantic `CALLS` from a node
nested inside the function it lands on. There are **0** on both corpora
(range-containment query, caller != target). [run] So on these corpora, every
wrong `Function` landing is a self-loop.

## 2. Options

Counts are semantic `CALLS` edges (excalidraw / ripgrep).

### A. Hold every `CALLS` landing to its name (GM-569 D1 option B)

Drop the `node.kind == NodeKind::Function` arm.

- Dropped: 8 / 9 wrong. Kept recursion: 6 (the answer points at
  `fn is_partial`'s name).
- Every other `Function` landing (1794 / 1716) now has to pass the name check
  too. Static emulation found the target's name on a non-comment line in every
  range, so the predicted loss is 0. That does not prove it, because the index
  does not store the answer's column. [run]
- Benefit: it would also catch a non-self fallback onto a function (0 today).
- Risk: it changes the acceptance path of about 3.5k edges on every bridge
  language (TS, Python, Rust). Python is not measured. A node whose name is
  not the identifier at its declaration loses all its callers. The TS
  extractor names an anonymous `export default () => {}` `default`, its range
  starts at `(`, so any call answered onto it fails (0 semantic edges onto the
  2 such nodes today; 2 syntactic). A named `export default function Home` is
  named `Home` (index: `dev-docs/src/pages/index.tsx`) and is safe. The
  existing test `a_call_answered_inside_a_function_still_lands_on_it` inverts.
  The measure slice must classify every lost edge.

### B. Drop every landing on the caller itself

`node.id != site.from_id`.

- Dropped: 8 / 15, which includes **6 right recursive edges**.
- Benefit: simplest, no text inspection.
- Risk: loses all genuine recursion that the engine answered.
  `find_callers Error::is_partial` would no longer list itself.

### C. Drop a landing on the caller itself unless it points at the caller's name (Recommended)

```rust
agree && (site.edge_kind != EdgeKind::Calls
    || (node.kind == NodeKind::Function && node.id != site.from_id)
    || lands_on_name(index, path, wire_position(..), node))
```

- Dropped: 8 / 9 wrong. Kept recursion: 6. Other edges changed: 0 by
  construction, because only self-landings (23 edges) reach the new check.
- `export default`: an anonymous `default` node cannot call itself by name,
  so a self-landing on it is always a fallback and is dropped, which is
  right. A call from another function into it is not a self-landing and is
  recorded as today. A named `export default function Home` recursing at
  `Home` passes `lands_on_name`.
- Benefit: the smallest change that removes all 17. It reuses GM-569's
  vetted check. ADR 0035's "a false 'not at its name' only drops an edge"
  still holds.
- Risk: (1) a non-self fallback onto a `Function` (a node nested in the
  function it lands on) still links: 0 on both corpora. (2) Recursion inside a
  node whose `name` does not occur first in its range is dropped (an anonymous
  `default`, or a range that starts at a doc comment containing the name): 0
  of the 6 on the corpora.

## 3. Decisions

- **D1. Option.** C (Recommended) / A / B, as compared above.
- **D2. REFERENCES self-loops** (not asked; found here). ripgrep has 20
  semantic `REFERENCES` self-loops (17 Function: mostly
  `<X as Serialize>::serialize` and `Searcher::search_*`; 2 Type: `Space`,
  `DecimalFormatter`), excalidraw has 0. This is the same fallback for a bare
  use, e.g. a local or a nested item. The recommendation is to keep C
  `CALLS`-only (the task's scope) and open a backlog task to classify them.
  Risk of including them now: none of them is classified, and a reference to
  a function's own parameter type is often right.

## 4. Edit map (option C)

Change:

- `plugins/sdk/src/lsp/bridge.rs` `record_answer` 2568-2736: the comment and
  filter at 2613-2623. Add `&& node.id != site.from_id` to the `Function`
  arm, and extend the comment: a call landing on its own caller is the
  encloser fallback unless it points at the caller's name (recursion).
- `bridge.rs` `lands_on_name` doc 2009-2014 and `node_at` doc 1991-1996:
  say that the check also applies to a `CALLS` landing on its own caller.
- `docs/adr/0035-engine-calls-answers-kind-rule.md`: add a GM-593 line to D1
  ("... and a `CALLS` answer landing on its own caller") and to Consequences
  ("nested-fn / local calls no longer produce self-loops; recursion stays").

Read, unchanged: `untyped_call_answered` (2754). It is computed before the
filter, so a dropped self-loop still counts as answered, which is GM-569's
behaviour and keeps GM-497's structural-list trimming. Also read
`overload_landing` (2085). Overload sets are precise declarations, so they
are untouched, and the overload branch returns before the filter.
`OpenSite::from_id` (`plugins/sdk/src/graph.rs` 152) is the caller's node id,
the same id space as `WireNode::id`. [code]

Tests go in `bridge.rs`'s test module, beside GM-569's
(`landing_index` 3707-3732, `answered_by` 3617-3630, tests 3737-3792). A self
site is `site_of(&index, "len", 1)` with `from_id` set to `outer`'s node id
(the `answered_by` caller key changes with it). For the export-default case,
add a `Function` node named `default` to `landing_index`, with source like
`export default () => { go(); };` and its range starting at `(`.

## 5. Behaviours the tests must pin, with controls

| # | Behaviour | Test (sketch) | Control (revert, the test must fail) |
|---|---|---|---|
| T1 | A call landing inside its own caller, away from the caller's name, records nothing | self site, answer `in_a(9, 17)` (`inner` inside `outer`) -> 0 edges | Drop `&& node.id != site.from_id` (today's filter): 1 edge |
| T2 | Genuine recursion is kept: a self-landing at the caller's name records the edge | self site, answer `in_a(9, 3)` (`outer`'s name) -> 1 edge | Make self-landings always drop (option B: `node.id != site.from_id &&` around the whole filter): 0 edges |
| T3 | A call from another function landing inside a function anywhere is still recorded (non-self is not held to the name) | existing `a_call_answered_inside_a_function_still_lands_on_it`, plus the anonymous `default` node answered from b.toy's caller -> 1 edge | Option A: drop the `Function` arm entirely: 0 edges |
| T4 | export default: a self-landing in an anonymous `default` function records nothing, because its name never occurs in its range | self site with `from_id` = `default`'s id, answer inside its body -> 0 edges | Same as T1's control (shared build): 1 edge |

That is 3 distinct reverts (T1/T4 share one) in one test binary (`-p
g-mesh-plugin-sdk --lib lsp::bridge`). Optional T5 if D2 keeps REFERENCES
out: a `REFERENCES` self-landing inside `outer` is still recorded; control:
drop the `site.edge_kind != Calls` guard for self-landings.

## 6. Measure slice

Reuse GM-569/S8's `run.sh` shape: release build per arm, roots typescript +
rust, reindex excalidraw and ripgrep, before 4.10.0 base / after the branch.
Expected: semantic `CALLS` self-loops 8 -> 0 and 15 -> 6 (the six `Error::*`
remain). All other semantic `CALLS` edges are unchanged (lost 0, retargeted 0).
Total edges should drop by the number of unsent answers. Cheap observation:
`try_resolve_binary`'s self-edge is gone and `Error::is_partial`'s remains.
