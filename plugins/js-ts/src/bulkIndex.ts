// Initial bulk index: walks a project's JS/TS/JSX/TSX files, runs the
// existing tree-sitter extraction (extract.ts) on each one, and streams the
// resulting nodes/edges out as NDJSON (one compact JSON object per line)
// matching core's wire contract (core/src/protocol/types.rs - `WireNode` /
// `WireEdge`). Driven by index.ts's one-shot `--bulk-index` mode, whose whole
// stdout is this stream - never by the long-lived control plane, which speaks
// framed request/response instead.

import * as fs from "node:fs/promises";
import type { Dirent } from "node:fs";
import * as path from "node:path";
import ignore, { type Ignore } from "ignore";

import { extractFile, isSupportedFile, type ExtractedEdge, type ExtractedNode } from "./extract";
import { createProjectResolver } from "./resolve";

/**
 * Directories skipped unconditionally, regardless of .gitignore contents.
 * `.claude` holds Claude Code's own session/worktree artifacts (e.g.
 * `.claude/worktrees/agent-<hash>/`, full copies of the project made for
 * parallel agent sessions) - real project source is never there, and a
 * project's own .gitignore has no reason to list it, so it needs the same
 * hard exclusion as `.git`.
 */
const HARD_EXCLUDED_DIRS = new Set([".git", "node_modules", "dist", ".claude"]);

/**
 * Where bulk-indexed NDJSON lines go. A callback mirrors the simplest test
 * seam; a `WritableStream` mirrors `writeMessage(stream, ...)` in
 * jsonrpc.ts so a future caller can pass `process.stdout` directly.
 */
export type NdjsonSink = NodeJS.WritableStream | ((line: string) => void);

/** Returns false when a stream sink is already over its high-water mark,
 * i.e. the consumer is behind and the caller should let it catch up. */
function emitLine(sink: NdjsonSink, value: unknown): boolean {
  const line = JSON.stringify(value);
  if (typeof sink === "function") {
    sink(line);
    return true;
  }
  return sink.write(line + "\n");
}

/**
 * Parks until a stream sink has flushed enough to accept more, so a project
 * bigger than the pipe buffer queues in the *consumer's* pace rather than in
 * this process's memory. Core reads this stream while committing each batch
 * to SQLite (`daemon::bulk_index`), so it is routinely slower than the walk;
 * without this, `bulkIndexProject`'s bounded-memory promise would hold for
 * extraction but not for output.
 */
function waitForDrain(sink: NdjsonSink): Promise<void> {
  if (typeof sink === "function") return Promise.resolve();
  return new Promise((resolve) => {
    const settle = (): void => {
      sink.removeListener("drain", settle);
      sink.removeListener("close", settle);
      sink.removeListener("error", settle);
      resolve();
    };
    sink.once("drain", settle);
    // A consumer that died never drains: resuming on close/error lets the
    // next write fail loudly instead of parking here forever.
    sink.once("close", settle);
    sink.once("error", settle);
  });
}

// --- wire shape (core/src/protocol/types.rs) -----------------------------

export interface WirePosition {
  line: number;
  col: number;
}

export interface WireRange {
  start: WirePosition;
  end: WirePosition;
}

/** Mirrors core's `WireNode`. The one real transformation off `ExtractedNode`:
 * flat startLine/startCol/endLine/endCol become a nested `range`. */
export interface WireNode {
  id: string;
  kind: ExtractedNode["kind"];
  name: string;
  qualifiedName: string;
  filePath: string;
  range: WireRange;
  signature: string | null;
  exported: boolean;
  docComment: string | null;
  language: string;
  nativeKind: string | null;
  hasSyntaxErrors: boolean;
}

/** Identical shape to `ExtractedEdge` - passed through unchanged. */
export type WireEdge = ExtractedEdge;

export function toWireNode(node: ExtractedNode): WireNode {
  return {
    id: node.id,
    kind: node.kind,
    name: node.name,
    qualifiedName: node.qualifiedName,
    filePath: node.filePath,
    range: {
      start: { line: node.startLine, col: node.startCol },
      end: { line: node.endLine, col: node.endCol },
    },
    signature: node.signature ?? null,
    exported: node.exported,
    docComment: node.docComment ?? null,
    language: node.language,
    nativeKind: node.nativeKind ?? null,
    hasSyntaxErrors: node.hasSyntaxErrors,
  };
}

// --- gitignore-aware walk -------------------------------------------------

/** One directory's own `.gitignore`, plus the (absolute) directory it applies to. */
interface GitignoreLayer {
  readonly baseDir: string;
  readonly matcher: Ignore;
}

function toPosixPath(p: string): string {
  return p.split(path.sep).join("/");
}

async function loadGitignoreLayer(dir: string): Promise<GitignoreLayer | null> {
  let contents: string;
  try {
    contents = await fs.readFile(path.join(dir, ".gitignore"), "utf8");
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
function isIgnoredByLayers(layers: readonly GitignoreLayer[], absPath: string): boolean {
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
 * Walks `projectRoot` depth-first, yielding project-relative POSIX paths of
 * every JS/TS/JSX/TSX file that is not gitignored. `.git`, `node_modules`,
 * `dist`, and `.claude` are never descended into, regardless of `.gitignore`
 * contents. Entries within a directory are visited in sorted order for
 * deterministic output.
 */
export async function* walkProjectFiles(projectRoot: string): AsyncGenerator<string> {
  yield* walkDir(projectRoot, projectRoot, []);
}

async function* walkDir(
  projectRoot: string,
  dir: string,
  layers: readonly GitignoreLayer[],
): AsyncGenerator<string> {
  const ownLayer = await loadGitignoreLayer(dir);
  const nextLayers = ownLayer ? [...layers, ownLayer] : layers;

  let entries: Dirent[];
  try {
    entries = await fs.readdir(dir, { withFileTypes: true });
  } catch {
    return; // directory vanished / unreadable - nothing to yield
  }
  entries.sort((a, b) => a.name.localeCompare(b.name));

  for (const entry of entries) {
    const abs = path.join(dir, entry.name);

    if (entry.isDirectory()) {
      if (HARD_EXCLUDED_DIRS.has(entry.name)) continue;
      if (isIgnoredByLayers(nextLayers, abs)) continue;
      yield* walkDir(projectRoot, abs, nextLayers);
      continue;
    }

    if (!entry.isFile()) continue; // symlinks, sockets, etc. - not source files
    // Anything with an unsupported extension - including tsconfig.json - is
    // never opened here, which is what keeps a malicious tsconfig
    // `compilerOptions.plugins` entry inert (see security.test.ts).
    if (!isSupportedFile(abs)) continue;
    if (isIgnoredByLayers(nextLayers, abs)) continue;
    yield toPosixPath(path.relative(projectRoot, abs));
  }
}

// --- bulk index ------------------------------------------------------------

export interface BulkIndexSummary {
  filesProcessed: number;
  nodesEmitted: number;
  edgesEmitted: number;
}

/**
 * Walks `projectRoot`'s supported source files (gitignore-aware, see
 * `walkProjectFiles`), extracts each one with the existing tree-sitter pass,
 * and writes its nodes then its edges to `sink` as compact NDJSON - one line
 * per node/edge, emitted as soon as that file's extraction completes. Files
 * are processed one at a time; nothing from a later file is read before an
 * earlier file's output has been written, so memory use stays bounded by a
 * single file's extraction result, never the whole project's - and a stream
 * sink that falls behind pauses the walk at the next file boundary
 * (`waitForDrain`) rather than letting the backlog accumulate here.
 *
 * A file with syntax errors still contributes whatever nodes/edges
 * tree-sitter's error-tolerant parse produced (see `ExtractedNode.hasSyntaxErrors`) -
 * only files `isSupportedFile` rejects, or gitignored files, are skipped.
 */
export async function bulkIndexProject(
  projectRoot: string,
  sink: NdjsonSink,
): Promise<BulkIndexSummary> {
  let filesProcessed = 0;
  let nodesEmitted = 0;
  let edgesEmitted = 0;

  // One resolver (and therefore one existence memo) for the whole walk: this
  // process is one-shot, so nothing can create or delete a file behind its
  // back while it runs, and the same handful of shared modules is otherwise
  // re-stat-ed once per importing file.
  const resolveSpecifier = createProjectResolver(projectRoot);

  for await (const relPath of walkProjectFiles(projectRoot)) {
    const absPath = path.join(projectRoot, relPath);
    let sourceText: string;
    try {
      sourceText = await fs.readFile(absPath, "utf8");
    } catch {
      continue; // vanished between walk and read - not this ticket's concern
    }

    let result;
    try {
      result = extractFile(relPath, sourceText, { resolveSpecifier });
    } catch {
      continue; // e.g. UnsupportedFileError should not happen post-walk-filter,
      // but a corrupt/oversized file must not abort the whole bulk index
    }
    filesProcessed += 1;

    let sinkReady = true;
    for (const node of result.nodes) {
      sinkReady = emitLine(sink, toWireNode(node));
      nodesEmitted += 1;
    }
    for (const edge of result.edges) {
      sinkReady = emitLine(sink, edge);
      edgesEmitted += 1;
    }
    // Only at a file boundary: a file's nodes and edges belong together (an
    // edge is only ever emitted between nodes of the file it came from), and
    // keeping them in one uninterrupted run is what lets a consumer commit
    // whatever it has at any point without ever holding a dangling edge.
    if (!sinkReady) await waitForDrain(sink);
  }

  return { filesProcessed, nodesEmitted, edgesEmitted };
}
