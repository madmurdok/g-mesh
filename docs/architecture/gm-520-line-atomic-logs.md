# GM-520: line-atomic daemon log

Status: design (GM-520/S1). No production code or tests changed in this slice.

## Problem

A shim-bootstrapped daemon's stderr is the project's `daemon.log` (or
`G_MESH_DAEMON_LOG`), opened `O_APPEND` in `shim::daemon_stderr`
(core/src/shim.rs:488). Plugins are spawned with `Stdio::inherit()`, so every
plugin process appends to the same file directly:

- `daemon::plugin::PluginProcess::spawn` (core/src/daemon/plugin.rs:600, stderr at :622): the long-lived plugin and every crash relaunch.
- `daemon::bulk_index::walk_one_language_in` (core/src/daemon/bulk_index.rs:356, stderr at :389): one bulk child per language per walk.

No other daemon spawn inherits stderr (`memory.rs` uses null; the SDK pipes
and drains LSP servers' stderr line by line in `lsp::client::drain_stderr`,
plugins/sdk/src/lsp/client.rs:736; python's helper at
plugins/python/src/semantic.rs:855 uses null).

So several processes append to one file, and a line is safe only if it reaches
the file in a single `write(2)`. Rust's `eprintln!` does not do that.

### Confirmed: `eprintln!` is one `write(2)` per format piece

I checked this in a scratch program outside the repo (rustc 1.97.1). Its stderr was one end of an `AF_UNIX`/`SOCK_DGRAM`
socketpair, so every `write(2)` arrives as its own datagram:

| call | datagrams |
|---|---|
| `eprintln!("trace: tool={tool} ms={ms} ok={ok}")` | 7 (`trace: tool=`, `find_callers`, ` ms=`, `42`, ` ok=`, `true`, `\n`) |
| `eprintln!("constant line")` | 1 (no arguments: `Arguments::as_str` fast path) |
| `format!(..)` then `stderr().write_all(line)` | 1 |
| `write_all` of a 100 001-byte line | 1 |

`Stderr` is unbuffered (`StderrRaw`). `write_fmt` takes the reentrant stderr
lock for the whole call, so threads **inside one process** never interleave. The
interleaving happens between processes. The example kept in
`core/tests/replay_progress.rs:a_plugin_line_inside_a_replay_trace_line_is_cut_back_out`
is exactly this: the TypeScript plugin's exit line landed between the
`elapsed_ms=` and `1987` pieces of the daemon's `replay:` trace line.

Both sides cause it. The plugin line in that example is itself a
multi-piece `eprintln!` (plugins/sdk/src/run.rs), so a daemon line can split a
plugin line just as easily.

### Who writes to the log

| writer | how | line-atomic today |
|---|---|---|
| daemon (core, all daemon-reachable modules) | `eprintln!` with args | no |
| SDK plugins: typescript, python, rust (via `g_mesh_plugin_sdk`) | `eprintln!` in the SDK and in the plugin crates | no |
| `g-mesh-fake-plugin`, `g-mesh-plugin-toy` (plugins/sdk/fake, toy) | `eprintln!` | no |
| Go plugin | `logf` -> `fmt.Fprintf(os.Stderr, ...)` (plugins/go/main.go:46), the only stderr writer in the Go plugin, no `exec.Command` | **yes**: `Fprintf` formats into its buffer and calls `Write` once |
| LSP servers (tsserver, pyright, rust-analyzer) | piped to their plugin, re-emitted by `drain_stderr` | becomes yes once `drain_stderr` is fixed |
| a second daemon sharing `G_MESH_DAEMON_LOG` | the doc on `DAEMON_LOG_ENV` allows sharing ("several daemons ... can share one file") | no |
| Rust panic messages (default hook) | several writes | no, out of scope (see Must confirm) |

No `tracing`/`log`/`env_logger` crate is used in core or the plugins.

## Options

### (a) Line-atomic writes at every source (recommended)

Format the whole line, newline included, into one buffer, then make one
`write_all` call under the stderr lock. On a regular file opened `O_APPEND`, a single
`write(2)` both positions and writes atomically against other appenders
(local filesystems on Linux and macOS; on Windows, `OpenOptions::append` opens with
`FILE_APPEND_DATA`, and each `WriteFile` is one append). Line length does not matter
for a file: `PIPE_BUF` applies only to pipes, and the 100 KB row above was still
one write.

- Benefit: it fixes every writer, including a second daemon or an orphaned plugin
  sharing the file, which (b) cannot. It adds no threads, pipes or liveness coupling.
  The Go plugin already behaves this way.
- Risk: every future `eprintln!` in daemon or plugin code brings the bug back.
  The mitigation is a lint guard, an owner decision under Must confirm.
  A third-party plugin that does not follow the contract still splits lines. The
  contract gets documented next to the SDK helper.
- Limit: when the daemon's stderr is a **pipe** (a test spawning `g-mesh
  daemon` directly with `Stdio::piped()`), lines over `PIPE_BUF` (512 B on macOS,
  4 KiB on Linux) can still interleave. The AC is about the log file, which is
  the bootstrapped path, so this limit is acceptable.

### (b) The daemon pipes plugin stderr and forwards it through one writer (rejected)

The daemon would spawn plugins with `Stdio::piped()` and run a thread per child
that reads lines and re-emits them with `eprintln!`. Within one process the stderr lock
serializes those lines, so plugins would need no change.

Rejected because:

1. **It does not cover every source.** Two daemons sharing
   `G_MESH_DAEMON_LOG` (allowed by its doc, and used by tests and bench runs)
   still interleave. A plugin orphaned by a killed daemon has lost its reader.
2. **Logging becomes a liveness dependency.** If the forwarder stalls (a wedged
   daemon, see `wedged_daemon.rs`; SIGSTOP; a slow disk), the pipe fills (16-64 KiB) and the
   plugin blocks inside an `eprintln!`, holding the stderr lock, so every
   other thread of the plugin that logs blocks with it.
3. **It loses the lines that matter most and changes how plugins die.** After the daemon
   dies, the read end closes and the plugin's next `eprintln!` gets `EPIPE`.
   `eprintln!` panics on a write error. The plugin then dies of a panic instead of taking its
   lifeline path (`plugins_die_with_daemon.rs`), and its
   `core closed the control stream - exiting` line is never written, the very
   line in the replay_progress example.
4. **It costs more.** It needs a thread and a pipe per plugin process (one per language per
   walk, plus relaunches), and on Windows more handles must be kept
   non-inheritable (`shim_handle_inheritance.rs`, GM-251), all to save a
   mechanical edit to about 70 call sites.

## Design

### The helper, two copies

The SDK must not depend on core (see the workspace `Cargo.toml` comment), and
`g-mesh-wire` holds wire types only. So the ~15-line helper exists twice,
with the same semantics:

- core: new module `core/src/log.rs` (`pub mod log;` in core/src/lib.rs),
  `#[macro_export] macro_rules! log_line` -> `g_mesh::log_line!`.
- SDK: new module `plugins/sdk/src/log.rs` (`mod log;` in plugins/sdk/src/lib.rs),
  `#[macro_export] macro_rules! log_line` -> `g_mesh_plugin_sdk::log_line!`.

The shape the implementer follows:

```rust
#[macro_export]
macro_rules! log_line {
    ($($arg:tt)*) => { $crate::log::write_line(format_args!($($arg)*)) };
}

#[doc(hidden)]
pub fn write_line(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let mut line = std::fmt::format(args); // one buffer
    line.push('\n');
    // One write(2) under the stderr lock. A failed log write is ignored:
    // a diagnostic must never take the process down (eprintln! panics).
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}
```

The SDK copy's doc comment states the contract for plugin authors: *every
stderr line goes out in one `write`; the daemon log is shared by the daemon
and every plugin*.

### Edit map

Replace `eprintln!(...)` with `log_line!(...)` (same arguments), **only in non-test
code**. Skip any site inside `#[cfg(test)]`, `*/tests.rs` or `tests/`.

**Core, daemon-process code.** These modules run in the daemon. `embedding`, `graph`,
`storage` and `watcher` may also run in-process for a CLI command, which does no harm.

| file | eprintln! lines (main @dc7d160) |
|---|---|
| core/src/mcp/mod.rs | `trace_call` :93-96 (the fn the AC names), 319 331 402 426 428 530 597 643 656 |
| core/src/mcp/front.rs | 100 |
| core/src/daemon/mod.rs | 205 209 248 251 276 445 548 686 |
| core/src/daemon/activation.rs | 122 142 189 198 242 247 267 272 |
| core/src/daemon/bulk_index.rs | 212 284 292 444 460 506 |
| core/src/daemon/front.rs | 72 131 |
| core/src/daemon/indexing_status.rs | 423 |
| core/src/daemon/lifecycle.rs | 114 313 321 348 364 536 559 584 588 598 603 764 779 |
| core/src/daemon/manifest.rs | 605 |
| core/src/daemon/plugin.rs | 259 341 370 958 1122 1307 1378 1435 1443 |
| core/src/daemon/registry.rs | 630 971 1009 1025 1043 1050 1274 |
| core/src/daemon/semantic.rs | 203 273 276 325 390 431 473 496 504 521 |
| core/src/daemon/workspace_reindex.rs | 99 131 133 144 149 198 209 238 250 260 324 |
| core/src/embedding/{pipeline,backfill,cache,rerank}.rs | pipeline 108 117 297 373 457 528 651 657 670 693 704 723; backfill 135 152 244; cache 220; rerank 265 298 |
| core/src/watcher/{apply,mod}.rs | apply 164 267 276 281 288 481 512 577; mod 122 |
| core/src/graph/{containers,symbol_links}.rs | containers 512; symbol_links 1301 |
| core/src/storage/{index_store,schema}.rs | index_store 138; schema 1037 |
| core/src/main.rs | 8: the top-level error print. In the `daemon` subcommand it is the daemon's last log line |

**Core, left alone (different role, not in the log):**
- `core/src/shim.rs`, `core/src/shim/router.rs`: the shim's stderr is the MCP
  client's, not the daemon log. Its threads are already serialized by the in-process lock.
- `core/src/cli/**` (`cli/mod.rs:114`, `model.rs`, `embed_eval*`, `clean.rs`):
  CLI-only output to a terminal or the caller's pipe.
- `core/src/graph/containers/tests.rs:983` and all test code.

**Plugins (Rust):**

| file | lines |
|---|---|
| plugins/sdk/src/run.rs | 161 176 198 207 210 246 277 286 293 348 384 402 467 492 554 714 787 793 830 855 |
| plugins/sdk/src/lsp/bridge.rs | 611 635 659 667 778 2205 2233 2237 2252 2339 2344 2377 2774 2912 |
| plugins/sdk/src/lsp/client.rs | 660, and `drain_stderr` 743 745 755 (re-emits the LSP server's lines) |
| plugins/sdk/src/{semantic,manifest,hold}.rs | semantic 248 277 336; manifest 134 139; hold 52 54 |
| plugins/sdk/fake/main.rs | 160 211 |
| plugins/sdk/fake/toy.rs | 317 |
| plugins/python/src/semantic.rs | 283 302 309 412 |
| plugins/typescript/src/semantic.rs, extractor/mod.rs | 109; 77 |
| plugins/rust/src/semantic.rs | 91 |

Left alone: `plugins/sdk/tests/lsp_bridge.rs` (test), `plugins/sdk/fake-lsp`
(an LSP server whose stderr the bridge drains line by line), and the Go plugin (already atomic).

**Docs only:** `DAEMON_LOG_ENV` doc (core/src/shim.rs:58-66). Add one sentence:
every writer to this file emits a line in one `write`, which is why sharing is safe.

**Read for context:** `shim::daemon_stderr` (core/src/shim.rs:488),
`PluginProcess::spawn` (plugin.rs:600), `walk_one_language_in`
(bulk_index.rs:356), `drain_stderr` (client.rs:736), `trace_call` and its
callers `prepare`, `replay_queued_changes`, `ensure_file_fresh`, `wait_until`
(all core/src/mcp/mod.rs).

## Behaviour list (for the tests slice)

1. `log_line!` emits the formatted text plus `\n` as exactly one `write(2)`,
   for a line with arguments and for one over 64 KiB (`core::log::write_line`,
   `g_mesh_plugin_sdk::log::write_line`).
2. A daemon trace line (`mcp::trace_call`) and a plugin line written
   concurrently to one `O_APPEND` file never split each other. Every line in the
   file is a whole line from exactly one source.
3. Several processes appending through `log_line!` to one file (the shared
   `G_MESH_DAEMON_LOG` case) never split each other's lines.
4. A failed stderr write (closed or invalid stderr) is ignored: no panic
   (`write_line`).
5. With line atomicity in place, `replay_progress`'s fragment stripping is no longer needed
   (AC 2).

## Stress tests

**T1, multi-process writer test (fast, the main control).** The test binary
re-executes itself as N=4 writer children (env-gated), each with
`stderr = Stdio::from(file.try_clone())` of one file the parent opened with
`append(true)`, which is the same setup as `daemon_stderr`. Each child writes M=5000
lines through `log_line!`, built from many interpolated pieces, e.g.
`"[w{id}] seq={n} a={x} b={y} ... end={id}:{n}"`. The parent asserts N*M lines,
each matching the full grammar, each `(id, n)` present once. Put it in core
(`core/tests/`) for `g_mesh::log_line!`. An SDK twin is optional,
because the helpers are identical.
- Control: replace `write_line`'s body with `eprintln!("{args}")`. The test must
  find a malformed line. The tests slice confirms this control fails 5 of 5.

**T2, end-to-end (the AC's "concurrent plugin stderr and daemon trace").**
The daemon is bootstrapped through the shim with `G_MESH_DAEMON_LOG=<file>` and
`TRACE_CALLS_ENV=1`, and uses the fake plugin in its fixture persona (as `idle_lifecycle.rs` does).
A new test-only knob on the fake plugin keeps logging multi-argument lines through
`g_mesh_plugin_sdk::log_line!` in a loop while `<dir>/stderr-spam` exists. The test creates the file,
fires about 200 tool calls concurrently (each emits several `g-mesh daemon:
prepare: ...` lines), then removes it. Assert: no line contains both
`g-mesh daemon:` and `g-mesh-fake-plugin`, every spam line matches its grammar,
and every daemon trace line matches the `prepare:` shape.
- Control: revert `trace_call` (mod.rs:95) and the spam loop to `eprintln!`.
  A split line appears. Adding the knob to the fake plugin is test infrastructure,
  so it belongs to the tests slice.

**replay_progress (AC 2).** Delete `PLUGIN_LINE_TAGS` (:94) and the
stripping loop in `replay_trace_lines` (:253), leaving a plain filter. Delete
`a_plugin_line_inside_a_replay_trace_line_is_cut_back_out` (:271), since it tests
the stripping itself. Run `replay_progress` 10 times, once under load
(alongside the full suite).

New tests spawn processes, so each runs 5 times in the tests slice and 5 times in verify.

## Must confirm (owner)

1. **Option (a)** over (b), for the reasons above.
2. **Lint guard against regressions:** add `clippy.toml` `disallowed-macros =
   ["std::eprintln", "std::eprint"]`, with `#[allow]` in `core/src/cli`, the shim,
   and test code. Recommendation: **a follow-up task, not this one**. It touches
   every integration-test file that prints, and it is policy, not this fix.
   Risk if skipped: new daemon or plugin code reintroduces split lines quietly.
3. **Ignore write errors** in `write_line` (today `eprintln!` panics). Recommended:
   a lost log line is better than a dead daemon or plugin. No current code relies on
   the panic, because plugin stderr is a file and a file write never gets `EPIPE`.
4. **Panic messages stay out of scope.** The default panic hook writes in pieces
   and can still be split. Fixing that needs a custom hook in the daemon and the SDK. Proposed as a
   follow-up if it ever shows up in a log.
5. **Two copies of the helper** (core and SDK) rather than putting it in `g-mesh-wire`.
   Alternative: a `wire::log` module, which avoids the duplicate but stretches wire's
   "wire types only" scope.
6. **Windows:** the atomicity claim for `FILE_APPEND_DATA` writes rests on
   documentation. T1 running in the Windows CI job is the evidence. If it fails
   there, that is a finding, not a flake.
7. **ONNX Runtime** (`ort` rc.9, in the daemon) is assumed not to write to
   stderr itself. Nothing in core configures its logging. Check one `daemon.log`
   after an embedding run for ORT lines.

## g-mesh calls behind this note

- callers of `trace_call`: `find_callers(symbol_name="trace_call")`. It returned
  `prepare`, `replay_queued_changes`, `ensure_file_fresh`, `wait_until`, all in
  core/src/mcp/mod.rs (complete, `hasMore: false`).
- crates that use the SDK: `get_dependencies(Incoming, plugins/sdk/src/lib.rs, depth 1)`.
  It returned the typescript, python and rust plugin crates, plus the SDK's own bins and tests
  (complete). The SDK has no logging helper today. Each crate calls `eprintln!` directly.
- spawns with inherited stderr: found by grep `Stdio::` over core/src, a
  non-symbol search, then `PluginProcess::spawn` and `walk_one_language_in`
  read from their files.
