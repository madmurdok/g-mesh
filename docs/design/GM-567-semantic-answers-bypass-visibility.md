# GM-567: semantic answers bypass the linker's visibility check

Design note for GM-567 (GM-519 owner decision D1 "B: семантика мимо видимости",
D3 "Через B (если B в D1)"). No production code changes here.

Claim markers: **[run]** verified by running a command for this note;
**[code]** read from the code at `release-4.8.0` (`03c5164`); **[GM-519]**
taken from `docs/results/GM-519-member-visibility-findings.md` and not re-run.
Line numbers are 1-based, in `core/src/graph/symbol_links.rs` unless stated.

## 1. Where the check runs today

- **The rule.** `Resolver::visible` (1975-1988) admits `public` always,
  `container(c)` through `Resolver::sees` (1993-2012, requester's container is
  `c` or has it on its parent chain), and `file` only when the scope is a
  `Scope::Container` and the candidate is in `requester.file`. [code]
- **Its two callers** (g-mesh `find_callers Resolver::visible`, complete, no
  `hasMore`): `Resolver::walk_capped` (1511-1581, the candidate loop at
  1528-1534, every depth of every walk) and `Resolver::through_head`
  (1695-1727, the member loop at 1720-1725). [code + g-mesh]
- **Re-export hops** have their own restriction, `Hop::restricted_to`, checked
  with `sees` in `binding_hops` (1588-1622). That is not `visible` and this
  task leaves it alone. [code]
- **Who triggers it.** `link` (1238-1332) calls `resolver.resolve(&placeholder)`
  once per placeholder (1282) and then decides each edge kind (1284-1327).
  `resolve` (1480-1489) is called only from `link` (g-mesh `find_callers`).
  [code + g-mesh]

### How placeholders and answers carry their source

- **A placeholder carries no source.** `Placeholder` (505-512) is `id`,
  `scope`, `key`, `key_path`, `requester`; `PLACEHOLDER_SELECT` (518-519) reads
  only `nodes` + `placeholder_targets`. [code]
- **The source is on the edge.** `edges.source` is `'syntactic' | 'semantic'`
  with a CHECK constraint (`core/src/storage/schema.rs` 306-317), plus
  `edges.engine`. Legacy labels map in `storage/write.rs`
  `normalize_legacy_source` (`ts-compiler` -> semantic, 265). [code]
- **Placeholders are shared between tiers.** The placeholder id is derived
  from its address, so a structural and a semantic edge with the same address
  hang on one placeholder: 26 (TS), 9 (Go), 28 (Rust) such placeholders on the
  corpora. [GM-519]
- **Semantic answers are `qualifiedName`-keyed by construction**: the SDK
  bridge's `address_of` (`plugins/sdk/src/lsp/bridge.rs` 1813-1825) always
  sends `TargetKey::QualifiedName` at the declaration's container, else its
  file; the Go plugin's `answerImplements` (`plugins/go/semantic.go` 959-1024)
  addresses `container(<pkg path>)` by dotted path. The TS semantic pass also
  sends some `name`-keyed semantic edges (38 on excalidraw, re-asked imports
  through barrels). [code, GM-519 for the 38]
- **Structural `qualifiedName` keys exist** (Rust plugin `bodies.rs`/`emit.rs`
  for `T::m` paths; the head walk of GM-472). So "key is `qualifiedName`" alone
  does not identify an engine answer; the edge's `source` does. [code]

## 2. The split: per edge source, not per placeholder

Because one placeholder can carry both a structural and a semantic edge of the
same kind, the decision moves from "per (placeholder, edge kind)" to "per
(placeholder, edge kind, edge source)".

1. `pending_kinds` (1251-1256) reads `kind, source` instead of `kind`.
2. Each `(kind, source)` group resolves under a requester that says whether
   the visibility check applies:
   `trusted = source == 'semantic' && key is QualifiedName && kind in ENGINE_ANSWER_EDGE_KINDS`
   (decisions D1 and D2 below fix the last two conjuncts; recommended values
   shown).
3. `Requester` (496-500) gains `engine_answer: bool` (false from
   `placeholder_from_row`, 530-562). It is part of the `provides` cache key
   (`Resolver::provides`, 1671-1680) automatically since `Requester` derives
   `Hash`; `provides` only ever walks `name` keys, so the flag never changes
   its answer, only splits a cache entry.
4. One admission helper replaces the two direct `visible` calls:
   `admits(candidate, scope, key, requester) = (requester.engine_answer && key is QualifiedName) || visible(..)`.
   In `walk_capped` a `qualifiedName` key never hops, so only the depth-0 lookup
   is affected; in `through_head` the **head** is walked under a `name` key and
   stays checked (D3), while the **member** lookup (a `qualifiedName` key) is
   admitted.
5. `link` resolves lazily: at most two candidate lists per placeholder
   (checked, trusted), computed only for the groups present.
6. `repoint` (1257-1262) and `unlink` (1263-1265) get `AND source = ?` so a
   group moves only its own edges. Without it, the semantic group's repoint
   would carry the structural edge along (or the structural group's unlink
   would pull the semantic one back), whichever runs last.

Where the rule is expressed: a const `ENGINE_ANSWER_EDGE_KINDS` beside
`LINKABLE_EDGE_KINDS` (339-344). Per the project's comment rule (CLAUDE.md),
its doc states only the invariant; the decision (B over C, the `SUPERTYPE_OF`
exclusion, the `qualifiedName`-only scope) goes to a new ADR, linked once from
the module doc's "Visibility" section.

### link_diff must wake what the bypass can now answer

`seeds`/`published_scopes` (780-814) publish a `file`-visible declaration under
its container only, because no file-scoped lookup ever accepted it. With the
bypass, a semantic `qualifiedName` placeholder in a file scope can now be
answered by one (every TS non-exported declaration). Without a new seed,
`link_diff` misses the case where the declaration arrives after the
placeholder, and `link_all`/`link_diff` disagree (a missing edge, never a
wrong one). Change: in `seeds`, a `file`-visible declaration also pushes the
exact seed `(Scope::File(file_path), qualified_name)` (only to `exact`, never to
`named`: it is not republished by re-exports). `waiting_placeholders` matches
`t.key` regardless of key kind, so this wakes the placeholder; a structural
`name` placeholder woken by it is re-decided to the same answer
(over-inclusive, as every trigger is). `heads_of_members` (824-880) needs no
change: the head stays checked. [code]

## 3. Composition with GM-568 (C1)

GM-568 lands first in the same file: for `name` keys it drops type members
before the kind rule (today's `fitting` step, 1290-1299, and/or
`sole_non_member`, 1742-1757). GM-567 changes which candidate list feeds that
step (per source group) and which candidates `walk_capped`/`through_head`
admit. The predicates are disjoint: C1 acts only on `name` keys, the bypass
only on `qualifiedName` keys, so for any one placeholder at most one of them
applies and the order of application does not matter. The only overlap is
textual (the body of `link`'s per-kind loop). Code slice: rebase onto GM-568,
extract the post-candidate decision ("kind filter, C1 member drop, exactly
one, tie-breaks") into one helper
`fn choose(&mut self, key, edge_kind, candidates) -> Result<Option<String>>`
that holds GM-568's filter unchanged, and call it once per `(kind, source)`
group.

## 4. The gin SUPERTYPE_OF case

- **What it is.** `SUPERTYPE_OF errorMsgs -> binding.testInterface`: an
  unexported interface declared in a `_test.go` file of package `binding`;
  `errorMsgs` is in package `gin`. Refused today by the `container` rule. [GM-519]
- **Where it comes from.** It is not an answer to a written use. The Go
  plugin's `answerImplements` pairs every project named type with every
  project interface (`packages.Config{Tests: true}`, `semantic.go` 340) and
  emits an edge wherever `types.Implements` holds (`semantic.go` 959-1024).
  Satisfaction is structural, so the pair is true in the type system, but no
  code in `gin` can name `binding.testInterface`. [code]
- **Contrast.** A semantic `CALLS`/`REFERENCES` edge is the engine resolving a
  written name in code that compiles, which is the case where "the engine
  already resolved it" holds. The Go `SUPERTYPE_OF` is a computed relation for
  which visibility was the only filter against cross-package noise: with the
  bypass, every unexported interface in any package would collect implementers
  from every package.
- **What it costs to keep the check on SUPERTYPE_OF: nothing measured.** All
  62 excalidraw visibility refusals are `CALLS`; the only `SUPERTYPE_OF`
  visibility refusal on excalidraw, gin and ripgrep is this gin edge; ripgrep
  has none. [run: `GM-519-count-refusals.sql` CTEs grouped by
  `edge_kind`/`outcome` over the GM-519 S2 indexes
  `~/.g-mesh/projects/{39fba07a57afbba6,fab171c84b1fcefb,2e37c14f1bb87919}/index.db`]
- **Recommendation:** keep the visibility check for semantic `SUPERTYPE_OF`
  (D2, option "CALLS and REFERENCES only"). The gin edge stays refused, which
  GM-519 judged correct.

## 5. Fixtures

| fixture | today [GM-519] | after (recommended) | why [code] |
|---|---|---|---|
| t03 nonexported class via instance | missing `boot -> ServiceImpl#start` | links | vtsls answer, `qualifiedName` at `File(src/svc.ts)`, semantic `CALLS`; member is `file`; admitted |
| t04 same-file local class | missing `tally -> Counter#bump` | links | as t03, scope is the requester's own file; exact key finds one node, so the module doc's self-import worry (a `name` key concern) does not arise |
| t06 module augmentation | missing `show -> glow` | links | answer addressed at `src/plugin.ts`, augmenting interface is `file`; admitted |
| t12 exported alias of local class | missing `go -> Engine#ignite` | links | as t03 |
| r01 macro_rules sibling module | missing `user::doubled -> twice` | links | rust-analyzer answer, `qualifiedName` at `Container(crate::macros)`, `container(crate::macros)` not on `crate::user`'s chain; admitted. The kind fits (GM-519 classified the refusal as `container`, not kind) |
| t01c control (structural) | correct, no link | unchanged | structural edge, still checked |
| t01, p01 | wrong link | fixed by GM-568, not here | `name` key, structural |
| t02 | missing (kind rule) | unchanged | kind rule, not visibility |

Expected, not run for this note: the fixtures need the semantic tier, and the
before state is GM-519's run with 4.6.0. The verify slice runs
`docs/results/GM-519-fixtures/run-all.sh` with the branch build.

## 6. Risks: wrong links a semantic answer can now create

1. **Engine errors are no longer second-checked** for non-visible targets: a
   wrong `definition` answer onto a private symbol now links. Bounded: the
   bypass needs an exact `qualifiedName`, a kind that fits, and exactly one
   candidate.
2. **Code that does not compile**: a TS `private` member used from another
   file, or a Rust private item reached illegally, links if the engine answers
   it. The edge describes what the code says; acceptable.
3. **Go SUPERTYPE_OF noise** (section 4): avoided only if D2 excludes
   `SUPERTYPE_OF`.
4. **Structural edges riding along** on shared placeholders, if the split is
   per placeholder instead of per edge, or if `repoint`/`unlink` are not
   filtered by source. Pinned by tests B3/B4.
5. **New ambiguity, a missing edge where one linked**: a scope declaring one
   `qualifiedName` twice with different visibility (a visible and a
   non-visible node) used to yield one candidate and now yields two. Not seen
   in GM-519; the measure slice watches for a drop in semantic links.
6. **`name`-keyed semantic edges** (38 on excalidraw) would reopen the
   module doc's self-import hole (non-exported `x` in the importer's own file
   becoming a candidate through a barrel round trip) if D1 extends the bypass
   to them.

## 7. Owner decisions

### D1. Which semantic edges skip the check: exact answers only, or every semantic edge?

- **Today:** every edge, structural or semantic, has its candidates filtered by
  visibility. A semantic edge is either an exact answer (`qualifiedName` key:
  the engine named the declaration) or a `name` key (the engine only said which
  file to look in; the linker's own name walk, through barrels, picks the
  node). Excalidraw has 2147 of the first and 38 of the second.
- **Change:** skip `visible` for semantic edges with a `qualifiedName` key only
  (Recommended), or for every semantic edge.
- **Example:** `ns.render()` where `ns` is a barrel that re-exports the
  importer's own file, and that file has `export const render` plus a class
  method `render`. With a `name` key, skipping visibility makes the
  non-exported method a candidate too: an ambiguity (missing edge) or, under
  `CALLS`, a link onto the method (wrong edge). With a `qualifiedName` key the
  engine already named one node, so no second candidate appears.
- **Consequence:** `qualifiedName` only: fixes all five fixtures and the 62
  edges, keeps the self-import guard; risk: a future engine answer sent with a
  `name` key stays checked. Every semantic edge: no extra fix measured; reopens
  the self-import hole.

### D2. Does the bypass apply to SUPERTYPE_OF?

- **Today:** gin's `errorMsgs -> binding.testInterface` (an unexported
  interface in another package's test file) is refused by the `container`
  rule.
- **Change:** bypass only for `CALLS` and `REFERENCES` (Recommended), or for
  every edge kind, or every kind plus a Go plugin task to stop emitting
  `SUPERTYPE_OF` onto interfaces the type's package cannot name.
- **Example:** Go's `SUPERTYPE_OF` is computed by pairing all types with all
  interfaces, not by resolving written code. Every unexported interface (say a
  package-local `type writer interface{ Write(...) }`) is satisfied by many
  types in other packages.
- **Consequence:** `CALLS`/`REFERENCES` only: 0 corpus edges lost (all 62
  refusals are `CALLS`, verified), gin's edge stays refused; risk: a future TS
  or Rust `implements`/trait-impl answer onto a non-visible type stays
  refused. Every kind: gin gains one edge and any Go corpus with unexported
  interfaces gains cross-package implementers (`find_implementations` noise).
  Every kind + Go task: clean, but needs a second change in another plugin.

### D3. Is the head of a re-exported head walk (GM-472) also exempt?

- **Today:** `use crate::named::T; T::m()` is answered as `named::T::m` at
  `named`, which only re-exports `T`; the linker walks `T` by name through the
  re-exports, checking visibility, then looks `m` up beside it.
- **Change:** exempt only the member lookup, keep the head walk checked
  (Recommended), or exempt both.
- **Example:** two `T`s reachable from `named`, one private to a sibling
  module. Checked: the private one is not followed, the visible one links.
  Exempt: both are candidates, ambiguity, no edge.
- **Consequence:** member only: the head walk is the linker's own name
  resolution, so it keeps the same rules as any `name` walk; one extra
  condition in the code. Both: simpler rule, risk of new ambiguity on head
  walks.

## 8. Edit map for the code slice (after rebasing on GM-568)

`core/src/graph/symbol_links.rs`:

| what | where | change |
|---|---|---|
| module doc, contract step 2 | 53-56 | the visibility sentence: structural keys always checked; semantic exact answers of `CALLS`/`REFERENCES` admitted |
| module doc, "Visibility" | 72-108 | add the engine-answer exemption as a present-tense rule, with one link to the new ADR |
| module doc, "Placeholders the semantic pass sends" | 225-238 | "Nothing here treats one differently" is no longer true: the edge source now matters |
| `LINKABLE_EDGE_KINDS` | 339-344 | add `ENGINE_ANSWER_EDGE_KINDS: [&str; 2] = ["CALLS", "REFERENCES"]` beside it (doc: invariant only) |
| `Requester` | 493-501 | add `engine_answer: bool` + doc |
| `placeholder_from_row` | 530-562 | set `engine_answer: false` |
| `seeds` (+ its doc) | 768-805 | `file`-visible declaration: also an exact seed at `Scope::File(file_path)` |
| `link` | 1238-1332 | `pending_kinds` -> `kind, source`; per `(kind, source)` group pick the requester; lazy checked/trusted candidate lists; `repoint`/`unlink` filtered by source; call GM-568's extracted `choose` |
| `Resolver::resolve` | 1470-1489 | take `requester: &Requester` (or the flag) and pass it on |
| `Resolver::walk_capped` | 1528-1534 | `visible` -> `admits(candidate, scope, key, requester)` |
| `Resolver::through_head` | 1695-1727 | head walk with `engine_answer: false`; member loop (1720-1725) via `admits` |
| new `Resolver::admits` | next to `visible`, 1975 | `(requester.engine_answer && matches!(key, Key::QualifiedName(_))) \|\| self.visible(..)` |

New ADR `docs/adr/0034-semantic-answers-bypass-visibility.md` (next free
number at `release-4.8.0`; re-check after rebasing) plus its line in
`docs/adr/README.md`: the decision, options B/C considered, D1-D3 as decided.

`docs/architecture/multi-language-plugins.md` 693-704 (linker contract,
"Visibility"): the line "A `qualifiedName` key is checked too, so a semantic
tier's mistake cannot link a private symbol from outside" becomes the
engine-answer exemption.

`core/src/graph/symbol_links/tests.rs`: the doc of
`a_go_unexported_symbol_links_within_its_package_and_not_from_outside` (1469-1471)
says the exact-key refusal "stops a semantic tier's mistake"; its edge is
structural (`usage_edge` uses `tree-sitter`), so the assertion holds but the
comment must say "a structural exact key".

**Read for context, do not change:** `visible` (1975-1988), `sees` (1993-2012),
`binding_hops` (1588-1622), `provides` (1671-1680), `heads_of_members`
(824-880), `waiting_placeholders` (927-986), `sole_non_member` (1742-1757),
`sole_accessor_getter` (2033-2053), `storage/write.rs`
`normalize_legacy_source`, tests helpers `usage_edge_from` (tests.rs 239-251),
`use_through` (1192-1205), `container_placeholder`, `member`.

**Callers relied on** (all g-mesh, project `g-mesh-wt-gm567`, complete
answers): `find_callers Resolver::visible` -> `walk_capped`, `through_head`;
`find_callers Resolver::resolve` -> `link`; `find_callers
Resolver::walk_capped` -> `walk`, `provides`; `find_callers published_scopes`
-> `seeds`, `heads_of_members`; `find_references Requester` -> 10 refs, all in
`symbol_links.rs` (no test constructs it); `find_callers link_diff` ->
`storage/index_store.rs` and tests. No signature outside this file changes.

## 9. Behaviours for the tests slice, each with a candidate control

Semantic edges in tests: `usage_edge_from(.., "ts-compiler")` (maps to
`semantic`).

| # | behaviour (test) | control (revert in code) |
|---|---|---|
| B1 | a semantic `qualifiedName` `CALLS` onto a TS `file`-visible member in another file links (t03 shape); same-file (t04) and Rust `container` from a sibling module (r01) as further assertions | `admits` returns `self.visible(..)` only |
| B2 | a structural `qualifiedName` edge onto the same target stays unlinked (existing Go `outside_exact` plus a TS one) | `link` sets `engine_answer` for every source |
| B3 | one placeholder carrying a structural and a semantic `CALLS` edge: the semantic one links, the structural one stays on the placeholder, after `link_all` and again after a second pass | drop `AND source = ?` from `repoint` and `unlink` |
| B4 | a semantic `SUPERTYPE_OF` onto a Go unexported interface of another package stays unlinked (gin shape) | add `SUPERTYPE_OF` to `ENGINE_ANSWER_EDGE_KINDS` |
| B5 | a semantic `name`-keyed edge keeps the self-import guard (a `ts-compiler` variant of `a_self_addressed_placeholder_sees_only_what_its_file_exports`) | drop the `Key::QualifiedName` condition in `admits` |
| B6 | through a re-exported head: a semantic exact answer reaches a non-visible member, while a private head is still not followed | head walk in `through_head` passes the trusted requester |
| B7 | `link_diff`: the `file`-visible declaration arriving after the semantic placeholder links it; add the case to `link_all_and_link_diff_agree_on_the_same_end_state` (tests.rs 1992) | remove the new exact seed in `seeds` |

Seven controls, one per behaviour. B1 and B2 revert different lines of one
function; B3-B7 can share one control build group (all in the `core` lib test
binary).

## 10. Measure slice plan

- **Arms:** before = `release-4.8.0` build (with GM-568 if it has merged into
  the release branch by then, so only GM-567 differs); after = this branch.
  Same corpora checkouts (`$TMPDIR/gmesh-bench-corpora/{excalidraw,gin,ripgrep}`,
  commits as in `GM-519-refusal-counts.md`), `g-mesh reindex` per arm with the
  semantic pass `completed: true`. Record `uptime` and `/usr/bin/time -p`
  `real/user/sys` per reindex.
- **Counts per corpus and arm:** `GM-519-count-refusals.sql` (outcome by
  language/source), the same CTEs grouped by `edge_kind` (the query used for
  section 4), semantic linked edges (`linkedFrom` set, `source = 'semantic'`),
  syntactic linked edges, total edges.
- **Expected:** excalidraw `refused_visibility_file` 62 -> 0 and semantic
  linked +62 (2021 -> 2083 at the GM-519 commit); gin
  `refused_visibility_container` 1 -> 1 (D2 recommended); ripgrep unchanged
  (no visibility refusals); syntactic linked identical in both arms on all
  three (the structural rules did not move).
- **Cheap observation where the arms must differ:** on excalidraw, the edge
  from `App#clearSelection`'s same-file caller (one of the 4 "through an
  exported value" refusals): `find_callers App#clearSelection` (or the SQL row
  for that edge's `toId`) is empty / on its placeholder before, and names the
  caller after. If it is identical in both arms, the measurement measured
  nothing (likely the wrong binary).
- **Watch for:** any drop in semantic linked edges (risk 5, new ambiguity), and
  any `SUPERTYPE_OF` change on gin (would mean D2 was not applied).
