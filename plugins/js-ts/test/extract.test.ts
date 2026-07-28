import { test } from "node:test";
import assert from "node:assert/strict";
import {
  extractFile,
  isSupportedFile,
  nodeIdFor,
  UnsupportedFileError,
  type EdgeKind,
  type ExtractResult,
  type ExtractedNode,
  type NodeKind,
} from "../src/extract";

function node(result: ExtractResult, kind: NodeKind, qualifiedName: string): ExtractedNode {
  const matches = result.nodes.filter((n) => n.kind === kind && n.qualifiedName === qualifiedName);
  assert.equal(matches.length, 1, `expected exactly one ${kind} ${qualifiedName}`);
  return matches[0];
}

function hasEdge(
  result: ExtractResult,
  kind: EdgeKind,
  fromQualifiedName: string,
  toQualifiedName: string,
): boolean {
  const byId = new Map(result.nodes.map((n) => [n.id, n]));
  return result.edges.some(
    (edge) =>
      edge.kind === kind &&
      byId.get(edge.fromId)?.qualifiedName === fromQualifiedName &&
      byId.get(edge.toId)?.qualifiedName === toQualifiedName,
  );
}

// Covers the ticket's acceptance criteria in one file: a class implementing
// an interface, a function calling another function, and an import.
const GREETER_TS = `import { translate } from "./i18n";

/**
 * Anything that can greet.
 */
export interface Greeter {
  greet(name: string): string;
}

export class LoudGreeter implements Greeter {
  greet(name: string): string {
    return this.shout(name);
  }

  private shout(text: string): string {
    return text.toUpperCase();
  }
}

export function greetAll(names: string[]): string[] {
  return names.map((name) => formatName(name));
}

function formatName(name: string): string {
  return translate(name);
}

export const DEFAULT_GREETING = "hello";
`;

test("extracts the documented node kinds from a TypeScript file", () => {
  const result = extractFile("src/greeter.ts", GREETER_TS);

  const file = node(result, "File", "src/greeter.ts");
  assert.equal(file.name, "greeter.ts");
  assert.equal(file.language, "typescript");

  assert.equal(node(result, "Type", "Greeter").nativeKind, "interface");
  assert.equal(node(result, "Type", "LoudGreeter").nativeKind, "class");
  assert.equal(node(result, "Function", "LoudGreeter#greet").nativeKind, "method");
  assert.equal(node(result, "Function", "greetAll").nativeKind, "function");
  assert.equal(node(result, "Variable", "DEFAULT_GREETING").nativeKind, "const");

  // Local `name` bindings inside the function bodies are deliberately not nodes.
  assert.deepEqual(
    result.nodes.filter((n) => n.name === "name"),
    [],
  );

  assert.equal(node(result, "Function", "greetAll").signature, "greetAll(names: string[]): string[]");
  assert.equal(node(result, "Type", "Greeter").docComment, "Anything that can greet.");
  assert.equal(node(result, "Type", "Greeter").exported, true);
  assert.equal(node(result, "Function", "formatName").exported, false);
});

test("extracts SUPERTYPE_OF, CALLS and IMPORTS edges", () => {
  const result = extractFile("src/greeter.ts", GREETER_TS);

  // Subtype -> supertype, so find_implementations is an inbound lookup.
  assert.ok(hasEdge(result, "SUPERTYPE_OF", "LoudGreeter", "Greeter"));
  assert.ok(hasEdge(result, "CALLS", "greetAll", "formatName"));
  assert.ok(hasEdge(result, "CALLS", "LoudGreeter#greet", "LoudGreeter#shout"));
  assert.ok(hasEdge(result, "IMPORTS", "src/greeter.ts", "./i18n"));

  assert.equal(node(result, "Module", "./i18n").nativeKind, "external_module");

  // `translate` comes from another module; without a resolver there is no
  // in-file target to point a CALLS edge at.
  assert.equal(
    result.edges.filter((e) => e.kind === "CALLS").length,
    2,
    "only within-file calls resolve",
  );
});

test("every edge is unresolved and attributed to tree-sitter", () => {
  const result = extractFile("src/greeter.ts", GREETER_TS);
  assert.ok(result.edges.length > 0);
  for (const edge of result.edges) {
    assert.equal(edge.source, "tree-sitter");
    assert.equal(edge.resolved, false);
  }
});

test("File node DEFINES every symbol and EXPORTS only the exported ones", () => {
  const result = extractFile("src/greeter.ts", GREETER_TS);
  const filePath = "src/greeter.ts";

  for (const qualifiedName of [
    "Greeter",
    "LoudGreeter",
    "LoudGreeter#greet",
    "LoudGreeter#shout",
    "greetAll",
    "formatName",
    "DEFAULT_GREETING",
  ]) {
    assert.ok(hasEdge(result, "DEFINES", filePath, qualifiedName), `DEFINES ${qualifiedName}`);
  }

  assert.ok(hasEdge(result, "EXPORTS", filePath, "greetAll"));
  assert.ok(hasEdge(result, "EXPORTS", filePath, "DEFAULT_GREETING"));
  assert.ok(!hasEdge(result, "EXPORTS", filePath, "formatName"));
  // The import placeholder is a target, never something this file defines.
  assert.ok(!hasEdge(result, "DEFINES", filePath, "./i18n"));
});

test("module-level Variables and namespace Modules are extracted, locals are not", () => {
  const result = extractFile(
    "src/config.ts",
    `export namespace Config {
  export const retries = 3;
  export function reset(): void {}
}

export let attempts = 0;
const secret = "s";

export function run(): void {
  const cached = attempts;
  Config.reset();
}
`,
  );

  const namespaceNode = node(result, "Module", "Config");
  assert.equal(namespaceNode.nativeKind, "namespace");
  assert.equal(namespaceNode.exported, true);

  assert.equal(node(result, "Variable", "Config.retries").exported, true);
  assert.equal(node(result, "Variable", "attempts").nativeKind, "let");
  assert.equal(node(result, "Variable", "secret").exported, false);
  assert.deepEqual(
    result.nodes.filter((n) => n.name === "cached"),
    [],
    "function-local variables stay out of the graph",
  );

  assert.ok(hasEdge(result, "DEFINES", "src/config.ts", "Config"));
  assert.ok(hasEdge(result, "DEFINES", "src/config.ts", "Config.retries"));
  assert.ok(hasEdge(result, "EXPORTS", "src/config.ts", "attempts"));
  assert.ok(!hasEdge(result, "EXPORTS", "src/config.ts", "secret"));
  // Namespace-qualified call: the receiver names a Module declared here.
  assert.ok(hasEdge(result, "CALLS", "run", "Config.reset"));
  assert.ok(hasEdge(result, "REFERENCES", "run", "attempts"));
});

test("`export { name }` marks an already-declared symbol as exported", () => {
  const result = extractFile(
    "src/reexport.ts",
    `function boot(): void {}
export { boot as started };
export * from "./other";
`,
  );

  assert.equal(node(result, "Function", "boot").exported, true);
  assert.ok(hasEdge(result, "EXPORTS", "src/reexport.ts", "boot"));
  assert.ok(hasEdge(result, "IMPORTS", "src/reexport.ts", "./other"));
});

test("a getter and a setter sharing a name stay distinct nodes", () => {
  const result = extractFile(
    "src/accessors.ts",
    `export class Box {
  private inner = 0;
  get value(): number { return this.inner; }
  set value(next: number) { this.inner = next; }
  static of(): Box { return new Box(); }
}
`,
  );

  const accessors = result.nodes.filter((n) => n.qualifiedName === "Box#value");
  assert.deepEqual(
    accessors.map((n) => n.nativeKind).sort(),
    ["getter", "setter"],
    "nativeKind is part of the node id, so accessors do not collapse",
  );
  assert.equal(new Set(accessors.map((n) => n.id)).size, 2);
  // Static members use `.`, instance members `#`.
  assert.equal(node(result, "Function", "Box.of").nativeKind, "method");
});

test("flags syntax errors on a partially broken file without dropping what parsed", () => {
  const broken = `export function ok(): void {}

export class Broken {
  run(): void { const x = ; }
}
`;
  const result = extractFile("src/broken.ts", broken);

  assert.equal(result.hasSyntaxErrors, true);
  assert.ok(result.nodes.length > 1, "error-tolerant parsing still yields symbols");
  for (const extracted of result.nodes) {
    assert.equal(extracted.hasSyntaxErrors, true, `${extracted.qualifiedName} must be flagged`);
  }
  assert.equal(node(result, "Function", "ok").name, "ok");

  const clean = extractFile("src/clean.ts", `export function ok(): void {}\n`);
  assert.equal(clean.hasSyntaxErrors, false);
  assert.ok(clean.nodes.every((n) => !n.hasSyntaxErrors));
});

test("node ids survive edits elsewhere in the file", () => {
  const before = extractFile("src/greeter.ts", GREETER_TS);
  const after = extractFile(
    "src/greeter.ts",
    `// an unrelated comment that shifts every line below it\n${GREETER_TS}\nexport function added(): void {}\n`,
  );

  const idOf = (result: ExtractResult, qualifiedName: string): string =>
    node(result, "Function", qualifiedName).id;

  assert.equal(idOf(after, "greetAll"), idOf(before, "greetAll"));
  assert.equal(idOf(after, "LoudGreeter#shout"), idOf(before, "LoudGreeter#shout"));
  assert.notEqual(
    node(after, "Function", "greetAll").startLine,
    node(before, "Function", "greetAll").startLine,
    "the fixture really did move",
  );
  assert.equal(idOf(before, "greetAll"), nodeIdFor("src/greeter.ts", "Function", "greetAll", "function"));
});

test("handles .jsx/.js with the JavaScript grammar, including require()", () => {
  const result = extractFile(
    "src/App.jsx",
    `const { render } = require("./renderer");

function Label() { return <span />; }

export default function App() {
  return <Label />;
}
`,
  );

  assert.equal(node(result, "File", "src/App.jsx").language, "javascript");
  assert.equal(result.hasSyntaxErrors, false);
  assert.ok(hasEdge(result, "IMPORTS", "src/App.jsx", "./renderer"));
  // A JSX element name is a usage of the component, not a binding.
  assert.ok(hasEdge(result, "REFERENCES", "App", "Label"));
  assert.equal(node(result, "Function", "App").exported, true);
});

test("rejects files this plugin does not own", () => {
  assert.equal(isSupportedFile("src/lib.rs"), false);
  assert.equal(isSupportedFile("src/lib.mts"), true);
  assert.throws(() => extractFile("src/lib.rs", "fn main() {}"), UnsupportedFileError);
});

test("parses files larger than the native parser's default read buffer", () => {
  // node-tree-sitter hands the whole source over in one slice, so anything
  // past ~16k characters throws unless bufferSize is sized to the input.
  const lines: string[] = [];
  for (let i = 0; i < 2000; i += 1) lines.push(`export function fn${i}(a: number): number { return a; }`);
  const result = extractFile("src/big.ts", lines.join("\n"));

  assert.equal(result.hasSyntaxErrors, false);
  assert.equal(result.nodes.filter((n) => n.kind === "Function").length, 2000);
});
