import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import {
  extractFile,
  PENDING_SYMBOL_NATIVE_KIND,
  pendingSymbolQualifiedName,
  type ExtractedEdge,
} from "../src/extract";
import { resetIncrementalState } from "../src/incremental";
import { createProjectResolver } from "../src/resolve";
import { SemanticProject } from "../src/semantic";
import {
  resetSemanticPassState,
  runSemanticPass,
  type SemanticPassResult,
} from "../src/semanticPass";

// Like semantic.test.ts, these drive a **real** tsserver against real projects
// on disk. A stub would only prove this file's own assumptions about what the
// checker answers, and the entire value of the pass is that it does not have to
// assume: no name-matching layer can attribute `ns.member`, and none can say
// which of two `export *` branches a consumer really sees, so what makes a test
// here meaningful is that a compiler really did.
//
// Each test that reaches the checker pays one child startup (~1.2s), which is
// why there are few of them.

async function makeProject(files: Record<string, string>): Promise<string> {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-semantic-pass-"));
  for (const [rel, contents] of Object.entries(files)) {
    const abs = path.join(root, rel);
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, contents, "utf8");
  }
  return root;
}

/** For fixtures that put their sources under `src/`. */
const TSCONFIG = JSON.stringify({ compilerOptions: { strict: true }, include: ["src"] }) + "\n";

/** For the barrel fixtures, whose files sit at the project root. */
const ROOT_TSCONFIG = JSON.stringify({ compilerOptions: { strict: true } }) + "\n";

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

// --- question 1: a namespace member use has no edge to upgrade ------------

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

// --- question 2: an unresolved edge through a re-export chain -------------

/**
 * The scenario core's `two_reexport_branches_offering_one_name_leave_the_edge_unresolved`
 * documents: one barrel, two `export *` branches, both offering `mutate`.
 * `caller.ts` imports it through the barrel, so the address it writes down is
 * the barrel's and neither branch is more obviously right than the other.
 */
function ambiguousBarrel(order: readonly string[]): Record<string, string> {
  return {
    "tsconfig.json": ROOT_TSCONFIG,
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
  resetSemanticPassState();
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
    // An upgraded edge is the structural pass's own; nothing here retracts it.
    assert.deepEqual(diff.deleteEdgeIds, []);
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("swapping the two `export *` statements swaps the declaration the pass lands on", async () => {
  resetSemanticPassState();
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

test("a whole-project pass finds the same edge without being told which file to look at", async () => {
  resetSemanticPassState();
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

// --- both questions in one sweep ------------------------------------------

/**
 * The two halves are opposite shapes - one invents an edge and a placeholder,
 * the other rewrites an edge that already exists - and a file can perfectly
 * well pose both at once. One pass, one child, both answers, and the two must
 * not be confused for each other: the invented edge stays `resolved: false` on
 * a placeholder for core to link, the rewritten one arrives already resolved
 * onto a real declaration.
 */
test("one pass answers a namespace use and an ambiguous re-export together", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    ...ambiguousBarrel(["a", "b"]),
    "caller.ts": `import { mutate } from "./index";
import * as ns from "./b";

export function run(): string {
  mutate();
  return ns.mutate();
}
`,
  });
  const project = new SemanticProject(root);
  try {
    const diff = await runSemanticPass(root, ["caller.ts"], { project });

    assert.equal(diff.upsertEdges.length, 2, `expected both answers: ${JSON.stringify(diff.upsertEdges)}`);

    // Question 2: the barrel call, upgraded in place onto `a.ts`.
    const upgraded = diff.upsertEdges.filter((edge) => edge.resolved);
    assert.equal(upgraded.length, 1);
    assert.equal(upgraded[0].toId, await declarationId(root, "a.ts", "mutate"));
    assert.equal(upgraded[0].source, "ts-compiler");

    // Question 1: `ns.mutate()`, an edge nothing emitted before, addressed at
    // `b.ts` and left for core's placeholder walk to link.
    const { placeholder, edge } = edgeOnto(diff, pendingSymbolQualifiedName("b.ts", "mutate"));
    assert.equal(placeholder.nativeKind, PENDING_SYMBOL_NATIVE_KIND);
    assert.equal(placeholder.filePath, "caller.ts");
    assert.equal(edge.resolved, false);
    assert.equal(edge.source, "ts-compiler");
    // Only the invented edge is this pass's to retract later.
    assert.deepEqual(diff.deleteEdgeIds, []);
  } finally {
    project.stop();
    await fs.rm(root, { recursive: true, force: true });
  }
});

// --- what it refuses to answer, and what it refuses to ask ----------------

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

test("a barrel whose branches all end outside the index leaves the edge alone", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": ROOT_TSCONFIG,
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

test("a name the target file declares itself is left to the structural layer", async () => {
  resetSemanticPassState();
  const root = await makeProject({
    "tsconfig.json": ROOT_TSCONFIG,
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

/**
 * The same gate seen from the other half: a project with no namespace member
 * use *and* nothing unresolved that a barrel could explain has no question to
 * ask at all, so it pays nothing beyond the parse both halves already need.
 */
test("a project with neither a namespace import nor a barrel costs no tsserver child", async () => {
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

// --- a checker that will not answer ---------------------------------------

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

/** The other failure shape, on the other half: a child that starts and dies,
 * which is what a checker OOM looks like from here. */
test("a checker that dies costs the pass its answers, not the plugin", async () => {
  resetSemanticPassState();
  const root = await makeProject(ambiguousBarrel(["a", "b"]));
  // A path that exists but is not a tsserver.
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

// --- staying honest across edits -----------------------------------------

/**
 * The edge the namespace half writes is one nothing else can retract: the
 * structural reparse diff never knew about it, so it cannot list it as removed.
 * Deleting the call has to take the edge with it, or `find_callers` keeps
 * answering with a call site that is no longer in the file.
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
