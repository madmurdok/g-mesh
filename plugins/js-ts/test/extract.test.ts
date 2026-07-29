import { test } from "node:test";
import assert from "node:assert/strict";
import {
  extractFile,
  isSupportedFile,
  nodeIdFor,
  PENDING_SYMBOL_NATIVE_KIND,
  pendingSymbolQualifiedName,
  RESOLVED_MODULE_NATIVE_KIND,
  UnsupportedFileError,
  type EdgeKind,
  type ExtractOptions,
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

// --- import placeholders and their resolution ----------------------------

const IMPORTER_TS = `import { connect } from "./db/connection.js";
import { z } from "zod";
import { helper } from "./missing/helper.js";
export { pool } from "./db/pool";
const legacy = require("./legacy");
`;

/** A `SpecifierResolver` over a fixed set of project files, i.e. the shape
 * resolve.ts produces - kept local so this file tests what the *extractor*
 * does with a resolution, not how one is arrived at. */
function resolverOver(files: Record<string, string>) {
  return (specifier: string, fromFilePath: string): string | null => {
    assert.equal(fromFilePath, "src/index.ts", "the importer's own path is what specifiers are relative to");
    return files[specifier] ?? null;
  };
}

test("without a resolver every import target stays a raw-specifier placeholder", () => {
  const result = extractFile("src/index.ts", IMPORTER_TS);

  for (const specifier of ["./db/connection.js", "zod", "./missing/helper.js", "./db/pool", "./legacy"]) {
    const placeholder = node(result, "Module", specifier);
    assert.equal(placeholder.name, specifier);
    assert.equal(placeholder.nativeKind, "external_module");
    assert.ok(hasEdge(result, "IMPORTS", "src/index.ts", specifier), `IMPORTS ${specifier}`);
  }
});

test("a resolved specifier becomes a placeholder addressed by the path it names", () => {
  const result = extractFile("src/index.ts", IMPORTER_TS, {
    resolveSpecifier: resolverOver({
      "./db/connection.js": "src/db/connection.ts",
      "./db/pool": "src/db/pool.ts",
      "./legacy": "src/legacy.js",
    }),
  });

  const connection = node(result, "Module", "src/db/connection.ts");
  assert.equal(connection.nativeKind, RESOLVED_MODULE_NATIVE_KIND);
  assert.equal(connection.name, "./db/connection.js", "the raw specifier is still what the source says");
  assert.equal(connection.filePath, "src/index.ts", "the placeholder lives where the import statement is");
  assert.ok(hasEdge(result, "IMPORTS", "src/index.ts", "src/db/connection.ts"));

  // `export ... from` and `require()` are imports too, and resolve alike.
  assert.equal(node(result, "Module", "src/db/pool.ts").nativeKind, RESOLVED_MODULE_NATIVE_KIND);
  assert.equal(node(result, "Module", "src/legacy.js").nativeKind, RESOLVED_MODULE_NATIVE_KIND);
});

test("a bare specifier and a dangling relative one keep the old placeholder behaviour", () => {
  const result = extractFile("src/index.ts", IMPORTER_TS, {
    resolveSpecifier: resolverOver({ "./db/connection.js": "src/db/connection.ts" }),
  });

  // Nothing local to point at: a package, and a relative import of a file
  // that is not there (deleted, or a typo).
  for (const specifier of ["zod", "./missing/helper.js"]) {
    assert.equal(node(result, "Module", specifier).nativeKind, "external_module");
    assert.ok(hasEdge(result, "IMPORTS", "src/index.ts", specifier));
  }
});

test("resolution never claims an edge is resolved - that is core's call", () => {
  const result = extractFile("src/index.ts", IMPORTER_TS, {
    resolveSpecifier: resolverOver({ "./db/connection.js": "src/db/connection.ts" }),
  });

  for (const edge of result.edges) {
    assert.equal(edge.source, "tree-sitter");
    assert.equal(edge.resolved, false, "the target node is still a placeholder until core links it");
  }
});

test("two specifiers naming the same file collapse into one placeholder", () => {
  const result = extractFile(
    "src/index.ts",
    `import { a } from "./db/connection.js";\nimport type { B } from "./db/connection";\n`,
    { resolveSpecifier: () => "src/db/connection.ts" },
  );

  assert.equal(
    result.nodes.filter((n) => n.kind === "Module").length,
    1,
    "identity is the resolved path, so the two spellings are one target",
  );
  assert.equal(result.edges.filter((e) => e.kind === "IMPORTS").length, 1);
});

// --- cross-file symbol usages --------------------------------------------

/** Every specifier resolves to one and the same project file, which is all
 * these tests need: what is under test is what the extractor does with the
 * *names* such an import binds, not how a path is arrived at. */
const RESOLVES_TO_LIB: ExtractOptions = { resolveSpecifier: () => "src/lib.ts" };

/** The pending-symbol placeholder for `name`, asserted to be the only one. */
function pending(result: ExtractResult, targetPath: string, name: string): ExtractedNode {
  const matches = result.nodes.filter(
    (n) => n.nativeKind === PENDING_SYMBOL_NATIVE_KIND && n.name === name,
  );
  assert.equal(matches.length, 1, `expected exactly one pending symbol ${name}`);
  assert.equal(matches[0].qualifiedName, pendingSymbolQualifiedName(targetPath, name));
  return matches[0];
}

test("a call to an imported function becomes a CALLS edge onto a pending symbol", () => {
  const result = extractFile(
    "src/app.ts",
    `import { mutate } from "./lib";

export function run(): void {
  mutate(1);
}
`,
    RESOLVES_TO_LIB,
  );

  const placeholder = pending(result, "src/lib.ts", "mutate");
  assert.equal(placeholder.kind, "Module");
  assert.equal(placeholder.filePath, "src/app.ts", "the placeholder lives where the usage is");
  assert.ok(hasEdge(result, "CALLS", "run", "src/lib.ts#mutate"));
  assert.equal(
    result.edges.find((e) => e.kind === "CALLS")?.resolved,
    false,
    "whether that file really exports it is core's call, not the extractor's",
  );
});

test("an aliased import is addressed by the name the target file exports", () => {
  const result = extractFile(
    "src/app.ts",
    `import { mutate as change } from "./lib";

export function run(): void {
  change(1);
}
`,
    RESOLVES_TO_LIB,
  );

  const placeholder = pending(result, "src/lib.ts", "mutate");
  assert.ok(hasEdge(result, "CALLS", "run", placeholder.qualifiedName));
});

test("a default import is addressed as `default`", () => {
  const result = extractFile(
    "src/app.ts",
    `import cache from "./lib";

export function run(): void {
  cache.clear();
}
`,
    RESOLVES_TO_LIB,
  );

  assert.ok(hasEdge(result, "REFERENCES", "run", pending(result, "src/lib.ts", "default").qualifiedName));
});

test("an imported type used as a supertype becomes a SUPERTYPE_OF edge", () => {
  const result = extractFile(
    "src/laser.ts",
    `import { Trail } from "./lib";
import type { Drawable } from "./lib";

export class LaserTrails implements Trail {}

export interface Sketch extends Drawable {}
`,
    RESOLVES_TO_LIB,
  );

  assert.ok(hasEdge(result, "SUPERTYPE_OF", "LaserTrails", "src/lib.ts#Trail"));
  assert.ok(hasEdge(result, "SUPERTYPE_OF", "Sketch", "src/lib.ts#Drawable"));
});

test("imported names in type positions and JSX become REFERENCES edges", () => {
  const result = extractFile(
    "src/app.tsx",
    `import { Widget } from "./lib";
import type { Options } from "./lib";

export function render(options: Options) {
  return <Widget />;
}
`,
    RESOLVES_TO_LIB,
  );

  assert.ok(hasEdge(result, "REFERENCES", "render", "src/lib.ts#Options"));
  assert.ok(hasEdge(result, "REFERENCES", "render", "src/lib.ts#Widget"));
});

test("a local declaration shadows an import of the same name", () => {
  const result = extractFile(
    "src/app.ts",
    `import { mutate } from "./lib";

function mutate(): void {}

export function run(): void {
  mutate();
}
`,
    RESOLVES_TO_LIB,
  );

  assert.ok(hasEdge(result, "CALLS", "run", "mutate"), "the file's own declaration wins");
  assert.deepEqual(
    result.nodes.filter((n) => n.nativeKind === PENDING_SYMBOL_NATIVE_KIND),
    [],
    "nothing is pending when the name resolves locally",
  );
});

test("an unused import and an unresolvable specifier produce no pending symbol", () => {
  const unused = extractFile("src/app.ts", `import { mutate } from "./lib";\n`, RESOLVES_TO_LIB);
  assert.deepEqual(unused.nodes.filter((n) => n.nativeKind === PENDING_SYMBOL_NATIVE_KIND), []);

  // A package: its symbols are not in this index, so a placeholder for one
  // could never be linked to anything.
  const bare = extractFile(
    "src/app.ts",
    `import { z } from "zod";\n\nexport function run(): void {\n  z();\n}\n`,
    { resolveSpecifier: () => null },
  );
  assert.deepEqual(bare.nodes.filter((n) => n.nativeKind === PENDING_SYMBOL_NATIVE_KIND), []);
});

test("every usage of one imported symbol shares a single placeholder", () => {
  const result = extractFile(
    "src/app.ts",
    `import { mutate } from "./lib";

export function a(): void { mutate(); }
export function b(): void { mutate(); }
`,
    RESOLVES_TO_LIB,
  );

  pending(result, "src/lib.ts", "mutate"); // asserts there is exactly one
  assert.equal(result.edges.filter((e) => e.kind === "CALLS").length, 2);
});

test("a call to an import at module top level degrades to a usage edge", () => {
  const result = extractFile(
    "src/app.ts",
    `import { create } from "./lib";\n\nexport const instance = create();\n`,
    RESOLVES_TO_LIB,
  );

  // CALLS is Function -> Function, and a `const` initializer is neither.
  assert.equal(result.edges.filter((e) => e.kind === "CALLS").length, 0);
  assert.ok(hasEdge(result, "REFERENCES", "instance", "src/lib.ts#create"));
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
