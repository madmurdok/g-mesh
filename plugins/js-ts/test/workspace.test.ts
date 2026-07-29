import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import {
  packageEntryTargets,
  parseBareSpecifier,
  readWorkspacePackages,
  type WorkspacePackage,
} from "../src/workspace";

/** A real directory tree, like the other filesystem-facing tests here: the
 * manifest reading being tested *is* filesystem behaviour (globs, missing
 * files, malformed JSON), which a stubbed fs would only re-state. */
async function makeProject(files: Record<string, string>): Promise<string> {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-workspace-"));
  for (const [rel, contents] of Object.entries(files)) {
    const abs = path.join(root, rel);
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, contents, "utf8");
  }
  return root;
}

function manifest(fields: Record<string, unknown>): string {
  return JSON.stringify(fields, null, 2);
}

async function withProject(
  files: Record<string, string>,
  body: (root: string) => void | Promise<void>,
): Promise<void> {
  const root = await makeProject(files);
  try {
    await body(root);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

// --- specifier shapes -----------------------------------------------------

test("a package specifier splits into its name and the subpath it addresses", () => {
  assert.deepEqual(parseBareSpecifier("react"), { name: "react", subpath: "." });
  assert.deepEqual(parseBareSpecifier("@excalidraw/math"), {
    name: "@excalidraw/math",
    subpath: ".",
  });
  assert.deepEqual(parseBareSpecifier("@excalidraw/element/types"), {
    name: "@excalidraw/element",
    subpath: "./types",
  });
  assert.deepEqual(parseBareSpecifier("lodash/fp/get.js"), {
    name: "lodash",
    subpath: "./fp/get.js",
  });
});

test("what is not a package specifier at all is refused", () => {
  for (const specifier of ["", ".", "./x", "../x", "/abs/x", "node:crypto", "#internal", "data:,x"]) {
    assert.equal(parseBareSpecifier(specifier), null, specifier);
  }
});

// --- entry targets --------------------------------------------------------

function pkg(fields: Record<string, unknown>, dir = "packages/math"): WorkspacePackage {
  return { dir, manifest: fields };
}

test("the declared entry fields are all offered, most authoritative first", () => {
  const targets = packageEntryTargets(
    pkg({ main: "./dist/index.js", types: "./dist/index.d.ts", source: "./src/index.ts" }),
    ".",
  );
  assert.deepEqual(targets.slice(0, 3), [
    "packages/math/src/index.ts",
    "packages/math/dist/index.js",
    "packages/math/dist/index.d.ts",
  ]);
});

test("an exports map is read for the subpath asked about", () => {
  const target = pkg({
    exports: {
      ".": { types: "./dist/index.d.ts", import: "./dist/index.js" },
      "./utils": "./src/utils.ts",
    },
  });
  assert.equal(packageEntryTargets(target, ".")[0], "packages/math/dist/index.js");
  assert.ok(packageEntryTargets(target, ".").includes("packages/math/dist/index.d.ts"));
  assert.equal(packageEntryTargets(target, "./utils")[0], "packages/math/src/utils.ts");
});

test("a condition map without subpath keys describes the package root", () => {
  const target = pkg({ exports: { import: "./src/index.ts", require: "./dist/index.cjs" } });
  assert.deepEqual(packageEntryTargets(target, ".").slice(0, 2), [
    "packages/math/src/index.ts",
    "packages/math/dist/index.cjs",
  ]);
});

test("a wildcard exports subpath substitutes the matched part", () => {
  const target = pkg({ exports: { "./*": "./src/*.ts" } });
  assert.equal(packageEntryTargets(target, "./point")[0], "packages/math/src/point.ts");
  assert.equal(packageEntryTargets(target, "./geo/line")[0], "packages/math/src/geo/line.ts");
});

/** The case every unbuilt monorepo lands in: the manifest only ever mentions
 * build output, so without this fallback nothing in the workspace resolves. */
test("the source-tree conventions are offered after whatever the manifest declares", () => {
  const targets = packageEntryTargets(pkg({ main: "./dist/prod/index.js" }), ".");
  assert.deepEqual(targets, [
    "packages/math/dist/prod/index.js",
    "packages/math/src/index",
    "packages/math/index",
  ]);
});

test("a subpath falls back to the same path inside the package and its src", () => {
  assert.deepEqual(packageEntryTargets(pkg({}), "./types"), [
    "packages/math/types",
    "packages/math/src/types",
  ]);
});

test("an entry pointing outside the project is not offered", () => {
  assert.deepEqual(packageEntryTargets(pkg({ main: "../../../etc/passwd" }, "packages/math"), "."), [
    "packages/math/src/index",
    "packages/math/index",
  ]);
  assert.ok(!packageEntryTargets(pkg({ main: "/etc/passwd" }), ".").includes("/etc/passwd"));
});

// --- reading a workspace off disk -----------------------------------------

test("pnpm workspace globs are expanded to the packages they name", async () => {
  await withProject(
    {
      "pnpm-workspace.yaml": "packages:\n  - 'packages/*'\n  - excalidraw-app\n",
      "packages/math/package.json": manifest({ name: "@excalidraw/math", main: "./src/index.ts" }),
      "packages/element/package.json": manifest({ name: "@excalidraw/element" }),
      "excalidraw-app/package.json": manifest({ name: "@excalidraw/app" }),
      "packages/not-a-package/readme.md": "no manifest here\n",
    },
    (root) => {
      const packages = readWorkspacePackages(root);
      assert.deepEqual(
        [...packages.keys()].sort(),
        ["@excalidraw/app", "@excalidraw/element", "@excalidraw/math"],
      );
      assert.equal(packages.get("@excalidraw/math")?.dir, "packages/math");
      assert.equal(packages.get("@excalidraw/math")?.manifest.main, "./src/index.ts");
    },
  );
});

test("the root package.json `workspaces` field is read the same way", async () => {
  await withProject(
    {
      "package.json": manifest({ name: "root", workspaces: ["apps/*", "libs/shared"] }),
      "apps/web/package.json": manifest({ name: "@acme/web" }),
      "libs/shared/package.json": manifest({ name: "@acme/shared" }),
      "libs/other/package.json": manifest({ name: "@acme/other" }),
    },
    (root) => {
      const packages = readWorkspacePackages(root);
      assert.deepEqual([...packages.keys()].sort(), ["@acme/shared", "@acme/web"]);
    },
  );
});

test("yarn's object form of `workspaces` is read too", async () => {
  await withProject(
    {
      "package.json": manifest({ name: "root", workspaces: { packages: ["libs/*"] } }),
      "libs/shared/package.json": manifest({ name: "@acme/shared" }),
    },
    (root) => {
      assert.deepEqual([...readWorkspacePackages(root).keys()], ["@acme/shared"]);
    },
  );
});

test("a `!` pattern removes a package the globs had picked up", async () => {
  await withProject(
    {
      "pnpm-workspace.yaml": 'packages:\n  - "packages/**"\n  - "!packages/legacy/**"\n',
      "packages/math/package.json": manifest({ name: "@acme/math" }),
      "packages/legacy/old/package.json": manifest({ name: "@acme/old" }),
    },
    (root) => {
      assert.deepEqual([...readWorkspacePackages(root).keys()], ["@acme/math"]);
    },
  );
});

test("node_modules is never walked into, so a vendored copy is not a workspace package", async () => {
  await withProject(
    {
      "pnpm-workspace.yaml": "packages:\n  - '**'\n",
      "packages/math/package.json": manifest({ name: "@acme/math" }),
      "node_modules/react/package.json": manifest({ name: "react" }),
      "packages/math/node_modules/left-pad/package.json": manifest({ name: "left-pad" }),
    },
    (root) => {
      assert.deepEqual([...readWorkspacePackages(root).keys()], ["@acme/math"]);
    },
  );
});

test("a project with no workspace manifest has no workspace packages", async () => {
  await withProject(
    {
      "package.json": manifest({ name: "single", main: "./src/index.ts" }),
      "src/index.ts": "export const x = 1;\n",
    },
    (root) => {
      assert.equal(readWorkspacePackages(root).size, 0);
    },
  );
});

test("a malformed or nameless manifest is skipped rather than thrown on", async () => {
  await withProject(
    {
      "pnpm-workspace.yaml": "packages:\n  - packages/*\n",
      "packages/broken/package.json": "{ this is not json",
      "packages/nameless/package.json": manifest({ version: "1.0.0" }),
      "packages/fine/package.json": manifest({ name: "@acme/fine" }),
    },
    (root) => {
      assert.deepEqual([...readWorkspacePackages(root).keys()], ["@acme/fine"]);
    },
  );
});

test("pnpm-workspace.yaml keys other than `packages` are ignored", async () => {
  await withProject(
    {
      "pnpm-workspace.yaml": [
        "# the real excalidraw file has several of these",
        "onlyBuiltDependencies:",
        "  - esbuild",
        "packages:",
        "  - packages/* # inline comment",
        "catalog:",
        "  react: ^19.0.0",
        "",
      ].join("\n"),
      "packages/math/package.json": manifest({ name: "@acme/math" }),
      "esbuild/package.json": manifest({ name: "esbuild" }),
    },
    (root) => {
      assert.deepEqual([...readWorkspacePackages(root).keys()], ["@acme/math"]);
    },
  );
});

test("a flow-sequence `packages` list is read as well", async () => {
  await withProject(
    {
      "pnpm-workspace.yaml": 'packages: ["packages/*"]\n',
      "packages/math/package.json": manifest({ name: "@acme/math" }),
    },
    (root) => {
      assert.deepEqual([...readWorkspacePackages(root).keys()], ["@acme/math"]);
    },
  );
});
