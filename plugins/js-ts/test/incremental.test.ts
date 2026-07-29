import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import {
  computeSourceEdit,
  forgetFile,
  hasCachedFile,
  isEmptyDiff,
  reparseChangedFile,
  reparseFile,
  resetIncrementalState,
  type FileDiff,
} from "../src/incremental";
import {
  extractFile,
  UnsupportedFileError,
  type ExtractedEdge,
  type ExtractedNode,
} from "../src/extract";

const FILE = "src/math.ts";

/**
 * The fixture is shaped so each acceptance case can be edited in isolation:
 *
 *  - `add` / `sub` are two interchangeable call targets, so swapping one for
 *    the other inside a body changes exactly one CALLS edge in each direction.
 *  - `combine`'s body edit keeps the line length, so nothing's range moves and
 *    the diff can be asserted to contain no nodes at all.
 *  - `inline`'s body lives on the declaration's own line, so widening it moves
 *    that function's end column - and only its own, since no later line
 *    shifts and the file's line count is unchanged.
 *  - `untouched` is the control: it must never show up in any of these diffs.
 *  - the file ends in a `//` comment on its last line, which is where the
 *    provably range-neutral whitespace edit goes (see that test).
 */
const BASE = `import { format } from "./format";

/** Adds two numbers. */
export function add(a: number, b: number): number {
  return a + b;
}

/** Subtracts two numbers. */
export function sub(a: number, b: number): number {
  return a - b;
}

/** Combines two numbers. */
export function combine(a: number, b: number): number {
  const total = add(a, b);
  return total;
}

/** One-liner whose body is edited in place. */
export function inline(a: number): number { return add(a, 1); }

/** Never touched by any edit below. */
export function untouched(label: string): string {
  return format(label);
}

// trailing note
`;

/** `String.replace` that fails loudly when the needle is not there - a silent
 * no-op edit would turn any of these tests green for the wrong reason. */
function edited(source: string, from: string, to: string): string {
  assert.ok(source.includes(from), `fixture edit target not found: ${from}`);
  return source.replace(from, to);
}

function nodeLabels(nodes: readonly ExtractedNode[]): string[] {
  return nodes.map((n) => `${n.kind}:${n.qualifiedName}`).sort();
}

/** Renders edges as `KIND from -> to` using qualified names, resolving ids
 * against every source version involved (a removed edge's endpoints only
 * exist in the old text, an added edge's only in the new). */
function edgeLabels(edges: readonly ExtractedEdge[], ...versions: string[]): string[] {
  const names = new Map<string, string>();
  for (const version of versions) {
    for (const node of extractFile(FILE, version).nodes) names.set(node.id, node.qualifiedName);
  }
  return edges
    .map((e) => `${e.kind} ${names.get(e.fromId) ?? e.fromId} -> ${names.get(e.toId) ?? e.toId}`)
    .sort();
}

/** Every qualified name mentioned anywhere in the diff, node side. */
function touchedNames(diff: FileDiff): Set<string> {
  return new Set([...diff.addedNodes, ...diff.removedNodes].map((n) => n.qualifiedName));
}

/** Seeds the cache with `BASE` from a clean slate and returns the diff of
 * applying `next` to it. */
function reparseFromBase(next: string): FileDiff {
  resetIncrementalState();
  reparseFile(FILE, BASE);
  return reparseFile(FILE, next);
}

// --- cache miss ----------------------------------------------------------

test("a first sighting of a file reports the whole extraction as added", () => {
  resetIncrementalState();
  assert.equal(hasCachedFile(FILE), false);

  const diff = reparseFile(FILE, BASE);

  assert.equal(diff.fullExtraction, true);
  assert.equal(diff.filePath, FILE);
  assert.equal(diff.removedNodes.length, 0);
  assert.equal(diff.removedEdges.length, 0);
  assert.deepEqual(
    nodeLabels(diff.addedNodes),
    [
      "File:src/math.ts",
      "Function:add",
      "Function:combine",
      "Function:inline",
      "Function:sub",
      "Function:untouched",
      "Module:./format",
    ],
  );
  assert.ok(hasCachedFile(FILE));

  // A cache miss must be indistinguishable from a from-scratch extraction:
  // the caller never has to bulk-index first to get a correct answer.
  const full = extractFile(FILE, BASE);
  assert.deepEqual(diff.addedNodes, full.nodes);
  assert.deepEqual(diff.addedEdges, full.edges);
  assert.equal(diff.hasSyntaxErrors, full.hasSyntaxErrors);
});

test("forgetting a file makes the next reparse a full extraction again", () => {
  resetIncrementalState();
  reparseFile(FILE, BASE);
  assert.equal(forgetFile(FILE), true);
  assert.equal(hasCachedFile(FILE), false);

  const diff = reparseFile(FILE, BASE);
  assert.equal(diff.fullExtraction, true);
  assert.equal(diff.addedNodes.length, extractFile(FILE, BASE).nodes.length);
});

test("an unsupported extension is rejected rather than silently cached", () => {
  resetIncrementalState();
  assert.throws(() => reparseFile("README.md", "# hello\n"), UnsupportedFileError);
  assert.equal(hasCachedFile("README.md"), false);
});

// --- editing one function's body ------------------------------------------

test("swapping a callee inside one body changes only that function's edges", () => {
  const next = edited(BASE, "  const total = add(a, b);", "  const total = sub(a, b);");
  const diff = reparseFromBase(next);

  assert.equal(diff.fullExtraction, false);

  // The edit replaces three characters on one line: no line count change, no
  // column change past the edit, so not one node in the file - including
  // `combine` itself - has different content.
  assert.deepEqual(nodeLabels(diff.addedNodes), []);
  assert.deepEqual(nodeLabels(diff.removedNodes), []);

  assert.deepEqual(edgeLabels(diff.removedEdges, BASE, next), ["CALLS combine -> add"]);
  assert.deepEqual(edgeLabels(diff.addedEdges, BASE, next), ["CALLS combine -> sub"]);
});

test("widening one function's body emits only that function, as remove-old + add-new", () => {
  const next = edited(
    BASE,
    "export function inline(a: number): number { return add(a, 1); }",
    "export function inline(a: number): number { return sub(a, 1) + 100; }",
  );
  const diff = reparseFromBase(next);

  // `inline` keeps its id (ids come from qualifiedName, not position), so an
  // id-only set difference would have reported nothing here. The structural
  // compare catches the moved end column and encodes the change as a pair.
  assert.deepEqual(nodeLabels(diff.removedNodes), ["Function:inline"]);
  assert.deepEqual(nodeLabels(diff.addedNodes), ["Function:inline"]);
  assert.equal(diff.removedNodes[0].id, diff.addedNodes[0].id);
  assert.notEqual(diff.removedNodes[0].endCol, diff.addedNodes[0].endCol);
  // The signature is untouched - this is a body edit, not a rename.
  assert.equal(diff.removedNodes[0].signature, diff.addedNodes[0].signature);

  // No other symbol in the file is re-emitted, in particular not the File
  // node and not the four sibling functions.
  assert.deepEqual([...touchedNames(diff)], ["inline"]);

  assert.deepEqual(edgeLabels(diff.removedEdges, BASE, next), ["CALLS inline -> add"]);
  assert.deepEqual(edgeLabels(diff.addedEdges, BASE, next), ["CALLS inline -> sub"]);

  // DEFINES/EXPORTS for `inline` are unchanged (same endpoints, same kind),
  // so they must not be churned even though the node itself was re-emitted.
  const stable = ["DEFINES", "EXPORTS"];
  for (const edge of [...diff.addedEdges, ...diff.removedEdges]) {
    assert.ok(!stable.includes(edge.kind), `${edge.kind} edge should not have changed`);
  }
});

// --- no-op edits -----------------------------------------------------------

test("a whitespace-only edit that shifts nothing produces an empty diff", () => {
  // The edit adds one space *inside* the `//` comment on the file's last
  // line. That comment is not a symbol and not a doc comment (only `/** */`
  // blocks are), nothing follows it, and the program node's end position is
  // row/column of end-of-file - which the extra column on a line that is
  // already the last one does not move. So every node's content is provably
  // byte-identical and the empty diff is unambiguous.
  //
  // Contrast the next test: whitespace appended *after* the final newline is
  // equally "no-op" to a reader but does move the File node's end, and is
  // correctly reported.
  const next = edited(BASE, "// trailing note", "//  trailing note");
  const diff = reparseFromBase(next);

  assert.ok(isEmptyDiff(diff), `expected an empty diff, got ${JSON.stringify(diff)}`);
  assert.deepEqual(diff.addedNodes, []);
  assert.deepEqual(diff.removedNodes, []);
  assert.deepEqual(diff.addedEdges, []);
  assert.deepEqual(diff.removedEdges, []);
});

test("whitespace appended past the last line moves only the File node's range", () => {
  const diff = reparseFromBase(BASE + "  ");

  // Documents the boundary of the rule above: the File node spans the whole
  // text, so its range genuinely changed and reporting it is correct, not a
  // leak. No symbol is touched.
  assert.deepEqual(nodeLabels(diff.addedNodes), ["File:src/math.ts"]);
  assert.deepEqual(nodeLabels(diff.removedNodes), ["File:src/math.ts"]);
  assert.deepEqual(diff.addedEdges, []);
  assert.deepEqual(diff.removedEdges, []);
});

test("a notification for text identical to the cached copy is an empty diff", () => {
  resetIncrementalState();
  reparseFile(FILE, BASE);
  const diff = reparseFile(FILE, BASE);

  assert.equal(diff.fullExtraction, false);
  assert.ok(isEmptyDiff(diff));
});

// --- structural edits ------------------------------------------------------

const WITH_FRESH = edited(
  BASE,
  "// trailing note",
  `/** Brand new. */
export function fresh(n: number): number {
  return add(n, n);
}

// trailing note`,
);

test("adding a function reports it as purely added", () => {
  const diff = reparseFromBase(WITH_FRESH);

  assert.ok(nodeLabels(diff.addedNodes).includes("Function:fresh"));
  assert.equal(
    diff.removedNodes.some((n) => n.qualifiedName === "fresh"),
    false,
    "a brand new symbol must have no removal counterpart",
  );

  // Only the new function and the File node (whose range now covers more
  // lines) are touched; the insertion sits after every other declaration, so
  // none of them shifted.
  assert.deepEqual(nodeLabels(diff.addedNodes), ["File:src/math.ts", "Function:fresh"]);
  assert.deepEqual(nodeLabels(diff.removedNodes), ["File:src/math.ts"]);

  assert.deepEqual(edgeLabels(diff.addedEdges, BASE, WITH_FRESH), [
    "CALLS fresh -> add",
    "DEFINES src/math.ts -> fresh",
    "EXPORTS src/math.ts -> fresh",
  ]);
  assert.deepEqual(diff.removedEdges, []);
});

test("deleting a function reports it as purely removed", () => {
  resetIncrementalState();
  reparseFile(FILE, WITH_FRESH);
  const diff = reparseFile(FILE, BASE);

  assert.ok(nodeLabels(diff.removedNodes).includes("Function:fresh"));
  assert.equal(
    diff.addedNodes.some((n) => n.qualifiedName === "fresh"),
    false,
    "a deleted symbol must have no addition counterpart",
  );
  assert.deepEqual(nodeLabels(diff.removedNodes), ["File:src/math.ts", "Function:fresh"]);
  assert.deepEqual(nodeLabels(diff.addedNodes), ["File:src/math.ts"]);

  assert.deepEqual(edgeLabels(diff.removedEdges, BASE, WITH_FRESH), [
    "CALLS fresh -> add",
    "DEFINES src/math.ts -> fresh",
    "EXPORTS src/math.ts -> fresh",
  ]);
  assert.deepEqual(diff.addedEdges, []);
});

test("renaming a function is a removal of the old id plus an addition of the new", () => {
  // The one case where the id itself changes, since qualifiedName feeds it.
  const next = edited(BASE, "export function sub(", "export function minus(");
  const diff = reparseFromBase(next);

  const removed = diff.removedNodes.filter((n) => n.kind === "Function");
  const added = diff.addedNodes.filter((n) => n.kind === "Function");
  assert.deepEqual(removed.map((n) => n.qualifiedName), ["sub"]);
  assert.deepEqual(added.map((n) => n.qualifiedName), ["minus"]);
  assert.notEqual(removed[0].id, added[0].id);
});

// --- incremental output must equal a from-scratch parse --------------------

test("an incremental reparse yields exactly what a full parse of the new text would", () => {
  // The whole point of the diff is that applying it to the previous state
  // reproduces the full extraction; if the incremental tree ever diverged
  // from a fresh one, this is where it would show.
  resetIncrementalState();
  let state = extractFile(FILE, BASE);
  reparseFile(FILE, BASE);

  const revisions = [
    edited(BASE, "  const total = add(a, b);", "  const total = sub(a, b);"),
    WITH_FRESH,
    edited(WITH_FRESH, "  return format(label);", "  return format(label).trim();"),
    BASE + "  ",
    BASE,
  ];

  for (const revision of revisions) {
    const diff = reparseFile(FILE, revision);

    const removedNodes = new Set(diff.removedNodes.map((n) => n.id));
    const removedEdges = new Set(diff.removedEdges.map((e) => e.id));
    const nodes = [
      ...state.nodes.filter((n) => !removedNodes.has(n.id)),
      ...diff.addedNodes,
    ];
    const edges = [
      ...state.edges.filter((e) => !removedEdges.has(e.id)),
      ...diff.addedEdges,
    ];

    const expected = extractFile(FILE, revision);
    assert.deepEqual(
      new Set(nodes.map((n) => JSON.stringify(n))),
      new Set(expected.nodes.map((n) => JSON.stringify(n))),
    );
    assert.deepEqual(
      new Set(edges.map((e) => JSON.stringify(e))),
      new Set(expected.edges.map((e) => JSON.stringify(e))),
    );
    state = { nodes, edges, hasSyntaxErrors: expected.hasSyntaxErrors };
  }
});

test("a file edited into a syntax error keeps reparsing and flags it", () => {
  const broken = edited(BASE, "  return a + b;\n}", "  return a + b;");
  const diff = reparseFromBase(broken);

  assert.equal(diff.hasSyntaxErrors, true);
  assert.ok(!isEmptyDiff(diff), "a broken edit must still produce a diff");
});

// --- reading from disk -----------------------------------------------------

test("reparseChangedFile reads the project-relative path and keys state by it", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-incr-"));
  try {
    resetIncrementalState();
    const abs = path.join(root, FILE);
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, BASE, "utf8");

    const first = await reparseChangedFile(root, FILE);
    assert.equal(first.fullExtraction, true);
    // Ids must be built from the project-relative path, not the tempdir one.
    assert.ok(first.addedNodes.some((n) => n.kind === "File" && n.qualifiedName === FILE));

    await fs.writeFile(abs, edited(BASE, "  const total = add(a, b);", "  const total = sub(a, b);"), "utf8");
    const second = await reparseChangedFile(root, FILE);
    assert.equal(second.fullExtraction, false);
    assert.equal(second.addedEdges.length, 1);
    assert.equal(second.removedEdges.length, 1);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("reparseChangedFile resolves relative imports against the project on disk", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-incr-"));
  try {
    resetIncrementalState();
    const write = async (rel: string, text: string): Promise<void> => {
      const abs = path.join(root, rel);
      await fs.mkdir(path.dirname(abs), { recursive: true });
      await fs.writeFile(abs, text, "utf8");
    };
    await write("src/index.ts", `import { format } from "./format.js";\nexport const x = format;\n`);

    // The target does not exist yet, so the import is honestly unresolved.
    const before = await reparseChangedFile(root, "src/index.ts");
    const dangling = before.addedNodes.find((n) => n.kind === "Module");
    assert.equal(dangling?.qualifiedName, "./format.js");
    assert.equal(dangling?.nativeKind, "external_module");

    // Creating the target does not by itself re-resolve the importer - the
    // plugin only ever re-answers about a file it is asked to reparse (core
    // closes this gap from its side, see graph::imports::link_diff).
    await write("src/format.ts", `export const format = (x: unknown) => String(x);\n`);
    assert.ok(isEmptyDiff(await reparseChangedFile(root, "src/index.ts")), "unchanged text, unchanged diff");

    // Editing the importer re-runs resolution, which now finds it.
    await write("src/index.ts", `import { format } from "./format.js";\nexport const y = format;\n`);
    const after = await reparseChangedFile(root, "src/index.ts");
    const resolved = after.addedNodes.find((n) => n.kind === "Module");
    assert.equal(resolved?.qualifiedName, "src/format.ts");
    assert.equal(resolved?.nativeKind, "resolved_module");
    assert.ok(
      after.removedNodes.some((n) => n.qualifiedName === "./format.js"),
      "the old placeholder must be reported removed, or core would keep both",
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

// --- the edit description itself -------------------------------------------

test("computeSourceEdit returns null for identical text", () => {
  assert.equal(computeSourceEdit("const a = 1;\n", "const a = 1;\n"), null);
  assert.equal(computeSourceEdit("", ""), null);
});

test("computeSourceEdit spans exactly the replaced region, on both strings", () => {
  const oldText = "const a = 1;\nconst b = 2;\nconst c = 3;\n";
  const newText = "const a = 1;\nconst b = 22222;\nconst c = 3;\n";
  const edit = computeSourceEdit(oldText, newText)!;

  assert.ok(edit !== null);
  assert.equal(oldText.slice(0, edit.startIndex), newText.slice(0, edit.startIndex));
  assert.equal(oldText.slice(edit.oldEndIndex), newText.slice(edit.newEndIndex));
  assert.equal(oldText.slice(edit.startIndex, edit.oldEndIndex), "");
  assert.equal(newText.slice(edit.startIndex, edit.newEndIndex), "2222");

  // Row/column for the two end boundaries come from different strings; here
  // they happen to agree in row and differ in column.
  assert.deepEqual(edit.startPosition, { row: 1, column: 11 });
  assert.deepEqual(edit.oldEndPosition, { row: 1, column: 11 });
  assert.deepEqual(edit.newEndPosition, { row: 1, column: 15 });
});

test("computeSourceEdit handles a multi-line insertion", () => {
  const oldText = "a\nb\n";
  const newText = "a\nX\nY\nb\n";
  const edit = computeSourceEdit(oldText, newText)!;

  assert.equal(oldText.slice(0, edit.startIndex), newText.slice(0, edit.startIndex));
  assert.equal(oldText.slice(edit.oldEndIndex), newText.slice(edit.newEndIndex));
  assert.deepEqual(edit.startPosition, { row: 1, column: 0 });
  assert.deepEqual(edit.oldEndPosition, { row: 1, column: 0 });
  assert.deepEqual(edit.newEndPosition, { row: 3, column: 0 });
});

test("computeSourceEdit keeps prefix and suffix from overlapping", () => {
  // "aa" -> "aaa": the same characters match from both ends, so an uncapped
  // suffix scan would run past the prefix and produce a negative-length span.
  for (const [oldText, newText] of [
    ["aa", "aaa"],
    ["aaa", "aa"],
    ["", "abc"],
    ["abc", ""],
    ["\n\n\n", "\n\n"],
  ]) {
    const edit = computeSourceEdit(oldText, newText)!;
    assert.ok(edit !== null, `${JSON.stringify(oldText)} -> ${JSON.stringify(newText)}`);
    assert.ok(edit.startIndex <= edit.oldEndIndex, "old span must be non-negative");
    assert.ok(edit.startIndex <= edit.newEndIndex, "new span must be non-negative");
    assert.ok(edit.oldEndIndex <= oldText.length);
    assert.ok(edit.newEndIndex <= newText.length);
    // Splicing the new span into the old text at the edit must rebuild newText.
    const rebuilt =
      oldText.slice(0, edit.startIndex) +
      newText.slice(edit.startIndex, edit.newEndIndex) +
      oldText.slice(edit.oldEndIndex);
    assert.equal(rebuilt, newText);
  }
});

test("computeSourceEdit does not cut a surrogate pair in half", () => {
  // The emoji is two UTF-16 code units; an edit right after it must not land
  // between them, and neither must the suffix boundary of an edit before it.
  const oldText = "const s = \u{1F600}x;\n";
  const newText = "const s = \u{1F600}y;\n";
  const edit = computeSourceEdit(oldText, newText)!;

  assert.ok(!isLoneSurrogateBoundary(oldText, edit.startIndex));
  assert.ok(!isLoneSurrogateBoundary(oldText, edit.oldEndIndex));
  assert.ok(!isLoneSurrogateBoundary(newText, edit.newEndIndex));

  const dropped = computeSourceEdit(oldText, "const s = x;\n")!;
  assert.ok(!isLoneSurrogateBoundary(oldText, dropped.startIndex));
  assert.ok(!isLoneSurrogateBoundary(oldText, dropped.oldEndIndex));
});

/** True when `index` falls between a high and a low surrogate of one pair. */
function isLoneSurrogateBoundary(text: string, index: number): boolean {
  if (index <= 0 || index >= text.length) return false;
  const before = text.charCodeAt(index - 1);
  const after = text.charCodeAt(index);
  return before >= 0xd800 && before <= 0xdbff && after >= 0xdc00 && after <= 0xdfff;
}
