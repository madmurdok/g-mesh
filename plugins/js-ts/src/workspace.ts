// The one class of bare specifier a syntax-only pass can resolve exactly:
// a package that lives *in this project*, linked by a workspace manifest
// rather than fetched from a registry. `@excalidraw/math` is a directory in
// the same tree as its importer, so the specifier still names a file this
// index holds - the only thing missing is the manifest lookup that turns the
// package name into that directory.
//
// Packages outside the workspace (`react`, `node:crypto`, anything only
// present under `node_modules`) stay out of scope: no file of theirs is
// indexed, so resolving them would point an edge at nothing.
//
// This module answers "which directory is package X, and which paths could
// its entry be?" - manifest facts only. Turning those paths into a file is
// resolve.ts's job, so both specifier kinds go through one filesystem policy.

import * as fs from "node:fs";
import * as path from "node:path";

/** The package.json fields this resolution reads; everything else is ignored. */
interface PackageManifest {
  readonly name?: unknown;
  readonly exports?: unknown;
  readonly source?: unknown;
  readonly main?: unknown;
  readonly module?: unknown;
  readonly types?: unknown;
  readonly typings?: unknown;
}

export interface WorkspacePackage {
  /** Project-relative POSIX directory holding the package's package.json. */
  readonly dir: string;
  readonly manifest: PackageManifest;
}

/** In-workspace packages by the name their package.json declares. */
export type WorkspacePackages = ReadonlyMap<string, WorkspacePackage>;

export const NO_WORKSPACE_PACKAGES: WorkspacePackages = new Map();

/**
 * Conditions in the order a *source* index wants them, not the order Node
 * would pick: `source` and the runtime conditions name the package's own
 * code, while `types` names a `.d.ts` describing code that is very often
 * already in this index under its real path. Existence still decides - this
 * only settles which of several existing files wins.
 *
 * Task 84 (see ignorePolicy.ts) already makes `FileExists` refuse a
 * gitignored or hard-excluded candidate regardless of where it ranks, so the
 * only case this ordering still has to arbitrate is a package that ships
 * *both* an ESM and a CJS build actually committed in-tree. Ranking
 * source/import/module ahead of require/default, and types last, means such a
 * package deterministically resolves to its ESM/source-facing copy every
 * time - a defensible, source-oriented-index choice, and importantly a
 * *stable* one: the same input always resolves to the same file, which rules
 * out the flakiness that would be the real cost of getting this wrong. This is
 * a deliberate, documented choice, not an open gap.
 */
const CONDITION_PRIORITY = [
  "source",
  "import",
  "module",
  "require",
  "default",
  "node",
  "browser",
  "development",
  "production",
  "types",
  "typings",
];

/** How deep a `**` pattern segment may descend before it is treated as noise. */
const MAX_GLOB_DEPTH = 6;

const IGNORED_DIRECTORIES = new Set(["node_modules", ".git"]);

/**
 * `@scope/pkg/sub` split into the package name and the subpath it addresses
 * (`"."` when it addresses the package itself), or `null` when the specifier
 * is not a package name at all - relative, absolute, a `node:`/`data:` URL,
 * or a `#private` imports-map key.
 */
export function parseBareSpecifier(
  specifier: string,
): { readonly name: string; readonly subpath: string } | null {
  if (specifier === "" || specifier.startsWith(".") || specifier.startsWith("/")) return null;
  if (specifier.startsWith("#") || /^[a-zA-Z][a-zA-Z\d+\-.]*:/.test(specifier)) return null;

  const segments = specifier.split("/");
  if (segments.some((segment) => segment === "")) return null;

  const nameLength = specifier.startsWith("@") ? 2 : 1;
  if (segments.length < nameLength) return null;

  const rest = segments.slice(nameLength);
  return {
    name: segments.slice(0, nameLength).join("/"),
    subpath: rest.length === 0 ? "." : `./${rest.join("/")}`,
  };
}

/**
 * Every project-relative path `subpath` of `pkg` could name, most specific
 * first: what the manifest declares, then the source-tree conventions.
 *
 * The conventional `src/index`/`index` fallback is not a nicety - a
 * workspace package's declared entry almost always points into build output
 * (`dist/...`) that an unbuilt checkout does not have, and that a built one
 * gitignores, so for most monorepos it is the *only* candidate backed by a
 * file this index holds.
 */
export function packageEntryTargets(pkg: WorkspacePackage, subpath: string): string[] {
  const targets = exportsTargets(pkg.manifest.exports, subpath);

  if (subpath === ".") {
    const { source, main, module: moduleField, types, typings } = pkg.manifest;
    for (const field of [source, main, moduleField, types, typings]) {
      if (typeof field === "string") targets.push(field);
    }
    targets.push("src/index", "index");
  } else {
    const relative = subpath.slice(2);
    targets.push(relative, `src/${relative}`);
  }

  const resolved: string[] = [];
  for (const target of targets) {
    const inside = insidePackage(pkg.dir, target);
    if (inside !== null && !resolved.includes(inside)) resolved.push(inside);
  }
  return resolved;
}

/**
 * The workspace's packages, read from disk once. Callers cache the result for
 * as long as they cache their other filesystem answers (see resolve.ts): a
 * workspace manifest changes far more rarely than the sources it lists.
 */
export function readWorkspacePackages(projectRoot: string): WorkspacePackages {
  const patterns = workspacePatterns(projectRoot);
  if (patterns.length === 0) return NO_WORKSPACE_PACKAGES;

  const excluded = patterns
    .filter((pattern) => pattern.startsWith("!"))
    .map((pattern) => globToRegExp(pattern.slice(1)));
  const listSubdirectories = subdirectoryLister(projectRoot);

  const packages = new Map<string, WorkspacePackage>();
  for (const pattern of patterns) {
    if (pattern.startsWith("!")) continue;
    for (const dir of expandPattern(pattern, projectRoot, listSubdirectories)) {
      if (excluded.some((regex) => regex.test(dir))) continue;
      const manifest = readJson(path.join(projectRoot, ...dir.split("/"), "package.json"));
      if (manifest === null || typeof manifest.name !== "string") continue;
      // First declaration wins, like a re-declared symbol elsewhere in this plugin.
      if (!packages.has(manifest.name)) packages.set(manifest.name, { dir, manifest });
    }
  }
  return packages;
}

// --- package imports (#private) --------------------------------------------

/**
 * A package.json's `imports` map, plus the directory (project-relative POSIX)
 * that map's targets resolve against - the same "config facts, not yet a
 * file" shape `TsconfigPathsConfig` uses for `paths`.
 */
export interface PackageImportsConfig {
  /** Project-relative POSIX dir of the package.json that declared `imports`. */
  readonly dir: string;
  readonly imports: Record<string, unknown>;
}

/**
 * "Which `#private` imports map applies to this file?" - the `imports` analog
 * of `TsconfigPathsIndex`, a function rather than a map because the answer
 * depends on where the importing file sits: `#specifier` resolution is
 * strictly scoped to the *nearest enclosing package*, not inherited from a
 * parent package the way a workspace-wide manifest lookup would be.
 */
export type PackageImportsIndex = (fromFilePath: string) => PackageImportsConfig | null;

/** For callers with no project on disk - tests, and resolution built from a
 * bare `FileExists`. Every file gets "no `#imports` map". */
export const NO_PACKAGE_IMPORTS: PackageImportsIndex = () => null;

/**
 * The `#imports` map in force for `fromFilePath`, or `null` when the nearest
 * enclosing package.json declares none.
 *
 * Walks upward from `fromFilePath`'s own directory, exactly like
 * `createTsconfigPathsIndex` walks for the nearest tsconfig/jsconfig: the
 * first directory holding a package.json wins outright, whether or not that
 * package.json declares `imports` - a `#specifier` in a package with no
 * `imports` map is simply unresolved, matching Node's real behaviour, where
 * `#imports` never fall through to a grandparent package. The walk never
 * looks above `projectRoot`, and every directory it passes through is
 * memoized with the same answer, so a later lookup starting deeper in the
 * same package costs nothing.
 */
export function createPackageImportsIndex(projectRoot: string): PackageImportsIndex {
  const projectRootAbs = path.resolve(projectRoot);
  const nearestCache = new Map<string, PackageImportsConfig | null>();

  return (fromFilePath: string): PackageImportsConfig | null => {
    const startDir = path.join(projectRootAbs, ...path.posix.dirname(fromFilePath).split("/"));
    // A caller is expected to pass a project-relative path, but one that
    // climbs out would start the walk outside the project - the one way this
    // could read a package.json that is none of its business.
    if (projectRelativeDir(startDir, projectRootAbs) === null) return null;

    // Every directory walked past shares the answer: none of them held a
    // package.json, so their nearest one is whatever this walk ends up finding.
    const visited: string[] = [];
    let current = startDir;
    let found: PackageImportsConfig | null = null;

    while (true) {
      const cached = nearestCache.get(current);
      if (cached !== undefined) {
        found = cached;
        break;
      }
      visited.push(current);

      const manifestPath = path.join(current, "package.json");
      if (isFile(manifestPath)) {
        found = readPackageImports(manifestPath, current, projectRootAbs);
        break;
      }

      if (current === projectRootAbs) break; // never look above the project root
      const parent = path.dirname(current);
      if (parent === current) break; // walked past the root - stop rather than loop
      current = parent;
    }

    for (const dir of visited) nearestCache.set(dir, found);
    return found;
  };
}

/** One package.json's `imports` field, or `null` when it has none worth
 * reporting - missing, malformed (handled by `readJson`), or not a plain
 * object. */
function readPackageImports(
  manifestAbsPath: string,
  dirAbs: string,
  projectRootAbs: string,
): PackageImportsConfig | null {
  const manifest = readJson(manifestAbsPath);
  const imports = manifest?.imports;
  if (!isRecord(imports)) return null;
  const dir = projectRelativeDir(dirAbs, projectRootAbs);
  return dir === null ? null : { dir, imports };
}

/**
 * Every project-relative path `specifier` (always `#`-prefixed) could name
 * under `config`, most specific first - the `imports` analog of
 * `exportsTargets`.
 *
 * All matching keys contribute, not just the most specific one, the same
 * "let existence decide" policy `exportsTargets` and `expandPathsCandidates`
 * already use.
 */
export function importsTargets(config: PackageImportsConfig, specifier: string): string[] {
  const targets: string[] = [];
  for (const [key, value] of Object.entries(config.imports)) {
    let capture: string | null = null;
    if (key.includes("*")) {
      capture = matchWildcard(key, specifier);
      if (capture === null) continue;
    } else if (key !== specifier) {
      continue;
    }
    targets.push(...keyTargets(value, capture));
  }

  const resolved: string[] = [];
  for (const target of targets) {
    const inside = insidePackage(config.dir, target);
    if (inside !== null && !resolved.includes(inside)) resolved.push(inside);
  }
  return resolved;
}

// --- manifests ------------------------------------------------------------

function workspacePatterns(projectRoot: string): string[] {
  const patterns = pnpmWorkspacePatterns(projectRoot);

  // npm/yarn/bun keep the same list in the root package.json; a repo can
  // carry both (pnpm workspaces with a package.json kept for tooling), and
  // the union is what either package manager would see.
  const rootManifest = readJson(path.join(projectRoot, "package.json"));
  const workspaces = rootManifest?.workspaces;
  const list = Array.isArray(workspaces)
    ? workspaces
    : isRecord(workspaces) && Array.isArray(workspaces.packages)
      ? workspaces.packages
      : [];
  for (const entry of list) {
    if (typeof entry === "string" && entry.trim() !== "") patterns.push(entry.trim());
  }
  return patterns;
}

/**
 * The `packages:` list of a pnpm-workspace.yaml, read without a YAML parser:
 * this plugin ships no dependency that could do it, and the shape in question
 * is a flat list of strings under one top-level key - the one corner of YAML
 * that is unambiguous enough to read line by line.
 */
function pnpmWorkspacePatterns(projectRoot: string): string[] {
  const text =
    readText(path.join(projectRoot, "pnpm-workspace.yaml")) ??
    readText(path.join(projectRoot, "pnpm-workspace.yml"));
  if (text === null) return [];

  const patterns: string[] = [];
  let inPackages = false;
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.replace(/\t/g, "  ");
    if (line.trim() === "" || line.trimStart().startsWith("#")) continue;

    if (!/^\s/.test(line)) {
      const key = /^packages\s*:\s*(.*)$/.exec(line);
      inPackages = key !== null;
      if (key && key[1].trim() !== "") {
        patterns.push(...flowSequenceItems(key[1]));
        inPackages = false;
      }
      continue;
    }

    if (!inPackages) continue;
    const item = /^\s*-\s*(.+?)\s*$/.exec(line);
    if (item) {
      const value = scalarValue(item[1]);
      if (value !== "") patterns.push(value);
    }
  }
  return patterns;
}

function flowSequenceItems(text: string): string[] {
  const inner = /^\[(.*)\]$/.exec(text.trim());
  if (!inner) return [];
  return inner[1]
    .split(",")
    .map((item) => scalarValue(item.trim()))
    .filter((item) => item !== "");
}

function scalarValue(raw: string): string {
  const quoted = /^(["'])(.*)\1$/.exec(raw);
  if (quoted) return quoted[2];
  const commented = raw.indexOf(" #");
  return (commented === -1 ? raw : raw.slice(0, commented)).trim();
}

// --- exports maps ---------------------------------------------------------

function exportsTargets(exports: unknown, subpath: string): string[] {
  if (exports === undefined || exports === null) return [];

  const targets: string[] = [];
  if (typeof exports === "string" || Array.isArray(exports)) {
    if (subpath === ".") collectConditionTargets(exports, targets);
    return targets;
  }
  if (!isRecord(exports)) return [];

  // `{ ".": ... }` is a subpath map, `{ "import": ... }` a bare condition map
  // that applies to the package root - the two are told apart by their keys,
  // which is what the spec itself does.
  const keys = Object.keys(exports);
  if (!keys.some((key) => key === "." || key.startsWith("./"))) {
    if (subpath === ".") collectConditionTargets(exports, targets);
    return targets;
  }

  const exact = exports[subpath];
  if (exact !== undefined) {
    collectConditionTargets(exact, targets);
    return targets;
  }

  for (const key of keys) {
    const match = matchWildcard(key, subpath);
    if (match === null) continue;
    targets.push(...keyTargets(exports[key], match));
  }
  return targets;
}

/**
 * One key's declared value - a string, an array, or a nested condition object
 * - turned into ranked target strings, with a wildcard capture substituted in
 * when the key that matched had one (`null` for an exact-match key, which
 * substitutes nothing). Shared between `exportsTargets` and `importsTargets`:
 * an `exports` subpath key and an `imports` `#name` key declare their value in
 * exactly the same shape, so the "condition object -> candidate strings" step
 * is one piece of logic for both.
 */
function keyTargets(value: unknown, capture: string | null): string[] {
  const patterns: string[] = [];
  collectConditionTargets(value, patterns);
  if (capture === null) return patterns;
  return patterns.map((pattern) => pattern.split("*").join(capture));
}

function collectConditionTargets(value: unknown, out: string[]): void {
  if (typeof value === "string") {
    out.push(value);
    return;
  }
  if (Array.isArray(value)) {
    for (const item of value) collectConditionTargets(item, out);
    return;
  }
  if (!isRecord(value)) return;

  const keys = Object.keys(value).sort((a, b) => conditionRank(a) - conditionRank(b));
  for (const key of keys) collectConditionTargets(value[key], out);
}

function conditionRank(condition: string): number {
  const index = CONDITION_PRIORITY.indexOf(condition);
  return index === -1 ? CONDITION_PRIORITY.length : index;
}

/** What `*` stands for when `subpath` matches `pattern`, or `null`. */
export function matchWildcard(pattern: string, subpath: string): string | null {
  const star = pattern.indexOf("*");
  if (star === -1) return null;
  const prefix = pattern.slice(0, star);
  const suffix = pattern.slice(star + 1);
  if (subpath.length < prefix.length + suffix.length) return null;
  if (!subpath.startsWith(prefix) || !subpath.endsWith(suffix)) return null;
  return subpath.slice(prefix.length, subpath.length - suffix.length);
}

// --- patterns and the filesystem ------------------------------------------

/**
 * Project-relative directories `pattern` names. Only `*` and `**` are
 * understood; both stop at `node_modules` and dot-directories, which no
 * package manager treats as workspace members and which are the one way this
 * walk could get expensive.
 */
function expandPattern(
  pattern: string,
  projectRoot: string,
  listSubdirectories: (dir: string) => string[],
): string[] {
  const segments = pattern.split("/").filter((segment) => segment !== "" && segment !== ".");
  if (segments.length === 0 || segments.includes("..")) return [];

  let dirs = [""];
  for (const segment of segments) {
    const next: string[] = [];
    for (const dir of dirs) {
      if (segment === "**") {
        next.push(dir, ...descendants(dir, listSubdirectories, MAX_GLOB_DEPTH));
        continue;
      }
      if (segment.includes("*") || segment.includes("?")) {
        const regex = globToRegExp(segment);
        for (const child of listSubdirectories(dir)) {
          if (regex.test(child)) next.push(join(dir, child));
        }
        continue;
      }
      const candidate = join(dir, segment);
      if (isDirectory(path.join(projectRoot, ...candidate.split("/")))) next.push(candidate);
    }
    dirs = [...new Set(next)];
  }
  return dirs.filter((dir) => dir !== "");
}

function descendants(
  dir: string,
  listSubdirectories: (dir: string) => string[],
  depth: number,
): string[] {
  if (depth === 0) return [];
  const found: string[] = [];
  for (const child of listSubdirectories(dir)) {
    const childDir = join(dir, child);
    found.push(childDir, ...descendants(childDir, listSubdirectories, depth - 1));
  }
  return found;
}

function subdirectoryLister(projectRoot: string): (dir: string) => string[] {
  const memo = new Map<string, string[]>();
  return (dir: string): string[] => {
    const cached = memo.get(dir);
    if (cached !== undefined) return cached;

    let names: string[] = [];
    try {
      names = fs
        .readdirSync(path.join(projectRoot, ...dir.split("/").filter(Boolean)), {
          withFileTypes: true,
        })
        .filter((entry) => entry.isDirectory() && !isIgnoredDirectory(entry.name))
        .map((entry) => entry.name);
    } catch {
      names = []; // missing or unreadable - nothing to expand into
    }
    memo.set(dir, names);
    return names;
  };
}

function isIgnoredDirectory(name: string): boolean {
  return name.startsWith(".") || IGNORED_DIRECTORIES.has(name);
}

function globToRegExp(pattern: string): RegExp {
  const glob = pattern.replace(/^\.\//, "");
  let source = "";
  for (let index = 0; index < glob.length; index += 1) {
    const char = glob[index];
    if (char === "*" && glob[index + 1] === "*") {
      index += 1;
      if (glob[index + 1] === "/") {
        index += 1;
        source += "(?:.*/)?";
        continue;
      }
      source += ".*";
      continue;
    }
    if (char === "*") {
      source += "[^/]*";
      continue;
    }
    source += char === "?" ? "[^/]" : char.replace(/[.+^${}()|[\]\\]/, "\\$&");
  }
  return new RegExp(`^${source}$`);
}

/**
 * `target` as a project-relative path, or `null` when it leaves the package -
 * an absolute path or one climbing past the project root names a file this
 * index cannot hold, and is also the only way this could address the
 * filesystem outside the project.
 *
 * Shared with tsconfigPaths.ts, where the "package directory" role is played
 * by a tsconfig's `resolveDir`: an alias target escaping it has to be refused
 * on exactly the same grounds.
 */
export function insidePackage(packageDir: string, target: string): string | null {
  if (target === "" || path.posix.isAbsolute(target) || /^[a-zA-Z]:/.test(target)) return null;
  const joined = path.posix.normalize(path.posix.join(packageDir, target)).replace(/\/+$/, "");
  if (joined === "" || joined === "." || joined === ".." || joined.startsWith("../")) return null;
  return joined;
}

function join(dir: string, name: string): string {
  return dir === "" ? name : `${dir}/${name}`;
}

function isDirectory(absolutePath: string): boolean {
  try {
    return fs.statSync(absolutePath).isDirectory();
  } catch {
    return false;
  }
}

function isFile(absolutePath: string): boolean {
  try {
    return fs.statSync(absolutePath).isFile();
  } catch {
    return false;
  }
}

/** `absPath` as a project-relative POSIX path (`""` for the root itself), or
 * `null` when it is not under the project root at all. Mirrors
 * tsconfigPaths.ts's helper of the same name and purpose. */
function projectRelativeDir(absPath: string, projectRootAbs: string): string | null {
  const relative = path.relative(projectRootAbs, absPath);
  if (relative === "") return "";
  if (path.isAbsolute(relative) || relative === ".." || relative.startsWith(`..${path.sep}`)) {
    return null;
  }
  return relative.split(path.sep).join("/");
}

function readText(absolutePath: string): string | null {
  try {
    return fs.readFileSync(absolutePath, "utf8");
  } catch {
    return null;
  }
}

function readJson(absolutePath: string): (PackageManifest & Record<string, unknown>) | null {
  const text = readText(absolutePath);
  if (text === null) return null;
  try {
    const parsed: unknown = JSON.parse(text);
    return isRecord(parsed) ? parsed : null;
  } catch {
    return null; // a malformed manifest is a package this resolution skips, not a crash
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
