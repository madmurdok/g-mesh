// Incremental reparse of a single changed file.
//
// Core's "file changed" control message carries a file path and nothing else -
// no edit range, no previous contents - so this module keeps its own per-file
// state (last source text, last parse tree, last extraction) and derives the
// edit itself by diffing the two full source strings. The previous tree is
// then fed back into tree-sitter, which re-lexes only the affected ranges
// instead of reparsing the file from scratch.
//
// What leaves this module is a diff, not a snapshot: only the nodes/edges that
// actually differ from the last extraction of that same file. Deliberately
// standalone - nothing here wires into index.ts's `fileChanged` handler, per
// this ticket's scope (same as bulkIndex.ts).

import * as fs from "node:fs/promises";
import * as path from "node:path";

import {
  extractIncremental,
  type ExtractedEdge,
  type ExtractedNode,
  type ExtractOptions,
  type ExtractResult,
  type ParsedTree,
  type SymbolDeclaration,
} from "./extract";
import { createProjectResolver } from "./resolve";

// --- edit description ----------------------------------------------------

/** tree-sitter's `Point`. Rows and columns are zero-based; columns count
 * UTF-16 code units, which is what node-tree-sitter's binding expects (it
 * feeds the native side a UTF-16 buffer and scales by 2 itself). */
export interface SourcePoint {
  row: number;
  column: number;
}

/**
 * Structurally tree-sitter's `Parser.Edit`: the single contiguous span that
 * changed, given as offsets into the old text (`startIndex`, `oldEndIndex`)
 * and the new text (`startIndex`, `newEndIndex`), plus the same three
 * boundaries as row/column positions.
 */
export interface SourceEdit {
  startIndex: number;
  oldEndIndex: number;
  newEndIndex: number;
  startPosition: SourcePoint;
  oldEndPosition: SourcePoint;
  newEndPosition: SourcePoint;
}

function isHighSurrogate(code: number): boolean {
  return code >= 0xd800 && code <= 0xdbff;
}

function isLowSurrogate(code: number): boolean {
  return code >= 0xdc00 && code <= 0xdfff;
}

/**
 * Offset -> `{row, column}` against `text`. Rows advance on `\n` only, which
 * is how tree-sitter counts them (a `\r` in a CRLF file is ordinary line
 * content and occupies a column, in both this function and the parser).
 *
 * Which string this is called on matters: the start and old-end boundaries
 * live in the old text, the new-end boundary in the new one.
 */
function positionAt(text: string, index: number): SourcePoint {
  let row = 0;
  let lineStart = 0;
  for (let i = 0; i < index; i += 1) {
    if (text.charCodeAt(i) === 0x0a) {
      row += 1;
      lineStart = i + 1;
    }
  }
  return { row, column: index - lineStart };
}

/**
 * Derives the minimal single-span edit turning `oldText` into `newText`, by
 * walking in from both ends until the strings disagree: everything before
 * `startIndex` and everything after the two end indices is common, so the
 * changed span is `[startIndex, oldEndIndex)` in the old text and
 * `[startIndex, newEndIndex)` in the new one.
 *
 * The suffix scan is capped so prefix and suffix cannot overlap in either
 * string (they would for, say, `"aa"` -> `"aaa"`, where the same characters
 * match from both ends); the cap keeps both spans non-negative and makes the
 * inserted/deleted text land at one well-defined offset.
 *
 * Surrogate pairs: both boundaries are pulled back off the middle of a pair,
 * so an edit next to an astral character (an emoji in a string or comment)
 * describes whole characters rather than half of one.
 *
 * Returns `null` when the two texts are identical - there is nothing to
 * reparse.
 *
 * This is a coarse "one changed region" description, not a real diff: two
 * distant edits in one save collapse into one span covering both. That costs
 * some subtree reuse inside the span and nothing in correctness, since the
 * span is always a true superset of what changed.
 */
export function computeSourceEdit(oldText: string, newText: string): SourceEdit | null {
  if (oldText === newText) return null;

  const shorter = Math.min(oldText.length, newText.length);

  let startIndex = 0;
  while (startIndex < shorter && oldText.charCodeAt(startIndex) === newText.charCodeAt(startIndex)) {
    startIndex += 1;
  }
  // Never cut between a high and a low surrogate.
  if (startIndex > 0 && isHighSurrogate(oldText.charCodeAt(startIndex - 1))) startIndex -= 1;

  let suffix = 0;
  const maxSuffix = shorter - startIndex;
  while (
    suffix < maxSuffix &&
    oldText.charCodeAt(oldText.length - 1 - suffix) === newText.charCodeAt(newText.length - 1 - suffix)
  ) {
    suffix += 1;
  }
  if (suffix > 0 && isLowSurrogate(oldText.charCodeAt(oldText.length - suffix))) suffix -= 1;

  const oldEndIndex = oldText.length - suffix;
  const newEndIndex = newText.length - suffix;

  return {
    startIndex,
    oldEndIndex,
    newEndIndex,
    // The prefix is common to both strings, so the start position is the same
    // whichever one it is measured against; the two end positions are not.
    startPosition: positionAt(oldText, startIndex),
    oldEndPosition: positionAt(oldText, oldEndIndex),
    newEndPosition: positionAt(newText, newEndIndex),
  };
}

// --- diff ----------------------------------------------------------------

/**
 * What changed about one file since its last extraction. The vocabulary is
 * only added/removed, so a symbol that survived an edit with different
 * content (a function whose body moved its end line, say) shows up as both a
 * removal of the old version and an addition of the new one - see
 * `diffItems`.
 */
export interface FileDiff {
  filePath: string;
  addedNodes: ExtractedNode[];
  removedNodes: ExtractedNode[];
  addedEdges: ExtractedEdge[];
  removedEdges: ExtractedEdge[];
  /** From the new extraction, not a diff of the flag. */
  hasSyntaxErrors: boolean;
  /**
   * True when there was no cached state for this file and the whole
   * extraction is reported as added (see `reparseFile`).
   */
  fullExtraction: boolean;
}

export function isEmptyDiff(diff: FileDiff): boolean {
  return (
    diff.addedNodes.length === 0 &&
    diff.removedNodes.length === 0 &&
    diff.addedEdges.length === 0 &&
    diff.removedEdges.length === 0
  );
}

/**
 * Full structural equality. Comparing ids alone would be wrong: ids are
 * derived from identity (path/kind/qualifiedName), never from position, so a
 * function whose body changed keeps its id while its range, signature or doc
 * comment move - an id-only diff would silently report "nothing changed"
 * about it.
 */
function nodesEqual(a: ExtractedNode, b: ExtractedNode): boolean {
  return (
    a.id === b.id &&
    a.kind === b.kind &&
    a.name === b.name &&
    a.qualifiedName === b.qualifiedName &&
    a.filePath === b.filePath &&
    a.startLine === b.startLine &&
    a.startCol === b.startCol &&
    a.endLine === b.endLine &&
    a.endCol === b.endCol &&
    a.signature === b.signature &&
    a.exported === b.exported &&
    a.docComment === b.docComment &&
    a.language === b.language &&
    a.nativeKind === b.nativeKind &&
    a.hasSyntaxErrors === b.hasSyntaxErrors &&
    declarationsEqual(a.declarations, b.declarations)
  );
}

/**
 * Declaration lists compared by order and contents, which is the only
 * comparison that is right in both directions. Reference equality would say
 * "changed" every reparse, since each extraction builds fresh objects - and a
 * node reported as changed is removed and re-added, which costs a live symbol
 * its embedding and its inbound edges for nothing. Ignoring the lists instead
 * would say "unchanged" when an overload signature was edited, silently
 * leaving the stored list describing code that no longer exists.
 *
 * Order matters rather than being incidental: `ordinal` is a declaration's
 * identity, the thing a call site's binding is recorded against, so two lists
 * holding the same declarations in a different order are genuinely different.
 * (`ordinal` is compared as a field of its own too - a list rebuilt with the
 * same ranges but renumbered is not the same list.)
 */
function declarationsEqual(
  a: SymbolDeclaration[] | undefined,
  b: SymbolDeclaration[] | undefined,
): boolean {
  if (a === undefined || b === undefined) return a === b;
  if (a.length !== b.length) return false;
  return a.every((declaration, index) => {
    const other = b[index];
    return (
      declaration.ordinal === other.ordinal &&
      declaration.startLine === other.startLine &&
      declaration.startCol === other.startCol &&
      declaration.endLine === other.endLine &&
      declaration.endCol === other.endCol &&
      declaration.signature === other.signature &&
      declaration.hasBody === other.hasBody
    );
  });
}

function edgesEqual(a: ExtractedEdge, b: ExtractedEdge): boolean {
  return (
    a.id === b.id &&
    a.fromId === b.fromId &&
    a.toId === b.toId &&
    a.kind === b.kind &&
    a.source === b.source &&
    a.resolved === b.resolved &&
    // Part of the edge id already, so two edges that disagree here disagree
    // about `id` too and never reach this comparison as a pair - checked all
    // the same, so that the day something derives an id differently this
    // stays a content comparison rather than a restatement of the id.
    a.toDeclaration === b.toDeclaration
  );
}

/**
 * Set difference by id, refined by content:
 *  - id only in `next` -> added
 *  - id only in `previous` -> removed
 *  - id in both, content differs -> removed (old) *and* added (new)
 *  - id in both, content equal -> omitted entirely
 *
 * Order follows the input arrays, so the output is as deterministic as the
 * extraction is.
 */
function diffItems<T extends { id: string }>(
  previous: readonly T[],
  next: readonly T[],
  equal: (a: T, b: T) => boolean,
): { added: T[]; removed: T[] } {
  const previousById = new Map(previous.map((item) => [item.id, item]));
  const nextById = new Map(next.map((item) => [item.id, item]));

  const removed = previous.filter((item) => {
    const current = nextById.get(item.id);
    return current === undefined || !equal(item, current);
  });
  const added = next.filter((item) => {
    const before = previousById.get(item.id);
    return before === undefined || !equal(before, item);
  });

  return { added, removed };
}

const EMPTY_RESULT: ExtractResult = {
  nodes: [],
  edges: [],
  hasSyntaxErrors: false,
  namespaceMemberUses: [],
  overloadCallSites: [],
};

function diffResults(
  filePath: string,
  previous: ExtractResult,
  next: ExtractResult,
  fullExtraction: boolean,
): FileDiff {
  const nodes = diffItems(previous.nodes, next.nodes, nodesEqual);
  const edges = diffItems(previous.edges, next.edges, edgesEqual);
  return {
    filePath,
    addedNodes: nodes.added,
    removedNodes: nodes.removed,
    addedEdges: edges.added,
    removedEdges: edges.removed,
    hasSyntaxErrors: next.hasSyntaxErrors,
    fullExtraction,
  };
}

// --- per-file state ------------------------------------------------------

interface FileState {
  /** The text `tree` was parsed from - the left-hand side of the next edit. */
  sourceText: string;
  tree: ParsedTree;
  /** The extraction the core was last told about, i.e. the diff baseline. */
  result: ExtractResult;
}

/**
 * Process-lifetime only. A restart loses it, and that is fine: the plugin's
 * cold path is a full bulk index (bulkIndex.ts), so there is nothing to
 * resume. Keyed by the same path string the caller puts in node ids.
 */
const fileStates = new Map<string, FileState>();

export function hasCachedFile(filePath: string): boolean {
  return fileStates.has(filePath);
}

/**
 * The extraction core was last told about for `filePath`, if this process
 * still holds it.
 *
 * Read-only, and deliberately not a parse: semanticPass.ts runs immediately
 * after a reparse of the same file and needs the *committed* extraction rather
 * than a fresh one - the ids it writes edges from have to be ids that are
 * actually in the index, and re-deriving them would both cost a second parse
 * and risk answering about a file that changed again in between. A miss (a cold
 * process, a file only the bulk-index run ever saw) is normal and the caller
 * re-parses instead.
 */
export function cachedExtraction(filePath: string): ExtractResult | undefined {
  return fileStates.get(filePath)?.result;
}

/** Drops one file's state - e.g. after a delete, so a re-creation is treated
 * as a first sighting rather than diffed against a stale tree. */
export function forgetFile(filePath: string): boolean {
  return fileStates.delete(filePath);
}

/** Drops every file's state. Mainly a test seam; also the right response to a
 * full reindex. */
export function resetIncrementalState(): void {
  fileStates.clear();
}

// --- entry points --------------------------------------------------------

/**
 * Reparses one file against whatever this module last saw of it and returns
 * only what changed.
 *
 * On a cache miss the file is parsed in full and the entire extraction comes
 * back as additions (`fullExtraction: true`) - the diff against "nothing" is
 * "everything", which is both the honest answer and the one that leaves the
 * core's graph correct without the caller having to seed anything first. So
 * this function is safe to call as the very first thing that ever touches a
 * file; it does not require a prior bulk index.
 *
 * On a hit the edit is derived from the cached text (`computeSourceEdit`),
 * applied to the cached tree, and the reparse reuses that tree. Identical
 * text short-circuits to an empty diff without touching tree-sitter at all -
 * editors save unchanged buffers often enough for that to be worth it.
 *
 * The cache is updated only after a successful extraction, so a throwing
 * parse leaves the previous state intact and the next notification still has
 * a valid baseline.
 *
 * @throws {UnsupportedFileError} for extensions this plugin does not own.
 */
export function reparseFile(
  filePath: string,
  sourceText: string,
  options?: ExtractOptions,
): FileDiff {
  const cached = fileStates.get(filePath);

  if (cached === undefined) {
    const { tree, result } = extractIncremental(filePath, sourceText, undefined, options);
    fileStates.set(filePath, { sourceText, tree, result });
    return diffResults(filePath, EMPTY_RESULT, result, true);
  }

  const edit = computeSourceEdit(cached.sourceText, sourceText);
  if (edit === null) {
    return diffResults(filePath, cached.result, cached.result, false);
  }

  // Mutates the cached tree in place: from here on it describes the *new*
  // text's layout with the edited ranges marked stale, which is exactly what
  // `parse` needs as its `oldTree`.
  cached.tree.edit(edit);
  const { tree, result } = extractIncremental(filePath, sourceText, cached.tree, options);

  fileStates.set(filePath, { sourceText, tree, result });
  return diffResults(filePath, cached.result, result, false);
}

/**
 * `reparseFile` for the shape core's notification actually has: a
 * project-relative path, with the new contents read from disk.
 * `projectRoot` only locates the file; ids keep using `filePath` verbatim,
 * the same convention bulkIndex.ts follows.
 *
 * A fresh resolver per call, unlike the bulk index's one-per-walk: this
 * process is long-lived, so a memo shared across calls would keep answering
 * "that file does not exist" about a file created since the last edit.
 *
 * Resolution is still only ever as fresh as the *importer's* last reparse:
 * creating `b.ts` does not by itself re-resolve `a.ts`'s dangling
 * `import "./b"`, it stays a placeholder until `a.ts` is reindexed. Core
 * closes exactly that gap from the other side - a diff that adds a `File`
 * node relinks the placeholders already pointing at its path (see
 * `graph::imports::link_diff`) - so the two together leave only "the importer
 * has not been touched and the target was never added" unresolved, which the
 * next reparse of the importer fixes.
 */
export async function reparseChangedFile(
  projectRoot: string,
  filePath: string,
): Promise<FileDiff> {
  const sourceText = await fs.readFile(path.join(projectRoot, filePath), "utf8");
  return reparseFile(filePath, sourceText, { resolveSpecifier: createProjectResolver(projectRoot) });
}
