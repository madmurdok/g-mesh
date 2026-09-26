import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import { createIndexabilityChecker, HARD_EXCLUDED_DIRS, hasHardExcludedSegment } from "../src/ignorePolicy";

/** Builds a small real tree under a fresh tempdir; `files` maps
 * project-relative paths to their contents (same fixture shape as
 * bulkIndex.test.ts's `makeProject`). */
async function makeProject(files: Record<string, string>): Promise<string> {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-ignore-"));
  for (const [rel, contents] of Object.entries(files)) {
    const abs = path.join(root, rel);
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, contents, "utf8");
  }
  return root;
}

async function cleanup(root: string): Promise<void> {
  await fs.rm(root, { recursive: true, force: true });
}

// --- hasHardExcludedSegment (pure) ---------------------------------------

test("hasHardExcludedSegment sees a hard-excluded name at any depth", () => {
  assert.equal(hasHardExcludedSegment("dist/index.js"), true);
  assert.equal(hasHardExcludedSegment("packages/math/dist/prod/index.js"), true);
  assert.equal(hasHardExcludedSegment("a/node_modules/b/file.ts"), true);
  assert.equal(hasHardExcludedSegment(".git/objects/x.ts"), true);
  assert.equal(hasHardExcludedSegment(".claude/worktrees/agent-abc/src/index.ts"), true);
});

test("hasHardExcludedSegment only matches whole segments", () => {
  assert.equal(hasHardExcludedSegment("src/index.ts"), false);
  assert.equal(hasHardExcludedSegment("src/distribution/index.ts"), false);
  assert.equal(hasHardExcludedSegment("src/redistributed.ts"), false);
  assert.equal(hasHardExcludedSegment("my-dist/index.ts"), false);
  assert.equal(hasHardExcludedSegment("packages/node_modulesX/a.ts"), false);
});

// --- createIndexabilityChecker (filesystem-backed) -----------------------

test("a hard-excluded directory is non-indexable regardless of depth or .gitignore", async () => {
  const root = await makeProject({
    "src/index.ts": `export const a = 1;\n`,
    "packages/math/dist/prod/index.js": `module.exports = {};\n`,
    "packages/math/src/index.ts": `export const b = 1;\n`,
    "a/node_modules/dep/file.ts": `export const c = 1;\n`,
    // Explicitly *not* mentioning these dirs: hard exclusion must not depend
    // on a .gitignore listing them.
    ".gitignore": "*.log\n",
  });
  try {
    const isIndexable = createIndexabilityChecker(root);
    assert.equal(isIndexable("src/index.ts"), true);
    assert.equal(isIndexable("packages/math/src/index.ts"), true);
    assert.equal(isIndexable("packages/math/dist/prod/index.js"), false);
    assert.equal(isIndexable("a/node_modules/dep/file.ts"), false);
  } finally {
    await cleanup(root);
  }
});

test("a root .gitignore excludes the paths it names, and only those", async () => {
  const root = await makeProject({
    "src/kept.ts": `export const kept = 1;\n`,
    "src/ignored.ts": `export const ignored = 1;\n`,
    "generated/out.ts": `export const out = 1;\n`,
    ".gitignore": "src/ignored.ts\ngenerated/\n",
  });
  try {
    const isIndexable = createIndexabilityChecker(root);
    assert.equal(isIndexable("src/kept.ts"), true);
    assert.equal(isIndexable("src/ignored.ts"), false);
    assert.equal(isIndexable("generated/out.ts"), false);
    // Repeated queries come off the layer cache; the answer must not change.
    assert.equal(isIndexable("src/ignored.ts"), false);
    assert.equal(isIndexable("src/kept.ts"), true);
  } finally {
    await cleanup(root);
  }
});

test("a nested .gitignore applies to its own subtree only", async () => {
  const root = await makeProject({
    "pkg/a/kept.ts": `export const kept = 1;\n`,
    "pkg/a/ignored.ts": `export const ignored = 1;\n`,
    "pkg/a/.gitignore": "ignored.ts\n",
    "pkg/b/ignored.ts": `export const notIgnoredHere = 1;\n`, // same basename, sibling dir
  });
  try {
    const isIndexable = createIndexabilityChecker(root);
    assert.equal(isIndexable("pkg/a/kept.ts"), true);
    assert.equal(isIndexable("pkg/a/ignored.ts"), false);
    assert.equal(isIndexable("pkg/b/ignored.ts"), true, "a sibling directory is out of that layer's scope");
  } finally {
    await cleanup(root);
  }
});

test("a negation re-includes a path an earlier rule excluded", async () => {
  const root = await makeProject({
    "src/keep-me.ts": `export const a = 1;\n`,
    "src/skip-me.ts": `export const b = 1;\n`,
    ".gitignore": "src/*.ts\n!src/keep-me.ts\n",
  });
  try {
    const isIndexable = createIndexabilityChecker(root);
    assert.equal(isIndexable("src/keep-me.ts"), true);
    assert.equal(isIndexable("src/skip-me.ts"), false);
  } finally {
    await cleanup(root);
  }
});

test("a deeper .gitignore's negation overrides a broader rule from the root", async () => {
  const root = await makeProject({
    "pkg/a/gen.ts": `export const a = 1;\n`,
    "pkg/b/gen.ts": `export const b = 1;\n`,
    ".gitignore": "gen.ts\n",
    "pkg/a/.gitignore": "!gen.ts\n",
  });
  try {
    const isIndexable = createIndexabilityChecker(root);
    assert.equal(isIndexable("pkg/a/gen.ts"), true, "the deeper layer wins, as git itself layers them");
    assert.equal(isIndexable("pkg/b/gen.ts"), false);
  } finally {
    await cleanup(root);
  }
});

test("a project with no .gitignore anywhere calls everything outside a hard-excluded dir indexable", async () => {
  const root = await makeProject({ "src/index.ts": `export const a = 1;\n` });
  try {
    const isIndexable = createIndexabilityChecker(root);
    assert.equal(isIndexable("src/index.ts"), true);
    // Never-created paths are answered the same way: this predicate is about
    // policy, not existence.
    assert.equal(isIndexable("src/missing.ts"), true);
    assert.equal(isIndexable("dist/missing.js"), false);
  } finally {
    await cleanup(root);
  }
});

// --- plugin.toml's exclude_dirs ------------------------------------------

/** The names core's walk already excludes for every language
 * (`BASELINE_EXCLUDED_DIRS` in plugins/sdk/src/walk.rs), which plugin.toml
 * therefore leaves out. */
const BASELINE_EXCLUDED_DIRS = new Set([".git", ".claude"]);

/** `[plugin.workspace] exclude_dirs` read from this plugin's own plugin.toml.
 * A targeted parse of that one key rather than a TOML dependency: the key is
 * a single-line array of plain strings, which is also valid JSON, and any
 * other shape (a multi-line array, a moved section) makes this throw rather
 * than quietly read nothing. */
function pluginTomlExcludeDirs(): string[] {
  // __dirname is dist/test at runtime: tsc compiles this file before it runs.
  const toml = readFileSync(path.join(__dirname, "..", "..", "plugin.toml"), "utf8");
  const section = /^\[plugin\.workspace\]\s*$([\s\S]*?)(?=^\[|(?![\s\S]))/m.exec(toml);
  assert.ok(section, "plugin.toml has a [plugin.workspace] section");
  const line = /^exclude_dirs\s*=\s*(\[.*\])\s*$/m.exec(section[1]);
  assert.ok(line, "[plugin.workspace] declares exclude_dirs as a single-line array");
  return JSON.parse(line[1]) as string[];
}

test("plugin.toml's exclude_dirs equals HARD_EXCLUDED_DIRS minus the baseline", () => {
  // Core reads plugin.toml as data, so it cannot be derived from
  // HARD_EXCLUDED_DIRS; this pins the two. Compared exactly, order included.
  const own = [...HARD_EXCLUDED_DIRS].filter((dir) => !BASELINE_EXCLUDED_DIRS.has(dir));
  assert.deepEqual(pluginTomlExcludeDirs(), own);
  for (const dir of BASELINE_EXCLUDED_DIRS) assert.ok(HARD_EXCLUDED_DIRS.has(dir), `${dir} stays hard-excluded`);
});
