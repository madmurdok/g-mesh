# 0003. MCP prompt text: instructions rendered from the index's languages, terse tool schemas

## Status
Accepted

## Context
`get_info`'s `with_instructions` string (`core/src/mcp/instructions.rs`)
tells an agent when a g-mesh answer is exhaustive and when to fall back to
grep. The one standing gap is a method call through a variable receiver
(`x.foo()`), and whether it is a gap depends on the language: TypeScript
never emits that edge; Go (`go/types`), Rust (`rust-analyzer`) and Python
(`pyright`) emit it only once their semantic pass has completed
project-wide (`language_state.semanticPassAt`). A fixed sentence can be true
for at most one of those.

Constraints:
- Claude Code truncates `with_instructions` at 2KB. The working ceiling is
  1,900 bytes (`INSTRUCTIONS_BYTE_CEILING`), leaving margin under the hard
  cut.
- Core must not hardcode per-language names or syntax: every language fact
  comes from plugin manifests (`[plugin.capabilities]`) and the index.
- `get_info` is read once per MCP session, during `initialize`.

## Decision
1. **We render the receiver-call paragraph from what is indexed**, in four
   shapes, cheapest first: the generic sentence (nothing known, or exactly
   one language); a narrowed sentence when every present language resolves
   receiver calls; a sentence naming exactly the gapped languages when two or
   more are present; and a generic fallback with "check which" when the
   named list would exceed the ceiling. Only the named shape grows with the
   number of languages, which is why the fallback exists.
2. **A single language is never named.** It cannot be ambiguous, so naming
   it only spends bytes; a TypeScript-only project renders the same text as
   an empty index. Once two languages are present, every gapped language is
   named, even when that is all of them, so the phrasing does not change
   meaning silently between projects.
3. **Language names are manifest ids** (`"typescript"`, `"go"`), not a
   display-name table: the bytes are the same, a table would be a language
   list hardcoded in core, it needs no `plugin.toml` field, and every other
   tool surface spells a language the same way.
4. **When every language resolves receiver calls, the clause narrows rather
   than drops.** A semantic tier binds `x.foo()` to the receiver's declared
   or inferred type, so an override's own caller page silently loses calls
   that went through a base or interface. Measured through the real MCP
   handlers on the Go, Rust and Python fixtures, all three tiers agree: e.g.
   on the Go fixture `find_callers("Conn.Close")` returns `results: []`,
   `hasMore: false`, `allUnresolved: false`, while `find_implementations`
   names `Conn` as a `Closer`. The sentence points at `find_implementations`
   and **never states a count**: the index can count implementors, but not
   the missing call sites, and which override runs is a run-time fact (the
   same rule as `mcp::provenance`: a manufactured number a caller acts on is
   worse than silence).
5. **The disclosure lives in the instructions, not on responses.** A per-row
   marker cannot describe rows absent from the page (the 240-byte empty
   answer above has none) and costs 34.8x a response-level block on the
   51-row worst case. A per-response field would fire on every go/rust/python
   answer ("a disclosure that fires everywhere is noise"), and firing it only
   for overriding members would need core to parse `qualifiedName` in four
   grammars, since the graph has no member-to-type or `OVERRIDES` edge. What
   remains is a property of the tier, constant per language and session,
   which is what the instructions render. The named and generic shapes keep
   their existing warning; restating the narrowing once per named language
   is kept out for complexity, not budget.
6. **The fallback names no response field.** The candidate pointers (a
   `get_file_outline` `language` field, a per-row `receiverCallsResolved`)
   do not exist yet; pointing at a missing field is worse than the generic
   sentence. It also drops the "bare calls have no such gap" reassurance,
   which buys less than it costs once languages are no longer named. It is
   kept well under the named sentence (413 vs 526 bytes for today's eight
   languages) as headroom for the language that first makes the named list
   not fit.
7. **Staleness within a session is accepted.** `semanticPassAt` only moves
   from unset to set, so the only drift is a session still naming a language
   as gapped after its pass landed: the conservative direction (an
   unnecessary grep, never a missing edge the caller was promised). A
   per-response field read fresh would remove it and is future work.
8. **Tool parameter docs are terse.** Every `///` on a parameter struct in
   `core/src/mcp/mod.rs` is compiled by `schemars` into the tool's JSON
   Schema, and the whole `tools/list` response sits in the model's cached
   prompt prefix, re-read (and billed as `cacheReadTokens`) on every
   request. Measured against serena's 5-tool surface as a reference, the
   eight schemas were 11,722 bytes, 62% of it description text, with
   `file_paths`/`symbol_name`/`limit`/`symbol_id`/`cursor` repeated across
   three to six tools; compressing the prose without dropping a fact a
   caller acts on took the surface to 9,845 bytes, about 600 fewer prompt
   tokens per request. So schema docs state defaults, caps, mutual
   exclusions and the ambiguity protocol; rationale goes in `//` comments,
   and cross-tool guidance in the instructions (sent once per session).

The cold-start line and the multi-project front's text follow D12 of
[`lazy-indexing.md`](../architecture/lazy-indexing.md#d12-instructions-text-and-the-byte-budget):
each has a no-path fallback so the root path can never be what breaks the
ceiling.

## Consequences
- Adding a language needs no change in `instructions.rs`: its manifest
  capabilities decide which shape it lands in.
- The worst-case test (every bundled and planned language present and
  gapped) and `build`'s fallback share one constant, so the ceiling cannot
  drift between them.
- A future per-response language field would let the fallback point at it
  and would remove the per-session staleness.
