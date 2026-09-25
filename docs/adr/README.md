# Architecture Decision Records

This directory is the index and home for g-mesh's architecture decisions.

## Template

New decisions get their own file: `docs/adr/NNNN-<slug>.md` (four-digit,
zero-padded, incrementing — the next free number is `0002`). Use this
template:

```markdown
# NNNN. <Title>

## Status
Proposed | Accepted | Superseded by NNNN | Deprecated

## Context
What problem forced a decision. Constraints, forces, prior attempts.

## Decision
What was chosen, stated as a decision ("We will ..."), not a discussion.

## Consequences
What this makes easier or harder, going forward. Follow-on work it implies.
```

Keep it short — a screen or two. Long design exploration (options
considered, data flow, failure modes) belongs in `docs/architecture/`, not
in the ADR; the ADR records the decision itself and links out to the design
doc for the reasoning behind it.

## Index

### Existing design docs (`docs/architecture/`)

The seven docs below predate this directory and are indexed here, not
rewritten: one row per doc, plus one row per numbered decision for the docs
that already number their decisions individually. Everything else in them
(options considered, data flow, failure modes, measurement plans) stays in
place — read the doc itself for that detail.

| # | Decision | Doc |
|---|----------|-----|
| — | g-mesh v1: overall architecture (index/query pipeline, data model, MCP interface) | [`g-mesh-v1.md`](../architecture/g-mesh-v1.md#g-mesh-v1--architecture) |
| — | Multi-language plugins: Go and Rust first, plugin protocol and SDK, without paying again for language N+1 | [`multi-language-plugins.md`](../architecture/multi-language-plugins.md#multi-language-plugins-go-and-rust-first-without-paying-again-for-language-n1) |
| — | Modular multi-language plugin system (plugin boundary, components, data model) | [`plugin-modularity.md`](../architecture/plugin-modularity.md#modular-multi-language-plugin-system) |
| — | Plugin lifetime: plugins die with their daemon (GM-397) | [`plugin-lifetime.md`](../architecture/plugin-lifetime.md#plugin-lifetime-plugins-die-with-their-daemon-gm-397) |
| — | Symbol resolution: a ladder inside the tool, not rules outside it | [`symbol-resolution-ladder.md`](../architecture/symbol-resolution-ladder.md#symbol-resolution-a-ladder-inside-the-tool-not-rules-outside-it) |
| — | Pushed context: an Aider-style repo map for g-mesh | [`pushed-context-repo-map.md`](../architecture/pushed-context-repo-map.md#pushed-context-an-aider-style-repo-map-for-g-mesh) |
| — | Lazy indexing (GM-395): overall doc — see D1-D14 below for the individual decisions | [`lazy-indexing.md`](../architecture/lazy-indexing.md#lazy-indexing-gm-395) |
| D1 | Where laziness lives: the daemon starts idle; the shim is unchanged in when it spawns | [`lazy-indexing.md#d1`](../architecture/lazy-indexing.md#d1-where-laziness-lives-the-daemon-starts-idle-the-shim-is-unchanged-in-when-it-spawns) |
| D2 | Activation: any index-needing tool call triggers it; it runs once, independently of the caller; failure is reported, not fatal | [`lazy-indexing.md#d2`](../architecture/lazy-indexing.md#d2-activation-any-index-needing-tool-call-triggers-it-it-runs-once-independently-of-the-caller-failure-is-reported-not-fatal) |
| D3 | Two-phase readiness; embeddings leave the walk and become a separate pass | [`lazy-indexing.md#d3`](../architecture/lazy-indexing.md#d3-two-phase-readiness-embeddings-leave-the-walk-and-become-a-separate-pass) |
| D4 | `search_code` while embeddings are running: it waits (with progress and the D7 cap) | [`lazy-indexing.md#d4`](../architecture/lazy-indexing.md#d4-search_code-while-embeddings-are-running-it-waits-with-progress-and-the-d7-cap) |
| D5 | When the embedding pass starts: right after the structural phase and the semantic pass, as part of the same activation | [`lazy-indexing.md#d5`](../architecture/lazy-indexing.md#d5-when-the-embedding-pass-starts-right-after-the-structural-phase-and-the-semantic-pass-as-part-of-the-same-activation) |
| D6 | Progress notifications: a heartbeat while a call waits, only when the request carries a token | [`lazy-indexing.md#d6`](../architecture/lazy-indexing.md#d6-progress-notifications-a-heartbeat-while-a-call-waits-only-when-the-request-carries-a-token) |
| D7 | Wait cap and the "still indexing" answer | [`lazy-indexing.md#d7`](../architecture/lazy-indexing.md#d7-wait-cap-and-the-still-indexing-answer) |
| D8 | The watcher | [`lazy-indexing.md#d8`](../architecture/lazy-indexing.md#d8-the-watcher) |
| D9 | Reusing an existing index | [`lazy-indexing.md#d9`](../architecture/lazy-indexing.md#d9-reusing-an-existing-index) |
| D10 | Multi-project detection (cheap, bounded, marker-based) | [`lazy-indexing.md#d10`](../architecture/lazy-indexing.md#d10-multi-project-detection-cheap-bounded-marker-based) |
| D11 | Multi-project roots: a front daemon plus a session switch in the shim | [`lazy-indexing.md#d11`](../architecture/lazy-indexing.md#d11-multi-project-roots-a-front-daemon-plus-a-session-switch-in-the-shim) |
| D12 | Instructions text and the byte budget | [`lazy-indexing.md#d12`](../architecture/lazy-indexing.md#d12-instructions-text-and-the-byte-budget) |
| D13 | CLI and other paths | [`lazy-indexing.md#d13`](../architecture/lazy-indexing.md#d13-cli-and-other-paths) |
| D14 | Test-suite migration | [`lazy-indexing.md#d14`](../architecture/lazy-indexing.md#d14-test-suite-migration) |

### New ADRs (`docs/adr/NNNN-<slug>.md`)

Each decision made from here on gets its own file, added as a row here.

| # | Title | Status |
|---|-------|--------|
| 0001 | [IndexStore: one owner for the SQLite connection and its lock policy](0001-index-store.md) | Accepted |
