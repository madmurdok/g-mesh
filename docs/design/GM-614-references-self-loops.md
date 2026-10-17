# GM-614: semantic REFERENCES self-loops on ripgrep

Status: design (S1). Branch `docs/GM-614-references-self-loops`, off
release-4.11.0 (`a561175`, includes GM-593). Follows GM-593 design D2.

Tags: [code] read in this branch; [run] queried on the GM-593/S9 "after"
index of ripgrep (`gm593-s9/index-ripgrep-after.db` in the GM-593 session
scratchpad, ripgrep `e89fff8`, clean checkout in
`$TMPDIR/gmesh-bench-corpora/ripgrep`); no reindex. [inferred] reasoned from
the landing, not observed on the wire.

## 0. Summary

All 20 were found. **19 are the enclosing-node fallback and 1 is real.** In
18 of the 19, the reference is to a generic type parameter (`S::Ok`,
`S::Error`, `M::Captures`). One is a nested `enum`. Neither has a node of its
own, so the definition lands on the function or struct that declares it. The
one real edge is `Self::MAX_U64_LEN` inside `struct DecimalFormatter`, where
`Self` is the struct itself.

The same bug also makes **16 wrong edges that are not self-loops**. In each,
a method uses `S::Error` from its `impl<S: Sink>` header, and the edge lands
on the `impl` node that encloses the method.

**Recommendation: extend GM-593's rule to REFERENCES, in its "encloser"
form.** If the landing node is the referrer or contains the site, the answer
is kept only when it points at that node's own name (`lands_on_name`). This
drops 35 edges (the 19 self-loops and the 16 impl edges) and keeps
`DecimalFormatter`. Apply the same predicate to `CALLS`. It covers everything
GM-593's `node.id != site.from_id` covers, and on the corpora it changes no
`CALLS` edge.

## 1. What happens today

`record_answer` (`plugins/sdk/src/lsp/bridge.rs` 2626-2800) sends every
definition location through `node_at` (2040-2050). `node_at` picks the
tightest addressable node whose range contains the location
(`SdkIndex::node_at`, `index.rs` 176). GM-569 and GM-593 then filter only
`CALLS` landings (2692-2705):

```rust
agree && (site.edge_kind != EdgeKind::Calls
    || (node.kind == NodeKind::Function && node.id != site.from_id)
    || lands_on_name(..))
```

A `REFERENCES` landing is never filtered. The test
`a_reference_landing_inside_its_own_encloser_is_recorded` (4339-4350) pins
that on purpose, as GM-593's T5. [code]

How the site was found: each semantic edge's `linkedFrom` is the
`pending_symbol` placeholder node whose range is the site (`site_range(site.
position, &site.name)`). The bridge asks `textDocument/definition` at
`site.position`, the first character of the path (`S` in `S::Ok`). [code]
For 19 of the 20 edges the placeholder belongs to that edge alone. For
`search_slice` it is shared with 3 test `CALLS` edges onto `search_slice`,
and the placeholder's range is still the self-reference site. [run]

**Example** (`crates/printer/src/jsont.rs` 19-22):

```rust
fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
```

Asked at `S` of `S::Ok`, rust-analyzer answers with the type parameter `S` at
19:18. The parameter has no node, so `node_at` returns `serialize`, and the
edge is `serialize REFERENCES serialize`. [inferred: the landing is inside
`serialize`'s range; the only declaration there that `S` can name is the
parameter. Asked at `Ok`, the answer would be serde's associated type, which
is outside the index, so no edge would be made.]

## 2. The 20 REFERENCES self-loops [run]

Lines and columns are 1-based. "Site" is the placeholder range. "Landing" is
where the definition pointed.

| # | node (kind) | file:node line | site | site text | landing | class | would pass `lands_on_name` |
|---|---|---|---|---|---|---|---|
| 1 | `<Message as Serialize>::serialize` (Function) | printer/src/jsont.rs:19 | 22:17 | `S::Ok` | type param `S` in `serialize<S: ..>` | fallback | no |
| 2 | `<Begin as Serialize>::serialize` | printer/src/jsont.rs:53 | 56:17 | `S::Ok` | type param `S` | fallback | no |
| 3 | `<End as Serialize>::serialize` | printer/src/jsont.rs:72 | 75:17 | `S::Ok` | type param `S` | fallback | no |
| 4 | `<Match as Serialize>::serialize` | printer/src/jsont.rs:95 | 98:17 | `S::Ok` | type param `S` | fallback | no |
| 5 | `<Context as Serialize>::serialize` | printer/src/jsont.rs:120 | 123:17 | `S::Ok` | type param `S` | fallback | no |
| 6 | `<SubMatch as Serialize>::serialize` | printer/src/jsont.rs:144 | 147:17 | `S::Ok` | type param `S` | fallback | no |
| 7 | `<Data as Serialize>::serialize` | printer/src/jsont.rs:204 | 207:17 | `S::Ok` | type param `S` | fallback | no |
| 8 | `<Stats as Serialize>::serialize` | printer/src/stats.rs:152 | 155:17 | `S::Ok` | type param `S` | fallback | no |
| 9 | `<NiceDuration as Serialize>::serialize` | printer/src/util.rs:392 | 395:17 | `S::Ok` | type param `S` | fallback | no |
| 10 | `Searcher::search_path` | searcher/src/searcher/mod.rs:643 | 648:21 | `S::Error` | type param `S` in `search_path<P, M, S>` | fallback | no |
| 11 | `Searcher::search_file` | searcher/src/searcher/mod.rs:665 | 670:21 | `S::Error` | type param `S` | fallback | no |
| 12 | `Searcher::search_file_maybe_path` | searcher/src/searcher/mod.rs:678 | 684:21 | `S::Error` | type param `S` | fallback | no |
| 13 | `Searcher::search_reader` | searcher/src/searcher/mod.rs:727 | 732:21 | `S::Error` | type param `S` | fallback | no |
| 14 | `Searcher::search_slice` | searcher/src/searcher/mod.rs:769 | 774:21 | `S::Error` | type param `S` | fallback | no |
| 15 | `Searcher::fill_multi_line_buffer_from_file` | searcher/src/searcher/mod.rs:940 | 943:21 | `S::Error` | type param `S` in `<S: Sink>` | fallback | no |
| 16 | `Searcher::fill_multi_line_buffer_from_reader` | searcher/src/searcher/mod.rs:975 | 978:21 | `S::Error` | type param `S` in `<R: io::Read, S: Sink>` | fallback | no |
| 17 | `replace_with_captures_in_context` (Function) | printer/src/util.rs:557 | 562:16 | `M::Captures` | type param `M` in `<M, F>` | fallback | no |
| 18 | `Space` (Type, struct) | printer/src/util.rs:18 | 20:11 | `M::Captures` | type param `M` in `struct Space<M: Matcher>` | fallback | no |
| 19 | `<HyperlinkFormat as FromStr>::from_str` (Function) | printer/src/hyperlink/mod.rs:102 | 125:25 | `State::OpenVariable` | nested `enum State` at 106 (nested items have no node) | fallback | no |
| 20 | `DecimalFormatter` (Type, struct) | printer/src/util.rs:415 (range starts at `#[derive]`, 415; name at 416) | 417:15 | `Self::MAX_U64_LEN` | the struct `DecimalFormatter` (what `Self` names) | **real** | yes [inferred] |

Evidence for #20: the landing is inside the struct's range (415-418). That
range holds only the struct's name and two fields. A field would be the
tighter node, so the location must be at the struct itself. For an ADT,
rust-analyzer's `Location` is the name's focus range. `MAX_U64_LEN` is an
associated const in a separate `impl`, outside this range. So the edge says
"`DecimalFormatter` uses its own type", the same as a field of type
`Box<Self>`. It is real, but not very useful.

### The same fallback, not a self-loop [run]

Query: semantic edges whose placeholder site lies inside the target's range,
same file.

| kind | self | non-self | what |
|---|---|---|---|
| REFERENCES | 20 (above) | 16 | 7 methods of `impl<'a, S: Sink> Sink for &'a mut S` (sink.rs 225) and 7 of `impl<S: Sink + ?Sized> Sink for Box<S>` (sink.rs 278), each `-> impl` node via `S::Error`; `type Error`/`type Captures` of `impl<'a, M: Matcher> Matcher for &'a M` (matcher/lib.rs 1133) via `M::Captures` |
| CALLS | 6 (recursion, kept by GM-593) | 3 | not real: the 3 test `CALLS` onto `search_slice` share #14's placeholder, and their real sites are in the tests at 1052-1072 |

All 16 semantic `REFERENCES` onto an `impl` node are these. Every semantic
`REFERENCES` onto a `Function` is one of the 18 self-loops. excalidraw has 0
semantic edges whose site is inside their target (`gm593-s9/index-excalidraw-
after.db`).

## 3. Options

All options change only `record_answer`'s landing filter.

### A. Extend C verbatim: a REFERENCES self-landing is kept only at the referrer's name

- Today: `serialize REFERENCES serialize` is recorded.
- Change: `node.id == site.from_id` makes a `REFERENCES` landing go through
  `lands_on_name`.
- Example: #1-#19 are dropped. #20 is kept. `matched -> impl Sink for &mut S`
  is still recorded.
- Consequence: drops 19 and keeps 1. The 16 impl edges stay wrong.

### B. Encloser rule: a landing on the referrer or on any node containing the site is kept only at its name (Recommended)

- Today: the 20 self-loops and the 16 impl edges are recorded.
- Change: if the landing node is the referrer, or is in the site's file and
  its range contains `site.position`, the answer must pass `lands_on_name`.
  Apply this to `REFERENCES` and use the same predicate for `CALLS`, in place
  of `node.id != site.from_id`.
- Example: `fn matched(..) -> Result<bool, S::Error>` inside
  `impl<'a, S: Sink> Sink for &'a mut S`: the answer is the impl's `S`, the
  landing is the impl node, and the impl contains the site. The impl node's
  name (`<&'a mut S as Sink>`) never occurs as a word, so the edge is
  dropped. #20's `Self` answer is at `DecimalFormatter`'s name, so it is
  kept.
- Consequence: drops 35 (19 + 16) and keeps #20. Semantic `REFERENCES` on
  ripgrep go from 204 to 169. `CALLS` see no change on either corpus:
  GM-593 counted 0 non-self `CALLS` that land on an encloser, and the 6
  recursive edges are at their names.
- Benefit: one rule for "the definition has no node, so it landed on what
  encloses the use". It holds for every edge kind, and it fixes the
  non-self-loop half too.
- Risk: (1) A true reference to an encloser that the server answers away
  from the encloser's name would be dropped. That needs the server to point
  inside a declaration's body for the declaration itself, which definition
  answers do not do. 0 on the corpora: the only kept self-reference is at a
  name. (2) `lands_on_name` misfires when a node's name first occurs in a
  doc comment inside its range (GM-593's known risk). That drops an edge; it
  never adds one. (3) A dropped answer leaves the structural placeholder
  edge where it is (R1, the `else` branch), so the measure counts may not
  fall by exactly 35 in total edges.

### C. Fix at the source, in the Rust plugin: emit no site whose first path segment is a generic parameter or `Self`

- Today: the tree-sitter tier emits a `REFERENCES` site for `S::Ok`.
- Change: skip a path whose head names a type parameter of an enclosing
  item.
- Consequence: Rust only. It misses #19 (nested enum) and any TS or Python
  equivalent. It also needs scope tracking in the plugin. Not recommended.

### D. Keep REFERENCES unfiltered and document it

- Consequence: 19 self-loops and 16 impl edges stay. `find_references
  serialize` lists `serialize` itself 9 times. Not recommended.

## 4. Decisions

- **D1. Scope of the rule for REFERENCES.** B (encloser, Recommended) / A
  (self only) / D (none).
  - Today: no filter.
  - Change: as described in section 3.
  - Example: `matched -> impl` is dropped by B and kept by A.
  - Consequence: B fixes 35 edges, A fixes 19. Both keep #20.
- **D2. Use the encloser predicate for CALLS too.** Yes (Recommended) / keep
  GM-593's `node.id != site.from_id` for `CALLS`.
  - Today: a `CALLS` landing on its own caller is held to the caller's name.
  - Change: a `CALLS` landing on any `Function` that contains the site is
    held to its name.
  - Example: a call in a closure node whose answer falls back to the outer
    function. Count on the corpora: 0.
  - Consequence: one predicate instead of two. The `CALLS` edge set does not
    change on the corpora. Risk: an untested path, covered by T5.
- **D3. The existing test `a_reference_landing_inside_its_own_encloser_is_
  recorded`** pins today's behaviour. Under A or B it is inverted (T1), not
  deleted.

## 5. Edit map (option B + D2)

- `plugins/sdk/src/lsp/bridge.rs` `record_answer` 2626-2800, comment and
  filter at 2692-2705:

  ```rust
  let encloses = |path: &RelPath, node: &WireNode| {
      node.id == site.from_id || (path == &question.file && node_contains(node, site.position))
  };
  agree && ((!encloses(path, node)
          && (site.edge_kind != EdgeKind::Calls || node.kind == NodeKind::Function))
      || lands_on_name(index, path, wire_position(..), node))
  ```

  Check it against today's filter. For `CALLS` the outcome is the same,
  except that a non-self encloser is also held to its name. For other kinds,
  a landing that does not enclose is recorded as today, and one that
  encloses must be at its name. Rewrite the comment: "a landing on the
  referrer or on anything containing the site is the encloser fallback for a
  declaration with no node (a nested fn or enum, a local, a generic
  parameter), unless it points at that node's own name (recursion, `Self`)".
  Per CLAUDE.md, the comment carries no ticket id; drop the existing
  "(GM-593)" suffixes in the touched comments while there.
- `plugins/sdk/src/index.rs` `contains` 198-203: make it `pub(crate)` and
  reuse it as `node_contains`. Or add `SdkIndex::encloses(path, node,
  position)`. The ranges are the same wire units as `site.position`.
- `bridge.rs` docs: `node_at` 2034-2039 and `lands_on_name` 2074-2081 say
  "a landing on its own caller (GM-593)". Change this to "a landing on the
  referrer or on a node containing the site, any edge kind" (no ticket id).
- `bridge.rs` test `a_reference_landing_inside_its_own_encloser_is_recorded`
  (4339-4350): invert and rename (T1).
- `docs/adr/0035-engine-calls-answers-kind-rule.md` D1 (lines 22-25) and
  Consequences (line 57): add a GM-614 line. `REFERENCES` landings on the
  referrer or an encloser are held to the name. Generic parameters and
  nested items no longer make edges onto their declaring fn, struct or impl.

Read, unchanged: `untyped_call_answered` (`CALLS` only, computed before the
filter), `overload_landing` (returns before the filter), and `named_landing`
(`CALLS` disagreement only). `site.position` and `question.file` are the
site's own file and wire position (`Question` built at 1100 with
`position: site.position`). [code]

## 6. Behaviours the tests must pin, with controls

The fixture is `landing_index` (3838). T3 and T5 need a site in `a.toy`
itself, because containment requires the same file. Build the `Question`
with `file: a.toy`, or add an `answered_in` helper. Add a nested pair to
`a.toy`, for example `impl<S> Sink {\n  fn m() -> S::E {}\n}` with a Type node
`<S as Sink>` for the impl and a Function node `m`, plus an `OpenSite`
REFERENCES at `S::E` with `from_id` set to `m`.

| # | Behaviour | Test (sketch) | Control (revert; the test must fail) |
|---|---|---|---|
| T1 | A REFERENCES landing inside its own referrer, away from its name, records nothing (#1-#19) | invert `a_reference_landing_inside_its_own_encloser_is_recorded`: from `outer`, answer `in_a(9, 17)` -> 0 edges | Restore `site.edge_kind != Calls ||` as a short-circuit for non-`CALLS`: 1 edge |
| T2 | A REFERENCES self-landing at the referrer's own name is kept (`Self`, #20) | from `outer`, answer `in_a(9, 3)` -> 1 edge | Drop the `lands_on_name` escape for enclosing landings: 0 edges |
| T3 | A REFERENCES landing on a non-self encloser (impl), away from its name, records nothing (the 16) | site in `m`, answer at the impl's `S` -> 0 edges | Option A (`node.id == site.from_id` in place of `encloses`): 1 edge |
| T4 | A REFERENCES landing inside a node that does not enclose the site is recorded as today | from `caller` (b.toy), answer `in_a(9, 17)` -> 1 edge on `outer` | Hold every REFERENCES landing to its name (drop the `!encloses` arm): 0 edges |
| T5 | (D2) A CALLS landing on a non-self enclosing Function, away from its name, records nothing; GM-593's tests stay green | `CALLS` site in `m`, answer inside `m`'s encloser function -> 0 edges | Restore `node.id != site.from_id` for `CALLS`: 1 edge |

That is 5 reverts in one test binary (`-p g-mesh-plugin-sdk --lib
lsp::bridge`).

## 7. Measure slice

Reindex ripgrep and excalidraw at base and after, using GM-593/S9's method.
Expect on ripgrep:

- semantic `REFERENCES` self-loops: 20 -> 1 (`DecimalFormatter`)
- semantic `REFERENCES` onto an `impl`: 16 -> 0
- semantic `CALLS`: unchanged (6 self-loops)

Expect excalidraw unchanged. Cheap observation: `find_references serialize`
in `jsont.rs` no longer lists `serialize`.

## 8. How this was found

- Index: `sqlite3` on `gm593-s9/index-ripgrep-after.db`. Edges `source =
  'semantic' AND fromId = toId AND kind = 'REFERENCES'`, joined with
  `linkedFrom` -> `pending_symbol` node for the site. Site text was read with
  `sed` on the clean corpus checkout.
- Code: `bridge.rs` `record_answer`, `node_at`, `lands_on_name`,
  `is_addressable`, and the GM-593 tests; `index.rs` `node_at`/`contains`.
  Read by symbol with `grep -n` and `sed -n`.
- g-mesh: not used. Every code question was about one known file (bridge.rs
  or index.rs), and the worktree is not indexed.
