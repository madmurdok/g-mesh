import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import {
  createProjectFileExists,
  createProjectResolver,
  createSpecifierResolver,
  isRelativeSpecifier,
  resolveRelativeSpecifier,
  resolveTsconfigPathsSpecifier,
  resolveWorkspaceSpecifier,
  type FileExists,
} from "../src/resolve";
import type { TsconfigPathsConfig, TsconfigPathsIndex } from "../src/tsconfigPaths";
import type { WorkspacePackages } from "../src/workspace";

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

test("relative resolution never claims a bare or package specifier", () => {
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

test("the fs-backed predicate treats a hard-excluded directory as non-existent even when the file is there", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-resolve-"));
  try {
    await fs.mkdir(path.join(root, "packages", "math", "dist", "prod"), { recursive: true });
    await fs.writeFile(path.join(root, "packages", "math", "dist", "prod", "index.js"), "module.exports = {};\n");
    await fs.mkdir(path.join(root, "packages", "math", "src"), { recursive: true });
    await fs.writeFile(path.join(root, "packages", "math", "src", "index.ts"), "export const x = 1;\n");

    const exists = createProjectFileExists(root);
    assert.equal(exists("packages/math/dist/prod/index.js"), false, "dist is hard-excluded regardless of gitignore");
    assert.equal(exists("packages/math/src/index.ts"), true);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("the fs-backed predicate treats a gitignored file as non-existent", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-resolve-"));
  try {
    await fs.writeFile(path.join(root, ".gitignore"), "src/generated.ts\n");
    await fs.mkdir(path.join(root, "src"), { recursive: true });
    await fs.writeFile(path.join(root, "src", "generated.ts"), "export const x = 1;\n");
    await fs.writeFile(path.join(root, "src", "kept.ts"), "export const y = 1;\n");

    const exists = createProjectFileExists(root);
    assert.equal(exists("src/generated.ts"), false);
    assert.equal(exists("src/kept.ts"), true);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

// --- workspace package specifiers ----------------------------------------

/** The excalidraw shape: an unbuilt monorepo whose manifests only mention
 * `dist`, so the entry that exists is the source one. */
const WORKSPACE: WorkspacePackages = new Map([
  [
    "@excalidraw/math",
    { dir: "packages/math", manifest: { name: "@excalidraw/math", main: "./dist/prod/index.js" } },
  ],
  [
    "@excalidraw/element",
    {
      dir: "packages/element",
      manifest: {
        name: "@excalidraw/element",
        exports: { ".": "./src/index.ts", "./types": "./src/types.ts" },
      },
    },
  ],
]);

function resolveBare(specifier: string, exists: FileExists): string | null {
  return resolveWorkspaceSpecifier(specifier, WORKSPACE, exists);
}

test("a workspace package resolves to the entry file it actually has", () => {
  const exists = existing("packages/math/src/index.ts");
  assert.equal(resolveBare("@excalidraw/math", exists), "packages/math/src/index.ts");
});

test("a declared entry wins when the build output is really there", () => {
  const exists = existing("packages/math/dist/prod/index.js", "packages/math/src/index.ts");
  assert.equal(resolveBare("@excalidraw/math", exists), "packages/math/dist/prod/index.js");
});

test("an exports map decides the entry, for the package root and its subpaths", () => {
  const exists = existing("packages/element/src/index.ts", "packages/element/src/types.ts");
  assert.equal(resolveBare("@excalidraw/element", exists), "packages/element/src/index.ts");
  assert.equal(resolveBare("@excalidraw/element/types", exists), "packages/element/src/types.ts");
});

test("a subpath with no exports entry falls back onto the package's source tree", () => {
  const exists = existing("packages/math/src/point.ts");
  assert.equal(resolveBare("@excalidraw/math/point", exists), "packages/math/src/point.ts");
});

test("a package outside the workspace stays unresolved", () => {
  // The one thing this must not start doing: claiming registry packages. Each
  // of these has a plausible-looking file in the tree and still resolves to
  // nothing, because no package.json in the workspace declares that name.
  const exists = existing(
    "node_modules/react/index.js",
    "packages/math/src/index.ts",
    "src/react.ts",
    "react.ts",
  );
  for (const specifier of ["react", "node:crypto", "lodash/fp", "@types/node"]) {
    assert.equal(resolveBare(specifier, exists), null, specifier);
  }
});

test("a workspace package whose entry is missing is not invented", () => {
  assert.equal(resolveBare("@excalidraw/math", existing("packages/math/package.json")), null);
});

test("a workspace with no packages resolves nothing", () => {
  const exists: FileExists = () => true;
  assert.equal(resolveWorkspaceSpecifier("@excalidraw/math", new Map(), exists), null);
});

// --- the whole resolver, against a real workspace on disk ----------------

test("the project resolver resolves workspace imports and leaves real packages alone", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-monorepo-"));
  const write = async (rel: string, contents: string): Promise<void> => {
    await fs.mkdir(path.dirname(path.join(root, rel)), { recursive: true });
    await fs.writeFile(path.join(root, rel), contents, "utf8");
  };
  try {
    await write("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n");
    await write(
      "packages/math/package.json",
      JSON.stringify({ name: "@excalidraw/math", main: "./dist/prod/index.js" }),
    );
    await write("packages/math/src/index.ts", "export const pointFrom = () => {};\n");
    await write("packages/math/src/point.ts", "export const p = 1;\n");
    await write("packages/element/package.json", JSON.stringify({ name: "@excalidraw/element" }));
    await write("packages/element/src/index.ts", "export const mutateElement = () => {};\n");
    await write("packages/element/src/shape.ts", "export const shape = 1;\n");
    await write("node_modules/react/package.json", JSON.stringify({ name: "react" }));
    await write("node_modules/react/index.js", "module.exports = {};\n");

    const resolve = createProjectResolver(root);
    const from = "packages/element/src/shape.ts";

    assert.equal(resolve("@excalidraw/math", from), "packages/math/src/index.ts");
    assert.equal(resolve("@excalidraw/math/point", from), "packages/math/src/point.ts");
    assert.equal(resolve("@excalidraw/element", from), "packages/element/src/index.ts");
    assert.equal(resolve("react", from), null);
    assert.equal(resolve("node:crypto", from), null);
    assert.equal(resolve("@excalidraw/nope", from), null);
    assert.equal(resolve("./index", from), "packages/element/src/index.ts");
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

// --- tsconfig `paths` aliases ---------------------------------------------

/** The alias map of a plain `@/*` app, as the config reader would hand it
 * over - so this stays a test of the resolution policy, not of tsconfig
 * parsing (see tsconfigPaths.test.ts for that half). */
const ALIASES: TsconfigPathsConfig = {
  resolveDir: "",
  paths: [
    { pattern: "@/*", targets: ["./src/*"] },
    { pattern: "config", targets: ["./src/config/index.ts"] },
  ],
};

const ALIAS_INDEX: TsconfigPathsIndex = () => ALIASES;

test("a paths alias resolves through the same extension guessing as a relative import", () => {
  const exists = existing("src/utils.ts", "src/config/index.ts", "src/db/index.tsx");
  const alias = (specifier: string): string | null =>
    resolveTsconfigPathsSpecifier(specifier, "src/app/page.tsx", ALIAS_INDEX, exists);

  assert.equal(alias("@/utils"), "src/utils.ts");
  assert.equal(alias("@/db"), "src/db/index.tsx");
  assert.equal(alias("config"), "src/config/index.ts");
  assert.equal(alias("@/nowhere"), null);
  assert.equal(alias("react"), null, "no key matches, so this is not an alias at all");
  assert.equal(alias("./utils"), null, "a relative specifier is the other function's job");
});

/** The excalidraw case: a specifier that is *both* a workspace package and an
 * alias key. Here the alias even points at a file that exists, and the
 * workspace answer still wins - the alias map is never consulted for it. */
test("the workspace answer wins over an alias for the same specifier", () => {
  const exists = existing("packages/math/src/index.ts", "src/shims/math.ts");
  const shadowing: TsconfigPathsIndex = () => ({
    resolveDir: "",
    paths: [
      { pattern: "@excalidraw/math", targets: ["./src/shims/math.ts"] },
      { pattern: "@/*", targets: ["./src/*"] },
    ],
  });
  const resolve = createSpecifierResolver(exists, WORKSPACE, shadowing);

  assert.equal(resolve("@excalidraw/math", "src/app.ts"), "packages/math/src/index.ts");
  // ...while a specifier no workspace package claims does fall through.
  assert.equal(resolve("@/shims/math", "src/app.ts"), "src/shims/math.ts");
});

test("the project resolver resolves aliases and workspace packages side by side", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-aliases-"));
  const write = async (rel: string, contents: string): Promise<void> => {
    await fs.mkdir(path.dirname(path.join(root, rel)), { recursive: true });
    await fs.writeFile(path.join(root, rel), contents, "utf8");
  };
  try {
    await write("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n");
    await write(
      "tsconfig.json",
      JSON.stringify({ compilerOptions: { baseUrl: ".", paths: { "@/*": ["./apps/web/src/*"] } } }),
    );
    await write("packages/math/package.json", JSON.stringify({ name: "@excalidraw/math" }));
    await write("packages/math/src/index.ts", "export const pointFrom = () => {};\n");
    await write("apps/web/src/util.ts", "export const util = 1;\n");
    await write("apps/web/src/page.tsx", 'import { util } from "@/util";\n');

    const resolve = createProjectResolver(root);

    assert.equal(
      resolve("@excalidraw/math", "apps/web/src/page.tsx"),
      "packages/math/src/index.ts",
    );
    assert.equal(resolve("@/util", "apps/web/src/page.tsx"), "apps/web/src/util.ts");
    // Same resolver instance, a file in another package: the alias is rooted
    // in the config, not in the importer's directory.
    assert.equal(resolve("@/util", "packages/math/src/index.ts"), "apps/web/src/util.ts");
    assert.equal(resolve("react", "apps/web/src/page.tsx"), null);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("an alias is refused when its target is outside the project or unparseable", async () => {
  const parent = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-alias-escape-"));
  const root = path.join(parent, "project");
  const write = async (abs: string, contents: string): Promise<void> => {
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, contents, "utf8");
  };
  try {
    // A real file, really outside the project root - so refusing it is a
    // decision, not an accident of the file being missing.
    await write(path.join(parent, "outside.ts"), "export const secret = 1;\n");
    await write(
      path.join(root, "tsconfig.json"),
      JSON.stringify({
        compilerOptions: {
          baseUrl: ".",
          paths: { outside: ["../outside.ts"], styles: ["./src/app.css"], "@/*": ["./src/*"] },
        },
      }),
    );
    await write(path.join(root, "src", "app.css"), ".a { color: red }\n");
    await write(path.join(root, "src", "kept.ts"), "export const kept = 1;\n");

    const resolve = createProjectResolver(root);
    assert.equal(resolve("outside", "src/kept.ts"), null);
    assert.equal(resolve("styles", "src/kept.ts"), null, "no File node could ever back a .css");
    assert.equal(resolve("@/kept", "src/kept.ts"), "src/kept.ts");
  } finally {
    await fs.rm(parent, { recursive: true, force: true });
  }
});
