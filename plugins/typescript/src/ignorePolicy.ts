// The single answer to "would the walk ever turn this path into a `File`
// node?" - hard-excluded directory names plus git-style `.gitignore` layering.
//
// It lives in its own module because both sides of indexing need it and they
// need it in different shapes: bulkIndex.ts's walk asks directory by directory
// as it descends (async, one `.gitignore` read per directory it enters), while
// resolve.ts asks about one candidate path at a time, from a synchronous
// `FileExists`/`SpecifierResolver` contract. Two independent copies of the
// policy would be free to drift, and a resolver that claims a target the walk
// silently skipped is exactly the bug this module exists to prevent.
//
// Why here and not merged into either caller: bulkIndex.ts already imports
// from resolve.ts, so putting the shared policy in bulkIndex.ts would make
// resolve.ts import back into it and close an import cycle.

import { readFileSync } from "node:fs";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import ignore, { type Ignore } from "ignore";

/**
 * Directories skipped unconditionally, regardless of .gitignore contents.
 * `.claude` holds Claude Code's own session/worktree artifacts (e.g.
 * `.claude/worktrees/agent-<hash>/`, full copies of the project made for
 * parallel agent sessions) - real project source is never there, and a
 * project's own .gitignore has no reason to list it, so it needs the same
 * hard exclusion as `.git`.
 */
export const HARD_EXCLUDED_DIRS = new Set([".git", "node_modules", "dist", ".claude"]);

/** One directory's own `.gitignore`, plus the (absolute) directory it applies to. */
export interface GitignoreLayer {
  readonly baseDir: string;
  readonly matcher: Ignore;
}

export function toPosixPath(p: string): string {
  return p.split(path.sep).join("/");
}

export async function loadGitignoreLayer(dir: string): Promise<GitignoreLayer | null> {
  let contents: string;
  try {
    contents = await fs.readFile(path.join(dir, ".gitignore"), "utf8");
  } catch {
    return null; // no .gitignore here - not an error
  }
  return { baseDir: dir, matcher: ignore().add(contents) };
}

/**
 * The synchronous twin of `loadGitignoreLayer`, for the one caller that cannot
 * await: resolution runs inside a `FileExists` predicate, whose whole point is
 * being a plain synchronous question the extractor can ask mid-parse.
 */
function loadGitignoreLayerSync(dir: string): GitignoreLayer | null {
  let contents: string;
  try {
    contents = readFileSync(path.join(dir, ".gitignore"), "utf8");
  } catch {
    return null; // no .gitignore here - not an error
  }
  return { baseDir: dir, matcher: ignore().add(contents) };
}

/**
 * Combines every ancestor `.gitignore` layer the same way git does: each
 * layer's patterns apply to paths relative to that layer's own directory,
 * and a later (deeper, or later-declared) match - including a negation -
 * overrides an earlier one.
 */
export function isIgnoredByLayers(
  layers: readonly GitignoreLayer[],
  absPath: string,
): boolean {
  let ignored = false;
  for (const layer of layers) {
    const rel = path.relative(layer.baseDir, absPath);
    if (rel === "" || rel.startsWith("..")) continue; // not under this layer
    const result = layer.matcher.test(toPosixPath(rel));
    if (result.ignored) ignored = true;
    else if (result.unignored) ignored = false;
  }
  return ignored;
}

/**
 * Whether any segment of a project-relative POSIX path names a
 * hard-excluded directory - `packages/x/dist/prod/index.js` as much as
 * `dist/index.js`, since the walk never descends into such a directory at
 * whatever depth it meets one. Pure string work, no filesystem access.
 */
export function hasHardExcludedSegment(projectRelativePath: string): boolean {
  for (const segment of projectRelativePath.split("/")) {
    if (HARD_EXCLUDED_DIRS.has(segment)) return true;
  }
  return false;
}

/**
 * "Would the walk give this project-relative path a `File` node?" - the same
 * two exclusion mechanisms `walkProjectFiles` applies, asked about one path
 * instead of a whole tree. Says nothing about whether the path exists or
 * whether this plugin parses that extension; those are the caller's questions.
 *
 * Each checker caches the `.gitignore` of every directory it has looked at, so
 * a deep tree costs one read per directory rather than one per query - which
 * also means, exactly like the existence memo it is created alongside, that an
 * instance must not outlive the assumption that the tree is not changing
 * underneath it.
 */
export function createIndexabilityChecker(
  projectRoot: string,
): (projectRelativePath: string) => boolean {
  const layerCache = new Map<string, GitignoreLayer | null>();

  const layerFor = (dir: string): GitignoreLayer | null => {
    const cached = layerCache.get(dir);
    if (cached !== undefined) return cached;
    const layer = loadGitignoreLayerSync(dir);
    layerCache.set(dir, layer);
    return layer;
  };

  /** Root-first (ancestors before descendants), because `isIgnoredByLayers`
   * lets a later layer's match or negation override an earlier one - the same
   * precedence git gives a deeper `.gitignore`. */
  const ancestorLayers = (dir: string): GitignoreLayer[] => {
    const chain: string[] = [];
    let current = dir;
    while (true) {
      chain.push(current);
      if (current === projectRoot) break;
      const parent = path.dirname(current);
      if (parent === current) break; // walked past the root - stop rather than loop
      current = parent;
    }
    chain.reverse();

    const layers: GitignoreLayer[] = [];
    for (const ancestor of chain) {
      const layer = layerFor(ancestor);
      if (layer) layers.push(layer);
    }
    return layers;
  };

  return (projectRelativePath: string): boolean => {
    if (hasHardExcludedSegment(projectRelativePath)) return false; // no I/O needed
    const absPath = path.join(projectRoot, ...projectRelativePath.split("/"));
    return !isIgnoredByLayers(ancestorLayers(path.dirname(absPath)), absPath);
  };
}
