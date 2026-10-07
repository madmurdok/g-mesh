# GM-325: TypeScript semantics through `LspBridge`, against a server g-mesh does not ship

Design note for GM-325/S1. It builds on GM-324's structural port, which is on
the integration branch `feat/GM-324-325-ts-port`. `plugin.toml` currently runs
`g-mesh-plugin-typescript` with `semantic_pass = false`. GM-351's note
(`docs/architecture/gm-351-core-without-node-plugin.md`) is the companion for
core's Node paths. This note uses GM-351's interfaces and does not redesign
them.

Paths are given as they are today (`plugins/typescript/rust/...`). GM-351/S3
moves that directory to `plugins/typescript/src/` before GM-325's TS-tier code
slice, so the code slices use the moved paths. Line numbers are 1-based and
taken at `af8a588`.

## 0. Summary

| Question | Answer | Evidence |
|---|---|---|
| Server | **vtsls** (`@vtsls/language-server`), engine label `vtsls` | §1. It answered every traced question, including the **bound overload** at both overloaded calls. It bundles TypeScript 5.9.3, the same tsserver version the Node pass drove, so it does not depend on the project's TypeScript. typescript-language-server answers the same questions but fails at `initialize` in a TypeScript 7 project, after its `--version` probe has passed. The native port answers fastest, but returns the whole overload set and its hover cannot bind an imported overload. |
| Resolution order | PATH, then `<root>/node_modules/.bin`, then `npx --yes --package @vtsls/language-server vtsls`. Every candidate is probed with `--version` under a budget. | §2 |
| Readiness | `on-demand` | §3. tsserver loads a project when a document is opened; there is no workspace index to wait for. Its one false-empty answer arrives inside the bridge's 2 s deferral window. |
| `receiver_calls` | Flips to `"resolved"`. `receiver_calls_structural` stays `"unresolved"`. The plugin also records a `ReceiverCall` site for an unbound `this.m()` / `super.m()`. | §4. All three servers answered `g.greet()` on an interface parameter, `s.pick()` on a local, and inherited `this.hello()` / `super.hello()` from another file. |
| SDK lift | New `plugins/sdk/src/lsp/resolve.rs`. python and rust both use it. | §5, with the g-mesh caller list |
| Re-export / default upgrade (the Node pass's 2nd question) | The TS extractor records `Reference` sites with `replaces`. The bridge asks only where the placeholder's file does not declare the name. | §4.3 |
| 27 red core tests | 10 go green with a server on PATH (2 of them need the new re-export sites), 2 need expectation flips. The 15 lifecycle/status tests go green from the static `semantic_pass = true`, with or without a server. | §6 |
| CI | `scripts/test-deps.sh typescript` runs `npm ci` against a pin-only `plugins/typescript/package.json` and adds its `.bin` to `GITHUB_PATH`. This is the pyright step, repeated. | §7 |
| npm deletion + `git mv` | GM-351/S3, as GM-351's note §9 already orders it. GM-325 then adds back a pin-only `package.json`. | §8 |
| README gap list | A measure slice on the fixture and on excalidraw, per site kind | §9 |

## 1. Server choice: what each real server answered

### 1.1 Method

The trace followed GM-299's precedent: real servers, not READMEs. A minimal
LSP client (`trace.mjs`, about 200 lines of Node, run once per server by one
`run.sh`) sends the bridge's own `initialize` parameters, which are copied
from `plugins/sdk/src/lsp/client.rs:269-301`: `positionEncodings`
`[utf-8, utf-16]`, `workDoneProgress`, `configuration`, and `definition` /
`implementation` with `linkSupport`. It answers `workspace/configuration` with
nulls and every other server request with null, the way the client does. It
was run against a scratch copy of `plugins/typescript/conformance/project`
plus two files, `src/extra/base.ts` and `src/extra/derived.ts`, which add an
inherited `this.hello()` / `super.hello()` from another file and receiver
calls on locals (`new Store()`, `new Greeter()`).

The order of operations was:

1. A **cold question**: open only `src/main.ts` and immediately ask for the
   definition at `m.double`. `math.ts` is never opened.
2. Open every file, as `sync_documents` does for a whole-project scope, then
   ask every question below.
3. Ask for hover at both overloaded calls and at each of `format`'s three
   declaration names.
4. Ask for the implementation at `Greetable`, `Greetable#greet` and
   `Container`.

`COLD=all|wait3|main` varied step 1: open everything first, open `main.ts` and
wait 3 s, or open `main.ts` and ask at once.

Installed under the scratchpad with npm 9.8.1 on Node v20.6.1 (darwin x64):

| Label | Package | Server version |
|---|---|---|
| tsls5 | `typescript-language-server@5.3.0` + `typescript@5.9.3` | tsserver 5.9.3 |
| tsls6-ts7 | `typescript-language-server@6.0.1` (engines `node >=22.22.2`) with `typescript@7.0.2` resolvable | - |
| tsls6-ts59 | same, `initializationOptions.tsserver.path` pointed at 5.9.3 | tsserver 5.9.3 |
| vtsls | `@vtsls/language-server@0.3.0` | bundled `typescript@5.9.3` (dependency of `@vtsls/language-service`) |
| tsgo | `@typescript/native-preview@7.0.0-dev.20260707.2`, platform binary `lib/tsgo --lsp --stdio` | typescript-go |
| tsc7 | `typescript@7.0.2`, platform binary `@typescript/typescript-darwin-x64/lib/tsc --lsp --stdio` | typescript-go 7.0.2 |

`typescript@latest` is **7.0.2**, the native compiler. It has no
`lib/tsserver.js`, and its `tsc` *is* an LSP server (`--lsp --stdio`). Its npm
`bin/tsc` wrapper does not run on Node 20.6: it fails with
`ERR_UNKNOWN_FILE_EXTENSION` and exit 1. `@typescript/native-preview`'s
wrapper fails the same way. The platform binaries run. Machine state: load
averages 8.5-19.6 during the runs (`uptime` before and after). `time -p`
user+sys was 0.15-0.6 s per run against 1.8-5.8 s real, so the wall clock is
mostly the script's own waits and the servers' blocking project loads, not
CPU.

### 1.2 Capabilities (initialize result)

| | tsls5 / tsls6-ts59 | vtsls | tsgo / tsc7 |
|---|---|---|---|
| `definitionProvider` | true | true | true |
| `implementationProvider` | true | true | true |
| `hoverProvider` | true | true | true |
| `positionEncoding` | absent (UTF-16) | absent (UTF-16) | `utf-8` |
| `$/progress` during a pass | 0 (main) / 2 (all) | 1-4 ("Analyzing 'main.ts' and its dependencies") | 0-2 |
| server→client requests | `workspace/configuration` | + `window/workDoneProgress/create` | + `client/registerCapability` |

tsls6-ts7 **failed at `initialize`**: *"The TypeScript of the workspace
(TypeScript 7.0.2 ...) provides no tsserver.js. No other valid TypeScript
installation was found. Exiting."* Its `--version` printed `6.0.1` all the
same. That is a name that probes healthy but does not run as a server, the
same failure plugins/rust found with the rustup proxy.

### 1.3 Definitions (0-based LSP sent, 1-based `file:line:col` shown)

All three tsserver-backed runs (tsls5, tsls6-ts59, vtsls) gave identical
answers. tsgo and tsc7 agreed with each other.

| Question (site) | tsserver-backed | native (tsgo/tsc7) |
|---|---|---|
| namespace member call `m.double(4)` (main.ts:32) | `src/math.ts:6:17` | same |
| namespace member read `lib.target` (nsref/use.ts:4) | `src/nsref/lib.ts:1:17` | same |
| receiver `g.greet()`, `g: Greetable` (shapes.ts:30) | `src/shapes.ts:7:3` (the **interface** method) | same |
| receiver `g.greet()`, `g = new Greeter()` | `src/shapes.ts:11:3` (`Greeter#greet`) | same |
| receiver `s.pick()`, `s = new Store()` | `src/members.ts:9:3` | same |
| `this.hello()`, inherited from another file | `src/extra/base.ts:2:3` | same |
| `super.hello()`, other file | `src/extra/base.ts:2:3` | same |
| static `Store.drop()` | `src/members.ts:13:10` | same |
| **overload** `format("a")` (main.ts:38) | **`src/overload.ts:10:17`** (declaration 0 only) | `11:17, 12:17, 10:17` (whole set) |
| **overload** `format(1)` | **`src/overload.ts:11:17`** (declaration 1 only) | `10:17, 12:17, 11:17` (whole set) |
| named re-export `twice(3)` | `src/math.ts:6:17` (through `index.ts` to `double`) | same |
| `export *` re-export `addViaBarrel()` | `src/math.ts:2:17` | same |
| ambiguous `export *` `mutate()` (amb/use.ts:4) | `src/amb/a.ts:2:17` (the first `export *`) | same |
| default import `DropdownMenuGroup()` | `src/defaults/menuGroup.ts:2:25` (`MenuGroup`) | same |
| plain named import `add()` | `src/math.ts:2:17` | same |
| workspace package `pointOf` via `@fx/geom` | `packages/app/src/root.ts:2:10` (its own import binding) | same |

The last row is a measured gap. Without an `npm install` there is no
`node_modules/@fx/geom` symlink, so no server resolves the package, and the
answer lands on the import binding. `is_addressable` rejects that as a target,
so no edge is recorded. The structural tier already links this call through
`workspaces` (conformance `[[callers]] pointOf`). It is a README gap entry,
not a regression (§9).

### 1.4 Overload binding: GM-348's containment path

- **tsserver-backed:** `definition` names exactly the bound overload's
  declaration. `declaration_at` then finds ordinal 0 for `format("a")` and
  ordinal 1 for `format(1)`. `choose_overload` binds a single bodiless
  ordinal. So `overload_disambiguation = "none"`, the default the SDK doc
  already attributes to tsserver (`config.rs:198-211`), is what this server
  needs.
- **native:** `definition` lists all three declarations, the implementation
  included. With `"none"` nothing binds. With `"hover"` the bridge compares
  the call's hover with each candidate's hover:
  - call: `(alias) function format(value: string): string<doc>`
  - declaration 0: `function format(value: string): string<doc>`

  The plaintext hover also runs the JSDoc straight onto the signature, with
  no blank line in between. `hover_matches` (`bridge.rs:1961-1966`) compares
  exact text, so the `(alias) ` prefix that every **imported** call carries
  defeats it, and the call stays unbound (fail closed). A same-file call
  would match. This is exactly the "quietly stops setting `to_declaration`"
  that the acceptance criteria forbid.
- vtsls's hover also carries a `(loading...)` prefix while the project loads.
  That does not matter under `"none"`, which never hovers, but it rules out
  `"hover"` for vtsls too.

### 1.5 Implementation

All servers answered `Greetable` → `Greeter` and `Greetable#greet` →
`Greeter#greet`. For `Container` they returned `box.ts:4:14` (`Box`) **and**
`box.ts:10:14` (`SpecialBox extends Box`): a transitive answer. The kit
asserts implementations are direct (`expect.toml` `[[implementations]]
Container`), and the structural tier already emits the direct `SUPERTYPE_OF`
edges. So `implementation_kinds = []`, the same as python. Turning the sweep
on would add `SpecialBox` and break that entry.

### 1.6 Time to first answer

| | initialize | cold question, `main.ts` only, asked at once | same, all files opened first | same, after 3 s |
|---|---|---|---|---|
| tsls5 | 177-1003 ms | **empty** after 908-1544 ms; correct when re-asked about 200 ms later | correct after 1667 ms | correct in 21 ms |
| vtsls | 225-440 ms | **empty** after 1109-2062 ms; correct when re-asked | correct after 1631 ms | correct in 11 ms |
| tsc7 / tsgo | 28-327 ms | correct after 61-146 ms | correct after 146 ms | correct in 3 ms |

After the first answer, every server answered in 0-20 ms per question. The
first question that touches `math.ts` cost about 350 ms on tsserver and about
100 ms on native.

### 1.7 Choice

| Criterion (what `LspBridge` needs) | typescript-language-server | vtsls | native (tsc 7 / tsgo) |
|---|---|---|---|
| definition, all non-overload rows | yes | yes | yes |
| bound overload (`to_declaration`, GM-348) | yes | yes | **no** (whole set; hover blocked by `(alias)`) |
| runs regardless of the project's TypeScript | **no**: needs a JS `typescript` ≤6 resolvable, and fails at `initialize` against TS 7 after `--version` passes | yes (bundles 5.9.3) | yes (but it is the project's TS only if the project is on 7) |
| same engine as the Node pass it replaces | project's tsserver (Node pass preferred it, README:390-396) | tsserver 5.9.3 = plugins/typescript's own pin (`package.json` `^5.9.3`) | different compiler |
| Node requirement | 5.x: ≥20; 6.x: ≥22 | ≥18 | none (binary); the npm wrapper fails on 20.6 |
| `--version` proves it runs | no (see tsls6-ts7) | yes (`vtsls --version` → `0.3.0`, exit 0; npx: 3.9 s cold, 0.95 s cached) | `tsc --version` is also TypeScript ≤6's non-LSP `tsc` |
| reports `$/progress` | rarely | yes | rarely |
| maintenance | active (6.0.1, 2026-09) | last release 0.3.0, 2025-12 | Microsoft, active |
| size | 25 MB with TS | 25 MB | 28 MB |

**Recommendation: vtsls, alone.** It is the only candidate that meets both of
GM-325's hard criteria with no SDK change:

- it resolves at least what tsserver resolved (it *is* tsserver 5.9.3);
- it does not drop overload binding.

It is also the only one whose `--version` probe is honest about whether the
server will start. A user who wants typescript-language-server points
`[plugin.semantic] command` at it, and that works with the same `--stdio` args
and `"none"` disambiguation. The native port is a follow-up (owner Q1), once
either the bridge can strip a hover prefix per language or tsgo narrows
`definition` to the bound signature, as tsserver does.

## 2. Resolution order

The python precedent (`plugins/python/src/semantic.rs:413-455`), with vtsls's
names:

```toml
[plugin.semantic]
command = "vtsls"
args = ["--stdio"]
engine = "vtsls"
readiness = "on-demand"
# overload_disambiguation left at its default, "none": tsserver's definition
# already names the bound overload (trace in docs/architecture/gm-325-...).
implementation_kinds = []
```

1. **A `command` that is a path** is the only candidate (`script_spellings`
   of it), as in python and rust.
2. **A bare name** is tried in this order:
   - `PATH`, with every host script spelling (`vtsls`, then `vtsls.cmd` on
     Windows);
   - `<root>/node_modules/.bin/vtsls`, with the same spellings;
   - `npx --yes --package @vtsls/language-server vtsls` (the documented
     fallback; `npx.cmd` on Windows).
3. **Probe:** `<candidate> --version`, run under the SDK's budgeted probe
   (60 s, both pipes drained, killed on timeout). vtsls's server binary
   answers `--version` itself and exits (traced), so there is no CLI twin as
   pyright has. The npx candidate's probe is
   `npx --yes --package @vtsls/language-server vtsls --version`: the same
   argv as the server with `--version` in place of `--stdio`.
4. **Nothing usable:** an error such as `no usable vtsls: <each candidate:
   reason>. Install it with npm install -g @vtsls/language-server (or in the
   project), or point [plugin.semantic] command in plugins/typescript/plugin.toml
   at a TypeScript language server`. The factory returns `Err`. That is the
   existing degradation: one log line, an empty incomplete diff, and the
   receiver gap stays listed.

The project's own `node_modules/.bin` is searched only for `vtsls`. A
project's `typescript` is **not** used:

- with TS ≤6 it has no LSP entry point;
- with TS 7, `tsc --lsp` is the native server and its overload answers fall
  short (§1.4).

This is a deliberate departure from the Node pass, which preferred the
project's `node_modules/typescript`. The README has to say so (§9).

## 3. Readiness: `on-demand`

Evidence (§1.6):

- No server builds an index before documents are opened. With `wait3`, every
  server answered in 3-21 ms, and none reported progress before the first
  `didOpen`.
- tsserver loads the project that contains an opened file **when it is
  opened**. A question asked while that load is in flight either blocks until
  it finishes and answers correctly (`all`: 1.6 s), or returns `[]` (`main`:
  0.9-2.1 s), after which a re-ask about 200 ms later is correct.
- The bridge's deferral is built for that empty answer
  (`ServerReadiness::OnDemand` doc, `config.rs:132-178`). An empty answer
  while the client has not been quiet for a full `Budgets::settle` (2 s,
  `bridge.rs:126-137`) is re-asked once, after the server goes quiet. The
  traced empty answers arrived 0.9-2.1 s after `didOpen`. vtsls also reports
  `$/progress` during that window, which keeps the client "busy" regardless
  of the clock.
- `indexed` would only add a 2 s quiet wait before the first question of
  every plugin process, for nothing, because there is no startup index to
  wait for.

The guarantee in `config.rs` holds: `on-demand` cannot record "no target"
anywhere `indexed` would not. The residual exposure is the same one
`indexed` has. A project large enough that tsserver's load **returns empty
more than 2 s after the last `didOpen`, with no progress**, defeats both. The
measure slice (§9) counts empty answers on excalidraw that a re-run
contradicts.

## 4. What the plugin asks, and the `receiver_calls` flip

### 4.1 The three questions of `semanticPass.ts`, mapped to the bridge

| Node pass (`semanticPass.ts:10-48`) | Rust port open site (GM-324 §1.4) | Bridge ask | Traced |
|---|---|---|---|
| 1. namespace member use | `Reference`, `replaces = None` | `Ask::Definition` | `m.double`, `lib.target` resolved |
| 2. unresolved edge through a re-export chain (and default imports) | **none yet** | §4.3: `Reference` with `replaces`, hop-filtered | `mutate` → `a.ts`, `DropdownMenuGroup` → `MenuGroup` |
| 3. overloaded call | `OverloadCall`, `replaces` = the CALLS edge | `Ask::Overload` → `bind_overload` | ordinals 0 and 1 bound |
| (new) receiver call | `ReceiverCall`, `replaces = None` | `Ask::Definition` | all resolved |

### 4.2 `receiver_calls` flips to `"resolved"`

The trace answers the criterion: the server resolves the `ReceiverCall` sites
the plugin records (§1.3, four receiver rows). So:

- `plugin.toml`: `receiver_calls = "resolved"`. `receiver_calls_structural`
  stays `"unresolved"`, because the structural tier still cannot type `x`.
- The GM-323 inventory sites flip together:
  - `expect.toml` `[[callers]] Greetable#greet` becomes
    `expect = ["src/shapes.ts:viaGreetable"]` with `tier = "semantic"`. The
    server binds `g.greet()` to the **interface** method, which is the
    static-type rule that `P4_STATIC` disclosed for go, rust and python, and
    which now applies to TypeScript too. Since GM-502 the caller page
    discloses it instead (`overrides`, see `gm-502-override-callers-field.md`).
  - `core/tests/plugin_check.rs` `a_namespace_import_caller_needs_the_semantic_pass_to_resolve`
    (1370-1496): move `Greetable#greet` from the "must pass structurally"
    list to the expected structural failures.
  - `core/src/mcp/instructions/tests.rs:19-22`: the TypeScript sentence ("a
    method call through a variable receiver ... may produce no edge") follows
    what `instructions.rs:179` renders once `receiver_calls` is `Resolved`
    and a pass has landed.
- **`untypedCalls` on the wire** (`FileGraphBuilder::record_untyped_receiver_calls`,
  `plugins/sdk/src/graph.rs:444`). GM-324 Q5 deferred turning this on until
  the flip. It is turned on with the flip. The three other plugins send it, and
  the bridge trims answered names (`trim_untyped_calls`, `bridge.rs:2459-2499`).
  So the time before the first pass lands, and a project with no server, show
  `unlinkedUsages` instead of a silent under-report.

**Handoff question: should an unbound `this.m()` / `super.m()` get a site?
Yes.**

- In `resolve_call` (`plugins/typescript/rust/extractor/bodies.rs:309-356`),
  the `ReceiverCall` site is recorded only for `CallReceiver::Qualified`.
- A `This` / `Super` call whose `lookup_call_target` is `None`, for example a
  method inherited from a class in another file, gets no edge and no site
  today.
- All three servers resolve it (`base.ts:2:3` for both spellings).
- The MCP guidance already claims that "this/super ... calls have no such
  gap". That claim is false for a cross-file base class until these sites
  exist, so `receiver_calls = "resolved"` would overclaim without them.
- Cost: one `definition` per unbound `this.`/`super.` call.

### 4.3 Re-export hops and default imports: `Reference` sites with `replaces`

GM-324 left this to GM-325 ("how the bridge expresses them"). The structural
edge exists and points at a `pending_symbol` placeholder addressed
`TargetScope::File(f)` + `TargetKey::Name(n)` (`keys.rs:118 file_target`).
core's linker cannot settle it when `f` re-exports `n` ambiguously
(`export *` ×2), or when `n` is `default`.

- **Extractor (TS):** for every `CALLS` / `REFERENCES` edge onto a
  `pending_symbol` placeholder, record `OpenSiteKind::Reference` with
  `edge_kind` = that edge's kind and `replaces = Some(edge id)` at the use's
  name token. A new `record_placeholder_use_site` beside
  `record_overload_call_sites` (`sites.rs:54-74`) does this.
- **Bridge (SDK), in `questions` (`bridge.rs:869-930`):** a `Reference` site
  with `replaces` is asked only when the replaced edge's placeholder is
  `File(f)`/`Name(n)`, `f` is in the index, and `f` declares no
  non-placeholder node named `n`. That is the Node pass's own rule
  (`semanticPass.ts:74-80`, "only those whose target file does not itself
  declare the name"). It is also skipped when an `OverloadCall` at the same
  `(from_id, position)` was kept as an overload question, because the
  overload binding is the stronger answer and two answers for one edge would
  leave two rows.
- **Answer:** the existing `Ask::Definition` arm (`bridge.rs:2258-2359`). An
  answer that lands elsewhere triggers the contradiction rule: retract the
  structural edge and record a placeholder addressed at the declaration
  (`a.ts#mutate`, `menuGroup.ts#MenuGroup`), which core links. An empty or
  ambiguous answer upholds the structural edge (R1).
- **Why not reuse `OverloadCall` for this:** python records `OverloadCall` for
  every cross-file call onto a placeholder
  (`plugins/python/src/extractor/bodies.rs:815-823`). The hop rule applied to
  that kind would change python's semantic pass, which is out of scope.
  `Reference` with `replaces` is recorded by no other plugin. Grep:
  python's `Reference` sites carry `replaces: None`
  (`bodies.rs:634`), and rust's `replaces` sites are `ReceiverCall`
  (`plugins/rust/src/extractor/bodies.rs:578-594`). So the new rule is
  TypeScript-only in effect without a manifest knob.
- **Volume:** one `definition` per hop-through-barrel use, the same set the
  Node pass asked. A named re-export that core already links
  (`twice` → `double`) is asked too, as the Node pass did. Its answer
  replaces the edge with an equivalent one.

## 5. Lifting server resolution into the SDK

### 5.1 What exists (g-mesh, project `g-mesh`, main-checkout index)

| Symbol | Callers / references (call) |
|---|---|
| python `semantic::candidates` (id `5cb156bc…`) | `resolve`, tests `a_bare_name_is_path_then_the_projects_node_modules_then_npx`, `the_npx_probe_differs_from_the_npx_server_only_in_the_bin_name`, `on_windows_every_origin_is_tried_bare_then_with_each_script_extension`, `every_candidate_is_probed_through_the_cli_twin_and_never_the_server` (`find_callers symbol_id`) |
| python `semantic::script_spellings` | `candidates` only (`find_callers` and `find_references`). **g-mesh missed** the test calls at `semantic.rs:774-800`, which `grep` found (`script_spellings_keeps_the_bare_path_and_appends_every_extension`, `..._does_not_touch_an_explicit_extension`). |
| python `semantic::probe` (id `13856d55…`) | `resolve`, tests `a_project_local_install_is_found_and_probed_for_real`, `a_probe_that_never_answers_is_killed_rather_than_waited_on`, `installed_bin_dir` (`find_callers symbol_id`) |
| rust `semantic::candidates` (id `33398653…`) | `resolve`, test `a_bare_name_keeps_its_path_lookup_first` |
| rust `semantic::probe` (id `8362feea…`) | `resolve` |
| python `resolve` / `cli_twin` / `npx_args`; rust `resolve` / `rustup_which` | `grep` in the two known files: python `prepare` (311) and tests 891, 1322; rust `engine` (90) and test 282 |

### 5.2 Proposed API: `plugins/sdk/src/lsp/resolve.rs`, re-exported from `lsp/mod.rs:82-93`

```rust
/// One spelling of "run this language server", and how to prove it is one.
pub struct Candidate {
    pub command: PathBuf,
    pub prefix_args: Vec<String>,          // before the manifest's args (npx's package)
    pub probe: (PathBuf, Vec<String>),     // `<0> <1...> --version`
    pub origin: &'static str,              // "PATH", "the project's node_modules/.bin", "npx", ...
}
pub struct Resolved { pub command: PathBuf, pub prefix_args: Vec<String>, pub version: String, pub origin: &'static str }

/// An npm-installed server: the package, the bin, and how to find the bin a
/// probe may ask (`twin`: pyright-langserver -> pyright; identity for vtsls).
pub struct NpmServer<'a> { pub package: &'a str, pub bin: &'a str, pub probe_bin: &'a str, pub twin: fn(&Path) -> PathBuf }

pub const WINDOWS_SCRIPT_EXTENSIONS: [&str; 1] = [".cmd"];
pub const HOST_SCRIPT_EXTENSIONS: &[&str];              // cfg(windows) arm, as in python today
pub const PROBE_BUDGET: Duration = Duration::from_secs(60);

pub fn is_bare(command: &Path) -> bool;
pub fn script_spellings(path: &Path, extensions: &[&str]) -> Vec<PathBuf>;            // python 467-479, verbatim
pub fn npm_candidates(command: &Path, root: &Path, npm: &NpmServer, extensions: &[&str]) -> Vec<Candidate>; // python 413-455 generalised
pub fn probe(command: &Path, args: &[String], budget: Duration) -> Result<String>;     // python 523-560 + reader 563-571
pub fn resolve(candidates: Vec<Candidate>, budget: Duration, what: &str, remedy: &str) -> Result<Resolved>; // python 379-401
```

What each plugin keeps:

- **python:** `cli_twin` (pyright's rewrite, passed as `twin`),
  `NpmServer { package: "pyright", bin: "pyright-langserver", probe_bin: "pyright", .. }`,
  `interpreter` / `set_scope` / `add_project_settings`, and its remedy text.
  `nothing_usable_names_the_remedy` keeps its exact string, so the move is
  checked by an existing test.
- **rust:** `rustup_which`. Its `candidates` becomes
  `[Candidate{command, probe: (command, [])}, rustup copy]`, and it calls the
  SDK's `resolve`. This is a behaviour change, and a safe one: rust's probe
  gains python's 60 s budget and pipe draining. Today `probe` is
  `Command::output()` with no timeout (`plugins/rust/src/semantic.rs:175-189`).
  Rust does **not** get script spellings, because rustup ships a native `.exe`
  (that file's own GM-341 correction).
- **typescript:** `NpmServer { package: "@vtsls/language-server", bin: "vtsls", probe_bin: "vtsls", twin: identity }`.

Tests that move to the SDK with the code:

- `script_spellings_*` (2);
- `a_probe_that_never_answers_is_killed_rather_than_waited_on`;
- `on_windows_every_origin_is_tried_bare_then_with_each_script_extension`,
  rewritten against `npm_candidates` with a fixed `twin`;
- a new `a_bare_name_is_path_then_node_modules_then_npx` for the generic
  order.

The pyright-specific tests stay in python: `cli_twin`, the npx bin name, and
the real local install.

## 6. The 27 red core tests

Every one of them depends on two interfaces:

- **GM-351** points core's TS spawns at the Rust binary, with `plugin.toml`
  findable (its §2, §5).
- **GM-325** keeps `semantic_pass = true` **static**: it does not depend on
  whether a server resolves (GM-351 §7).

This design keeps that. With no server, the plugin still spawns for the
cold-start pass, the factory returns `Err`, and the pass is an empty
incomplete diff.

| Test | Verdict with the tier on | What it needs |
|---|---|---|
| `plugin_check::the_typescript_plugin_satisfies_its_own_expectations_file` | green | server on PATH; `expect.toml` flips (§4.2, and `mutate` in `b.ts` untagged, §10 S6) |
| `namespace_import_after_init` (1) | green | server; `Reference` sites already recorded |
| `namespace_import_resolution` (1) | green | server |
| `overload_call_resolution` (1) | green | server; asserts `source='semantic'` (`:153`), which the bridge sets. `engine` changes from `ts-compiler` to `vtsls` and no test reads it (grep). |
| `overload_call_binding` (1) | green | server; tsserver names the bound overload (§1.4). This is also the criterion's **real-server containment test**, plus a crate-level one (§10 S6). |
| `ambiguous_reexport_linking` (1) | green **only with §4.3** | hop sites + bridge filter |
| `default_export_linking` (1) | green **only with §4.3** | same |
| `plugin_bridge::an_ambiguous_reexport_is_resolved_by_the_plugin_semantic_pass` | green only with §4.3 | asserts `source='semantic' AND resolved=1` (`:236`, `:259`) |
| `cli_reindex::reindex_against_a_deliberately_stale_index_rebuilds_it_to_match_disk` | green | waits on `plugin_pid_path`; static capability |
| `replay_progress` (2) | green | static capability. The handoff says "without a token" fails under load, so it runs 5×, once under load, in verify. |
| `cli_stop` (4), `cli_status` (2), `daemon_sigterm` (1), `idle_lifecycle` (2), `plugins_die_with_daemon` (1), `orphaned_daemon` (4) | green | plugin spawned by the cold-start pass (`cli_stop.rs:62-80` doc), server or not |
| core lib `cli::status::tests` (2): `a_pending_semantic_pass_is_named_with_its_file_count_until_it_completes`, `a_recorded_semantic_pass_failure_is_shown_with_its_reason_instead_of_the_generic_advice` | green | both read the capable set from the manifest, so `semantic_pass = true` is enough |

Also flipped back, as named in the handoff:

- `plugin_check::the_typescript_plugin_passes_on_a_small_typescript_fixture`
  (793-806), whose lazy check now skips;
- `plugins/typescript/tests/conformance.rs`'s structural-only arm (63-79,
  110-152), which becomes python's three arms: `semantic`, the 3.x
  structural manifest, and `missing_toolchain`.

Interfaces that are GM-351's, not designed here:

- `bundled_manifest()` / `typescript_manifest()` (GM-351 §5);
- `build.rs` / `build_stamp.rs` (§3);
- `test_plugin.rs` (§1);
- the version gates (§4).

GM-325 adds only `plugin.toml`'s `[plugin.semantic]`, the capability flags,
and a pin-only `package.json` (§7).

## 7. CI

The pyright precedent (`scripts/test-deps.sh:29-57`, `ci.yml:482-492`), one
for one:

- `plugins/typescript/package.json` becomes pin-only:

  ```json
  {"name": "@g-mesh/plugin-typescript-test-deps", "private": true,
   "devDependencies": {"@vtsls/language-server": "0.3.0"}}
  ```

  plus its lockfile. GM-351/S3 deletes the old `package.json` and
  `package-lock.json` (§8), and GM-325 adds these back. No `version` field:
  GM-351 §4 drops `cut-release.sh`'s TS `package.json` read.
- `scripts/test-deps.sh`: `install_typescript` (29-32) becomes
  `npm ci --prefix plugins/typescript`, then a check that
  `plugins/typescript/node_modules/.bin/vtsls --version` equals the pin.
  This is the same shape as `install_pyright`.
- `ci.yml`: a step "Install vtsls" beside "Install pyright" (482-492),
  `working-directory: plugins/typescript`, running
  `../../scripts/test-deps.sh typescript` and appending the `.bin` dir to
  `GITHUB_PATH`, with `cygpath -w` on Windows. That puts `vtsls.cmd` on
  `PATH` on the Windows runner, exercising GM-341's `.cmd` spelling for a
  second plugin. `setup-node` 22.x (398-400) stays: npm and npx need it.
  GM-351/S3 removes the plugin's `npm ci` / `npm test` steps (408-410,
  508-510).
- The kit's `tier = "semantic"` entries (`expect.toml` lines 92, 121, 369,
  382, 390, 457, plus the new `Greetable#greet`) run against that server in
  `plugins/typescript/tests/conformance.rs`'s semantic arm, and the 8 core
  semantic tests listed in §6 run against it in the core job.

## 8. Deleting the npm package and `git mv rust/ src/`

**GM-351/S3 does both, as its own mechanical commit.** GM-351's note §9
already orders it:

```
GM-351/S2 core ──┐
GM-325/S2,S3 SDK ┴─> GM-351/S3 delete npm + git mv ─> GM-325/S4a,S4b,S5 ─> GM-351/S4 tests ─> GM-325/S6 tests ─> GM-325/S7 verify ─> GM-351/S5 verify
```

GM-325 agrees, for two reasons:

- the deletion has to follow GM-351's core slice, because `core/build.rs` runs
  `npm` for `plugins/typescript` until then;
- every GM-325 TS-tier edit lands under `src/`, so it has to come after the
  move.

GM-325's only addition to that commit's result is the pin-only `package.json`
(§7). The SDK lift (S2, S3) shares no file with GM-351's core slice and runs in
parallel in its own worktree.

## 9. README gap list: how it is measured

The README's TypeScript paragraphs (`README.md:4-9`, `382-396`) and its
`## Known limits` (733) are rewritten from **measured** gaps, not inherited
ones. The measure slice:

1. **Fixture:** the kit's three arms report, per expectation, what holds with
   vtsls, with no server, and structurally. Each traced row of §1.3 that did
   not land in the index (today: the uninstalled workspace package) is a
   candidate gap entry.
2. **excalidraw** (the corpus GM-324 measured on), with vtsls on PATH:
   - per open-site kind (`ReceiverCall`, `Reference` with and without
     `replaces`, `OverloadCall`), count the questions asked, answered into the
     index, answered outside it (`node_modules` / `lib.d.ts`), empty after
     re-ask, and ambiguous. The counts come from the bridge's pass log line
     plus a `semantic` edge count by kind;
   - run a second pass and diff, to catch the false-empty exposure of §3;
   - record time to the first pass and the whole-pass wall time, with
     `uptime` and `time -p`;
   - **without** a server: confirm the one log line, the provenance block,
     and that the receiver gap stays listed.
3. The README states exactly:
   - no server installed means structural answers only;
   - vtsls is resolved from PATH, then `node_modules/.bin`, then npx (so Node
     is needed for semantics);
   - the semantic tier runs bundled TypeScript 5.9.3, not the project's;
   - each measured gap by category: uninstalled workspace packages, calls
     whose definition lands outside the index, receivers typed `any` or
     untyped JS, computed members (`obj[k]()`), and any non-zero empty-answer
     category from step 2.

## 10. Slices (revise GM-325 S2-S5)

| # | Kind | Model | Scope | Exit |
|---|---|---|---|---|
| S2 | code | opus | §5: `plugins/sdk/src/lsp/resolve.rs`; python's `resolve` / `candidates` / `script_spellings` / `probe` / `reader` move; rust's `resolve` / `candidates` / `probe` use it. Parallel with GM-351/S2, own worktree. | python, rust and sdk crate tests touched by the move pass; no copy left (`grep -n "fn script_spellings\|fn probe" plugins/`) |
| S3 | tests | opus | Moved generic tests and new `npm_candidates` order test in the SDK; controls described (revert the order → the order test fails; drop the budget → the never-answers test hangs past budget) | tests with controls |
| S4a | code | opus | SDK bridge: §4.3 hop filter and overload dedupe in `questions` | sdk crate tests touched |
| S4b | code | opus | TS tier, after GM-351/S3: `src/semantic.rs` (`engine`, `prepare`, `NpmServer`); `main.rs` passes the factory; `plugin.toml` (`semantic_pass = true`, `[plugin.semantic]`, `receiver_calls = "resolved"`); extractor: placeholder-use `Reference` sites, `this` / `super` `ReceiverCall` sites, `record_untyped_receiver_calls` | crate tests touched; one manual kit run on the fixture with vtsls |
| S5 | code | sonnet | §7: pin-only `package.json` + lock, `test-deps.sh`, `ci.yml` step | `scripts/test-deps.sh typescript` prints the pinned version |
| S6 | tests | opus | `conformance.rs` three arms; `expect.toml` flips (`Greetable#greet` semantic; `mutate` in `b.ts` untagged, since it is empty in both arms and would otherwise pass in the structural arm that requires every semantic entry to fail); fixture additions for inherited `this` / `super` (`[[callers]] Base#hello`, tier semantic); a crate real-server test of the overload containment path (`format("a")` → ordinal 0, `format(1)` → ordinal 1); hop-site unit tests; core flips in `plugin_check.rs` and `instructions/tests.rs`; `small_typescript_fixture` flip-back | tests with controls |
| S7 | verify | opus | Fresh agent. Every control built in a throwaway worktree; the 27 tests by name; the kit 3-way; `replay_progress` 5×, once under load; full suite once (`-p` sdk, python, rust, typescript, core) | every control fails; suite green |
| S8 | measure | opus | §9 steps 1-2, on the fixture and excalidraw | numbers table in `docs/results/` |
| S9 | docs | sonnet | §9 step 3: README top, the TS paragraph, `Known limits`; `plugin.toml` comments | none (docs) |

## 11. Edit map

| File | Function / site | Lines (today) | Change |
|---|---|---|---|
| `plugins/sdk/src/lsp/resolve.rs` | new | - | §5.2 |
| `plugins/sdk/src/lsp/mod.rs` | `mod` / `pub use` | 82-93 | add `mod resolve` and its `pub use` |
| `plugins/sdk/src/lsp/bridge.rs` | `questions` | 869-930 | `Reference`+`replaces` hop filter; dedupe against kept `OverloadCall` (§4.3) |
| `plugins/sdk/src/lsp/bridge.rs` | `record_answer`, `Ask::Definition` arm | 2258-2359 | none expected (contradiction and R1 rules reused); S4a confirms |
| `plugins/python/src/semantic.rs` | constants `WINDOWS_SCRIPT_EXTENSIONS` / `HOST_SCRIPT_EXTENSIONS` / `PROBE_BUDGET`; `resolve`; `candidates`; `script_spellings`; `npx_args`; `probe`; `reader` | 257-280, 379-401, 413-455, 467-479, 486-488, 523-571 | move to SDK; keep `cli_twin` (496-515) and the remedy |
| `plugins/python/src/semantic.rs` | tests | 651-856 | generic ones move (§5.2) |
| `plugins/rust/src/semantic.rs` | `resolve`, `candidates`, `probe` | 121-136, 138-157, 175-189 | use SDK `Candidate` / `resolve` / `probe` |
| `plugins/typescript/rust/main.rs` → `src/main.rs` | `main` | 10-17 | pass `Some(semantic::engine)` |
| `plugins/typescript/src/semantic.rs` (new, after the move) | `engine`, `prepare` | - | python's shape, vtsls `NpmServer` |
| `plugins/typescript/rust/extractor/sites.rs` | `record_overload_call_sites`; new `record_placeholder_use_site` | 54-74 | §4.3 sites |
| `plugins/typescript/rust/extractor/bodies.rs` | `resolve_call` | 309-356 | `This` / `Super` with no target → `record_receiver_call` |
| `plugins/typescript/rust/extractor/*` | builder setup | - | `record_untyped_receiver_calls()` (S4b finds the call site) |
| `plugins/typescript/plugin.toml` | header comment; `[plugin.capabilities]`; new `[plugin.semantic]` | 7-15, 37, 52-53 | §2, §4.2 |
| `plugins/typescript/conformance/expect.toml` | `Greetable#greet`; `mutate` in `b.ts`; new `Base#hello` | 123-157, 384-390 | §4.2, S6 |
| `plugins/typescript/conformance/project/src/` | new inherited-method files | - | S6 |
| `plugins/typescript/tests/conformance.rs` | `structural`, the test | 63-79, 110-152 | three arms, python precedent `plugins/python/tests/conformance.rs:191-507` |
| `core/tests/plugin_check.rs` | `a_namespace_import_caller_needs_the_semantic_pass_to_resolve`; `the_typescript_plugin_passes_on_a_small_typescript_fixture` | 1370-1496; 793-806 | §4.2; flip back |
| `core/src/mcp/instructions/tests.rs` | TypeScript sentence | 19-22 | §4.2 |
| `plugins/typescript/package.json` (+ lock) | new, pin-only | - | §7 |
| `scripts/test-deps.sh` | `install_typescript` | 29-32 | §7 |
| `.github/workflows/ci.yml` | new "Install vtsls" step beside "Install pyright" | 482-492 | §7 |
| `README.md` | intro; TS semantic paragraph; `Known limits` | 4-9, 382-396, 733-746 | §9 |

## 12. Risks and trade-offs

| Risk | Effect | Mitigation |
|---|---|---|
| vtsls's last release was 2025-12, and it pins TS 5.9.3 | TS 7-only syntax or library typings go unseen by the semantic tier; the project depends on one maintainer | A `command` override (typescript-language-server) works today; the native port is a follow-up (Q1); the README says the tier runs TS 5.9.3 |
| npx fallback | First use downloads 25 MB (3.9 s); core tests run on a machine without vtsls fetch once per npx cache | Parity with pyright (GM-341); CI has vtsls on PATH; README "Run tests" names `scripts/test-deps.sh typescript` |
| tsserver false-empty answers beyond 2 s settle on a big project | A missed edge on that pass | Measured in S8 (second-pass diff); `Budgets::settle` is the knob, and the same exposure applies under `indexed` |
| Every core test that spawns the TS plugin now runs a cold-start pass that starts vtsls (~0.3 s init plus project load) | More load in parallel nextest runs; `replay_progress` is already flaky under load | Verify runs it 5×, once under load; a failure is a finding, not a rerun |
| Hop questions replace edges core already links (`twice`) | Churn: the edge id changes, the target is the same | Same set the Node pass asked; kit `files` tallies would catch a duplicate row |
| `receiver_calls = "resolved"` with an interface-typed receiver binds to the interface method | `find_callers Greeter#greet` does not list `viaGreetable` | The same static-type rule `P4_STATIC` disclosed for go, rust and python; it is now disclosed for TS too. Since GM-502 the page names `Greetable#greet` in `overrides` (`gm-502-override-callers-field.md`) |
| `--version` proves only that vtsls starts, not that tsserver loads | A broken install fails at `initialize` | The bridge's start failure is the one log line plus an incomplete diff; the same as pyright |

Trade-offs taken:

- One server rather than an ordered list of three. The order would mean
  per-server args and per-server overload rules, and one of the three
  (tsls) needs a handshake probe to be honest.
- A bundled compiler over the project's own. The bundled one is
  deterministic and binds overloads, but it no longer analyses with the
  project's TypeScript version.
- `Reference` with `replaces` over a manifest knob or a new `OpenSiteKind`. It
  needs no new config, and it is TypeScript-only in effect. The cost is one
  bridge rule whose scope is defined by what plugins record rather than by a
  declared switch.

## 13. Owner questions

1. **Server: vtsls only.** typescript-language-server is a documented
   `command` override, and the native port gets a follow-up task (bridge
   hover normalisation of `(alias) `, or a tsgo that narrows `definition`).
   *Recommend yes.* It is the only traced server that binds overloads with
   no SDK change and runs whatever the project's TypeScript is.
2. **npx as the documented third candidate** (parity with pyright)? *Recommend
   yes.* The alternative, failing with the remedy text, is safer for offline
   test runs but leaves users with Node and no global install without
   semantics.
3. **`readiness = "on-demand"`?** *Recommend yes* (§3 traces; S8 measures the
   false-empty exposure).
4. **Flip `receiver_calls` to `"resolved"` and put `untypedCalls` on the
   wire together?** *Recommend yes.* That brings TypeScript into line with
   the other three plugins.
5. **`ReceiverCall` sites for unbound `this.m()` / `super.m()`?**
   *Recommend yes.* The servers resolve them, and without them "resolved"
   and the MCP guidance overclaim.
6. **Re-export hops as `Reference` sites with `replaces` plus a bridge
   filter**, rather than a manifest knob or a new site kind? *Recommend
   yes.*
7. **`implementation_kinds = []`?** *Recommend yes.* Server implementations
   are transitive (`SpecialBox`), and the structural tier already emits the
   direct ones.
8. **Engine label `vtsls` rather than the legacy `ts-compiler`?** *Recommend
   yes.* No test reads the engine; the label names what answered, as
   `pyright` and `rust-analyzer` do.
9. **The SDK lift moves rust onto the budgeted probe** (a timeout it lacks
   today)? *Recommend yes.*
10. **npm deletion + `git mv` in GM-351/S3**, with GM-325 adding back a
    pin-only `package.json`? *Recommend yes.* This matches GM-351's note §9.

## Appendix: how the facts were found

- **Traces:** `trace.mjs` and `run.sh` / `run2.sh` in the session scratchpad
  (`gm325-servers/`), 16 runs, all output JSON read in full. Server versions
  and the failure text are quoted from the servers' own replies.
- **g-mesh** (project `g-mesh`, main-checkout index):
  - `find_callers` on python `candidates` / `probe` and rust
    `candidates` / `probe` by `symbol_id`, after the bare names came back
    `ambiguous`;
  - `find_callers` / `find_references` on `semantic::script_spellings`
    (under-reported its test callers; grep found them);
  - `find_callers daemon::plugin_pid_path` (21 test files, used for §6);
  - `get_file_outline` of `plugins/sdk/src/lsp/bridge.rs`,
    `plugins/sdk/src/lsp/config.rs` and `plugins/python/src/semantic.rs`.
- **grep**, for single known files and for the TS crate, which is not
  indexed:
  - `sites.rs`, `bodies.rs`, `keys.rs`;
  - `replaces` across python and rust;
  - `ts-compiler` / `engine` in core tests;
  - `plugin.toml`, `ci.yml`, `test-deps.sh`, `README.md`.

## Resolved at review (2026-10-05)

The owner approved the note and its slicing ("хорошо, с планом согласен"):
all ten owner questions as recommended (§13). The native port gets a backlog
task.
