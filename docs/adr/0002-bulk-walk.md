# 0002. Bulk walk: one one-shot process per language, and one failure fails the walk

## Status
Accepted

## Context
A never-indexed project needs a populated graph before its daemon answers
anything. The file watcher cannot provide it: `notify` reports only changes
made while it runs and synthesizes nothing for a tree that already exists.
So core walks the whole project once (`core/src/daemon/bulk_index.rs`).

Two choices shape that walk:

1. **How the plugin is asked.** The interactive path already has a
   long-lived plugin process behind a control-plane pipe
   (`daemon::plugin::PluginProcess`, supervised by
   `daemon::registry::PluginRegistry`, which spawns lazily on first touch).
   A whole-project walk, though, is an open-ended stream of nodes and edges,
   not one request/response frame.
2. **What a failing language does to the walk.** Every *discovered* plugin
   is walked, whether or not the project contains a file of its language;
   core does not pre-scan file extensions. So a checkout with an unbuilt
   plugin (e.g. `cargo build -p g-mesh` without the sibling plugin
   binaries) fails that language even for a project with none of its files.

## Decision
1. We spawn each discovered language's plugin a second time, in its one-shot
   `--bulk-index` mode, one process per language, walked one after another
   in language order. Its stdout is a self-contained, EOF-terminated NDJSON
   stream that `protocol::ndjson::NdjsonReader` consumes directly: nothing
   is interleaved with `FileChanged` traffic, and no end-of-bulk marker is
   needed. The spawn uses exactly each manifest's `command`/`args`, the same
   ones the interactive path spawns. `bulk_index::run` therefore takes
   `DiscoveredPlugins`, not a `PluginRegistry`: it needs only the resolved
   commands, and walks every language unconditionally in one pass, so the
   registry's lazy-spawn machinery has no use here.
2. We fail the whole walk when any one language fails, rather than skip that
   language and index the rest. An index that silently skipped a language
   looks complete to every tool downstream, and none of them can tell "this
   symbol does not exist" from "the plugin that would have found it never
   ran"; a wrong answer is trusted until someone checks by hand, a missing
   one never is. The failure message names the command that fixes an
   unbuilt plugin (`plugin::missing_plugin_binary_hint`).

## Consequences
- A language with zero files in the project still costs a spawned one-shot
  process. Skipping absent languages would need a pre-scan of file
  extensions, which is not done.
- A dev checkout needs every bundled plugin built (Go on `$PATH`,
  `npm ci && npm run build` for TypeScript, `cargo build --workspace` for
  the Rust-built plugins) before its first index: a one-time cost, not a
  per-project one. A released binary ships its plugins staged beside it and
  never hits this.
- Downgrading (2) to "skip and continue" would require the extension
  pre-scan above, and would still let a project that does contain the
  failing language serve a graph missing it while reporting itself indexed.
