// The one answer to "may this traversal step onto this directory entry, and
// what is it really?" - the symlink policy shared by every walk this plugin
// runs over a project tree.
//
// The decision it encodes: symlinks are *followed*. A symlinked package is an
// ordinary part of how monorepos are laid out (yarn/lerna-linked packages,
// vendored shared code linked into `packages/`), and the alternative - the
// `Dirent.isDirectory()`/`isFile()` pair both answering `false` for a symlink,
// so the walk silently skips it - makes a real, imported, in-tree package
// invisible to the index with nothing anywhere saying why.
//
// Following them is only safe with a guard, which is what this module is:
// a link can point at one of its own ancestors (an infinite descent), at a
// directory some other path already reached (the same file indexed twice, under
// two paths), or outside the project entirely (a path this index must never
// hold). All three have to be refused, and refused *identically* on both sides
// - bulkIndex.ts's file walk and workspace.ts's glob expansion are separate
// traversals answering different questions ("which source files are there?" vs
// "which directories are candidate packages?"), and a disagreement between them
// is a package whose manifest is found but whose files were never indexed, or
// the reverse. Each side builds its own fresh guard per call (live state is
// never shared between them); the logic they share is this file.

import * as fs from "node:fs";
import type { Dirent } from "node:fs";
import * as path from "node:path";

/**
 * One directory entry a traversal is allowed to step onto, with what it turned
 * out to be once followed.
 *
 * `absPath` is the **as-reached** path - the symlink's own location when the
 * entry was one, never the canonical target it resolved to. That is deliberate:
 * everywhere else in this plugin a file's or package's identity is the
 * project-relative path it was reached by (`WireNode.filePath`,
 * `WorkspacePackage.dir`), which is the path a human wrote in an import and the
 * path a tool answer has to name. Canonicalizing here would rename a symlinked
 * package to wherever its real files live, breaking every specifier that names
 * it. The real path is used for identity only *inside* this module, to decide
 * what has already been claimed.
 *
 * `isDirectory`/`isFile` are the *followed* answers (`stat`, not `lstat`), so a
 * caller never has to know whether it is looking at a link.
 */
export interface ResolvedEntry {
  readonly absPath: string;
  readonly isDirectory: boolean;
  readonly isFile: boolean;
}

/**
 * "May this traversal step onto `entry` of `parentAbsDir`?" - the resolved
 * entry, or `null` when it must be skipped (a cycle, a second path onto an
 * already-claimed target, an escape from the project root, or a dangling link).
 *
 * Stateful: each instance remembers what its own traversal has claimed, so one
 * belongs to exactly one traversal and must not outlive it or be shared with
 * another.
 */
export type SymlinkGuard = (parentAbsDir: string, entry: Dirent) => ResolvedEntry | null;

/**
 * `projectRoot` as a real path, which is what every guard comparison is made
 * against.
 *
 * The `realpathSync` is not optional. A project root is very often reached
 * through a symlink itself - on macOS `/tmp` is a link to `/private/tmp` and
 * `/var` to `/private/var`, so every temp-directory fixture (and every real
 * project under one) is behind one; checkouts under a linked home directory or
 * a linked mount point are the same story. Left uncanonicalized, the root would
 * be spelled one way while anything reached through an in-project link reports
 * the resolved spelling of the very same location - two strings that never
 * compare equal, which silently disables cycle and duplicate detection exactly
 * where it is needed.
 *
 * A root that does not exist is returned resolved-but-not-real: there is
 * nothing to canonicalize, and the caller's own `readdir`/`readFile` already
 * treats an unreadable root as an empty walk.
 */
export function canonicalizeProjectRoot(projectRoot: string): string {
  const resolved = path.resolve(projectRoot);
  try {
    return fs.realpathSync(resolved);
  } catch {
    return resolved; // root does not exist (yet) - let the caller's own readdir/readFile catch handle it
  }
}

/**
 * A guard for one traversal rooted at `projectRootReal` (which must already be
 * a real path - see `canonicalizeProjectRoot`).
 *
 * What it guarantees, for as long as one instance is used for one traversal:
 *
 * - **No real location is claimed twice.** Identity is the resolved real path,
 *   so a directory reached both directly and through a link to it is walked
 *   once, under whichever path the traversal reached first. Its sibling order
 *   is therefore what decides the winner, which is why both callers sort their
 *   entries: the outcome has to be a property of the tree, not of `readdir`
 *   order. The loser is dropped outright rather than merged or rewritten onto a
 *   canonical path - see `ResolvedEntry.absPath` for why there is no canonical
 *   path to rewrite it to.
 * - **No cycle.** A link onto one of its own ancestors resolves to a real path
 *   that ancestor already claimed, so it is refused by the same check - no
 *   separate ancestor-chain test, and no unbounded descent.
 * - **Nothing outside the project.** A link resolving out of `projectRootReal`
 *   is refused, the same never-escape-the-project-root rule `insidePackage` in
 *   workspace.ts and `projectRelativeDir` in tsconfigPaths.ts apply to the
 *   paths they hand out.
 * - **A dangling link is a skip, not a throw.**
 *
 * A non-symlink entry costs no extra syscall: its own path is already a real
 * path by induction - the traversal starts at a canonicalized root and only
 * ever descends into directories this guard has already established as real -
 * so `absPath` *is* its identity. It is still checked against `claimed`, and
 * that check is not redundant: a link resolving to this exact location may have
 * been followed earlier in the same traversal, which is what makes "indexed
 * once" hold when the alias is reached *before* the real directory, not just
 * when a link points back at an ancestor.
 *
 * `claimed` starts holding `projectRootReal` itself for the one location no
 * entry check would ever cover: the root is never some parent directory's
 * `Dirent`, so a link pointing back at it (`packages/self -> ../..`) would
 * otherwise be followed and the whole project walked a second time underneath
 * it.
 */
export function createSymlinkGuard(projectRootReal: string): SymlinkGuard {
  const claimed = new Set<string>([projectRootReal]);

  return (parentAbsDir: string, entry: Dirent): ResolvedEntry | null => {
    const absPath = path.join(parentAbsDir, entry.name);

    if (!entry.isSymbolicLink()) {
      if (claimed.has(absPath)) return null;
      claimed.add(absPath);
      return { absPath, isDirectory: entry.isDirectory(), isFile: entry.isFile() };
    }

    let real: string;
    try {
      real = fs.realpathSync(absPath);
    } catch {
      return null; // dangling symlink
    }

    const rel = path.relative(projectRootReal, real);
    if (rel === ".." || rel.startsWith(`..${path.sep}`) || path.isAbsolute(rel)) {
      return null; // escapes the project root
    }
    if (claimed.has(real)) return null; // cycle, or a second path onto an already-claimed target

    let stat: fs.Stats;
    try {
      stat = fs.statSync(absPath);
    } catch {
      return null; // vanished between realpath and stat
    }
    claimed.add(real);
    return { absPath, isDirectory: stat.isDirectory(), isFile: stat.isFile() };
  };
}
