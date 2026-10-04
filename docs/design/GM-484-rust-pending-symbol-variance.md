# GM-484: Rust pending_symbol variance between identical bulk builds

Status: design note (GM-484/S2). Commit under study: `cafe6db`.

## Summary

The two outcomes are not two ways of naming the same placeholders. In the LOW
builds the **Rust semantic pass never ran**. The rust plugin was idled out
while it waited for the Go and Python passes, and `PluginSupervisor::semantic_pass`
then answered `Ok(false)` ("plugin asleep") for Rust. The language stays owed
until the next daemon start or `g-mesh reindex`.

- **HIGH (8324) is the correct outcome.** LOW is missing the Rust semantic tier.
- The doubled module path (`g_mesh::cli::clean::cli::clean::…`) has nothing to
  do with the variance. It is how a placeholder *label* is rendered, and it
  shows up in both outcomes (see "The doubled path").
- Fix: keep an owed plugin from being idled out while a semantic run that owes
  it a pass is in progress.

## Evidence

### 1. The delta is exactly the Rust semantic pass's output

The LOW DB is `h/b1`, the HIGH DB is `h/b2`. The SQL and the classifier are in
the S1 scratch dir, `s2/` (`shape.py`).

| | LOW (b1) | HIGH (b2) |
|---|---|---|
| `language_state.rust.semanticPassAt` | NULL | 2026-10-04 02:40:53 |
| `language_state.rust.semanticPassError` | `not run - its plugin was asleep or memory-suspended; …` | NULL |
| `edges` with `engine='rust-analyzer'` | **0** | 5987 (CALLS 3176, REFERENCES 2792, SUPERTYPE_OF 19) |
| rust pending_symbol | 5387 | 8324 |
| pending only in this build (by node id) | 0 | 2937 |

- Every one of the 2937 HIGH-only pending nodes has a `placeholder_targets` row.
- None of them has a real node with the same qualifiedName.
- The daemon log for b2 shows the Rust pass upserting them:
  `[rust] semantic pass: 270 file(s), 7190 node(s)/5987 edge(s) upserted, 825 edge(s) retracted`.
- Syntactic tree-sitter edges differ by the semantic pass's retractions only
  (CALLS 10099→9142, REFERENCES 10196→10148, SUPERTYPE_OF 26→23).

So the producing path is the Rust plugin's semantic pass (LSP bridge →
`add_placeholder`). It is not bulk extraction, and it is not core linking.

### 2. The daemon log of the LOW build

```
[g-mesh-go] semantic pass … in 4.557s
g-mesh daemon: rust plugin (pid 7765) put to sleep - idle for 60s; …
[python] semantic pass: … in 64.702876144s
g-mesh daemon: the rust semantic pass was not run - its plugin is asleep or suspended
```

In b2 the same sequence ran (Go 5.9s, Python 63.2s), but the rust plugin was
not idled before its turn came, and its pass ran (255s).

### 3. Mechanism (code at cafe6db)

1. `daemon::semantic::run_with_registry_and_progress` (core/src/daemon/semantic.rs:313-358)
   computes the owed languages: go, python, rust, typescript.
2. `prepare_owed` (semantic.rs:368-386) spawns the rust plugin and tells it the
   pass is coming, because Rust declares `semantic_prepare`.
   `PluginSupervisor::prepare_semantic_pass` (lifecycle.rs:375-386) `touch()`es
   the supervisor once. rust-analyzer starts priming in the plugin, but nothing
   that crosses the supervisor touches it again.
3. The passes run sequentially: go, then python. That takes about 69s under any
   load, because pyright via npx takes about 63s.
4. Meanwhile `lifecycle::supervise` (lifecycle.rs:694-735) ticks every
   `min(plugin, core idle)/4`. With S1's `G_MESH_PLUGIN_IDLE_MS=60000` and
   `G_MESH_CORE_IDLE_MS=60000` that is every 15s. On each tick
   `PluginRegistry::sleep_if_idle_all` (registry.rs:1037-1041) calls
   `PluginSupervisor::sleep_if_idle` (lifecycle.rs:427-445) on the rust
   supervisor, which has been idle since the prepare. Once it has been idle for
   60s or more, it is put to sleep.
5. When the loop reaches rust, `get_or_spawn` (registry.rs:812-853) returns the
   existing `Running` supervisor. It does not wake the plugin. Then
   `PluginSupervisor::semantic_pass` (lifecycle.rs:353-367) finds
   `inner.process == None` and returns `Ok(false)`, and `record_not_run`
   writes `NOT_RUN_REASON`.

The race is whether a supervise tick lands after rust's idle clock passes
60s and before python's pass returns, about 69s after the prepare. The tick
phase is arbitrary relative to the prepare, so some runs hit that window and
some miss it. Load stretches the go/python passes and the rust startup,
which widens the window. That explains the load correlation S1 saw (2 of 2
high-load builds came out LOW, 0 of 5 low-load builds did).

Production exposure: the default plugin idle timeout is 60 min
(`DEFAULT_PLUGIN_IDLE`, lifecycle.rs:45). Production hits this only when the
passes queued ahead of a prepared language take more than an hour. That is
rare but reachable on a large polyglot project. Any harness that sets
`G_MESH_PLUGIN_IDLE_MS=60000` hits it regularly. That covers S1, GM-481, and
g-mesh-bench runs that copy the setting. Their Rust semantic numbers are
unreliable until this is fixed.

### 4. Forced control (same binaries, corpus, scripts; `s2/run_ctl.sh`, `s2/controls.sh`)

Two builds ran back to back at low load (1-min load 3-5, the same band as S1's
HIGH builds). Only the plugin idle timeout changed:

| build | `G_MESH_PLUGIN_IDLE_MS` | rust_pending | nodes | edges | rust-analyzer edges | rust semanticPassError | real |
|---|---|---|---|---|---|---|---|
| b11 | 20000 (shorter than go+python's ~40s) | **5387** (LOW) | 19938 | 50868 | 0 | `not run - …asleep…` | 165.5s |
| b12 | 0 (idle timer off) | **8324** (HIGH) | 22875 | 55836 | 5987 | NULL | 221.8s |

The b11 log shows the same three lines as S1's LOW build: rust put to sleep
after `idle for 20s`, python's pass finishing in 33.8s, then "the rust semantic
pass was not run". Both counts match S1's two modes to the node. The idle timer
alone flips the outcome. Load only moves the race window.

## The doubled path

`g_mesh::cli::clean::cli::clean::Target::Cwd` is the placeholder's
`qualifiedName`, built by `render_target` (plugins/sdk/src/graph.rs:618-628)
as `<container>::<key>`. Here the container is `g_mesh::cli::clean` and the key
is the crate-relative qualified name `cli::clean::Target::Cwd`, which is the
same shape as real Rust nodes (`cli::clean::Target`). Its doc comment says
outright that it is "a label, not an address": core resolves through
`placeholder_targets` (scope, key, keyPath), and the label is only there so the
id is unique.

Doubled labels show up in **both** outcomes. LOW has 2401 of 5387 pending, HIGH
has 4849 of 8324, and only 4 real nodes have them. HIGH has more of them only
because it has more placeholders. S1's "1660 names only in HIGH" are the
semantic pass's placeholders. They are not malformed extra nodes.

Changing the label rendering would be cosmetic. It would also change every
placeholder id (`placeholder_id` hashes the label). That is out of scope here.
If anyone wants it, it should be a separate low-priority task.

The edge-less placeholders have the same story: 3723 of 5387 in LOW and
6653 of 8324 in HIGH have no incoming edge. That is existing behaviour in both
outcomes, not part of this variance.

## Proposed fix

**An "awake hold" on the supervisor for the length of a semantic run that owes
that language a pass.**

- `PluginSupervisor` (core/src/daemon/lifecycle.rs:165-193): add
  `semantic_holds: AtomicUsize`, initialised in `start` (lifecycle.rs:204-231).
- New `PluginSupervisor::hold_awake(self: &Arc<Self>) -> AwakeHold`. It
  increments the count. Its `Drop` decrements the count and then `touch()`es,
  so the idle clock restarts from the moment the hold is released, not from
  the last request before it.
- `PluginSupervisor::sleep_if_idle` (lifecycle.rs:427-445) returns `false`
  while `semantic_holds > 0`, checked both outside and inside the `inner` lock,
  the same way `idle_for` is.
- `sleep_now` (core shutdown, orphan) and `check_memory_limit`
  (lifecycle.rs:473-518) ignore holds. A memory suspension is a deliberate
  policy, and its `Ok(false)` stays.
- `daemon::semantic::prepare_owed` (semantic.rs:368-386) returns
  `HashMap<String, AwakeHold>`, with one hold for each language it spawned and
  told.
- `run_with_registry_and_progress` (semantic.rs:313-358) keeps that map and
  drops a language's hold right after that language's pass (`Ok`/`Ok(false)`/`Err`).
  Until then, the earlier languages' passes cannot idle it out. Holds left in
  the map are dropped at the end of the function.

Not waking a sleeping plugin is deliberate and stays as it is.
`a_sleeping_preparing_plugin_is_not_woken_to_be_told` and
`a_pass_not_run_because_the_plugin_is_asleep_is_recorded_with_its_reason`
(semantic.rs:756-772, 955-981) pin it: a plugin asleep before the run started
is still skipped. The fix only stops the run from putting a plugin to sleep
between telling it a pass is coming and asking for the pass.

Benefits:
- It targets exactly the observed race.
- It keeps the warm rust-analyzer that `semantic_prepare` exists for.
- It does not change `semantic_pass`'s contract for `workspace_reindex::run_with`.

Risks:
- A plugin whose pass never comes (a panic in the loop) is held until the
  map drops. Drop-based release covers unwinding.
- A held plugin's RSS still counts for `check_memory_limit`. That is intended.

Rejected alternatives:
- Waking a sleeping supervisor in the semantic loop. It contradicts the two
  pinned tests, and it would cold-start rust-analyzer inside the pass's
  file-count-scaled timeout.
- Touching all owed supervisors between passes. A single pass longer than the
  idle timeout still loses the next language.
- Raising the idle timeout in harnesses. That hides the problem, and
  production at 60 min is still exposed.

Recommended harness follow-up (not code): S1/GM-481/g-mesh-bench runs with
`G_MESH_PLUGIN_IDLE_MS=60000` should check `language_state.semanticPassError`
or the "was not run" log line before they trust Rust semantic counts.

### Edit map

| Change | Location |
|---|---|
| field + init | `PluginSupervisor` struct, lifecycle.rs:165-193; `PluginSupervisor::start`, lifecycle.rs:204-231 |
| new `hold_awake` + `AwakeHold` (Drop: decrement, touch) | lifecycle.rs, next to `touch`/`idle_for` (573-580) |
| hold check | `PluginSupervisor::sleep_if_idle`, lifecycle.rs:427-445 |
| return holds | `daemon::semantic::prepare_owed`, semantic.rs:368-386 |
| keep and release per language | `daemon::semantic::run_with_registry_and_progress`, semantic.rs:313-358 |
| doc: update "expected to be unreachable" | `run_with_registry` doc, semantic.rs:295-306 |

Read for context: `PluginSupervisor::semantic_pass` and `prepare_semantic_pass`
(lifecycle.rs:353-386), `lifecycle::supervise` (694-735),
`PluginRegistry::get_or_spawn` (registry.rs:812-853) and
`sleep_if_idle_all` (registry.rs:1037-1041), the gate fixture
`test_plugin::install_gated` / `open_handshake_gate` (core/src/daemon/test_plugin.rs:212-245).

Callers and references relied on:
- `PluginSupervisor::semantic_pass` callers are `run_with_registry_and_progress`,
  `workspace_reindex::run_with` and the `race_pass_against_memory_check` test
  (g-mesh `find_callers`, symbol_id 092beb06…).
- `run_with_registry_and_progress` callers are `run_with_registry`,
  `ActivationCtx::activate` and `ActivationCtx::walk` (`find_callers`).
- `sleep_if_idle`: g-mesh `find_callers` returned an empty result, because the
  call is a method on a loop variable, the documented gap. grep found the only
  caller, registry.rs:1039 `sleep_if_idle_all`.

## Behaviours the tests slice must pin

Each item lists the test, then its control: the code change that must make the
test fail.

1. **A held supervisor is not idled out.** Unit test, lifecycle.rs. Supervisor
   with `idle_timeout = Some(1ms)`; take `hold_awake()`; wait past the timeout;
   `sleep_if_idle()` is `false` and `pid()` is `Some`.
   *Control:* remove the hold check from `sleep_if_idle`. It returns `true`.
2. **Releasing a hold restarts the idle clock.** Same supervisor, with
   `idle_timeout` well above the test's own steps (e.g. 200ms). Hold for longer
   than the timeout, drop the hold, and `sleep_if_idle()` right away is `false`.
   After the timeout it is `true`.
   *Control:* remove the `touch()` from `AwakeHold::drop`. The first call
   returns `true`.
3. **A prepared language survives a preceding language's long pass** (the bug
   itself). semantic.rs tests. Registry built with a short `idle_timeout`.
   `two_language_registry_adjusted` needs an idle parameter. Beta declares
   `semantic_prepare`. Alpha's `semanticPass` is held open by a gate. That
   needs a new fixture knob modeled on `install_gated`/`open_handshake_gate`,
   because `install_stalling` never answers. While the gate is closed, a thread
   calls `registry.sleep_if_idle_all()` past the timeout, then opens the gate.
   Assert `run.completed == [alpha, beta]` and `beta` has `semanticPassAt`.
   *Control:* make `prepare_owed` return no holds (or revert the
   `sleep_if_idle` check). Beta is recorded with `NOT_RUN_REASON` and
   `completed == [alpha]`.
4. **The hold is released after the language's own pass.** Same fixture: after
   `run_with_registry` returns, wait past the timeout and call
   `sleep_if_idle_all()`. Beta's plugin is asleep (`pid()` is `None`).
   *Control:* leak the holds (e.g. `std::mem::forget` the map). Beta stays awake.
5. **No regression for an already-sleeping plugin.** The existing
   `a_sleeping_preparing_plugin_is_not_woken_to_be_told` and
   `a_pass_not_run_because_the_plugin_is_asleep_is_recorded_with_its_reason`
   stay green unchanged.
   *Control:* not a new test. It guards against the rejected "wake" alternative.
6. **Memory suspension still wins over a hold.** Optional, if cheap: a held,
   memory-suspended supervisor's `semantic_pass` still answers `Ok(false)`.
   *Control:* make `semantic_pass` skip the suspension check while held.

End-to-end confirmation, measured in the verify slice and not as a unit test:
rerun `s2/controls.sh`'s `PIDLE=20000` build on the fixed binary. Expect
rust_pending 8324 and rust `semanticPassAt` set.

## Acceptance criteria

The criterion was "identify the cause and fix it, or file it with the
evidence".
- The cause is identified and confirmed by a forced control.
- The fix is two functions in `lifecycle.rs` plus two in `semantic.rs`, with
  unit tests. It stays within the criteria.
- The doubled-label observation is explained as by-design. Changing it would
  be a separate cosmetic task and is not needed for GM-484.
