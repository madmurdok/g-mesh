import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import { existsSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import {
  SemanticProject,
  resolveTsserverPath,
  tsserverCandidates,
  type TsServerPosition,
} from "../src/semantic";

// These tests drive a **real** tsserver child against real projects on disk -
// there is no stub here, deliberately: the whole point of the ticket is
// proving the compiler integration itself works, and a fake tsserver would
// only prove this file's own assumptions about it. The cost is that each test
// that asks a semantic question pays one child startup (~1.2s measured on a
// small project), which is why there are few of them and each one asks
// several questions.

// __dirname is dist/test at runtime (package.json's `pretest` compiles first),
// so two levels up is the plugin's own root - itself a real TypeScript project
// with a real tsconfig.json, which test #4 below uses as its corpus.
const PLUGIN_ROOT = path.join(__dirname, "..", "..");

/** 1-based `{line, offset}` of `needle` inside `text`, tsserver's coordinate
 * system. Derived rather than hardcoded so a test does not silently start
 * probing the wrong token when the file it reads is edited. */
function positionOf(text: string, needle: string, offsetWithin = 0): TsServerPosition {
  const index = text.indexOf(needle);
  assert.notEqual(index, -1, `fixture text must contain ${JSON.stringify(needle)}`);
  const before = text.slice(0, index + offsetWithin);
  const lastNewline = before.lastIndexOf("\n");
  return { line: before.split("\n").length, offset: before.length - lastNewline };
}

async function makeProject(files: Record<string, string>): Promise<string> {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-semantic-"));
  for (const [rel, contents] of Object.entries(files)) {
    const abs = path.join(root, rel);
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, contents, "utf8");
  }
  return root;
}

// --- compiler selection ---------------------------------------------------

test("tsserver resolution prefers the project's own TypeScript over the bundled copy", async () => {
  const candidates = tsserverCandidates("/some/project");
  assert.deepEqual(candidates[0], path.join("/some/project", "node_modules", "typescript", "lib", "tsserver.js"));
  assert.equal(candidates.length, 2);
  assert.ok(candidates[1].endsWith(path.join("node_modules", "typescript", "lib", "tsserver.js")));
  assert.notEqual(candidates[1], candidates[0]);

  // The bundled copy is a real file, not an aspiration - `typescript` is a
  // runtime dependency precisely because tsserver ships inside it.
  assert.ok(existsSync(candidates[1]), "the bundled tsserver must exist");
});

test("resolveTsserverPath picks the project's own install when it exists, else the bundled one", async () => {
  const withOwn = await makeProject({
    "node_modules/typescript/lib/tsserver.js": "// stand-in for a project-pinned compiler\n",
    "src/a.ts": "export const a = 1;\n",
  });
  const withoutOwn = await makeProject({ "src/a.ts": "export const a = 1;\n" });
  try {
    assert.equal(
      resolveTsserverPath(withOwn),
      path.join(withOwn, "node_modules", "typescript", "lib", "tsserver.js"),
    );
    // Falls back rather than failing: a project with no TypeScript of its own
    // is still analyzable, just by this plugin's compiler.
    assert.equal(resolveTsserverPath(withoutOwn), tsserverCandidates(withoutOwn)[1]);
    assert.ok(existsSync(resolveTsserverPath(withoutOwn) as string));
  } finally {
    await fs.rm(withOwn, { recursive: true, force: true });
    await fs.rm(withoutOwn, { recursive: true, force: true });
  }
});

// --- lifecycle ------------------------------------------------------------

test("the tsserver child is not started until a semantic question is actually asked", async () => {
  const root = await makeProject({
    "tsconfig.json": JSON.stringify({ compilerOptions: { strict: true }, include: ["src"] }) + "\n",
    "src/a.ts": "export function alpha(): number {\n  return 1;\n}\n\nexport const used = alpha();\n",
  });
  const project = new SemanticProject(root);
  try {
    // Constructing must cost nothing: a plugin process for a project whose
    // semantic pass never runs must not pay a ~265MB child for it.
    assert.equal(project.isRunning, false, "constructing must not spawn anything");

    const file = path.join(root, "src", "a.ts");
    const source = await fs.readFile(file, "utf8");
    const definitions = await project.definition(file, positionOf(source, "alpha();", 1));
    assert.equal(definitions.length, 1);
    assert.equal(project.isRunning, true, "the first query must start the child");

    project.stop();
    assert.equal(project.isRunning, false, "stop() must end the child");

    // Restart is lazy and transparent, mirroring how core relaunches a dead
    // plugin on the next request that needs it rather than eagerly.
    const again = await project.definition(file, positionOf(source, "alpha();", 1));
    assert.equal(again.length, 1);
    assert.equal(project.isRunning, true);
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a tsserver that dies fails only the work in flight - the plugin survives and the next query respawns it", async () => {
  const root = await makeProject({
    "tsconfig.json": JSON.stringify({ include: ["src"] }) + "\n",
    "src/a.ts": "export function alpha(): number {\n  return 1;\n}\n\nexport const used = alpha();\n",
  });
  const project = new SemanticProject(root);
  const file = path.join(root, "src", "a.ts");
  try {
    const source = await fs.readFile(file, "utf8");
    const position = positionOf(source, "alpha();", 1);
    await project.definition(file, position);

    // Kill the child out from under an in-flight request: this is the
    // blast-radius case the subprocess exists for (an OOM in the checker
    // looks the same from here). The request must fail; this process must not.
    const server = project.ensureServer();
    const inFlight = project.definition(file, position);
    server.dispose();
    await assert.rejects(inFlight, /tsserver/);
    assert.equal(project.isRunning, false);

    const recovered = await project.definition(file, position);
    assert.equal(recovered.length, 1, "the next query must transparently start a fresh child");
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

// --- real semantic answers ------------------------------------------------

/**
 * The end-to-end proof, against a real project already in this repository:
 * this plugin itself. `src/index.ts` uses `FrameReader`, which is declared in
 * a *different* file - the cross-file case the structural layer cannot answer
 * on its own and the whole reason this layer exists.
 */
test("resolves a real cross-file declaration in this plugin's own source tree", async () => {
  const project = new SemanticProject(PLUGIN_ROOT);
  try {
    const indexPath = path.join(PLUGIN_ROOT, "src", "index.ts");
    const source = await fs.readFile(indexPath, "utf8");
    // The *use* site, inside main() - not the import statement.
    const definitions = await project.definition(indexPath, positionOf(source, "new FrameReader(", 5));

    assert.equal(definitions.length, 1, "FrameReader must resolve to exactly one declaration");
    const [definition] = definitions;
    assert.equal(definition.file, path.join(PLUGIN_ROOT, "src", "jsonrpc.ts"));

    // Verify against the file itself rather than a pinned line number: the
    // reported span must actually be the `FrameReader` class declaration.
    const target = await fs.readFile(definition.file, "utf8");
    const lines = target.split("\n");
    assert.ok(
      lines[definition.start.line - 1].includes("class FrameReader"),
      `expected the class declaration, got ${JSON.stringify(lines[definition.start.line - 1])}`,
    );

    // And it must have been answered under this project's own tsconfig.json,
    // not an inferred project that happened to guess the same answer.
    assert.equal(await project.configuredProjectFor(indexPath), path.join(PLUGIN_ROOT, "tsconfig.json"));
  } finally {
    project.stop();
  }
});

/**
 * `paths` aliases are the case where "respect the project's tsconfig" stops
 * being a formality: `@app/util` names no file and no package, only a rewrite
 * rule the project wrote down. tsserver applies it natively once pointed at
 * the project root - this pins that it really does, rather than that this
 * plugin's own tsconfigPaths.ts would have to be reused here.
 */
test("respects the project's tsconfig, including a paths alias, when resolving an import", async () => {
  const root = await makeProject({
    "tsconfig.json": JSON.stringify(
      {
        compilerOptions: {
          target: "ES2020",
          module: "commonjs",
          strict: true,
          baseUrl: ".",
          paths: { "@app/*": ["src/*"] },
        },
        include: ["src"],
      },
      null,
      2,
    ) + "\n",
    "src/util.ts": "export function helper(n: number): number {\n  return n + 1;\n}\n",
    "src/main.ts": 'import { helper } from "@app/util";\n\nexport function main(): number {\n  return helper(41);\n}\n',
  });
  const project = new SemanticProject(root);
  try {
    const mainPath = path.join(root, "src", "main.ts");
    const source = await fs.readFile(mainPath, "utf8");

    // (a) the aliased *specifier* resolves to the file it names.
    const moduleDefinition = await project.definition(mainPath, positionOf(source, '"@app/util"', 3));
    assert.equal(moduleDefinition.length, 1);
    assert.equal(moduleDefinition[0].file, path.join(root, "src", "util.ts"));

    // (b) a symbol imported through that alias resolves to its declaration,
    // which is the answer the semantic pass actually needs.
    const symbolDefinition = await project.definition(mainPath, positionOf(source, "helper(41)", 1));
    assert.equal(symbolDefinition.length, 1);
    assert.equal(symbolDefinition[0].file, path.join(root, "src", "util.ts"));
    const util = (await fs.readFile(symbolDefinition[0].file, "utf8")).split("\n");
    assert.ok(util[symbolDefinition[0].start.line - 1].includes("function helper"));

    assert.equal(await project.configuredProjectFor(mainPath), path.join(root, "tsconfig.json"));
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a project with no tsconfig.json is still answerable, through an inferred project", async () => {
  const root = await makeProject({
    "a.js": "function alpha(n) {\n  return n + 1;\n}\n\nmodule.exports = { alpha };\n",
    "b.js": 'const { alpha } = require("./a");\n\nalpha(1);\n',
  });
  const project = new SemanticProject(root);
  try {
    const bPath = path.join(root, "b.js");
    const source = await fs.readFile(bPath, "utf8");
    const definitions = await project.definition(bPath, positionOf(source, "alpha(1)", 1));
    assert.equal(definitions.length, 1);
    assert.equal(definitions[0].file, path.join(root, "a.js"));
    // No config was applied, and the caller can tell - an inferred project's
    // synthetic name must never be reported as if it were a real tsconfig.
    assert.equal(await project.configuredProjectFor(bPath), null);
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a missing tsserver is a clean failure, not a crash", async () => {
  const root = await makeProject({ "src/a.ts": "export const a = 1;\n" });
  const project = new SemanticProject(root, { tsserverPath: path.join(root, "no-such-tsserver.js") });
  try {
    await assert.rejects(
      project.definition(path.join(root, "src", "a.ts"), { line: 1, offset: 14 }),
      /tsserver/,
    );
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});
