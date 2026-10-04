# Plugin distribution and lifecycle

## Context & Problem

g-mesh ships one archive containing core and every bundled plugin. Measured on
the 3.5.0 artifact for `x86_64-apple-darwin`:

| component | unpacked | share |
|---|---:|---:|
| `plugins/typescript` | **87 MB** | **59%** |
| `g-mesh` (core) | 36 MB | 24% |
| `plugins/go` | 8.4 MB | 6% |
| `plugins/rust` | 5.5 MB | 4% |
| `plugins/python` | 4.9 MB | 3% |
| total | **147 MB** (50 MB compressed) | |

Two things follow immediately, and both contradict the intuition that started
this design.

**The archive is one plugin.** TypeScript is 59% of it, because it is a Node
SEA: it embeds the Node runtime (the system `node` on the measuring machine is
94 MB on its own) plus the 23 MB `typescript` package. Go, Rust and Python
together are 18.8 MB - 13%. Making *plugins in general* optional buys almost
nothing; making *TypeScript* optional buys nearly everything. Any design that
treats the four symmetrically for size reasons is solving the wrong problem.

**Core is not the problem either.** 36 MB including bundled SQLite, Oniguruma
and the ONNX Runtime that `search_code` needs. It is the second-largest single
item and it is not where the weight is.

The second problem is not about size at all. Today a language whose plugin is
absent is *invisible*: `mcp::instructions` builds from
`present_languages_with_semantic_state`, which reads the languages that have
`File` nodes in the index, and a language nothing indexed has none. So an agent
asking g-mesh about a Python file in a project with no Python plugin gets an
empty answer that is indistinguishable from "there is nothing there". That is
the failure shape this project spends most of its effort eliminating, and it is
currently built in.

## Goals / Non-goals

**Goals.**
- Make the default archive small enough that a user who does not write
  TypeScript is not paying 87 MB for it.
- Make "this language has no plugin" a state the agent is *told about*, with
  the command that fixes it, rather than a silent empty result.
- Let one language's absent or broken plugin cost only that language.
- Keep install and uninstall symmetric without a config file to edit.
- Let a user fetch a plugin deliberately, with the checksum discipline
  `install.sh` already has.

**Non-goals.**
- Bundling language *servers* (rust-analyzer, pyright, a TS server). Rejected
  under Options below; rust-analyzer alone is 40 MB on disk and 535-580 MB
  resident, and each would have to be version-tracked against upstream.
- Automatic network fetches by the daemon. The daemon is spawned by an agent,
  not a human; a process that downloads and executes binaries without a person
  asking is a trust boundary this design will not cross.
- Changing what the plugins *do*. This is packaging, discovery and reporting.

## Constraints

- **The daemon has no human.** It is spawned by the agent on the first tool
  call. Nothing in the daemon path may ask a question, and any prompt belongs
  in the CLI (`g-mesh config` already hosts one).
- **Discovery is already filesystem-based.** `daemon::manifest::discover()`
  scans `plugins/*/plugin.toml` under each root in precedence order at daemon
  start, keyed by language, earlier root winning. Adding a directory adds a
  plugin; removing it removes one. There is no registry to keep in step - which
  is why uninstall needs no design of its own, only a note (below).
- **Instructions are already dynamic.** `mcp::instructions::build` assembles the
  `initialize` text per session from the languages in the index and their
  `[plugin.capabilities]`. The mechanism to tell an agent what is and is not
  covered exists; what it lacks is a way to know about a language that was
  never indexed.
- **GM-316 decided that a partial index is dangerous.** One unspawnable plugin
  fails the whole cold-start index today, deliberately: "a partial index that
  keeps serving is indistinguishable to an MCP caller from a complete one." That
  argument is sound and this design must answer it rather than ignore it.
- **The TS structural tier already uses tree-sitter** - `tree-sitter`,
  `tree-sitter-javascript`, `tree-sitter-typescript` from npm. It uses the Node
  *bindings* to the same grammars that `plugins/python` and `plugins/rust` use
  from Rust. Porting is a binding change, not a strategy change.
- **Every language already has a semantic tier.** TypeScript is not the only one
  with semantics; it is the only one that *pays for them in archive size*. Go
  needs `go` on PATH, Rust needs rust-analyzer, Python needs pyright. TypeScript
  needs nothing because it carries tsserver.

## Options Considered

### For TypeScript's weight

**A. Keep the SEA (status quo).** 87 MB, and TypeScript's semantic tier is the
only one that works with no installation at all. Costs the whole archive
problem, and keeps a class of defect this project has already been bitten by:
the embedded runtime is not the system Node, and GM-317 was exactly a
Node-20-vs-22 behavioural difference that only appeared once CI ran.

**B. Un-embed Node, keep the JavaScript plugin.** The plugin becomes roughly the
`typescript` package plus its own code - order 25 MB - and requires a system
Node. Cheapest change by far; removes two thirds of the weight. But it swaps a
known runtime for an unknown one, which is the GM-317 problem made permanent
rather than removed, and it leaves TypeScript structurally different from the
other three for no remaining reason.

**C. Port the structural tier to Rust; drive a TS language server through
`LspBridge`.** TypeScript becomes what Python is: a Rust binary on tree-sitter
(`plugins/python` is 4.9 MB) plus an external server resolved at run time. Base
weight falls to about 5 MB. It removes the embedded-runtime class of defect
entirely and makes all four languages uniform. The cost is real and must be
stated plainly: **TypeScript loses the only free semantic tier in the product.**
It is also the largest job here - the TS plugin is the oldest and most-tested
component (298 tests, and the bench numbers this project's claims rest on).

### For semantic tiers generally

**S1. Bundle every server.** Honest "all-in-one", and the archive grows rather
than shrinks: rust-analyzer 40 MB, pyright an npm tree, a TS server likewise.
Each then needs version-tracking against upstream, and rust-analyzer's 535-580
MB resident is a cost the user pays whether or not they asked. Rejected.

**S2. Bundle none; resolve all externally.** What Go, Rust and Python already
do, and what the resolve-and-degrade machinery is already built for (GM-290,
GM-299: probe candidates, log one line, answer an empty incomplete diff, keep
the gap listed). Uniform and small. TypeScript is the only language that would
lose something.

**S3. Structural always; a semantic bundle fetched per language from g-mesh's
own releases.** Considered and dropped once S2 was chosen, because it has no
content: if no server is ever bundled, there is nothing for g-mesh to host and
fetch. The only thing a user installs for semantics is the upstream server
itself, from upstream. S3 would have been a second distribution channel for
something that is not ours to distribute.

## Chosen Approach

**C + S2.** One format for every language, with no exceptions: a plugin is a
small structural binary, and **no plugin ever bundles its language server**.
Every semantic tier resolves an external program at run time and degrades
honestly when it is absent. TypeScript's structural tier is ported to Rust so it
stops being the exception; `g-mesh plugins install <language>` installs the
*plugin*, never a server.

The decision was taken on uniformity rather than on a size threshold, and the
reason is worth recording because it is not a technical one: **we do not know
the audience.** A special case for TypeScript is only justified if TypeScript
users are the majority, and nobody here has that number. Absent it, one format
that is the same for all four languages beats a format that is better for one of
them and different for the rest - and it is also the format that makes a fifth
language cheap, which is the premise `plugins/sdk` exists to defend.

Note that Go already complies: its semantic tier is compiled into the plugin but
requires the `go` toolchain on PATH, so it too resolves something external and
degrades without it. After the TypeScript port, all four behave the same way.

Projected base archive: core 36 MB + four structural plugins at roughly 5-8 MB
each ≈ **60-70 MB unpacked**, against 147 MB today; compressed, roughly 20 MB
against 50 MB.

Why C over B, given B is far cheaper: B leaves TypeScript depending on a Node it
does not control, which is the GM-317 defect made permanent. C removes the
dependency rather than relocating it, and it makes the four plugins one shape
instead of three-plus-one - which is the whole premise of `plugins/sdk` and the
reason a fifth language is supposed to be cheap. B remains available as an
interim if C's schedule is a problem; the two are not mutually exclusive, since
B is a strictly smaller step along the same path.

**The honest cost, restated so nobody discovers it later:** after C, a user with
no TypeScript language server gets structural TypeScript only - `find_callers`
on a method reached through a variable will under-report, and the instructions
will say so. Today that user gets full semantics for free. This is a real
regression for the most common language in the product's audience, and it is the
price of the archive dropping by two thirds. It should be taken only with that
sentence understood.

### The two decisions that depend on each other

Per-language degradation (below) is **only safe because** the instructions
declare coverage. GM-316's argument against a partial index - that it is
indistinguishable from a complete one - is exactly right, and the answer is not
to reject it but to remove its premise: once the agent is told which languages
are covered and which are not, a partial index is no longer indistinguishable
from a complete one. Neither change is safe without the other, and they must
land together or not at all.

## Components

```mermaid
graph TD
    subgraph "base archive (~60-70MB)"
        core["g-mesh core<br/>36MB"]
        ts["plugins/typescript<br/>structural, Rust+tree-sitter<br/>~5MB"]
        go["plugins/go<br/>8.4MB"]
        rs["plugins/rust<br/>5.5MB"]
        py["plugins/python<br/>4.9MB"]
    end

    subgraph "resolved at run time, never bundled"
        tsserver["a TS language server"]
        gotool["go on PATH"]
        ra["rust-analyzer"]
        pyright["pyright"]
    end

    subgraph "core's own knowledge"
        cat["language catalogue<br/>extension -> language id<br/>+ install command"]
    end

    core -->|"discover() scans<br/>plugins/*/plugin.toml"| ts & go & rs & py
    ts -.->|LspBridge| tsserver
    go -.-> gotool
    rs -.-> ra
    py -.-> pyright
    core -->|"names a language it has<br/>no plugin for"| cat
    cat -->|"feeds the<br/>absent-plugin state"| instr["mcp::instructions::build"]
```

**The language catalogue is the one genuinely new component.** Core must be able
to name a language it has *no plugin for* - otherwise the absent-plugin state
cannot exist, because the index has no files for it and discovery has no
manifest. The catalogue maps file extension to language id and carries the
install command for each. It ships with core, not with plugins.

This is a second place that knows about languages, and that deserves
justification rather than a shrug: a manifest describes a plugin that is
*present*, and this describes one that is *absent*. Nothing that ships with a
plugin can answer a question about the plugin not being there. The catalogue
must be kept deliberately thin - extension, id, install command - so that it
cannot drift into a second source of truth about capabilities, which remain the
manifest's alone.

## Data Flow

```mermaid
sequenceDiagram
    participant A as agent
    participant D as daemon
    participant W as walk
    participant I as instructions

    A->>D: first tool call in a project
    D->>D: discover() -> plugins present on disk
    D->>W: cold-start index
    W->>W: for each discovered language: walk and index
    W->>W: count files matching CATALOGUE extensions<br/>with no discovered plugin
    W-->>D: per-language outcome: indexed / absent / failed
    D-->>A: (later) initialize
    A->>I: get_info
    I->>I: languages in index + capabilities<br/>+ absent-but-present-on-disk
    I-->>A: "python: 412 files, no plugin installed.<br/>Run `g-mesh plugins install python`."
```

Correction (GM-329/S1, from the source): core does not walk the project today -
each plugin walks in its own `--bulk-index` process - so counting an absent
language's files is a new, daemon-side walk, not a by-product. It runs only when
some catalogue language has no plugin, counts only those languages, and is
measured before it ships; see [ADR 0021](../adr/0021-per-language-bulk-outcome.md)
sections 3-4.

## Interfaces

### `LanguageOutcome` - the per-language result of a cold-start index

```rust
enum LanguageOutcome {
    /// A plugin was discovered and indexed this language.
    Indexed { files: usize },
    /// The catalogue names this language and the project has files for it,
    /// but no plugin was discovered. NOT an error. `None`: files seen, not
    /// counted (ADR 0021's deadline fallback). The install command is derived
    /// from the catalogue (`CatalogueEntry::install_command`), not stored.
    PluginAbsent { files: Option<usize> },
    /// A plugin was discovered and could not be used. An error for this
    /// language, and only for this language.
    Failed { error: String },
}
```

`bulk_index::run` returns one of these per language instead of aborting on the
first failure. The whole index fails only when *every* discovered plugin failed,
which is the case that means something is wrong with the installation rather
than with one language. Decided, with the store GM-330 reads
(`schema::language_outcomes`), in [ADR 0021](../adr/0021-per-language-bulk-outcome.md).

### Instructions: three states, not two

`instructions::build` gains the absent case. The existing input describes
languages that are present; it must also carry those the catalogue names and the
project has files for:

```rust
struct AbsentLanguage {
    language: String,
    files: usize,
    /// The exact command, e.g. "g-mesh plugins install python".
    install: String,
}
```

The rendered contract, per language, is one of:
- **supported and working** - today's text, including the receiver-call gap
  sentence when the semantic tier has not run;
- **supported, plugin absent** - names the language, the file count, and the
  install command, and says plainly that g-mesh has no answers for this language
  until then, so the agent does not read an empty result as "nothing found";
- **not supported** - said once, generically, rather than per language; the
  catalogue is not a promise of future support.

This is the half of the design that makes a static line in `CLAUDE.md` safe. A
manifest that tells an agent "g-mesh handles Python" is *actively harmful* while
the plugin is absent, unless the instructions can correct it at run time. They
can - but only once this contract exists.

### CLI

```
g-mesh plugins list                 # exists
g-mesh plugins check <dir>          # exists
g-mesh plugins install <language>   # new: fetch from the GitHub release
g-mesh plugins install --from <path>   # new: from a local file
g-mesh plugins remove <language>    # new: delete the directory
```

`install` reuses `install.sh`'s checksum discipline (already 21 lines of it) and
verifies before unpacking. `remove` deletes `plugins/<language>/` and nothing
else - there is no config to edit, which is the whole benefit of
filesystem-based discovery.

**Both require a daemon restart to take effect**, because `discover()` runs once
at startup. This should be stated by the command rather than discovered by the
user; whether the daemon should instead re-discover on change is left open
below.

## Failure Modes & Edge Cases

- **Zero plugins installed.** A valid state, not an error: the index is empty
  and the instructions say g-mesh currently covers nothing and name the install
  command. This is what makes "ship without plugins" possible at all, and it is
  precisely what today's code cannot express.
- **A plugin is present but its binary is missing.** `Failed` for that language.
  GM-316's actionable message already names the cause and the fix; it now
  reaches the agent as well as the log, and does not take the other three
  languages with it.
- **Every discovered plugin fails.** The whole index fails, as today. Something
  is wrong with the installation rather than with one language.
- **The catalogue names a language whose plugin exists but is older than the
  catalogue entry.** The manifest wins on capabilities; the catalogue is only
  consulted for languages with no manifest at all.
- **A plugin is removed while the daemon runs.** Discovery is read at startup,
  so the daemon keeps using what it found. The index is not corrupted - the
  plugin simply keeps being spawned from a path that no longer exists, which
  degrades to `Failed` for that language on the next spawn.
- **Downloaded plugin fails its checksum.** Refuse, delete the partial file, and
  say which release and which digest were expected. Never unpack on a mismatch.

## Open Questions / Risks

- **Should `LspBridge` live in core rather than in the SDK?** Raised while
  asking why the Go plugin cannot use it. The barrier is not Go the language
  but Go the *implementation language of that plugin*: `LspBridge` is a Rust
  struct in `plugins/sdk`, and a separate Go module cannot import a Rust
  crate. Moving it into core would sever the link between a plugin's
  implementation language and whether it can have a semantic tier at all.

  What makes this more plausible than it first sounds is that nearly
  everything the bridge needs already crosses the plugin/core boundary: open
  sites are on the wire, `OpenSite::replaces` is on the wire, `native_kind`
  (which decides what is implementable) is on the wire, `[plugin.semantic]`
  is in a manifest core already reads, and core holds the full graph with
  positions plus the file contents the watcher maintains - which is exactly
  what mapping an LSP `Location` back to a node requires. The bridge sits in
  the SDK for historical reasons rather than because that is where its inputs
  are.

  The benefit is not fixing Go. It is removing one implementation of
  readiness, budgets, retraction and deferral per SDK - rules this project has
  already corrected four times against real servers (GM-289, GM-290, GM-309,
  GM-310), and a second copy would inherit none of those corrections, only
  their absence. It would also shrink a plugin to pure structural extraction,
  which is the "language N+1 is cheap" premise carried further than it
  currently goes.

  A separate sidecar process - speaking our plugin protocol on one side and
  LSP on the other - was considered and is worse: a third process per
  language, and it would still need the index to map an answer onto a node,
  which core is better placed to hold than a proxy is.

  **This would not change Go**, and that is worth stating so the two questions
  are not conflated. Go declines the bridge on merit, not on language: its
  semantics come from `go/types` through `packages.Load`, a batch API suited
  to walking a whole project, where `gopls` is built for interactive editing.
  Moving the bridge would give Go the *option*; taking it would mean trading a
  fitter tool for a less fit one and adding an external dependency, since a Go
  developer has `go` by definition and `gopls` is a separate install.

  Deliberately NOT scheduled. It touches the wire contract and the path this
  project has repaired four times, and doing that in the middle of the
  TypeScript port would put two large changes on the same code at once. After
  4.0.0 there is exactly one non-Rust plugin left, so the cost of waiting is
  low - which is the argument for writing it down now and deciding later.

- ~~Is TypeScript's free semantic tier worth 87 MB?~~ **Decided: no.** Not on a
  size threshold but on uniformity - we do not know the audience, and a special
  case is only defensible with a number nobody has. Recorded under Chosen
  Approach. The consequence stands and is not softened: TypeScript users who
  install no language server get structural answers only.
- **Should the daemon re-discover plugins without a restart?** It would make
  install and remove take effect immediately, at the cost of a filesystem watch
  on the plugin roots and a re-entrancy question during an in-flight pass. Left
  out of this design deliberately; `plugins install` printing "restart the
  daemon" is honest and costs one line.
- **How much of the TS test suite survives the port?** 298 tests encode
  behaviour that the bench results depend on. The port is only safe if the
  conformance fixture and `expect.toml` carry that behaviour across, and that
  should be established before the port starts, not after.
- **Where does the catalogue's install command come from for a language g-mesh
  does not yet support?** Saying "not supported" generically avoids promising
  anything, but a user with a Kotlin project gets no signal at all. That may be
  correct; it is not obviously correct.
- **Windows.** Every size figure here is macOS. The SEA's weight in particular
  may differ, and nothing in this design has been measured on Windows - where,
  as of this writing, four tests still fail for unrelated reasons (GM-322).
