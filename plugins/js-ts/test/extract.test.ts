import { test } from "node:test";
import assert from "node:assert/strict";
import {
  extractFile,
  isSupportedFile,
  nodeIdFor,
  PENDING_SYMBOL_NATIVE_KIND,
  pendingSymbolQualifiedName,
  REEXPORT_ALL_NAME,
  REEXPORT_NATIVE_KIND,
  RESOLVED_MODULE_NATIVE_KIND,
  UnsupportedFileError,
  type EdgeKind,
  type ExtractOptions,
  type ExtractResult,
  type ExtractedNode,
  type NamespaceMemberUse,
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

test("an edge onto a declaration of this file is resolved; one onto a placeholder is not", () => {
  const result = extractFile("src/greeter.ts", GREETER_TS);
  assert.ok(result.edges.length > 0);
  const byId = new Map(result.nodes.map((n) => [n.id, n]));

  for (const edge of result.edges) {
    assert.equal(edge.source, "tree-sitter");
    const target = byId.get(edge.toId)!;
    // The only non-declaration target here is the unresolvable `./i18n`
    // module: everything else is declared in this very file, so nothing about
    // those edges is left for core to confirm.
    assert.equal(
      edge.resolved,
      target.nativeKind !== "external_module",
      `${edge.kind} -> ${target.qualifiedName}`,
    );
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

test("specifier resolution never claims an IMPORTS edge is resolved - that is core's call", () => {
  const result = extractFile("src/index.ts", IMPORTER_TS, {
    resolveSpecifier: resolverOver({ "./db/connection.js": "src/db/connection.ts" }),
  });

  const imports = result.edges.filter((e) => e.kind === "IMPORTS");
  assert.ok(imports.length > 0);
  for (const edge of imports) {
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

// --- calls made from functions that are not declarations -----------------

/**
 * Both shapes are lifted from excalidraw, where `find_callers` used to miss
 * them: an arrow function as an object-literal property value handed to a
 * call (`register({ perform: ... })`, and every `React.memo(props => ...)` /
 * `forwardRef` component), and an arrow function as a callback argument
 * (`.map`, `setTimeout`, `.then`). Neither position gets a Function node -
 * there is no name to hang one on - so the calls inside them used to be
 * attributed to nothing and dropped.
 */
const NESTED_CALLBACKS_TS = `import { mutate } from "./lib";

function helper(): void {}

export const action = register({
  name: "wrap",
  perform: (element) => {
    helper();
    mutate(element);
  },
});

export const copies = elements.map((element) => {
  helper();
  return mutate(element);
});
`;

test("a call inside a callback is attributed to the symbol the callback was written into", () => {
  const result = extractFile("src/app.ts", NESTED_CALLBACKS_TS, RESOLVES_TO_LIB);

  // The object-literal property value handed to `register(...)`.
  assert.ok(hasEdge(result, "CALLS", "action", "helper"));
  assert.ok(hasEdge(result, "CALLS", "action", "src/lib.ts#mutate"));
  // The `.map` callback argument.
  assert.ok(hasEdge(result, "CALLS", "copies", "helper"));
  assert.ok(hasEdge(result, "CALLS", "copies", "src/lib.ts#mutate"));

  // The caller is the `const` itself: an anonymous function has no name to
  // build a qualifiedName from, and a positional one would break the rule
  // that node ids survive edits elsewhere in the file.
  assert.equal(node(result, "Variable", "action").kind, "Variable");
  assert.deepEqual(
    result.nodes.filter((n) => n.kind === "Function").map((n) => n.qualifiedName),
    ["helper"],
    "no synthetic node is invented for the callbacks themselves",
  );
});

test("a callback inside a function still attributes its calls to that function", () => {
  const result = extractFile(
    "src/app.ts",
    `function helper(): void {}

export function run(items: string[]): void {
  items.forEach((item) => {
    helper();
  });
}
`,
  );

  assert.ok(hasEdge(result, "CALLS", "run", "helper"));
  assert.equal(result.edges.filter((e) => e.kind === "CALLS").length, 1, "not doubled, not moved");
});

test("every way of writing a function as a value carries its calls, not just arrows", () => {
  const result = extractFile(
    "src/app.ts",
    `function first(): void {}
function second(): void {}
function third(): void {}
function fourth(): void {}

export const api = wrap({
  reset() { first(); },
  gen: function* () { second(); },
  legacy: function () { third(); },
  deferred: () => setTimeout(() => fourth(), 0),
});
`,
  );

  // An object-literal method, a generator, a function expression, and an
  // arrow nested two callbacks deep - the gap was never specific to arrows.
  for (const callee of ["first", "second", "third", "fourth"]) {
    assert.ok(hasEdge(result, "CALLS", "api", callee), `CALLS api -> ${callee}`);
  }
});

test("a callback in a class field attributes its calls to the class", () => {
  const result = extractFile(
    "src/collab.ts",
    `export class Collab {
  queue = throttle(() => {
    this.save();
  });

  save(): void {}
}
`,
  );

  // The field's value is a call, not a function, so the field is below symbol
  // granularity and the class is the nearest thing that can own the call.
  assert.ok(hasEdge(result, "CALLS", "Collab", "Collab#save"));
});

test("a callback at module top level still degrades to a usage edge", () => {
  const result = extractFile(
    "src/app.ts",
    `import { create } from "./lib";

setTimeout(() => create(), 0);
`,
    RESOLVES_TO_LIB,
  );

  // Nothing declared encloses it, and the File is not a caller: a call made
  // at module load time is made by nothing.
  assert.equal(result.edges.filter((e) => e.kind === "CALLS").length, 0);
  assert.ok(hasEdge(result, "REFERENCES", "src/app.ts", "src/lib.ts#create"));
});

test("locals declared inside a callback stay out of the graph", () => {
  const result = extractFile(
    "src/app.ts",
    `export const total = values.reduce((sum, value) => {
  const doubled = value * 2;
  function inner(): void {}
  return sum + doubled;
}, 0);
`,
  );

  // A callback body is a function body, so what it declares is a local -
  // these used to surface as module-level symbols of the enclosing file.
  assert.equal(node(result, "Variable", "total").kind, "Variable");
  assert.deepEqual(
    result.nodes.filter((n) => n.name === "doubled" || n.name === "inner"),
    [],
  );
});

// --- re-exports ----------------------------------------------------------

/** Every re-export placeholder the extraction produced, as
 * `[published name, address]` pairs, sorted for a stable comparison. */
function reexports(result: ExtractResult): [string, string][] {
  return result.nodes
    .filter((n) => n.nativeKind === REEXPORT_NATIVE_KIND)
    .map((n): [string, string] => [n.name, n.qualifiedName])
    .sort();
}

/** Resolution for the barrel fixtures below: one specifier per target file. */
const BARREL_RESOLVER: ExtractOptions = {
  resolveSpecifier: (specifier) =>
    specifier.startsWith("./") ? `src/${specifier.slice(2)}.ts` : null,
};

test("a barrel records what it publishes and where each name really lives", () => {
  const result = extractFile(
    "src/index.ts",
    `export * from "./mutateElement";
export { bindText } from "./textBinding";
export { newElement as create } from "./factory";
export type { Bounds } from "./bounds";
`,
    BARREL_RESOLVER,
  );

  assert.deepEqual(reexports(result), [
    ["*", "src/mutateElement.ts#*"],
    ["Bounds", "src/bounds.ts#Bounds"],
    // The published name is the alias; the address is the name over there.
    ["create", "src/factory.ts#newElement"],
    ["bindText", "src/textBinding.ts#bindText"],
  ].sort());

  const placeholder = node(result, "Module", "src/factory.ts#newElement");
  assert.equal(placeholder.kind, "Module");
  assert.equal(placeholder.filePath, "src/index.ts", "the placeholder lives where the statement is");
  assert.equal(placeholder.exported, false, "a placeholder is not a symbol this file exports");
  // `export ... from` is still an import, and still resolves as one.
  assert.ok(hasEdge(result, "IMPORTS", "src/index.ts", "src/factory.ts"));
});

test("a whole-module re-export is addressed by the name no symbol can have", () => {
  const result = extractFile("src/index.ts", `export * from "./lib";\n`, BARREL_RESOLVER);

  const placeholder = node(result, "Module", `src/lib.ts#${REEXPORT_ALL_NAME}`);
  assert.equal(placeholder.name, REEXPORT_ALL_NAME, "it publishes every name, so it names none");
  assert.equal(placeholder.qualifiedName, pendingSymbolQualifiedName("src/lib.ts", REEXPORT_ALL_NAME));
});

test("`export * as NS from` binds a namespace, so it is not a re-export of names", () => {
  const result = extractFile("src/index.ts", `export * as shapes from "./lib";\n`, BARREL_RESOLVER);

  // Same reason `import * as NS` binds nothing: resolving `shapes.f` needs to
  // tell a module's export from an ordinary property access.
  assert.deepEqual(reexports(result), []);
  assert.ok(hasEdge(result, "IMPORTS", "src/index.ts", "src/lib.ts"));
});

test("a re-export of a specifier that resolves to nothing records no placeholder", () => {
  const result = extractFile(
    "src/index.ts",
    `export * from "react";\nexport { z } from "zod";\n`,
    { resolveSpecifier: () => null },
  );

  // No file of theirs is in this index, so there is no chain to follow.
  assert.deepEqual(reexports(result), []);
});

test("a local `export { name }` still marks the declaration, not a re-export", () => {
  const result = extractFile(
    "src/lib.ts",
    `function boot(): void {}\nexport { boot as started };\n`,
    BARREL_RESOLVER,
  );

  assert.equal(node(result, "Function", "boot").exported, true);
  assert.deepEqual(reexports(result), [], "nothing is forwarded anywhere - the symbol is right here");
});

test("a re-export binds nothing locally, so it cannot be mistaken for a declaration", () => {
  const result = extractFile(
    "src/index.ts",
    `export { mutate } from "./lib";

export function run(): void {
  mutate();
}
`,
    BARREL_RESOLVER,
  );

  // The re-export placeholder is not a binding: `mutate` here is unbound (the
  // re-export never imported it into this scope), so nothing is claimed about
  // the call at all.
  assert.deepEqual(reexports(result), [["mutate", "src/lib.ts#mutate"]]);
  assert.equal(result.edges.filter((e) => e.kind === "CALLS").length, 0);
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

// --- lexical scoping -----------------------------------------------------
//
// Everything below guards the one thing that makes a same-file edge worth
// `resolved: true`: a name matched here is matched against the declarations
// the usage can actually reach. A local of the same name is not one of them,
// and neither is a class member, which no bare name can address.

/** Every CALLS/REFERENCES edge, as `KIND from -> to`, for whole-result asserts. */
function usageEdges(result: ExtractResult): string[] {
  const byId = new Map(result.nodes.map((n) => [n.id, n]));
  return result.edges
    .filter((e) => e.kind === "CALLS" || e.kind === "REFERENCES")
    .map((e) => `${e.kind} ${byId.get(e.fromId)?.qualifiedName} -> ${byId.get(e.toId)?.qualifiedName}`)
    .sort();
}

for (const [label, shadow] of [
  ["a parameter", "export function outer(helper: () => void): void {\n  helper();\n}"],
  ["a local const", "export function outer(): void {\n  const helper = () => {};\n  helper();\n}"],
  [
    "a nested function declaration",
    "export function outer(): void {\n  function helper(): void {}\n  helper();\n}",
  ],
  [
    "a hoisted `var` declared below the call",
    "export function outer(): void {\n  helper();\n  var helper = () => {};\n}",
  ],
  ["a destructured local", "export function outer(o: any): void {\n  const { a: helper } = o;\n  helper();\n}"],
  ["a `catch` parameter", "export function outer(): void {\n  try {} catch (helper) { helper(); }\n}"],
  ["a `for...of` binding", "export function outer(xs: any[]): void {\n  for (const helper of xs) helper();\n}"],
  ["a callback parameter", "export function outer(xs: any[]): void {\n  xs.forEach((helper) => helper());\n}"],
] as const) {
  test(`${label} shadowing a file-level function suppresses the call edge`, () => {
    const result = extractFile("src/p.ts", `function helper(): void {}\n\n${shadow}\n`);
    assert.deepEqual(usageEdges(result), [], "the call names the local, and locals are not graph symbols");
  });
}

test("shadowing confined to one block leaves a call outside it resolved", () => {
  const result = extractFile(
    "src/p.ts",
    `function helper(): void {}

export function outer(): void {
  { const helper = 1; void helper; }
  helper();
}
`,
  );

  // Over-approximating shadowing to the whole function would drop this, and
  // completeness for bare calls is a documented guarantee - so the scope
  // chain follows real block scoping instead.
  assert.ok(hasEdge(result, "CALLS", "outer", "helper"));
});

test("a function's own name is not shadowed by itself, so recursion resolves", () => {
  const direct = extractFile(
    "src/p.ts",
    `export function walk(n: number): void {\n  if (n) walk(n - 1);\n}\n`,
  );
  assert.ok(hasEdge(direct, "CALLS", "walk", "walk"));

  // `var f = function f() { f() }` - the inner binding and the symbol the
  // function was declared into are the same function.
  const expression = extractFile("src/p.js", `var walk = function walk(n) {\n  if (n) walk(n - 1);\n};\n`);
  assert.ok(hasEdge(expression, "CALLS", "walk", "walk"));
});

test("a bare name never resolves to a class member, which only a receiver can address", () => {
  const result = extractFile(
    "src/p.ts",
    `class Logger {
  log(message: string): void {}
  private level = 0;
}

export function outer(): void {
  log("hi");
  void level;
}
`,
  );

  assert.deepEqual(
    usageEdges(result),
    [],
    "`log`/`level` are unbound here; matching them onto Logger's members is the wrong-edge case",
  );
});

test("a member is still reached through a receiver that names its owner", () => {
  const result = extractFile(
    "src/p.ts",
    `class Logger {
  log(message: string): void { this.flush(); }
  private flush(): void {}
  static of(): Logger { return new Logger(); }
}

export function outer(): void {
  Logger.of();
}
`,
  );

  assert.ok(hasEdge(result, "CALLS", "Logger#log", "Logger#flush"), "`this.` names the owner");
  assert.ok(hasEdge(result, "CALLS", "outer", "Logger.of"), "`Class.` names the owner");
});

test("a local shadowing an import claims neither the import nor a same-named declaration", () => {
  const result = extractFile(
    "src/app.ts",
    `import { mutate } from "./lib";

export function run(): void {
  const mutate = () => {};
  mutate();
}
`,
    RESOLVES_TO_LIB,
  );

  assert.deepEqual(usageEdges(result), []);
  assert.deepEqual(result.nodes.filter((n) => n.nativeKind === PENDING_SYMBOL_NATIVE_KIND), []);
});

test("a method whose name matches an import calls the import, not itself", () => {
  const result = extractFile(
    "src/app.ts",
    `import { encrypt } from "./lib";

export class Envelope {
  async encrypt(): Promise<void> {
    await encrypt();
  }
}
`,
    RESOLVES_TO_LIB,
  );

  // The bare call cannot mean the method - only `this.encrypt()` could - so
  // it falls through to the import, which is what a reader sees too.
  assert.ok(hasEdge(result, "CALLS", "Envelope#encrypt", "src/lib.ts#encrypt"));
  assert.ok(!hasEdge(result, "CALLS", "Envelope#encrypt", "Envelope#encrypt"));
});

test("an edge onto a symbol this file declares is resolved, one onto an imported symbol is not", () => {
  const result = extractFile(
    "src/app.ts",
    `import { remote } from "./lib";

function local(): void {}

export function run(): void {
  local();
  remote();
}
`,
    RESOLVES_TO_LIB,
  );

  const byId = new Map(result.nodes.map((n) => [n.id, n]));
  const calls = new Map(
    result.edges.filter((e) => e.kind === "CALLS").map((e) => [byId.get(e.toId)!.qualifiedName, e.resolved]),
  );

  assert.equal(calls.get("local"), true, "nothing about a same-file call is left to confirm");
  assert.equal(
    calls.get("src/lib.ts#remote"),
    false,
    "whether that file exports it is a fact about the index, which only core has",
  );
});

// --- namespace imports ----------------------------------------------------
//
// `import * as ns` still binds no symbol here, and still emits no edge of its
// own - what is new is that the sites written against it are *recorded* rather
// than dropped, so semanticPass.ts can ask a checker which export each names.

function uses(result: ExtractResult): NamespaceMemberUse[] {
  return result.namespaceMemberUses;
}

test("a namespace import emits no edge of its own, but records the member sites", () => {
  const result = extractFile(
    "src/app.ts",
    `import * as lib from "./lib";

export function run(): void {
  lib.mutate();
}
`,
    RESOLVES_TO_LIB,
  );

  // The gap this exists to close: nothing here names `mutate`, so the
  // structural pass has nothing to point an edge at and emits none.
  assert.deepEqual(
    result.nodes.filter((n) => n.nativeKind === PENDING_SYMBOL_NATIVE_KIND),
    [],
    "the extractor must not guess which export `lib.mutate` names",
  );
  assert.deepEqual(
    result.edges.filter((e) => e.kind === "CALLS"),
    [],
    "and must not invent a call edge either",
  );

  assert.equal(uses(result).length, 1);
  const [use] = uses(result);
  assert.equal(use.memberName, "mutate");
  assert.equal(use.namespaceName, "lib");
  assert.equal(use.modulePath, "src/lib.ts");
  assert.equal(use.edgeKind, "CALLS");
  assert.equal(use.fromId, node(result, "Function", "run").id);
  // The recorded position is the member name itself - what a point query is
  // aimed at - not the receiver and not the whole expression.
  assert.equal(use.line, 3);
  assert.equal(use.col, "  lib.".length);
});

test("a namespace member read outside a call is recorded as a reference", () => {
  const result = extractFile(
    "src/app.ts",
    `import * as lib from "./lib";

export const answer = lib.value;
`,
    RESOLVES_TO_LIB,
  );

  assert.equal(uses(result).length, 1);
  assert.equal(uses(result)[0].edgeKind, "REFERENCES");
  assert.equal(uses(result)[0].memberName, "value");
  assert.equal(uses(result)[0].fromId, node(result, "Variable", "answer").id);
});

test("a namespace call at module top level degrades to a reference from the file", () => {
  const result = extractFile(
    "src/app.ts",
    `import * as lib from "./lib";

lib.boot();
`,
    RESOLVES_TO_LIB,
  );

  // Same rule an ordinary imported call follows: a CALLS edge is made by some
  // function, and a call written at module top level is made by none.
  assert.equal(uses(result)[0].edgeKind, "REFERENCES");
  assert.equal(uses(result)[0].fromId, node(result, "File", "src/app.ts").id);
});

test("a namespace import of a specifier outside this project records nothing", () => {
  const result = extractFile(
    "src/app.ts",
    `import * as p from "node:path";

export function run(): string {
  return p.join("a", "b");
}
`,
    { resolveSpecifier: () => null },
  );

  // Nothing of `node:path` is in this index, so no placeholder addressed at it
  // could ever be linked - and asking a checker about it would be pure cost.
  assert.deepEqual(uses(result), []);
});

test("a local binding shadowing a namespace import is not a namespace member access", () => {
  const result = extractFile(
    "src/app.ts",
    `import * as lib from "./lib";

export function run(lib: { mutate(): void }): void {
  lib.mutate();
}
`,
    RESOLVES_TO_LIB,
  );

  assert.deepEqual(uses(result), [], "the parameter shadows the import, as it does in the language");
});

test("an ordinary property access on a value is not mistaken for a namespace member", () => {
  const result = extractFile(
    "src/app.ts",
    `const config = { mutate(): void {} };

export function run(): void {
  config.mutate();
}
`,
    RESOLVES_TO_LIB,
  );

  assert.deepEqual(uses(result), []);
});

test("recording namespace sites leaves the edges a named import already resolved alone", () => {
  const result = extractFile(
    "src/app.ts",
    `import * as lib from "./lib";
import { helper } from "./lib";

export function run(): void {
  helper();
  lib.mutate();
}
`,
    RESOLVES_TO_LIB,
  );

  assert.ok(hasEdge(result, "CALLS", "run", "src/lib.ts#helper"));
  assert.equal(uses(result).length, 1);
});

// --- generic types ---------------------------------------------------------
//
// A name someone wrote down is a reference; an instantiation is not a symbol.
// `Box<Widget>` mentions two types and both are recorded - `Box<Widget>` itself
// never becomes a node. The three sites below each used to drop an explicitly
// written name, and the fourth test guards the prerequisite that makes them
// safe: a declaration's own `<T, ...>` must not name-match a file-level type.
// See "Generic types" in docs/architecture/g-mesh-v1.md.

test("a generic type's head is a reference, exactly as the same name written bare is", () => {
  const result = extractFile(
    "src/p.ts",
    `export class Widget {}
export class Box<T> {}

export const held: Box<Widget> = null!;
export const plain: Box = null!;
export type Held = Box<Widget>;
`,
  );

  // `Box` used to be discarded here and only kept in `plain`, because a
  // generic type holds its head in a field called `name` - the field every
  // other declaration binds through.
  assert.deepEqual(usageEdges(result), [
    "REFERENCES Held -> Box",
    "REFERENCES Held -> Widget",
    "REFERENCES held -> Box",
    "REFERENCES held -> Widget",
    "REFERENCES plain -> Box",
  ]);

  // Nothing here needs a checker: these are ordinary structural edges onto
  // declarations of this very file.
  for (const edge of result.edges) {
    assert.equal(edge.source, "tree-sitter");
    assert.equal(edge.resolved, true);
  }
});

test("type arguments in a heritage clause are references, and the head stays only a supertype", () => {
  const result = extractFile(
    "src/p.ts",
    `export class Widget {}
export class Box<T> {}
export interface Reg<T> {}

export class WidgetBox extends Box<Widget> implements Reg<Widget> {}
export interface WidgetReg extends Reg<Widget> {}
`,
  );

  assert.ok(hasEdge(result, "SUPERTYPE_OF", "WidgetBox", "Box"));
  assert.ok(hasEdge(result, "SUPERTYPE_OF", "WidgetBox", "Reg"));
  assert.ok(hasEdge(result, "SUPERTYPE_OF", "WidgetReg", "Reg"));

  // `Widget` was dropped entirely; `Box`/`Reg` must stay SUPERTYPE_OF *only*,
  // because find_references unions the two kinds and would otherwise report
  // one written name twice.
  assert.deepEqual(usageEdges(result), [
    "REFERENCES WidgetBox -> Widget",
    "REFERENCES WidgetReg -> Widget",
  ]);
});

test("type arguments at a call and a `new` site are references", () => {
  const result = extractFile(
    "src/p.ts",
    `export class Widget {}
export class Box<T> {}
export function identity<V>(value: V): V {
  return value;
}

export function build() {
  return new Box<Widget>();
}

export function pick() {
  return identity<Widget>(null!);
}
`,
  );

  // Neither function annotates a type, so `Widget` can only have come from the
  // `type_arguments` field - the third field beside `function`/`constructor`
  // and `arguments`, which neither handler used to read.
  assert.deepEqual(usageEdges(result), [
    "CALLS pick -> identity",
    "REFERENCES build -> Box",
    "REFERENCES build -> Widget",
    "REFERENCES pick -> Widget",
  ]);
});

test("a type parameter shadows a file-level type of the same name", () => {
  const result = extractFile(
    "src/p.ts",
    `export interface T { tag: string }

export class Holder<T> {
  item: T;
  wrap<T>(value: T): T {
    return value;
  }
}

export type Held<T> = Holder<T>;
export type Mapper = <T>(x: T) => T;
export interface Factory {
  <T>(x: T): T;
}
export function keep<T>(value: T): T {
  return value;
}

export const real: T = { tag: "" };
`,
  );

  // Every `T` above but the last is a type parameter, i.e. a type this
  // declaration itself declares - matching it onto the interface is the
  // wrong-edge case, and the generic fixes above multiply how often a bare
  // type-parameter name is walked. `real` proves the shadowing is scoped to
  // the declaration rather than suppressing the name file-wide.
  assert.deepEqual(usageEdges(result), ["REFERENCES Held -> Holder", "REFERENCES real -> T"]);
});

test("a type parameter shadows a type of that name and nothing else", () => {
  const result = extractFile(
    "src/p.ts",
    `export function make(): void {}

export function run<make>(value: make): void {
  make();
}
`,
  );

  // Contrived - nobody names a type parameter after a function - but it pins
  // the rule the shadowing is written to: `<T>` binds in the type namespace
  // only, so the call still names this file's function.
  assert.deepEqual(usageEdges(result), ["CALLS run -> make"]);
});
