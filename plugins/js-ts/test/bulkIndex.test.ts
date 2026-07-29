import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import { PassThrough } from "node:stream";

import { bulkIndexProject, toWireNode, walkProjectFiles, type WireNode, type WireEdge } from "../src/bulkIndex";
import type { EdgeKind, EdgeSource, NodeKind } from "../src/extract";

const NODE_KINDS: ReadonlySet<NodeKind> = new Set(["File", "Module", "Type", "Function", "Variable"]);
const EDGE_KINDS: ReadonlySet<EdgeKind> = new Set([
  "DEFINES",
  "IMPORTS",
  "CALLS",
  "SUPERTYPE_OF",
  "REFERENCES",
  "EXPORTS",
]);
const EDGE_SOURCES: ReadonlySet<EdgeSource> = new Set(["tree-sitter", "ts-compiler"]);

/** Builds a small real project directory under a fresh tempdir; `files` maps
 * project-relative paths to their contents (mirrors how other tests in this
 * repo build fixtures on disk rather than mocking the filesystem). */
async function makeProject(files: Record<string, string>): Promise<string> {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-bulk-"));
  for (const [rel, contents] of Object.entries(files)) {
    const abs = path.join(root, rel);
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, contents, "utf8");
  }
  return root;
}

async function cleanup(root: string): Promise<void> {
  await fs.rm(root, { recursive: true, force: true });
}

/** Every field the wire contract (core/src/protocol/types.rs) requires,
 * checked without invoking any Rust code - see the ticket report for why
 * this structural check was chosen over a cross-process Rust conformance
 * test. */
function assertConformsToWireShape(parsed: unknown): void {
  assert.equal(typeof parsed, "object");
  assert.ok(parsed !== null);
  const obj = parsed as Record<string, unknown>;

  if ("range" in obj) {
    // WireNode
    assert.equal(typeof obj.id, "string");
    assert.ok(NODE_KINDS.has(obj.kind as NodeKind), `unexpected NodeKind ${String(obj.kind)}`);
    assert.equal(typeof obj.name, "string");
    assert.equal(typeof obj.qualifiedName, "string");
    assert.equal(typeof obj.filePath, "string");
    const range = obj.range as { start: { line: unknown; col: unknown }; end: { line: unknown; col: unknown } };
    assert.equal(typeof range.start.line, "number");
    assert.equal(typeof range.start.col, "number");
    assert.equal(typeof range.end.line, "number");
    assert.equal(typeof range.end.col, "number");
    assert.equal(typeof obj.exported, "boolean");
    assert.equal(typeof obj.language, "string");
    assert.equal(typeof obj.hasSyntaxErrors, "boolean");
    assert.ok(
      obj.signature === null || typeof obj.signature === "string",
      "signature must be string or null",
    );
    assert.ok(
      obj.docComment === null || typeof obj.docComment === "string",
      "docComment must be string or null",
    );
    assert.ok(
      obj.nativeKind === null || typeof obj.nativeKind === "string",
      "nativeKind must be string or null",
    );
    // Flat extract.ts fields must not leak onto the wire.
    assert.equal(obj.startLine, undefined);
    assert.equal(obj.startCol, undefined);
    assert.equal(obj.endLine, undefined);
    assert.equal(obj.endCol, undefined);
  } else {
    // WireEdge
    assert.equal(typeof obj.id, "string");
    assert.equal(typeof obj.fromId, "string");
    assert.equal(typeof obj.toId, "string");
    assert.ok(EDGE_KINDS.has(obj.kind as EdgeKind), `unexpected EdgeKind ${String(obj.kind)}`);
    assert.ok(EDGE_SOURCES.has(obj.source as EdgeSource), `unexpected EdgeSource ${String(obj.source)}`);
    assert.equal(typeof obj.resolved, "boolean");
  }
}

test("streams valid NDJSON matching the wire contract for a small project", async () => {
  const root = await makeProject({
    "src/a.ts": `export function double(n: number): number {\n  return n * 2;\n}\n`,
    "src/b.ts": `import { double } from "./a";\n\nexport function quadruple(n: number): number {\n  return double(double(n));\n}\n`,
  });
  try {
    const lines: string[] = [];
    const summary = await bulkIndexProject(root, (line) => lines.push(line));

    assert.ok(lines.length > 0);
    assert.equal(lines.length, summary.nodesEmitted + summary.edgesEmitted);

    for (const line of lines) {
      assert.doesNotThrow(() => JSON.parse(line), `not valid JSON: ${line}`);
      const parsed = JSON.parse(line);
      assertConformsToWireShape(parsed);
    }

    const parsedLines = lines.map((l) => JSON.parse(l));
    const files = parsedLines.filter(
      (p): p is WireNode => "range" in p && p.kind === "File",
    );
    assert.deepEqual(
      new Set(files.map((f) => f.filePath)),
      new Set(["src/a.ts", "src/b.ts"]),
    );
  } finally {
    await cleanup(root);
  }
});

test("toWireNode nests flat range fields", () => {
  const wire = toWireNode({
    id: "n1",
    kind: "Function",
    name: "f",
    qualifiedName: "f",
    filePath: "a.ts",
    startLine: 1,
    startCol: 2,
    endLine: 3,
    endCol: 4,
    exported: false,
    language: "typescript",
    hasSyntaxErrors: false,
  });
  assert.deepEqual(wire.range, { start: { line: 1, col: 2 }, end: { line: 3, col: 4 } });
  assert.equal(wire.signature, null);
  assert.equal(wire.docComment, null);
  assert.equal(wire.nativeKind, null);
});

test("always skips node_modules, dist, .git, and .claude regardless of .gitignore", async () => {
  const root = await makeProject({
    "src/index.ts": `export const x = 1;\n`,
    "node_modules/dep/index.ts": `export const shouldNotAppear = 1;\n`,
    "dist/index.ts": `export const alsoShouldNotAppear = 1;\n`,
    ".git/objects/index.ts": `export const definitelyNot = 1;\n`,
    // A stale full-project copy under a Claude Code worktree - real source
    // never lives here, and nobody's own .gitignore anticipates it.
    ".claude/worktrees/agent-abc123/src/index.ts": `export const staleDuplicate = 1;\n`,
    // A .gitignore that does NOT mention these dirs at all - hard exclusion
    // must not depend on it.
    ".gitignore": "*.log\n",
  });
  try {
    const lines: string[] = [];
    await bulkIndexProject(root, (line) => lines.push(line));
    const filePaths = lines.map((l) => JSON.parse(l)).filter((p) => "filePath" in p).map((p) => p.filePath);
    assert.ok(filePaths.every((p: string) => !p.includes("node_modules")));
    assert.ok(filePaths.every((p: string) => !p.includes("dist")));
    assert.ok(filePaths.every((p: string) => !p.includes(".git")));
    assert.ok(filePaths.every((p: string) => !p.includes(".claude")));
    assert.ok(filePaths.includes("src/index.ts"));
  } finally {
    await cleanup(root);
  }
});

test("respects a root .gitignore excluding a specific file", async () => {
  const root = await makeProject({
    "src/kept.ts": `export const kept = 1;\n`,
    "src/ignored.ts": `export const ignored = 1;\n`,
    ".gitignore": "src/ignored.ts\n",
  });
  try {
    const files: string[] = [];
    for await (const rel of walkProjectFiles(root)) files.push(rel);
    assert.deepEqual(files.sort(), ["src/kept.ts"]);
  } finally {
    await cleanup(root);
  }
});

test("respects a nested .gitignore scoped to its own subdirectory", async () => {
  const root = await makeProject({
    "pkg/a/kept.ts": `export const kept = 1;\n`,
    "pkg/a/ignored.ts": `export const ignored = 1;\n`,
    "pkg/a/.gitignore": "ignored.ts\n",
    "pkg/b/ignored.ts": `export const notIgnoredHere = 1;\n`, // same basename, different dir
  });
  try {
    const files: string[] = [];
    for await (const rel of walkProjectFiles(root)) files.push(rel);
    assert.deepEqual(files.sort(), ["pkg/a/kept.ts", "pkg/b/ignored.ts"]);
  } finally {
    await cleanup(root);
  }
});

test("a negation pattern re-includes a path excluded by an earlier rule", async () => {
  const root = await makeProject({
    "src/keep-me.ts": `export const a = 1;\n`,
    "src/skip-me.ts": `export const b = 1;\n`,
    ".gitignore": "src/*.ts\n!src/keep-me.ts\n",
  });
  try {
    const files: string[] = [];
    for await (const rel of walkProjectFiles(root)) files.push(rel);
    assert.deepEqual(files.sort(), ["src/keep-me.ts"]);
  } finally {
    await cleanup(root);
  }
});

test("a file with syntax errors still contributes its recoverable nodes", async () => {
  const root = await makeProject({
    // Missing closing brace - tree-sitter's error-tolerant parse should
    // still recover the function declaration.
    "src/broken.ts": `export function broken(n: number): number {\n  return n + 1;\n`,
  });
  try {
    const lines: string[] = [];
    await bulkIndexProject(root, (line) => lines.push(line));
    const parsed = lines.map((l) => JSON.parse(l));
    const nodes = parsed.filter((p) => "range" in p) as WireNode[];
    assert.ok(nodes.length > 0, "a broken file must still yield some nodes");
    assert.ok(nodes.some((n) => n.hasSyntaxErrors === true));
    assert.ok(nodes.some((n) => n.kind === "Function" && n.name === "broken"));
  } finally {
    await cleanup(root);
  }
});

test("an empty project directory produces zero output lines", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-bulk-"));
  try {
    const lines: string[] = [];
    const summary = await bulkIndexProject(root, (line) => lines.push(line));
    assert.deepEqual(lines, []);
    assert.deepEqual(summary, { filesProcessed: 0, nodesEmitted: 0, edgesEmitted: 0 });
  } finally {
    await cleanup(root);
  }
});

test("a project with only unsupported file types produces zero output lines", async () => {
  const root = await makeProject({
    "README.md": "# hello\n",
    "package.json": "{}\n",
    "notes.txt": "just text\n",
  });
  try {
    const lines: string[] = [];
    await bulkIndexProject(root, (line) => lines.push(line));
    assert.deepEqual(lines, []);
  } finally {
    await cleanup(root);
  }
});

test("a project where every file is gitignored produces zero output lines", async () => {
  const root = await makeProject({
    "src/a.ts": `export const a = 1;\n`,
    "src/b.ts": `export const b = 1;\n`,
    ".gitignore": "*\n",
  });
  try {
    const lines: string[] = [];
    await bulkIndexProject(root, (line) => lines.push(line));
    assert.deepEqual(lines, []);
  } finally {
    await cleanup(root);
  }
});

test("does not buffer: the sink is invoked once per emitted node/edge, grouped per file in walk order", async () => {
  const root = await makeProject({
    "src/a.ts": `export function fa(): number {\n  return 1;\n}\n`,
    "src/b.ts": `export function fb(): number {\n  return 2;\n}\n`,
    "src/c.ts": `export function fc(): number {\n  return 3;\n}\n`,
  });
  try {
    const calls: string[] = [];
    const summary = await bulkIndexProject(root, (line) => {
      // A single call must carry exactly one JSON object - never a
      // newline-joined batch of several lines.
      assert.ok(!line.includes("\n"), "sink call carried more than one line");
      calls.push(line);
    });

    // One discrete sink invocation per node/edge - not one invocation for
    // the whole project's output.
    assert.equal(calls.length, summary.nodesEmitted + summary.edgesEmitted);
    assert.ok(calls.length >= 3, "expected multiple discrete emissions across 3 files");

    // Each file's File-node line appears before the next file's, proving
    // files are processed (and their output flushed) one at a time rather
    // than extracted in bulk and interleaved/sorted afterward.
    const fileNodeFilePaths = calls
      .map((l) => JSON.parse(l))
      .filter((p) => "range" in p && p.kind === "File")
      .map((p: WireNode) => p.filePath);
    assert.deepEqual(fileNodeFilePaths, ["src/a.ts", "src/b.ts", "src/c.ts"]);
  } finally {
    await cleanup(root);
  }
});

test("accepts a real NodeJS.WritableStream sink, one write() call per line", async () => {
  const root = await makeProject({
    "src/a.ts": `export function fa(): number {\n  return 1;\n}\n`,
    "src/b.ts": `export function fb(): number {\n  return 2;\n}\n`,
  });
  try {
    const stream = new PassThrough();
    const writeCalls: unknown[] = [];
    const originalWrite = stream.write.bind(stream);
    // Own-property override shadows the prototype method, so every call
    // this module makes is recorded before delegating to the real stream.
    (stream as unknown as { write: typeof stream.write }).write = ((
      chunk: unknown,
      ...rest: unknown[]
    ) => {
      writeCalls.push(chunk);
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      return (originalWrite as any)(chunk, ...rest);
    }) as typeof stream.write;

    const chunks: Buffer[] = [];
    stream.on("data", (chunk: Buffer) => chunks.push(chunk));

    const summary = await bulkIndexProject(root, stream);
    stream.end();

    assert.equal(writeCalls.length, summary.nodesEmitted + summary.edgesEmitted);

    const text = Buffer.concat(chunks).toString("utf8");
    const lines = text.split("\n").filter((l) => l.length > 0);
    assert.equal(lines.length, summary.nodesEmitted + summary.edgesEmitted);
    for (const line of lines) {
      assertConformsToWireShape(JSON.parse(line));
    }
  } finally {
    await cleanup(root);
  }
});

// --- import resolution over a real tree -----------------------------------

/** Every `Module` node the walk emitted, keyed by the file that imported it. */
async function modulesOf(root: string): Promise<WireNode[]> {
  const lines: string[] = [];
  await bulkIndexProject(root, (line) => lines.push(line));
  return lines
    .map((line) => JSON.parse(line))
    .filter((parsed): parsed is WireNode => "range" in parsed && parsed.kind === "Module");
}

test("relative imports are resolved against the real tree, bare ones are not", async () => {
  const root = await makeProject({
    // The ESM-TypeScript spelling: the specifier names a `.js` file that does
    // not exist on disk at all, only the `.ts` it is compiled from.
    "src/index.ts": `import { connect } from "./db/connection.js";
import { z } from "zod";
import { pool } from "./db";
import { fmt } from "./util";
import { gone } from "./deleted.js";
export const start = () => connect(pool, fmt(z));
`,
    "src/db/connection.ts": `export const connect = () => 1;\n`,
    "src/db/index.ts": `export const pool = 1;\n`,
    "src/util.tsx": `export const fmt = (x: unknown) => String(x);\n`,
  });
  try {
    const targets = new Map((await modulesOf(root)).map((m) => [m.name, m]));

    for (const [specifier, expected] of [
      ["./db/connection.js", "src/db/connection.ts"],
      ["./db", "src/db/index.ts"],
      ["./util", "src/util.tsx"],
    ] as const) {
      const target = targets.get(specifier);
      assert.ok(target, `no placeholder for ${specifier}`);
      assert.equal(target.qualifiedName, expected, `${specifier} must resolve to ${expected}`);
      assert.equal(target.nativeKind, "resolved_module");
      assert.equal(target.filePath, "src/index.ts", "the placeholder still belongs to the importing file");
    }

    // A package, and a relative import of a file that is not there: both stay
    // exactly what they were before resolution existed.
    for (const specifier of ["zod", "./deleted.js"]) {
      const target = targets.get(specifier);
      assert.ok(target, `no placeholder for ${specifier}`);
      assert.equal(target.qualifiedName, specifier);
      assert.equal(target.nativeKind, "external_module");
    }
  } finally {
    await cleanup(root);
  }
});

test("an import of a gitignored file resolves to nothing rather than to a node that is not there", async () => {
  const root = await makeProject({
    "src/index.ts": `import { secret } from "./generated";\nexport const x = secret;\n`,
    "src/generated.ts": `export const secret = 1;\n`,
    ".gitignore": "src/generated.ts\n",
  });
  try {
    // The walk skips the target, so nothing would ever create its `File`
    // node - core would find no target and leave the placeholder alone, but
    // the honest answer is available one step earlier than that.
    const modules = await modulesOf(root);
    assert.equal(modules.length, 1);
    assert.equal(modules[0].qualifiedName, "src/generated.ts");
    assert.equal(
      modules[0].nativeKind,
      "resolved_module",
      "resolution answers about the filesystem; whether the target is indexed is core's question",
    );
  } finally {
    await cleanup(root);
  }
});
