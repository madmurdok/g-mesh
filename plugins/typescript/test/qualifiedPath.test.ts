import { test } from "node:test";
import assert from "node:assert/strict";
import { toWireNode } from "../src/bulkIndex";
import { reparseFile, resetIncrementalState } from "../src/incremental";
import {
  extractFile,
  joinPath,
  type ExtractResult,
  type ExtractedNode,
  type PathSegment,
} from "../src/extract";

const SOURCE = `
import { helper } from "./helper";
export * from "./other";

export class C {
  m(): void { helper(); }
  static m(): void {}
  #priv(): void {}
  static #spriv(): void {}
  arrow = () => 1;
  static sarrow = () => 2;
}

export namespace N {
  export class C {
    m(): void {}
    static m(): void {}
    #priv(): void {}
  }
  export namespace Inner {
    export function deep(): void {}
  }
}

namespace Dotted.Name {
  export const v = 1;
}

declare module "*.scss" {
  const classes: Record<string, string>;
}

export interface I { x: number }
export type T = string;
export enum E { A = "a" }
export const f = () => 0;
export let counter = 0;
export default function () {}
`;

function byQName(result: ExtractResult, qualifiedName: string, kind?: string): ExtractedNode {
  const matches = result.nodes.filter(
    (n) => n.qualifiedName === qualifiedName && (kind === undefined || n.kind === kind),
  );
  assert.equal(matches.length, 1, `expected exactly one node ${qualifiedName}`);
  return matches[0];
}

/** The display text core stores for a path's suffix starting at `from`. */
function suffixFrom(path: readonly PathSegment[], from: number): string {
  return path[from].name + joinPath(path.slice(from + 1));
}

test("every declared node carries a qualifiedPath that joins back and ends in its name", () => {
  const result = extractFile("src/paths.ts", SOURCE);
  const declared = new Set(
    result.edges.filter((edge) => edge.kind === "DEFINES").map((edge) => edge.toId),
  );
  assert.ok(declared.size > 10, "the fixture declares symbols");

  for (const node of result.nodes) {
    if (!declared.has(node.id)) {
      assert.equal(node.qualifiedPath, undefined, `${node.qualifiedName} is not a declaration`);
      continue;
    }
    const path = node.qualifiedPath;
    assert.ok(path !== undefined && path.length > 0, `${node.qualifiedName} has a path`);
    assert.equal(joinPath(path), node.qualifiedName);
    assert.equal(path[path.length - 1].name, node.name);
    path.forEach((segment, index) => {
      assert.ok(segment.name.length > 0);
      if (index === 0) assert.equal(segment.sep, undefined);
      else assert.ok(segment.sep === "." || segment.sep === "#", `${node.qualifiedName}: sep`);
    });
  }
});

test("instance members are joined by `#`, static ones by `.`", () => {
  const result = extractFile("src/paths.ts", SOURCE);
  assert.deepEqual(byQName(result, "C#m").qualifiedPath, [{ name: "C" }, { sep: "#", name: "m" }]);
  assert.deepEqual(byQName(result, "C.m").qualifiedPath, [{ name: "C" }, { sep: ".", name: "m" }]);
  assert.deepEqual(byQName(result, "C#arrow").qualifiedPath, [
    { name: "C" },
    { sep: "#", name: "arrow" },
  ]);
  assert.deepEqual(byQName(result, "C.sarrow").qualifiedPath, [
    { name: "C" },
    { sep: ".", name: "sarrow" },
  ]);

  // Under a namespace, the two become distinct partial-path suffixes.
  const instance = byQName(result, "N.C#m").qualifiedPath!;
  const statik = byQName(result, "N.C.m").qualifiedPath!;
  assert.equal(suffixFrom(instance, 1), "C#m");
  assert.equal(suffixFrom(statik, 1), "C.m");
});

test("a #private member keeps its `#` in the name, not the separator", () => {
  const result = extractFile("src/paths.ts", SOURCE);
  assert.deepEqual(byQName(result, "C##priv").qualifiedPath, [
    { name: "C" },
    { sep: "#", name: "#priv" },
  ]);
  assert.deepEqual(byQName(result, "C.#spriv").qualifiedPath, [
    { name: "C" },
    { sep: ".", name: "#spriv" },
  ]);
  const nested = byQName(result, "N.C##priv").qualifiedPath!;
  assert.deepEqual(nested, [
    { name: "N" },
    { sep: ".", name: "C" },
    { sep: "#", name: "#priv" },
  ]);
  assert.equal(suffixFrom(nested, 1), "C##priv");
});

test("namespaces nest by `.`; a dotted or quoted module name is one segment", () => {
  const result = extractFile("src/paths.ts", SOURCE);
  assert.deepEqual(byQName(result, "N.Inner.deep").qualifiedPath, [
    { name: "N" },
    { sep: ".", name: "Inner" },
    { sep: ".", name: "deep" },
  ]);
  const dotted = byQName(result, "Dotted.Name", "Module");
  assert.deepEqual(dotted.qualifiedPath, [{ name: dotted.name }]);
  assert.deepEqual(byQName(result, "Dotted.Name.v").qualifiedPath, [
    { name: "Dotted.Name" },
    { sep: ".", name: "v" },
  ]);
  assert.deepEqual(byQName(result, "*.scss", "Module").qualifiedPath, [{ name: "*.scss" }]);
  assert.deepEqual(byQName(result, "default").qualifiedPath, [{ name: "default" }]);
});

test("the wire node carries qualifiedPath only when the node has one", () => {
  const result = extractFile("src/paths.ts", SOURCE);
  const method = toWireNode(byQName(result, "N.C#m"));
  assert.deepEqual(method.qualifiedPath, [
    { name: "N" },
    { sep: ".", name: "C" },
    { sep: "#", name: "m" },
  ]);
  const file = toWireNode(byQName(result, "src/paths.ts", "File"));
  assert.ok(!("qualifiedPath" in file));
  assert.ok(!JSON.stringify(file).includes("qualifiedPath"));
});

test("a reparse that changes only a node's path re-sends that node", () => {
  // `v` keeps its kind, qualifiedName `A.B.v`, range and every other field;
  // only its path changes, from `A.B`, `.v` to `A`, `.B`, `.v`.
  resetIncrementalState();
  try {
    reparseFile("src/ns.ts", "namespace A.B {\nexport const v = 1;\n}\n");
    const diff = reparseFile("src/ns.ts", "namespace A{namespace B{\nexport const v = 1;\n}}\n");
    const added = diff.addedNodes.find((n) => n.qualifiedName === "A.B.v");
    assert.ok(added !== undefined, "A.B.v is re-sent");
    assert.deepEqual(added.qualifiedPath, [
      { name: "A" },
      { sep: ".", name: "B" },
      { sep: ".", name: "v" },
    ]);
  } finally {
    resetIncrementalState();
  }
});
