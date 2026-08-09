// The second class of bare specifier a syntax-only pass can resolve exactly:
// a `paths` alias declared by the project's own tsconfig/jsconfig. `@/utils`
// is not a package at all - it is a rewrite rule the project wrote down, and
// the file it rewrites to (`src/utils.ts`) is in the same tree as its
// importer, so the specifier still names a file this index holds. The only
// thing missing is the config lookup that turns the alias into that path.
//
// It matters for the same reason workspace resolution does, only more widely:
// `@/...` is the default import style of every Next.js/Vite/CRA app and of
// most monorepo app packages, so in such a project *most* internal imports are
// written this way and none of them resolve without this module.
//
// Like workspace.ts, this reads the config by hand rather than through
// TypeScript's own `readConfigFile`/`parseJsonConfigFileContent`: this plugin
// ships no dependency that could do it, and the corner of tsconfig in question
// - `extends`, `baseUrl`, `paths` - is a handful of string fields whose merge
// rules are documented and small. Nothing else is ever read off a tsconfig:
// notably not `compilerOptions.plugins`, whose entries a real TypeScript
// loader would `require()`, and which this plugin must keep inert (see
// test/security.test.ts and the comment on `isSupportedFile` in bulkIndex.ts).
//
// This module answers "which alias rules apply to a file, and which paths
// could an alias expand to?" - config facts only. Turning those paths into a
// file is resolve.ts's job, so every specifier kind goes through one
// filesystem policy.

import * as fs from "node:fs";
import * as path from "node:path";

import { toPosixPath } from "./ignorePolicy";
import { insidePackage, matchWildcard } from "./workspace";

/** One `paths` key with the target list it maps to, in declaration order. */
export interface TsconfigPathsEntry {
  readonly pattern: string;
  readonly targets: readonly string[];
}

/**
 * The alias rules in force for some file, already reduced to what resolution
 * needs: a directory the targets are relative to, and the rules themselves.
 *
 * `resolveDir` is a project-relative POSIX directory (`""` for the project
 * root), so expanding a target lands in the same vocabulary as every other
 * path in this plugin.
 */
export interface TsconfigPathsConfig {
  readonly resolveDir: string;
  readonly paths: readonly TsconfigPathsEntry[];
}

/**
 * "Which alias rules apply to this file?" - the tsconfig analog of
 * workspace.ts's package map, and a function rather than a map because the
 * answer depends on where the importer sits: a monorepo has one config per
 * package, and the nearest one wins (see `createTsconfigPathsIndex`).
 */
export type TsconfigPathsIndex = (fromFilePath: string) => TsconfigPathsConfig | null;

/** For callers with no project on disk - tests, and resolution built from a
 * bare `FileExists`. Every file gets "no alias rules". */
export const NO_TSCONFIG_PATHS: TsconfigPathsIndex = () => null;

/** The tsconfig field names read here, and nothing else. */
interface RawTsconfig {
  /** Normalized: absent -> `[]`, a single string -> one element. */
  readonly extends: readonly string[];
  readonly baseUrl?: string;
  readonly paths?: readonly TsconfigPathsEntry[];
}

const CONFIG_FILE_NAMES = ["tsconfig.json", "jsconfig.json"] as const;

/**
 * The alias rules for the file `fromFilePath` names, or `null` when no config
 * above it declares any.
 *
 * Deliberately lazy, unlike `readWorkspacePackages`, which reads every
 * manifest up front: a workspace manifest is one file at a known path, while
 * the configs that could apply here are "one per directory, at any depth", and
 * in a large repo the overwhelming majority of them govern directories no
 * import is ever written in. So the walk happens on first use per directory,
 * and only for directories that actually import something. The
 * once-per-resolver guarantee is unchanged: each config *file* is still read
 * and merged at most once per index instance, however many directories'
 * lookups end up pointing at it.
 *
 * The three memos are the reason this is a factory: like the existence memo in
 * resolve.ts they must not outlive the assumption that the tree is not
 * changing underneath them. A tsconfig created or edited since the last
 * reparse is picked up because `reparseChangedFile` builds a fresh resolver
 * (and therefore a fresh index) per changed file.
 */
export function createTsconfigPathsIndex(projectRoot: string): TsconfigPathsIndex {
  const projectRootAbs = path.resolve(projectRoot);
  const rawCache = new Map<string, RawTsconfig | null>();
  const effectiveCache = new Map<string, TsconfigPathsConfig | null>();
  const nearestCache = new Map<string, TsconfigPathsConfig | null>();

  const readRaw = (absConfigPath: string): RawTsconfig | null => {
    const cached = rawCache.get(absConfigPath);
    if (cached !== undefined) return cached;
    const raw = readRawTsconfig(absConfigPath);
    rawCache.set(absConfigPath, raw);
    return raw;
  };

  const effective = (absConfigPath: string): TsconfigPathsConfig | null =>
    resolveEffectiveConfig(absConfigPath, projectRootAbs, readRaw, effectiveCache, new Set());

  return (fromFilePath: string): TsconfigPathsConfig | null => {
    const startDir = path.join(projectRootAbs, ...path.posix.dirname(fromFilePath).split("/"));
    // A caller is expected to pass a project-relative path, but one that
    // climbs out would start the walk outside the project - the one way this
    // could read a config file that is none of its business.
    if (projectRelativeDir(startDir, projectRootAbs) === null) return null;

    // Every directory walked past shares the answer: none of them held a
    // config, so their nearest one is whatever this walk ends up finding.
    const visited: string[] = [];
    let current = startDir;
    let found: TsconfigPathsConfig | null = null;

    while (true) {
      const cached = nearestCache.get(current);
      if (cached !== undefined) {
        found = cached;
        break;
      }
      visited.push(current);

      // The first directory holding *either* file wins outright, even if that
      // file declares no usable `paths`: a nearer config shadows a more
      // distant one, which is how tsc itself scopes a config to its directory.
      const own = CONFIG_FILE_NAMES.map((name) => path.join(current, name)).find(isFile);
      if (own !== undefined) {
        found = effective(own);
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

/**
 * Every project-relative path `specifier` could name under `config`, in the
 * order the config declares them.
 *
 * All matching keys contribute, not just the most specific one: like
 * `exportsTargets` in workspace.ts this leaves the choice between existing
 * candidates to the filesystem, which is both cheaper than re-implementing
 * tsc's specificity ranking and more useful here - a repo whose `paths` still
 * point at a directory it renamed resolves through whichever key currently
 * names a file, instead of resolving to nothing at all.
 */
export function expandPathsCandidates(config: TsconfigPathsConfig, specifier: string): string[] {
  const candidates: string[] = [];
  for (const { pattern, targets } of config.paths) {
    // A key without `*` is an exact alias for one module, not a prefix.
    let capture: string | null = null;
    if (pattern.includes("*")) {
      capture = matchWildcard(pattern, specifier);
      if (capture === null) continue;
    } else if (pattern !== specifier) {
      continue;
    }

    for (const target of targets) {
      const substituted = capture === null ? target : target.split("*").join(capture);
      const inside = insidePackage(config.resolveDir, substituted);
      if (inside !== null && !candidates.includes(inside)) candidates.push(inside);
    }
  }
  return candidates;
}

// --- reading one config ---------------------------------------------------

/**
 * The three fields this module understands, off one config file, or `null`
 * when the file is missing, unreadable, or not JSON(C) describing an object -
 * a malformed config is one this resolution skips, not a crash, exactly like
 * `readJson`'s policy for a malformed package.json in workspace.ts.
 */
function readRawTsconfig(absConfigPath: string): RawTsconfig | null {
  let text: string;
  try {
    text = fs.readFileSync(absConfigPath, "utf8");
  } catch {
    return null; // no config here - not an error
  }

  const parsed = parseJsonc(text);
  if (!isRecord(parsed)) return null;

  const compilerOptions = isRecord(parsed.compilerOptions) ? parsed.compilerOptions : {};
  return {
    extends: extendsList(parsed.extends),
    baseUrl: typeof compilerOptions.baseUrl === "string" ? compilerOptions.baseUrl : undefined,
    paths: pathsEntries(compilerOptions.paths),
  };
}

/** `extends` is a single string classically and an array since TS 5.0; both
 * become an array here so the merge below has one shape to walk. */
function extendsList(value: unknown): string[] {
  if (typeof value === "string") return [value];
  if (Array.isArray(value)) return value.filter((entry): entry is string => typeof entry === "string");
  return [];
}

/** `paths` as declaration-ordered entries, or `undefined` when the config
 * declares none - the distinction the merge below turns on. */
function pathsEntries(value: unknown): TsconfigPathsEntry[] | undefined {
  if (!isRecord(value)) return undefined;
  const entries: TsconfigPathsEntry[] = [];
  for (const [pattern, targets] of Object.entries(value)) {
    if (!Array.isArray(targets)) continue;
    const strings = targets.filter((target): target is string => typeof target === "string");
    if (strings.length > 0) entries.push({ pattern, targets: strings });
  }
  return entries;
}

// --- the `extends` chain --------------------------------------------------

/**
 * One config's alias rules after its `extends` chain has been applied, or
 * `null` when neither it nor anything it extends declares any.
 *
 * The merge is tsc's, which is coarser than it looks: `paths` is inherited or
 * replaced *whole*, never merged key by key, and only a config that declares
 * `paths` itself gets to say which directory they resolve against. A config
 * that declares just a `baseUrl` therefore changes nothing about inherited
 * aliases - they keep resolving against the directory the ancestor that
 * declared them used.
 *
 * `inProgress` guards a cyclic `extends` chain: a link back into a config
 * already being resolved contributes nothing rather than recursing forever.
 */
function resolveEffectiveConfig(
  absConfigPath: string,
  projectRootAbs: string,
  readRaw: (absConfigPath: string) => RawTsconfig | null,
  effectiveCache: Map<string, TsconfigPathsConfig | null>,
  inProgress: Set<string>,
): TsconfigPathsConfig | null {
  const cached = effectiveCache.get(absConfigPath);
  if (cached !== undefined) return cached;
  if (inProgress.has(absConfigPath)) return null;

  const raw = readRaw(absConfigPath);
  if (raw === null) {
    effectiveCache.set(absConfigPath, null);
    return null;
  }

  inProgress.add(absConfigPath);
  let inherited: TsconfigPathsConfig | null = null;
  for (const entry of raw.extends) {
    const target = extendsTarget(absConfigPath, entry, projectRootAbs);
    if (target === null) continue; // a package extends, or one outside the project
    const contributed = resolveEffectiveConfig(
      target,
      projectRootAbs,
      readRaw,
      effectiveCache,
      inProgress,
    );
    // Later entries layer over earlier ones; one that contributes nothing
    // leaves whatever the chain had so far.
    if (contributed !== null) inherited = contributed;
  }
  inProgress.delete(absConfigPath);

  let result = inherited;
  if (raw.paths !== undefined) {
    const resolveDir = ownResolveDir(absConfigPath, raw.baseUrl, projectRootAbs);
    // A `baseUrl` pointing out of the project voids this config's own
    // declaration - the files it names cannot be in this index - without
    // voiding a valid ancestor's.
    if (resolveDir !== null) result = { resolveDir, paths: raw.paths };
  }

  effectiveCache.set(absConfigPath, result);
  return result;
}

/**
 * The config file an `extends` entry names, or `null` when this module has
 * nothing to say about it: a package specifier (`@tsconfig/node18/tsconfig.json`
 * lives under `node_modules`, whose files are never indexed), a path leaving
 * the project root, or a path with no config file at it. All three are
 * skipped, not errors - the rest of the chain, and the extending config's own
 * fields, still apply.
 */
function extendsTarget(
  absConfigPath: string,
  entry: string,
  projectRootAbs: string,
): string | null {
  if (!/^\.\.?\//.test(entry)) return null;

  const joined = path.resolve(path.dirname(absConfigPath), entry);
  // tsc's own leniency: an entry may name the file, drop the `.json`, or name
  // a directory holding a tsconfig.json.
  const candidates = joined.endsWith(".json")
    ? [joined]
    : [`${joined}.json`, path.join(joined, "tsconfig.json")];

  for (const candidate of candidates) {
    if (projectRelativeDir(candidate, projectRootAbs) === null) continue;
    if (isFile(candidate)) return candidate;
  }
  return null;
}

/**
 * The directory a config's own `paths` resolve against: its `baseUrl` when it
 * declares one, and otherwise the config file's own directory - TypeScript's
 * documented rule for `paths` without `baseUrl`. `null` when that lands
 * outside the project root.
 */
function ownResolveDir(
  absConfigPath: string,
  baseUrl: string | undefined,
  projectRootAbs: string,
): string | null {
  const configDir = path.dirname(absConfigPath);
  const abs = baseUrl === undefined ? configDir : path.resolve(configDir, baseUrl);
  return projectRelativeDir(abs, projectRootAbs);
}

/** `absPath` as a project-relative POSIX path (`""` for the root itself), or
 * `null` when it is not under the project root at all. */
function projectRelativeDir(absPath: string, projectRootAbs: string): string | null {
  const relative = path.relative(projectRootAbs, absPath);
  if (relative === "") return "";
  if (path.isAbsolute(relative) || relative === ".." || relative.startsWith(`..${path.sep}`)) {
    return null;
  }
  return toPosixPath(relative);
}

// --- JSONC ----------------------------------------------------------------

/**
 * `JSON.parse` over what a tsconfig actually is: JSON with comments and
 * trailing commas, both of which the real files in the wild are full of (every
 * `tsc --init` output starts as one big comment block). `null` on anything
 * that does not parse, same policy as everything else read here.
 */
function parseJsonc(text: string): unknown | null {
  try {
    return JSON.parse(stripJsonc(text)) as unknown;
  } catch {
    return null;
  }
}

/**
 * Comments and trailing commas removed, string literals left alone: the whole
 * difficulty is that `"http://example.com"` and `"/* not a comment *\/"` are
 * ordinary JSON strings, so the scan has to know when it is inside one
 * (`\"` escapes included) before it may treat `//` or `,` as syntax.
 *
 * Comments become whitespace rather than disappearing, so offsets stay close
 * to the original and two tokens can never be glued together.
 */
function stripJsonc(text: string): string {
  const out: string[] = [];
  const commas: number[] = [];
  let inString = false;

  for (let index = 0; index < text.length; index += 1) {
    const char = text[index];

    if (inString) {
      out.push(char);
      if (char === "\\") {
        const escaped = text[index + 1];
        if (escaped !== undefined) {
          out.push(escaped);
          index += 1;
        }
        continue;
      }
      if (char === '"') inString = false;
      continue;
    }

    if (char === '"') {
      inString = true;
      out.push(char);
      continue;
    }
    if (char === "/" && text[index + 1] === "/") {
      while (index < text.length && text[index] !== "\n") index += 1;
      out.push("\n");
      continue;
    }
    if (char === "/" && text[index + 1] === "*") {
      index += 2;
      while (index < text.length && !(text[index] === "*" && text[index + 1] === "/")) index += 1;
      index += 1; // the loop's own step then moves past the closing slash
      out.push(" ");
      continue;
    }

    if (char === ",") commas.push(out.length);
    out.push(char);
  }

  // Only now, with comments gone, is "the next thing after this comma" a
  // question plain whitespace-skipping can answer.
  for (const comma of commas) {
    let next = comma + 1;
    while (next < out.length && /\s/.test(out[next])) next += 1;
    if (next < out.length && (out[next] === "}" || out[next] === "]")) out[comma] = " ";
  }
  return out.join("");
}

// --- the filesystem -------------------------------------------------------

function isFile(absolutePath: string): boolean {
  try {
    return fs.statSync(absolutePath).isFile();
  } catch {
    return false;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
