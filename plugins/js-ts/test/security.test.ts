import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import { existsSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
// `import ... = require(...)` (rather than `import * as`) gives the real,
// mutable CommonJS module.exports object - esModuleInterop's `import *`
// wraps it in a getter-only namespace object that can't be monkey-patched,
// which is exactly what the network-spy test below needs to do.
import http = require("node:http");
import https = require("node:https");

import { bulkIndexProject, type WireNode } from "../src/bulkIndex";
import { isSupportedFile } from "../src/extract";

// Regression tests for the three mitigations documented in
// docs/architecture/g-mesh-v1.md's Security Model section: (1) a project's
// tsconfig.json `plugins` entries are never loaded/executed, (2) the plugin
// makes no outbound network calls while indexing, (3) the plugin only reads
// project source, never writing to it. None of these are enforced by an
// explicit guard - they hold as a natural consequence of this codebase's
// shape (an extension allowlist, no network dependency, no fs-write calls)
// - so these tests exist to pin that shape and fail loudly if it regresses.

/** Mirrors bulkIndex.test.ts's fixture helper: builds a small real project
 * directory under a fresh tempdir from a map of relative path -> contents. */
async function makeProject(files: Record<string, string>): Promise<string> {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-security-"));
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

// --- (1) tsconfig.json `plugins` is never loaded/executed -----------------

const EVIL_MARKER = "PWNED.marker";

/** A realistic tsconfig `compilerOptions.plugins` entry (the shape the
 * TypeScript language service loads plugins from - see
 * https://www.typescriptlang.org/tsconfig#plugins) pointing at a script
 * that proves, by its side effect, whether it was ever require()'d. */
function tsconfigFixtureFiles(): Record<string, string> {
  return {
    "tsconfig.json": JSON.stringify(
      { compilerOptions: { plugins: [{ name: "./evil-plugin.js" }] } },
      null,
      2,
    ) + "\n",
    "evil-plugin.js": `// If a loader ever require()'s this file, it proves the plugin process
// read tsconfig.json and executed a "plugins" entry - a known code-
// execution-at-startup attack vector this plugin must never touch.
const fs = require("node:fs");
const path = require("node:path");
fs.writeFileSync(path.join(__dirname, "${EVIL_MARKER}"), String(Date.now()));
module.exports = function init() { return {}; };
`,
    "src/good.ts": `export function double(n: number): number {\n  return n * 2;\n}\n`,
  };
}

test("isSupportedFile rejects tsconfig.json outright", () => {
  assert.equal(isSupportedFile("tsconfig.json"), false);
  assert.equal(isSupportedFile("/some/project/tsconfig.json"), false);
});

test("a malicious tsconfig.json `plugins` entry is never loaded/executed during indexing", async () => {
  const root = await makeProject(tsconfigFixtureFiles());
  try {
    // Calls bulkIndexProject() - the real, compiled indexing implementation
    // (dist/src/bulkIndex.js once this file is itself compiled, see
    // package.json's `pretest`) - against real files on a real tempdir,
    // exactly like bulkIndex.test.ts does. An e2e process spawn (see
    // e2e.test.ts) was considered instead, but core's "reindex" control
    // message doesn't call bulkIndexProject yet (see index.ts's own comment
    // on that case: "No NDJSON bulk-index wiring yet ... left for a later
    // ticket"), so driving the compiled binary over stdio today would only
    // prove the handshake works, not that the project walk ever touched
    // tsconfig.json - that would be false assurance. bulkIndexProject() is
    // the actual production indexing path; only the not-yet-existent stdio
    // hop in front of it is skipped here.
    const lines: string[] = [];
    const summary = await bulkIndexProject(root, (line) => lines.push(line));

    assert.equal(
      existsSync(path.join(root, EVIL_MARKER)),
      false,
      "evil-plugin.js must never execute - its marker file must not exist",
    );

    // Indexing must still succeed normally over the real source file in the
    // same project - proving tsconfig.json's presence didn't abort or
    // otherwise disrupt indexing, it was simply never opened.
    assert.ok(summary.filesProcessed >= 1);
    const nodes = lines.map((l) => JSON.parse(l) as WireNode).filter((p) => "range" in p);
    assert.ok(
      nodes.some((n) => n.filePath === "src/good.ts" && n.kind === "Function" && n.name === "double"),
      "the project's real source file must still be indexed",
    );

    // tsconfig.json has an unsupported extension, so it must never itself
    // contribute a node (evil-plugin.js, being a plain .js file, is fair
    // game to tree-sitter-parse as inert text - only *executing* it is the
    // forbidden thing, which the marker-file assertion above already covers).
    assert.ok(!nodes.some((n) => n.filePath === "tsconfig.json"));
  } finally {
    await cleanup(root);
  }
});

// --- (2) no outbound network calls -----------------------------------------

// __dirname is dist/test at runtime (this file is compiled by `tsc` before
// tests run, see package.json's `pretest`) - the real .ts sources under
// review live two levels up, not in the sibling dist/src output.
const PROJECT_ROOT = path.join(__dirname, "..", "..");
const SRC_DIR = path.join(PROJECT_ROOT, "src");

const FORBIDDEN_SOURCE_PATTERNS: RegExp[] = [
  /\bfetch\s*\(/,
  /\bXMLHttpRequest\b/,
  /\bWebSocket\b/,
  /\baxios\b/,
  /\bchild_process\b/,
  /require\(\s*["'](?:node:)?(?:http|https|net|dgram|dns|tls)["']\s*\)/,
  /from\s+["'](?:node:)?(?:http|https|net|dgram|dns|tls)["']/,
];

test("no source file imports or invokes a networking API", async () => {
  const entries = await fs.readdir(SRC_DIR);
  const tsFiles = entries.filter((f) => f.endsWith(".ts"));
  assert.ok(tsFiles.length > 0, "sanity: expected to find .ts files under src/");
  for (const file of tsFiles) {
    const contents = await fs.readFile(path.join(SRC_DIR, file), "utf8");
    for (const pattern of FORBIDDEN_SOURCE_PATTERNS) {
      assert.ok(!pattern.test(contents), `src/${file} matches forbidden network pattern ${pattern}`);
    }
  }
});

test("package.json declares no networking-capable runtime dependency", async () => {
  const pkgRaw = await fs.readFile(path.join(PROJECT_ROOT, "package.json"), "utf8");
  const pkg = JSON.parse(pkgRaw) as { dependencies?: Record<string, string> };
  // Pinned allowlist of today's actual runtime dependencies. Adding a new
  // runtime dependency must be a deliberate, reviewed edit to this set -
  // never a silent side effect of `npm install` introducing something with
  // network capability the static scan above doesn't know to look for.
  const ALLOWED_RUNTIME_DEPS = new Set(["ignore", "tree-sitter", "tree-sitter-javascript", "tree-sitter-typescript"]);
  for (const dep of Object.keys(pkg.dependencies ?? {})) {
    assert.ok(
      ALLOWED_RUNTIME_DEPS.has(dep),
      `unexpected new runtime dependency "${dep}" - review for network capability before allowing`,
    );
  }
});

test("indexing a real project never calls fetch or node:http(s) request APIs", async () => {
  const root = await makeProject({
    "src/a.ts": `export function fa(): number {\n  return 1;\n}\n`,
    "src/b.ts": `import { fa } from "./a";\n\nexport function fb(): number {\n  return fa() + 1;\n}\n`,
  });

  const originalFetch = globalThis.fetch;
  const originalHttpRequest = http.request;
  const originalHttpGet = http.get;
  const originalHttpsRequest = https.request;
  const originalHttpsGet = https.get;

  let called = false;
  const spy = () => {
    called = true;
    throw new Error("network call attempted during indexing");
  };

  try {
    // Own-property overrides shadow the real implementations for the
    // duration of this test - the same technique bulkIndex.test.ts uses to
    // spy on a stream's write() calls - so any *future* dependency that
    // starts making network calls without anyone updating the static
    // forbidden-pattern list above still gets caught here. Done inside the
    // try so the restore in `finally` always runs, even if an override
    // assignment itself were to throw.
    (globalThis as { fetch: typeof fetch }).fetch = spy as typeof fetch;
    (http as { request: typeof http.request }).request = spy as typeof http.request;
    (http as { get: typeof http.get }).get = spy as typeof http.get;
    (https as { request: typeof https.request }).request = spy as typeof https.request;
    (https as { get: typeof https.get }).get = spy as typeof https.get;

    const lines: string[] = [];
    await bulkIndexProject(root, (line) => lines.push(line));
    assert.ok(lines.length > 0, "sanity: the fixture project should still produce output");
    assert.equal(called, false, "a network API was invoked during indexing");
  } finally {
    (globalThis as { fetch: typeof fetch }).fetch = originalFetch;
    (http as { request: typeof http.request }).request = originalHttpRequest;
    (http as { get: typeof http.get }).get = originalHttpGet;
    (https as { request: typeof https.request }).request = originalHttpsRequest;
    (https as { get: typeof https.get }).get = originalHttpsGet;
    await cleanup(root);
  }
});

// --- (3) read-only: indexing never writes to the project tree --------------

interface FileSnapshot {
  relPath: string;
  size: number;
  mtimeMs: number;
}

/** Recursively snapshots every regular file under `root` (path, size,
 * mtime), sorted for a stable comparison. */
async function snapshotTree(root: string): Promise<FileSnapshot[]> {
  const out: FileSnapshot[] = [];

  async function walk(dir: string): Promise<void> {
    const entries = await fs.readdir(dir, { withFileTypes: true });
    for (const entry of entries) {
      const abs = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        await walk(abs);
      } else if (entry.isFile()) {
        const stat = await fs.stat(abs);
        out.push({
          relPath: path.relative(root, abs).split(path.sep).join("/"),
          size: stat.size,
          mtimeMs: stat.mtimeMs,
        });
      }
    }
  }

  await walk(root);
  out.sort((a, b) => a.relPath.localeCompare(b.relPath));
  return out;
}

test("indexing never creates, deletes, or modifies anything under the project tree", async () => {
  const root = await makeProject({
    ...tsconfigFixtureFiles(),
    "src/other.ts": `export const x = 1;\n`,
  });
  try {
    const before = await snapshotTree(root);
    const lines: string[] = [];
    await bulkIndexProject(root, (line) => lines.push(line));
    const after = await snapshotTree(root);

    assert.ok(lines.length > 0, "sanity: the fixture should still produce output");
    assert.deepEqual(after, before, "indexing must not create, delete, or modify any file under the project root");
  } finally {
    await cleanup(root);
  }
});
