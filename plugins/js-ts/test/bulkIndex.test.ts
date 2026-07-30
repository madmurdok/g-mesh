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

/** A symlink inside a fixture. `makeProject` only knows about regular files, and
 * deliberately stays that way (every other test here wants a plain tree), so the
 * symlink cases add the one link they are about on top of the tree it built. */
async function symlink(root: string, linkRelPath: string, target: string): Promise<void> {
  const linkAbs = path.join(root, linkRelPath);
  await fs.mkdir(path.dirname(linkAbs), { recursive: true });
  await fs.symlink(target, linkAbs);
}

async function walkedPaths(root: string): Promise<string[]> {
  const files: string[] = [];
  for await (const rel of walkProjectFiles(root)) files.push(rel);
  return files;
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

// --- symlink policy (task 87) ---------------------------------------------

test("a symlinked package directory is walked, and indexed under its own apparent path", async () => {
  const root = await makeProject({
    "vendor/real-lib/index.ts": `export const shared = 1;\n`,
  });
  try {
    // Nothing else lives at packages/aliased: the only way "index.ts" is
    // reachable there is by following the link.
    await symlink(root, "packages/aliased", "../vendor/real-lib");
    assert.deepEqual(await walkedPaths(root), ["packages/aliased/index.ts"]);
  } finally {
    await cleanup(root);
  }
});

/**
 * The maybe-surprising half of "index a real location exactly once": the winner
 * is whichever path the sorted, depth-first walk reaches first, and that can be
 * the *symlink*. Here `packages/` sorts before `vendor/`, so the alias is
 * reached first and claims the directory, and the real path is dropped outright
 * rather than being preferred for being real or merged onto the alias.
 *
 * This is correct, not a bug to "fix" later: a file's identity in this index is
 * the project-relative path it was reached by, so there is no canonical path to
 * prefer - only a deterministic rule for picking one of two, which is what the
 * sort makes it.
 */
test("two paths onto the same real directory index it exactly once, sorted-first path winning even when that is the symlink", async () => {
  const root = await makeProject({
    "vendor/shared/thing.ts": `export const thing = 1;\n`,
  });
  try {
    await symlink(root, "packages/dup", "../vendor/shared");
    assert.deepEqual(await walkedPaths(root), ["packages/dup/thing.ts"]);
  } finally {
    await cleanup(root);
  }
});

test("a symlinked file aliasing an already-walked real file is skipped, not indexed twice", async () => {
  const root = await makeProject({
    "src/index.ts": `export const x = 1;\n`,
  });
  try {
    // Named to sort *after* index.ts on purpose: the real file is reached first
    // and claims the location, so the alias is the one dropped. Named
    // "alias.ts" instead, the same rule would flip the winner - which is the
    // point of the test above, and why this one pins the ordering explicitly
    // rather than implying links always lose.
    await symlink(root, "src/zalias.ts", "./index.ts");
    const files = await walkedPaths(root);
    assert.ok(files.includes("src/index.ts"), "the real file must still be indexed");
    assert.ok(!files.includes("src/zalias.ts"), "the alias must not be indexed a second time");
    assert.deepEqual(files, ["src/index.ts"]);
  } finally {
    await cleanup(root);
  }
});

test("a symlink pointing back at its own containing directory does not loop forever", async () => {
  const root = await makeProject({
    "cycle/a.ts": `export const a = 1;\n`,
  });
  try {
    // A genuine cycle: cycle/loop resolves to cycle itself, which the walk has
    // already claimed by the time it looks at this entry. If the guard were
    // missing, this test would hang rather than fail - the runner's own timeout
    // is the assertion.
    await symlink(root, "cycle/loop", ".");
    assert.deepEqual(await walkedPaths(root), ["cycle/a.ts"]);
  } finally {
    await cleanup(root);
  }
});

test("a symlink resolving outside the project root is refused", async () => {
  const root = await makeProject({ "src/index.ts": `export const x = 1;\n` });
  const outsideRoot = await makeProject({ "outside/secret.ts": `export const secret = 1;\n` });
  try {
    await symlink(root, "packages/escape", path.join(outsideRoot, "outside"));
    const files = await walkedPaths(root);
    assert.ok(
      files.every((rel) => !rel.startsWith("packages/escape")),
      `nothing outside the project may be indexed, got ${JSON.stringify(files)}`,
    );
    assert.deepEqual(files, ["src/index.ts"]);
  } finally {
    await cleanup(root);
    await cleanup(outsideRoot);
  }
});

test("a dangling symlink is skipped rather than throwing", async () => {
  const root = await makeProject({ "src/index.ts": `export const x = 1;\n` });
  try {
    await symlink(root, "packages/broken", "./does-not-exist");
    const files = await walkedPaths(root);
    assert.ok(files.every((rel) => !rel.startsWith("packages/broken")));
    assert.deepEqual(files, ["src/index.ts"]);
  } finally {
    await cleanup(root);
  }
});

// --- import resolution over a real tree -----------------------------------

/**
 * Every import placeholder the walk emitted. `Module` is also the kind of a
 * pending *symbol* placeholder (extract.ts's `PENDING_SYMBOL_NATIVE_KIND`),
 * which stands for one imported name rather than for the module itself - a
 * different handshake, and not what these two tests are about.
 */
async function modulesOf(root: string): Promise<WireNode[]> {
  const lines: string[] = [];
  await bulkIndexProject(root, (line) => lines.push(line));
  return lines
    .map((line) => JSON.parse(line))
    .filter(
      (parsed): parsed is WireNode =>
        "range" in parsed &&
        parsed.kind === "Module" &&
        (parsed.nativeKind === "resolved_module" || parsed.nativeKind === "external_module"),
    );
}

test("relative imports are resolved against the real tree, off-workspace packages are not", async () => {
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

test("an import of a gitignored file stays an unresolved placeholder, matching the walk's own exclusion policy", async () => {
  const root = await makeProject({
    "src/index.ts": `import { secret } from "./generated";\nexport const x = secret;\n`,
    "src/generated.ts": `export const secret = 1;\n`,
    ".gitignore": "src/generated.ts\n",
  });
  try {
    const modules = await modulesOf(root);
    assert.equal(modules.length, 1);
    assert.equal(modules[0].qualifiedName, "./generated", "unresolved - the raw specifier, not a path");
    assert.equal(
      modules[0].nativeKind,
      "external_module",
      "resolution now agrees with the walk: a gitignored target is never indexed, so it is never claimed as resolved either",
    );
  } finally {
    await cleanup(root);
  }
});

test("a relative import into a hard-excluded directory is also treated as unresolved", async () => {
  const root = await makeProject({
    "src/index.ts": `import { secret } from "./dist/generated";\nexport const x = secret;\n`,
    "src/dist/generated.ts": `export const secret = 1;\n`,
  });
  try {
    const modules = await modulesOf(root);
    assert.equal(modules.length, 1);
    assert.equal(modules[0].qualifiedName, "./dist/generated");
    assert.equal(modules[0].nativeKind, "external_module");
  } finally {
    await cleanup(root);
  }
});

/**
 * The monorepo case this resolution exists for: cross-package usage is
 * written as a package name, not a relative path, so without workspace
 * resolution neither the IMPORTS edge nor the imported *symbol* behind it has
 * anything to point at - which is exactly what made find_callers blind to
 * `pointFrom`'s real call sites in the excalidraw corpus.
 */
test("a workspace package import resolves to the package's source, symbols included", async () => {
  const root = await makeProject({
    "pnpm-workspace.yaml": "packages:\n  - 'packages/*'\n",
    "packages/math/package.json": JSON.stringify({
      name: "@excalidraw/math",
      main: "./dist/prod/index.js", // build output, absent from an unbuilt checkout
    }),
    "packages/math/src/index.ts": `export const pointFrom = (x: number) => x;\n`,
    "packages/element/package.json": JSON.stringify({ name: "@excalidraw/element" }),
    "packages/element/src/shape.ts": `import { pointFrom } from "@excalidraw/math";
import { useState } from "react";
export const shape = () => pointFrom(1) + useState();
`,
  });
  try {
    const lines: string[] = [];
    await bulkIndexProject(root, (line) => lines.push(line));
    const nodes = lines.map((line) => JSON.parse(line)).filter((parsed) => "range" in parsed) as WireNode[];
    const byName = new Map(nodes.map((node) => [`${node.name}:${node.nativeKind}`, node]));

    const target = byName.get("@excalidraw/math:resolved_module");
    assert.ok(target, "the workspace package must resolve to a file");
    assert.equal(target.qualifiedName, "packages/math/src/index.ts");

    const external = byName.get("react:external_module");
    assert.ok(external, "a registry package must stay an unresolved placeholder");
    assert.equal(external.qualifiedName, "react");

    // The payoff: the imported name now has a placeholder core can link to
    // the real declaration, because the specifier resolved.
    const pending = byName.get("pointFrom:pending_symbol");
    assert.ok(pending, "the symbol imported across packages must get a placeholder");
    assert.equal(pending.qualifiedName, "packages/math/src/index.ts#pointFrom");
    assert.equal(byName.has("useState:pending_symbol"), false, "nothing to link a package symbol to");
  } finally {
    await cleanup(root);
  }
});

/** Same package, now *built*: the declared entry is physically on disk, and it
 * is the one file of the pair the walk would never index. */
test("a workspace package's declared entry under dist/ does not shadow its source, even though dist physically exists", async () => {
  const root = await makeProject({
    "pnpm-workspace.yaml": "packages:\n  - 'packages/*'\n",
    "packages/math/package.json": JSON.stringify({ name: "@excalidraw/math", main: "./dist/prod/index.js" }),
    "packages/math/dist/prod/index.js": `export const pointFrom = () => 0; // must not win\n`,
    "packages/math/src/index.ts": `export const pointFrom = (x: number) => x;\n`,
    "packages/element/package.json": JSON.stringify({ name: "@excalidraw/element" }),
    "packages/element/src/shape.ts": `import { pointFrom } from "@excalidraw/math";\nexport const shape = () => pointFrom(1);\n`,
  });
  try {
    const lines: string[] = [];
    await bulkIndexProject(root, (line) => lines.push(line));
    const nodes = lines.map((line) => JSON.parse(line)).filter((p) => "range" in p) as WireNode[];
    const byName = new Map(nodes.map((n) => [`${n.name}:${n.nativeKind}`, n]));
    const target = byName.get("@excalidraw/math:resolved_module");
    assert.ok(target, "the workspace package must still resolve");
    assert.equal(target.qualifiedName, "packages/math/src/index.ts");
  } finally {
    await cleanup(root);
  }
});

test("a workspace package's declared entry under a gitignored (not hard-excluded-named) directory does not shadow its source", async () => {
  const root = await makeProject({
    "pnpm-workspace.yaml": "packages:\n  - 'packages/*'\n",
    "packages/math/package.json": JSON.stringify({ name: "@excalidraw/math", main: "./build-output/index.js" }),
    "packages/math/.gitignore": "build-output/\n",
    "packages/math/build-output/index.js": `export const pointFrom = () => 0; // must not win\n`,
    "packages/math/src/index.ts": `export const pointFrom = (x: number) => x;\n`,
    "packages/element/package.json": JSON.stringify({ name: "@excalidraw/element" }),
    "packages/element/src/shape.ts": `import { pointFrom } from "@excalidraw/math";\nexport const shape = () => pointFrom(1);\n`,
  });
  try {
    const lines: string[] = [];
    await bulkIndexProject(root, (line) => lines.push(line));
    const nodes = lines.map((line) => JSON.parse(line)).filter((p) => "range" in p) as WireNode[];
    const byName = new Map(nodes.map((n) => [`${n.name}:${n.nativeKind}`, n]));
    const target = byName.get("@excalidraw/math:resolved_module");
    assert.ok(target);
    assert.equal(target.qualifiedName, "packages/math/src/index.ts");
  } finally {
    await cleanup(root);
  }
});

// --- `#private` package-imports specifiers, over a real tree --------------

test("a `#private` import resolves to the real file its package.json `imports` map names", async () => {
  const root = await makeProject({
    "package.json": JSON.stringify({ name: "single", imports: { "#util": "./src/util.ts" } }),
    "src/util.ts": `export const util = 1;\n`,
    "src/index.ts": `import { util } from "#util";\nexport const start = () => util;\n`,
  });
  try {
    const modules = await modulesOf(root);
    const target = modules.find((m) => m.name === "#util");
    assert.ok(target, "no placeholder for #util");
    assert.equal(target.nativeKind, "resolved_module");
    assert.equal(target.qualifiedName, "src/util.ts");
  } finally {
    await cleanup(root);
  }
});

test("a `#private` import with no matching `imports` key stays an unresolved placeholder, not a crash", async () => {
  const root = await makeProject({
    "package.json": JSON.stringify({ name: "single", imports: { "#util": "./src/util.ts" } }),
    "src/util.ts": `export const util = 1;\n`,
    "src/index.ts": `import { gone } from "#gone";\nexport const start = () => gone;\n`,
  });
  try {
    const modules = await modulesOf(root);
    assert.equal(modules.length, 1);
    assert.equal(modules[0].qualifiedName, "#gone", "unresolved - the raw specifier, not a path");
    assert.equal(modules[0].nativeKind, "external_module");
  } finally {
    await cleanup(root);
  }
});
