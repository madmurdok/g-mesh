import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import {
  createProjectFileExists,
  isRelativeSpecifier,
  resolveRelativeSpecifier,
  type FileExists,
} from "../src/resolve";

/** The resolution policy is a pure function of "which paths exist", so a set
 * is a complete stand-in for a filesystem - and one that can describe cases
 * (a `.js` file that does *not* exist next to its `.ts` source) far more
 * legibly than a temp directory can. */
function existing(...paths: string[]): FileExists {
  const set = new Set(paths);
  return (candidate) => set.has(candidate);
}

function resolve(specifier: string, from: string, exists: FileExists): string | null {
  return resolveRelativeSpecifier(specifier, from, exists);
}

test("a specifier that already names a source file resolves to itself", () => {
  const exists = existing("src/db/connection.ts");
  assert.equal(resolve("./db/connection.ts", "src/index.ts", exists), "src/db/connection.ts");
});

test("an extensionless specifier picks up the source extension", () => {
  assert.equal(resolve("./util", "src/index.ts", existing("src/util.ts")), "src/util.ts");
  assert.equal(resolve("./util", "src/index.ts", existing("src/util.tsx")), "src/util.tsx");
  assert.equal(resolve("./util", "src/index.js", existing("src/util.js")), "src/util.js");
  assert.equal(resolve("./util", "src/index.ts", existing("src/util.d.ts")), "src/util.d.ts");
});

test("TypeScript's own extensions win over the JS ones for the same stem", () => {
  // A built project has both on disk; the source file is the one with nodes.
  const exists = existing("src/util.ts", "src/util.js");
  assert.equal(resolve("./util", "src/index.ts", exists), "src/util.ts");
});

/**
 * The case that makes or breaks a real ESM TypeScript project: the specifier
 * names the *emitted* file, which is not on disk at all before a build.
 */
test("an ESM `.js` specifier resolves to the `.ts` source it is compiled from", () => {
  const exists = existing("src/db/connection.ts");
  assert.equal(resolve("./db/connection.js", "src/index.ts", exists), "src/db/connection.ts");
  assert.equal(resolve("../db/connection.js", "src/mcp/tools.ts", exists), "src/db/connection.ts");
});

test("a real `.js` file next to TS sources still resolves to itself", () => {
  const exists = existing("scripts/build.js");
  assert.equal(resolve("./build.js", "scripts/run.ts", exists), "scripts/build.js");
});

test("the other emitted-extension pairs substitute the same way", () => {
  assert.equal(resolve("./x.mjs", "a.ts", existing("x.mts")), "x.mts");
  assert.equal(resolve("./x.cjs", "a.ts", existing("x.cts")), "x.cts");
  assert.equal(resolve("./x.jsx", "a.tsx", existing("x.tsx")), "x.tsx");
});

test("a directory specifier resolves to its index file", () => {
  const exists = existing("src/db/index.ts");
  assert.equal(resolve("./db", "src/index.ts", exists), "src/db/index.ts");
  assert.equal(resolve("./db/", "src/index.ts", exists), "src/db/index.ts");
  assert.equal(resolve("..", "src/db/pool.ts", existing("src/index.ts")), "src/index.ts");
  // Node resolves `"./"` to the importing directory's own index, which can be
  // the importing file itself - a real (if pointless) self-import, not an error.
  assert.equal(resolve("./", "src/index.ts", existing("src/index.ts")), "src/index.ts");
});

test("a file wins over a same-named directory's index", () => {
  const exists = existing("src/db.ts", "src/db/index.ts");
  assert.equal(resolve("./db", "src/index.ts", exists), "src/db.ts");
});

test("`..` segments are resolved against the importing file's directory", () => {
  const exists = existing("src/shared/log.ts");
  assert.equal(resolve("../shared/log", "src/mcp/tools.ts", exists), "src/shared/log.ts");
  assert.equal(resolve("./log", "src/shared/index.ts", exists), "src/shared/log.ts");
});

test("a bare or package specifier is never resolved", () => {
  // Even if a path-shaped file happens to exist, these do not address it.
  const exists = existing("node:crypto", "react.ts", "@scope/pkg.ts", "src/react.ts");
  for (const specifier of ["react", "node:crypto", "@modelcontextprotocol/sdk/server/stdio.js", "@scope/pkg"]) {
    assert.equal(resolve(specifier, "src/index.ts", exists), null, specifier);
    assert.equal(isRelativeSpecifier(specifier), false, specifier);
  }
});

test("a dangling relative import resolves to nothing rather than throwing", () => {
  const exists = existing("src/index.ts");
  assert.equal(resolve("./deleted", "src/index.ts", exists), null);
  assert.equal(resolve("./deleted.js", "src/index.ts", exists), null);
  assert.equal(resolve("../../way/outside", "src/index.ts", exists), null);
  assert.equal(resolve("./nowhere/", "src/index.ts", exists), null);
});

test("a specifier climbing out of the project root resolves to nothing", () => {
  // Such a file cannot have a node in this project's index even if it exists
  // on disk, and refusing it here is also what keeps the fs-backed
  // implementation from stat-ing outside the project root.
  const exists: FileExists = () => true;
  assert.equal(resolve("../outside", "index.ts", exists), null);
  assert.equal(resolve("../../outside", "src/index.ts", exists), null);
  // ...while a `..` that lands back inside it is perfectly ordinary.
  assert.equal(resolve("../index.ts", "src/deep/index.ts", exists), "src/index.ts");
});

test("a target this plugin does not parse is not claimed as resolved", () => {
  // The file is really there, but it will never have a `File` node, so
  // calling the import resolved would promise a link that cannot exist.
  const exists = existing("src/styles.css", "src/data.json", "src/README");
  assert.equal(resolve("./styles.css", "src/index.ts", exists), null);
  assert.equal(resolve("./data.json", "src/index.ts", exists), null);
  assert.equal(resolve("./README", "src/index.ts", exists), null);
});

test("an importer at the project root resolves against the root", () => {
  const exists = existing("util.ts", "lib/index.ts");
  assert.equal(resolve("./util", "index.ts", exists), "util.ts");
  assert.equal(resolve("./lib", "index.ts", exists), "lib/index.ts");
});

// --- the filesystem-backed predicate -------------------------------------

test("the fs-backed predicate answers about real files under the project root", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-resolve-"));
  try {
    await fs.mkdir(path.join(root, "src", "db"), { recursive: true });
    await fs.writeFile(path.join(root, "src", "db", "connection.ts"), "export const x = 1;\n");

    const exists = createProjectFileExists(root);
    assert.equal(exists("src/db/connection.ts"), true);
    assert.equal(exists("src/db/missing.ts"), false);
    assert.equal(exists("src/db"), false, "a directory is not a file");
    // Repeated lookups come off the memo; the answer must not change.
    assert.equal(exists("src/db/connection.ts"), true);

    assert.equal(
      resolveRelativeSpecifier("./db/connection.js", "src/index.ts", exists),
      "src/db/connection.ts",
      "the emitted-extension rule must work against a real tree too",
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});
