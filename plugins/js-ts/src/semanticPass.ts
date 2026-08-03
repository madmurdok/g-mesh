// The `semanticPass` control method's answer: what TypeScript's own checker
// can resolve that the structural (tree-sitter) pass could not.
//
// Today that is one thing - `import * as ns from "./mod"` followed by
// `ns.someExport()` - and it is unlike the other cross-file gaps in one
// important way: there is no unresolved edge to *upgrade*. The structural pass
// never sees the bare name `someExport` at the use site at all, only the
// receiver `ns` and a property access, so it emits nothing, deliberately
// (`Extractor.recordImportBindings`). What this pass produces is therefore a
// new edge, not a better `source` on an existing one - which is why the gap was
// invisible rather than merely unconfirmed: `find_callers` on a symbol every
// caller reached through a namespace import answered nothing at all.
//
// ## How an answer becomes an edge
//
// Not by pointing the edge at the declaration's node id, which this process
// could compute (ids are a pure function of path/kind/qualifiedName) and must
// not: whether the declaring file is in the index at all is a fact only core
// holds - it may be gitignored, excluded, or simply not walked yet - and the
// whole placeholder handshake exists because a per-file pass cannot know it
// (see `Extractor.recordImport`'s note on why edges never leave their file).
//
// So the pass emits exactly what a *named* import of the same symbol would
// have emitted: a pending-symbol placeholder addressed `<file>#<name>`, plus
// the usage edge hanging on it. Core's `graph::symbol_links` then does what it
// already does for every other cross-file usage - finds the export, repoints
// the edge, marks it resolved - with no change of its own required. The
// difference from a tree-sitter placeholder is only which file and name it is
// addressed with, and that `source` says `ts-compiler`: the address comes from
// `tsserver`'s `definition` at the use site, so it names the declaration
// TypeScript itself binds, through re-export chains, `paths` aliases and
// aliased exports that a name-matching pass would have to guess at.
//
// ## What it costs when there is nothing to do
//
// The common file has no namespace import, and must not pay for this. Three
// gates, cheapest first: a substring test on the raw source before anything is
// parsed, then the extraction's own (usually empty) list of member uses, and
// only if some file has one does a `tsserver` child get started at all.

import * as fs from "node:fs/promises";
import * as path from "node:path";

import { walkProjectFiles } from "./bulkIndex";
import {
  edgeIdFor,
  extractFile,
  isSupportedFile,
  nodeIdFor,
  pendingSymbolQualifiedName,
  PENDING_SYMBOL_NATIVE_KIND,
  type EdgeSource,
  type ExtractResult,
  type ExtractedEdge,
  type ExtractedNode,
  type NamespaceMemberUse,
} from "./extract";
import { toPosixPath } from "./ignorePolicy";
import { cachedExtraction } from "./incremental";
import { createProjectResolver } from "./resolve";
import { semanticProjectFor, type DefinitionLocation, type SemanticProject } from "./semantic";

/** `source` of every edge this pass writes - the whole point of it. The
 * counterpart to extract.ts's `EDGE_SOURCE`, which is the other value. */
const SEMANTIC_EDGE_SOURCE: EdgeSource = "ts-compiler";

/**
 * How many questions in a row may fail to reach the checker at all before the
 * rest of the pass stops asking.
 *
 * `SemanticProject` replaces a dead child lazily, on the next query - which is
 * right for a one-off crash and wrong for a checker that cannot start on this
 * project at all, where it would mean one ~1.2s spawn per remaining question.
 * This turns that into a bounded cost and one honest outcome: the edges stay
 * missing, which is the state the graph was in a moment ago. A question the
 * checker *answers* with "nothing is declared there" resets the count - that is
 * a working child, not a failing one.
 */
const MAX_CONSECUTIVE_FAILURES = 5;

/**
 * The cheapest possible "could this file even have a namespace import", run on
 * the raw text so the overwhelming majority of a project's files are skipped
 * before they are parsed at all. `import * as ns from "./x"` cannot be written
 * without a `*` followed by `as`, whatever whitespace sits between them.
 *
 * Bare `*` alone would be useless - every file with a JSDoc block has one - so
 * the `as` is what makes this a filter rather than a formality. False positives
 * (a JSDoc line reading `* as ...`) only cost the parse; the direction that
 * matters is that there are no false negatives.
 */
const NAMESPACE_IMPORT_HINT = /\*\s*as\s/;

function mightHaveNamespaceImport(sourceText: string): boolean {
  return NAMESPACE_IMPORT_HINT.test(sourceText);
}

/** What the pass hands back, in the shape index.ts puts on the wire. */
export interface SemanticPassResult {
  upsertNodes: ExtractedNode[];
  upsertEdges: ExtractedEdge[];
  deleteEdgeIds: string[];
  /** Files actually looked at, for the log line. */
  filesScanned: number;
  /** Member uses that found no single declaration, likewise. */
  unresolvedUses: number;
}

/** A function rather than a shared constant: the arrays are the caller's to
 * keep, and one frozen instance handed out repeatedly would be aliased. */
function emptyResult(): SemanticPassResult {
  return { upsertNodes: [], upsertEdges: [], deleteEdgeIds: [], filesScanned: 0, unresolvedUses: 0 };
}

/**
 * The edge ids this pass last wrote for a file. Process-lifetime only, exactly
 * like incremental.ts's parse cache and for the same reason - the cold path is
 * a full rebuild, so there is nothing to persist.
 *
 * Its job is deletions. A `ns.someExport()` call that an edit removes leaves an
 * edge nothing else can retract: the structural reparse diff never knew about
 * it, so it cannot list it as removed. Remembering what this pass emitted, and
 * diffing the next emission against it, is what keeps a deleted call from
 * outliving its source line.
 */
const emittedEdgesByFile = new Map<string, Set<string>>();

/** Test seam; also the right response to a full reindex. */
export function resetSemanticPassState(): void {
  emittedEdgesByFile.clear();
}

export interface SemanticPassOptions {
  /** Diagnostics sink - the pass never throws at its caller for one file. */
  onLog?: (message: string) => void;
  /** Override the semantic project (tests, a pinned compiler). */
  project?: SemanticProject;
}

/**
 * Runs the pass over `filePaths`, or over the whole project when it is empty -
 * the wire's own convention for "everything" (see core's
 * `ControlMessage::SemanticPass`).
 *
 * Never throws for a single file: a file that cannot be read, parsed, or
 * answered about contributes nothing and the pass carries on. The failure mode
 * this layer is allowed to have is "that edge stays missing", never "the
 * upgrade took the reparse down with it".
 */
export async function runSemanticPass(
  projectRoot: string,
  filePaths: readonly string[],
  options: SemanticPassOptions = {},
): Promise<SemanticPassResult> {
  const scope = filePaths.length > 0 ? filePaths.map(toPosixPath) : await allProjectFiles(projectRoot);

  // Collected before a child is spawned: a project (or an edit) with no
  // namespace import at all must not pay tsserver's ~1.2s and ~265MB to be
  // told there was nothing to ask.
  const perFile: ScopedFile[] = [];
  let filesScanned = 0;
  for (const filePath of scope) {
    const extraction = await extractionFor(projectRoot, filePath, options);
    if (extraction === null) {
      // Nothing to say about it *now* - but a file that had something to say
      // last time has to stay in scope anyway, or an edit deleting its last
      // namespace import would leave the edges it used to have behind. It is
      // exactly the file this cheap gate skips that needs retracting.
      if (emittedEdgesByFile.has(filePath)) {
        perFile.push({ filePath, uses: [], language: "", structural: new Map() });
      }
      continue;
    }
    filesScanned += 1;
    // A file with no uses is kept too, for the same reason and one gate later.
    perFile.push({
      filePath,
      uses: extraction.namespaceMemberUses,
      language: languageOf(extraction),
      structural: new Map(extraction.nodes.map((node) => [node.id, node])),
    });
  }

  const work = perFile.filter((entry) => entry.uses.length > 0);
  if (work.length === 0) {
    return { ...emptyResult(), ...retractions(perFile), filesScanned };
  }

  const project = options.project ?? semanticProjectFor(projectRoot, { onLog: options.onLog });
  const declarations = new DeclarationReader(projectRoot);

  const upsertNodes = new Map<string, ExtractedNode>();
  const upsertEdges = new Map<string, ExtractedEdge>();
  const emitted = new Map<string, Set<string>>(perFile.map((entry) => [entry.filePath, new Set()]));
  let unresolvedUses = 0;
  let consecutiveFailures = 0;

  for (const entry of work) {
    const absPath = path.join(projectRoot, entry.filePath);
    // One question per distinct `ns.member` in a file: every site spelling it
    // the same way binds the same declaration, and the checking is what the
    // pass actually spends its time on.
    const answers = new Map<string, { declPath: string; declName: string } | null>();

    for (const use of entry.uses) {
      const key = `${use.namespaceName}.${use.memberName}`;
      if (!answers.has(key)) {
        if (consecutiveFailures >= MAX_CONSECUTIVE_FAILURES) {
          unresolvedUses += 1;
          continue;
        }
        const outcome = await resolveMemberDeclaration(project, declarations, absPath, use, options);
        consecutiveFailures = outcome.asked ? 0 : consecutiveFailures + 1;
        answers.set(key, outcome.address);
      }
      const answer = answers.get(key) ?? null;
      if (answer === null) {
        unresolvedUses += 1;
        continue;
      }

      const qualifiedName = pendingSymbolQualifiedName(answer.declPath, answer.declName);
      const id = nodeIdFor(entry.filePath, "Module", qualifiedName, PENDING_SYMBOL_NATIVE_KIND);
      // A file that *also* imports the same symbol by name already has this
      // exact placeholder, down to the id - the address is all a node id is
      // derived from. Reusing the structural pass's own record keeps the two
      // from overwriting each other's source position on every pass; only
      // where there is no such import is one built here.
      const placeholder: ExtractedNode = entry.structural.get(id) ?? {
        id,
        kind: "Module",
        name: answer.declName,
        qualifiedName,
        // The *importing* file: that is where the usage is written, and it is
        // what makes the placeholder go away with the file that needed it.
        filePath: entry.filePath,
        // Anchored at the `import * as ns` binding, as an ordinary import
        // placeholder is anchored at its own - a symbol used in ten places
        // still has one placeholder, so a use site would be an arbitrary
        // choice among them.
        startLine: use.bindingLine,
        startCol: use.bindingCol,
        endLine: use.bindingLine,
        endCol: use.bindingCol + use.namespaceName.length,
        exported: false,
        language: entry.language,
        nativeKind: PENDING_SYMBOL_NATIVE_KIND,
        hasSyntaxErrors: false,
      };
      upsertNodes.set(placeholder.id, placeholder);

      const edge: ExtractedEdge = {
        id: edgeIdFor(use.fromId, use.edgeKind, placeholder.id),
        fromId: use.fromId,
        toId: placeholder.id,
        kind: use.edgeKind,
        source: SEMANTIC_EDGE_SOURCE,
        // Pointing at a placeholder *is* what unresolved means here, whichever
        // pass built it: only core can confirm the target file exports the
        // name, and it says so by repointing this edge (`graph::symbol_links`).
        resolved: false,
      };
      upsertEdges.set(edge.id, edge);
      emitted.get(entry.filePath)?.add(edge.id);
    }
  }

  const deleteEdgeIds: string[] = [];
  for (const [filePath, ids] of emitted) {
    for (const previous of emittedEdgesByFile.get(filePath) ?? []) {
      if (!ids.has(previous) && !upsertEdges.has(previous)) deleteEdgeIds.push(previous);
    }
    if (ids.size === 0) emittedEdgesByFile.delete(filePath);
    else emittedEdgesByFile.set(filePath, ids);
  }

  return {
    upsertNodes: [...upsertNodes.values()],
    upsertEdges: [...upsertEdges.values()],
    deleteEdgeIds,
    filesScanned,
    unresolvedUses,
  };
}

/** One file in the pass's scope, and everything it took to decide that. */
interface ScopedFile {
  filePath: string;
  uses: NamespaceMemberUse[];
  language: string;
  /** The structural extraction's nodes by id, so a placeholder the
   * name-matching layer already built is reused rather than rebuilt. */
  structural: Map<string, ExtractedNode>;
}

/**
 * The retraction-only answer for a scope that turned out to have no namespace
 * member uses left in it - which is what deleting the last one from a file
 * looks like from here.
 */
function retractions(
  perFile: readonly { filePath: string }[],
): Pick<SemanticPassResult, "deleteEdgeIds"> {
  const deleteEdgeIds: string[] = [];
  for (const { filePath } of perFile) {
    const previous = emittedEdgesByFile.get(filePath);
    if (previous === undefined) continue;
    deleteEdgeIds.push(...previous);
    emittedEdgesByFile.delete(filePath);
  }
  return { deleteEdgeIds };
}

async function allProjectFiles(projectRoot: string): Promise<string[]> {
  const files: string[] = [];
  for await (const relPath of walkProjectFiles(projectRoot)) files.push(relPath);
  return files;
}

function languageOf(extraction: ExtractResult): string {
  return extraction.nodes.find((node) => node.kind === "File")?.language ?? "typescript";
}

/**
 * The file's extraction, reusing incremental.ts's cache when it has one - that
 * copy is by construction the one core was last told about, so the ids an edge
 * is written from are the ids actually in the index.
 *
 * Returns null for anything this pass has nothing to say about: a file it does
 * not parse, one that vanished, one whose text cannot contain a namespace
 * import at all ([`mightHaveNamespaceImport`]), or one whose parse threw.
 */
async function extractionFor(
  projectRoot: string,
  filePath: string,
  options: SemanticPassOptions,
): Promise<ExtractResult | null> {
  const cached = cachedExtraction(filePath);
  if (cached !== undefined) return cached;
  if (!isSupportedFile(filePath)) return null;

  let sourceText: string;
  try {
    sourceText = await fs.readFile(path.join(projectRoot, filePath), "utf8");
  } catch {
    return null; // deleted, unreadable - the next reparse is the one that matters
  }
  if (!mightHaveNamespaceImport(sourceText)) return null;

  try {
    return extractFile(filePath, sourceText, { resolveSpecifier: createProjectResolver(projectRoot) });
  } catch (err) {
    options.onLog?.(`semantic pass could not parse ${filePath}: ${(err as Error).message}`);
    return null;
  }
}

/**
 * Where `ns.member` really is declared, as a `<project-relative file, declared
 * name>` address - or null when the answer is anything less than unambiguous.
 *
 * Deliberately *not* "the module `ns` names, plus `member`". Asking the checker
 * is the whole difference: it follows the alias chain, so a member republished
 * by a barrel, renamed on the way (`export { impl as someExport }`), or reached
 * through a `paths` alias resolves to the file that really declares it. Where
 * it stops at a re-export statement instead, the address names that barrel and
 * core's own re-export walk finishes the job - both ends of the handshake
 * already handle the case.
 *
 * Refused, on the standing rule that a missing edge beats a wrong one:
 *  - no definition, or several that disagree about which declaration they are
 *    (a `class`/`interface` merge - one symbol to TypeScript, two nodes here);
 *  - a declaration outside this project, or in a file this plugin does not
 *    parse: `node_modules`, an ambient `.d.ts`, a JSON module. Nothing there is
 *    in the index, so a placeholder addressed at it could never be linked.
 */
async function resolveMemberDeclaration(
  project: SemanticProject,
  declarations: DeclarationReader,
  absPath: string,
  use: NamespaceMemberUse,
  options: SemanticPassOptions,
): Promise<{ address: { declPath: string; declName: string } | null; asked: boolean }> {
  let locations: DefinitionLocation[];
  try {
    // tree-sitter counts rows/columns from zero, tsserver lines/offsets from
    // one; both count UTF-16 code units, so this is the whole conversion.
    locations = await project.definition(absPath, { line: use.line + 1, offset: use.col + 1 });
  } catch (err) {
    options.onLog?.(
      `semantic pass could not resolve ${use.namespaceName}.${use.memberName}: ${(err as Error).message}`,
    );
    // `asked: false` - the checker was never reached, which is what
    // MAX_CONSECUTIVE_FAILURES counts. A refusal below is a different thing.
    return { address: null, asked: false };
  }

  const addresses = new Map<string, { declPath: string; declName: string }>();
  for (const location of locations) {
    const address = await declarations.addressOf(location);
    if (address !== null) addresses.set(`${address.declPath}#${address.declName}`, address);
  }
  if (addresses.size !== 1) return { address: null, asked: true };
  return { address: [...addresses.values()][0], asked: true };
}

/**
 * Turns `tsserver`'s answer - an absolute path and the span of the declared
 * *name* - into the `<file, name>` pair a placeholder is addressed by, reading
 * each declaring file at most once per pass.
 *
 * The name has to be read out of the source because `definition` reports where
 * it is, not what it says, and the two differ exactly when it matters: an
 * aliased re-export lands on the original name, not the one written at the use
 * site.
 */
class DeclarationReader {
  private readonly lines = new Map<string, string[] | null>();

  constructor(private readonly projectRoot: string) {}

  async addressOf(
    location: DefinitionLocation,
  ): Promise<{ declPath: string; declName: string } | null> {
    // A name never spans lines, so anything that claims to is not one.
    if (location.start.line !== location.end.line) return null;

    const relative = path.relative(this.projectRoot, location.file);
    // `..` means outside the project root; an absolute result means a
    // different volume. Either way it is not a file this index contains.
    if (relative.startsWith("..") || path.isAbsolute(relative)) return null;
    const declPath = toPosixPath(relative);
    if (!isSupportedFile(declPath)) return null;
    if (declPath.split("/").includes("node_modules")) return null;

    const lines = await this.linesOf(location.file);
    if (lines === null) return null;
    const line = lines[location.start.line - 1];
    if (line === undefined) return null;

    const declName = line.slice(location.start.offset - 1, location.end.offset - 1);
    // Guards against a span that is not an identifier at all - a string-literal
    // export name (`export { x as "a b" }`), a computed member, a `default`
    // keyword sitting where a name would be. `#` in particular must never
    // appear: it is the separator the address itself is built from.
    if (!/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(declName)) return null;

    return { declPath, declName };
  }

  private async linesOf(file: string): Promise<string[] | null> {
    const cached = this.lines.get(file);
    if (cached !== undefined) return cached;
    let lines: string[] | null;
    try {
      lines = (await fs.readFile(file, "utf8")).split("\n");
    } catch {
      lines = null;
    }
    this.lines.set(file, lines);
    return lines;
  }
}
