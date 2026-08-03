// The semantic pass: the answers TypeScript's own checker has and the
// structural (tree-sitter) pass could only leave blank.
//
// Core drives this with a `semanticPass` control request carrying the files to
// look at (empty = the whole project, see protocol.ts) and commits whatever
// diff comes back through the pipeline an ordinary reparse already runs. An
// upgraded edge is just that edge re-sent under its own id with a better
// `toId`, `source: "ts-compiler"` and `resolved: true` - `apply_diff`'s
// `ON CONFLICT(id) DO UPDATE` rewrites the row in place, so this needs no
// storage or protocol work of its own (docs/architecture/g-mesh-v1.md,
// "Core <-> language plugin protocol").
//
// ## Which edges it asks about
//
// Only usage edges tree-sitter left `resolved: false` on a *pending symbol*
// placeholder - and of those, only the ones whose target file does not itself
// declare the name. That second filter is what keeps the pass cheap and its
// answers additive:
//
//   - When `./x` really does export a `foo`, core's own breadth-first walk
//     (`core/src/graph/symbol_links.rs`) links the edge without a checker, in
//     a lookup rather than a subprocess round trip. Asking tsserver about it
//     would spend ~2ms to reach the same node and would flip a perfectly good
//     `tree-sitter` edge's `source` for nothing.
//   - When it does not, the name arrives through a re-export chain, and that
//     is exactly the family of questions core's walk documents as beyond it:
//     two `export *` branches offering one name (below), a `default` import of
//     a named default export, an alias chain it cannot see the far end of.
//
// So the questions this pass actually asks are barrel questions, which are a
// small fraction of a project's imports.
//
// ## Two `export *` branches offering one name
//
// `export * from "./a"; export * from "./b"` where both declare `mutate`:
// core's walk finds two equally good candidates one hop down and refuses to
// choose, on its standing rule that a missing edge beats a wrong one. The
// language does have an answer, and it is not "nobody": measured against
// TypeScript 5.9.3 (Node 20.6.1) on exactly that fixture,
//
//   - `tsc --noEmit` reports `TS2308: Module "./a" has already exported a
//     member named 'mutate'` **on the second `export *`** - a diagnostic about
//     the barrel, not an error that removes the name from a consumer's view;
//   - `definition` at a consumer's `import { mutate } from "./index"` returns
//     exactly **one** location, in `a.ts`, and `quickinfo` there reads
//     `(alias) mutate(): "a"` - the first branch's declaration;
//   - swapping the two statements swaps the answer to `b.ts`, so the rule is
//     the *first `export *` in the barrel's source order that offers the name*,
//     not the first file alphabetically and not the shortest chain.
//
// That is `extendExportSymbols` in the checker: the first star export to
// contribute a name keeps it, and every later one only adds to the TS2308
// collision list. Deterministic, and not a rule worth reimplementing here -
// the point of driving the real compiler is that the answer comes from it.
//
// ## What it refuses to answer
//
// The structural pass's rule, unchanged: a missing edge beats a wrong one. An
// answer is dropped when tsserver returns anything other than exactly one
// location, when that location is outside what this project indexes (a
// `node_modules` declaration, a gitignored file), when no declaration node
// covers it, or when the declaration is of the wrong kind for the edge (a
// `CALLS` edge only ever lands on a `Function`). A dropped answer leaves the
// edge exactly as the structural pass left it.

import * as fs from "node:fs/promises";
import * as path from "node:path";

import { walkProjectFiles } from "./bulkIndex";
import {
  extractFile,
  isPlaceholder,
  isSupportedFile,
  PENDING_SYMBOL_NATIVE_KIND,
  type EdgeKind,
  type ExtractedEdge,
  type ExtractedNode,
  type ExtractResult,
  type NodeKind,
} from "./extract";
import { createIndexabilityChecker, toPosixPath } from "./ignorePolicy";
import { cachedExtraction } from "./incremental";
import { createProjectResolver } from "./resolve";
import { semanticProjectFor, type DefinitionLocation, type SemanticProject } from "./semantic";
import { canonicalizeProjectRoot } from "./symlinks";

/**
 * The edge kinds a pending-symbol placeholder can carry, and the node kind
 * each one demands of the symbol it is linked to. Mirrors
 * `LINKABLE_EDGE_KINDS` in core/src/graph/symbol_links.rs: the semantic layer
 * must not land an edge somewhere the structural layer would have refused to,
 * or the two passes would disagree about the same graph.
 */
const LINKABLE_EDGE_KINDS: ReadonlyMap<EdgeKind, NodeKind | null> = new Map<EdgeKind, NodeKind | null>([
  ["CALLS", "Function"],
  ["SUPERTYPE_OF", "Type"],
  ["REFERENCES", null],
]);

/** What one pass has to say. Only ever additions: the pass upgrades rows that
 * already exist and never removes anything, so there is no delete side. */
export interface SemanticPassDiff {
  upsertNodes: ExtractedNode[];
  upsertEdges: ExtractedEdge[];
}

export interface SemanticPassOptions {
  /** The tsserver-backed project to ask. Defaults to the per-root singleton
   * (`semanticProjectFor`), which is what the plugin process uses; tests pass
   * their own so they can dispose of it deterministically. */
  project?: SemanticProject;
  onLog?: (message: string) => void;
}

/** One placeholder's worth of work: the position to ask tsserver about, and
 * every edge that lands on it. All of them get the same answer, so the round
 * trip is per placeholder rather than per usage. */
interface Question {
  /** Project-relative path of the file the import is written in. */
  importer: string;
  placeholder: ExtractedNode;
  edges: ExtractedEdge[];
}

/**
 * Splits a `<file>#<name>` placeholder address on the *last* `#` - a symbol
 * name never contains one, whatever a file path might. The same split core
 * does in `symbol_links::split_address`.
 */
function splitAddress(address: string): { file: string; name: string } | undefined {
  const cut = address.lastIndexOf("#");
  if (cut <= 0 || cut === address.length - 1) return undefined;
  return { file: address.slice(0, cut), name: address.slice(cut + 1) };
}

/** Whether `node`'s span covers a zero-based (line, col). */
function covers(node: ExtractedNode, line: number, col: number): boolean {
  if (line < node.startLine || line > node.endLine) return false;
  if (line === node.startLine && col < node.startCol) return false;
  if (line === node.endLine && col > node.endCol) return false;
  return true;
}

/** Whether `a` spans strictly less source than `b` - the tie-break that picks
 * a method over the class containing it. */
function isTighter(a: ExtractedNode, b: ExtractedNode): boolean {
  const aLines = a.endLine - a.startLine;
  const bLines = b.endLine - b.startLine;
  if (aLines !== bLines) return aLines < bLines;
  return a.endCol - a.startCol < b.endCol - b.startCol;
}

/**
 * Answers what the checker can about the files in `filePaths` (empty = the
 * whole project) and returns it as a diff core can commit unchanged.
 *
 * Never throws for a question it could not answer: a dead tsserver, a file
 * that vanished, a `No Project` refusal are all reported through `onLog` and
 * cost exactly the edges they were about. The pass is an upgrade over a graph
 * that is already committed and serviceable (core drops a failing one on the
 * floor by design), so a partial answer is always better than none.
 *
 * The tsserver child is started lazily by the first question actually asked -
 * a pass that finds nothing to ask about spawns no checker at all, which is
 * the common case for a project with no barrels.
 */
export async function runSemanticPass(
  projectRoot: string,
  filePaths: readonly string[],
  options: SemanticPassOptions = {},
): Promise<SemanticPassDiff> {
  const log = options.onLog ?? ((): void => {});
  const resolveSpecifier = createProjectResolver(projectRoot);
  const isIndexable = createIndexabilityChecker(projectRoot);
  const projectRelative = makeProjectRelative(projectRoot);
  // One extraction per file for the whole pass. Both halves of the work need
  // it - the importer's placeholders on the way in, the target's declarations
  // on the way out - and a barrel is by definition asked about repeatedly.
  const extractions = new Map<string, ExtractResult | null>();

  async function extractionOf(relPath: string): Promise<ExtractResult | null> {
    const memoized = extractions.get(relPath);
    if (memoized !== undefined) return memoized;

    let result = cachedExtraction(relPath) ?? null;
    if (result === null) {
      try {
        const sourceText = await fs.readFile(path.join(projectRoot, relPath), "utf8");
        result = extractFile(relPath, sourceText, { resolveSpecifier });
      } catch {
        result = null; // vanished, unreadable, or an extension this plugin does not own
      }
    }
    extractions.set(relPath, result);
    return result;
  }

  /** Whether `file` itself declares an exported `name` - i.e. whether core's
   * own walk already has everything it needs and this pass should stay out of
   * the way. */
  async function declaresExport(file: string, name: string): Promise<boolean> {
    const result = await extractionOf(file);
    if (result === null) return false;
    return result.nodes.some((node) => node.exported && node.name === name && !isPlaceholder(node));
  }

  const scope = filePaths.length > 0 ? filePaths : await collectProjectFiles(projectRoot);
  const questions: Question[] = [];

  for (const file of scope) {
    const result = await extractionOf(file);
    if (result === null) continue;

    const byId = new Map(result.nodes.map((node) => [node.id, node]));
    const byPlaceholder = new Map<string, Question>();

    for (const edge of result.edges) {
      if (edge.resolved) continue; // the structural pass already answered this one
      if (!LINKABLE_EDGE_KINDS.has(edge.kind)) continue;
      const placeholder = byId.get(edge.toId);
      if (placeholder === undefined || placeholder.nativeKind !== PENDING_SYMBOL_NATIVE_KIND) continue;

      const existing = byPlaceholder.get(placeholder.id);
      if (existing !== undefined) {
        existing.edges.push(edge);
        continue;
      }
      byPlaceholder.set(placeholder.id, { importer: file, placeholder, edges: [edge] });
    }

    for (const question of byPlaceholder.values()) {
      const address = splitAddress(question.placeholder.qualifiedName);
      if (address === undefined) continue;
      // The target declares it: core links this without a checker.
      if (await declaresExport(address.file, address.name)) continue;
      questions.push(question);
    }
  }

  if (questions.length === 0) return { upsertNodes: [], upsertEdges: [] };

  log(`semantic pass: ${questions.length} re-exported name(s) to ask the compiler about`);
  const project = options.project ?? semanticProjectFor(projectRoot, { onLog: log });

  const upsertEdges: ExtractedEdge[] = [];
  const upsertNodes = new Map<string, ExtractedNode>();

  for (const question of questions) {
    let definitions: DefinitionLocation[];
    try {
      definitions = await project.definition(path.join(projectRoot, question.importer), {
        // The placeholder is anchored at the import specifier's local name
        // (`Extractor.bindImport`), which is precisely the token whose
        // declaration is being asked for. tree-sitter counts from zero and
        // tsserver from one.
        line: question.placeholder.startLine + 1,
        offset: question.placeholder.startCol + 1,
      });
    } catch (err) {
      log(`semantic pass: ${(err as Error).message}`);
      continue;
    }

    // Exactly one, or nothing: several declarations is a merged symbol whose
    // pieces this graph does not distinguish between yet, and choosing one of
    // them would be the guess the whole pipeline refuses to make.
    if (definitions.length !== 1) continue;
    const target = await declarationAt(definitions[0]);
    if (target === undefined) continue;

    for (const edge of question.edges) {
      const required = LINKABLE_EDGE_KINDS.get(edge.kind);
      if (required !== undefined && required !== null && target.kind !== required) continue;
      if (target.id === edge.fromId) continue; // a self-edge carries no information
      upsertEdges.push({ ...edge, toId: target.id, source: "ts-compiler", resolved: true });
      upsertNodes.set(target.id, target);
    }
  }

  return { upsertNodes: [...upsertNodes.values()], upsertEdges };

  /**
   * The declaration node a tsserver `definition` answer names, or undefined if
   * this index does not hold one.
   *
   * The node is re-sent with the edge rather than assumed to be in the index
   * already. It costs nothing when it is - the id is derived from the same
   * (path, kind, qualifiedName) the walk derived it from, so the upsert is a
   * no-op - and it is what keeps the diff self-sufficient when it is not,
   * instead of failing core's foreign key and taking the whole pass down with
   * it.
   */
  async function declarationAt(definition: DefinitionLocation): Promise<ExtractedNode | undefined> {
    const relative = projectRelative(definition.file);
    // Outside the project, or somewhere the walk would never give a `File`
    // node: a `node_modules` or gitignored declaration is a real answer to the
    // language's question and not one this index can point at.
    if (relative === undefined) return undefined;
    if (!isSupportedFile(relative) || !isIndexable(relative)) return undefined;

    const result = await extractionOf(relative);
    if (result === null) return undefined;

    const line = definition.start.line - 1;
    const col = definition.start.offset - 1;
    let best: ExtractedNode | undefined;
    for (const node of result.nodes) {
      // A `File` node covers everything and names nothing; a placeholder is
      // the re-export statement the chain passes *through*, never its end.
      if (node.kind === "File" || isPlaceholder(node)) continue;
      if (!covers(node, line, col)) continue;
      if (best === undefined || isTighter(node, best)) best = node;
    }
    return best;
  }
}

/**
 * An absolute path as this index spells it - project-relative POSIX - or
 * undefined when it is not under the project at all.
 *
 * Tried against the canonical root as well as the one this pass was handed,
 * because the two are routinely different spellings of the same directory
 * (`/tmp` is a symlink to `/private/tmp` on macOS) and tsserver answers in
 * whichever one it was asked in. Comparing only one way would read a
 * perfectly ordinary in-project declaration as "outside the project" and drop
 * the answer.
 */
function makeProjectRelative(projectRoot: string): (absolute: string) => string | undefined {
  const roots = new Set([path.resolve(projectRoot), canonicalizeProjectRoot(projectRoot)]);
  return (absolute) => {
    for (const root of roots) {
      const relative = toPosixPath(path.relative(root, absolute));
      if (relative === "" || relative.startsWith("../") || path.isAbsolute(relative)) continue;
      return relative;
    }
    return undefined;
  };
}

/** Every file the walk would index, as project-relative POSIX paths. The
 * whole-project scope - an empty `filePaths` means "everything indexed so
 * far", which after a cold start is the whole tree. */
async function collectProjectFiles(projectRoot: string): Promise<string[]> {
  const files: string[] = [];
  for await (const relPath of walkProjectFiles(projectRoot)) files.push(relPath);
  return files;
}
