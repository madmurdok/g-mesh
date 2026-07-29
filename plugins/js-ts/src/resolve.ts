// Module-specifier resolution for the structural (tree-sitter) pass.
//
// A specifier is the one thing a syntax-only extractor can resolve exactly:
// it is a string literal, not an identifier, so no type information is
// involved. For *relative* specifiers ("./x", "../x") the answer is pure path
// arithmetic against the filesystem - the same shape of algorithm Node's
// CommonJS/ESM resolver uses (try the literal path, then the known source
// extensions, then `index.*` inside a directory), plus TypeScript's
// extension-substitution rule, which is what makes `import "./db/connection.js"`
// in an ESM TypeScript file point at `db/connection.ts` on disk.
//
// A *bare* specifier is resolved only when it names a package of this
// project's own workspace ("@excalidraw/math" -> packages/math): the manifest
// says which directory that is (workspace.ts), and from there it is the same
// path arithmetic, against the same filesystem, producing the same kind of
// project-relative path - so nothing downstream can tell the two apart.
// That case matters because in a monorepo most cross-package imports are
// written this way, not relatively.
//
// Everything else - "react", "node:crypto", any package that exists only
// under `node_modules` - stays an unresolved placeholder (see `recordImport`
// in extract.ts): no file of theirs is indexed, so there is no node an edge
// could honestly point at. `paths` aliases and `imports` (#private) maps need
// project configuration this pass does not read, and are left to the semantic
// layer as well.

// Depends on extract.ts, never the other way round: the extractor takes a
// `SpecifierResolver` from its caller and knows nothing about how one is
// built, which keeps the two modules acyclic at runtime.

import * as fs from "node:fs";
import * as path from "node:path";

import { isSupportedFile, type SpecifierResolver } from "./extract";
import { createIndexabilityChecker } from "./ignorePolicy";
import {
  NO_WORKSPACE_PACKAGES,
  packageEntryTargets,
  parseBareSpecifier,
  readWorkspacePackages,
  type WorkspacePackages,
} from "./workspace";

/**
 * "Is there a regular file at this project-relative POSIX path?" - the one
 * filesystem fact resolution needs, injected rather than called directly so
 * the resolution *policy* below stays a pure function that tests can drive
 * with a set of paths instead of a temp directory.
 */
export type FileExists = (projectRelativePath: string) => boolean;

/** Source extensions a specifier without one may be pointing at, in the order
 * TypeScript itself tries them (its own extensions before the JS ones). */
const EXTENSIONS = [".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"] as const;

/**
 * TypeScript's rule for ESM-style imports: a specifier ending in a JS
 * extension names the *emitted* file, so the source it refers to is the
 * corresponding TS file. Without this, nothing in an ESM TypeScript project
 * (`import { x } from "./x.js"`) would resolve at all, since `./x.js` does
 * not exist on disk until the project is built.
 */
const TS_SUBSTITUTIONS: Readonly<Record<string, readonly string[]>> = {
  ".js": [".ts", ".tsx", ".d.ts"],
  ".jsx": [".tsx"],
  ".mjs": [".mts", ".d.mts"],
  ".cjs": [".cts", ".d.cts"],
};

/** `./x`, `../x`, `.` and `..` - everything else is a bare/package specifier. */
export function isRelativeSpecifier(specifier: string): boolean {
  return specifier === "." || specifier === ".." || /^\.\.?\//.test(specifier);
}

/** Longest known extension of `p` (so `a.d.ts` is `.ts`, not `.d.ts`), lowercased. */
function extensionOf(p: string): string {
  return path.posix.extname(p).toLowerCase();
}

/**
 * Every path `base` could name, most specific first. Candidates are filtered
 * against `isSupportedFile` by construction: a specifier pointing at
 * something this plugin never parses (`./styles.css`, `./data.json`) has no
 * `File` node to link to, so claiming it resolved would be a lie that costs a
 * stat call to tell.
 */
function* candidatePaths(base: string): Generator<string> {
  const extension = extensionOf(base);

  const substitutions = TS_SUBSTITUTIONS[extension];
  if (substitutions) {
    const stem = base.slice(0, base.length - extension.length);
    for (const substitution of substitutions) yield stem + substitution;
    yield base; // a real `.js`/`.cjs` file next to TS sources is legal too
    return;
  }

  // Already names a source file outright (`./x.ts`, allowed by
  // `allowImportingTsExtensions`) - it is a file, so never a directory.
  if (isSupportedFile(base)) {
    yield base;
    return;
  }

  for (const candidate of EXTENSIONS) yield base + candidate;
  for (const candidate of EXTENSIONS) yield `${base}/index${candidate}`;
}

/**
 * Project-relative POSIX path `specifier` names when written in
 * `fromFilePath`, or `null` when it names nothing this index can hold - a
 * bare specifier, a path that climbs out of the project root, a dangling
 * relative import (deleted file, typo), or a file this plugin does not parse.
 *
 * `fromFilePath` must be the same project-relative POSIX path the extractor
 * puts in node ids, so the result is directly comparable with the `filePath`
 * of the target's own `File` node.
 */
export function resolveRelativeSpecifier(
  specifier: string,
  fromFilePath: string,
  fileExists: FileExists,
): string | null {
  if (!isRelativeSpecifier(specifier)) return null;

  const base = path.posix.normalize(path.posix.join(path.posix.dirname(fromFilePath), specifier));
  // `..`-escaping paths and the degenerate self-directory cases address
  // nothing inside the project, and letting them through would also be the
  // one way a specifier could reach outside the project root.
  if (base === "" || base === "." || base === ".." || base.startsWith("../")) return null;

  for (const candidate of candidatePaths(base.replace(/\/+$/, ""))) {
    if (fileExists(candidate)) return candidate;
  }
  return null;
}

/**
 * Project-relative POSIX path a bare specifier names, or `null` when it names
 * nothing in this project - the overwhelmingly common case, since only a
 * package `workspacePackages` lists is in the tree at all.
 *
 * Unlike relative resolution this does not depend on the importing file: a
 * package name means the same thing everywhere in the workspace.
 */
export function resolveWorkspaceSpecifier(
  specifier: string,
  workspacePackages: WorkspacePackages,
  fileExists: FileExists,
): string | null {
  if (workspacePackages.size === 0) return null;

  const parsed = parseBareSpecifier(specifier);
  if (parsed === null) return null;

  const target = workspacePackages.get(parsed.name);
  if (target === undefined) return null;

  for (const base of packageEntryTargets(target, parsed.subpath)) {
    for (const candidate of candidatePaths(base)) {
      if (fileExists(candidate)) return candidate;
    }
  }
  return null;
}

/** Both resolutions in the single shape the extractor consumes. */
export function createSpecifierResolver(
  fileExists: FileExists,
  workspacePackages: WorkspacePackages = NO_WORKSPACE_PACKAGES,
): SpecifierResolver {
  return (specifier, fromFilePath) =>
    isRelativeSpecifier(specifier)
      ? resolveRelativeSpecifier(specifier, fromFilePath, fileExists)
      : resolveWorkspaceSpecifier(specifier, workspacePackages, fileExists);
}

/**
 * A [`FileExists`] backed by the real filesystem under `projectRoot`, answering
 * the fuller question the index actually cares about: the path is a regular
 * file *and* it is something the walk would index - not under a hard-excluded
 * directory (`dist`, `node_modules`, ...), not gitignored (see
 * ignorePolicy.ts, shared with `walkProjectFiles` so the two answers cannot
 * drift apart). A build output that happens to be on disk is therefore not
 * allowed to shadow the source file it was compiled from.
 *
 * The memo caches that combined answer, not raw filesystem existence: one
 * module is typically imported by many files, and each miss costs several stat
 * calls' worth of extension guessing.
 *
 * The memo is the reason this is a factory rather than a module-level
 * function: it must not outlive the assumption that the tree is not changing
 * underneath it. The bulk index is a one-shot process, so one instance covers
 * the whole walk; the long-lived plugin builds a fresh one per changed file
 * (see `reparseChangedFile`), so a file created since the last edit is seen.
 */
export function createProjectFileExists(projectRoot: string): FileExists {
  const memo = new Map<string, boolean>();
  const isIndexable = createIndexabilityChecker(projectRoot);
  return (projectRelativePath: string): boolean => {
    const cached = memo.get(projectRelativePath);
    if (cached !== undefined) return cached;

    let exists = false;
    try {
      // Path segments are re-joined with the platform separator; the caller's
      // path is normalized and cannot start with "..", so this stays inside
      // `projectRoot`.
      // The stat comes first deliberately: most candidate probes are misses
      // that fail it, and those never pay for the gitignore-aware check.
      exists =
        fs.statSync(path.join(projectRoot, ...projectRelativePath.split("/"))).isFile() &&
        isIndexable(projectRelativePath);
    } catch {
      exists = false; // missing, unreadable, or a broken symlink - all "no"
    }
    memo.set(projectRelativePath, exists);
    return exists;
  };
}

/**
 * The resolver the real indexing paths use: filesystem-backed, rooted at
 * `projectRoot`.
 *
 * The workspace is read once here, and so has the same lifetime as the
 * existence memo above - once per bulk-index walk, once per reparsed file. It
 * is the cheaper of the two answers to refresh (a handful of manifests, well
 * under a millisecond) and by far the less volatile, so nothing is gained by
 * holding it any longer than the memo it travels with.
 */
export function createProjectResolver(projectRoot: string): SpecifierResolver {
  return createSpecifierResolver(
    createProjectFileExists(projectRoot),
    readWorkspacePackages(projectRoot),
  );
}
