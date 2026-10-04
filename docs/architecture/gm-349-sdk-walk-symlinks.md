# GM-349: the SDK walk follows symlinks, behind a guard

Design note (slice S1). No production code. Inputs read: `plugins/sdk/src/walk.rs`,
`plugins/typescript/src/symlinks.ts` + `bulkIndex.ts` `walkDir` + `workspace.ts`
`subdirectoryLister`, `plugins/go/symlinks.go` + `walk.go` (Go already has its own
port of the TS guard), `core/src/project_walk.rs`, `core/src/watcher/mod.rs`,
`core/src/daemon/mod.rs` `watch_and_route_once`, `plugins/sdk/src/run.rs`
(`bulk_index`, `Session::hydrate`, `Session::file_changed`), notify 6.1.1 and
ignore 0.4.31 sources, `docs/architecture/gm-323-ts-port-inventory.md`.

## 0. Findings that change the premise

1. **Yarn/pnpm workspaces do not need link following to be indexed.** Their
   packages live in-tree (`packages/*`); the links are
   `node_modules/@scope/pkg -> ../../packages/pkg`, and every walk (TS, Go,
   SDK) drops `node_modules` *by name before the guard runs*
   (`bulkIndex.ts` `walkDir`: "By name, before anything is resolved"). Those
   links are never consulted today. The brief's "real source reachable only via
   a link" does not describe a pnpm workspace.
2. **With links outside the root refused (TS's rule, kept below), following a
   link adds coverage in exactly one case: an in-root target the plain walk
   does not enter** - a gitignored directory (generated code, a vendored
   checkout listed in `.gitignore`). Every other in-root link is an *alias* of
   files the plain walk already reaches; following it only changes which
   spelling wins. So the acceptance fixture "reachable only through a symlinked
   directory" is necessarily a gitignored target (section 7, B1).
3. **The TS/Go guard has two bugs, reproduced on the Go copy** (scratch probe
   against `plugins/go`, not committed; TS has the same statement order, not
   run):
   - *Claim before ignore.* The guard claims an entry's real path before the
     caller checks `.gitignore`. A gitignored target that sorts before its
     link (`real-src/` ignored, `src/linked -> ../real-src`) is claimed while
     being skipped, and the link is then refused as a duplicate: Go walk
     returns `[]`. Rename the target to `z-src/` and it returns
     `[src/linked/pkg.go]`. The one case link following exists for depends on
     sibling names.
   - *As-reached identity under an alias.* A non-link entry is claimed by its
     own path, "real by induction" - false below a followed link. `a -> zlib`,
     `b -> zlib/sub`: Go walk returns `[a/sub/x.go, b/x.go]`, one file twice.
4. **Alias-wins breaks the watcher and core's walk.** TS and Go index a file
   reached two ways under whichever spelling sorts first, which can be the
   link (TS tests 467/493, Go `TestWalkProjectFilesFollowsSymlinkedDirectoryOnce`).
   Core's walk (`project_walk::project_files`) never follows links, so it sees
   the real spelling; `g-mesh status` then counts a file the index holds under
   another name. And `Session::file_changed` indexes any claimed-extension path
   it is handed without consulting the walk, so an event spelled the other way
   inserts a second copy (section 2.3).

## 1. Decisions

### D1. Always on, no flag

Every SDK plugin follows links, with the guard. Reasons:
- `walk.rs`'s own argument for not following was "the option with no failure
  modes". With the guard below the failure modes (cycle, double index, escape,
  dangling) are closed, so that argument is spent; the doc already said a
  plugin needing links should "get the guard rather than a flag".
- TS and Go already follow. A per-plugin flag makes the project tree depend on
  which language is asking, and core's single walk cannot mirror a per-language
  choice.
- Cost on a link-free tree is nil by construction (ignore's `follow_links`
  stats only symlink entries; the guard's lookups are skipped while no link
  has been accepted). Measured in section 5.

Risk: Python and Rust projects that have links today gain files. Every gain is
either a gitignored target (intended) or nothing (aliases collapse onto the
real spelling).

### D2. "Escaping the root": refused, and a target under an excluded directory is refused too

A link is followed only if its target's real path is inside the canonicalized
root **and** no component of the target's root-relative real path is in
`BASELINE_EXCLUDED_DIRS` or the manifest's `exclude_dirs`.

Outside the root, re-examined rather than inherited. The case it would serve
is real: a sub-project opened as the root that links a sibling
(`apps/web/shared -> ../../libs/shared`). Refused anyway, because:
- **Unbounded cost.** `bazel-out`/`bazel-<ws>` link into `~/.cache/bazel`
  (the whole execroot with external deps), nix `result` into `/nix/store`, a
  stray link into `$HOME` or `/`. In-root is the only boundary that bounds the
  walk by the project.
- **Silent staleness.** core watches the root only. FSEvents (macOS) and
  ReadDirectoryChangesW (Windows) do not follow links, so an edit in an
  outside target never reaches core; only inotify (notify 6.1.1
  `inotify.rs:376` walks with `follow_links(true)`) would see it. A file
  indexed once and never updated is worse than one never indexed.
- A narrower boundary ("inside the enclosing git work tree") would serve the
  sibling case and exclude bazel/nix/home, but the staleness stays until core's
  watcher also watches link targets. Owner question Q3.

The excluded-target rule is new (TS follows `src/dep -> ../node_modules/foo`).
Reasons: `exclude_dirs` means "never source in this ecosystem"; a link into
`.claude/` would index a whole session copy of the project; and core's
registry drops events under a language's `exclude_dirs`
(`registry.rs` `under_excluded_dir`), so such files could never be refreshed.
A gitignored target is *not* refused: gitignore is a VCS statement, not a
"not source" one (generated sources), and it is the one case the feature
exists for.

### D3. A file reachable two ways gets its real path when the plain walk reaches it ("real wins")

Rule: group walked files by real path. If one spelling contains no followed
link (the plain walk reached it), that spelling is the file's identity.
Otherwise (target only reachable through links) the first spelling in walk
order wins. Sibling names never decide the identity of a file the plain walk
reaches.

Why not TS's sorted-first (alias can win):
- **Core's walk agrees.** `project_walk` (status coverage, absent-file count)
  does not follow links and keeps doing so (D4). Under real-wins every file
  core sees is indexed under the same spelling; its module doc's "diverges
  only toward doing less" stays true. Under alias-wins `status` reports the
  real spelling as unindexed forever.
- **Events agree.** `watch_and_route_once` routes the path the OS reports.
  FSEvents reports real paths (to be confirmed by B9), so an edit to a file
  indexed under its alias arrives as the real spelling and
  `Session::file_changed` (`run.rs:528`) indexes it as a new file: a duplicate
  plus a stale alias. Real-wins makes the reported path the indexed one.
- **Semantic tiers agree.** tsserver (default `preserveSymlinks: false`),
  pyright and the LSP bridge (`bridge.rs` `with_budgets` computes `real_root`)
  answer in real paths; `semanticPass.ts` `indexedPathOf` maps a definition
  location to an index path by stripping the root, so a definition in an
  alias-indexed file maps to a path the index does not hold.
- **Stability.** Renaming `packages/` to `zpackages/` would rename every id
  under an aliased package under alias-wins.

What it costs: TS tests 467 and 493, workspace 291, and Go's
`TestWalkProjectFilesFollowsSymlinkedDirectoryOnce` change expected values
(the real spelling instead of the alias). That contradicts the acceptance
line "today's TS behaviour is preserved, not approximated" for those four
assertions; everything else is preserved. **Owner question Q1, blocking S2.**
If the owner keeps alias-wins, the guard is section 2 with the winner rule
swapped, and the watcher/status mismatches above become known gaps.

Import resolution consequence (for GM-324's port): a relative import written
through a link (`../packages/dup/thing`) must be resolved through the walk's
link table to the indexed spelling (`vendor/shared/thing.ts`); a workspace
package matched by a glob at a link maps to the indexed spelling of its
directory. The SDK exposes that table (section 2.2).

### D4. Core's walker does not follow links; no change to core's watcher

`core/src/project_walk.rs` stays non-following: core does not depend on the
SDK (it shares only `g-mesh-wire`), and under D3 its result is the plugin
walk minus the gitignored-target files, which is "doing less". Its module doc
gains one sentence naming that difference. Core's watcher is untouched.
Consequences, documented, not fixed here:
- alias-only files (gitignored target) are invisible to `g-mesh status`;
- an edit to an alias-only file arrives (macOS) as the real spelling, which
  core's watcher drops when the root `.gitignore` covers it: such files refresh
  only on the next bulk index. A follow-up (core learns the link table) is
  owner question Q4.

### D5. Windows

- `std::fs::canonicalize` returns the verbatim spelling (`\\?\C:\...`). Both
  sides of every comparison are canonicalized (root once, each link target), so
  prefix tests hold without `without_verbatim_prefix`. Emitted paths stay
  as-reached relative to the *given* root, unchanged from today.
- Junctions: Rust's `FileType::is_symlink` is true for name-surrogate reparse
  points, which includes junctions, and walkdir follows them through
  `metadata`. Believed, not verified: B10 checks it on CI.
- Testable on CI: `windows-2022` runners run elevated, so
  `std::os::windows::fs::symlink_dir`/`symlink_file` work; junctions via
  `cmd /C mklink /J`. Links are created by the test at run time, never
  committed (git on Windows checks a committed symlink out as a text file;
  the repo has three committed links under `eval/`, none in fixtures).
- Case-insensitive volumes: `canonicalize` returns on-disk case on both
  macOS and Windows; comparisons are only ever between canonical spellings.

### D6. `node_modules` and workspace links

No interaction to design: the name check runs before the guard (ignore's
`should_skip_entry` precedes `filter_entry`, `walk.rs` 1160 vs 1175 in
ignore 0.4.31), so a link *named* `node_modules`, and everything inside a real
`node_modules`, is never resolved. D2 adds the converse: a link *into*
`node_modules` is refused. Workspace packages are walked at their real
in-tree location (finding 1).

## 2. The guard's shape

### 2.1 Walk algorithm (one walk, then a winner pass)

1. `root_real = canonicalize(root)` (fallback: `root` itself). Emitted paths
   remain relative to `root` as given.
2. One `ignore::WalkBuilder` as today, with `follow_links(true)`. walkdir's
   own ancestor-loop detection yields `Err(Loop)` entries, which the existing
   `let Ok(entry) else continue` drops; the guard is the primary defence and
   the loop check a backstop.
3. `filter_entry`, called after gitignore and the name excludes (this ordering
   is what fixes the claim-before-ignore bug), for each entry:
   - not a symlink, directory: record it entered (its real path is
     `real_of(parent)` + name); never refused for being already entered
     (a directly-reached directory always walks, which is what lets real-wins
     see the real spelling).
   - symlink: `canonicalize`; refuse if dangling, outside `root_real`, under an
     excluded name (D2), or (directory) its real path already entered. Else
     accept, and record `as_reached -> real` in the link table.
4. `real_of(path)`: nearest accepted-link ancestor's real path + the suffix;
   with an empty link table it is `root_real` + the relative path, no lookup.
5. Winner pass over the walked files: key = real path; winner = the spelling
   with no accepted link among its ancestors, else the first in walk order.
   Output = winners, sorted by component-wise `Path` order (the walk order;
   also what `walk_project`'s "sorted" doc promises).

Bounds (corrected at verify: nested links can walk one real directory more than twice; each link is still entered once): a real directory is walked at most twice (once through the first link
that reaches it before the plain walk does, once directly); a second link
onto an entered directory is refused. Files are extracted once - dedupe
happens before `walk_project` returns.

State lives in an `Arc<Mutex<..>>` because `filter_entry` must be
`Fn + Send + Sync + 'static`; the lock is taken only for directory and symlink
entries.

### 2.2 Public surface (SDK)

- `walk_project(root, extensions, exclude_dirs) -> Vec<RelPath>`: unchanged
  signature, now the winners.
- New `walk_project_detailed(..) -> WalkedProject { files: Vec<RelPath>,
  links: Vec<WalkedLink> }`, `WalkedLink { at: RelPath, outcome }` with
  `outcome` one of `Aliases(RelPath)` (target reached directly; its indexed
  spelling), `Followed`, `Duplicate(RelPath)`, `Refused(Dangling | OutsideRoot |
  ExcludedTarget)`. Consumers: GM-324's workspace expansion and import
  resolver (D3), the measure slice (counts per outcome), and Q4.
- `walk_scope`: unchanged policy; it keeps listing only real directories as
  pruned. It uses the same `walker()`, so `entered` will now include accepted
  link directories; that is harmless (their ignored children get listed under
  the alias spelling) and is pinned by B12.

### 2.3 Session event remap (recommended inside GM-349; Q2)

`Session::file_changed(p)`: if `canonicalize(root/p)` is inside `root_real`,
differs from `root_real/p`, and the index holds an entry for that real
spelling, handle the event as that spelling. One syscall per event. This
covers an event spelled through a link (inotify shares one watch descriptor
per inode across both spellings, so notify may report either; to be confirmed
by B9 on Linux CI). It does not cover the alias-only case of D4.

## 3. TS test -> SDK test mapping

Expected values for SDK tests under D3. "Same" means the TS assertion carries
over unchanged.

| TS test (file:line) | SDK test (in `walk.rs` tests) | Expected |
|---|---|---|
| bulkIndex 467 symlinked package dir indexed under its apparent path | `a_link_to_a_walked_directory_adds_nothing_and_the_real_spelling_wins` | **changed**: `vendor/real-lib/index.ts` |
| bulkIndex 493 two paths onto one real dir, sorted-first wins even if link | same test, second tree (`packages/dup`) | **changed**: `vendor/shared/thing.ts` |
| bulkIndex 505 symlinked file aliasing a walked file is skipped | `a_file_link_to_a_walked_file_is_indexed_once` | same, plus the reversed name (`alias.ts` before `index.ts`) also gives `src/index.ts` |
| bulkIndex 525 link to its own containing dir does not loop | `a_link_to_an_ancestor_terminates_and_adds_nothing` | same (`cycle/a.ts`); plus link to the root itself |
| bulkIndex 541 link outside the root refused | `a_link_outside_the_root_is_refused` | same |
| bulkIndex 558 dangling link skipped | `a_dangling_link_is_skipped` | same |
| workspace 291 glob matches a symlinked package dir under the matched path | GM-324 workspace test over `WalkedLink::Aliases` | **changed**: package dir `vendor/math` |
| workspace 310 cycle under `**` neither hangs nor invents | `a_link_to_an_ancestor_terminates_and_adds_nothing` + GM-324 | same |
| workspace 335 real dir and alias collapse, sorted-first | `a_link_to_a_walked_directory_adds_nothing_and_the_real_spelling_wins` | same (`original` is real) |
| workspace 350 glob symlink outside root refused | `a_link_outside_the_root_is_refused` | same |
| workspace 372 dangling link under a glob skipped | `a_dangling_link_is_skipped` | same |

The workspace rows need the TS crate (GM-324); GM-349 supplies the walk-level
test and `WalkedProject.links`. New SDK tests with no TS counterpart are the
B-numbers in section 7 (B1, B2, B4, B5, B8).

## 4. Edit map (S2, code)

Change:
- `plugins/sdk/src/walk.rs`
  - module doc 1-46: rewrite "The policy, exactly" last bullet and 37-46
    (links followed under the guard; real-wins; refusals). Invariants only,
    decisions go to the ADR.
  - `walk_project` 67-96: winner pass; delegate to `walk_project_detailed`.
  - `walk_scope` doc 118-125 (the "a symlink is never listed (the walk does
    not follow one)" sentence) and 126-163 only if B12 shows a change.
  - `walker` 179-202: `follow_links(true)`, guard in `filter_entry`; return
    the guard state alongside the builder.
  - new private `LinkGuard` (root_real, excluded names, entered dirs, link
    table) + `WalkedProject`/`WalkedLink` (public, re-exported in
    `plugins/sdk/src/lib.rs:104`).
- `plugins/sdk/src/run.rs` `Session::file_changed` 528-570: the remap of
  section 2.3 (if Q2 = yes). A separate code slice is fine.
- `core/src/project_walk.rs` module doc 10-14: one sentence (D4). Doc only.
- Go (separate slice, Go toolchain): `plugins/go/symlinks.go`
  `symlinkGuard.resolve` 72-108 and `plugins/go/walk.go` `walkDir` 36-89 -
  claim after the ignore check, real-path identity under a link, D2's
  excluded-target refusal, D3's winner rule; update
  `TestWalkProjectFilesFollowsSymlinkedDirectoryOnce` (walk_test.go 52).
- Node TS plugin: `symlinks.ts` `createSymlinkGuard` and `bulkIndex.ts`
  `walkDir` have the same two bugs. Fix or leave to GM-324: Q5.

Read (callers that must keep working; no change expected):
`run.rs` `bulk_index` 166 and `Session::hydrate` 501;
`plugins/python/src/project/mod.rs` `ProjectContext::load` 416;
`plugins/python/src/semantic.rs` `add_project_settings` 328 (walk_scope);
`plugins/rust/tests/semantic_pass_measurement.rs` `index_corpus` 53;
census tests in `plugins/{python,rust}/src/census.rs`.

## 5. Measurement plan (measure slice)

- **Corpora.** ripgrep (Rust), py-requests (Python), gin (Go) from
  `../g-mesh-bench/corpora`; none has a single symlink outside `.git`
  (counted: 0 in all five corpora). Plus excalidraw after `yarn install`
  (real workspace links, all under `node_modules`).
- **Arms.** A = merge-base of this branch (no following); B = branch tip.
  Same binary build profile, same machine, warm cache.
- **Numbers.**
  1. `walk_project` wall time, median of 20 runs, per corpus (link-free
     overhead; expected within noise). Driven by a small bench binary or a
     `#[ignore]` test calling `walk_project`, not by a full bulk index.
  2. Worst case: ripgrep plus one link `aaa -> crates` (sorts before the
     target, so `crates/` is walked twice): walk time and the file count
     (must equal arm A's count: real-wins dedupe).
  3. End-to-end `g-mesh init` wall/user/sys on ripgrep and py-requests, A vs B.
  Record `uptime` and `/usr/bin/time -p` user/sys/real for every run.
- **Control that tells the arms apart.** A tree with a gitignored target and
  one link: B returns the alias file, A does not. Run it on the exact two
  binaries measured, before the timing runs.

## 6. Risks and trade-offs

- Real-wins changes four TS/Go assertions (Q1).
- Alias-only files are not refreshed by the watcher on macOS (D4/Q4) and not
  counted by `status`.
- A file under an alias can be gitignored under one spelling and not the
  other (as-reached `.gitignore` layers apply, as in TS); the winner is chosen
  among spellings that survived their own layers.
- pyright is told `walk_scope`'s excludes; a refused outside-root link is not
  among them (unchanged from today), so pyright may still read it.
- `Arc<Mutex<..>>` per directory entry: negligible, measured by 5.1.
- Two copies of the policy remain (SDK Rust, Go) until a shared spec exists;
  the test names in section 3 should match in Go so drift is visible.

## 7. Behaviours for the tests slice (each with its control)

Unix tests create links with `std::os::unix::fs::symlink`; Windows variants
use `symlink_dir`/`symlink_file`. Controls are built in a throwaway worktree
by reverting the named code, never the test.

| # | Behaviour | Control (revert -> test fails) |
|---|---|---|
| B1 | Gitignored target reached only via a link is walked under the link spelling, **for both sibling orders** (`real-src` and `z-src`) | set `follow_links(false)` -> no alias file; move the claim before the ignore check -> the `real-src` order returns `[]` |
| B2 | Real-wins: link sorting before its walked target yields only the real spelling | swap the winner rule to first-in-walk-order -> alias spelling returned |
| B3 | File link to a walked file: indexed once, both name orders | drop the winner pass -> two entries |
| B4 | Nested aliases (`a -> zlib`, `b -> zlib/sub`, `zlib` ignored): `x` indexed once | identity by as-reached path instead of `real_of` -> `[a/sub/x, b/x]` |
| B5 | Link to an ancestor and link to the root terminate and add nothing | remove the entered-dir refusal (keep walkdir's loop check) -> duplicate files; remove both -> hang (test under a timeout thread) |
| B6 | Link outside the root refused | remove the prefix check -> outside file listed |
| B7 | Dangling link skipped, walk continues | `unwrap` the canonicalize -> panic |
| B8 | Link into an excluded dir (`node_modules`, `.claude`) refused; a link *named* `node_modules` is never resolved | remove the excluded-target check -> file listed |
| B9 | (core integration, macOS and Linux CI) edit the real file of an aliased pair: exactly one file row after the event, under the real spelling | remove the 2.3 remap -> Linux may show the alias row; on macOS confirms FSEvents reports the real path |
| B10 | Windows: junction to an in-root ignored dir followed; junction to an ancestor terminates | `follow_links(false)` -> no file |
| B11 | Root given through a link (`/tmp` vs `/private/tmp`; temp dir on macOS): B2/B5 still hold and emitted paths are relative to the given root | canonicalize only one side -> B5 duplicates |
| B12 | `walk_scope` on a tree with an accepted link: `pruned` lists no link and no refused target | n/a (pins current output; compare with arm A) |
| B13 | **Acceptance fixture**: `core/tests/` test running real `g-mesh init` (pattern: `core/tests/one_plugin_binary_missing.rs`) on a tree with `.ts/.py/.rs/.go` files in a gitignored `gen/` linked as `src/gen`: every language's file is in the index under `src/gen/...` | `follow_links(false)` in the SDK -> py/rs rows missing; Go/TS controls per their own guard |

## 8. ADR

Propose **ADR 0024 "Project walks follow symlinks; a file's identity is its
real spelling when the plain walk reaches it"**: D1-D3 and the D4 gap.
Not written in this slice.

## 9. Owner questions

1. **Real-wins (D3) instead of TS's sorted-first?** It changes TS 467, 493,
   workspace 291 and Go's `...FollowsSymlinkedDirectoryOnce`, against the
   "preserved, not approximated" criterion. Recommendation: yes - alias-wins
   disagrees with core's walk, the watcher and every language server.
   Blocks S2.
2. **Include the `file_changed` remap (2.3) in GM-349?** Recommendation: yes,
   as its own small code slice; it is what keeps "indexed once" true after
   the first edit.
3. **Outside-root links stay refused?** Recommendation: yes now; a
   "within the enclosing git work tree" boundary is a follow-up that only makes
   sense together with Q4.
4. **Follow-up task: core learns the link table** (watcher remaps real ->
   alias for alias-only files; `status` counts them). Recommendation: create
   it in backlog, not in this release.
5. **Node TS plugin**: fix its two guard bugs now, or let GM-324's port
   inherit the SDK walk? Recommendation: leave Node to GM-324 if it ships in
   the same release; otherwise a minimal reorder in `symlinks.ts`/`walkDir`.
   B13's TS arm depends on this answer.
6. **Acceptance fixture = gitignored target (finding 2)?** With outside-root
   refused, it is the only "reachable only through a link" shape.
   Recommendation: accept, and reword the criterion's pnpm motivation.

### Resolved at review (2026-10-04)

The owner accepted all six recommendations: real-wins (D3), replacing the four
TS/Go assertions that encoded alias-wins; the `file_changed` remap (2.3) is in
this task as its own code slice; outside-root links stay refused; core learning
the link table is a backlog task; the Node TypeScript plugin's two guard bugs
are left to GM-324, which moves it onto this walk in the same release (so
B13's TypeScript arm is held by GM-324); the acceptance fixture is a
gitignored target reached through a link. The ADR proposed in section 8 takes
number 0025, since GM-348 writes 0024.

## Appendix: g-mesh calls this note relied on

| Question | Call | Answer |
|---|---|---|
| Who calls the SDK walk | `find_callers walk_project` | `run.rs` `bulk_index`, `Session::hydrate`; `python/src/project/mod.rs` `ProjectContext::load`; `python`/`rust` `census.rs` tests; `rust/tests/semantic_pass_measurement.rs` `index_corpus`; walk.rs tests. Complete (`hasMore: false`) |
| Who calls `walk_scope` | `find_callers walk_scope` | `python/src/semantic.rs` `add_project_settings`; walk.rs tests |
| Who calls core's walker | `find_callers project_files` | `languages::count_absent_files_in`, `cli::status::discover_source_files` |
| `project_walk` shape | `get_file_outline core/src/project_walk.rs` | `project_files` 34-64, `relative_wire_path` |
| Watcher consumer | `find_callers next_change` (the qualified `ProjectWatcher.next_change` returned only a semantic neighbour; the bare name resolved) | `daemon::watch_and_route_once` |
| Event routing | `find_definition watch_and_route_once` | routes the OS-reported path, root-relative |
| status's use | `find_callers discover_source_files`, `count_absent_files_in` | `index_status`; `count_absent_files` |

grep was used for: non-code (manifests, Cargo.lock, inventory), the notify and
ignore crate sources, `follow_links`/`realpath` spellings, and corpus symlink
counts (`find -type l`).
