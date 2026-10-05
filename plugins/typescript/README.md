# g-mesh's TypeScript plugin

A TypeScript/JavaScript plugin with a structural tier (extractor) and, since
GM-325, a semantic tier driven by vtsls 0.3.0 over the SDK's generic
`LspBridge`. `scripts/test-deps.sh typescript` installs vtsls (pinned in
`package.json`). The semantic tier needs Node. This file lists what it does
not see; the measurements are in `docs/results/gm-325-ts-semantic-gaps.md`.

## What it does not see

These are gaps, not bugs. Counts are from the conformance fixture and from
excalidraw (655 TS/JS files) without and with `node_modules`.

1. **No server installed.** Answers are structural only: receiver calls are
   listed in `untypedReceiverCalls` and `provenance.semanticTier` is
   `"absent"`. On excalidraw that is 6,914 untyped-call rows and 0 semantic
   edges; 7 of the fixture's 36 entries fail.
2. **Uninstalled workspace package.** The answer is the import binding, so the
   structural edge stands and nothing more.
3. **Definition outside the index** (`lib.d.ts`, `node_modules`): no edge.
   6,817 receiver calls without deps; 14,024 receiver calls, 159 references
   and 16 overload calls with them.
4. **Receiver typed `any`, or untyped JS.** tsserver answers empty:
   8,577 receiver calls (37%) without deps, 655 (3%) with.
5. **Ambiguous answer** (more than one location): not guessed. 682 without
   deps, 1,365 with.
6. **Computed members** (`obj[k]()`): the extractor records no open site, so
   they never reach the bridge.

Two costs: a cold vtsls spends 13-33 s loading projects on a project of this
size, and without `node_modules` tsserver's automatic type acquisition
downloads `@types/*` into a machine-wide cache during the pass.

## Cold start

The first question gets a 120 s warm-up budget, the rest 10 s. In 6 of 6
cold passes (with and without `node_modules`) there were 0 timeouts and the
pass was recorded complete; the first answer arrived after 13-38 s. Before the
warm-up, a cold pass timed out 8-32 questions and was retried at every daemon
start.
