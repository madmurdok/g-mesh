import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import { extractFile, type ExtractedEdge } from "../src/extract";
import { createProjectResolver } from "../src/resolve";
import { SemanticProject } from "../src/semantic";
import { runSemanticPass } from "../src/semanticPass";

// Like semantic.test.ts, these drive a **real** tsserver child against a real
// project on disk. The whole claim being tested is "TypeScript's own module
// resolution knows which declaration an ambiguous `export *` hands a
// consumer", and a stub could only prove this file's assumptions about it.

async function makeProject(files: Record<string, string>): Promise<string> {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-semantic-pass-"));
  for (const [rel, contents] of Object.entries(files)) {
    const abs = path.join(root, rel);
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, contents, "utf8");
  }
  return root;
}

const TSCONFIG = JSON.stringify({ compilerOptions: { strict: true } }) + "\n";

/**
 * The scenario core's `two_reexport_branches_offering_one_name_leave_the_edge_unresolved`
 * documents: one barrel, two `export *` branches, both offering `mutate`.
 * `caller.ts` imports it through the barrel, so the address it writes down is
 * the barrel's and neither branch is more obviously right than the other.
 */
function ambiguousBarrel(order: readonly string[]): Record<string, string> {
  return {
    "tsconfig.json": TSCONFIG,
    "a.ts": 'export function mutate(): "a" {\n  return "a";\n}\n',
    "b.ts": 'export function mutate(): "b" {\n  return "b";\n}\n',
    "index.ts": order.map((file) => `export * from "./${file}";`).join("\n") + "\n",
    "caller.ts": 'import { mutate } from "./index";\n\nexport function run(): void {\n  mutate();\n}\n',
  };
}

/** The id the structural pass gives `file`'s exported declaration of `name` -
 * derived the same way the walk derives it, rather than hardcoded, so the test
 * pins *which declaration* was chosen and not a hashing detail. */
async function declarationId(root: string, file: string, name: string): Promise<string> {
  const source = await fs.readFile(path.join(root, file), "utf8");
  const result = extractFile(file, source, { resolveSpecifier: createProjectResolver(root) });
  const node = result.nodes.find((candidate) => candidate.exported && candidate.name === name);
  assert.ok(node, `${file} must export a ${name}`);
  return node.id;
}

/** The structural pass's own view of `caller.ts`: the `CALLS` edge it left on
 * a pending-symbol placeholder, unresolved, because only the index knows what
 * `./index` really publishes. */
function structuralCallEdge(root: string): ExtractedEdge {
  const source =
    'import { mutate } from "./index";\n\nexport function run(): void {\n  mutate();\n}\n';
  const result = extractFile("caller.ts", source, { resolveSpecifier: createProjectResolver(root) });
  const call = result.edges.find((edge) => edge.kind === "CALLS");
  assert.ok(call, "the structural pass must produce a CALLS edge for caller.ts");
  assert.equal(call.resolved, false, "and must leave it unresolved - it cannot see index.ts");
  assert.equal(call.source, "tree-sitter");
  return call;
}

test("two `export *` branches offering one name resolve to the branch TypeScript picks", async () => {
  const root = await makeProject(ambiguousBarrel(["a", "b"]));
  const project = new SemanticProject(root);
  try {
    const structural = structuralCallEdge(root);

    const diff = await runSemanticPass(root, ["caller.ts"], { project });

    assert.equal(diff.upsertEdges.length, 1, "exactly the one edge that was ambiguous");
    const [edge] = diff.upsertEdges;
    // Same id as the structural edge, so core's `ON CONFLICT(id) DO UPDATE`
    // rewrites that row rather than adding a second one alongside it.
    assert.equal(edge.id, structural.id);
    assert.equal(edge.fromId, structural.fromId);
    assert.equal(edge.kind, "CALLS");
    assert.equal(edge.source, "ts-compiler");
    assert.equal(edge.resolved, true);
    // `export * from "./a"` comes first, so `a.ts`'s declaration is the one a
    // consumer actually sees (`tsc` calls the second branch a TS2308
    // ambiguity, not the consumer's import).
    assert.equal(edge.toId, await declarationId(root, "a.ts", "mutate"));
    assert.notEqual(edge.toId, structural.toId, "it must have moved off the placeholder");

    // The declaration travels with the edge, so the diff cannot land core in a
    // dangling-foreign-key state whatever it already holds.
    assert.deepEqual(
      diff.upsertNodes.map((node) => [node.filePath, node.name, node.kind]),
      [["a.ts", "mutate", "Function"]],
    );
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("swapping the two `export *` statements swaps the declaration the pass lands on", async () => {
  const root = await makeProject(ambiguousBarrel(["b", "a"]));
  const project = new SemanticProject(root);
  try {
    const diff = await runSemanticPass(root, ["caller.ts"], { project });

    assert.equal(diff.upsertEdges.length, 1);
    // Not the shortest chain, not the alphabetically first file: the first
    // `export *` in the barrel's own source order is what wins, which is the
    // rule the checker implements and the reason this is asked rather than
    // reimplemented.
    assert.equal(diff.upsertEdges[0].toId, await declarationId(root, "b.ts", "mutate"));
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a name the target file declares itself is left to the structural layer", async () => {
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "target.ts": "export function mutate(): void {}\n",
    "caller.ts": 'import { mutate } from "./target";\n\nexport function run(): void {\n  mutate();\n}\n',
  });
  const project = new SemanticProject(root);
  try {
    const diff = await runSemanticPass(root, ["caller.ts"], { project });

    // Core's own breadth-first walk links this in a lookup. Answering it here
    // would spend a subprocess round trip to reach the same node and would
    // flip a perfectly good `tree-sitter` edge's source for nothing.
    assert.deepEqual(diff.upsertEdges, []);
    assert.equal(project.isRunning, false, "and no checker is started to decide that");
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a whole-project pass finds the same edge without being told which file to look at", async () => {
  const root = await makeProject(ambiguousBarrel(["a", "b"]));
  const project = new SemanticProject(root);
  try {
    const diff = await runSemanticPass(root, [], { project });

    assert.equal(diff.upsertEdges.length, 1);
    assert.equal(diff.upsertEdges[0].toId, await declarationId(root, "a.ts", "mutate"));
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a barrel whose branches all end outside the index leaves the edge alone", async () => {
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    // The chain leaves the project: `mutate` is declared in a package, which
    // is a real answer to the language's question and not one this index can
    // point at.
    "node_modules/pkg/index.d.ts": "export declare function mutate(): void;\n",
    "node_modules/pkg/package.json": JSON.stringify({ name: "pkg", types: "index.d.ts" }) + "\n",
    "index.ts": 'export * from "pkg";\n',
    "caller.ts": 'import { mutate } from "./index";\n\nexport function run(): void {\n  mutate();\n}\n',
  });
  const project = new SemanticProject(root);
  try {
    const diff = await runSemanticPass(root, ["caller.ts"], { project });

    assert.deepEqual(diff.upsertEdges, [], "a missing edge beats one onto a file core never indexed");
    assert.deepEqual(diff.upsertNodes, []);
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a checker that cannot be started costs the pass its answers, not the plugin", async () => {
  const root = await makeProject(ambiguousBarrel(["a", "b"]));
  // A path that exists but is not a tsserver: the child starts and dies, which
  // is the shape of a checker OOM as far as this pass is concerned.
  const project = new SemanticProject(root, {
    tsserverPath: path.join(root, "a.ts"),
    requestTimeoutMs: 5_000,
  });
  try {
    const diff = await runSemanticPass(root, ["caller.ts"], { project });
    assert.deepEqual(diff.upsertEdges, []);
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});
