// The `semanticPass` control method's answer: what TypeScript's own checker
// can resolve that the structural (tree-sitter) pass could not.
//
// Core drives this with a `semanticPass` request carrying the files to look at
// (empty = the whole project, see protocol.ts) and commits whatever diff comes
// back through the pipeline an ordinary reparse already runs - so nothing here
// needs storage or protocol work of its own (docs/architecture/g-mesh-v1.md,
// "Core <-> language plugin protocol").
//
// ## The two questions it asks
//
// They are opposite shapes, and the pass does both in one sweep because they
// share every expensive thing they need: one extraction per file, one tsserver
// child, one failure budget.
//
//  1. **A namespace member use** - `import * as ns from "./mod"` then
//     `ns.someExport()`. There is no unresolved edge to upgrade here: the
//     structural pass never sees the bare name `someExport` at the use site,
//     only the receiver and a property access, so it emits nothing at all,
//     deliberately (`Extractor.recordImportBindings`). The gap was therefore
//     *invisible* rather than merely unconfirmed - `find_callers` on a symbol
//     every caller reached through a namespace import answered nothing. What
//     this pass produces for it is a **new** edge.
//
//  2. **An unresolved edge through a re-export chain** - most sharply, a
//     barrel with two `export *` branches offering one name. tree-sitter does
//     emit an edge here; core's own breadth-first walk
//     (`core/src/graph/symbol_links.rs`) then finds two equally good
//     candidates one hop down and refuses to choose, on the standing rule that
//     a missing edge beats a wrong one. What this pass produces for it is the
//     **same** edge re-sent under its own id with a better `toId`,
//     `source: "ts-compiler"` and `resolved: true`; `apply_diff`'s
//     `ON CONFLICT(id) DO UPDATE` rewrites the row in place.
//
// ## Why the first emits a placeholder and the second a real node id
//
// Not an inconsistency - a difference in what each one knows.
//
// The namespace case must not point the edge at the declaration's node id,
// which it could compute (ids are a pure function of path/kind/qualifiedName)
// and must not: whether the declaring file is in the index at all is a fact
// only core holds - it may be gitignored, excluded, or simply not walked yet -
// and the whole placeholder handshake exists because a per-file pass cannot
// know it (see `Extractor.recordImport`'s note on why edges never leave their
// file). So it emits exactly what a *named* import of the same symbol would
// have: a pending-symbol placeholder addressed `<file>#<name>`, plus the usage
// edge hanging on it, `resolved: false`. Core's `graph::symbol_links` then does
// what it already does for every other cross-file usage - finds the export,
// repoints the edge, marks it resolved - with no change of its own required.
// The address is the entire contract; that it came from the checker shows only
// in `source`, and buys the alias chain, `paths` aliases and aliased exports a
// name-matching pass would have to guess at.
//
// The re-export case is past that handshake: the edge already exists, already
// hangs on a placeholder core could not link, and the checker's answer is a
// declaration site this pass can look at and match against a node it extracted
// itself. Handing back another placeholder would only re-ask the question core
// has already failed to answer. It sends the target node along with the edge,
// so the diff cannot land core in a dangling-foreign-key state whatever it
// already holds.
//
// ## Which unresolved edges are worth asking about
//
// Only those whose target file does not itself declare the name. Where it does,
// core's walk links the edge in a lookup rather than a subprocess round trip,
// and asking would spend ~2ms to reach the same node and flip a perfectly good
// `tree-sitter` edge's `source` for nothing. Where it does not, the name
// arrives through a re-export chain - which is exactly the family core's walk
// documents as beyond it: two `export *` branches offering one name, a
// `default` import of a named default export, an alias chain it cannot see the
// far end of. So the questions actually asked are barrel questions, a small
// fraction of a project's imports.
//
// ## Two `export *` branches offering one name
//
// `export * from "./a"; export * from "./b"` where both declare `mutate`.
// The language does have an answer, and it is not "nobody": measured against
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
// ## What it costs when there is nothing to do
//
// One gate, and it is the one that matters: a `tsserver` child (~1.2s, ~265MB)
// is started only if the scan found a question to ask. Everything before that
// is extraction, which the second question needs for every file in scope
// anyway - it has to see the *edges* to know whether any were left unresolved,
// and no amount of squinting at raw text answers that. (An earlier
// namespace-only version of this pass gated on a `/\*\s*as\s/` test over the
// source before parsing. That gate is gone rather than kept for half the work:
// all it can now skip is a parse the other half performs regardless, so the
// only thing it would buy is a second code path to keep honest.)
//
// ## What it refuses to answer
//
// The structural pass's rule, unchanged: a missing edge beats a wrong one. An
// answer is dropped when tsserver returns anything other than exactly one
// declaration, when that declaration is outside what this project indexes (a
// `node_modules` or gitignored file, an ambient `.d.ts`, a JSON module), when
// no declaration node covers it, or when it is of the wrong kind for the edge
// (a `CALLS` edge only ever lands on a `Function`). A dropped answer leaves the
// edge exactly as the structural pass left it.

import * as fs from "node:fs/promises";
import * as path from "node:path";

import { walkProjectFiles } from "./bulkIndex";
import {
  edgeIdFor,
  extractFile,
  isPlaceholder,
  isSupportedFile,
  nodeIdFor,
  pendingSymbolQualifiedName,
  PENDING_SYMBOL_NATIVE_KIND,
  type EdgeKind,
  type EdgeSource,
  type ExtractedEdge,
  type ExtractedNode,
  type ExtractResult,
  type NamespaceMemberUse,
  type NodeKind,
  type SpecifierResolver,
} from "./extract";
import { createIndexabilityChecker, toPosixPath } from "./ignorePolicy";
import { cachedExtraction } from "./incremental";
import { createProjectResolver } from "./resolve";
import { semanticProjectFor, type DefinitionLocation, type SemanticProject } from "./semantic";
import { canonicalizeProjectRoot } from "./symlinks";

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

/** Stand-in for "this file emitted nothing this time", so the retraction loop
 * needs no special case for the overwhelmingly common file. */
const NO_EDGE_IDS: ReadonlySet<string> = new Set<string>();

/** What the pass hands back, in the shape index.ts puts on the wire. */
export interface SemanticPassResult {
  upsertNodes: ExtractedNode[];
  upsertEdges: ExtractedEdge[];
  /**
   * Only ever edges this pass itself wrote for a namespace member use that has
   * since gone. An upgraded edge is never retracted here: it is the structural
   * pass's own edge, and deleting the call deletes it through the ordinary
   * reparse diff.
   */
  deleteEdgeIds: string[];
  /** Files actually looked at, for the log line. */
  filesScanned: number;
  /** Questions that reached no single declaration, likewise. */
  unresolvedUses: number;
}

export interface SemanticPassOptions {
  /** Diagnostics sink - the pass never throws at its caller for one file. */
  onLog?: (message: string) => void;
  /** The tsserver-backed project to ask. Defaults to the per-root singleton
   * (`semanticProjectFor`), which is what the plugin process uses; tests pass
   * their own so they can dispose of it deterministically. */
  project?: SemanticProject;
}

/**
 * The edge ids this pass last wrote for a file. Process-lifetime only, exactly
 * like incremental.ts's parse cache and for the same reason - the cold path is
 * a full rebuild, so there is nothing to persist.
 *
 * Its job is deletions, and only for the namespace half. A `ns.someExport()`
 * call that an edit removes leaves an edge nothing else can retract: the
 * structural reparse diff never knew about it, so it cannot list it as removed.
 * Remembering what this pass emitted, and diffing the next emission against it,
 * is what keeps a deleted call from outliving its source line.
 */
const emittedEdgesByFile = new Map<string, Set<string>>();

/** Test seam; also the right response to a full reindex. */
export function resetSemanticPassState(): void {
  emittedEdgesByFile.clear();
}

/**
 * Runs the pass over `filePaths`, or over the whole project when it is empty -
 * the wire's own convention for "everything" (see core's
 * `ControlMessage::SemanticPass`).
 *
 * Never throws for a single file or a single question: a file that cannot be
 * read or parsed, a dead tsserver, a `No Project` refusal are all reported
 * through `onLog` and cost exactly the edges they were about. The pass is an
 * upgrade over a graph that is already committed and serviceable (core drops a
 * failing one on the floor by design), so a partial answer is always better
 * than none, and "that edge stays missing" is the only failure mode this layer
 * is allowed to have.
 *
 * The tsserver child is started lazily by the first question actually asked - a
 * pass that finds nothing to ask about spawns no checker at all, which is the
 * common case for a project with neither namespace imports nor barrels.
 */
export async function runSemanticPass(
  projectRoot: string,
  filePaths: readonly string[],
  options: SemanticPassOptions = {},
): Promise<SemanticPassResult> {
  const scope = filePaths.length > 0 ? filePaths.map(toPosixPath) : await allProjectFiles(projectRoot);
  const index = new ProjectIndex(projectRoot, options);

  const scanned: ScopedFile[] = [];
  let filesScanned = 0;
  for (const filePath of scope) {
    const extraction = await index.extractionOf(filePath);
    // Nothing to say about it *now* - but a file that had something to say last
    // time still has to be answered for, or an edit deleting its last namespace
    // member use would leave the edges it used to have behind. That is why the
    // retraction loop walks `scope` rather than this list: a file that vanished
    // needs no entry here to be retracted.
    if (extraction === null) continue;
    filesScanned += 1;
    scanned.push({
      filePath,
      language: languageOf(extraction),
      structural: new Map(extraction.nodes.map((node) => [node.id, node])),
      uses: extraction.namespaceMemberUses,
      upgrades: await upgradeQuestionsIn(filePath, extraction, index),
    });
  }

  const out = new PassOutput();
  const namespaceWork = scanned.filter((entry) => entry.uses.length > 0);
  const upgradeWork = scanned.flatMap((entry) => entry.upgrades);

  if (namespaceWork.length > 0 || upgradeWork.length > 0) {
    options.onLog?.(
      `semantic pass: ${namespaceWork.length} file(s) with namespace member uses, ` +
        `${upgradeWork.length} re-exported name(s) to ask the compiler about`,
    );
    const checker = new Checker(
      options.project ?? semanticProjectFor(projectRoot, { onLog: options.onLog }),
      options,
    );
    // Namespace uses first, so that if the two halves ever reached for one edge
    // id the upgrade - which carries a real declaration rather than another
    // placeholder - would be the one that lands.
    for (const entry of namespaceWork) await askNamespaceUses(entry, index, checker, out);
    for (const question of upgradeWork) await askUpgrade(question, index, checker, out);
  }

  return out.finish(scope, filesScanned);
}

// --- what one file contributes -------------------------------------------

/** One file in the pass's scope, and everything the scan learned about it. */
interface ScopedFile {
  filePath: string;
  language: string;
  /** The structural extraction's nodes by id, so a placeholder the
   * name-matching layer already built is reused rather than rebuilt. */
  structural: Map<string, ExtractedNode>;
  /** Sites the structural pass emitted nothing for - question 1. */
  uses: NamespaceMemberUse[];
  /** Edges it emitted but could not resolve - question 2. */
  upgrades: UpgradeQuestion[];
}

/** One placeholder's worth of upgrade work: the position to ask tsserver
 * about, and every edge that lands on it. All of them get the same answer, so
 * the round trip is per placeholder rather than per usage. */
interface UpgradeQuestion {
  /** Project-relative path of the file the import is written in. */
  importer: string;
  placeholder: ExtractedNode;
  edges: ExtractedEdge[];
  /** `<name> from <file>`, for diagnostics. */
  label: string;
}

/**
 * The unresolved edges in one file that are this pass's to re-ask: usage edges
 * on a pending-symbol placeholder whose target file does not itself declare the
 * name (see the module comment on which questions are worth a round trip).
 */
async function upgradeQuestionsIn(
  filePath: string,
  extraction: ExtractResult,
  index: ProjectIndex,
): Promise<UpgradeQuestion[]> {
  const byId = new Map(extraction.nodes.map((node) => [node.id, node]));
  const byPlaceholder = new Map<string, UpgradeQuestion>();

  for (const edge of extraction.edges) {
    if (edge.resolved) continue; // the structural pass already answered this one
    if (!LINKABLE_EDGE_KINDS.has(edge.kind)) continue;
    const placeholder = byId.get(edge.toId);
    if (placeholder === undefined || placeholder.nativeKind !== PENDING_SYMBOL_NATIVE_KIND) continue;

    const existing = byPlaceholder.get(placeholder.id);
    if (existing !== undefined) {
      existing.edges.push(edge);
      continue;
    }
    const address = splitAddress(placeholder.qualifiedName);
    if (address === undefined) continue;
    byPlaceholder.set(placeholder.id, {
      importer: filePath,
      placeholder,
      edges: [edge],
      label: `${address.name} from ${address.file}`,
    });
  }

  const questions: UpgradeQuestion[] = [];
  for (const question of byPlaceholder.values()) {
    const address = splitAddress(question.placeholder.qualifiedName);
    if (address === undefined) continue;
    // The target declares it: core links this without a checker.
    if (await index.declaresExport(address.file, address.name)) continue;
    questions.push(question);
  }
  return questions;
}

// --- asking, and what an answer becomes ----------------------------------

/**
 * Question 1, for one file: every distinct `ns.member` in it becomes a
 * placeholder addressed at the declaration the checker names, plus the usage
 * edge hanging on it.
 *
 * One question per distinct `ns.member`: every site spelling it the same way
 * binds the same declaration, and the checking is what the pass actually spends
 * its time on.
 */
async function askNamespaceUses(
  entry: ScopedFile,
  index: ProjectIndex,
  checker: Checker,
  out: PassOutput,
): Promise<void> {
  const absPath = index.absolute(entry.filePath);
  const answers = new Map<string, DeclarationAddress | null>();

  for (const use of entry.uses) {
    const key = `${use.namespaceName}.${use.memberName}`;
    if (!answers.has(key)) {
      answers.set(key, await resolveMemberDeclaration(checker, index, absPath, use));
    }
    const answer = answers.get(key) ?? null;
    if (answer === null) {
      out.unresolved += 1;
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

    out.placeholderEdge(entry.filePath, placeholder, {
      id: edgeIdFor(use.fromId, use.edgeKind, placeholder.id),
      fromId: use.fromId,
      toId: placeholder.id,
      kind: use.edgeKind,
      source: SEMANTIC_EDGE_SOURCE,
      // Pointing at a placeholder *is* what unresolved means here, whichever
      // pass built it: only core can confirm the target file exports the
      // name, and it says so by repointing this edge (`graph::symbol_links`).
      resolved: false,
    });
  }
}

/**
 * Question 2, for one placeholder: the edges hanging on it, re-sent pointing at
 * the declaration the checker bound the imported name to.
 */
async function askUpgrade(
  question: UpgradeQuestion,
  index: ProjectIndex,
  checker: Checker,
  out: PassOutput,
): Promise<void> {
  // The placeholder is anchored at the import specifier's local name
  // (`Extractor.bindImport`), which is precisely the token whose declaration is
  // being asked for.
  const definitions = await checker.definitionAt(
    index.absolute(question.importer),
    question.placeholder.startLine,
    question.placeholder.startCol,
    question.label,
  );
  // Exactly one, or nothing: several declarations is a merged symbol whose
  // pieces this graph does not distinguish between yet, and choosing one of
  // them would be the guess the whole pipeline refuses to make.
  if (definitions === null || definitions.length !== 1) {
    out.unresolved += 1;
    return;
  }
  const target = await index.declarationAt(definitions[0]);
  if (target === undefined) {
    out.unresolved += 1;
    return;
  }

  for (const edge of question.edges) {
    const required = LINKABLE_EDGE_KINDS.get(edge.kind);
    if (required !== undefined && required !== null && target.kind !== required) continue;
    if (target.id === edge.fromId) continue; // a self-edge carries no information
    out.upgradedEdge({ ...edge, toId: target.id, source: SEMANTIC_EDGE_SOURCE, resolved: true }, target);
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
 * already handle that case, which is why this reads the declared *name* out of
 * the source rather than insisting on a declaration node the way [`askUpgrade`]
 * has to.
 *
 * Refused, on the standing rule that a missing edge beats a wrong one: no
 * definition, or several that disagree about which declaration they are (a
 * `class`/`interface` merge - one symbol to TypeScript, two nodes here); or a
 * declaration in a file this index does not hold.
 */
async function resolveMemberDeclaration(
  checker: Checker,
  index: ProjectIndex,
  absPath: string,
  use: NamespaceMemberUse,
): Promise<DeclarationAddress | null> {
  const locations = await checker.definitionAt(
    absPath,
    use.line,
    use.col,
    `${use.namespaceName}.${use.memberName}`,
  );
  if (locations === null) return null;

  const addresses = new Map<string, DeclarationAddress>();
  for (const location of locations) {
    const address = await index.addressOf(location);
    if (address !== null) addresses.set(`${address.declPath}#${address.declName}`, address);
  }
  if (addresses.size !== 1) return null;
  return [...addresses.values()][0];
}

// --- the shared plumbing --------------------------------------------------

/**
 * The tsserver child, plus the one thing both halves need on top of it: a
 * budget. "The checker was never reached" and "the checker answered nothing"
 * are different outcomes, and only the first is counted against it.
 */
class Checker {
  private consecutiveFailures = 0;

  constructor(
    private readonly project: SemanticProject,
    private readonly options: SemanticPassOptions,
  ) {}

  /**
   * The declarations at a zero-based (line, col) - tree-sitter's own
   * convention - or null when the checker could not be reached at all, which an
   * empty answer is not.
   */
  async definitionAt(
    file: string,
    line: number,
    col: number,
    what: string,
  ): Promise<DefinitionLocation[] | null> {
    if (this.consecutiveFailures >= MAX_CONSECUTIVE_FAILURES) return null;
    try {
      // tree-sitter counts rows/columns from zero, tsserver lines/offsets from
      // one; both count UTF-16 code units, so this is the whole conversion.
      const locations = await this.project.definition(file, { line: line + 1, offset: col + 1 });
      this.consecutiveFailures = 0;
      return locations;
    } catch (err) {
      this.options.onLog?.(`semantic pass could not resolve ${what}: ${(err as Error).message}`);
      this.consecutiveFailures += 1;
      return null;
    }
  }
}

/** A `<file, name>` pair a pending-symbol placeholder is addressed by. */
interface DeclarationAddress {
  declPath: string;
  declName: string;
}

/**
 * What this index holds, as far as one pass needs to know it: an extraction per
 * file, and the two ways a `tsserver` answer is turned into something the graph
 * can point at.
 *
 * One instance per pass and not a moment longer: it memoizes extractions, file
 * contents and `.gitignore` layers, so it must not outlive the assumption that
 * the tree is not changing underneath it. Both halves share one because the
 * importer's placeholders on the way in and the target's declarations on the
 * way out are the same files, and a barrel is by definition asked about
 * repeatedly.
 */
class ProjectIndex {
  private readonly resolveSpecifier: SpecifierResolver;
  private readonly isIndexable: (projectRelativePath: string) => boolean;
  /**
   * The roots an absolute path may be spelled against. Two of them because the
   * canonical and the given spelling are routinely different names for one
   * directory (`/tmp` is a symlink to `/private/tmp` on macOS) and tsserver
   * answers in whichever one it was asked in; comparing only one way would read
   * a perfectly ordinary in-project declaration as "outside the project" and
   * drop the answer.
   */
  private readonly roots: ReadonlySet<string>;
  private readonly extractions = new Map<string, ExtractResult | null>();
  private readonly lines = new Map<string, string[] | null>();

  constructor(
    private readonly projectRoot: string,
    private readonly options: SemanticPassOptions,
  ) {
    this.resolveSpecifier = createProjectResolver(projectRoot);
    this.isIndexable = createIndexabilityChecker(projectRoot);
    this.roots = new Set([path.resolve(projectRoot), canonicalizeProjectRoot(projectRoot)]);
  }

  absolute(relPath: string): string {
    return path.join(this.projectRoot, relPath);
  }

  /**
   * The file's extraction, reusing incremental.ts's cache when it has one -
   * that copy is by construction the one core was last told about, so the ids
   * an edge is written from are the ids actually in the index, and re-deriving
   * them would both cost a second parse and risk answering about a file that
   * changed again in between.
   *
   * Returns null for anything this pass has nothing to say about: a file it
   * does not parse, one that vanished, one whose parse threw.
   */
  async extractionOf(relPath: string): Promise<ExtractResult | null> {
    const memoized = this.extractions.get(relPath);
    if (memoized !== undefined) return memoized;

    const result = await this.extract(relPath);
    this.extractions.set(relPath, result);
    return result;
  }

  private async extract(relPath: string): Promise<ExtractResult | null> {
    const cached = cachedExtraction(relPath);
    if (cached !== undefined) return cached;
    if (!isSupportedFile(relPath)) return null;

    let sourceText: string;
    try {
      sourceText = await fs.readFile(this.absolute(relPath), "utf8");
    } catch {
      return null; // deleted, unreadable - the next reparse is the one that matters
    }
    try {
      return extractFile(relPath, sourceText, { resolveSpecifier: this.resolveSpecifier });
    } catch (err) {
      this.options.onLog?.(`semantic pass could not parse ${relPath}: ${(err as Error).message}`);
      return null;
    }
  }

  /** Whether `file` itself declares an exported `name` - i.e. whether core's
   * own walk already has everything it needs and this pass should stay out of
   * the way. */
  async declaresExport(file: string, name: string): Promise<boolean> {
    const result = await this.extractionOf(file);
    if (result === null) return false;
    return result.nodes.some((node) => node.exported && node.name === name && !isPlaceholder(node));
  }

  /**
   * An absolute path as this index spells it - project-relative POSIX - or
   * undefined when it is not a path this index holds anything for: outside the
   * project, gitignored, hard-excluded (`node_modules`), or an extension this
   * plugin does not parse. A declaration in any of those is a real answer to
   * the language's question and not one this graph can point at.
   */
  indexedPathOf(absolute: string): string | undefined {
    for (const root of this.roots) {
      const relative = toPosixPath(path.relative(root, absolute));
      if (relative === "" || relative.startsWith("../") || path.isAbsolute(relative)) continue;
      if (!isSupportedFile(relative) || !this.isIndexable(relative)) return undefined;
      return relative;
    }
    return undefined;
  }

  /**
   * The `<file, name>` address a `definition` answer names, for a placeholder
   * to be built from.
   *
   * The name has to be read out of the source because `definition` reports
   * where it is, not what it says, and the two differ exactly when it matters:
   * an aliased re-export lands on the original name, not the one written at the
   * use site.
   */
  async addressOf(location: DefinitionLocation): Promise<DeclarationAddress | null> {
    // A name never spans lines, so anything that claims to is not one.
    if (location.start.line !== location.end.line) return null;

    const declPath = this.indexedPathOf(location.file);
    if (declPath === undefined) return null;

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

  /**
   * The declaration *node* a `definition` answer names, or undefined if this
   * index does not hold one.
   *
   * The node is re-sent with the edge rather than assumed to be in the index
   * already. It costs nothing when it is - the id is derived from the same
   * (path, kind, qualifiedName) the walk derived it from, so the upsert is a
   * no-op - and it is what keeps the diff self-sufficient when it is not,
   * instead of failing core's foreign key and taking the whole pass down with
   * it.
   */
  async declarationAt(definition: DefinitionLocation): Promise<ExtractedNode | undefined> {
    const relative = this.indexedPathOf(definition.file);
    if (relative === undefined) return undefined;

    const result = await this.extractionOf(relative);
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

/**
 * What the two halves write into, and the one place the per-file bookkeeping
 * lives.
 *
 * Only placeholder edges are booked: they are the ones nothing else can
 * retract. An upgraded edge belongs to the structural pass, which deletes it
 * along with the line it came from, and booking it here would let a later pass
 * that simply had nothing to upgrade retract a perfectly live edge.
 */
class PassOutput {
  private readonly nodes = new Map<string, ExtractedNode>();
  private readonly edges = new Map<string, ExtractedEdge>();
  private readonly emitted = new Map<string, Set<string>>();
  unresolved = 0;

  placeholderEdge(filePath: string, placeholder: ExtractedNode, edge: ExtractedEdge): void {
    this.nodes.set(placeholder.id, placeholder);
    this.edges.set(edge.id, edge);
    let ids = this.emitted.get(filePath);
    if (ids === undefined) {
      ids = new Set<string>();
      this.emitted.set(filePath, ids);
    }
    ids.add(edge.id);
  }

  upgradedEdge(edge: ExtractedEdge, target: ExtractedNode): void {
    this.nodes.set(target.id, target);
    this.edges.set(edge.id, edge);
  }

  /**
   * The diff, once every question has been asked: what to write, and what a
   * previous pass wrote for these same files that no longer has a call site
   * behind it.
   */
  finish(scope: readonly string[], filesScanned: number): SemanticPassResult {
    const deleteEdgeIds: string[] = [];
    for (const filePath of scope) {
      const ids = this.emitted.get(filePath);
      for (const previous of emittedEdgesByFile.get(filePath) ?? NO_EDGE_IDS) {
        if (ids?.has(previous) !== true && !this.edges.has(previous)) deleteEdgeIds.push(previous);
      }
      if (ids === undefined) emittedEdgesByFile.delete(filePath);
      else emittedEdgesByFile.set(filePath, ids);
    }

    return {
      upsertNodes: [...this.nodes.values()],
      upsertEdges: [...this.edges.values()],
      deleteEdgeIds,
      filesScanned,
      unresolvedUses: this.unresolved,
    };
  }
}

// --- small pure helpers ---------------------------------------------------

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

function languageOf(extraction: ExtractResult): string {
  return extraction.nodes.find((node) => node.kind === "File")?.language ?? "typescript";
}

/** Every file the walk would index, as project-relative POSIX paths. The
 * whole-project scope - an empty `filePaths` means "everything indexed so
 * far", which after a cold start is the whole tree. */
async function allProjectFiles(projectRoot: string): Promise<string[]> {
  const files: string[] = [];
  for await (const relPath of walkProjectFiles(projectRoot)) files.push(relPath);
  return files;
}
