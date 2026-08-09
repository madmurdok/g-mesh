import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import {
  createTsconfigPathsIndex,
  expandPathsCandidates,
  type TsconfigPathsConfig,
} from "../src/tsconfigPaths";

/** A real directory tree, like workspace.test.ts: what is under test *is*
 * filesystem behaviour (which config a file is governed by, missing extends
 * targets, malformed JSON), which a stubbed fs would only re-state. */
async function makeProject(files: Record<string, string>): Promise<string> {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-tsconfig-"));
  for (const [rel, contents] of Object.entries(files)) {
    const abs = path.join(root, rel);
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, contents, "utf8");
  }
  return root;
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

function tsconfig(fields: Record<string, unknown>): string {
  return JSON.stringify(fields, null, 2);
}

function configFor(root: string, fromFilePath: string): TsconfigPathsConfig | null {
  return createTsconfigPathsIndex(root)(fromFilePath);
}

/** The paths an alias offers, in order - the whole answer this module owes
 * resolve.ts, before existence gets a say. */
function candidates(root: string, fromFilePath: string, specifier: string): string[] {
  const config = configFor(root, fromFilePath);
  return config === null ? [] : expandPathsCandidates(config, specifier);
}

// --- the shape every Next.js/Vite app is generated with -------------------

test("a baseUrl-anchored wildcard alias resolves to the directory it names", async () => {
  await withProject(
    {
      "tsconfig.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } },
      }),
    },
    (root) => {
      assert.deepEqual(candidates(root, "src/app/page.tsx", "@/foo"), ["src/foo"]);
      assert.deepEqual(candidates(root, "src/app/page.tsx", "@/lib/deep/thing"), [
        "src/lib/deep/thing",
      ]);
      assert.deepEqual(configFor(root, "src/app/page.tsx")?.resolveDir, "");
      // A specifier no key matches is not this mechanism's business.
      assert.deepEqual(candidates(root, "src/app/page.tsx", "react"), []);
    },
  );
});

test("an exact alias key matches only that literal specifier", async () => {
  await withProject(
    {
      "tsconfig.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { utils: ["./src/utils/index.ts"] } },
      }),
    },
    (root) => {
      assert.deepEqual(candidates(root, "src/a.ts", "utils"), ["src/utils/index.ts"]);
      for (const specifier of ["utils/deep", "util", "@/utils", "./utils"]) {
        assert.deepEqual(candidates(root, "src/a.ts", specifier), [], specifier);
      }
    },
  );
});

test("a key's targets are offered in declaration order", async () => {
  await withProject(
    {
      "tsconfig.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*", "./legacy/*"] } },
      }),
    },
    (root) => {
      assert.deepEqual(candidates(root, "src/a.ts", "@/thing"), ["src/thing", "legacy/thing"]);
    },
  );
});

/** Both keys really do apply - which one wins is a question about what exists
 * on disk, and resolve.ts is the side that knows. */
test("every matching key contributes, in the order the config declares them", async () => {
  await withProject(
    {
      "tsconfig.json": tsconfig({
        compilerOptions: {
          baseUrl: ".",
          paths: { "@/*": ["./src/*"], "@/components/*": ["./ui/*"] },
        },
      }),
    },
    (root) => {
      assert.deepEqual(candidates(root, "src/a.ts", "@/components/button"), [
        "src/components/button",
        "ui/button",
      ]);
    },
  );
});

test("without a baseUrl, targets resolve against the config file's own directory", async () => {
  await withProject(
    {
      "packages/app/tsconfig.json": tsconfig({
        compilerOptions: { paths: { "@/*": ["./src/*"] } },
      }),
    },
    (root) => {
      assert.equal(configFor(root, "packages/app/src/a.ts")?.resolveDir, "packages/app");
      assert.deepEqual(candidates(root, "packages/app/src/a.ts", "@/foo"), [
        "packages/app/src/foo",
      ]);
    },
  );
});

// --- extends --------------------------------------------------------------

test("a config with no paths of its own inherits the ones it extends", async () => {
  await withProject(
    {
      "tsconfig.base.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } },
      }),
      "tsconfig.json": tsconfig({
        extends: "./tsconfig.base.json",
        compilerOptions: { strict: true },
      }),
    },
    (root) => {
      assert.deepEqual(candidates(root, "src/a.ts", "@/foo"), ["src/foo"]);
    },
  );
});

test("a config's own paths replace an inherited map whole, key by key included", async () => {
  await withProject(
    {
      "tsconfig.base.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"], "~/*": ["./lib/*"] } },
      }),
      "tsconfig.json": tsconfig({
        extends: "./tsconfig.base.json",
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./app/*"] } },
      }),
    },
    (root) => {
      assert.deepEqual(candidates(root, "src/a.ts", "@/foo"), ["app/foo"]);
      assert.deepEqual(
        candidates(root, "src/a.ts", "~/foo"),
        [],
        "the base's other key is gone, not merged in",
      );
    },
  );
});

test("a baseUrl without paths of its own leaves an inherited map's directory alone", async () => {
  await withProject(
    {
      // Declares paths and no baseUrl, so its own directory anchors them.
      "configs/tsconfig.base.json": tsconfig({
        compilerOptions: { paths: { "@/*": ["./src/*"] } },
      }),
      "tsconfig.json": tsconfig({
        extends: "./configs/tsconfig.base.json",
        compilerOptions: { baseUrl: "./app" },
      }),
    },
    (root) => {
      assert.equal(configFor(root, "src/a.ts")?.resolveDir, "configs");
      assert.deepEqual(candidates(root, "src/a.ts", "@/foo"), ["configs/src/foo"]);
    },
  );
});

test("an unreadable or package-named extends entry is skipped, not fatal", async () => {
  await withProject(
    {
      "tsconfig.base.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } },
      }),
      "tsconfig.json": tsconfig({
        extends: [
          "@tsconfig/node18/tsconfig.json", // never installed, and never ours to read
          "./nope.json", // simply not there
          "./tsconfig.base.json",
        ],
      }),
    },
    (root) => {
      assert.deepEqual(
        candidates(root, "src/a.ts", "@/foo"),
        ["src/foo"],
        "the rest of the chain must still apply",
      );
    },
  );
});

test("the nearest config wins over a more distant one", async () => {
  await withProject(
    {
      "tsconfig.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./root-src/*"] } },
      }),
      "packages/app/tsconfig.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } },
      }),
    },
    (root) => {
      assert.deepEqual(candidates(root, "packages/app/src/a.ts", "@/foo"), [
        "packages/app/src/foo",
      ]);
      assert.deepEqual(candidates(root, "root-src/a.ts", "@/foo"), ["root-src/foo"]);
    },
  );
});

// --- what a real tsconfig actually looks like ------------------------------

test("comments and trailing commas are read the same as strict JSON", async () => {
  const strict = tsconfig({
    $schema: "https://json.schemastore.org/tsconfig",
    compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } },
  });
  const jsonc = [
    "{",
    '  // Generated by `tsc --init`; see https://www.typescriptlang.org/tsconfig',
    '  "$schema": "https://json.schemastore.org/tsconfig",',
    "  /* Only the fields below are ever read by g-mesh:",
    '     baseUrl, paths, extends. */',
    '  "compilerOptions": {',
    '    "baseUrl": ".",',
    '    "paths": {',
    '      "@/*": ["./src/*"],', // trailing comma inside the map
    "    },",
    "  },",
    "}",
    "",
  ].join("\n");

  await withProject({ "tsconfig.json": strict }, async (strictRoot) => {
    await withProject({ "tsconfig.json": jsonc }, (jsoncRoot) => {
      assert.deepEqual(candidates(jsoncRoot, "src/a.ts", "@/foo"), ["src/foo"]);
      assert.deepEqual(
        candidates(jsoncRoot, "src/a.ts", "@/foo"),
        candidates(strictRoot, "src/a.ts", "@/foo"),
      );
    });
  });
});

// --- nothing outside the project root ------------------------------------

test("a baseUrl climbing out of the project voids the aliases it anchors", async () => {
  await withProject(
    {
      "tsconfig.json": tsconfig({
        compilerOptions: { baseUrl: "../..", paths: { "@/*": ["./src/*"] } },
      }),
    },
    (root) => {
      assert.equal(configFor(root, "src/a.ts"), null);
      assert.deepEqual(candidates(root, "src/a.ts", "@/foo"), []);
    },
  );
});

test("an alias target climbing out of the project is not offered", async () => {
  await withProject(
    {
      "tsconfig.json": tsconfig({
        compilerOptions: {
          baseUrl: ".",
          paths: {
            "@/*": ["./src/*"],
            secrets: ["../../../etc/passwd"],
            absolute: ["/etc/passwd"],
            "escape/*": ["../../*"],
          },
        },
      }),
    },
    (root) => {
      assert.deepEqual(candidates(root, "src/a.ts", "secrets"), []);
      assert.deepEqual(candidates(root, "src/a.ts", "absolute"), []);
      assert.deepEqual(candidates(root, "src/a.ts", "escape/etc/passwd"), []);
      assert.deepEqual(candidates(root, "src/a.ts", "@/foo"), ["src/foo"]);
    },
  );
});

// --- jsconfig and memoization --------------------------------------------

test("a jsconfig.json is read the same way, when there is no tsconfig.json", async () => {
  await withProject(
    {
      "jsconfig.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } },
      }),
    },
    (root) => {
      assert.deepEqual(candidates(root, "src/a.ts", "@/foo"), ["src/foo"]);
    },
  );

  await withProject(
    {
      "tsconfig.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./ts-src/*"] } },
      }),
      "jsconfig.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./js-src/*"] } },
      }),
    },
    (root) => {
      assert.deepEqual(
        candidates(root, "src/a.ts", "@/foo"),
        ["ts-src/foo"],
        "tsconfig.json is the one TypeScript itself would use",
      );
    },
  );
});

/** One config file, one parse: the same resolved object comes back for every
 * file it governs, which is what makes the walk-per-directory affordable. */
test("the resolved config is shared by every file it governs", async () => {
  await withProject(
    {
      "tsconfig.json": tsconfig({
        compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } },
      }),
    },
    (root) => {
      const index = createTsconfigPathsIndex(root);
      const first = index("src/a.ts");
      assert.ok(first !== null);
      assert.equal(index("src/b.ts"), first, "same directory, same answer object");
      assert.equal(index("src/deep/c.ts"), first, "same config, one directory further down");
    },
  );
});
