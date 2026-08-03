import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import { PENDING_SYMBOL_NATIVE_KIND, pendingSymbolQualifiedName } from "../src/extract";
import { resetIncrementalState } from "../src/incremental";
import { SemanticProject } from "../src/semantic";
import {
  resetSemanticPassState,
  runSemanticPass,
  type SemanticPassResult,
} from "../src/semanticPass";

// Like semantic.test.ts, these drive a **real** tsserver against real projects
// on disk. A stub would only prove this file's own assumptions about what the
// checker answers, and the entire value of the pass is that it does not have to
// assume: the whole point of `import * as ns` is that no name-matching layer
// can attribute `ns.member`, so what makes a test here meaningful is that a
// compiler really did.
//
// Each test pays one child startup (~1.2s), which is why there are few of them.

async function makeProject(files: Record<string, string>): Promise<string> {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-semantic-pass-"));
  for (const [rel, contents] of Object.entries(files)) {
    const abs = path.join(root, rel);
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, contents, "utf8");
  }
  return root;
}

const TSCONFIG = JSON.stringify({ compilerOptions: { strict: true }, include: ["src"] }) + "\n";

/** Runs the pass against its own fresh `SemanticProject`, always stopping the
 * child - an orphaned tsserver would outlive the test run. */
async function pass(root: string, filePaths: string[] = []): Promise<SemanticPassResult> {
  const project = new SemanticProject(root);
  try {
    return await runSemanticPass(root, filePaths, { project });
  } finally {
    project.stop();
  }
}

function edgeOnto(result: SemanticPassResult, qualifiedName: string) {
  const placeholder = result.upsertNodes.find((node) => node.qualifiedName === qualifiedName);
  assert.ok(
    placeholder,
    `expected a placeholder addressed ${qualifiedName}, got ${JSON.stringify(
      result.upsertNodes.map((n) => n.qualifiedName),
    )}`,
  );
  const edges = result.upsertEdges.filter((edge) => edge.toId === placeholder.id);
  assert.equal(edges.length, 1, `expected exactly one edge onto ${qualifiedName}`);
  return { placeholder, edge: edges[0] };
}

test("state is per-process, so each test starts from nothing", () => {
  resetSemanticPassState();
  resetIncrementalState();
});

// --- the case the pass exists for ----------------------------------------

test("`import * as ns` then `ns.someExport()` resolves to the declaration tree-sitter could not see", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "src/mod.ts": "export function someExport(): number {\n  return 1;\n}\n",
    "src/app.ts": `import * as ns from "./mod";

export function run(): number {
  return ns.someExport();
}
`,
  });
  try {
    const result = await pass(root);

    const { placeholder, edge } = edgeOnto(result, pendingSymbolQualifiedName("src/mod.ts", "someExport"));
    assert.equal(placeholder.nativeKind, PENDING_SYMBOL_NATIVE_KIND);
    assert.equal(placeholder.name, "someExport");
    // The placeholder belongs to the *importing* file: that is where the usage
    // is written, and it is what makes it go away with the file that needed it.
    assert.equal(placeholder.filePath, "src/app.ts");

    assert.equal(edge.kind, "CALLS");
    assert.equal(edge.source, "ts-compiler", "the checker answered this, not a name match");
    // Pointing at a placeholder is what unresolved means, whichever pass built
    // it: core repoints it and marks it resolved (`graph::symbol_links`).
    assert.equal(edge.resolved, false);
    assert.equal(result.unresolvedUses, 0);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

/**
 * The reason the address comes from the checker rather than from
 * `import * as ns from "./barrel"` plus the member name: the declaration is two
 * files away and renamed on the way, so nothing written at the use site names
 * it. A name-matching pass would address `barrel.ts#someExport` and be wrong
 * about which file, or right only by way of core's own re-export walk.
 */
test("the address comes from the checker, so an aliased re-export lands on the real declaration", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "src/impl.ts": "export function realName(): number {\n  return 1;\n}\n",
    "src/barrel.ts": 'export { realName as someExport } from "./impl";\n',
    "src/app.ts": `import * as ns from "./barrel";

export function run(): number {
  return ns.someExport();
}
`,
  });
  try {
    const result = await pass(root);

    const { edge } = edgeOnto(result, pendingSymbolQualifiedName("src/impl.ts", "realName"));
    assert.equal(edge.kind, "CALLS");
    assert.equal(edge.source, "ts-compiler");
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a namespace member read outside a call becomes a REFERENCES edge", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "src/mod.ts": "export const setting = 3;\n",
    "src/app.ts": `import * as ns from "./mod";

export const answer = ns.setting;
`,
  });
  try {
    const result = await pass(root);

    const { edge } = edgeOnto(result, pendingSymbolQualifiedName("src/mod.ts", "setting"));
    assert.equal(edge.kind, "REFERENCES");
    assert.equal(edge.source, "ts-compiler");
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

// --- what it refuses to answer -------------------------------------------

test("a member the module does not export is left unresolved, not guessed at", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "src/mod.ts": "export function someExport(): number {\n  return 1;\n}\n",
    "src/app.ts": `import * as ns from "./mod";

export function run(): unknown {
  return (ns as never as { missing(): number }).missing();
}
`,
  });
  try {
    const result = await pass(root);
    // The cast makes `missing` a property of an anonymous type declared right
    // here - no export of `./mod` at all - so nothing is addressed at it.
    assert.deepEqual(
      result.upsertNodes.map((n) => n.qualifiedName),
      [],
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a namespace import of a package is never asked about, so no child is started", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "src/app.ts": `import * as p from "node:path";

export function run(): string {
  return p.join("a", "b");
}
`,
  });
  const project = new SemanticProject(root);
  try {
    const result = await runSemanticPass(root, [], { project });
    assert.deepEqual(result.upsertEdges, []);
    // The gate that keeps an ordinary project from paying for this layer: the
    // specifier resolved to nothing in this project, so there was never a
    // question worth a ~265MB child.
    assert.equal(project.isRunning, false, "nothing to ask must mean nothing spawned");
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a project with no namespace import at all costs no tsserver child", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "src/mod.ts": "export function someExport(): number {\n  return 1;\n}\n",
    "src/app.ts": `import { someExport } from "./mod";

/** A doc comment, so the file is full of asterisks. */
export function run(): number {
  return someExport() * 2;
}
`,
  });
  const project = new SemanticProject(root);
  try {
    const result = await runSemanticPass(root, [], { project });
    assert.deepEqual(result.upsertEdges, []);
    assert.equal(project.isRunning, false);
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

/**
 * A file importing the same symbol both ways addresses one placeholder from
 * two directions - the address is all a node id is derived from. The pass must
 * reuse the structural record rather than build a second one that differs only
 * in where it says it is, which would make every pass rewrite that row.
 */
test("a symbol imported both by name and through a namespace shares one placeholder", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "src/mod.ts": "export function someExport(): number {\n  return 1;\n}\n",
    "src/app.ts": `import * as ns from "./mod";
import { someExport } from "./mod";

export function direct(): number {
  return someExport();
}

export function viaNamespace(): number {
  return ns.someExport();
}
`,
  });
  try {
    const result = await pass(root);

    const address = pendingSymbolQualifiedName("src/mod.ts", "someExport");
    const placeholders = result.upsertNodes.filter((node) => node.qualifiedName === address);
    assert.equal(placeholders.length, 1, "one address is one node");
    // The structural pass anchors it at the *named* import's binding, which is
    // the record that must survive: line 2, not the namespace import on line 1.
    assert.equal(placeholders[0].startLine, 1);

    const edges = result.upsertEdges.filter((edge) => edge.toId === placeholders[0].id);
    assert.equal(edges.length, 1, "only the namespace use is this pass's to write");
    assert.equal(edges[0].source, "ts-compiler");
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

/**
 * The pass is an upgrade over a graph that is already committed and
 * serviceable, so a checker that cannot start has to cost exactly the answers
 * it would have added - never the pass, and never the reparse behind it.
 */
test("a checker that cannot start leaves the pass empty instead of failing it", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "src/mod.ts": "export function someExport(): number {\n  return 1;\n}\n",
    "src/app.ts": `import * as ns from "./mod";

export function run(): number {
  return ns.someExport();
}
`,
  });
  const project = new SemanticProject(root, { tsserverPath: path.join(root, "no-such-tsserver.js") });
  const logged: string[] = [];
  try {
    const result = await runSemanticPass(root, [], { project, onLog: (m) => logged.push(m) });

    assert.deepEqual(result.upsertEdges, []);
    assert.deepEqual(result.upsertNodes, []);
    assert.equal(result.unresolvedUses, 1, "the site is counted, not silently dropped");
    assert.ok(
      logged.some((message) => message.includes("ns.someExport")),
      `the failure must be reported: ${JSON.stringify(logged)}`,
    );
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

// --- staying honest across edits -----------------------------------------

/**
 * The edge this pass writes is one nothing else can retract: the structural
 * reparse diff never knew about it, so it cannot list it as removed. Deleting
 * the call has to take the edge with it, or `find_callers` keeps answering with
 * a call site that is no longer in the file.
 */
test("deleting the call retracts the edge the previous pass wrote", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "src/mod.ts": "export function someExport(): number {\n  return 1;\n}\n",
    "src/app.ts": `import * as ns from "./mod";

export function run(): number {
  return ns.someExport();
}
`,
  });
  try {
    const before = await pass(root, ["src/app.ts"]);
    assert.equal(before.upsertEdges.length, 1);
    assert.deepEqual(before.deleteEdgeIds, []);

    await fs.writeFile(
      path.join(root, "src", "app.ts"),
      "export function run(): number {\n  return 1;\n}\n",
      "utf8",
    );
    const after = await pass(root, ["src/app.ts"]);

    assert.deepEqual(after.upsertEdges, []);
    assert.deepEqual(after.deleteEdgeIds, [before.upsertEdges[0].id]);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("running the same pass twice is idempotent", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": TSCONFIG,
    "src/mod.ts": "export function someExport(): number {\n  return 1;\n}\n",
    "src/app.ts": `import * as ns from "./mod";

export function run(): number {
  return ns.someExport();
}
`,
  });
  try {
    const first = await pass(root);
    const second = await pass(root);

    assert.deepEqual(second.upsertEdges, first.upsertEdges);
    assert.deepEqual(second.upsertNodes, first.upsertNodes);
    assert.deepEqual(second.deleteEdgeIds, [], "a second pass retracts nothing it just re-sent");
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});
