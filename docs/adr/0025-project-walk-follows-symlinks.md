# 0025. Project walks follow symlinks; a file's identity is its real spelling when the plain walk reaches it

## Status
Accepted 2026-10-04 (GM-349, owner review). Reasoning, the guard's algorithm,
the bugs found in the TS/Go guard and the test plan:
[`gm-349-sdk-walk-symlinks.md`](../architecture/gm-349-sdk-walk-symlinks.md).

## Context
The SDK walk (`plugins/sdk/src/walk.rs`) did not follow symlinks, on the
argument that not following has no failure modes. The TypeScript and Go
plugins do follow them, behind a guard against cycles, double indexing and
links escaping the root, so the project tree depended on which language was
asking. GM-324 moves TypeScript onto the SDK walk, which would have dropped
link following for it.

Two findings shaped the decision. First, workspace links
(`node_modules/@scope/pkg -> ../../packages/pkg`) never need following: every
walk drops `node_modules` by name before any link is resolved, and the
packages are walked at their real in-tree location. With links outside the
root refused, following a link adds coverage in one case only: an in-root
target the plain walk does not enter (a gitignored directory of generated or
vendored source). Every other in-root link is an alias of files already
walked, and following it only decides which spelling wins. Second, the TS/Go
guard claims a real path before `.gitignore` is checked (so the one useful
case depends on sibling names), identifies a file below a followed link by its
as-reached path (so one file can be indexed twice), and lets an alias spelling
win when it sorts first.

## Decision
- **Always on, no flag.** Every SDK plugin follows links, behind the guard.
  A per-plugin flag would make the tree depend on the language, and core's
  single walk could not mirror it.
- **What is refused.** A link is followed only if its target resolves, its
  real path is inside the root's real path, and no component of the target's
  root-relative real path is in `BASELINE_EXCLUDED_DIRS` or the manifest's
  `exclude_dirs`. Outside-root targets are unbounded (bazel's execroot, the
  nix store, `$HOME`) and silently stale (core's watcher watches the root
  only, and FSEvents/ReadDirectoryChangesW do not follow links). Excluded
  targets are never source, and core drops their events. A gitignored target
  is not refused: it is the case the feature exists for. A link is judged
  only after `.gitignore` and the name excludes have let it through.
- **Real wins.** Files are keyed by real path. A file the plain walk reaches
  keeps that spelling; a file reachable only through links takes the first
  spelling in walk order. This agrees with core's walk (which does not follow
  links), with the paths the OS reports to the watcher, and with every
  language server, which answer in real paths; and it keeps ids stable when a
  sibling is renamed.
- **Bounds.** A directory is entered through a link at most once and never
  through a link once entered; a directory reached without a link is always
  entered. Nested links can walk one real directory more than once, but each
  link is entered once, so the walk ends; walkdir's ancestor-loop check is a
  backstop. A dangling link and a link to an ancestor fail in the directory
  iterator before `.gitignore` is consulted, so they are reported in the link
  table even when ignored (never under an excluded directory name, which is
  not entered).
- **Cost, measured** (release build, median of 20 interleaved walks, load
  4-6): following links costs 0.5-1.3 ms per walk on link-free corpora
  (ripgrep 4.8 -> 5.5 ms, py-requests 2.8 -> 4.2, gin 2.4 -> 3.0), 4.5 ms on
  ripgrep with a link that re-enters `crates/` (same 100 files, deduplicated);
  `g-mesh init` end to end is within noise (+3% real, user and sys flat).
- **Core's walk does not follow links.** Under real-wins its file set is the
  plugins' minus the files reachable only through a link.
- The SDK exposes what became of each link (`walk_project_detailed`:
  aliases, followed, duplicate, refused) for import resolution in GM-324.

Rejected: alias-wins (TS's sorted-first rule; it disagrees with core's walk,
the watcher and the language servers), outside-root targets under a narrower
"enclosing git work tree" boundary (the staleness stays until core watches
link targets), and keeping the SDK non-following (TypeScript would lose
coverage it has today).

## Consequences
- Python and Rust projects with links can gain files; every gain is a
  gitignored target, since aliases collapse onto the real spelling.
- Four TS/Go assertions that encoded alias-wins change to the real spelling
  (TS tests 467, 493, workspace 291; Go
  `TestWalkProjectFilesFollowsSymlinkedDirectoryOnce`).
- Files reachable only through a link are invisible to `g-mesh status`, and
  an edit to one arrives (on macOS) under its real spelling, which core drops
  when `.gitignore` covers it, so it refreshes only on the next bulk index.
  Core learning the link table is a backlog task.
- An event spelled through a link is remapped to the indexed real spelling by
  the SDK session (GM-349's own code slice), so an aliased file stays indexed
  once after its first edit. The remap applies only when the index holds the
  real spelling and it lies inside the root; a deleted path is handled as
  spelled.
- Windows junctions are followed as links (believed from `is_symlink`'s
  definition; checked on CI).

## Resolved at review (owner, 2026-10-04)
- Real-wins replaces TS's sorted-first rule, including the four assertions.
- Outside-root links stay refused.
- The Node TypeScript plugin's guard bugs are left to GM-324, which moves it
  onto this walk in the same release.
- The acceptance fixture is a gitignored target reached through a link.
